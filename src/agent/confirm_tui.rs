//! Full-screen ratatui dashboard for `mini --interactive --ui ratatui`.
//!
//! `RatatuiDashboard` is both a `StreamSink` (so trajectory events flow
//! into a live log panel) and a `ConfirmCallback` (so the confirmation
//! prompt is rendered as a centred modal). The renderer task owns the
//! terminal; the agent thread drives state through a mutex and a
//! `Notify` channel.

use std::collections::VecDeque;
use std::io::{Stdout, Write as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::stream::StreamExt as _;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tokio::sync::{Notify, oneshot};

use super::confirm::{ConfirmCallback, ConfirmContext, ConfirmDecision};
use crate::stream::{StreamEvent, StreamSink};

const MAX_LOG_LINES: usize = 400;

#[derive(Default)]
struct DashboardState {
    task: Option<String>,
    model: Option<String>,
    started_at: Option<String>,
    cost_usd: f64,
    step: u32,
    step_limit: u32,
    log: VecDeque<LogLine>,
    pending: Option<PendingPrompt>,
    finished: Option<String>,
}

#[derive(Clone)]
struct LogLine {
    kind: LineKind,
    text: String,
}

#[derive(Clone, Copy)]
enum LineKind {
    Info,
    AssistantMsg,
    BashRun,
    BashOk,
    BashErr,
    Observation,
    Warn,
}

struct PendingPrompt {
    ctx: ConfirmContext,
    responder: oneshot::Sender<ConfirmDecision>,
}

pub struct RatatuiDashboardHandle {
    inner: Arc<RatatuiDashboard>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    renderer_task: Option<tokio::task::JoinHandle<()>>,
}

impl RatatuiDashboardHandle {
    pub fn stream_sink(&self) -> Arc<dyn StreamSink> {
        self.inner.clone() as Arc<dyn StreamSink>
    }

    pub fn confirm_callback(&self) -> Arc<dyn ConfirmCallback> {
        self.inner.clone() as Arc<dyn ConfirmCallback>
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.renderer_task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RatatuiDashboardHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = restore_terminal();
    }
}

pub struct RatatuiDashboard {
    state: Mutex<DashboardState>,
    notify: Notify,
}

impl RatatuiDashboard {
    /// Enter alt-screen + raw mode and spawn the renderer task.
    ///
    /// # Errors
    /// Returns `Err` if the terminal cannot be put into raw mode or
    /// alt-screen mode.
    pub fn start() -> std::io::Result<RatatuiDashboardHandle> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        let dash = Arc::new(Self {
            state: Mutex::new(DashboardState::default()),
            notify: Notify::new(),
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let renderer_task = tokio::spawn(renderer_loop(dash.clone(), terminal, shutdown_rx));
        Ok(RatatuiDashboardHandle {
            inner: dash,
            shutdown_tx: Some(shutdown_tx),
            renderer_task: Some(renderer_task),
        })
    }

    fn append(&self, kind: LineKind, text: impl Into<String>) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.log.len() >= MAX_LOG_LINES {
            s.log.pop_front();
        }
        s.log.push_back(LogLine {
            kind,
            text: text.into(),
        });
        drop(s);
        self.notify.notify_waiters();
    }
}

fn restore_terminal() -> std::io::Result<()> {
    let mut stdout: Stdout = std::io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = stdout.flush();
    disable_raw_mode()
}

impl StreamSink for RatatuiDashboard {
    fn emit(&self, event: StreamEvent) {
        match event {
            StreamEvent::RunStarted {
                task,
                model,
                started_at,
            } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    s.task = Some(task.clone());
                    s.model = Some(model.clone());
                    s.started_at = Some(started_at);
                }
                self.append(LineKind::Info, format!("run started: {model} :: {task}"));
            }
            StreamEvent::AssistantMessage {
                step,
                content,
                cost_usd,
                ..
            } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(cost) = cost_usd {
                        s.cost_usd = cost;
                    }
                    s.step = step;
                }
                let preview = first_lines(&content, 6);
                self.append(
                    LineKind::AssistantMsg,
                    format!("step {step} assistant: {preview}"),
                );
            }
            StreamEvent::BashStart { step, command, .. } => {
                self.append(LineKind::BashRun, format!("step {step} bash: {command}"));
            }
            StreamEvent::BashResult {
                step,
                exit_code,
                stdout,
                stderr,
                timed_out,
                ..
            } => {
                let kind = if exit_code == 0 && !timed_out {
                    LineKind::BashOk
                } else {
                    LineKind::BashErr
                };
                let summary = format!(
                    "step {step} bash exit {exit_code}{}{}",
                    if timed_out { " timed_out" } else { "" },
                    summarize_stream(&stdout, &stderr),
                );
                self.append(kind, summary);
            }
            StreamEvent::Observation { step, content, .. } => {
                let preview = first_lines(&content, 4);
                self.append(
                    LineKind::Observation,
                    format!("step {step} observation: {preview}"),
                );
            }
            StreamEvent::FormatError { step, content, .. } => {
                let preview = first_lines(&content, 4);
                self.append(LineKind::Warn, format!("step {step} format error: {preview}"));
            }
            StreamEvent::RunEnded {
                exit_reason,
                steps,
                total_cost_usd,
                ..
            } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    s.cost_usd = total_cost_usd;
                    s.step = steps;
                    s.finished = Some(exit_reason.clone());
                }
                self.append(
                    LineKind::Info,
                    format!("run ended: {exit_reason} (steps={steps} cost=${total_cost_usd:.4})"),
                );
            }
        }
    }
}

#[async_trait]
impl ConfirmCallback for RatatuiDashboard {
    async fn confirm(&self, ctx: &ConfirmContext) -> ConfirmDecision {
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.step = ctx.step;
            s.step_limit = ctx.step_limit;
            s.cost_usd = ctx.cost_usd;
            s.pending = Some(PendingPrompt {
                ctx: ctx.clone(),
                responder: tx,
            });
        }
        self.notify.notify_waiters();
        rx.await.unwrap_or(ConfirmDecision::Abort)
    }
}

async fn renderer_loop(
    dash: Arc<RatatuiDashboard>,
    mut terminal: Terminal<CrosstermBackend<Stdout>>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(150));
    loop {
        if let Err(err) = draw_frame(&dash, &mut terminal) {
            tracing::warn!(?err, "ratatui draw failed");
        }
        tokio::select! {
            _ = &mut shutdown => break,
            () = dash.notify.notified() => {}
            _ = tick.tick() => {}
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) => handle_key(&dash, key),
                    Some(Err(err)) => {
                        tracing::warn!(?err, "ratatui event stream error");
                    }
                    _ => {}
                }
            }
        }
    }
    let _ = terminal.show_cursor();
    drop(terminal);
    let _ = restore_terminal();
}

fn handle_key(dash: &Arc<RatatuiDashboard>, key: KeyEvent) {
    if !matches!(key.kind, KeyEventKind::Press) {
        return;
    }
    let pending = {
        let mut s = dash
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.pending.take()
    };
    let Some(pending) = pending else {
        return;
    };
    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'));
    let decision = if ctrl_c {
        Some(ConfirmDecision::Abort)
    } else {
        match key.code {
            KeyCode::Char('y' | 'Y') => Some(ConfirmDecision::Approve),
            KeyCode::Char('n' | 'N') => Some(ConfirmDecision::Reject),
            KeyCode::Char('a' | 'A') | KeyCode::Esc => Some(ConfirmDecision::Abort),
            _ => None,
        }
    };
    if let Some(d) = decision {
        let _ = pending.responder.send(d);
    } else {
        let mut s = dash
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.pending = Some(pending);
    }
}

fn draw_frame(
    dash: &Arc<RatatuiDashboard>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> std::io::Result<()> {
    let snapshot = {
        let s = dash
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        DashboardSnapshot {
            task: s.task.clone(),
            model: s.model.clone(),
            step: s.step,
            step_limit: s.step_limit,
            cost_usd: s.cost_usd,
            finished: s.finished.clone(),
            log: s.log.iter().cloned().collect(),
            pending: s.pending.as_ref().map(|p| p.ctx.clone()),
        }
    };
    terminal.draw(|frame| draw(frame, &snapshot))?;
    Ok(())
}

struct DashboardSnapshot {
    task: Option<String>,
    model: Option<String>,
    step: u32,
    step_limit: u32,
    cost_usd: f64,
    finished: Option<String>,
    log: Vec<LogLine>,
    pending: Option<ConfirmContext>,
}

fn draw(frame: &mut ratatui::Frame, snap: &DashboardSnapshot) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    frame.render_widget(header_paragraph(snap), chunks[0]);
    frame.render_widget(log_paragraph(snap), chunks[1]);
    frame.render_widget(footer_paragraph(snap), chunks[2]);

    if let Some(ctx) = &snap.pending {
        draw_modal(frame, ctx, area);
    }
}

fn header_paragraph(snap: &DashboardSnapshot) -> Paragraph<'_> {
    let title = snap
        .task
        .as_deref()
        .unwrap_or("(no task)")
        .lines()
        .next()
        .unwrap_or("");
    let model = snap.model.as_deref().unwrap_or("(no model)");
    let line = Line::from(vec![
        Span::styled(
            format!("step {}/{}  ", snap.step, snap.step_limit),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            format!("cost ${:.4}  ", snap.cost_usd),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(format!("model: {model}  "), Style::default().fg(Color::Green)),
        Span::styled(
            format!("task: {title}"),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" maxwell's daemon — interactive "),
    )
}

fn log_paragraph(snap: &DashboardSnapshot) -> Paragraph<'_> {
    let max_lines = snap.log.len().min(MAX_LOG_LINES);
    let take_from = snap.log.len().saturating_sub(max_lines);
    let lines: Vec<Line> = snap.log[take_from..]
        .iter()
        .map(|l| {
            let style = match l.kind {
                LineKind::Info => Style::default().fg(Color::Gray),
                LineKind::AssistantMsg => Style::default().fg(Color::Cyan),
                LineKind::BashRun => Style::default().fg(Color::White),
                LineKind::BashOk => Style::default().fg(Color::Green),
                LineKind::BashErr => Style::default().fg(Color::Red),
                LineKind::Observation => Style::default().fg(Color::LightBlue),
                LineKind::Warn => Style::default().fg(Color::LightYellow),
            };
            Line::from(Span::styled(l.text.clone(), style))
        })
        .collect();
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" trajectory "))
        .wrap(Wrap { trim: false })
}

fn footer_paragraph(snap: &DashboardSnapshot) -> Paragraph<'_> {
    let hint = if snap.finished.is_some() {
        "run complete — press 'q' or Ctrl-C to close"
    } else if snap.pending.is_some() {
        "(y) approve   (n) reject   (a) abort"
    } else {
        "waiting for next agent step…"
    };
    Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .block(Block::default().borders(Borders::ALL))
}

fn draw_modal(frame: &mut ratatui::Frame, ctx: &ConfirmContext, area: Rect) {
    let modal = centered_rect(70, 50, area);
    frame.render_widget(Clear, modal);
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "step {}/{}  cost ${:.4}  {}",
                ctx.step, ctx.step_limit, ctx.cost_usd, ctx.cache_marker
            ),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            format!("tool: {}", ctx.tool_name),
            Style::default().fg(Color::Cyan),
        )),
        Line::from(""),
        Line::from(Span::styled("command:", Style::default().fg(Color::White))),
    ];
    for cmd_line in ctx.command.lines().take(12) {
        lines.push(Line::from(format!("  {cmd_line}")));
    }
    if ctx.command.lines().count() > 12 {
        lines.push(Line::from(Span::styled(
            "  …",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "(y) approve   (n) reject   (a) abort",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" confirm action ");
    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(p, modal);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn first_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().take(n).collect();
    let mut out = lines.join(" | ");
    if s.lines().count() > n {
        out.push_str(" …");
    }
    if out.len() > 240 {
        out.truncate(240);
        out.push_str(" …");
    }
    out
}

fn summarize_stream(stdout: &str, stderr: &str) -> String {
    let bytes = stdout.len() + stderr.len();
    if bytes > 0 {
        format!(" ({bytes}B output)")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_lines_caps_count_and_length() {
        let s = "a\nb\nc\nd\ne\nf\ng";
        assert_eq!(first_lines(s, 3), "a | b | c …");
    }

    #[test]
    fn summarize_stream_returns_byte_count() {
        assert_eq!(summarize_stream("abc", ""), " (3B output)");
        assert_eq!(summarize_stream("", ""), "");
    }
}
