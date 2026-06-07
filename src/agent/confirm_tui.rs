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
    feedback_input: Option<String>,
    edit_input: Option<String>,
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
    /// alt-screen mode. On failure the terminal is restored to cooked
    /// mode and the alt-screen is left, so the caller never observes a
    /// half-initialised terminal.
    pub fn start() -> std::io::Result<RatatuiDashboardHandle> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => {
                let _ = restore_terminal();
                return Err(e);
            }
        };
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
                self.append(
                    LineKind::Warn,
                    format!("step {step} format error: {preview}"),
                );
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

fn handle_key_feedback_input(
    dash: &RatatuiDashboard,
    s: &mut DashboardState,
    pending: PendingPrompt,
    key_code: KeyCode,
    mut buffer: String,
) {
    match key_code {
        KeyCode::Enter => {
            let decision = if buffer.trim().is_empty() {
                ConfirmDecision::Reject(None)
            } else {
                ConfirmDecision::Reject(Some(buffer))
            };
            let _ = pending.responder.send(decision);
        }
        KeyCode::Esc => {
            s.feedback_input = None;
            s.pending = Some(pending);
            dash.notify.notify_waiters();
        }
        KeyCode::Backspace => {
            buffer.pop();
            s.feedback_input = Some(buffer);
            s.pending = Some(pending);
            dash.notify.notify_waiters();
        }
        KeyCode::Char(c) => {
            buffer.push(c);
            s.feedback_input = Some(buffer);
            s.pending = Some(pending);
            dash.notify.notify_waiters();
        }
        _ => {
            s.feedback_input = Some(buffer);
            s.pending = Some(pending);
        }
    }
}

fn handle_key_edit_input(
    dash: &RatatuiDashboard,
    s: &mut DashboardState,
    pending: PendingPrompt,
    key_code: KeyCode,
    mut buffer: String,
) {
    match key_code {
        KeyCode::Enter => {
            if buffer.trim().is_empty() {
                s.pending = Some(pending);
                dash.notify.notify_waiters();
            } else {
                let _ = pending.responder.send(ConfirmDecision::Edit(buffer));
            }
        }
        KeyCode::Esc => {
            s.pending = Some(pending);
            dash.notify.notify_waiters();
        }
        KeyCode::Backspace => {
            buffer.pop();
            s.edit_input = Some(buffer);
            s.pending = Some(pending);
            dash.notify.notify_waiters();
        }
        KeyCode::Char(c) => {
            buffer.push(c);
            s.edit_input = Some(buffer);
            s.pending = Some(pending);
            dash.notify.notify_waiters();
        }
        _ => {
            s.edit_input = Some(buffer);
            s.pending = Some(pending);
        }
    }
}

fn handle_key_normal(
    dash: &RatatuiDashboard,
    s: &mut DashboardState,
    pending: PendingPrompt,
    key: KeyEvent,
) {
    if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
        s.pending = Some(pending);
        return;
    }
    match key.code {
        KeyCode::Char('y' | 'Y') => {
            let _ = pending.responder.send(ConfirmDecision::Approve);
        }
        KeyCode::Char('n' | 'N') => {
            s.feedback_input = Some(String::new());
            s.pending = Some(pending);
            dash.notify.notify_waiters();
        }
        KeyCode::Char('e' | 'E') => {
            s.edit_input = Some(pending.ctx.command.clone());
            s.pending = Some(pending);
            dash.notify.notify_waiters();
        }
        KeyCode::Char('a' | 'A') | KeyCode::Esc => {
            let _ = pending.responder.send(ConfirmDecision::Abort);
        }
        _ => {
            s.pending = Some(pending);
        }
    }
}

fn handle_key(dash: &Arc<RatatuiDashboard>, key: KeyEvent) {
    if !matches!(key.kind, KeyEventKind::Press) {
        return;
    }
    let mut s = dash
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(pending) = s.pending.take() else {
        return;
    };
    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'));
    if ctrl_c {
        s.feedback_input = None;
        s.edit_input = None;
        let _ = pending.responder.send(ConfirmDecision::Abort);
        return;
    }

    if let Some(buffer) = s.feedback_input.take() {
        handle_key_feedback_input(dash, &mut s, pending, key.code, buffer);
    } else if let Some(buffer) = s.edit_input.take() {
        handle_key_edit_input(dash, &mut s, pending, key.code, buffer);
    } else {
        handle_key_normal(dash, &mut s, pending, key);
    }
    drop(s);
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
            feedback_input: s.feedback_input.clone(),
            edit_input: s.edit_input.clone(),
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
    feedback_input: Option<String>,
    edit_input: Option<String>,
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
        draw_modal(
            frame,
            ctx,
            snap.feedback_input.as_ref(),
            snap.edit_input.as_ref(),
            area,
        );
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
        Span::styled(
            format!("model: {model}  "),
            Style::default().fg(Color::Green),
        ),
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
    } else if snap.edit_input.is_some() {
        "[Enter] execute edit   [Esc] cancel"
    } else if snap.pending.is_some() {
        "(y) approve   (n) reject   (e) edit   (a) abort"
    } else {
        "waiting for next agent step…"
    };
    Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .block(Block::default().borders(Borders::ALL))
}

fn draw_modal(
    frame: &mut ratatui::Frame,
    ctx: &ConfirmContext,
    feedback_input: Option<&String>,
    edit_input: Option<&String>,
    area: Rect,
) {
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
    // Short-circuit at the first line past the cap instead of counting
    // every line in a long command string.
    if ctx.command.lines().nth(12).is_some() {
        lines.push(Line::from(Span::styled(
            "  …",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    lines.push(Line::from(""));

    if let Some(buffer) = feedback_input {
        lines.push(Line::from(Span::styled(
            "Provide corrective feedback (optional):",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled(" > ", Style::default().fg(Color::Green)),
            Span::styled(buffer.clone(), Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(Color::Green)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[Enter] submit   [Esc] back to choices",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else if let Some(buffer) = edit_input {
        lines.push(Line::from(Span::styled(
            "Edit proposed command:",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled(" > ", Style::default().fg(Color::Green)),
            Span::styled(buffer.clone(), Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(Color::Green)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[Enter] execute edit   [Esc] cancel",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "(y) approve   (n) reject   (e) edit   (a) abort",
            Style::default().add_modifier(Modifier::BOLD),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" confirm action ");
    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
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
    // Short-circuit at the first line past the cap rather than counting
    // every line in the source string.
    if s.lines().nth(n).is_some() {
        out.push_str(" …");
    }
    if out.len() > 240 {
        // `String::truncate` panics if the index falls inside a
        // multi-byte UTF-8 codepoint. Walk back to the nearest char
        // boundary so arbitrary tool output never crashes the renderer.
        let mut cut = 240;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
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
    #![allow(clippy::unwrap_used)]
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

    #[test]
    fn first_lines_truncates_at_char_boundary_for_multibyte_input() {
        // 240 bytes of a 3-byte UTF-8 character ("é" is 2 bytes; "🦀" is 4).
        // Repeat 🦀 (4 bytes) enough times to overflow 240 — a naive
        // truncate(240) would land mid-codepoint and panic.
        let s = "🦀".repeat(200);
        let out = first_lines(&s, 1);
        // Must not panic; result is bounded and ends with our ellipsis marker.
        assert!(out.len() <= 244);
        assert!(out.ends_with("…"));
    }

    #[test]
    fn first_lines_no_ellipsis_when_under_cap() {
        assert_eq!(first_lines("hello", 4), "hello");
    }

    #[test]
    fn summarize_stream_counts_both_streams() {
        assert_eq!(summarize_stream("ab", "cd"), " (4B output)");
    }

    // ── Dashboard-without-a-terminal tests ──────────────────────────────
    //
    // Most renderer behaviour is locked behind `start()`, which enters
    // alt-screen + raw mode. These tests construct a `RatatuiDashboard`
    // directly and exercise the `StreamSink` / `ConfirmCallback` paths
    // plus the pure draw helpers via ratatui's `TestBackend`, so the
    // interesting logic ships covered even though a real renderer-loop
    // tick needs a TTY.

    use ratatui::Terminal as RatatuiTerminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use tokio::sync::oneshot;

    fn make_dashboard() -> Arc<RatatuiDashboard> {
        Arc::new(RatatuiDashboard {
            state: Mutex::new(DashboardState::default()),
            notify: Notify::new(),
        })
    }

    fn snap(dash: &Arc<RatatuiDashboard>) -> DashboardSnapshot {
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
            feedback_input: s.feedback_input.clone(),
            edit_input: s.edit_input.clone(),
        }
    }

    fn render_to_buffer(snap: &DashboardSnapshot, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = RatatuiTerminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, snap)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn emit_run_started_populates_header_state() {
        let d = make_dashboard();
        d.emit(StreamEvent::RunStarted {
            task: "fix the bug".into(),
            model: "claude-opus-4-7".into(),
            started_at: "2026-05-18T12:00:00Z".into(),
        });
        let s = snap(&d);
        assert_eq!(s.task.as_deref(), Some("fix the bug"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(s.log.len(), 1);
        assert!(s.log[0].text.contains("run started"));
    }

    #[test]
    fn emit_assistant_message_updates_step_and_cost() {
        let d = make_dashboard();
        d.emit(StreamEvent::AssistantMessage {
            step: 4,
            content: "think think".into(),
            cost_usd: Some(0.1234),
            timestamp: "t".into(),
        });
        let s = snap(&d);
        assert_eq!(s.step, 4);
        assert!((s.cost_usd - 0.1234).abs() < f64::EPSILON);
        assert!(s.log[0].text.contains("step 4 assistant"));
    }

    #[test]
    fn emit_assistant_message_without_cost_keeps_prior_cost() {
        let d = make_dashboard();
        // First set a cost.
        d.emit(StreamEvent::AssistantMessage {
            step: 1,
            content: "x".into(),
            cost_usd: Some(0.5),
            timestamp: "t".into(),
        });
        // Second message without cost shouldn't reset to 0.
        d.emit(StreamEvent::AssistantMessage {
            step: 2,
            content: "y".into(),
            cost_usd: None,
            timestamp: "t".into(),
        });
        let s = snap(&d);
        assert_eq!(s.step, 2);
        assert!((s.cost_usd - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn emit_bash_lifecycle_logs_run_and_result() {
        let d = make_dashboard();
        d.emit(StreamEvent::BashStart {
            step: 2,
            command: "echo hi".into(),
            timestamp: "t".into(),
        });
        d.emit(StreamEvent::BashResult {
            step: 2,
            exit_code: 0,
            stdout: "hi\n".into(),
            stderr: String::new(),
            timed_out: false,
            timestamp: "t".into(),
        });
        d.emit(StreamEvent::BashResult {
            step: 3,
            exit_code: 1,
            stdout: String::new(),
            stderr: "oops".into(),
            timed_out: false,
            timestamp: "t".into(),
        });
        d.emit(StreamEvent::BashResult {
            step: 4,
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
            timestamp: "t".into(),
        });
        let s = snap(&d);
        assert_eq!(s.log.len(), 4);
        assert!(matches!(s.log[0].kind, LineKind::BashRun));
        assert!(matches!(s.log[1].kind, LineKind::BashOk));
        assert!(matches!(s.log[2].kind, LineKind::BashErr));
        assert!(s.log[3].text.contains("timed_out"));
    }

    #[test]
    fn emit_observation_and_format_error_log_with_preview() {
        let d = make_dashboard();
        d.emit(StreamEvent::Observation {
            step: 1,
            content: "line1\nline2".into(),
            timestamp: "t".into(),
        });
        d.emit(StreamEvent::FormatError {
            step: 1,
            content: "bad".into(),
            timestamp: "t".into(),
        });
        let s = snap(&d);
        assert_eq!(s.log.len(), 2);
        assert!(matches!(s.log[0].kind, LineKind::Observation));
        assert!(matches!(s.log[1].kind, LineKind::Warn));
    }

    #[test]
    fn emit_run_ended_stamps_finished_and_totals() {
        let d = make_dashboard();
        d.emit(StreamEvent::RunEnded {
            exit_reason: "submitted".into(),
            failure_category: None,
            final_output: None,
            steps: 7,
            total_cost_usd: 1.2345,
            ended_at: "t".into(),
        });
        let s = snap(&d);
        assert_eq!(s.step, 7);
        assert!((s.cost_usd - 1.2345).abs() < f64::EPSILON);
        assert_eq!(s.finished.as_deref(), Some("submitted"));
    }

    #[test]
    fn append_caps_log_at_max_lines() {
        let d = make_dashboard();
        for _ in 0..(MAX_LOG_LINES + 10) {
            d.append(LineKind::Info, "x");
        }
        let s = snap(&d);
        assert_eq!(s.log.len(), MAX_LOG_LINES);
    }

    #[tokio::test]
    async fn confirm_publishes_pending_and_completes_on_decision() {
        let d = make_dashboard();
        let d2 = d.clone();
        let ctx = ConfirmContext {
            tool_name: "bash".into(),
            command: "echo".into(),
            step: 1,
            step_limit: 5,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
        };
        let task = tokio::spawn(async move { d2.confirm(&ctx).await });
        // Spin briefly until the pending slot is populated.
        for _ in 0..50 {
            let s = snap(&d);
            if s.pending.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Simulate the renderer thread receiving a `y` keystroke.
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        handle_key(&d, key);
        let decision = task.await.unwrap();
        assert_eq!(decision, ConfirmDecision::Approve);
    }

    #[tokio::test]
    async fn confirm_falls_back_to_abort_when_responder_dropped() {
        let d = make_dashboard();
        let d2 = d.clone();
        let ctx = ConfirmContext {
            tool_name: "bash".into(),
            command: "echo".into(),
            step: 0,
            step_limit: 1,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
        };
        let task = tokio::spawn(async move { d2.confirm(&ctx).await });
        for _ in 0..50 {
            if snap(&d).pending.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Pull the pending prompt out and drop the responder — confirm()
        // should observe the dropped sender and abort.
        let pending = {
            let mut s = d
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending.take()
        };
        drop(pending);
        let decision = task.await.unwrap();
        assert_eq!(decision, ConfirmDecision::Abort);
    }

    fn make_pending(d: &Arc<RatatuiDashboard>) -> oneshot::Receiver<ConfirmDecision> {
        let (tx, rx) = oneshot::channel();
        let mut s = d
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.pending = Some(PendingPrompt {
            ctx: ConfirmContext {
                tool_name: "bash".into(),
                command: "x".into(),
                step: 0,
                step_limit: 1,
                cost_usd: 0.0,
                cache_marker: "cache:auto-or-none",
            },
            responder: tx,
        });
        rx
    }

    #[test]
    fn handle_key_maps_keystrokes_to_decisions() {
        let d = make_dashboard();
        for (key, expected) in [
            (
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                ConfirmDecision::Approve,
            ),
            (
                KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE),
                ConfirmDecision::Approve,
            ),
            (
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                ConfirmDecision::Abort,
            ),
            (
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                ConfirmDecision::Abort,
            ),
            (
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                ConfirmDecision::Abort,
            ),
        ] {
            let mut rx = make_pending(&d);
            handle_key(&d, key);
            assert_eq!(rx.try_recv().unwrap(), expected);
        }
    }

    #[test]
    fn handle_key_rejection_feedback_flow() {
        let d = make_dashboard();

        // 1. Initial pending prompt
        let mut rx = make_pending(&d);
        assert!(snap(&d).feedback_input.is_none());

        // 2. Press 'n' to enter feedback mode
        let key_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        handle_key(&d, key_n);

        // Assert no decision sent yet, and we are in feedback mode
        assert!(rx.try_recv().is_err());
        assert_eq!(snap(&d).feedback_input.as_deref(), Some(""));

        // 3. Type "f", "i", "x"
        for c in ['f', 'i', 'x'] {
            handle_key(&d, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(snap(&d).feedback_input.as_deref(), Some("fix"));

        // 4. Backspace
        handle_key(&d, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(snap(&d).feedback_input.as_deref(), Some("fi"));

        // 5. Enter to submit
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            rx.try_recv().unwrap(),
            ConfirmDecision::Reject(Some("fi".to_owned()))
        );
    }

    #[test]
    fn handle_key_rejection_empty_feedback_flow() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);

        // Press 'n'
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

        // Press Enter without typing feedback
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(rx.try_recv().unwrap(), ConfirmDecision::Reject(None));
    }

    #[test]
    fn handle_key_rejection_cancel_flow() {
        let d = make_dashboard();
        let _rx = make_pending(&d);

        // Press 'n'
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(snap(&d).feedback_input.is_some());

        // Press Esc
        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(snap(&d).feedback_input.is_none());
        assert!(snap(&d).pending.is_some());
    }

    #[test]
    fn handle_key_edit_flow() {
        let d = make_dashboard();

        // 1. Initial pending prompt
        let mut rx = make_pending(&d);
        assert!(snap(&d).edit_input.is_none());

        // 2. Press 'e' to enter edit mode (should pre-fill with pending command, which is "x" in make_pending)
        let key_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        handle_key(&d, key_e);

        // Assert no decision sent yet, and we are in edit mode pre-filled with "x"
        assert!(rx.try_recv().is_err());
        assert_eq!(snap(&d).edit_input.as_deref(), Some("x"));

        // 3. Type "y", "z" -> "xyz"
        for c in ['y', 'z'] {
            handle_key(&d, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(snap(&d).edit_input.as_deref(), Some("xyz"));

        // 4. Backspace -> "xy"
        handle_key(&d, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(snap(&d).edit_input.as_deref(), Some("xy"));

        // 5. Enter to submit
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            rx.try_recv().unwrap(),
            ConfirmDecision::Edit("xy".to_owned())
        );
    }

    #[test]
    fn handle_key_edit_flow_uppercase() {
        let d = make_dashboard();

        // 1. Initial pending prompt
        let mut rx = make_pending(&d);
        assert!(snap(&d).edit_input.is_none());

        // 2. Press 'E' to enter edit mode
        let key_e = KeyEvent::new(KeyCode::Char('E'), KeyModifiers::NONE);
        handle_key(&d, key_e);

        // Assert no decision sent yet, and we are in edit mode
        assert!(rx.try_recv().is_err());
        assert_eq!(snap(&d).edit_input.as_deref(), Some("x"));
    }

    #[test]
    fn handle_key_edit_flow_with_invalid_modifiers_is_ignored() {
        let d = make_dashboard();

        // 1. Initial pending prompt
        let mut rx = make_pending(&d);
        assert!(snap(&d).edit_input.is_none());

        // 2. Press 'Ctrl-e' (unsupported modifier) -> should be ignored, stay in choice screen
        let key_ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        handle_key(&d, key_ctrl_e);

        assert!(rx.try_recv().is_err());
        assert!(snap(&d).edit_input.is_none());
        assert!(snap(&d).pending.is_some());

        // 3. Press 'Alt-E' (unsupported modifier) -> should be ignored, stay in choice screen
        let key_alt_e = KeyEvent::new(KeyCode::Char('E'), KeyModifiers::ALT);
        handle_key(&d, key_alt_e);

        assert!(rx.try_recv().is_err());
        assert!(snap(&d).edit_input.is_none());
        assert!(snap(&d).pending.is_some());

        // 4. Press 'Shift-E' (supported modifier for uppercase E) -> should enter edit mode
        let key_shift_e = KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT);
        handle_key(&d, key_shift_e);

        assert!(rx.try_recv().is_err());
        assert_eq!(snap(&d).edit_input.as_deref(), Some("x"));
    }

    #[test]
    fn handle_key_edit_empty_cancels_flow() {
        let d = make_dashboard();
        let _rx = make_pending(&d);

        // Press 'e'
        handle_key(&d, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(snap(&d).edit_input.as_deref(), Some("x"));

        // Backspace to clear the command
        handle_key(&d, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(snap(&d).edit_input.as_deref(), Some(""));

        // Press Enter with empty command -> cancels (returns to choice screen, edit_input is None)
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(snap(&d).edit_input.is_none());
        assert!(snap(&d).pending.is_some());
    }

    #[test]
    fn handle_key_edit_cancel_flow() {
        let d = make_dashboard();
        let _rx = make_pending(&d);

        // Press 'e'
        handle_key(&d, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(snap(&d).edit_input.is_some());

        // Press Esc
        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(snap(&d).edit_input.is_none());
        assert!(snap(&d).pending.is_some());
    }

    #[test]
    fn handle_key_unknown_keystroke_keeps_prompt_open() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        // 'z' is not a documented key; the prompt should stay pending.
        let key = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        handle_key(&d, key);
        assert!(snap(&d).pending.is_some());
    }

    #[test]
    fn handle_key_ignores_release_events() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        let mut key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        key.kind = KeyEventKind::Release;
        handle_key(&d, key);
        // The release event shouldn't consume the prompt.
        assert!(snap(&d).pending.is_some());
    }

    #[test]
    fn handle_key_with_no_pending_is_noop() {
        let d = make_dashboard();
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        // Just verify it doesn't panic when state has no pending prompt.
        handle_key(&d, key);
        assert!(snap(&d).pending.is_none());
    }

    #[test]
    fn draw_renders_header_log_and_footer_against_test_backend() {
        let d = make_dashboard();
        d.emit(StreamEvent::RunStarted {
            task: "round trip".into(),
            model: "deterministic".into(),
            started_at: "t".into(),
        });
        d.emit(StreamEvent::BashStart {
            step: 1,
            command: "echo hi".into(),
            timestamp: "t".into(),
        });
        let s = snap(&d);
        let buf = render_to_buffer(&s, 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("round trip"),
            "task title should appear in header; got:\n{text}"
        );
        assert!(
            text.contains("deterministic"),
            "model name should appear; got:\n{text}"
        );
        assert!(
            text.contains("echo hi"),
            "bash line should appear in log; got:\n{text}"
        );
        assert!(
            text.contains("waiting for next agent step"),
            "footer hint should appear; got:\n{text}"
        );
    }

    #[test]
    fn draw_modal_renders_when_pending_prompt_present() {
        let d = make_dashboard();
        // Inject a pending prompt directly.
        {
            let (tx, _rx) = oneshot::channel();
            let mut s = d
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending = Some(PendingPrompt {
                ctx: ConfirmContext {
                    tool_name: "bash".into(),
                    command: "rm -rf /tmp/dangerous".into(),
                    step: 2,
                    step_limit: 5,
                    cost_usd: 0.0099,
                    cache_marker: "cache:explicit",
                },
                responder: tx,
            });
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 20);
        let text = buffer_text(&buf);
        assert!(
            text.contains("rm -rf /tmp/dangerous"),
            "modal should show command; got:\n{text}"
        );
        assert!(
            text.contains("(y) approve"),
            "modal should show key hint; got:\n{text}"
        );
        assert!(
            text.contains("step 2/5"),
            "modal should show step counter; got:\n{text}"
        );
    }

    #[test]
    fn draw_modal_marks_long_commands_with_ellipsis() {
        let d = make_dashboard();
        let big = (0..20)
            .map(|i| format!("line_{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        {
            let (tx, _rx) = oneshot::channel();
            let mut s = d
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending = Some(PendingPrompt {
                ctx: ConfirmContext {
                    tool_name: "bash".into(),
                    command: big,
                    step: 0,
                    step_limit: 1,
                    cost_usd: 0.0,
                    cache_marker: "cache:explicit",
                },
                responder: tx,
            });
        }
        let s = snap(&d);
        // Render tall so the 12-line cap + ellipsis line both fit
        // inside the modal area (the modal is 50% of the screen height).
        let buf = render_to_buffer(&s, 100, 50);
        let text = buffer_text(&buf);
        assert!(
            text.contains("…"),
            "long commands should be truncated with an ellipsis; got:\n{text}"
        );
    }

    #[test]
    fn footer_changes_when_run_finished() {
        let d = make_dashboard();
        d.emit(StreamEvent::RunEnded {
            exit_reason: "submitted".into(),
            failure_category: None,
            final_output: None,
            steps: 3,
            total_cost_usd: 0.1,
            ended_at: "t".into(),
        });
        let s = snap(&d);
        let buf = render_to_buffer(&s, 80, 8);
        let text = buffer_text(&buf);
        assert!(text.contains("run complete"));
    }

    #[test]
    fn centered_rect_is_subset_of_area() {
        let area = Rect::new(0, 0, 100, 50);
        let inner = centered_rect(50, 50, area);
        assert!(inner.x >= area.x);
        assert!(inner.y >= area.y);
        assert!(inner.x + inner.width <= area.x + area.width);
        assert!(inner.y + inner.height <= area.y + area.height);
    }

    #[test]
    fn handle_dashboard_handle_constructors_clone_arcs() {
        // Construct the handle without going through `start()` so we don't
        // mutate the terminal. Renderer task is fake — `oneshot::channel`
        // gives us a sender we drop immediately.
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
        let handle = RatatuiDashboardHandle {
            inner: make_dashboard(),
            shutdown_tx: Some(shutdown_tx),
            renderer_task: None,
        };
        let _sink = handle.stream_sink();
        let _cb = handle.confirm_callback();
    }
}
