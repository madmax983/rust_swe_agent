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
use tokio::sync::{Notify, oneshot, watch};

use super::confirm::{ConfirmCallback, ConfirmContext, ConfirmDecision};
use crate::stream::{StreamEvent, StreamSink};

const MAX_LOG_LINES: usize = 400;
const MAX_RETAINED_ENTRY_BYTES: usize = 100 * 1024; // 100 KB cap per entry

fn truncate_to_cap(mut s: String) -> String {
    if s.len() > MAX_RETAINED_ENTRY_BYTES {
        let mut cut = MAX_RETAINED_ENTRY_BYTES;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n... [truncated due to entry size cap] ...");
    }
    s
}

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
    active_rules: Vec<String>,
    selected_index: Option<usize>,
    feed_scroll_top: usize,
    viewport_height: u16,
    detail_open: bool,
    detail_scroll_top: usize,
    detail_viewport_height: u16,
    detail_viewport_width: u16,
    scroll_offset: usize,
    auto_follow: bool,
    last_log_width: usize,
    last_log_height: usize,
    should_exit: bool,
    is_monitor: bool,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            task: None,
            model: None,
            started_at: None,
            cost_usd: 0.0,
            step: 0,
            step_limit: 0,
            log: VecDeque::new(),
            pending: None,
            finished: None,
            feedback_input: None,
            edit_input: None,
            active_rules: Vec::new(),
            selected_index: None,
            feed_scroll_top: 0,
            viewport_height: 20,
            detail_open: false,
            detail_scroll_top: 0,
            detail_viewport_height: 10,
            detail_viewport_width: 80,
            scroll_offset: 0,
            auto_follow: true,
            last_log_width: 80,
            last_log_height: 20,
            should_exit: false,
            is_monitor: false,
        }
    }
}

#[derive(Clone)]
struct LogLine {
    kind: LineKind,
    text: String,
    full_text: Option<Arc<str>>,
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
        loop {
            let notification = self.inner.notify.notified();
            let (finished, should_exit) = {
                let s = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (s.finished.is_some(), s.should_exit)
            };
            if !finished || should_exit {
                break;
            }
            notification.await;
        }

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
    cancel_tx: Option<watch::Sender<bool>>,
}

impl RatatuiDashboard {
    /// Enter alt-screen + raw mode and spawn the renderer task.
    ///
    /// # Errors
    /// Returns `Err` if the terminal cannot be put into raw mode or
    /// alt-screen mode. On failure the terminal is restored to cooked
    /// mode and the alt-screen is left, so the caller never observes a
    /// half-initialised terminal.
    pub fn start(
        is_monitor: bool,
        cancel_tx: Option<watch::Sender<bool>>,
    ) -> std::io::Result<RatatuiDashboardHandle> {
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
            state: Mutex::new(DashboardState {
                is_monitor,
                ..DashboardState::default()
            }),
            notify: Notify::new(),
            cancel_tx,
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let renderer_task = tokio::spawn(renderer_loop(dash.clone(), terminal, shutdown_rx));
        Ok(RatatuiDashboardHandle {
            inner: dash,
            shutdown_tx: Some(shutdown_tx),
            renderer_task: Some(renderer_task),
        })
    }

    fn append(&self, kind: LineKind, text: impl Into<String>, full_text: Option<String>) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old_len = s.log.len();
        let was_at_end =
            s.selected_index.is_none() || (old_len > 0 && s.selected_index == Some(old_len - 1));

        let popped = if s.log.len() >= MAX_LOG_LINES {
            s.log.pop_front();
            true
        } else {
            false
        };
        s.log.push_back(LogLine {
            kind,
            text: text.into(),
            full_text: full_text.map(|t| Arc::from(truncate_to_cap(t))),
        });

        let auto_follow_selection = was_at_end && !s.detail_open;
        if auto_follow_selection {
            s.selected_index = Some(s.log.len() - 1);
            let visible_height = s.viewport_height as usize;
            if visible_height > 0 && s.log.len() >= visible_height {
                s.feed_scroll_top = s.log.len() - visible_height;
            }
        } else if popped {
            if let Some(idx) = s.selected_index {
                s.selected_index = Some(idx.saturating_sub(1));
            }
            s.feed_scroll_top = s.feed_scroll_top.saturating_sub(1);
        }
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
    #[allow(clippy::too_many_lines)]
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
                self.append(
                    LineKind::Info,
                    format!("run started: {model} :: {task}"),
                    None,
                );
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
                    Some(content),
                );
            }
            StreamEvent::BashStart { step, command, .. } => {
                let preview = first_lines(&command, 4);
                self.append(
                    LineKind::BashRun,
                    format!("step {step} bash: {preview}"),
                    Some(command),
                );
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
                let full_content = if stdout.is_empty() && stderr.is_empty() {
                    None
                } else if stderr.is_empty() {
                    Some(stdout)
                } else if stdout.is_empty() {
                    Some(stderr)
                } else {
                    Some(format!("{stdout}\n--- stderr ---\n{stderr}"))
                };
                self.append(kind, summary, full_content);
            }
            StreamEvent::Observation { step, content, .. } => {
                let preview = first_lines(&content, 4);
                self.append(
                    LineKind::Observation,
                    format!("step {step} observation: {preview}"),
                    Some(content),
                );
            }
            StreamEvent::FormatError { step, content, .. } => {
                let preview = first_lines(&content, 4);
                self.append(
                    LineKind::Warn,
                    format!("step {step} format error: {preview}"),
                    Some(content),
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
                    None,
                );
            }
            StreamEvent::AutoApproveRuleCreated { scope } => {
                let mut s = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !s.active_rules.contains(&scope) {
                    s.active_rules.push(scope.clone());
                }
                drop(s);
                self.notify.notify_waiters();
                self.append(
                    LineKind::Info,
                    format!("auto-approve rule created for: {scope}"),
                    None,
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
            s.detail_open = false;
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
        let exit = {
            let s = dash
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.should_exit
        };
        if exit {
            break;
        }

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

fn perform_scroll(s: &mut DashboardState, code: KeyCode) -> bool {
    let total = total_wrapped_lines(&s.log, s.last_log_width);
    let max_scroll = total.saturating_sub(s.last_log_height);
    let current_offset = if s.auto_follow {
        max_scroll
    } else {
        s.scroll_offset
    };

    match code {
        KeyCode::Up => {
            let next_offset = current_offset.saturating_sub(1);
            if next_offset != current_offset {
                s.scroll_offset = next_offset;
                s.auto_follow = false;
            }
            true
        }
        KeyCode::Down => {
            let next_offset = (current_offset + 1).min(max_scroll);
            if next_offset != current_offset {
                s.scroll_offset = next_offset;
                s.auto_follow = false;
            }
            true
        }
        KeyCode::PageUp => {
            let next_offset = current_offset.saturating_sub(s.last_log_height);
            if next_offset != current_offset {
                s.scroll_offset = next_offset;
                s.auto_follow = false;
            }
            true
        }
        KeyCode::PageDown => {
            let next_offset = (current_offset + s.last_log_height).min(max_scroll);
            if next_offset != current_offset {
                s.scroll_offset = next_offset;
                s.auto_follow = false;
            }
            true
        }
        KeyCode::Home => {
            let next_offset = 0;
            if next_offset != current_offset {
                s.scroll_offset = next_offset;
                s.auto_follow = false;
            }
            true
        }
        KeyCode::End => {
            s.scroll_offset = max_scroll;
            s.auto_follow = true;
            true
        }
        _ => false,
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

    if perform_scroll(s, key.code) {
        s.pending = Some(pending);
        dash.notify.notify_waiters();
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
        KeyCode::Char('A') => {
            let scope = pending.ctx.derive_scope();
            let _ = pending.responder.send(ConfirmDecision::AutoApprove(scope));
        }
        KeyCode::Char('a') | KeyCode::Esc => {
            let _ = pending.responder.send(ConfirmDecision::Abort);
        }
        _ => {
            s.pending = Some(pending);
        }
    }
}

fn move_cursor_up(s: &mut DashboardState) {
    if s.log.is_empty() {
        return;
    }
    let current = s.selected_index.unwrap_or(s.log.len() - 1);
    let new_selected = current.saturating_sub(1);
    s.selected_index = Some(new_selected);

    if new_selected < s.feed_scroll_top {
        s.feed_scroll_top = new_selected;
    }
}

fn move_cursor_down(s: &mut DashboardState) {
    if s.log.is_empty() {
        return;
    }
    let current = s.selected_index.unwrap_or(s.log.len() - 1);
    let new_selected = (current + 1).min(s.log.len() - 1);
    s.selected_index = Some(new_selected);

    let visible_height = s.viewport_height as usize;
    if visible_height > 0 && new_selected >= s.feed_scroll_top + visible_height {
        s.feed_scroll_top = new_selected + 1 - visible_height;
    }
}

fn get_max_detail_scroll(s: &DashboardState) -> usize {
    let full_text = s
        .selected_index
        .and_then(|idx| s.log.get(idx))
        .map_or("", |line| {
            line.full_text.as_deref().unwrap_or(line.text.as_str())
        });
    let wrapped_lines = wrap_text(full_text, s.detail_viewport_width as usize);
    let lines_count = wrapped_lines.len();
    let viewport_h = s.detail_viewport_height as usize;
    lines_count.saturating_sub(viewport_h)
}

#[allow(clippy::too_many_lines)]
fn handle_key(dash: &Arc<RatatuiDashboard>, key: KeyEvent) {
    if !matches!(key.kind, KeyEventKind::Press) {
        return;
    }
    let mut s = dash
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'));

    if let Some(pending) = s.pending.take() {
        if ctrl_c {
            s.feedback_input = None;
            s.edit_input = None;
            drop(s);
            let _ = pending.responder.send(ConfirmDecision::Abort);
            return;
        }

        if let Some(buffer) = s.feedback_input.take() {
            handle_key_feedback_input(dash, &mut s, pending, key.code, buffer);
        } else if let Some(buffer) = s.edit_input.take() {
            handle_key_edit_input(dash, &mut s, pending, key.code, buffer);
        } else if s.detail_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q' | 'Q') => {
                    s.detail_open = false;
                    s.pending = Some(pending);
                    dash.notify.notify_waiters();
                }
                KeyCode::Up | KeyCode::Char('k' | 'K') => {
                    s.detail_scroll_top = s.detail_scroll_top.saturating_sub(1);
                    s.pending = Some(pending);
                    dash.notify.notify_waiters();
                }
                KeyCode::Down | KeyCode::Char('j' | 'J') => {
                    let max_scroll = get_max_detail_scroll(&s);
                    s.detail_scroll_top = (s.detail_scroll_top + 1).min(max_scroll);
                    s.pending = Some(pending);
                    dash.notify.notify_waiters();
                }
                KeyCode::PageUp => {
                    let page_size = s.detail_viewport_height as usize;
                    s.detail_scroll_top = s.detail_scroll_top.saturating_sub(page_size);
                    s.pending = Some(pending);
                    dash.notify.notify_waiters();
                }
                KeyCode::PageDown => {
                    let page_size = s.detail_viewport_height as usize;
                    let max_scroll = get_max_detail_scroll(&s);
                    s.detail_scroll_top = (s.detail_scroll_top + page_size).min(max_scroll);
                    s.pending = Some(pending);
                    dash.notify.notify_waiters();
                }
                KeyCode::Home => {
                    s.detail_scroll_top = 0;
                    s.pending = Some(pending);
                    dash.notify.notify_waiters();
                }
                KeyCode::End => {
                    s.detail_scroll_top = get_max_detail_scroll(&s);
                    s.pending = Some(pending);
                    dash.notify.notify_waiters();
                }
                _ => {
                    s.pending = Some(pending);
                }
            }
        } else {
            let mut handled_by_navigation = false;
            if !s.is_monitor {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k' | 'K') => {
                        move_cursor_up(&mut s);
                        dash.notify.notify_waiters();
                        handled_by_navigation = true;
                    }
                    KeyCode::Down | KeyCode::Char('j' | 'J') => {
                        move_cursor_down(&mut s);
                        dash.notify.notify_waiters();
                        handled_by_navigation = true;
                    }
                    KeyCode::Enter => {
                        if let Some(idx) = s.selected_index {
                            if idx < s.log.len() {
                                s.detail_open = true;
                                s.detail_scroll_top = 0;
                                dash.notify.notify_waiters();
                            }
                        }
                        handled_by_navigation = true;
                    }
                    _ => {}
                }
            }

            if handled_by_navigation {
                s.pending = Some(pending);
            } else {
                handle_key_normal(dash, &mut s, pending, key);
            }
        }
    } else {
        // No modal open
        if ctrl_c && s.finished.is_none() {
            if let Some(ref tx) = dash.cancel_tx {
                let _ = tx.send(true);
            }
        }

        if s.detail_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q' | 'Q') => {
                    s.detail_open = false;
                    dash.notify.notify_waiters();
                }
                KeyCode::Up | KeyCode::Char('k' | 'K') => {
                    s.detail_scroll_top = s.detail_scroll_top.saturating_sub(1);
                    dash.notify.notify_waiters();
                }
                KeyCode::Down | KeyCode::Char('j' | 'J') => {
                    let max_scroll = get_max_detail_scroll(&s);
                    s.detail_scroll_top = (s.detail_scroll_top + 1).min(max_scroll);
                    dash.notify.notify_waiters();
                }
                KeyCode::PageUp => {
                    let page_size = s.detail_viewport_height as usize;
                    s.detail_scroll_top = s.detail_scroll_top.saturating_sub(page_size);
                    dash.notify.notify_waiters();
                }
                KeyCode::PageDown => {
                    let page_size = s.detail_viewport_height as usize;
                    let max_scroll = get_max_detail_scroll(&s);
                    s.detail_scroll_top = (s.detail_scroll_top + page_size).min(max_scroll);
                    dash.notify.notify_waiters();
                }
                KeyCode::Home => {
                    s.detail_scroll_top = 0;
                    dash.notify.notify_waiters();
                }
                KeyCode::End => {
                    s.detail_scroll_top = get_max_detail_scroll(&s);
                    dash.notify.notify_waiters();
                }
                _ => {
                    if s.finished.is_some() && ctrl_c {
                        s.should_exit = true;
                        drop(s);
                        dash.notify.notify_waiters();
                    }
                }
            }
        } else {
            if s.finished.is_some() {
                let close_key =
                    matches!(key.code, KeyCode::Char('q' | 'Q') | KeyCode::Esc) || ctrl_c;
                if close_key {
                    s.should_exit = true;
                    drop(s);
                    dash.notify.notify_waiters();
                    return;
                }
            }

            if s.is_monitor {
                if perform_scroll(&mut s, key.code) {
                    drop(s);
                    dash.notify.notify_waiters();
                }
            } else {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k' | 'K') => {
                        move_cursor_up(&mut s);
                        dash.notify.notify_waiters();
                    }
                    KeyCode::Down | KeyCode::Char('j' | 'J') => {
                        move_cursor_down(&mut s);
                        dash.notify.notify_waiters();
                    }
                    KeyCode::Enter => {
                        if let Some(idx) = s.selected_index {
                            if idx < s.log.len() {
                                s.detail_open = true;
                                s.detail_scroll_top = 0;
                                dash.notify.notify_waiters();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw_frame(
    dash: &Arc<RatatuiDashboard>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> std::io::Result<()> {
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);
    let log_chunk = chunks[1];
    let log_width = log_chunk.width.saturating_sub(2) as usize;
    let log_height = log_chunk.height.saturating_sub(2) as usize;

    let snapshot = {
        let mut s = dash
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.last_log_width = log_width;
        s.last_log_height = log_height;
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
            active_rules: s.active_rules.clone(),
            selected_index: s.selected_index,
            feed_scroll_top: s.feed_scroll_top,
            detail_open: s.detail_open,
            detail_scroll_top: s.detail_scroll_top,
            scroll_offset: s.scroll_offset,
            auto_follow: s.auto_follow,
            last_log_width: s.last_log_width,
            last_log_height: s.last_log_height,
            is_monitor: s.is_monitor,
        }
    };
    terminal.draw(|frame| draw(frame, dash, &snapshot))?;
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
    active_rules: Vec<String>,
    selected_index: Option<usize>,
    feed_scroll_top: usize,
    detail_open: bool,
    detail_scroll_top: usize,
    scroll_offset: usize,
    auto_follow: bool,
    last_log_width: usize,
    last_log_height: usize,
    is_monitor: bool,
}

fn draw(frame: &mut ratatui::Frame, dash: &Arc<RatatuiDashboard>, snap: &DashboardSnapshot) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    let inner_feed_height = chunks[1].height.saturating_sub(2);
    {
        let mut s = dash
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.viewport_height = inner_feed_height;
        // Keep selected_index visible by adjusting feed_scroll_top
        if let Some(idx) = s.selected_index {
            let visible_height = inner_feed_height as usize;
            if visible_height > 0 {
                if idx < s.feed_scroll_top {
                    s.feed_scroll_top = idx;
                } else if idx >= s.feed_scroll_top + visible_height {
                    s.feed_scroll_top = idx + 1 - visible_height;
                }
            }
        }
    }

    frame.render_widget(header_paragraph(snap), chunks[0]);
    frame.render_widget(log_paragraph(snap, inner_feed_height as usize), chunks[1]);
    frame.render_widget(footer_paragraph(snap), chunks[2]);

    if snap.detail_open {
        let detail_area = chunks[1];
        frame.render_widget(Clear, detail_area);

        let inner_detail_height = detail_area.height.saturating_sub(2);
        let inner_detail_width = detail_area.width.saturating_sub(2);
        {
            let mut s = dash
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.detail_viewport_height = inner_detail_height;
            s.detail_viewport_width = inner_detail_width;
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" detail inspector ");

        let full_text = snap
            .selected_index
            .and_then(|idx| snap.log.get(idx))
            .map_or("", |line| {
                line.full_text.as_deref().unwrap_or(line.text.as_str())
            });

        let wrapped_lines = wrap_text(full_text, inner_detail_width as usize);
        let display_lines: Vec<Line> = wrapped_lines
            .iter()
            .skip(snap.detail_scroll_top)
            .take(inner_detail_height as usize)
            .map(|l| Line::from(l.as_str()))
            .collect();

        let p = Paragraph::new(display_lines).block(block);
        frame.render_widget(p, detail_area);
    }

    if let Some(ctx) = &snap.pending {
        if !snap.detail_open {
            draw_modal(
                frame,
                ctx,
                snap.feedback_input.as_ref(),
                snap.edit_input.as_ref(),
                area,
            );
        }
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
    let block_title = if snap.is_monitor {
        " maxwell's daemon — monitor ".to_string()
    } else if snap.active_rules.is_empty() {
        " maxwell's daemon — interactive ".to_string()
    } else {
        format!(
            " maxwell's daemon — interactive [auto-approve: {}] ",
            snap.active_rules.join(", ")
        )
    };
    Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(block_title))
}

fn log_paragraph(snap: &DashboardSnapshot, visible_lines: usize) -> Paragraph<'_> {
    let items = if visible_lines > 0 {
        snap.log
            .iter()
            .skip(snap.feed_scroll_top)
            .take(visible_lines)
            .collect::<Vec<_>>()
    } else {
        snap.log
            .iter()
            .skip(snap.feed_scroll_top)
            .collect::<Vec<_>>()
    };

    let lines: Vec<Line> = items
        .into_iter()
        .enumerate()
        .map(|(offset, l)| {
            let idx = snap.feed_scroll_top + offset;
            let is_selected = Some(idx) == snap.selected_index;
            let mut style = match l.kind {
                LineKind::Info => Style::default().fg(Color::Gray),
                LineKind::AssistantMsg => Style::default().fg(Color::Cyan),
                LineKind::BashRun => Style::default().fg(Color::White),
                LineKind::BashOk => Style::default().fg(Color::Green),
                LineKind::BashErr => Style::default().fg(Color::Red),
                LineKind::Observation => Style::default().fg(Color::LightBlue),
                LineKind::Warn => Style::default().fg(Color::LightYellow),
            };
            if is_selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::from(Span::styled(l.text.clone(), style))
        })
        .collect();

    let (scroll_y_u16, title) = if snap.is_monitor {
        let total = total_wrapped_lines(&snap.log, snap.last_log_width);
        let max_scroll = total.saturating_sub(snap.last_log_height);
        let scroll_y = if snap.auto_follow {
            max_scroll
        } else {
            snap.scroll_offset.min(max_scroll)
        };
        let t = if snap.auto_follow {
            " trajectory [LIVE] "
        } else {
            " trajectory [SCROLLED] "
        };
        #[allow(clippy::cast_possible_truncation)]
        (scroll_y.min(u16::MAX as usize) as u16, t)
    } else {
        (0, " trajectory ")
    };

    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((scroll_y_u16, 0))
}

fn footer_paragraph(snap: &DashboardSnapshot) -> Paragraph<'_> {
    let hint = if snap.detail_open {
        "[Esc/q] close   [Up/Down/j/k] scroll   [PgUp/PgDn] page   [Home/End] bounds".to_string()
    } else if snap.finished.is_some() {
        "run complete — press 'q' or Ctrl-C to close".to_string()
    } else if snap.edit_input.is_some() {
        "[Enter] execute edit   [Esc] cancel".to_string()
    } else if let Some(pending) = &snap.pending {
        let scope = pending.derive_scope();
        format!(
            "(y) approve   (n) reject   (e) edit   (a) abort   (A) auto-approve {scope}   [Up/Down] navigate   [Enter] inspect"
        )
    } else {
        "waiting for next agent step…   [Up/Down] navigate   [Enter] inspect".to_string()
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
        let edit_lines: Vec<&str> = buffer
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .collect();
        for (i, line) in edit_lines.iter().enumerate() {
            let prefix = if i == 0 { " > " } else { "   " };
            if i == edit_lines.len() - 1 {
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Green)),
                    Span::styled((*line).to_string(), Style::default().fg(Color::White)),
                    Span::styled("█", Style::default().fg(Color::Green)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Green)),
                    Span::styled((*line).to_string(), Style::default().fg(Color::White)),
                ]));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[Enter] execute edit   [Esc] cancel",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        let scope = ctx.derive_scope();
        lines.push(Line::from(Span::styled(
            format!("(y) approve   (n) reject   (e) edit   (a) abort   (A) auto-approve {scope}"),
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

fn total_wrapped_lines<'a>(log: impl IntoIterator<Item = &'a LogLine>, _width: usize) -> usize {
    log.into_iter().count()
}

#[cfg(test)]
fn count_wrapped_lines(text: &str, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let mut total_lines = 0;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        total_lines += count_wrapped_line(line, width);
    }
    total_lines
}

#[cfg(test)]
fn count_wrapped_line(line: &str, width: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    if line.is_empty() {
        return 1;
    }
    let mut total_lines = 0;
    let mut current_width = 0;
    let mut word_width = 0;
    let mut space_width = 0;

    for g in line.graphemes(true) {
        if g == " " {
            if word_width > 0 {
                if current_width + word_width <= width {
                    current_width += word_width;
                } else {
                    total_lines += 1;
                    current_width = word_width;
                }
                word_width = 0;
            }
            space_width += 1;
        } else {
            if space_width > 0 {
                let remaining_on_line = width - current_width;
                if space_width <= remaining_on_line {
                    current_width += space_width;
                } else {
                    total_lines += 1;
                    let mut rem = space_width - remaining_on_line;
                    rem = rem.saturating_sub(1);
                    while rem > width {
                        total_lines += 1;
                        rem = rem.saturating_sub(width + 1);
                    }
                    current_width = rem;
                }
                space_width = 0;
            }
            let g_width = g.width();
            if g_width > width {
                continue;
            }
            word_width += g_width;
            if word_width > width {
                total_lines += 1;
                word_width = g_width;
                current_width = 0;
            }
        }
    }

    if space_width > 0 {
        let remaining_on_line = width - current_width;
        if space_width <= remaining_on_line {
            current_width += space_width;
        } else {
            total_lines += 1;
            let mut rem = space_width - remaining_on_line;
            rem = rem.saturating_sub(1);
            while rem > width {
                total_lines += 1;
                rem = rem.saturating_sub(width + 1);
            }
            current_width = rem;
        }
    } else if word_width > 0 {
        if current_width + word_width <= width {
            current_width += word_width;
        } else {
            total_lines += 1;
            current_width = word_width;
        }
    }

    if current_width > 0 {
        total_lines += 1;
    }
    total_lines
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut wrapped = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        wrapped.extend(wrap_line(line, width));
    }
    wrapped
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    if line.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    let mut current_word = String::new();
    let mut word_width = 0;

    let mut current_spaces = String::new();
    let mut space_width = 0;

    for g in line.graphemes(true) {
        if g == " " {
            if word_width > 0 {
                if current_width + word_width <= width {
                    current_line.push_str(&current_word);
                    current_width += word_width;
                } else {
                    lines.push(std::mem::take(&mut current_line));
                    current_line = current_word.clone();
                    current_width = word_width;
                }
                current_word.clear();
                word_width = 0;
            }
            current_spaces.push_str(g);
            space_width += 1;
        } else {
            if space_width > 0 {
                let remaining_on_line = width - current_width;
                if space_width <= remaining_on_line {
                    current_line.push_str(&current_spaces);
                    current_width += space_width;
                } else {
                    lines.push(std::mem::take(&mut current_line));
                    let mut rem = space_width - remaining_on_line;
                    rem = rem.saturating_sub(1);
                    while rem > width {
                        lines.push(String::new());
                        rem = rem.saturating_sub(width + 1);
                    }
                    current_line = " ".repeat(rem);
                    current_width = rem;
                }
                current_spaces.clear();
                space_width = 0;
            }
            let g_width = g.width();
            if g_width > width {
                continue;
            }
            current_word.push_str(g);
            word_width += g_width;
            if word_width > width {
                if !current_line.is_empty() {
                    lines.push(std::mem::take(&mut current_line));
                }
                let mut prev_word = current_word.clone();
                let g_len = g.len();
                prev_word.truncate(prev_word.len() - g_len);

                lines.push(prev_word);
                current_word = g.to_string();
                word_width = g_width;
                current_line.clear();
                current_width = 0;
            }
        }
    }

    if space_width > 0 {
        let remaining_on_line = width - current_width;
        if space_width <= remaining_on_line {
            current_line.push_str(&current_spaces);
            current_width += space_width;
        } else {
            lines.push(std::mem::take(&mut current_line));
            let mut rem = space_width - remaining_on_line;
            rem = rem.saturating_sub(1);
            while rem > width {
                lines.push(String::new());
                rem = rem.saturating_sub(width + 1);
            }
            current_line = " ".repeat(rem);
            current_width = rem;
        }
    } else if word_width > 0 {
        if current_width + word_width <= width {
            current_line.push_str(&current_word);
            current_width += word_width;
        } else {
            lines.push(std::mem::take(&mut current_line));
            current_line = current_word;
            current_width = word_width;
        }
    }

    if current_width > 0 || current_line.is_empty() && lines.is_empty() {
        lines.push(std::mem::take(&mut current_line));
    }
    lines
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn push_info(s: &mut DashboardState, text: &str) {
        s.log.push_back(LogLine {
            kind: LineKind::Info,
            text: text.to_string(),
            full_text: None,
        });
    }

    #[test]
    fn test_dashboard_state_defaults() {
        let state = DashboardState::default();
        assert_eq!(state.scroll_offset, 0);
        assert!(state.auto_follow);
        assert_eq!(state.last_log_width, 80);
        assert_eq!(state.last_log_height, 20);
        assert!(!state.should_exit);
    }

    #[test]
    fn test_count_wrapped_lines_greedy() {
        assert_eq!(count_wrapped_lines("", 10), 1);
        assert_eq!(count_wrapped_lines("hello", 10), 1);
        assert_eq!(count_wrapped_lines("hello", 5), 1);
        assert_eq!(count_wrapped_lines("hello world", 5), 2);
        assert_eq!(count_wrapped_lines("hello world", 8), 2);
        assert_eq!(count_wrapped_lines("hello world", 11), 1);
        assert_eq!(count_wrapped_lines("hello\nworld", 10), 2);
        assert_eq!(count_wrapped_lines("supercalifragilistic", 5), 4);
        assert_eq!(count_wrapped_lines("  hello", 5), 2);
        assert_eq!(count_wrapped_lines("🦀🦀", 5), 1);
        assert_eq!(count_wrapped_lines("🦀🦀", 3), 2);
        assert_eq!(count_wrapped_lines("👨‍👩‍👧‍👦", 5), 1);
        assert_eq!(count_wrapped_lines("👨‍👩‍👧‍👦👨‍👩‍👧‍👦", 3), 2);
    }

    #[test]
    fn test_scroll_navigation_keys() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 5;
            s.last_log_width = 10;
            s.is_monitor = true;
            push_info(&mut s, "line1");
            push_info(&mut s, "line2");
            push_info(&mut s, "line3");
            push_info(&mut s, "line4");
            push_info(&mut s, "line5");
            push_info(&mut s, "line6");
            push_info(&mut s, "line7");
            push_info(&mut s, "line8");
        }

        handle_key(&d, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(!auto_follow);
            assert_eq!(scroll_offset, 2);
        }

        handle_key(&d, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(!auto_follow);
            assert_eq!(scroll_offset, 3);
        }

        handle_key(&d, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(!auto_follow);
            assert_eq!(scroll_offset, 0);
        }

        handle_key(&d, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(!auto_follow);
            assert_eq!(scroll_offset, 3);
        }

        handle_key(&d, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(auto_follow);
            assert_eq!(scroll_offset, 3);
        }

        handle_key(&d, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(auto_follow);
            assert_eq!(scroll_offset, 3);
        }
    }

    #[test]
    fn test_scroll_navigation_keys_when_modal_open() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 5;
            s.last_log_width = 10;
            push_info(&mut s, "line1");
            push_info(&mut s, "line2");
            push_info(&mut s, "line3");
            push_info(&mut s, "line4");
            push_info(&mut s, "line5");
            push_info(&mut s, "line6");
            push_info(&mut s, "line7");
            push_info(&mut s, "line8");
        }

        // Send PageUp key. It should scroll the log but keep s.pending (modal) intact!
        handle_key(&d, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            let pending_is_some = s.pending.is_some();
            drop(s);
            assert!(!auto_follow);
            assert_eq!(scroll_offset, 0); // scrolled up from 3 by height 5, clamped to 0
            assert!(pending_is_some); // Modal is still open!
        }

        // Send End key. It should re-engage auto-follow and keep modal intact!
        handle_key(&d, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            let pending_is_some = s.pending.is_some();
            drop(s);
            assert!(auto_follow);
            assert_eq!(scroll_offset, 3);
            assert!(pending_is_some); // Modal is still open!
        }
    }

    #[test]
    fn test_close_keys_when_finished() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.finished = Some("completed".into());
        }

        handle_key(&d, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let should_exit = s.should_exit;
            drop(s);
            assert!(should_exit);
        }

        {
            let mut s = d.state.lock().unwrap();
            s.should_exit = false;
        }
        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let should_exit = s.should_exit;
            drop(s);
            assert!(should_exit);
        }
    }

    #[test]
    fn test_render_scrolled_vs_live() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 3;
            s.last_log_width = 10;
            s.is_monitor = true;
            push_info(&mut s, "line1");
            push_info(&mut s, "line2");
            push_info(&mut s, "line3");
            push_info(&mut s, "line4");
            push_info(&mut s, "line5");
        }

        let s_live = snap(&d);
        let buf_live = render_to_buffer(&s_live, 40, 9);
        let text_live = buffer_text(&buf_live);
        assert!(text_live.contains("[LIVE]"));
        assert!(!text_live.contains("[SCROLLED]"));

        {
            let mut s = d.state.lock().unwrap();
            s.auto_follow = false;
            s.scroll_offset = 1;
        }
        let s_scrolled = snap(&d);
        let buf_scrolled = render_to_buffer(&s_scrolled, 40, 9);
        let text_scrolled = buffer_text(&buf_scrolled);
        assert!(text_scrolled.contains("[SCROLLED]"));
        assert!(!text_scrolled.contains("[LIVE]"));
    }

    #[test]
    fn test_interactive_mode_does_not_scroll() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 3;
            s.last_log_width = 10;
            s.is_monitor = false;
            push_info(&mut s, "line1");
            push_info(&mut s, "line2");
            push_info(&mut s, "line3");
            push_info(&mut s, "line4");
            push_info(&mut s, "line5");
        }

        let s_interactive = snap(&d);
        let buf = render_to_buffer(&s_interactive, 40, 9);
        let text = buffer_text(&buf);
        assert!(text.contains(" trajectory "));
        assert!(!text.contains("[LIVE]"));
        assert!(!text.contains("[SCROLLED]"));
    }

    #[test]
    fn test_scrollback_success_metric() {
        let d = make_dashboard();
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(auto_follow);
            assert_eq!(scroll_offset, 0);
        }

        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 5;
            s.last_log_width = 80;
            s.is_monitor = true;
        }

        d.emit(StreamEvent::RunStarted {
            task: "testing e2e".into(),
            model: "claude-3-5".into(),
            started_at: "t".into(),
        });
        for i in 1..=10 {
            d.emit(StreamEvent::Observation {
                step: i,
                content: format!("observation content {i}"),
                timestamp: "t".into(),
            });
        }

        {
            let s = snap(&d);
            let buf = render_to_buffer(&s, 80, 11);
            let text = buffer_text(&buf);
            assert!(text.contains("[LIVE]"));
        }

        handle_key(&d, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(!auto_follow);
            assert_eq!(scroll_offset, 5);
        }

        d.emit(StreamEvent::Observation {
            step: 11,
            content: "new scrolled-away event".into(),
            timestamp: "t".into(),
        });
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(!auto_follow);
            assert_eq!(scroll_offset, 5);
        }

        handle_key(&d, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let auto_follow = s.auto_follow;
            let scroll_offset = s.scroll_offset;
            drop(s);
            assert!(auto_follow);
            assert_eq!(scroll_offset, 7);
        }

        d.emit(StreamEvent::RunEnded {
            exit_reason: "success".into(),
            failure_category: None,
            final_output: None,
            steps: 12,
            total_cost_usd: 0.0,
            ended_at: "t".into(),
        });
        {
            let s = d.state.lock().unwrap();
            let finished_is_some = s.finished.is_some();
            let should_exit = s.should_exit;
            drop(s);
            assert!(finished_is_some);
            assert!(!should_exit);
        }

        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let should_exit = s.should_exit;
            drop(s);
            assert!(should_exit);
        }
    }

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

    #[test]
    fn test_truncate_to_cap() {
        let under_limit = "a".repeat(10);
        assert_eq!(truncate_to_cap(under_limit.clone()), under_limit);

        let over_limit = "a".repeat(MAX_RETAINED_ENTRY_BYTES + 10);
        let truncated = truncate_to_cap(over_limit);
        assert!(truncated.len() <= MAX_RETAINED_ENTRY_BYTES + 50);
        assert!(truncated.contains("truncated"));
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
            cancel_tx: None,
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
            active_rules: s.active_rules.clone(),
            selected_index: s.selected_index,
            feed_scroll_top: s.feed_scroll_top,
            detail_open: s.detail_open,
            detail_scroll_top: s.detail_scroll_top,
            scroll_offset: s.scroll_offset,
            auto_follow: s.auto_follow,
            last_log_width: s.last_log_width,
            last_log_height: s.last_log_height,
            is_monitor: s.is_monitor,
        }
    }

    fn render_to_buffer(snap: &DashboardSnapshot, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = RatatuiTerminal::new(backend).unwrap();
        let dash = make_dashboard();
        terminal.draw(|frame| draw(frame, &dash, snap)).unwrap();
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
            d.append(LineKind::Info, "x", None);
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
    fn handle_key_ctrl_c_cancels_before_completion() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let d = Arc::new(RatatuiDashboard {
            state: Mutex::new(DashboardState::default()),
            notify: Notify::new(),
            cancel_tx: Some(tx),
        });

        // 1. Initially not cancelled
        assert!(!*rx.borrow());

        // 2. Press Ctrl-C when run is not finished
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key(&d, key);

        // 3. Verify that cancellation was triggered
        assert!(*rx.borrow());
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
    fn draw_modal_renders_multiline_edit_buffer() {
        let d = make_dashboard();
        {
            let (tx, _rx) = oneshot::channel();
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
            s.edit_input = Some("first line\nsecond line\nthird line".to_string());
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 30);
        let text = buffer_text(&buf);

        assert!(
            text.contains(" > first line"),
            "modal should format first line with ' > '; got:\n{text}"
        );
        assert!(
            text.contains("   second line"),
            "modal should format second line with '   '; got:\n{text}"
        );
        assert!(
            text.contains("   third line█"),
            "modal should format third line with '   ' and append cursor; got:\n{text}"
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

    #[test]
    fn tui_processes_auto_approve_rule_created_event() {
        let dash = make_dashboard();
        assert!(snap(&dash).active_rules.is_empty());

        // Emit event
        dash.emit(StreamEvent::AutoApproveRuleCreated {
            scope: "cargo".to_string(),
        });

        // Assert state updated
        let s = snap(&dash);
        assert_eq!(s.active_rules, vec!["cargo".to_string()]);

        // Render and check header block title
        let buf = render_to_buffer(&s, 80, 24);
        // Check that block title contains "[auto-approve: cargo]"
        let header_text = buffer_text(&buf);
        assert!(header_text.contains("[auto-approve: cargo]"));
    }

    #[test]
    fn tui_key_a_submits_auto_approve_decision() {
        let dash = make_dashboard();
        let mut rx = make_pending(&dash);

        // Send keystroke 'A' (Shift + A)
        let event = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT);
        handle_key(&dash, event);

        let decision = rx.try_recv().unwrap();
        assert_eq!(decision, ConfirmDecision::AutoApprove("x".to_string())); // "x" is the default scope for the dummy pending prompt command "x"
    }

    #[test]
    fn test_feed_navigation_and_toggle() {
        let d = make_dashboard();
        d.append(LineKind::Info, "line1", None);
        d.append(LineKind::Info, "line2", None);
        d.append(LineKind::Info, "line3", None);

        // State starts with selected_index at the last item (2)
        assert_eq!(snap(&d).selected_index, Some(2));
        assert!(!snap(&d).detail_open);

        // Move cursor up: 2 -> 1
        handle_key(&d, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(snap(&d).selected_index, Some(1));

        // Move cursor up: 1 -> 0
        handle_key(&d, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(snap(&d).selected_index, Some(0));

        // Move cursor up again: stays at 0
        handle_key(&d, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(snap(&d).selected_index, Some(0));

        // Move cursor down: 0 -> 1
        handle_key(&d, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(snap(&d).selected_index, Some(1));

        // Move cursor down: 1 -> 2
        handle_key(&d, KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(snap(&d).selected_index, Some(2));

        // Move cursor down again: stays at 2
        handle_key(&d, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(snap(&d).selected_index, Some(2));

        // Press Enter to open detail view
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(snap(&d).detail_open);
        assert_eq!(snap(&d).detail_scroll_top, 0);
    }

    #[test]
    fn test_detail_view_scrolling() {
        let d = make_dashboard();
        let multiline_text = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        d.append(LineKind::Info, "summary", Some(multiline_text));

        // Open detail view
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(snap(&d).detail_open);
        assert_eq!(snap(&d).detail_scroll_top, 0);

        // Set detail_viewport_height to 3
        {
            let mut s = d.state.lock().unwrap();
            s.detail_viewport_height = 3;
        }

        // Scroll down 1: 0 -> 1
        handle_key(&d, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(snap(&d).detail_scroll_top, 1);

        // Scroll down 1 with 'j': 1 -> 2
        handle_key(&d, KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(snap(&d).detail_scroll_top, 2);

        // Scroll up 1: 2 -> 1
        handle_key(&d, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(snap(&d).detail_scroll_top, 1);

        // Scroll up 1 with 'k': 1 -> 0
        handle_key(&d, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(snap(&d).detail_scroll_top, 0);

        // Page Down: 0 -> 3 (page size is 3)
        handle_key(&d, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(snap(&d).detail_scroll_top, 3);

        // End: 3 -> 7 (max scroll is 10 - 3 = 7)
        handle_key(&d, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(snap(&d).detail_scroll_top, 7);

        // Page Up: 7 -> 4
        handle_key(&d, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(snap(&d).detail_scroll_top, 4);

        // Home: 4 -> 0
        handle_key(&d, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(snap(&d).detail_scroll_top, 0);

        // Esc closes
        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!snap(&d).detail_open);
    }

    #[test]
    fn test_rendered_viewport_hints() {
        let d = make_dashboard();
        d.append(LineKind::Info, "item", None);

        // 1. Normal state (no modal, detail closed)
        let s1 = snap(&d);
        let buf1 = render_to_buffer(&s1, 120, 10);
        let text1 = buffer_text(&buf1);
        assert!(text1.contains("waiting for next agent step"));
        assert!(text1.contains("navigate"));
        assert!(text1.contains("inspect"));

        // 2. Detail open state
        {
            let mut s = d.state.lock().unwrap();
            s.detail_open = true;
        }
        let s2 = snap(&d);
        let buf2 = render_to_buffer(&s2, 120, 10);
        let text2 = buffer_text(&buf2);
        assert!(text2.contains("close"));
        assert!(text2.contains("scroll"));
        assert!(text2.contains("bounds"));

        // 3. Modal pending state
        {
            let mut s = d.state.lock().unwrap();
            s.detail_open = false;
        }
        let _rx = make_pending(&d);
        let s3 = snap(&d);
        let buf3 = render_to_buffer(&s3, 120, 10);
        let text3 = buffer_text(&buf3);
        assert!(text3.contains("approve"));
        assert!(text3.contains("reject"));
        assert!(text3.contains("inspect"));
    }

    #[test]
    fn test_backend_detail_inspection() {
        let d = make_dashboard();
        // 1. Emit a BashResult event with a long stdout that exceeds the summary feed
        let long_stdout = "THIS IS A UNIQUE STDOUT LINE THAT EXCEEDS THE FEED SUMMARY";
        d.emit(StreamEvent::BashResult {
            step: 1,
            exit_code: 0,
            stdout: long_stdout.to_string(),
            stderr: String::new(),
            timed_out: false,
            timestamp: "t".into(),
        });

        // 2. Render normal view and check that feed shows only the summary (byte count)
        // and NOT the long stdout line itself.
        let s = snap(&d);
        let buf_feed = render_to_buffer(&s, 100, 10);
        let text_feed = buffer_text(&buf_feed);
        assert!(text_feed.contains("58B output")); // 58 bytes output
        assert!(!text_feed.contains(long_stdout));

        // 3. Move selection cursor to the entry and press Enter to open detail view
        // Since it's the only entry, it will already be selected by default (selected_index = Some(0)).
        assert_eq!(s.selected_index, Some(0));

        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // 4. Render the detail view and check that the full stdout is now rendered
        let s_detail = snap(&d);
        assert!(s_detail.detail_open);

        let buf_detail = render_to_buffer(&s_detail, 100, 10);
        let text_detail = buffer_text(&buf_detail);
        assert!(text_detail.contains(long_stdout));
    }

    #[test]
    fn header_paragraph_renders_monitor_mode_correctly() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.is_monitor = true;
            s.task = Some("test task".into());
            s.model = Some("test-model".into());
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 80, 10);
        let text = buffer_text(&buf);
        assert!(
            text.contains("maxwell's daemon — monitor"),
            "expected monitor header, got: {text}"
        );
        assert!(
            !text.contains("interactive"),
            "should not contain interactive label"
        );
    }

    #[tokio::test]
    async fn test_detail_closes_on_confirm() {
        let d = make_dashboard();
        d.append(LineKind::Info, "summary", Some("detail".to_string()));
        // Open detail view
        {
            let mut s = d.state.lock().unwrap();
            s.selected_index = Some(0);
            s.detail_open = true;
        }
        assert!(snap(&d).detail_open);

        // Trigger confirm callback
        let ctx = ConfirmContext {
            tool_name: "bash".into(),
            command: "x".into(),
            step: 0,
            step_limit: 1,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
        };
        let d_clone = d.clone();
        let task = tokio::spawn(async move { d_clone.confirm(&ctx).await });

        // Wait briefly for confirm to execute and set state
        for _ in 0..50 {
            if snap(&d).pending.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert!(!snap(&d).detail_open);

        // Clean up: send decision to resolve confirm future
        {
            let mut s = d.state.lock().unwrap();
            if let Some(pending) = s.pending.take() {
                let _ = pending.responder.send(ConfirmDecision::Approve);
            }
        }
        let _ = task.await;
    }

    #[test]
    fn test_scrolling_on_wrapped_long_lines() {
        let d = make_dashboard();
        // A single logical line of 50 chars
        let long_line = "a".repeat(50);
        d.append(LineKind::Info, "summary", Some(long_line));

        // Open detail view
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(snap(&d).detail_open);

        // Set viewport width to 10 and height to 2
        {
            let mut s = d.state.lock().unwrap();
            s.detail_viewport_height = 2;
            s.detail_viewport_width = 10;
        }

        // The 50 character line wrapped to width 10 should produce 5 rows.
        // With viewport height of 2, max scroll should be 5 - 2 = 3.
        let max_scroll = {
            let s = d.state.lock().unwrap();
            get_max_detail_scroll(&s)
        };
        assert_eq!(max_scroll, 3);
    }

    #[test]
    fn test_navigation_during_confirmation() {
        let d = make_dashboard();
        d.append(LineKind::Info, "line1", Some("detail1".to_string()));
        d.append(LineKind::Info, "line2", Some("detail2".to_string()));

        // Injected pending prompt (modal is visible)
        let mut rx = make_pending(&d);

        // State selected_index starts at the last item (1)
        assert_eq!(snap(&d).selected_index, Some(1));
        assert!(!snap(&d).detail_open);

        // Move cursor up: 1 -> 0
        handle_key(&d, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(snap(&d).selected_index, Some(0));
        // Selected index changed, meaning cursor navigated instead of scrolling log!
        assert_eq!(snap(&d).scroll_offset, 0);

        // Press Enter to inspect detail view
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(snap(&d).detail_open);
        assert!(snap(&d).pending.is_some()); // Modal is still open!

        // Press Esc to close detail view (modal should stay open)
        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!snap(&d).detail_open);
        assert!(snap(&d).pending.is_some()); // Modal is still open!

        // Press Esc again to abort the pending prompt (modal should abort/close)
        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(snap(&d).pending.is_none());
        assert_eq!(rx.try_recv().unwrap(), ConfirmDecision::Abort);
    }

    #[test]
    fn test_inspected_entry_stable_on_append() {
        let d = make_dashboard();
        d.append(LineKind::Info, "line1", Some("detail1".to_string()));

        // Open detail view
        {
            let mut s = d.state.lock().unwrap();
            s.detail_open = true;
        }

        // Append line2
        d.append(LineKind::Info, "line2", Some("detail2".to_string()));

        // Selected index should still be Some(0), pointing to line1
        assert_eq!(snap(&d).selected_index, Some(0));

        // Now test the popped case. Let's fill the dashboard.
        let d2 = make_dashboard();
        for i in 0..MAX_LOG_LINES {
            d2.append(
                LineKind::Info,
                format!("line_{i}"),
                Some(format!("detail_{i}")),
            );
        }

        // Open detail view for the last item (MAX_LOG_LINES - 1)
        {
            let mut s = d2.state.lock().unwrap();
            s.detail_open = true;
            s.selected_index = Some(MAX_LOG_LINES - 1);
        }

        // Append one more item (will pop the first item, line_0)
        d2.append(LineKind::Info, "new_line", Some("new_detail".to_string()));

        // The selected index should decrement to MAX_LOG_LINES - 2 to point to line_{MAX_LOG_LINES - 1}
        let s = snap(&d2);
        assert_eq!(s.selected_index, Some(MAX_LOG_LINES - 2));
        assert_eq!(
            s.log[MAX_LOG_LINES - 2].text,
            format!("line_{}", MAX_LOG_LINES - 1)
        );
    }

    #[test]
    fn test_finished_closes_detail_before_exit() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.finished = Some("completed".into());
            s.detail_open = true;
        }

        // Press 'q'
        handle_key(&d, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let detail_open = s.detail_open;
            let should_exit = s.should_exit;
            drop(s);
            assert!(!detail_open);
            assert!(!should_exit);
        }

        // Press 'q' again
        handle_key(&d, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        {
            let s = d.state.lock().unwrap();
            let should_exit = s.should_exit;
            drop(s);
            assert!(should_exit);
        }
    }

    #[test]
    fn test_confirm_modal_hidden_when_detail_open() {
        let d = make_dashboard();
        {
            let (tx, _rx) = oneshot::channel();
            let mut s = d.state.lock().unwrap();
            s.detail_open = true;
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

        // When detail_open is true, the confirm modal should not be rendered
        assert!(!text.contains("rm -rf /tmp/dangerous"));
        assert!(!text.contains("confirm action"));
    }

    #[test]
    fn test_main_feed_does_not_wrap() {
        let d = make_dashboard();
        let long_line = "a".repeat(50);
        d.append(LineKind::Info, &long_line, None);

        // total_wrapped_lines on main feed should return 1 (not wrapping)
        let s = snap(&d);
        let total = total_wrapped_lines(&s.log, 10);
        assert_eq!(total, 1);
    }

    #[test]
    fn test_ignore_confirm_keys_when_detail_open() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);
        {
            let mut s = d.state.lock().unwrap();
            s.detail_open = true;
        }

        // Press 'y' while detail is open
        handle_key(&d, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        // It should be ignored, and pending is still Some
        assert!(snap(&d).pending.is_some());
        assert!(rx.try_recv().is_err());

        // Press 'n' while detail is open
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(snap(&d).pending.is_some());
        assert!(snap(&d).feedback_input.is_none());
    }

    #[test]
    fn test_bash_result_empty_output_fallback() {
        let d = make_dashboard();
        d.emit(StreamEvent::BashResult {
            step: 1,
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            timestamp: "t".into(),
        });

        // Set selected_index to 0 and detail_open to true
        {
            let mut s = d.state.lock().unwrap();
            s.selected_index = Some(0);
            s.detail_open = true;
        }

        let s = snap(&d);
        let buf = render_to_buffer(&s, 80, 10);
        let text = buffer_text(&buf);

        // It should fallback to the summary text rather than rendering an empty pane
        assert!(text.contains("bash exit 0"));
    }
}
