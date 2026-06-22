//! Full-screen ratatui dashboard for `mini --interactive --ui ratatui`.
//!
//! `RatatuiDashboard` is both a `StreamSink` (so trajectory events flow
//! into a live log panel) and a `ConfirmCallback` (so the confirmation
//! prompt is rendered as a centred modal). The renderer task owns the
//! terminal; the agent thread drives state through a mutex and a
//! `Notify` channel.

use std::collections::{HashSet, VecDeque};
use std::io::{Stdout, Write as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
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

/// Terminal bell (BEL, `0x07`) byte.
const BEL: u8 = 0x07;

/// Out-of-band operator attention signal (issue #648).
///
/// Writes a single BEL byte to a configurable sink to ring the controlling
/// terminal when the confirm modal is raised or the run ends. `enabled` is
/// resolved once at dashboard start (see [`bell_enabled`]) from the
/// `--no-bell` flag, the `NO_BELL` env var, and a TTY check; when `false`,
/// `ring()` is a no-op and writes zero bytes — keeping piped/CI runs
/// byte-clean.
pub(crate) struct Bell {
    enabled: bool,
    sink: Mutex<Box<dyn std::io::Write + Send>>,
}

impl Bell {
    /// Bell that writes to stdout when `enabled`.
    fn to_stdout(enabled: bool) -> Self {
        Self {
            enabled,
            sink: Mutex::new(Box::new(std::io::stdout())),
        }
    }

    /// A permanently-muted bell. Used as the construction default in tests
    /// that drive the dashboard without a terminal; production code resolves
    /// enablement via [`bell_enabled`] and [`Bell::to_stdout`].
    #[cfg(test)]
    fn silent() -> Self {
        Self {
            enabled: false,
            sink: Mutex::new(Box::new(std::io::sink())),
        }
    }

    /// Bell with an arbitrary sink — used by tests to capture BEL bytes.
    #[cfg(test)]
    fn to_writer(enabled: bool, w: Box<dyn std::io::Write + Send>) -> Self {
        Self {
            enabled,
            sink: Mutex::new(w),
        }
    }

    /// Emit exactly one BEL byte if enabled. Errors are swallowed: a failed
    /// attention signal must never disrupt the agent loop.
    fn ring(&self) {
        if !self.enabled {
            return;
        }
        let mut w = self
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = w.write_all(&[BEL]);
        let _ = w.flush();
    }
}

/// Resolve whether attention bells should fire, from the `--no-bell` flag,
/// the `NO_BELL` env var, and whether stdout is a TTY. Pure so the policy is
/// unit-tested without touching real globals.
///
/// `NO_BELL` suppresses bells when set to any **non-empty** value (covers the
/// conventional `NO_BELL=1`); an empty value is treated as unset.
pub fn bell_enabled(
    no_bell_flag: bool,
    no_bell_env: Option<std::ffi::OsString>,
    stdout_is_tty: bool,
) -> bool {
    let env_suppresses = no_bell_env.is_some_and(|v| !v.is_empty());
    !no_bell_flag && !env_suppresses && stdout_is_tty
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
    scroll_offset: usize,
    auto_follow: bool,
    last_log_width: usize,
    last_log_height: usize,
    should_exit: bool,
    is_monitor: bool,
    search: Option<SearchState>,
    /// Scroll offset (in wrapped display rows) within the confirm modal's
    /// reasoning region (issue #655). Reset to 0 each time a new prompt opens.
    rationale_scroll: usize,
    /// Total wrapped display rows of the current rationale, and the number of
    /// rows visible in its region — captured by the renderer (`draw_frame`) so
    /// the key handler can clamp scrolling by display rows, not logical lines
    /// (a long no-newline rationale wraps to many rows).
    last_rationale_total_rows: usize,
    last_rationale_visible_rows: usize,
    /// What the agent is doing right now, for the in-flight activity indicator
    /// (issue #649). Driven by `StreamEvent` transitions in `emit()`.
    activity: Activity,
    /// Elapsed-time threshold past which the activity indicator escalates to
    /// flag a likely stall (issue #649). Default 60s; configurable.
    stall_threshold: Duration,
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
            scroll_offset: 0,
            auto_follow: true,
            last_log_width: 80,
            last_log_height: 20,
            should_exit: false,
            is_monitor: false,
            search: None,
            rationale_scroll: 0,
            last_rationale_total_rows: 0,
            last_rationale_visible_rows: 0,
            activity: Activity::Idle,
            stall_threshold: Duration::from_secs(DEFAULT_STALL_THRESHOLD_SECS),
        }
    }
}

/// In-dashboard incremental find over the trajectory feed.
///
/// Two-phase (less/vim style): while `editing` the operator types the query
/// and matches highlight incrementally; pressing Enter commits, after which
/// `n`/`N` cycle matches. `current` is an ordinal into the live match list
/// (recomputed each frame from `query`), clamped where it is read.
#[derive(Clone)]
struct SearchState {
    query: String,
    current: usize,
    editing: bool,
}

impl SearchState {
    fn new() -> Self {
        Self {
            query: String::new(),
            current: 0,
            editing: true,
        }
    }
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

/// Default stall threshold (issue #649): once the current operation exceeds
/// this, the activity indicator escalates (yellow) to flag a likely hang.
/// Overridable via the `MAXWELL_STALL_THRESHOLD_SECS` env var at `start()`.
const DEFAULT_STALL_THRESHOLD_SECS: u64 = 60;

/// Resolve the stall threshold from `MAXWELL_STALL_THRESHOLD_SECS`, falling
/// back to [`DEFAULT_STALL_THRESHOLD_SECS`] when unset or unparsable (AC6:
/// "configurable").
fn stall_threshold_from_env() -> Duration {
    let secs = std::env::var("MAXWELL_STALL_THRESHOLD_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_STALL_THRESHOLD_SECS);
    Duration::from_secs(secs)
}

/// Braille spinner frames for the in-flight activity indicator (issue #649).
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Milliseconds per spinner frame. At 100ms the frame advances 10×/sec, so it
/// is always visibly changing within any one-second window (AC4): a frozen
/// render is distinguishable from a live one.
const SPINNER_FRAME_MS: u128 = 100;

/// What the agent is doing *right now*, inferred from the `StreamEvent` gap
/// (issue #649). There is no `model-call-started` event — the model-thinking
/// window is the span between a step boundary (`RunStarted` / `Observation` /
/// `FormatError`) and the next `AssistantMessage`. `since` anchors the
/// per-operation elapsed counter, which resets at every transition.
enum Activity {
    /// No operation in flight: awaiting a confirm decision, or run finished.
    /// Rendered as the static hint with no animation (AC3).
    Idle,
    /// A model call is in flight (step boundary → next `AssistantMessage`).
    Thinking { since: Instant },
    /// A bash command is executing (`BashStart` → `BashResult`).
    Running { since: Instant, command: String },
}

impl Activity {
    /// True while an operation is in flight, i.e. the renderer should keep
    /// ticking so the spinner advances. `Idle` returns `false` so the loop
    /// rests and adds zero animation frames (AC3).
    fn is_active(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

/// Per-frame snapshot of [`Activity`] with elapsed time already resolved, so
/// the pure render path (and `TestBackend` tests) is a deterministic function
/// of `elapsed` rather than reading the wall clock itself.
#[derive(Clone)]
enum ActivitySnapshot {
    Idle,
    Thinking { elapsed: Duration },
    Running { elapsed: Duration, command: String },
}

impl ActivitySnapshot {
    /// Resolve a live [`Activity`] into a snapshot, capturing wall-clock
    /// elapsed for the active operation.
    fn from_activity(activity: &Activity) -> Self {
        match activity {
            Activity::Idle => Self::Idle,
            Activity::Thinking { since } => Self::Thinking {
                elapsed: since.elapsed(),
            },
            Activity::Running { since, command } => Self::Running {
                elapsed: since.elapsed(),
                command: command.clone(),
            },
        }
    }
}

/// Spinner glyph for `elapsed`, advancing one frame per [`SPINNER_FRAME_MS`].
fn spinner_frame(elapsed: Duration) -> &'static str {
    // Reduce modulo the frame count in u128 first so the cast is always a
    // small in-range index (no truncation).
    let idx = ((elapsed.as_millis() / SPINNER_FRAME_MS) % SPINNER_FRAMES.len() as u128) as usize;
    SPINNER_FRAMES[idx]
}

/// Style for an active-operation footer: bold, escalating to yellow once the
/// operation outlives the stall threshold (issue #649, AC6).
fn activity_style(elapsed: Duration, stall_threshold: Duration) -> Style {
    let base = Style::default().add_modifier(Modifier::BOLD);
    if elapsed >= stall_threshold {
        base.fg(Color::Yellow)
    } else {
        base
    }
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
    /// Out-of-band attention signal (issue #648). Rings on modal raise and
    /// run completion; muted when `--no-bell`/`NO_BELL` is set or there is no
    /// TTY.
    bell: Bell,
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
        bell_enabled: bool,
    ) -> std::io::Result<RatatuiDashboardHandle> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        // The alt-screen is required; if it fails, unwind raw mode and bail
        // before anything is drawn so the terminal is never left half-configured.
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        // Bracketed paste is a best-effort enhancement (issue #745): it lets
        // pasted clipboard content arrive as a single `Event::Paste` instead of
        // a burst of key events whose first newline would submit an input
        // field. On terminals that don't support it — e.g. Windows legacy
        // WinAPI consoles, where `EnableBracketedPaste` returns `Unsupported` —
        // we degrade to the dashboard without multi-line paste handling rather
        // than failing to start the TUI entirely. The error is swallowed
        // deliberately; teardown's unconditional `DisableBracketedPaste` is
        // likewise harmless on terminals that never enabled it.
        let _ = execute!(stdout, EnableBracketedPaste);
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
                stall_threshold: stall_threshold_from_env(),
                ..DashboardState::default()
            }),
            notify: Notify::new(),
            cancel_tx,
            bell: Bell::to_stdout(bell_enabled),
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
    // Disable bracketed paste before leaving the alt-screen so the mode is not
    // leaked into the operator's shell on teardown — including the panic /
    // early-exit path, since this runs from `RatatuiDashboardHandle::drop`
    // (issue #745).
    let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
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
                    // The step-1 model call begins now (issue #649): there is
                    // no `model-call-started` event, so the first model-thinking
                    // window opens at run start and closes at `AssistantMessage`.
                    s.activity = Activity::Thinking {
                        since: Instant::now(),
                    };
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
                    // The model has returned: the thinking window closes. The
                    // brief span before the next bash/confirm is genuinely idle
                    // (issue #649, AC3).
                    s.activity = Activity::Idle;
                }
                let preview = first_lines(&content, 6);
                self.append(
                    LineKind::AssistantMsg,
                    format!("step {step} assistant: {preview}"),
                );
            }
            StreamEvent::BashStart { step, command, .. } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    s.activity = Activity::Running {
                        since: Instant::now(),
                        command: command.clone(),
                    };
                }
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
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Bash finished: the next `Observation` will reopen the
                    // thinking window; the gap until then is idle (issue #649).
                    s.activity = Activity::Idle;
                }
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
            StreamEvent::ToolStart { step, label, .. } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // A non-bash tool/hook/driver operation is in flight: render
                    // it like a bash run so a slow one escalates to the stall
                    // indicator instead of the static idle footer (issue #649).
                    s.activity = Activity::Running {
                        since: Instant::now(),
                        command: label.clone(),
                    };
                }
                self.append(LineKind::BashRun, format!("step {step} tool: {label}"));
            }
            StreamEvent::ToolEnd { .. } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // The tool finished; like `BashResult`, the gap until the
                    // next observation/assistant message is idle (issue #649).
                    s.activity = Activity::Idle;
                }
                // No log line — the following observation/result carries the
                // detail — but wake the renderer so the footer clears promptly.
                self.notify.notify_waiters();
            }
            StreamEvent::Observation { step, content, .. } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // The observation is the last event before the loop calls
                    // the model again, so the next thinking window opens here
                    // (issue #649): this is the inferred model-call-started.
                    s.activity = Activity::Thinking {
                        since: Instant::now(),
                    };
                }
                let preview = first_lines(&content, 4);
                self.append(
                    LineKind::Observation,
                    format!("step {step} observation: {preview}"),
                );
            }
            StreamEvent::FormatError { step, content, .. } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // A malformed response triggers a re-query, so a fresh
                    // model-thinking window opens here (issue #649).
                    s.activity = Activity::Thinking {
                        since: Instant::now(),
                    };
                }
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
                let first_finish = {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let was_running = s.finished.is_none();
                    s.cost_usd = total_cost_usd;
                    s.step = steps;
                    s.finished = Some(exit_reason.clone());
                    // Run is over: no operation in flight (issue #649).
                    s.activity = Activity::Idle;
                    was_running
                };
                // Signal the operator on the rising edge of the terminal
                // state — exactly one bell per run (issue #648).
                if first_finish {
                    self.bell.ring();
                }
                self.append(
                    LineKind::Info,
                    format!("run ended: {exit_reason} (steps={steps} cost=${total_cost_usd:.4})"),
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
                );
            }
        }
    }
}

#[async_trait]
impl ConfirmCallback for RatatuiDashboard {
    async fn confirm(&self, ctx: &ConfirmContext) -> ConfirmDecision {
        // The modal transitions absent -> present here, and `confirm` is
        // called exactly once per distinct prompt (redraws never call it), so
        // ringing here gives exactly one bell per raise, debounced by
        // construction (issue #648).
        self.bell.ring();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.step = ctx.step;
            s.step_limit = ctx.step_limit;
            s.cost_usd = ctx.cost_usd;
            s.rationale_scroll = 0;
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
    // Spinner cadence while an operation is in flight (issue #649). The tick
    // branch is guarded by `if active`, so an idle/finished dashboard redraws
    // solely on `Notify`/keystrokes — zero animation frames, no CPU spin while
    // nothing is happening (AC3). `Skip` prevents the default `Burst` behaviour
    // from firing a flurry of catch-up ticks when an operation resumes after an
    // idle gap during which the interval was never awaited.
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // Register interest in the next state change *before* reading state and
        // drawing. `Notify::notify_waiters()` only wakes waiters already
        // registered at the time it is called (it stores no permit), so an
        // emitter that flips idle→active in the window between this draw and
        // the `select!` await would otherwise be lost — and without the old
        // unconditional tick to mask it, an idle dashboard could stay frozen on
        // the stale footer until a keystroke (issue #649). Enabling the
        // `Notified` future up front closes that race: a notification that
        // arrives after `enable()` marks it ready, so the `select!` returns
        // immediately and the loop redraws with fresh state.
        let notified = dash.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let (exit, active) = {
            let s = dash
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (s.should_exit, s.activity.is_active())
        };
        if exit {
            break;
        }

        if let Err(err) = draw_frame(&dash, &mut terminal) {
            tracing::warn!(?err, "ratatui draw failed");
        }
        tokio::select! {
            _ = &mut shutdown => break,
            () = &mut notified => {}
            _ = tick.tick(), if active => {}
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) => handle_key(&dash, key),
                    Some(Ok(Event::Paste(text))) => handle_paste(&dash, &text),
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

/// The slice of `log` the dashboard actually renders: the last
/// `MAX_LOG_LINES` entries. Search indices are relative to this window so
/// that key handling and `log_paragraph` agree on what each index means.
fn displayed_window(log: &VecDeque<LogLine>) -> Vec<&LogLine> {
    let max_lines = log.len().min(MAX_LOG_LINES);
    let take_from = log.len().saturating_sub(max_lines);
    log.iter().skip(take_from).collect()
}

/// Case-insensitive literal-substring match over the rendered feed text.
/// Returns the indices (into `window`) of matching lines. An empty query
/// matches nothing. Pure: never executes or mutates anything.
fn search_matches(window: &[&LogLine], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    window
        .iter()
        .enumerate()
        .filter(|(_, line)| line.text.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// Scroll the feed so the wrapped line(s) of `line_idx` (an index into the
/// displayed window) are visible, introducing the minimal offset needed:
/// scroll up if the line is above the viewport, down if below, otherwise
/// leave the offset untouched. Disengages auto-follow and clamps to bounds.
fn scroll_to_line(s: &mut DashboardState, line_idx: usize) {
    let window = displayed_window(&s.log);
    let width = s.last_log_width;
    let height = s.last_log_height;
    let total = total_wrapped_lines(window.iter().copied(), width);
    let max_scroll = total.saturating_sub(height);

    let start = total_wrapped_lines(window.iter().take(line_idx).copied(), width);
    let line_height = window
        .get(line_idx)
        .map_or(1, |line| count_wrapped_lines(&line.text, width).max(1));
    let end = start + line_height;

    let current_top = if s.auto_follow {
        max_scroll
    } else {
        s.scroll_offset.min(max_scroll)
    };

    let new_top = if start < current_top {
        start
    } else if end > current_top + height {
        end.saturating_sub(height)
    } else {
        current_top
    };

    s.scroll_offset = new_top.min(max_scroll);
    s.auto_follow = false;
}

/// Handle a keystroke while the `/`-search sub-mode is active. Implements the
/// two-phase model: `editing` accepts the query (incremental highlight + jump
/// to first match); committed mode cycles matches with `n`/`N`. Returns the
/// (possibly mutated) `SearchState` to `s.search` unless search is exited.
fn handle_key_search(
    dash: &RatatuiDashboard,
    s: &mut DashboardState,
    mut search: SearchState,
    key: KeyEvent,
) {
    // Arrow / page / Home / End keep scrolling the feed without disturbing
    // the active query.
    if perform_scroll(s, key.code) {
        s.search = Some(search);
        dash.notify.notify_waiters();
        return;
    }

    // Ignore modifier combos (e.g. Ctrl-x) so they neither steal navigation
    // nor land in the query buffer. Ctrl-C is handled before we get here.
    if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
        s.search = Some(search);
        return;
    }

    match key.code {
        KeyCode::Esc => {
            // Exit search, clear highlights, keep the operator's scroll
            // position at the last match (no snap-back).
            s.search = None;
            dash.notify.notify_waiters();
        }
        KeyCode::Enter => {
            if search.query.is_empty() {
                // An empty query exits search cleanly.
                s.search = None;
                dash.notify.notify_waiters();
                return;
            }
            // Commit: leave editing mode and settle on the current match
            // (the first hit the incremental find already landed on). `n`/`N`
            // advance from here.
            search.editing = false;
            let window = displayed_window(&s.log);
            let matches = search_matches(&window, &search.query);
            if !matches.is_empty() {
                search.current = search.current.min(matches.len() - 1);
                let line_idx = matches[search.current];
                scroll_to_line(s, line_idx);
            }
            s.search = Some(search);
            dash.notify.notify_waiters();
        }
        KeyCode::Backspace if search.editing => {
            search.query.pop();
            search_recompute_after_edit(s, &mut search);
            s.search = Some(search);
            dash.notify.notify_waiters();
        }
        KeyCode::Char(c) if search.editing => {
            search.query.push(c);
            search_recompute_after_edit(s, &mut search);
            s.search = Some(search);
            dash.notify.notify_waiters();
        }
        // Committed-phase navigation.
        KeyCode::Char('n') => {
            search_jump(s, &mut search, 1);
            s.search = Some(search);
            dash.notify.notify_waiters();
        }
        KeyCode::Char('N') => {
            search_jump(s, &mut search, -1);
            s.search = Some(search);
            dash.notify.notify_waiters();
        }
        KeyCode::Char('/') => {
            // Restart a fresh search.
            s.search = Some(SearchState::new());
            dash.notify.notify_waiters();
        }
        _ => {
            s.search = Some(search);
        }
    }
}

/// After the query changes while editing, re-anchor the current match to the
/// first hit and jump the view to it (incremental find).
fn search_recompute_after_edit(s: &mut DashboardState, search: &mut SearchState) {
    let window = displayed_window(&s.log);
    let matches = search_matches(&window, &search.query);
    if matches.is_empty() {
        search.current = 0;
        return;
    }
    search.current = 0;
    let line_idx = matches[0];
    scroll_to_line(s, line_idx);
}

/// Move the current-match cursor by `step` (+1 next, -1 previous) with
/// wraparound and scroll the view to it. No-op when there are no matches.
fn search_jump(s: &mut DashboardState, search: &mut SearchState, step: isize) {
    let window = displayed_window(&s.log);
    let matches = search_matches(&window, &search.query);
    if matches.is_empty() {
        search.current = 0;
        return;
    }
    let len = matches.len();
    let cur = search.current.min(len - 1);
    let next = if step >= 0 {
        (cur + 1) % len
    } else {
        (cur + len - 1) % len
    };
    search.current = next;
    let line_idx = matches[next];
    scroll_to_line(s, line_idx);
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

/// Scroll the confirm modal's reasoning region (issue #655), reusing the
/// feed's scroll convention (line up/down, page up/down, home/end).
///
/// Scroll bounds are in wrapped display rows, using the metrics the renderer
/// stored on the last frame (`last_rationale_total_rows` /
/// `last_rationale_visible_rows`) so a long rationale with few newlines — which
/// wraps to many screen rows — is fully reachable. Returns `false` when the
/// rationale is empty or fits within its region (nothing to scroll), so the
/// caller falls through to feed scrolling and scroll keys are never silently
/// swallowed.
fn perform_rationale_scroll(s: &mut DashboardState, ctx: &ConfirmContext, code: KeyCode) -> bool {
    if ctx.rationale.trim().is_empty() {
        return false;
    }
    let visible = s.last_rationale_visible_rows;
    let max_scroll = s.last_rationale_total_rows.saturating_sub(visible);
    if max_scroll == 0 {
        return false;
    }
    let page = visible.max(1);
    let current = s.rationale_scroll.min(max_scroll);
    let next = match code {
        KeyCode::Up => current.saturating_sub(1),
        KeyCode::Down => (current + 1).min(max_scroll),
        KeyCode::PageUp => current.saturating_sub(page),
        KeyCode::PageDown => (current + page).min(max_scroll),
        KeyCode::Home => 0,
        KeyCode::End => max_scroll,
        _ => return false,
    };
    s.rationale_scroll = next;
    true
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

    // Scroll keys drive the in-modal rationale first; when the rationale fits
    // (nothing to scroll) they fall through to feed scrolling. Decision verbs
    // are never scroll keys, so this never shadows y/n/e/a/A/Esc.
    if perform_rationale_scroll(s, &pending.ctx, key.code) {
        s.pending = Some(pending);
        dash.notify.notify_waiters();
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
        } else {
            handle_key_normal(dash, &mut s, pending, key);
        }
    } else {
        // No modal open
        if ctrl_c && s.finished.is_none() {
            if let Some(ref tx) = dash.cancel_tx {
                let _ = tx.send(true);
            }
        }

        // The active /-search sub-mode owns Esc/Enter/typing/n/N, taking
        // priority over the finished-close and scroll handling below. Ctrl-C
        // still falls through so the global stop/close keeps working.
        if !ctrl_c {
            if let Some(search) = s.search.take() {
                handle_key_search(dash, &mut s, search, key);
                return;
            }
            if matches!(key.code, KeyCode::Char('/')) {
                s.search = Some(SearchState::new());
                drop(s);
                dash.notify.notify_waiters();
                return;
            }
        }

        if s.finished.is_some() {
            let close_key = matches!(key.code, KeyCode::Char('q' | 'Q') | KeyCode::Esc) || ctrl_c;
            if close_key {
                s.should_exit = true;
                drop(s);
                dash.notify.notify_waiters();
                return;
            }
        }

        if perform_scroll(&mut s, key.code) {
            drop(s);
            dash.notify.notify_waiters();
        }
    }
}

/// Normalize pasted text line endings to `\n` (issue #745 review). Windows
/// clipboards deliver `\r\n` and some sources lone `\r`; the modal renderer
/// strips trailing `\r` only for *display*, so without this an edit-command
/// paste could keep hidden carriage returns and submit them to bash via
/// `ConfirmDecision::Edit` (e.g. `true\r` becomes a bogus command). Collapsing
/// CRLF and lone CR to LF keeps the submitted value identical to what is shown.
fn normalize_pasted(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Insert bracketed-paste content (issue #745) into whichever modal input
/// field is active. crossterm delivers the whole clipboard payload as one
/// `Event::Paste(String)` — including embedded newlines — so the entire value
/// lands in the buffer without any character being interpreted as `Enter`
/// (which would otherwise submit the field at the first newline).
///
/// The paste is appended at the buffer's insertion point (this slice keeps a
/// single end-of-buffer insertion point; see the issue's Out of Scope), so
/// existing typed content is preserved. Routing precedence:
/// - a reject-feedback / edit-command input field (when a modal is pending);
/// - otherwise, while a modal is pending but on its choice screen, the paste is
///   dropped — it must NOT fall through to an open `/` search hidden behind the
///   modal, which dismissing the modal would then reveal as corrupted;
/// - otherwise (no modal), an actively-edited `/` search query;
/// - otherwise (normal feed navigation), ignored so it can never corrupt feed
///   state or trigger a hotkey (AC5).
fn handle_paste(dash: &Arc<RatatuiDashboard>, text: &str) {
    let text = normalize_pasted(text);
    let mut s = dash
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(buffer) = s.feedback_input.as_mut() {
        buffer.push_str(&text);
    } else if let Some(buffer) = s.edit_input.as_mut() {
        buffer.push_str(&text);
    } else if s.pending.is_some() {
        // Modal pending on its choice screen (no input field active): the paste
        // is not directed anywhere. Drop it before the search branch so an open
        // search behind the modal is never silently appended to.
        return;
    } else if let Some(mut search) = s.search.take() {
        // While the `/`-search query is being edited, paste appends to the
        // query — preserving the pre-bracketed-paste behaviour where pasted
        // text arrived as `Char` events and built up the query (issue #745
        // review). Re-run the incremental find so highlights/jump update, just
        // as typing a character does. A committed search (n/N navigation) is
        // not a text input, so its paste is dropped.
        if !search.editing {
            s.search = Some(search);
            return;
        }
        search.query.push_str(&text);
        search_recompute_after_edit(&mut s, &mut search);
        s.search = Some(search);
    } else {
        // No active input field: ignore the paste entirely.
        return;
    }
    drop(s);
    dash.notify.notify_waiters();
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

        // Capture the modal's rationale layout metrics (in wrapped display
        // rows) so the key handler can clamp scrolling correctly (issue #655).
        let (total_rows, visible_rows) = if let Some(pending) = &s.pending {
            let inner = modal_inner_rect(area);
            let body_width = inner.width as usize;
            let top_rows = wrapped_rows(&modal_top_lines(&pending.ctx, true, true), body_width);
            let control_rows = wrapped_rows(
                &modal_control_lines(
                    &pending.ctx,
                    s.feedback_input.as_ref(),
                    s.edit_input.as_ref(),
                ),
                body_width,
            );
            let (_, body_h, _) = modal_body_layout(inner.height, top_rows, control_rows);
            let total =
                count_wrapped_lines(&rationale_body_string(&pending.ctx.rationale), body_width);
            (total, body_h as usize)
        } else {
            (0, 0)
        };
        s.last_rationale_total_rows = total_rows;
        s.last_rationale_visible_rows = visible_rows;

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
            scroll_offset: s.scroll_offset,
            auto_follow: s.auto_follow,
            last_log_width: s.last_log_width,
            last_log_height: s.last_log_height,
            is_monitor: s.is_monitor,
            search: s.search.clone(),
            rationale_scroll: s.rationale_scroll,
            activity: ActivitySnapshot::from_activity(&s.activity),
            stall_threshold: s.stall_threshold,
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
    active_rules: Vec<String>,
    scroll_offset: usize,
    auto_follow: bool,
    last_log_width: usize,
    last_log_height: usize,
    is_monitor: bool,
    search: Option<SearchState>,
    rationale_scroll: usize,
    activity: ActivitySnapshot,
    stall_threshold: Duration,
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
            snap.rationale_scroll,
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

fn log_paragraph(snap: &DashboardSnapshot) -> Paragraph<'_> {
    let max_lines = snap.log.len().min(MAX_LOG_LINES);
    let take_from = snap.log.len().saturating_sub(max_lines);
    let window: Vec<&LogLine> = snap.log[take_from..].iter().collect();

    // Recompute matches each frame so highlights stay correct as the log
    // grows; the persisted `current` ordinal picks the active hit.
    let query = snap.search.as_ref().map_or("", |s| s.query.as_str());
    let matches = search_matches(&window, query);
    let match_set: HashSet<usize> = matches.iter().copied().collect();
    let current_line = snap.search.as_ref().and_then(|s| {
        matches
            .get(s.current.min(matches.len().saturating_sub(1)))
            .copied()
    });

    let lines: Vec<Line> = window
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let base = match l.kind {
                LineKind::Info => Style::default().fg(Color::Gray),
                LineKind::AssistantMsg => Style::default().fg(Color::Cyan),
                LineKind::BashRun => Style::default().fg(Color::White),
                LineKind::BashOk => Style::default().fg(Color::Green),
                LineKind::BashErr => Style::default().fg(Color::Red),
                LineKind::Observation => Style::default().fg(Color::LightBlue),
                LineKind::Warn => Style::default().fg(Color::LightYellow),
            };
            let style = if current_line == Some(i) {
                // The active match: a distinct high-contrast highlight so the
                // operator always knows which hit they are on.
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if match_set.contains(&i) {
                base.bg(Color::DarkGray)
            } else {
                base
            };
            Line::from(Span::styled(l.text.clone(), style))
        })
        .collect();

    let total = total_wrapped_lines(&snap.log, snap.last_log_width);
    let max_scroll = total.saturating_sub(snap.last_log_height);
    let scroll_y = if snap.auto_follow {
        max_scroll
    } else {
        snap.scroll_offset.min(max_scroll)
    };

    let title = if let Some(search) = &snap.search {
        let total_matches = matches.len();
        let pos = if total_matches == 0 {
            0
        } else {
            search.current.min(total_matches - 1) + 1
        };
        format!(" trajectory [SEARCH {pos}/{total_matches}] ")
    } else if snap.auto_follow {
        " trajectory [LIVE] ".to_string()
    } else {
        " trajectory [SCROLLED] ".to_string()
    };

    #[allow(clippy::cast_possible_truncation)]
    let scroll_y_u16 = scroll_y.min(u16::MAX as usize) as u16;

    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((scroll_y_u16, 0))
}

fn footer_paragraph(snap: &DashboardSnapshot) -> Paragraph<'_> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    // Search / finished / edit / pending all take precedence over the activity
    // cell — a raised confirm modal must show its decision keys, not a spinner
    // (issue #649, AC3: awaiting a confirm decision is "genuinely idle").
    let span = if let Some(search) = &snap.search {
        let max_lines = snap.log.len().min(MAX_LOG_LINES);
        let take_from = snap.log.len().saturating_sub(max_lines);
        let window: Vec<&LogLine> = snap.log[take_from..].iter().collect();
        let total_matches = search_matches(&window, &search.query).len();
        let pos = if total_matches == 0 {
            0
        } else {
            search.current.min(total_matches - 1) + 1
        };
        let hint = if search.editing {
            format!(
                "search: {}█   [Enter] find   [Esc] cancel   ({pos}/{total_matches})",
                search.query
            )
        } else {
            format!(
                "search: {}   [n] next   [N] prev   [/] new   [Esc] exit   ({pos}/{total_matches})",
                search.query
            )
        };
        Span::styled(hint, bold)
    } else if snap.finished.is_some() {
        Span::styled(
            "run complete — press 'q', Esc, or Ctrl-C to close  [scroll: ↑/↓/PgUp/PgDn/Home/End]  [/ search]",
            bold,
        )
    } else if snap.edit_input.is_some() {
        Span::styled("[Enter] execute edit   [Esc] cancel", bold)
    } else if let Some(pending) = &snap.pending {
        let scope = pending.derive_scope();
        Span::styled(
            format!("(y) approve   (n) reject   (e) edit   (a) abort   (A) auto-approve {scope}"),
            bold,
        )
    } else {
        // No modal, no search, not finished: surface the in-flight activity so
        // the operator can tell working from hung (issue #649).
        match &snap.activity {
            ActivitySnapshot::Thinking { elapsed } => Span::styled(
                format!(
                    "{} thinking… (model · {}s)",
                    spinner_frame(*elapsed),
                    elapsed.as_secs()
                ),
                activity_style(*elapsed, snap.stall_threshold),
            ),
            ActivitySnapshot::Running { elapsed, command } => Span::styled(
                format!(
                    "{} running: {command} ({}s)",
                    spinner_frame(*elapsed),
                    elapsed.as_secs()
                ),
                activity_style(*elapsed, snap.stall_threshold),
            ),
            ActivitySnapshot::Idle => Span::styled(
                "waiting for next agent step…  [scroll: ↑/↓/PgUp/PgDn/Home/End]  [/ search]",
                bold,
            ),
        }
    };
    Paragraph::new(Line::from(span)).block(Block::default().borders(Borders::ALL))
}

/// Render the confirm modal (issue #655).
///
/// The modal is split into three stacked regions inside its border:
/// a fixed **top** (step/cost, tool, command, reasoning label), a flexible
/// **rationale body** that wraps and scrolls by display rows, and a fixed
/// **controls** region (decision keys / feedback / edit input). The controls
/// are reserved first, so they are always visible regardless of how tall the
/// rationale or command is — the operator never types or decides blind.
fn draw_modal(
    frame: &mut ratatui::Frame,
    ctx: &ConfirmContext,
    feedback_input: Option<&String>,
    edit_input: Option<&String>,
    rationale_scroll: usize,
    area: Rect,
) {
    let modal = centered_rect(70, 50, area);
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" confirm action ");
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let body_width = inner.width as usize;
    let control_lines = modal_control_lines(ctx, feedback_input, edit_input);
    // Size the top and controls by wrapped display rows, not logical lines, so a
    // long single-line command (or edit buffer) that wraps is fully reserved and
    // never clipped before the operator sees it. The top is sized with the
    // worst-case reasoning label (both affordances present) so the label never
    // under-reserves regardless of scroll state.
    let top_rows = wrapped_rows(&modal_top_lines(ctx, true, true), body_width);
    let control_rows = wrapped_rows(&control_lines, body_width);
    let (top_h, body_h, control_h) = modal_body_layout(inner.height, top_rows, control_rows);

    // Scroll bounds in wrapped display rows for the rationale body.
    let total_rows = count_wrapped_lines(&rationale_body_string(&ctx.rationale), body_width);
    let visible = body_h as usize;
    let max_scroll = total_rows.saturating_sub(visible);
    let scroll = rationale_scroll.min(max_scroll);
    let can_up = scroll > 0;
    let can_down = scroll + visible < total_rows;

    let top_lines = modal_top_lines(ctx, can_up, can_down);

    let top_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: top_h,
    };
    let body_area = Rect {
        x: inner.x,
        y: inner.y + top_h,
        width: inner.width,
        height: body_h,
    };
    let control_area = Rect {
        x: inner.x,
        y: inner.y + top_h + body_h,
        width: inner.width,
        height: control_h,
    };

    frame.render_widget(
        Paragraph::new(top_lines).wrap(Wrap { trim: false }),
        top_area,
    );
    let scroll_y = u16::try_from(scroll).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(rationale_body_lines(&ctx.rationale))
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0)),
        body_area,
    );
    frame.render_widget(
        Paragraph::new(control_lines).wrap(Wrap { trim: false }),
        control_area,
    );
}

/// Inner (border-stripped) rectangle of the confirm modal for a given screen
/// `area`. Shared by the renderer and `draw_frame` so scroll metrics match.
fn modal_inner_rect(area: Rect) -> Rect {
    let modal = centered_rect(70, 50, area);
    Block::default().borders(Borders::ALL).inner(modal)
}

/// Reserve modal rows bottom-up: controls first (always visible), then the
/// fixed top, then whatever remains becomes the scrollable rationale body.
/// `top_rows` / `control_rows` are **wrapped display rows** (see [`wrapped_rows`]).
fn modal_body_layout(inner_h: u16, top_rows: usize, control_rows: usize) -> (u16, u16, u16) {
    let control_h = u16::try_from(control_rows).unwrap_or(u16::MAX).min(inner_h);
    let remaining = inner_h - control_h;
    let top_h = u16::try_from(top_rows).unwrap_or(u16::MAX).min(remaining);
    let body_h = remaining - top_h;
    (top_h, body_h, control_h)
}

/// Total wrapped display rows that `lines` occupy at `width`, matching how
/// `Paragraph` with `Wrap { trim: false }` renders them. Used to size the
/// modal's fixed regions so wrapped content is never clipped.
fn wrapped_rows(lines: &[Line<'_>], width: usize) -> usize {
    if width == 0 {
        return lines.len();
    }
    lines
        .iter()
        .map(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            count_wrapped_line(&text, width)
        })
        .sum()
}

/// Fixed top region: step/cost header, tool, the proposed command (capped at
/// 12 lines), then the magenta `reasoning:` label. The label carries the
/// more-above / more-below scroll affordances; its line *count* is invariant to
/// those flags, so `draw_frame` can call this with `false, false` purely to
/// size the layout.
fn modal_top_lines(
    ctx: &ConfirmContext,
    can_scroll_up: bool,
    can_scroll_down: bool,
) -> Vec<Line<'static>> {
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
    if ctx.command.lines().nth(12).is_some() {
        lines.push(Line::from(Span::styled(
            "  …",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    lines.push(Line::from(""));

    // Reasoning label (issue #655): magenta keeps it visually distinct from the
    // white "command:" so justification text is never mistaken for the command
    // being authorized. Scroll affordances ride on this single line.
    let mut label = String::from("reasoning:");
    if can_scroll_up {
        label.push_str("   ↑ more above");
    }
    if can_scroll_down {
        label.push_str("   ↓ more below");
    }
    lines.push(Line::from(Span::styled(
        label,
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )));
    lines
}

/// The rationale body as styled lines: magenta prose, or a dim
/// `(no rationale provided)` indicator when empty/whitespace-only. Kept in sync
/// with [`rationale_body_string`] (used for wrapped-row counting).
fn rationale_body_lines(rationale: &str) -> Vec<Line<'static>> {
    let empty = rationale.trim().is_empty();
    rationale_body_string(rationale)
        .split('\n')
        .map(|l| {
            let style = if empty {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(Color::Magenta)
            };
            Line::from(Span::styled(l.to_owned(), style))
        })
        .collect()
}

/// The plain text rendered in the rationale body, used both for display and for
/// counting wrapped display rows so the rendered region and the scroll bounds
/// agree.
fn rationale_body_string(rationale: &str) -> String {
    if rationale.trim().is_empty() {
        "(no rationale provided)".to_owned()
    } else {
        rationale.to_owned()
    }
}

/// Render a (possibly multi-line) input `buffer` as styled display lines for
/// the modal controls region (issue #745). Each logical line becomes its own
/// `Line` — a `> ` prompt prefix on the first, indentation thereafter — so a
/// pasted multi-line value is shown in full instead of being collapsed into a
/// single span with embedded newlines (which renders incorrectly). The green
/// caret block rides the last line, marking the end-of-buffer insertion point.
fn input_buffer_lines(buffer: &str) -> Vec<Line<'static>> {
    let buf_lines: Vec<&str> = buffer
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let last = buf_lines.len() - 1;
    buf_lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 { " > " } else { "   " };
            let mut spans = vec![
                Span::styled(prefix, Style::default().fg(Color::Green)),
                Span::styled((*line).to_string(), Style::default().fg(Color::White)),
            ];
            if i == last {
                spans.push(Span::styled("█", Style::default().fg(Color::Green)));
            }
            Line::from(spans)
        })
        .collect()
}

/// Controls region: the reject-feedback prompt, the edit buffer, or the default
/// decision keys. Always reserved space by [`modal_body_layout`] so it stays on
/// screen.
fn modal_control_lines(
    ctx: &ConfirmContext,
    feedback_input: Option<&String>,
    edit_input: Option<&String>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(buffer) = feedback_input {
        lines.push(Line::from(Span::styled(
            "Provide corrective feedback (optional):",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )));
        // Render one display line per logical line so a pasted multi-line value
        // (issue #745) is shown in full rather than collapsed into a single span
        // with embedded newlines. The caret rides the last line.
        lines.extend(input_buffer_lines(buffer));
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
        lines.extend(input_buffer_lines(buffer));
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
    lines
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

fn total_wrapped_lines<'a>(log: impl IntoIterator<Item = &'a LogLine>, width: usize) -> usize {
    log.into_iter()
        .map(|line| count_wrapped_lines(&line.text, width))
        .sum()
}

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

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
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line1".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line2".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line3".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line4".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line5".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line6".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line7".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line8".into(),
            });
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
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line1".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line2".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line3".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line4".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line5".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line6".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line7".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line8".into(),
            });
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
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line1".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line2".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line3".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line4".into(),
            });
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: "line5".into(),
            });
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
            bell: Bell::silent(),
        })
    }

    /// Shared, inspectable byte sink for asserting exact BEL output.
    #[derive(Clone, Default)]
    struct ByteSink(Arc<Mutex<Vec<u8>>>);

    impl ByteSink {
        fn bytes(&self) -> Vec<u8> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
        #[allow(clippy::naive_bytecount)]
        fn bel_count(&self) -> usize {
            self.bytes().iter().filter(|&&b| b == BEL).count()
        }
    }

    impl std::io::Write for ByteSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Dashboard whose bell writes to an inspectable buffer instead of stdout.
    fn make_dashboard_with_bell(enabled: bool) -> (Arc<RatatuiDashboard>, ByteSink) {
        let sink = ByteSink::default();
        let dash = Arc::new(RatatuiDashboard {
            state: Mutex::new(DashboardState::default()),
            notify: Notify::new(),
            cancel_tx: None,
            bell: Bell::to_writer(enabled, Box::new(sink.clone())),
        });
        (dash, sink)
    }

    fn confirm_ctx() -> ConfirmContext {
        ConfirmContext {
            tool_name: "bash".into(),
            command: "ls".into(),
            step: 0,
            step_limit: 5,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
            rationale: String::new(),
        }
    }

    /// Drive one `confirm()` to completion: spawn it, wait for the modal to be
    /// raised, then answer it via the pending oneshot responder.
    async fn drive_one_confirm(dash: &Arc<RatatuiDashboard>, decision: ConfirmDecision) {
        let d = dash.clone();
        let task = tokio::spawn(async move {
            let ctx = confirm_ctx();
            d.confirm(&ctx).await
        });
        // Wait until the modal is present, then answer it.
        loop {
            let responder = {
                let mut s = dash
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                s.pending.take().map(|p| p.responder)
            };
            if let Some(tx) = responder {
                let _ = tx.send(decision);
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let _ = task.await;
    }

    fn run_ended_event() -> StreamEvent {
        StreamEvent::RunEnded {
            exit_reason: "resolved".into(),
            failure_category: None,
            final_output: None,
            steps: 3,
            total_cost_usd: 0.05,
            ended_at: "2026-06-21T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn confirm_rings_exactly_one_bel_per_prompt() {
        let (dash, sink) = make_dashboard_with_bell(true);
        drive_one_confirm(&dash, ConfirmDecision::Approve).await;
        assert_eq!(sink.bel_count(), 1, "one BEL per modal raise");
        assert_eq!(
            sink.bytes(),
            vec![0x07],
            "exactly one BEL byte, nothing else"
        );
    }

    #[tokio::test]
    async fn two_prompts_ring_two_bels() {
        let (dash, sink) = make_dashboard_with_bell(true);
        drive_one_confirm(&dash, ConfirmDecision::Approve).await;
        drive_one_confirm(&dash, ConfirmDecision::Approve).await;
        assert_eq!(sink.bel_count(), 2, "one BEL per distinct prompt");
    }

    #[test]
    fn run_ended_rings_one_bel() {
        let (dash, sink) = make_dashboard_with_bell(true);
        dash.emit(run_ended_event());
        assert_eq!(sink.bel_count(), 1, "one BEL on terminal state");
    }

    #[test]
    fn run_ended_twice_rings_only_once() {
        let (dash, sink) = make_dashboard_with_bell(true);
        dash.emit(run_ended_event());
        dash.emit(run_ended_event());
        assert_eq!(
            sink.bel_count(),
            1,
            "completion bell is rising-edge debounced"
        );
    }

    #[tokio::test]
    async fn no_bell_writes_zero_bytes() {
        let (dash, sink) = make_dashboard_with_bell(false);
        drive_one_confirm(&dash, ConfirmDecision::Approve).await;
        dash.emit(run_ended_event());
        assert_eq!(sink.bytes().len(), 0, "suppressed: zero bytes written");
    }

    #[test]
    fn non_terminal_events_do_not_ring() {
        let (dash, sink) = make_dashboard_with_bell(true);
        dash.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });
        dash.emit(StreamEvent::AssistantMessage {
            step: 1,
            content: "x".into(),
            cost_usd: None,
            timestamp: "t".into(),
        });
        dash.emit(StreamEvent::BashStart {
            step: 1,
            command: "ls".into(),
            timestamp: "t".into(),
        });
        assert_eq!(sink.bel_count(), 0, "only modal-raise and run-end ring");
    }

    #[test]
    fn bell_enabled_truth_table() {
        use std::ffi::OsString;
        // All clear: flag off, env unset, TTY present.
        assert!(bell_enabled(false, None, true));
        // Flag suppresses.
        assert!(!bell_enabled(true, None, true));
        // NO_BELL with any non-empty value suppresses.
        assert!(!bell_enabled(false, Some(OsString::from("1")), true));
        assert!(!bell_enabled(false, Some(OsString::from("anything")), true));
        // Empty NO_BELL does not suppress.
        assert!(bell_enabled(false, Some(OsString::from("")), true));
        // No TTY suppresses.
        assert!(!bell_enabled(false, None, false));
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
            scroll_offset: s.scroll_offset,
            auto_follow: s.auto_follow,
            last_log_width: s.last_log_width,
            last_log_height: s.last_log_height,
            is_monitor: s.is_monitor,
            search: s.search.clone(),
            rationale_scroll: s.rationale_scroll,
            activity: ActivitySnapshot::from_activity(&s.activity),
            stall_threshold: s.stall_threshold,
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

    // ---- in-flight activity indicator (#649) ----

    /// True if any cell in the bottom 3 rows (the footer) carries `color` as
    /// its foreground — used to assert the stall escalation (yellow).
    fn footer_has_fg(buf: &Buffer, color: Color) -> bool {
        let h = buf.area.height;
        let footer_top = h.saturating_sub(3);
        for y in footer_top..h {
            for x in 0..buf.area.width {
                if buf[(x, y)].style().fg == Some(color) {
                    return true;
                }
            }
        }
        false
    }

    /// True if any spinner glyph is present anywhere in the buffer.
    fn has_spinner(buf: &Buffer) -> bool {
        let text = buffer_text(buf);
        SPINNER_FRAMES.iter().any(|f| text.contains(f))
    }

    /// Force the dashboard's activity, simulating an operation that started
    /// `elapsed` ago by anchoring `since` in the past.
    fn set_activity(d: &Arc<RatatuiDashboard>, activity: Activity) {
        let mut s = d.state.lock().unwrap();
        s.activity = activity;
        drop(s);
    }

    /// An `Instant` `secs` in the past, for simulating a long-running op. Test
    /// values are always smaller than the process uptime, so the subtraction
    /// is safe.
    #[allow(clippy::unchecked_time_subtraction)]
    fn ago(secs: u64) -> Instant {
        Instant::now() - Duration::from_secs(secs)
    }

    /// Discriminant of the current activity as a stable label, read without
    /// holding the state lock across assertions.
    fn activity_label(d: &Arc<RatatuiDashboard>) -> &'static str {
        let s = d.state.lock().unwrap();
        let label = match s.activity {
            Activity::Idle => "idle",
            Activity::Thinking { .. } => "thinking",
            Activity::Running { .. } => "running",
        };
        drop(s);
        label
    }

    #[test]
    fn thinking_state_renders_spinner_kind_and_elapsed() {
        let d = make_dashboard();
        set_activity(&d, Activity::Thinking { since: ago(12) });
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(text.contains("thinking"), "kind label present: {text}");
        assert!(text.contains("model"), "operation source present: {text}");
        assert!(text.contains("12s"), "elapsed seconds present: {text}");
        assert!(has_spinner(&buf), "spinner glyph present: {text}");
    }

    #[test]
    fn running_state_renders_command_and_elapsed() {
        let d = make_dashboard();
        set_activity(
            &d,
            Activity::Running {
                since: ago(8),
                command: "pytest -q".into(),
            },
        );
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(text.contains("running: pytest -q"), "running cmd: {text}");
        assert!(text.contains("8s"), "elapsed seconds present: {text}");
        assert!(has_spinner(&buf), "spinner glyph present: {text}");
    }

    #[test]
    fn idle_state_renders_static_hint_no_spinner() {
        let d = make_dashboard();
        // Default activity is Idle.
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("waiting for next agent step"),
            "static hint: {text}"
        );
        assert!(!has_spinner(&buf), "no spinner while idle: {text}");
    }

    #[test]
    fn finished_state_has_no_spinner() {
        let d = make_dashboard();
        d.emit(run_ended_event());
        let buf = render_to_buffer(&snap(&d), 80, 12);
        assert!(!has_spinner(&buf), "no spinner once finished");
    }

    #[test]
    fn run_started_enters_thinking() {
        let d = make_dashboard();
        d.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });
        assert_eq!(activity_label(&d), "thinking");
    }

    #[test]
    fn observation_enters_thinking_for_next_model_call() {
        let d = make_dashboard();
        d.emit(StreamEvent::Observation {
            step: 1,
            content: "ok".into(),
            timestamp: "t".into(),
        });
        assert_eq!(activity_label(&d), "thinking");
    }

    #[test]
    fn assistant_message_returns_to_idle() {
        let d = make_dashboard();
        set_activity(
            &d,
            Activity::Thinking {
                since: Instant::now(),
            },
        );
        d.emit(StreamEvent::AssistantMessage {
            step: 1,
            content: "x".into(),
            cost_usd: None,
            timestamp: "t".into(),
        });
        assert_eq!(activity_label(&d), "idle");
    }

    #[test]
    fn bash_start_enters_running_then_result_returns_to_idle() {
        let d = make_dashboard();
        d.emit(StreamEvent::BashStart {
            step: 1,
            command: "ls".into(),
            timestamp: "t".into(),
        });
        assert_eq!(activity_label(&d), "running");
        // The running command is carried through for the footer label.
        let cmd = {
            let s = d.state.lock().unwrap();
            let command = match &s.activity {
                Activity::Running { command, .. } => command.clone(),
                Activity::Idle | Activity::Thinking { .. } => {
                    panic!("expected Running after BashStart")
                }
            };
            drop(s);
            command
        };
        assert_eq!(cmd, "ls");
        d.emit(StreamEvent::BashResult {
            step: 1,
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            timestamp: "t".into(),
        });
        assert_eq!(activity_label(&d), "idle");
    }

    #[test]
    fn run_ended_returns_to_idle() {
        let d = make_dashboard();
        set_activity(
            &d,
            Activity::Thinking {
                since: Instant::now(),
            },
        );
        d.emit(run_ended_event());
        assert_eq!(activity_label(&d), "idle");
    }

    #[test]
    fn stalled_operation_escalates_to_yellow() {
        let d = make_dashboard();
        // Below threshold: no escalation.
        set_activity(&d, Activity::Thinking { since: ago(5) });
        let buf = render_to_buffer(&snap(&d), 80, 12);
        assert!(
            !footer_has_fg(&buf, Color::Yellow),
            "no yellow before threshold"
        );

        // Past the 60s default threshold: escalate.
        set_activity(&d, Activity::Thinking { since: ago(65) });
        let buf = render_to_buffer(&snap(&d), 80, 12);
        assert!(
            footer_has_fg(&buf, Color::Yellow),
            "yellow escalation past threshold"
        );
    }

    #[test]
    fn stall_threshold_is_configurable() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.stall_threshold = Duration::from_secs(5);
            s.activity = Activity::Running {
                since: ago(6),
                command: "slow".into(),
            };
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        assert!(
            footer_has_fg(&buf, Color::Yellow),
            "escalates at the configured 5s threshold"
        );
    }

    #[test]
    fn spinner_advances_within_one_second() {
        // Two renders within the same wall-clock second must differ, so a
        // frozen render is visually distinguishable from a live one (AC4).
        let a = spinner_frame(Duration::from_millis(0));
        let b = spinner_frame(Duration::from_millis(500));
        assert_ne!(a, b, "spinner advances at least once per second");
    }

    #[test]
    fn elapsed_counter_resets_per_operation() {
        let d = make_dashboard();
        // A long thinking phase...
        set_activity(&d, Activity::Thinking { since: ago(90) });
        // ...then a fresh bash op starts: its counter is independent.
        d.emit(StreamEvent::BashStart {
            step: 1,
            command: "ls".into(),
            timestamp: "t".into(),
        });
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(text.contains("running: ls"), "running cmd: {text}");
        assert!(text.contains("0s"), "elapsed reset to 0 for new op: {text}");
        assert!(!text.contains("90s"), "no carryover from prior op: {text}");
    }

    #[test]
    fn idle_activity_is_not_active() {
        assert!(!Activity::Idle.is_active());
        assert!(
            Activity::Thinking {
                since: Instant::now()
            }
            .is_active()
        );
        assert!(
            Activity::Running {
                since: Instant::now(),
                command: "x".into()
            }
            .is_active()
        );
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
            rationale: String::new(),
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
            rationale: String::new(),
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
                rationale: String::new(),
            },
            responder: tx,
        });
        rx
    }

    // ---- in-dashboard incremental search (#637) ----

    /// Push `n` plain info lines `line1..=linen` into the feed for search tests.
    fn push_lines(d: &Arc<RatatuiDashboard>, n: usize) {
        let mut s = d.state.lock().unwrap();
        for i in 1..=n {
            s.log.push_back(LogLine {
                kind: LineKind::Info,
                text: format!("line{i}"),
            });
        }
        drop(s);
    }

    /// True if any cell in the buffer carries the current-match highlight
    /// (yellow background) — i.e. the active hit is visible on screen.
    fn has_current_match_highlight(buf: &Buffer) -> bool {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].style().bg == Some(Color::Yellow) {
                    return true;
                }
            }
        }
        false
    }

    fn press(d: &Arc<RatatuiDashboard>, code: KeyCode) {
        handle_key(d, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_str(d: &Arc<RatatuiDashboard>, text: &str) {
        for c in text.chars() {
            press(d, KeyCode::Char(c));
        }
    }

    /// Clone out the active `SearchState` (if any) without holding the lock
    /// across the assertions.
    fn search_state(d: &Arc<RatatuiDashboard>) -> Option<SearchState> {
        let s = d.state.lock().unwrap();
        let out = s.search.clone();
        drop(s);
        out
    }

    #[test]
    fn search_slash_enters_search_mode() {
        let d = make_dashboard();
        press(&d, KeyCode::Char('/'));
        let search = search_state(&d).unwrap();
        assert!(search.editing);
        assert!(search.query.is_empty());
        assert_eq!(search.current, 0);
    }

    #[test]
    fn search_typing_appends_and_matches_incrementally() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 3;
            s.last_log_width = 80;
        }
        push_lines(&d, 8); // line1..line8, viewport shows 3
        press(&d, KeyCode::Char('/'));
        type_str(&d, "line6");

        let s = snap(&d);
        assert_eq!(s.search.as_ref().unwrap().query, "line6");
        // Incremental find anchored to the (only) match and scrolled to it.
        let window: Vec<&LogLine> = s.log.iter().collect();
        let matches = search_matches(&window, "line6");
        assert_eq!(matches.len(), 1);
        assert!(
            !s.auto_follow,
            "typing should disengage auto-follow to jump"
        );
    }

    #[test]
    fn search_is_case_insensitive() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.log.push_back(LogLine {
                kind: LineKind::BashErr,
                text: "Tests FAILED in module foo".into(),
            });
        }
        let s = snap(&d);
        let window: Vec<&LogLine> = s.log.iter().collect();
        assert_eq!(search_matches(&window, "failed"), vec![0]);
        assert_eq!(search_matches(&window, "TESTS"), vec![0]);
        assert!(search_matches(&window, "passed").is_empty());
    }

    #[test]
    fn search_commit_then_navigate_wraps() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 3;
            s.last_log_width = 80;
            for i in 1..=6 {
                // "mark" appears on lines 0, 2, 4 (three matches).
                let text = if i % 2 == 1 {
                    format!("mark line {i}")
                } else {
                    format!("other line {i}")
                };
                s.log.push_back(LogLine {
                    kind: LineKind::Info,
                    text,
                });
            }
        }
        press(&d, KeyCode::Char('/'));
        type_str(&d, "mark");
        press(&d, KeyCode::Enter); // commit, settle on first match
        let committed = search_state(&d).unwrap();
        assert!(!committed.editing);
        assert_eq!(committed.current, 0);
        // n cycles forward 0 -> 1 -> 2 -> wrap 0
        press(&d, KeyCode::Char('n'));
        assert_eq!(search_state(&d).unwrap().current, 1);
        press(&d, KeyCode::Char('n'));
        assert_eq!(search_state(&d).unwrap().current, 2);
        press(&d, KeyCode::Char('n'));
        assert_eq!(search_state(&d).unwrap().current, 0);
        // N cycles backward 0 -> wrap 2
        press(&d, KeyCode::Char('N'));
        assert_eq!(search_state(&d).unwrap().current, 2);
    }

    #[test]
    fn search_esc_exits_keeps_scroll() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 3;
            s.last_log_width = 80;
        }
        push_lines(&d, 8);
        press(&d, KeyCode::Char('/'));
        type_str(&d, "line1"); // matches line1 at top -> scrolls up
        let scroll_at_match = snap(&d).scroll_offset;
        press(&d, KeyCode::Esc);
        let s = snap(&d);
        assert!(s.search.is_none(), "Esc clears search");
        assert!(!s.auto_follow, "scroll position must not snap back");
        assert_eq!(s.scroll_offset, scroll_at_match);
    }

    #[test]
    fn search_enter_empty_query_exits() {
        let d = make_dashboard();
        push_lines(&d, 4);
        press(&d, KeyCode::Char('/'));
        press(&d, KeyCode::Enter); // empty query
        assert!(search_state(&d).is_none());
    }

    #[test]
    fn search_backspace_to_empty_stays_in_mode() {
        let d = make_dashboard();
        push_lines(&d, 4);
        press(&d, KeyCode::Char('/'));
        type_str(&d, "x");
        press(&d, KeyCode::Backspace);
        let search = search_state(&d).unwrap();
        assert!(search.query.is_empty());
    }

    #[test]
    fn search_zero_matches_shows_0_0() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 5;
            s.last_log_width = 80;
        }
        push_lines(&d, 4);
        press(&d, KeyCode::Char('/'));
        type_str(&d, "zzz-no-such-token");
        let s = snap(&d);
        let buf = render_to_buffer(&s, 80, 11);
        let text = buffer_text(&buf);
        assert!(
            text.contains("0/0"),
            "zero-match counter should show 0/0; got:\n{text}"
        );
        assert!(
            !has_current_match_highlight(&buf),
            "no highlight when no match"
        );
    }

    #[test]
    fn search_does_not_steal_modal_keys() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);
        // '/' while a modal is open must not enter search nor send a decision.
        press(&d, KeyCode::Char('/'));
        let s = snap(&d);
        assert!(s.pending.is_some(), "modal stays open");
        assert!(s.search.is_none(), "search is not reachable behind a modal");
        assert!(rx.try_recv().is_err(), "no decision sent by '/'");
        // The modal verbs still work, byte-for-byte.
        press(&d, KeyCode::Char('y'));
        assert_eq!(rx.try_recv().unwrap(), ConfirmDecision::Approve);
    }

    #[test]
    fn footer_shows_search_hints_contextually() {
        let d = make_dashboard();
        // Idle: a '/' hint.
        {
            let s = snap(&d);
            let buf = render_to_buffer(&s, 80, 12);
            assert!(buffer_text(&buf).contains("/ search"));
        }
        // Editing: query + Enter/Esc hints.
        press(&d, KeyCode::Char('/'));
        type_str(&d, "boom");
        {
            let s = snap(&d);
            let buf = render_to_buffer(&s, 80, 12);
            let text = buffer_text(&buf);
            assert!(text.contains("search: boom"), "shows query; got:\n{text}");
            assert!(text.contains("[Enter] find"));
            assert!(text.contains("[Esc] cancel"));
        }
        // Committed: n/N/Esc hints.
        press(&d, KeyCode::Enter);
        {
            let s = snap(&d);
            let buf = render_to_buffer(&s, 80, 12);
            let text = buffer_text(&buf);
            assert!(text.contains("[n] next"), "shows n/N hints; got:\n{text}");
            assert!(text.contains("[N] prev"));
        }
    }

    #[test]
    fn search_jumps_to_offscreen_match() {
        // Success metric: a feed longer than the viewport with a known token
        // only on an off-screen line; after entering search, typing the token,
        // and committing, the rendered frame shows that line with the
        // current-match highlight and a counter >= 1/1.
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 5;
            s.last_log_width = 80;
        }
        {
            let mut s = d.state.lock().unwrap();
            // 20 filler lines, the unique token buried near the top (off-screen
            // when the feed auto-follows to the bottom).
            s.log.push_back(LogLine {
                kind: LineKind::BashErr,
                text: "UNIQUETOKEN the failing assertion".into(),
            });
            for i in 1..=20 {
                s.log.push_back(LogLine {
                    kind: LineKind::Info,
                    text: format!("filler line number {i}"),
                });
            }
            drop(s);
        }
        // Before search: the token line is off-screen (auto-follow at bottom).
        {
            let s = snap(&d);
            let buf = render_to_buffer(&s, 80, 11);
            assert!(
                !buffer_text(&buf).contains("UNIQUETOKEN"),
                "token should start off-screen"
            );
        }

        press(&d, KeyCode::Char('/'));
        type_str(&d, "UNIQUETOKEN");
        press(&d, KeyCode::Enter);
        press(&d, KeyCode::Char('n'));

        let s = snap(&d);
        let buf = render_to_buffer(&s, 80, 11);
        let text = buffer_text(&buf);
        assert!(
            text.contains("UNIQUETOKEN"),
            "matched line must be scrolled into view; got:\n{text}"
        );
        assert!(
            has_current_match_highlight(&buf),
            "the current match must carry the highlight; got:\n{text}"
        );
        assert!(
            text.contains("1/1"),
            "counter should read 1/1; got:\n{text}"
        );
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

    // ---- bracketed paste into input fields (#745) ----

    #[test]
    fn paste_multiline_into_feedback_retains_full_content_and_does_not_submit() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);

        // Enter reject-feedback mode.
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(snap(&d).feedback_input.as_deref(), Some(""));

        // Paste a ≥3-line clipboard payload with embedded newlines.
        let pasted = "line one\nline two\nline three";
        handle_paste(&d, pasted);

        // Full content retained; embedded newlines did NOT submit the field.
        assert_eq!(snap(&d).feedback_input.as_deref(), Some(pasted));
        assert!(rx.try_recv().is_err(), "paste must not submit the field");
        assert!(snap(&d).pending.is_some());

        // An explicit Enter submits the full multi-line value.
        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            rx.try_recv().unwrap(),
            ConfirmDecision::Reject(Some(pasted.to_owned()))
        );
    }

    #[test]
    fn paste_multiline_into_edit_retains_full_content_and_does_not_submit() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);

        // Enter edit mode (pre-filled with the pending command "x").
        handle_key(&d, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(snap(&d).edit_input.as_deref(), Some("x"));

        // Paste a multi-line command at the insertion point (end of buffer);
        // existing typed content ("x") is preserved.
        let pasted = "cmd1\ncmd2\ncmd3";
        handle_paste(&d, pasted);

        assert_eq!(snap(&d).edit_input.as_deref(), Some("xcmd1\ncmd2\ncmd3"));
        assert!(rx.try_recv().is_err(), "paste must not submit the field");
        assert!(snap(&d).pending.is_some());

        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            rx.try_recv().unwrap(),
            ConfirmDecision::Edit("xcmd1\ncmd2\ncmd3".to_owned())
        );
    }

    #[test]
    fn paste_preserves_typed_content_and_inserts_at_insertion_point() {
        let d = make_dashboard();
        let _rx = make_pending(&d);

        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        // Type some content first.
        for c in ['a', 'b'] {
            handle_key(&d, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        handle_paste(&d, "PASTED");
        // Continue typing after the paste.
        handle_key(&d, KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));

        assert_eq!(snap(&d).feedback_input.as_deref(), Some("abPASTEDz"));
    }

    #[test]
    fn paste_in_normal_feed_mode_is_ignored() {
        let d = make_dashboard();
        // No pending modal, no active input field.
        handle_paste(&d, "garbage\nthat\nshould\nbe\nignored");
        let s = snap(&d);
        assert!(s.feedback_input.is_none());
        assert!(s.edit_input.is_none());
        assert!(s.pending.is_none());
        // Feed state untouched.
        assert!(s.log.is_empty());
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn paste_on_choice_screen_is_ignored() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);
        // Modal open but no input field active (choice screen).
        handle_paste(&d, "no\nfield\nhere");
        assert!(snap(&d).feedback_input.is_none());
        assert!(snap(&d).edit_input.is_none());
        assert!(rx.try_recv().is_err());
        assert!(snap(&d).pending.is_some());
    }

    #[test]
    fn paste_into_search_query_appends_while_editing() {
        let d = make_dashboard();
        push_lines(&d, 3); // line1, line2, line3

        // Enter the `/`-search sub-mode (editing).
        press(&d, KeyCode::Char('/'));
        assert!(search_state(&d).is_some_and(|s| s.editing));

        // Type part of the query, then paste the rest.
        type_str(&d, "li");
        handle_paste(&d, "ne2");

        let search = search_state(&d).unwrap();
        assert_eq!(search.query, "line2");
        // Still editing — paste must not commit the search.
        assert!(search.editing);
    }

    #[test]
    fn paste_into_committed_search_is_ignored() {
        let d = make_dashboard();
        push_lines(&d, 3);

        press(&d, KeyCode::Char('/'));
        type_str(&d, "line");
        // Commit the search (leaves editing mode).
        press(&d, KeyCode::Enter);
        assert!(search_state(&d).is_some_and(|s| !s.editing));

        // Paste in committed (n/N navigation) mode is dropped, not appended.
        handle_paste(&d, "garbage");
        assert_eq!(search_state(&d).unwrap().query, "line");
    }

    #[test]
    fn paste_does_not_corrupt_open_search_when_modal_pending() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        // The operator had a `/` search open when the confirm modal was raised;
        // confirm() sets `pending` without clearing `search`.
        {
            let mut s = d.state.lock().unwrap();
            s.search = Some(SearchState {
                query: "abc".into(),
                current: 0,
                editing: true,
            });
        }
        // Pasting on the modal's choice screen must NOT append to the hidden
        // search query.
        handle_paste(&d, "XYZ");
        assert_eq!(search_state(&d).unwrap().query, "abc");
        // And nothing leaked into the modal input fields.
        assert!(snap(&d).feedback_input.is_none());
        assert!(snap(&d).edit_input.is_none());
    }

    #[test]
    fn paste_into_edit_normalizes_crlf_to_lf() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);
        // Enter edit mode (pre-filled with the pending command "x").
        handle_key(&d, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        // Windows-style clipboard payload with CRLF and a lone CR.
        handle_paste(&d, "a\r\nb\rc");
        // Stored value carries no carriage returns: what is submitted matches
        // what the renderer shows.
        assert_eq!(snap(&d).edit_input.as_deref(), Some("xa\nb\nc"));

        handle_key(&d, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            rx.try_recv().unwrap(),
            ConfirmDecision::Edit("xa\nb\nc".to_owned())
        );
    }

    #[test]
    fn paste_into_feedback_normalizes_crlf_to_lf() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        handle_paste(&d, "line1\r\nline2");
        assert_eq!(snap(&d).feedback_input.as_deref(), Some("line1\nline2"));
    }

    #[test]
    fn multiline_feedback_value_renders_without_panic() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        // A wide, multi-line paste that wraps inside the modal.
        handle_paste(
            &d,
            "a very long pasted feedback line that should wrap across the modal width\nsecond line\nthird line",
        );

        // Rendering the multi-line value must not panic on wide/wrapped input.
        let _ = render_to_buffer(&snap(&d), 100, 40);
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
            bell: Bell::silent(),
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
        // A bash command is in flight, so the footer surfaces the live activity
        // cell rather than the static idle hint (issue #649).
        assert!(
            text.contains("running: echo hi"),
            "footer should show the in-flight activity; got:\n{text}"
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
                    rationale: String::new(),
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
    fn draw_modal_renders_rationale_and_keeps_decision_keys() {
        // AC: rationale sentinel line AND the command both appear in the
        // confirm frame, and y/n/a still map to Approve/Reject/Abort.
        let sentinel = "RATIONALE_SENTINEL_because_directory_is_stale";
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
                    command: "rm -rf /tmp/dangerous".into(),
                    step: 2,
                    step_limit: 5,
                    cost_usd: 0.0099,
                    cache_marker: "cache:explicit",
                    rationale: format!("{sentinel}\nsecond reasoning line"),
                },
                responder: tx,
            });
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 30);
        let text = buffer_text(&buf);
        assert!(
            text.contains(sentinel),
            "modal should show rationale; got:\n{text}"
        );
        assert!(
            text.contains("rm -rf /tmp/dangerous"),
            "modal should still show command; got:\n{text}"
        );
        assert!(
            text.contains("reasoning"),
            "rationale region should be labelled; got:\n{text}"
        );

        // Decision keys unchanged: y -> Approve, a -> Abort, n -> reject mode.
        let mut rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(rx.try_recv().unwrap(), ConfirmDecision::Approve);

        let mut rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(rx.try_recv().unwrap(), ConfirmDecision::Abort);

        let _rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        let (feedback_some, pending_some) = {
            let s = d.state.lock().unwrap();
            (s.feedback_input.is_some(), s.pending.is_some())
        };
        assert!(feedback_some, "n should enter reject mode");
        assert!(pending_some);
    }

    #[test]
    fn draw_modal_shows_no_rationale_indicator_when_empty() {
        let d = make_dashboard();
        {
            let (tx, _rx) = oneshot::channel();
            let mut s = d.state.lock().unwrap();
            s.pending = Some(PendingPrompt {
                ctx: ConfirmContext {
                    tool_name: "bash".into(),
                    command: "ls".into(),
                    step: 0,
                    step_limit: 1,
                    cost_usd: 0.0,
                    cache_marker: "cache:explicit",
                    rationale: "   \n  ".into(),
                },
                responder: tx,
            });
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 25);
        let text = buffer_text(&buf);
        assert!(
            text.contains("(no rationale provided)"),
            "empty rationale should degrade gracefully; got:\n{text}"
        );
        assert!(text.contains("ls"), "command still shown; got:\n{text}");
    }

    #[test]
    fn draw_modal_marks_truncated_rationale() {
        let d = make_dashboard();
        let many_lines = (0..crate::agent::confirm::RATIONALE_MAX_LINES + 50)
            .map(|i| format!("reasoning line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let capped = crate::agent::confirm::cap_rationale(&many_lines);
        {
            let (tx, _rx) = oneshot::channel();
            let mut s = d.state.lock().unwrap();
            s.pending = Some(PendingPrompt {
                ctx: ConfirmContext {
                    tool_name: "bash".into(),
                    command: "ls".into(),
                    step: 0,
                    step_limit: 1,
                    cost_usd: 0.0,
                    cache_marker: "cache:explicit",
                    rationale: capped,
                },
                responder: tx,
            });
            s.rationale_scroll = usize::MAX; // jump to end so the marker is visible
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 40);
        let text = buffer_text(&buf);
        assert!(
            text.contains("truncated"),
            "truncation marker must be visible; got:\n{text}"
        );
    }

    #[test]
    fn draw_modal_shows_full_wrapped_command() {
        // A long single-line command wraps to several rows; the top region must
        // be sized by wrapped rows so the tail (UNIQUETAIL) stays visible and the
        // operator never approves a command they can't fully see.
        let mut command = "run".to_string();
        for i in 0..60 {
            command.push_str(" step");
            command.push_str(&i.to_string());
        }
        command.push_str(" UNIQUETAIL");
        let d = make_dashboard();
        {
            let (tx, _rx) = oneshot::channel();
            let mut s = d.state.lock().unwrap();
            s.pending = Some(PendingPrompt {
                ctx: ConfirmContext {
                    tool_name: "bash".into(),
                    command,
                    step: 0,
                    step_limit: 1,
                    cost_usd: 0.0,
                    cache_marker: "cache:explicit",
                    rationale: "because reasons".into(),
                },
                responder: tx,
            });
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 40);
        let text = buffer_text(&buf);
        assert!(
            text.contains("UNIQUETAIL"),
            "the full wrapped command must be visible; got:\n{text}"
        );
    }

    #[test]
    fn rationale_scroll_keys_move_window_when_modal_open() {
        let d = make_dashboard();
        let long = (0..40)
            .map(|i| format!("rline{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (tx, _rx) = oneshot::channel();
        {
            let mut s = d.state.lock().unwrap();
            s.pending = Some(PendingPrompt {
                ctx: ConfirmContext {
                    tool_name: "bash".into(),
                    command: "ls".into(),
                    step: 0,
                    step_limit: 1,
                    cost_usd: 0.0,
                    cache_marker: "cache:explicit",
                    rationale: long,
                },
                responder: tx,
            });
            // Render-time metrics the key handler clamps against (normally set
            // by draw_frame): 40 display rows visible 8 at a time.
            s.last_rationale_total_rows = 40;
            s.last_rationale_visible_rows = 8;
        }
        // PageDown should advance the rationale window and keep the modal open.
        handle_key(&d, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        let (scroll_after_pgdn, pending_after_pgdn) = {
            let s = d.state.lock().unwrap();
            (s.rationale_scroll, s.pending.is_some())
        };
        assert!(scroll_after_pgdn > 0, "rationale should have scrolled");
        assert!(pending_after_pgdn, "modal must stay open");

        // Home returns to the top.
        handle_key(&d, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let (scroll_after_home, pending_after_home) = {
            let s = d.state.lock().unwrap();
            (s.rationale_scroll, s.pending.is_some())
        };
        assert_eq!(scroll_after_home, 0);
        assert!(pending_after_home);
    }

    #[test]
    fn whitespace_rationale_does_not_consume_scroll_keys() {
        // A whitespace-only rationale renders "(no rationale provided)" with no
        // scrollable lines, so scroll keys must fall through to the feed rather
        // than being silently swallowed by the rationale scroller.
        let d = make_dashboard();
        let (tx, _rx) = oneshot::channel();
        {
            let mut s = d.state.lock().unwrap();
            s.last_log_height = 5;
            s.last_log_width = 10;
            for i in 1..=8 {
                s.log.push_back(LogLine {
                    kind: LineKind::Info,
                    text: format!("line{i}"),
                });
            }
            // Many blank lines: trims to empty, but raw line count is non-zero.
            s.pending = Some(PendingPrompt {
                ctx: ConfirmContext {
                    tool_name: "bash".into(),
                    command: "ls".into(),
                    step: 0,
                    step_limit: 1,
                    cost_usd: 0.0,
                    cache_marker: "cache:explicit",
                    rationale: "\n".repeat(20),
                },
                responder: tx,
            });
        }
        handle_key(&d, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        let (rationale_scroll, auto_follow, pending_some) = {
            let s = d.state.lock().unwrap();
            (s.rationale_scroll, s.auto_follow, s.pending.is_some())
        };
        assert_eq!(rationale_scroll, 0, "rationale must not have scrolled");
        assert!(!auto_follow, "scroll key should have driven the feed");
        assert!(pending_some, "modal must stay open");
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
                    rationale: String::new(),
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
                    rationale: String::new(),
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
}
