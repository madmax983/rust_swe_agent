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
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
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
const MAX_RETAINED_ENTRY_BYTES: usize = 100 * 1024; // 100 KB cap per entry

/// Max logical input lines rendered in the modal controls region (issue #745
/// review). A large multi-line paste must not push the command/rationale/submit
/// hint off-screen; the buffer still retains the full value, only the display is
/// capped. Mirrors the read-only command preview cap (12 lines) — but shows the
/// TAIL, since the caret rides the last line of an actively-edited field.
const MODAL_INPUT_MAX_LINES: usize = 12;

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

/// Resolve the effective per-task cost cap to show in the header burn-down
/// (issue #640) from the two independently-enforced budget caps
/// (`agent.cost_limit_usd`, `agent.per_task_budget_usd`). Both are checked
/// against cumulative spend every step (see `DefaultAgent::step`), so
/// whichever is smaller fires first; the smaller of the two configured
/// values is therefore the cap that actually governs the run and is what
/// the operator needs to watch. `None` when neither is configured.
#[must_use]
pub fn effective_cost_cap_usd(
    cost_limit_usd: Option<f64>,
    per_task_budget_usd: Option<f64>,
) -> Option<f64> {
    match (cost_limit_usd, per_task_budget_usd) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

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

#[allow(clippy::struct_excessive_bools)]
struct DashboardState {
    task: Option<String>,
    model: Option<String>,
    /// Monotonic anchor captured locally when `RunStarted` arrives (issue
    /// #640), used to render the live elapsed wall-clock. A monotonic
    /// `Instant` rather than the RFC3339 `started_at` string carried by
    /// `StreamEvent::RunStarted` — the dashboard never needs to parse or
    /// re-derive a `Duration` from wall-clock text, and (unlike a wall-clock
    /// timestamp) it can't be skewed by clock adjustments during the run.
    started_at_instant: Option<Instant>,
    cost_usd: f64,
    /// Effective per-task cost ceiling for the header burn-down (issue
    /// #640): `min(cost_limit_usd, per_task_budget_usd)` over whichever of
    /// the two is configured, resolved once at dashboard construction (see
    /// [`effective_cost_cap_usd`]). `None` when neither cap is configured.
    cost_cap_usd: Option<f64>,
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
    /// Absolute terminal row of the first visible feed line, captured by the
    /// renderer each frame (issue #734) so `handle_mouse` can translate a
    /// click's screen row into a log index without duplicating the layout
    /// math in `draw_frame`.
    feed_top_row: u16,
    /// Rows moved per scroll-wheel notch, mirroring `stall_threshold`'s
    /// env-configurability (issue #734, AC2). Default 3; see
    /// `mouse_scroll_step_from_env`.
    mouse_scroll_step: usize,
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
    search: Option<SearchState>,
    /// True while the operator has pressed the global stop key (Ctrl-Q) and we
    /// are waiting for the y/N confirmation before aborting the run (#638).
    stop_pending: bool,
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
    /// True while the `?` help overlay (issue #639) is shown. Takes absolute
    /// priority in `handle_key`: every keystroke other than the ones that
    /// close or scroll it is swallowed, so opening it can never resolve a
    /// pending confirm prompt or advance/abort the run.
    help_open: bool,
    /// Scroll offset (in content lines) within the help overlay, reset to 0
    /// each time it opens.
    help_scroll: usize,
    /// Inner height of the help overlay, captured by the renderer so the key
    /// handler can clamp scrolling to content that actually overflows it.
    help_viewport_height: u16,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            task: None,
            model: None,
            started_at_instant: None,
            cost_usd: 0.0,
            cost_cap_usd: None,
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
            feed_top_row: 0,
            mouse_scroll_step: DEFAULT_MOUSE_SCROLL_STEP,
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
            search: None,
            stop_pending: false,
            rationale_scroll: 0,
            last_rationale_total_rows: 0,
            last_rationale_visible_rows: 0,
            activity: Activity::Idle,
            stall_threshold: Duration::from_secs(DEFAULT_STALL_THRESHOLD_SECS),
            help_open: false,
            help_scroll: 0,
            help_viewport_height: 20,
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

/// Default rows moved per scroll-wheel notch (issue #734, AC2): one notch
/// has the same effect as this many `Up`/`Down` keypresses.
const DEFAULT_MOUSE_SCROLL_STEP: usize = 3;

/// Resolve the mouse scroll step from `MAXWELL_MOUSE_SCROLL_STEP`, falling
/// back to [`DEFAULT_MOUSE_SCROLL_STEP`] when unset, unparsable, or zero.
fn mouse_scroll_step_from_env() -> usize {
    std::env::var("MAXWELL_MOUSE_SCROLL_STEP")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MOUSE_SCROLL_STEP)
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

/// Wraps `tokio::sync::Notify` with a monotonic version counter bumped on
/// every `notify_waiters()` call (issue #734 review). `Notify` alone can
/// only tell `renderer_loop` "wake up", not "did state actually change since
/// I last drew" — and those aren't the same question once a redraw can be
/// conditionally skipped (as the mouse-motion redraw-skip does): a
/// `notify_waiters()` call from another task (e.g. a fresh confirm prompt or
/// log line) can land in the same `select!` poll as an ignored mouse event,
/// or in the narrow window between one loop iteration finishing and the
/// next one's `Notified` being (re)registered, and `Notify` stores no permit
/// to recover it either way. Comparing this counter — bumped with `Release`
/// before the wakeup is even delivered, read with `Acquire` — lets
/// `renderer_loop` detect "something changed since my last draw" regardless
/// of exactly which `select!` branch happened to win, closing that class of
/// lost-redraw race without touching any of the ~60 existing
/// `notify_waiters()` call sites (the method name/signature are unchanged).
struct RedrawNotify {
    inner: Notify,
    version: std::sync::atomic::AtomicU64,
}

impl RedrawNotify {
    fn new() -> Self {
        Self {
            inner: Notify::new(),
            version: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn notify_waiters(&self) {
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.inner.notify_waiters();
    }

    fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.inner.notified()
    }

    fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Acquire)
    }
}

pub struct RatatuiDashboard {
    state: Mutex<DashboardState>,
    notify: RedrawNotify,
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
    ///
    /// `cost_cap_usd` is the effective per-task cost ceiling to show in the
    /// header burn-down (issue #640) — see [`effective_cost_cap_usd`]. Pass
    /// `None` when no cap is configured, or for surfaces (e.g. the
    /// `--yolo` monitor) that don't render one.
    ///
    /// `step_limit` is `config.agent.step_limit`, seeded into the header
    /// from construction (issue #640 review) rather than left at its
    /// `Default` of `0` until the first operator confirm prompt populates
    /// it — a run with no confirm prompts (e.g. every command auto-approved)
    /// would otherwise never show a real step count or trip the step-limit
    /// warning color.
    ///
    /// `initial_cost_usd` seeds the running cost total shown by the burn-down
    /// (PR #992 review). It's `0.0` for a fresh run, but on `mini --resume`/
    /// `--continue` the agent's `RunStarted` event fires *before*
    /// `ResumeState.total_cost_usd` is folded into `DefaultAgent`, so no
    /// stream event ever carries the prior spend to the dashboard — every
    /// subsequent `AssistantMessage` accumulates on top of whatever this
    /// starts at (see the `RunStarted`/`AssistantMessage` handlers below), so
    /// omitting it would under-report a resumed run's true spend by its
    /// entire pre-resume cost until a confirm prompt or `RunEnded`
    /// happened to re-sync it from the agent's authoritative total.
    pub fn start(
        is_monitor: bool,
        cancel_tx: Option<watch::Sender<bool>>,
        bell_enabled: bool,
        cost_cap_usd: Option<f64>,
        step_limit: u32,
        initial_cost_usd: f64,
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
        // SGR mouse capture (issue #734): best-effort like bracketed paste
        // above — a terminal that doesn't support it just never emits
        // `Event::Mouse`, and teardown's unconditional `DisableMouseCapture`
        // is harmless either way.
        let _ = execute!(stdout, EnableMouseCapture);
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
                mouse_scroll_step: mouse_scroll_step_from_env(),
                cost_cap_usd,
                step_limit,
                cost_usd: initial_cost_usd,
                ..DashboardState::default()
            }),
            notify: RedrawNotify::new(),
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

        // Don't advance the feed while a search is active: scroll_to_line
        // already positioned the viewport at the match, and moving selected_index
        // forward on each new append would scroll away from it.
        let auto_follow_selection =
            was_at_end && !s.detail_open && s.search.is_none() && (!s.is_monitor || s.auto_follow);
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
    // Disable mouse capture and bracketed paste before leaving the alt-screen
    // so neither mode is leaked into the operator's shell on teardown —
    // including the panic / early-exit path, since this runs from
    // `RatatuiDashboardHandle::drop` (issue #745, issue #734).
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
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
                started_at: _,
            } => {
                {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    s.task = Some(task.clone());
                    s.model = Some(model.clone());
                    s.started_at_instant = Some(Instant::now());
                    // The step-1 model call begins now (issue #649): there is
                    // no `model-call-started` event, so the first model-thinking
                    // window opens at run start and closes at `AssistantMessage`.
                    s.activity = Activity::Thinking {
                        since: Instant::now(),
                    };
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
                    // `cost_usd` here is this single response's cost
                    // (`resp.usage.cost_usd`, computed per-call from that
                    // response's token usage — see `DefaultAgent::step` and
                    // `ModelUsage`), not a running total, so it must
                    // accumulate rather than overwrite. Overwriting made the
                    // header's cost display silently regress to the last
                    // turn's cost on every assistant message; harmless when
                    // only a raw "cost $X" was shown, but it under-reports
                    // budget usage against the burn-down's `%`/warning
                    // threshold on any run that doesn't hit another confirm
                    // prompt (e.g. auto-approved bash) to re-sync it from
                    // the agent's true cumulative total (PR #992 review).
                    if let Some(cost) = cost_usd {
                        s.cost_usd += cost;
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
                    Some(content),
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
                let full_content = if stdout.is_empty() && stderr.is_empty() {
                    None
                } else if stderr.is_empty() {
                    Some(stdout)
                } else if stdout.is_empty() {
                    Some(stderr)
                } else {
                    Some(truncate_to_cap(format!(
                        "{stdout}\n--- stderr ---\n{stderr}"
                    )))
                };
                self.append(kind, summary, full_content);
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
                self.append(
                    LineKind::BashRun,
                    format!("step {step} tool: {label}"),
                    None,
                );
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
                    Some(content),
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
                    Some(content),
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
    // Spinner cadence while an operation is in flight (issue #649). The tick
    // branch is guarded by `if active`, so an idle/finished dashboard redraws
    // solely on `Notify`/keystrokes — zero animation frames, no CPU spin while
    // nothing is happening (AC3). `Skip` prevents the default `Burst` behaviour
    // from firing a flurry of catch-up ticks when an operation resumes after an
    // idle gap during which the interval was never awaited.
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Whether the *next* loop iteration should redraw. Defaults true (every
    // event redraws, as before); the mouse arm below can clear it. Needed
    // because `EnableMouseCapture` requests any-event motion tracking, so a
    // terminal that honors it emits an `Event::Mouse(Moved)` (or `Drag`) for
    // every cell the pointer crosses even when the button isn't held —
    // `handle_mouse` ignores those, but without this flag the unconditional
    // `draw_frame` below would still re-render the whole screen once per
    // pointer-move event, turning idle mouse motion into a redraw storm
    // (issue #734 review).
    let mut redraw = true;
    // The `RedrawNotify` version as of our last actual draw. The mouse arm
    // ORs its `handle_mouse` result with "has this changed since", so an
    // ignored mouse event can only suppress the next redraw when nothing
    // else needed one either — closing the lost-redraw race a bare `redraw`
    // bool can't (issue #734 review; see `RedrawNotify`'s doc comment).
    let mut last_drawn_version = dash.notify.version();
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

        if redraw {
            if let Err(err) = draw_frame(&dash, &mut terminal) {
                tracing::warn!(?err, "ratatui draw failed");
            }
            last_drawn_version = dash.notify.version();
        }
        redraw = true;
        tokio::select! {
            // `biased` makes `notified` win any tie against `events.next()`
            // (issue #734 review): without it, `select!`'s default random
            // choice could let an ignored mouse event (which clears `redraw`
            // above) win a race against a same-poll `notify_waiters()` call
            // from another task — e.g. a fresh confirm prompt or log line —
            // silently dropping that redraw since the `notified` future is
            // cancelled unread. Biased polling costs nothing here: every
            // branch's body other than `events.next()` is a no-op, and a
            // mouse/key event not chosen this poll simply stays queued in
            // the stream for the next one, so nothing is lost by picking
            // `notified` first when both are ready.
            biased;
            _ = &mut shutdown => break,
            () = &mut notified => {}
            _ = tick.tick(), if active => {}
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) => handle_key(&dash, key),
                    Some(Ok(Event::Mouse(mouse))) => {
                        redraw = handle_mouse(&dash, mouse)
                            || dash.notify.version() != last_drawn_version;
                    }
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

    // In interactive mode the feed is sliced by feed_scroll_top (entry-based),
    // not by scroll_offset. Bring the matched entry into the visible window.
    if !s.is_monitor {
        let take_from = s.log.len().saturating_sub(MAX_LOG_LINES.min(s.log.len()));
        let abs_idx = take_from + line_idx;
        let viewport = s.viewport_height as usize;
        if viewport > 0 {
            if abs_idx < s.feed_scroll_top {
                s.feed_scroll_top = abs_idx;
            } else if abs_idx >= s.feed_scroll_top + viewport {
                s.feed_scroll_top = abs_idx + 1 - viewport;
            }
        }
    }
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

/// Current scroll offset and max scroll bound for the feed, in wrapped
/// display rows. Shared by `perform_scroll` (keyboard, one row/page at a
/// time) and the mouse wheel handler, which moves by an arbitrary step and
/// must not re-run `total_wrapped_lines` — an O(log size) wrap of the whole
/// feed — once per scroll notch (issue #734 review).
fn feed_scroll_bounds(s: &DashboardState) -> (usize, usize) {
    let total = total_wrapped_lines(&s.log, s.last_log_width);
    let max_scroll = total.saturating_sub(s.last_log_height);
    let current_offset = if s.auto_follow {
        max_scroll
    } else {
        s.scroll_offset
    };
    (current_offset, max_scroll)
}

fn perform_scroll(s: &mut DashboardState, code: KeyCode) -> bool {
    let (current_offset, max_scroll) = feed_scroll_bounds(s);

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

    // Help overlay (issue #639): takes absolute priority over every other
    // state. Only the keys that close or scroll it are handled; everything
    // else — including Ctrl-C/Ctrl-Q — is swallowed, so showing help can
    // never send input to the agent, resolve a pending confirm prompt, or
    // advance/abort the run (AC4).
    if s.help_open {
        let content_len = help_overlay_lines_count();
        let handled = match key.code {
            KeyCode::Char('?') | KeyCode::Esc => {
                s.help_open = false;
                s.help_scroll = 0;
                true
            }
            KeyCode::Up | KeyCode::Char('k' | 'K') => {
                s.help_scroll = s.help_scroll.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j' | 'J') => {
                let max = content_len.saturating_sub(s.help_viewport_height as usize);
                s.help_scroll = (s.help_scroll + 1).min(max);
                true
            }
            KeyCode::PageUp => {
                let page = (s.help_viewport_height as usize).max(1);
                s.help_scroll = s.help_scroll.saturating_sub(page);
                true
            }
            KeyCode::PageDown => {
                let page = (s.help_viewport_height as usize).max(1);
                let max = content_len.saturating_sub(page);
                s.help_scroll = (s.help_scroll + page).min(max);
                true
            }
            KeyCode::Home => {
                s.help_scroll = 0;
                true
            }
            KeyCode::End => {
                s.help_scroll = content_len.saturating_sub(s.help_viewport_height as usize);
                true
            }
            _ => false,
        };
        if handled {
            drop(s);
            dash.notify.notify_waiters();
        }
        return;
    }

    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'));

    // Global stop gesture (Ctrl-Q, #638): works in every state — mid-activity,
    // modal-open, and monitor/yolo mode where no modal ever appears.
    let ctrl_q = key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('q' | 'Q'));

    // Toggle the help overlay open (issue #639). `?` is reserved globally
    // except: while the operator is typing free text (rejection feedback, an
    // edited command, or an in-progress search query), where it must stay a
    // literal character; and while the stop confirmation is showing, which
    // already owns all non-y/n/Esc input.
    //
    // Plain Shift must be accepted alongside no modifiers: terminals that
    // report Shift for printable characters (e.g. crossterm's Windows
    // parser) deliver `?` as `Char('?')` with `KeyModifiers::SHIFT`, matching
    // the convention `handle_key_normal`/`handle_key_search` already use.
    //
    // An in-progress search query only counts as "typing" while it is the
    // surface actually receiving keystrokes. A confirm prompt that arrives
    // mid-edit (`confirm()` doesn't clear `s.search`) shadows it — input goes
    // to the modal, not `handle_key_search` — so `search.editing` must be
    // ignored whenever `s.pending` is set, mirroring the precedence
    // `active_input_target` already uses for paste routing.
    let search_editing_visible =
        s.pending.is_none() && s.search.as_ref().is_some_and(|se| se.editing);
    let typing_free_text =
        s.feedback_input.is_some() || s.edit_input.is_some() || search_editing_visible;
    let plain_or_shift = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    if !typing_free_text
        && !s.stop_pending
        && plain_or_shift
        && matches!(key.code, KeyCode::Char('?'))
    {
        s.help_open = true;
        s.help_scroll = 0;
        drop(s);
        dash.notify.notify_waiters();
        return;
    }

    // When a stop confirmation is waiting, y/Y and Ctrl-C immediately abort;
    // n/N/Esc cancel; everything else is swallowed so a stray keypress cannot
    // accidentally approve a pending modal or navigate the feed.
    //
    // Edge case: if the run ends naturally (RunEnded sets `finished`) while
    // stop_pending is true and the operator has not yet answered, the stale
    // confirmation must be dismissed automatically so the finished-close
    // path (q/Esc → should_exit) becomes reachable again (#638).
    if s.stop_pending {
        if s.finished.is_some() {
            s.stop_pending = false;
            // fall through to normal finished-close handling below
        } else {
            let confirm_abort = ctrl_c || matches!(key.code, KeyCode::Char('y' | 'Y'));
            if confirm_abort {
                s.stop_pending = false;
                if let Some(pending) = s.pending.take() {
                    s.feedback_input = None;
                    s.edit_input = None;
                    drop(s);
                    let _ = pending.responder.send(ConfirmDecision::Abort);
                } else {
                    if let Some(ref tx) = dash.cancel_tx {
                        let _ = tx.send(true);
                    }
                    drop(s);
                }
                dash.notify.notify_waiters();
            } else if matches!(key.code, KeyCode::Char('n' | 'N') | KeyCode::Esc) {
                s.stop_pending = false;
                drop(s);
                dash.notify.notify_waiters();
            }
            return;
        }
    }

    // Activate the stop confirmation when the run is still live.
    if ctrl_q && s.finished.is_none() {
        s.stop_pending = true;
        drop(s);
        dash.notify.notify_waiters();
        return;
    }

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
                        // Rationale scroll takes priority; only move the feed
                        // cursor when the rationale fits entirely on screen.
                        if !perform_rationale_scroll(&mut s, &pending.ctx, KeyCode::Up) {
                            move_cursor_up(&mut s);
                        }
                        dash.notify.notify_waiters();
                        handled_by_navigation = true;
                    }
                    KeyCode::Down | KeyCode::Char('j' | 'J') => {
                        if !perform_rationale_scroll(&mut s, &pending.ctx, KeyCode::Down) {
                            move_cursor_down(&mut s);
                        }
                        dash.notify.notify_waiters();
                        handled_by_navigation = true;
                    }
                    KeyCode::Esc if s.detail_open => {
                        // Close detail pane without aborting the pending
                        // confirmation. A second Esc will then reach
                        // handle_key_normal and abort the modal.
                        s.detail_open = false;
                        dash.notify.notify_waiters();
                        handled_by_navigation = true;
                    }
                    KeyCode::Enter if !s.detail_open => {
                        // Open the detail inspector; draw_modal is guarded by
                        // !snap.detail_open so the modal hides while the pane
                        // is open. A following Esc will close the pane, then a
                        // second Esc aborts the pending confirmation.
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

        // Detail inspector consumes all keys while open so that '/', Esc,
        // j/k, etc. are not intercepted by search or finished-close handling.
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
                    KeyCode::PageUp => {
                        let page = s.viewport_height as usize;
                        for _ in 0..page {
                            move_cursor_up(&mut s);
                        }
                        dash.notify.notify_waiters();
                    }
                    KeyCode::PageDown => {
                        let page = s.viewport_height as usize;
                        for _ in 0..page {
                            move_cursor_down(&mut s);
                        }
                        dash.notify.notify_waiters();
                    }
                    KeyCode::Home => {
                        if !s.log.is_empty() {
                            s.selected_index = Some(0);
                            s.feed_scroll_top = 0;
                            dash.notify.notify_waiters();
                        }
                    }
                    KeyCode::End => {
                        let last = s.log.len().saturating_sub(1);
                        s.selected_index = Some(last);
                        dash.notify.notify_waiters();
                    }
                    KeyCode::Enter => {
                        if let Some(idx) = s.selected_index {
                            if idx < s.log.len() {
                                s.detail_open = true;
                                s.detail_scroll_top = 0;
                                drop(s);
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

/// Handle a mouse event (issue #734): wheel-scroll and click-to-select for
/// the main step feed and the detail inspector log, additive to (never a
/// replacement for) the keyboard bindings above.
///
/// Mirrors `handle_key`'s focus rules: mouse input is ignored outright
/// whenever a modal or overlay owns focus — the help overlay, the stop
/// confirmation, a pending confirm prompt (including its feedback/edit
/// sub-modes), or an active search — so a stray wheel notch or click can
/// never change the selection out from under those (AC4). What's left, in
/// priority order matching the keyboard's, is: the detail inspector (if
/// open), the read-only monitor feed, or the interactive step feed.
///
/// Returns whether the renderer should redraw on account of this event.
/// `EnableMouseCapture` requests any-event motion tracking, so terminals
/// that honor it emit a `Moved`/`Drag` event for every cell the pointer
/// crosses even with no button held; `renderer_loop` uses this return value
/// to skip the (comparatively expensive) full-frame redraw for those and
/// other kinds this function doesn't act on, so idle mouse motion can't
/// drive a redraw storm (issue #734 review).
fn handle_mouse(dash: &Arc<RatatuiDashboard>, mouse: MouseEvent) -> bool {
    let mut s = dash
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Neither `pending` nor a leftover `search` owns focus when `detail_open`
    // is also true: opening the detail inspector (Enter) hides both the
    // confirm modal and the search bar behind it (`draw` skips them whenever
    // `detail_open`), and `handle_key` already routes scroll keys to the
    // detail pane in that state regardless of `pending`/`search` — see the
    // `s.detail_open` arm nested inside `s.pending.take()` below, and the
    // "no modal open" branch's `if s.detail_open { .. } else { <search
    // routing> }` structure, neither of which ever consults `search` while
    // `detail_open` is true. `confirm()` doesn't clear an active `/` search,
    // so `pending` and `search` can both be `Some` at once while `detail_open`
    // is true; the mouse wheel must match keyboard behavior there too, or it
    // silently does nothing over a pane that's visibly scrollable (issue
    // #734 review). `feedback_input`/`edit_input` still win unconditionally:
    // they can only be set while `!detail_open` (`n`/`e` are only reachable
    // from the non-detail-open branch of `handle_key`), so this never lets
    // mouse input reach an active feedback/edit text buffer.
    let modal_owns_focus = s.help_open
        || s.stop_pending
        || s.feedback_input.is_some()
        || s.edit_input.is_some()
        || (!s.detail_open && (s.pending.is_some() || s.search.is_some()));
    if modal_owns_focus {
        return false;
    }

    match mouse.kind {
        // `detail_open`/`is_monitor` resolve their scroll bound exactly once
        // per event rather than once per stepped row: `get_max_detail_scroll`
        // wraps the selected entry's full text (up to 100 KB) and
        // `feed_scroll_bounds` wraps the whole feed, so re-running either
        // `mouse_scroll_step` times per notch would be O(step * log size)
        // for no benefit (issue #734 review). `move_cursor_up`/`_down` are
        // O(1), so the feed-selection branch keeps the step loop.
        MouseEventKind::ScrollUp => {
            if s.detail_open {
                s.detail_scroll_top = s.detail_scroll_top.saturating_sub(s.mouse_scroll_step);
            } else if s.is_monitor {
                let (current_offset, _max_scroll) = feed_scroll_bounds(&s);
                let next_offset = current_offset.saturating_sub(s.mouse_scroll_step);
                if next_offset != current_offset {
                    s.scroll_offset = next_offset;
                    s.auto_follow = false;
                }
            } else {
                for _ in 0..s.mouse_scroll_step {
                    move_cursor_up(&mut s);
                }
            }
        }
        MouseEventKind::ScrollDown => {
            if s.detail_open {
                let max_scroll = get_max_detail_scroll(&s);
                s.detail_scroll_top = (s.detail_scroll_top + s.mouse_scroll_step).min(max_scroll);
            } else if s.is_monitor {
                let (current_offset, max_scroll) = feed_scroll_bounds(&s);
                let next_offset = (current_offset + s.mouse_scroll_step).min(max_scroll);
                if next_offset != current_offset {
                    s.scroll_offset = next_offset;
                    s.auto_follow = false;
                }
            } else {
                for _ in 0..s.mouse_scroll_step {
                    move_cursor_down(&mut s);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // Reuses the same `selected_index` the keyboard cursor drives
            // (AC3) — only the main feed has clickable rows in this slice,
            // so this is skipped while the detail pane covers it or in
            // read-only monitor mode, which has no per-row selection.
            let feed_top_row = s.feed_top_row;
            if !s.detail_open && !s.is_monitor && mouse.row >= feed_top_row {
                let offset = (mouse.row - feed_top_row) as usize;
                if offset < s.viewport_height as usize {
                    let idx = s.feed_scroll_top + offset;
                    if idx < s.log.len() {
                        s.selected_index = Some(idx);
                    }
                }
            }
        }
        _ => return false,
    }
    drop(s);
    dash.notify.notify_waiters();
    true
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

/// Which text field a paste (or any inserted text) is directed at, in precedence
/// order. Centralizes the routing so the paste path delegates to one definition
/// instead of re-deriving it (issue #745 review).
///
/// `handle_key` encodes this same precedence structurally — feedback/edit only
/// exist while a modal is pending, a pending modal on its choice screen accepts
/// no text, and an actively-edited `/` search owns input only when no modal is
/// up — and must stay in sync with it.
enum InputTarget {
    Feedback,
    Edit,
    Search,
    None,
}

fn active_input_target(s: &DashboardState) -> InputTarget {
    if s.feedback_input.is_some() {
        InputTarget::Feedback
    } else if s.edit_input.is_some() {
        InputTarget::Edit
    } else if s.pending.is_some() {
        // Modal on its choice screen: no text input is active. Critically this
        // shadows the search branch, so an open `/` search hidden behind the
        // modal is never appended to (it would surface corrupted on dismissal).
        InputTarget::None
    } else if s.search.as_ref().is_some_and(|se| se.editing) {
        InputTarget::Search
    } else {
        InputTarget::None
    }
}

/// Insert bracketed-paste content (issue #745) into whichever input field is
/// active per [`active_input_target`]. crossterm delivers the whole clipboard
/// payload as one `Event::Paste(String)` — including embedded newlines — so the
/// entire value lands in the buffer without any character being interpreted as
/// `Enter` (which would otherwise submit the field at the first newline).
///
/// The paste is appended at the buffer's end-of-buffer insertion point (see the
/// issue's Out of Scope), so existing typed content is preserved. Line endings
/// are normalized only inside the consuming arms, so a paste with no active
/// field allocates nothing and can never corrupt feed state (AC5).
fn handle_paste(dash: &Arc<RatatuiDashboard>, text: &str) {
    let mut s = dash
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // While the stop confirmation is pending the UI only accepts y/n/Esc;
    // drop pastes so they cannot silently accumulate in a hidden input buffer.
    if s.stop_pending {
        return;
    }
    match active_input_target(&s) {
        InputTarget::Feedback => {
            if let Some(buffer) = s.feedback_input.as_mut() {
                buffer.push_str(&normalize_pasted(text));
            }
        }
        InputTarget::Edit => {
            if let Some(buffer) = s.edit_input.as_mut() {
                buffer.push_str(&normalize_pasted(text));
            }
        }
        InputTarget::Search => {
            // Paste appends to the live query, preserving the pre-bracketed-paste
            // behaviour where pasted text arrived as `Char` events. Re-run the
            // incremental find so highlights/jump update, as typing does.
            if let Some(mut search) = s.search.take() {
                search.query.push_str(&normalize_pasted(text));
                search_recompute_after_edit(&mut s, &mut search);
                s.search = Some(search);
            }
        }
        InputTarget::None => return,
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
            // 2 content rows (issue #640 review): step/cost/elapsed on one
            // line, model/task on the next, so neither can crowd the other
            // off-screen. Must stay in sync with `draw`'s identical layout.
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);
    let log_chunk = chunks[1];
    let log_width = log_chunk.width.saturating_sub(2) as usize;
    let log_height = log_chunk.height.saturating_sub(2) as usize;
    let inner_feed_height = log_chunk.height.saturating_sub(2);

    let snapshot = {
        let mut s = dash
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.last_log_width = log_width;
        s.last_log_height = log_height;
        s.viewport_height = inner_feed_height;
        s.feed_top_row = log_chunk.y.saturating_add(1);
        // Clamp feed_scroll_top before the snapshot so this frame renders
        // the corrected offset. Skip while search is active to avoid undoing
        // scroll_to_line's positioning.
        if s.search.is_none() {
            if let Some(idx) = s.selected_index {
                if log_height > 0 {
                    if idx < s.feed_scroll_top {
                        s.feed_scroll_top = idx;
                    } else if idx >= s.feed_scroll_top + log_height {
                        s.feed_scroll_top = idx + 1 - log_height;
                    }
                }
            }
        }

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

        // Capture the help overlay's inner height here, while the state lock
        // is already held, so `draw_help_overlay` can stay a pure rendering
        // function that never touches `dash`/the mutex (issue #639 review).
        let help_overlay = centered_rect(80, 80, area);
        let help_block = Block::default().borders(Borders::ALL);
        s.help_viewport_height = help_block.inner(help_overlay).height;

        DashboardSnapshot {
            task: s.task.clone(),
            model: s.model.clone(),
            step: s.step,
            step_limit: s.step_limit,
            cost_usd: s.cost_usd,
            cost_cap_usd: s.cost_cap_usd,
            elapsed: s.started_at_instant.map(|since| since.elapsed()),
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
            search: s.search.clone(),
            stop_pending: s.stop_pending,
            rationale_scroll: s.rationale_scroll,
            activity: ActivitySnapshot::from_activity(&s.activity),
            stall_threshold: s.stall_threshold,
            help_open: s.help_open,
            help_scroll: s.help_scroll,
        }
    };
    terminal.draw(|frame| draw(frame, dash, &snapshot))?;
    Ok(())
}

#[allow(clippy::struct_excessive_bools)]
struct DashboardSnapshot {
    task: Option<String>,
    model: Option<String>,
    step: u32,
    step_limit: u32,
    cost_usd: f64,
    /// See [`DashboardState::cost_cap_usd`].
    cost_cap_usd: Option<f64>,
    /// Wall-clock elapsed since `RunStarted`, resolved from
    /// `DashboardState::started_at_instant` at snapshot time (issue #640).
    /// `None` before the run has started.
    elapsed: Option<Duration>,
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
    search: Option<SearchState>,
    stop_pending: bool,
    rationale_scroll: usize,
    activity: ActivitySnapshot,
    stall_threshold: Duration,
    help_open: bool,
    help_scroll: usize,
}

fn draw(frame: &mut ratatui::Frame, dash: &Arc<RatatuiDashboard>, snap: &DashboardSnapshot) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            // 2 content rows (issue #640 review): step/cost/elapsed on one
            // line, model/task on the next, so neither can crowd the other
            // off-screen. Must stay in sync with `draw`'s identical layout.
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    let inner_feed_height = chunks[1].height.saturating_sub(2);

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
        // Suppress the modal while a stop confirmation is in progress so
        // the visible key hints stay consistent with what handle_key does:
        // the footer shows "stop run? [y] yes  [n/Esc] cancel" and the
        // modal's "(y) approve" prompt must not contradict it (#638).
        if !snap.detail_open && !snap.stop_pending {
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

    // The help overlay (issue #639) renders last so it sits on top of the
    // detail pane and confirm modal alike — `handle_key` leaves whatever was
    // showing untouched underneath, so closing it returns to the same view.
    if snap.help_open {
        draw_help_overlay(frame, snap.help_scroll, area);
    }
}

/// Percent-of-cap consumed at or above which a budget indicator escalates to
/// [`budget_warning_style`] (issue #640, AC2).
const BUDGET_WARNING_PCT: f64 = 80.0;

/// Style for a budget indicator (cost or step) that has crossed
/// [`BUDGET_WARNING_PCT`]. Reuses the dashboard's existing danger color
/// (`Color::Red`, already used by `LineKind::BashErr` and the stop-run
/// confirmation) rather than `Color::Yellow`, since cost's normal color is
/// already yellow and wouldn't visibly change.
fn budget_warning_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

/// Format a `Duration` as a compact elapsed-time string: `"45s"`,
/// `"2m14s"`, or `"1h02m03s"`. Sub-minute values omit the minutes field
/// entirely rather than rendering `"0m45s"`.
fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
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

    let step_pct = if snap.step_limit > 0 {
        f64::from(snap.step) / f64::from(snap.step_limit) * 100.0
    } else {
        0.0
    };
    let step_style = if step_pct >= BUDGET_WARNING_PCT {
        budget_warning_style()
    } else {
        Style::default().fg(Color::Cyan)
    };

    // Cost burn-down (issue #640, AC1): when a cap is configured — including
    // a $0.00 "no spend allowed" kill switch, or (nonsensically but safely) a
    // negative one — show spend/cap/percent and escalate to a warning style
    // at >=80% consumed. A zero-or-negative cap can't drive the normal
    // spend/cap*100 division (it would divide by zero or go negative), so it
    // is treated as already fully consumed the moment any spend has
    // occurred. With no cap configured, fall back to the plain "cost $X"
    // form — no "/ $0" artifact and no division by zero.
    let cost_span = match snap.cost_cap_usd {
        Some(cap) => {
            let pct = if cap > 0.0 {
                (snap.cost_usd / cap * 100.0).max(0.0)
            } else if snap.cost_usd > 0.0 {
                100.0
            } else {
                0.0
            };
            let style = if pct >= BUDGET_WARNING_PCT {
                budget_warning_style()
            } else {
                Style::default().fg(Color::Yellow)
            };
            Span::styled(
                format!("cost ${:.4} / ${cap:.2} ({pct:.0}%)  ", snap.cost_usd),
                style,
            )
        }
        None => Span::styled(
            format!("cost ${:.4}  ", snap.cost_usd),
            Style::default().fg(Color::Yellow),
        ),
    };

    let mut top_spans = vec![
        Span::styled(
            format!("step {}/{}  ", snap.step, snap.step_limit),
            step_style,
        ),
        cost_span,
    ];
    // Live elapsed wall-clock since the run started (issue #640, AC3); absent
    // until the first `RunStarted` event lands.
    if let Some(elapsed) = snap.elapsed {
        top_spans.push(Span::styled(
            format!("elapsed {}", format_elapsed(elapsed)),
            Style::default().fg(Color::Gray),
        ));
    }
    // Model/task get their own line (issue #640 review): the burn-down and
    // elapsed spans above can run long (a configured cap plus an hours-long
    // elapsed time), and packing everything onto one row risked silently
    // clipping the model name and task title — the fields an operator most
    // needs to identify the run — off the edge of a normal-width terminal.
    let bottom_spans = vec![
        Span::styled(
            format!("model: {model}  "),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("task: {title}"),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ];
    let lines = vec![Line::from(top_spans), Line::from(bottom_spans)];
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
    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(block_title))
}

fn line_base_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Info => Style::default().fg(Color::Gray),
        LineKind::AssistantMsg => Style::default().fg(Color::Cyan),
        LineKind::BashRun => Style::default().fg(Color::White),
        LineKind::BashOk => Style::default().fg(Color::Green),
        LineKind::BashErr => Style::default().fg(Color::Red),
        LineKind::Observation => Style::default().fg(Color::LightBlue),
        LineKind::Warn => Style::default().fg(Color::LightYellow),
    }
}

fn log_paragraph_monitor<'a>(snap: &'a DashboardSnapshot, window: &[&'a LogLine]) -> Paragraph<'a> {
    let query = snap.search.as_ref().map_or("", |s| s.query.as_str());
    let matches = search_matches(window, query);
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
            let base = line_base_style(l.kind);
            let style = if current_line == Some(i) {
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
        .scroll((scroll_y_u16, 0))
        .wrap(Wrap { trim: false })
}

fn log_paragraph(snap: &DashboardSnapshot, visible_lines: usize) -> Paragraph<'_> {
    // Both branches operate on the last MAX_LOG_LINES entries of the log.
    let max_lines = snap.log.len().min(MAX_LOG_LINES);
    let take_from = snap.log.len().saturating_sub(max_lines);
    let window: Vec<&LogLine> = snap.log[take_from..].iter().collect();

    if snap.is_monitor {
        return log_paragraph_monitor(snap, &window);
    }
    {
        // Interactive mode: slice by feed_scroll_top for rendering, but compute
        // search matches over the same full window that handle_key_search uses
        // so that match indices stay consistent and scroll_to_line can bring
        // off-screen matches into view.
        let query = snap.search.as_ref().map_or("", |s| s.query.as_str());
        let all_matches = search_matches(&window, query);
        let current_window_match = snap.search.as_ref().and_then(|s| {
            all_matches
                .get(s.current.min(all_matches.len().saturating_sub(1)))
                .copied()
        });
        let match_set: HashSet<usize> = all_matches.iter().copied().collect();

        // Visible slice for rendering (entry-based viewport).
        let items: Vec<_> = if visible_lines > 0 {
            snap.log
                .iter()
                .skip(snap.feed_scroll_top)
                .take(visible_lines)
                .collect()
        } else {
            snap.log.iter().skip(snap.feed_scroll_top).collect()
        };

        let lines: Vec<Line> = items
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let abs_idx = snap.feed_scroll_top + i;
                let is_selected = Some(abs_idx) == snap.selected_index;
                // window_idx is the index into window (same frame as matches).
                let window_idx = abs_idx.saturating_sub(take_from);
                let base = line_base_style(l.kind);
                let style = if current_window_match == Some(window_idx) {
                    Style::default()
                        .bg(Color::Yellow)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else if match_set.contains(&window_idx) {
                    base.bg(Color::DarkGray)
                } else if is_selected {
                    base.add_modifier(Modifier::REVERSED)
                } else {
                    base
                };
                Line::from(Span::styled(l.text.clone(), style))
            })
            .collect();

        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" trajectory "))
            .scroll((0, 0))
    }
}

/// One entry in the `?` help overlay (issue #639). This is the single source
/// of truth for both what is rendered (`category`/`keys`/`description`) and
/// what an automated test checks is documented (`matches`, the literal
/// keystrokes this entry covers) — see
/// `every_dispatched_keystroke_is_documented_in_help_overlay`, the drift
/// guard the issue requires: a new binding added to `handle_key` must also
/// be added here, or that test fails.
struct KeyBinding {
    category: &'static str,
    keys: &'static str,
    description: &'static str,
    /// Read only by the completeness test below; absent from non-test
    /// builds' code paths since rendering only needs `category`/`keys`/
    /// `description`.
    #[allow(dead_code)]
    matches: &'static [(KeyCode, KeyModifiers)],
}

const KEYBINDINGS: &[KeyBinding] = &[
    KeyBinding {
        category: "Navigation",
        keys: "Up/k  Down/j",
        description: "move the selection, or scroll the feed",
        matches: &[
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::Down, KeyModifiers::NONE),
            (KeyCode::Char('k'), KeyModifiers::NONE),
            (KeyCode::Char('K'), KeyModifiers::NONE),
            (KeyCode::Char('j'), KeyModifiers::NONE),
            (KeyCode::Char('J'), KeyModifiers::NONE),
        ],
    },
    KeyBinding {
        category: "Navigation",
        keys: "PgUp / PgDn",
        description: "scroll a page at a time",
        matches: &[
            (KeyCode::PageUp, KeyModifiers::NONE),
            (KeyCode::PageDown, KeyModifiers::NONE),
        ],
    },
    KeyBinding {
        category: "Navigation",
        keys: "Home / End",
        description: "jump to the oldest / latest entry",
        matches: &[
            (KeyCode::Home, KeyModifiers::NONE),
            (KeyCode::End, KeyModifiers::NONE),
        ],
    },
    KeyBinding {
        category: "Navigation",
        keys: "Enter",
        description: "open the detail inspector for the selected entry",
        matches: &[(KeyCode::Enter, KeyModifiers::NONE)],
    },
    KeyBinding {
        category: "Navigation",
        keys: "/",
        description: "start an incremental search of the trajectory feed",
        matches: &[(KeyCode::Char('/'), KeyModifiers::NONE)],
    },
    KeyBinding {
        category: "Navigation",
        keys: "n / N",
        description: "jump to the next / previous search match",
        matches: &[
            (KeyCode::Char('n'), KeyModifiers::NONE),
            (KeyCode::Char('N'), KeyModifiers::NONE),
        ],
    },
    KeyBinding {
        category: "Confirm",
        keys: "y / Y",
        description: "approve the proposed command",
        matches: &[
            (KeyCode::Char('y'), KeyModifiers::NONE),
            (KeyCode::Char('Y'), KeyModifiers::NONE),
        ],
    },
    KeyBinding {
        category: "Confirm",
        keys: "n / N",
        description: "reject, with an optional feedback note",
        matches: &[
            (KeyCode::Char('n'), KeyModifiers::NONE),
            (KeyCode::Char('N'), KeyModifiers::NONE),
        ],
    },
    KeyBinding {
        category: "Confirm",
        keys: "e / E",
        description: "edit the proposed command, then run it",
        matches: &[
            (KeyCode::Char('e'), KeyModifiers::NONE),
            (KeyCode::Char('E'), KeyModifiers::NONE),
        ],
    },
    KeyBinding {
        category: "Confirm",
        keys: "A",
        description: "auto-approve this scope for the rest of the run",
        matches: &[(KeyCode::Char('A'), KeyModifiers::NONE)],
    },
    KeyBinding {
        category: "Confirm",
        keys: "a",
        description: "abort the run",
        matches: &[(KeyCode::Char('a'), KeyModifiers::NONE)],
    },
    KeyBinding {
        category: "Run control",
        keys: "Ctrl-Q",
        description: "stop the run (asks for y/N confirmation)",
        matches: &[
            (KeyCode::Char('q'), KeyModifiers::CONTROL),
            (KeyCode::Char('Q'), KeyModifiers::CONTROL),
        ],
    },
    KeyBinding {
        category: "Run control",
        keys: "Ctrl-C",
        description: "abort immediately",
        matches: &[(KeyCode::Char('c'), KeyModifiers::CONTROL)],
    },
    KeyBinding {
        category: "Run control",
        keys: "Esc",
        description: "cancel the active prompt, search, or pane (aborts when nothing else is open)",
        matches: &[(KeyCode::Esc, KeyModifiers::NONE)],
    },
    KeyBinding {
        category: "Run control",
        keys: "q / Q",
        description: "close the detail pane, or close the dashboard once the run has finished",
        matches: &[
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('Q'), KeyModifiers::NONE),
        ],
    },
    KeyBinding {
        category: "Text input",
        keys: "Backspace",
        description: "delete the last character while typing feedback, an edit, or a search query",
        matches: &[(KeyCode::Backspace, KeyModifiers::NONE)],
    },
    KeyBinding {
        category: "View",
        keys: "?",
        description: "toggle this help overlay",
        matches: &[(KeyCode::Char('?'), KeyModifiers::NONE)],
    },
    // Mouse bindings (issue #734) have no `KeyCode`/`KeyModifiers` to match —
    // `matches` stays empty; they're documented here purely so the help
    // overlay and README stay the single source of truth for both input
    // modes, and to satisfy `help_overlay_lines_cover_every_keybindings_entry`.
    KeyBinding {
        category: "Mouse",
        keys: "Wheel",
        description: "scroll the feed or detail inspector (same as Up/Down)",
        matches: &[],
    },
    KeyBinding {
        category: "Mouse",
        keys: "Click",
        description: "select a step row in the feed",
        matches: &[],
    },
    KeyBinding {
        category: "Mouse",
        keys: "Shift-drag",
        description: "select text with the terminal's native copy, bypassing mouse capture",
        matches: &[],
    },
];

/// Render the `?` help overlay's body content (issue #639), grouped by
/// category with a blank separator line between groups. Pure and
/// terminal-size independent, so its length can be used to compute scroll
/// bounds without a live render.
fn help_overlay_lines() -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut last_category: Option<&str> = None;
    for binding in KEYBINDINGS {
        if last_category != Some(binding.category) {
            if last_category.is_some() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                binding.category,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            last_category = Some(binding.category);
        }
        lines.push(Line::from(format!(
            "  {:<14} {}",
            binding.keys, binding.description
        )));
    }
    lines
}

/// The line count `help_overlay_lines()` would produce, without allocating a
/// `Vec` or formatting any strings. `handle_key` calls this on every
/// keystroke while the overlay is open (including swallowed ones) to clamp
/// scrolling, so it must stay allocation-free (issue #639 review).
fn help_overlay_lines_count() -> usize {
    let mut count = 0;
    let mut last_category: Option<&str> = None;
    for binding in KEYBINDINGS {
        if last_category != Some(binding.category) {
            if last_category.is_some() {
                count += 1;
            }
            count += 1;
            last_category = Some(binding.category);
        }
        count += 1;
    }
    count
}

/// Render the `?` help overlay (issue #639) on top of everything else,
/// scrolling its content if it overflows the available height (AC5). Never
/// reads or mutates `pending`/`feedback_input`/`edit_input`/run state —
/// `handle_key` gives it absolute priority before any of that is touched.
/// Pure: `draw_frame` captures `help_viewport_height` into state itself
/// (while it already holds the lock), so this never needs to lock the
/// mutex from inside the render callback (issue #639 review).
fn draw_help_overlay(frame: &mut ratatui::Frame, scroll: usize, area: Rect) {
    let overlay = centered_rect(80, 80, area);
    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help — keybindings (? or Esc to close) ");
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let lines = help_overlay_lines();
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(scroll)
        .take(inner.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn footer_paragraph(snap: &DashboardSnapshot) -> Paragraph<'_> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    // stop_pending takes top priority so the confirmation prompt is always
    // visible even when the detail inspector is open (detail_open). The
    // remaining branches follow: search, finished, edit, pending, activity.
    let span = if snap.stop_pending {
        Span::styled("stop run? [y] yes   [n/Esc] cancel", bold.fg(Color::Red))
    } else if snap.detail_open {
        Span::styled(
            "[Esc/q] close   [Up/Down/j/k] scroll   [PgUp/PgDn] page   [Home/End] bounds",
            bold,
        )
    } else if let Some(search) = &snap.search {
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
            format!(
                "(y) approve   (n) reject   (e) edit   (a) abort   (A) auto-approve {scope}   [Up/Down] navigate   [Enter] inspect"
            ),
            bold,
        )
    } else {
        // No modal, no search, not finished: surface the in-flight activity so
        // the operator can tell working from hung (issue #649).
        match &snap.activity {
            ActivitySnapshot::Thinking { elapsed } => Span::styled(
                format!(
                    "{} thinking… (model · {}s)   [Ctrl-Q] stop",
                    spinner_frame(*elapsed),
                    elapsed.as_secs()
                ),
                activity_style(*elapsed, snap.stall_threshold),
            ),
            ActivitySnapshot::Running { elapsed, command } => Span::styled(
                format!(
                    "{} running: {command} ({}s)   [Ctrl-Q] stop",
                    spinner_frame(*elapsed),
                    elapsed.as_secs()
                ),
                activity_style(*elapsed, snap.stall_threshold),
            ),
            ActivitySnapshot::Idle => Span::styled(
                "waiting for next agent step…  [↑/↓] navigate   [Enter] inspect   [/ search]   [Ctrl-Q] stop",
                bold,
            ),
        }
    };

    // Stable `?: help` affordance (issue #639, AC6) so operators can discover
    // the overlay. Shown only while `?` actually toggles it — i.e. not while
    // it would instead be swallowed (stop confirmation) or typed as a literal
    // character (rejection feedback, an edited command, or an in-progress
    // search query). Mirrors the precedence `handle_key` uses to decide
    // whether `?` opens the overlay, including that a pending confirm prompt
    // shadows an in-progress search query (the prompt owns input, not the
    // search box) so `search.editing` alone must not suppress the hint then.
    let search_editing_visible =
        snap.pending.is_none() && snap.search.as_ref().is_some_and(|s| s.editing);
    let show_help_hint = !snap.stop_pending
        && snap.edit_input.is_none()
        && snap.feedback_input.is_none()
        && !search_editing_visible;
    // Appended, not prepended (issue #639 review follow-up): `footer_paragraph`'s
    // `Paragraph` is not wrapped, and several branches — idle, `finished`, and a
    // filled-in `pending` scope — are already wider than an 80-column terminal's
    // inner width (78 cols) on their own, independent of this hint. Prepending
    // was tried and reverted: it shifts *later* content out of view instead
    // (verified by test — it broke 3 previously-passing footer assertions for
    // exactly that reason), so it only relocates the pre-existing overflow
    // rather than fixing it. Appending keeps that pre-existing limitation
    // unchanged and adds the hint where the branch already had room (e.g.
    // `stop_pending`/`edit_input`-free short states); it does not fully
    // guarantee visibility in every state at 80 columns. Properly fixing that
    // needs either shorter footer copy across several branches or a taller/
    // wrapped footer — both larger, more disruptive changes than a review-
    // response fix, and better scoped as a separate follow-up.
    let line = if show_help_hint {
        Line::from(vec![span, Span::styled("   [?: help]", bold)])
    } else {
        Line::from(span)
    };
    Paragraph::new(line).block(Block::default().borders(Borders::ALL))
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
    let total = buf_lines.len();
    let last = total - 1;
    // Render only the tail (last MODAL_INPUT_MAX_LINES logical lines) so a large
    // multi-line paste can't push the command/rationale/submit hint off-screen
    // (issue #745 review). The caret rides the last line, so the tail is what the
    // operator is editing; the full value is still stored and submitted.
    let start = total.saturating_sub(MODAL_INPUT_MAX_LINES);
    let mut lines = Vec::with_capacity(total - start + usize::from(start > 0));
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  … {start} earlier line(s) hidden"),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    for (i, line) in buf_lines.iter().enumerate().skip(start) {
        // The `>` prompt marks the buffer start; once the head is truncated the
        // leading indicator stands in for it, so shown lines use plain indent.
        let prefix = if i == 0 { " > " } else { "   " };
        let mut spans = vec![
            Span::styled(prefix, Style::default().fg(Color::Green)),
            Span::styled((*line).to_string(), Style::default().fg(Color::White)),
        ];
        if i == last {
            spans.push(Span::styled("█", Style::default().fg(Color::Green)));
        }
        lines.push(Line::from(spans));
    }
    lines
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
        .map(|line| count_wrapped_lines(line.text.as_str(), width))
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
            full_text: None,
            kind: LineKind::Info,
            text: text.to_string(),
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

    /// `renderer_loop` uses this counter (rather than trusting whichever
    /// `select!` branch happens to win) to detect "did state change since my
    /// last draw," closing the mouse-motion redraw-skip's lost-notification
    /// race regardless of timing (issue #734 review).
    #[test]
    fn redraw_notify_version_increments_on_notify_waiters() {
        let n = RedrawNotify::new();
        assert_eq!(n.version(), 0);
        n.notify_waiters();
        assert_eq!(n.version(), 1);
        n.notify_waiters();
        n.notify_waiters();
        assert_eq!(n.version(), 3);
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
    #[allow(clippy::significant_drop_tightening)]
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
    #[allow(clippy::significant_drop_tightening)]
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
    #[allow(clippy::significant_drop_tightening)]
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
    #[allow(clippy::significant_drop_tightening)]
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
            notify: RedrawNotify::new(),
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
            notify: RedrawNotify::new(),
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
            cost_cap_usd: s.cost_cap_usd,
            elapsed: s.started_at_instant.map(|since| since.elapsed()),
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
            search: s.search.clone(),
            stop_pending: s.stop_pending,
            rationale_scroll: s.rationale_scroll,
            activity: ActivitySnapshot::from_activity(&s.activity),
            stall_threshold: s.stall_threshold,
            help_open: s.help_open,
            help_scroll: s.help_scroll,
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

    // ---- budget burn-down and elapsed clock (issue #640) ----

    /// True if any cell in the top 4 rows (the header, now 2 content rows
    /// plus borders — issue #640 review) carries `color` as its foreground —
    /// mirrors `footer_has_fg` below for header assertions.
    fn header_has_fg(buf: &Buffer, color: Color) -> bool {
        for y in 0..buf.area.height.min(4) {
            for x in 0..buf.area.width {
                if buf[(x, y)].style().fg == Some(color) {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn format_elapsed_formats_seconds_minutes_and_hours() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(134)), "2m14s");
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "1h01m01s");
    }

    #[test]
    fn effective_cost_cap_prefers_the_tighter_configured_cap() {
        assert_eq!(effective_cost_cap_usd(None, None), None);
        assert_eq!(effective_cost_cap_usd(Some(1.0), None), Some(1.0));
        assert_eq!(effective_cost_cap_usd(None, Some(0.5)), Some(0.5));
        // Both configured: the smaller one is what actually fires first.
        assert_eq!(effective_cost_cap_usd(Some(1.0), Some(0.5)), Some(0.5));
        assert_eq!(effective_cost_cap_usd(Some(0.5), Some(1.0)), Some(0.5));
    }

    #[test]
    fn header_shows_cost_burn_down_when_cap_configured() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 0.042;
            s.cost_cap_usd = Some(0.50);
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("cost $0.0420 / $0.50 (8%)"),
            "burn-down should show spend, cap, and percent; got:\n{text}"
        );
    }

    #[test]
    fn header_cost_indicator_warns_at_80_percent_consumed() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 0.42;
            s.cost_cap_usd = Some(0.50); // 84% consumed
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        assert!(
            header_has_fg(&buf, Color::Red),
            "cost indicator should escalate to warning style at >=80% consumed"
        );
    }

    #[test]
    fn header_cost_indicator_stays_normal_style_below_warning_threshold() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 0.042;
            s.cost_cap_usd = Some(0.50); // 8% consumed
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        assert!(
            !header_has_fg(&buf, Color::Red),
            "cost indicator should not warn well below the threshold"
        );
    }

    #[test]
    fn header_step_indicator_warns_at_80_percent_of_step_limit() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.step = 8;
            s.step_limit = 10; // 80% of step limit consumed
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        assert!(
            header_has_fg(&buf, Color::Red),
            "step indicator should escalate to warning style at >=80% of step_limit"
        );
    }

    #[test]
    fn header_falls_back_to_raw_cost_when_no_cap_configured() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 0.042;
            s.cost_cap_usd = None;
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("cost $0.0420"),
            "should fall back to the raw cost form; got:\n{text}"
        );
        assert!(
            !text.contains("/ $0"),
            "must not render a cap/percent artifact with no cap configured; got:\n{text}"
        );
    }

    #[test]
    fn header_shows_burn_down_for_a_zero_cost_cap_kill_switch() {
        // A $0.00 cap (e.g. `cost_limit_usd = 0.0`) is a legal "no spend
        // allowed" config, not "no cap configured" — code-review fix: the
        // old `cap > 0.0` guard silently hid it behind the plain fallback
        // right as the run was about to terminate on it.
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 0.01;
            s.cost_cap_usd = Some(0.0);
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("cost $0.0100 / $0.00 (100%)"),
            "a $0 cap with any spend should show as fully consumed; got:\n{text}"
        );
        assert!(
            header_has_fg(&buf, Color::Red),
            "a $0 cap with any spend should escalate to the warning style; got:\n{text}"
        );
    }

    #[test]
    fn header_shows_zero_percent_for_a_zero_cost_cap_before_any_spend() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 0.0;
            s.cost_cap_usd = Some(0.0);
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("cost $0.0000 / $0.00 (0%)"),
            "a $0 cap with no spend yet should show 0%, not crash/NaN/inf; got:\n{text}"
        );
    }

    #[test]
    fn header_shows_burn_down_for_a_negative_cost_cap_without_crashing() {
        // Negative caps are nonsensical but not rejected by config
        // validation; the header must degrade gracefully rather than
        // divide by a negative number or format NaN/inf.
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 0.01;
            s.cost_cap_usd = Some(-1.0);
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            !text.contains("NaN") && !text.contains("inf"),
            "must not render NaN/inf for a negative cap; got:\n{text}"
        );
    }

    #[test]
    fn header_keeps_model_and_task_visible_alongside_a_full_burn_down_and_elapsed_clock() {
        // Code-review fix: packing cost-cap/percent and elapsed onto the
        // same line as model/task could silently clip the latter off an
        // 80-column terminal. Model/task now get their own header line.
        let d = make_dashboard();
        d.emit(StreamEvent::RunStarted {
            task: "fix the flaky retry test".into(),
            model: "claude-opus-4-7".into(),
            started_at: "2026-05-18T12:00:00Z".into(),
        });
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 12.3456;
            s.cost_cap_usd = Some(15.0);
            s.started_at_instant = Some(ago(3 * 3600 + 45 * 60 + 12)); // 3h45m12s
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("cost $12.3456 / $15.00"),
            "burn-down should still render; got:\n{text}"
        );
        assert!(
            text.contains("elapsed 3h45m12s"),
            "elapsed should still render; got:\n{text}"
        );
        assert!(
            text.contains("claude-opus-4-7"),
            "model must not be clipped off-screen; got:\n{text}"
        );
        assert!(
            text.contains("fix the flaky retry test"),
            "task title must not be clipped off-screen; got:\n{text}"
        );
    }

    #[test]
    fn header_shows_elapsed_wall_clock_since_run_started() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.started_at_instant = Some(ago(134));
        }
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("elapsed 2m14s"),
            "header should show live elapsed wall-clock; got:\n{text}"
        );
    }

    #[test]
    fn header_omits_elapsed_before_run_started() {
        let d = make_dashboard();
        // Default state: no RunStarted event has landed yet.
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            !text.contains("elapsed"),
            "elapsed should not render before the run has started; got:\n{text}"
        );
    }

    #[test]
    fn emit_run_started_captures_started_at_instant() {
        let d = make_dashboard();
        d.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "2026-05-18T12:00:00Z".into(),
        });
        let s = snap(&d);
        assert!(
            s.elapsed.is_some(),
            "RunStarted should populate the elapsed anchor"
        );
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
    fn emit_assistant_message_accumulates_per_turn_cost_into_cumulative_spend() {
        // Regression for PR #992 review: `cost_usd` on `AssistantMessage` is
        // this response's own cost, not a running total (see
        // `DefaultAgent::step`'s `resp.usage.cost_usd`). Two $0.04 turns with
        // no intervening confirm prompt (e.g. auto-approved bash) must read
        // as $0.08 cumulative, not silently regress to the last turn's $0.04.
        let d = make_dashboard();
        d.emit(StreamEvent::AssistantMessage {
            step: 1,
            content: "x".into(),
            cost_usd: Some(0.04),
            timestamp: "t".into(),
        });
        d.emit(StreamEvent::AssistantMessage {
            step: 2,
            content: "y".into(),
            cost_usd: Some(0.04),
            timestamp: "t".into(),
        });
        let s = snap(&d);
        assert!(
            (s.cost_usd - 0.08).abs() < f64::EPSILON,
            "expected cumulative $0.08 after two $0.04 turns, got {}",
            s.cost_usd
        );
    }

    #[test]
    fn header_burn_down_reflects_cumulative_spend_across_turns_without_a_confirm_prompt() {
        // End-to-end version of the above through the actual header render:
        // two $0.04 assistant turns against a $0.10 cap, with no confirm
        // prompt in between, must show 80% (and the warning color it
        // crosses at), not 40%.
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_cap_usd = Some(0.10);
        }
        d.emit(StreamEvent::AssistantMessage {
            step: 1,
            content: "x".into(),
            cost_usd: Some(0.04),
            timestamp: "t".into(),
        });
        d.emit(StreamEvent::AssistantMessage {
            step: 2,
            content: "y".into(),
            cost_usd: Some(0.04),
            timestamp: "t".into(),
        });
        let buf = render_to_buffer(&snap(&d), 80, 12);
        let text = buffer_text(&buf);
        assert!(
            text.contains("cost $0.0800 / $0.10 (80%)"),
            "should show cumulative 80% consumed, not 40%; got:\n{text}"
        );
        assert!(
            header_has_fg(&buf, Color::Red),
            "80% consumed should already be at the warning threshold; got:\n{text}"
        );
    }

    #[test]
    fn resumed_prior_spend_seeded_at_construction_accumulates_with_new_turns() {
        // Regression for PR #992 review: `mini --resume`/`--continue` emits
        // `RunStarted` before folding `ResumeState.total_cost_usd` in, so no
        // stream event carries prior spend — `RatatuiDashboard::start` now
        // seeds `DashboardState.cost_usd` directly (simulated here since
        // `start()` needs a real terminal). Confirms it composes correctly
        // with the accumulate-not-overwrite fix above: new turns must add to
        // the resumed total, not replace it.
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.cost_usd = 0.30; // simulates a resumed run's prior spend
            s.cost_cap_usd = Some(0.50);
        }
        d.emit(StreamEvent::AssistantMessage {
            step: 1,
            content: "x".into(),
            cost_usd: Some(0.04),
            timestamp: "t".into(),
        });
        let s = snap(&d);
        assert!(
            (s.cost_usd - 0.34).abs() < f64::EPSILON,
            "expected resumed $0.30 + new $0.04 = $0.34, got {}",
            s.cost_usd
        );
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
                full_text: None,
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
                full_text: None,
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
                    full_text: None,
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
                full_text: None,
                kind: LineKind::BashErr,
                text: "UNIQUETOKEN the failing assertion".into(),
            });
            for i in 1..=20 {
                s.log.push_back(LogLine {
                    full_text: None,
                    kind: LineKind::Info,
                    text: format!("filler line number {i}"),
                });
            }
            // Simulate auto-follow: position feed_scroll_top so the bottom
            // entries are visible (render_to_buffer height=11 gives inner_feed
            // height=3 after borders and chrome; 21 entries – 3 visible = 18).
            s.feed_scroll_top = s.log.len().saturating_sub(3);
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
    fn paste_large_multiline_caps_display_but_keeps_submit_hint_and_full_buffer() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

        // Paste far more lines than the modal can show — the feature's own use
        // case (e.g. a 20-line stack trace fed back as guidance).
        let mut pasted = String::new();
        for i in 1..=20 {
            use std::fmt::Write as _;
            let _ = writeln!(pasted, "trace line {i}");
        }
        handle_paste(&d, &pasted);

        // Full content is retained in the buffer — only the display is capped.
        let buf = snap(&d).feedback_input.unwrap();
        assert!(buf.contains("trace line 1\n"));
        assert!(buf.contains("trace line 20"));

        // The rendered modal still shows the submit hint (not clipped by a
        // collapsed layout) and signals the truncation. At 80x40 the modal's
        // inner height (~18) clips a full 20-line paste's controls pre-cap but
        // fits the capped controls — so this asserts the fix, not the terminal.
        let rendered = buffer_text(&render_to_buffer(&snap(&d), 80, 40));
        assert!(
            rendered.contains("[Enter] submit"),
            "submit hint must stay visible: {rendered}"
        );
        assert!(
            rendered.contains("earlier line(s) hidden"),
            "truncation indicator must be shown: {rendered}"
        );
        // The tail (where the caret is) is visible; the head is hidden.
        assert!(rendered.contains("trace line 20"));
        assert!(!rendered.contains("trace line 1 "));
    }

    #[test]
    fn paste_large_multiline_into_edit_caps_display() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));

        let mut pasted = String::new();
        for i in 1..=20 {
            use std::fmt::Write as _;
            let _ = writeln!(pasted, "cmd line {i}");
        }
        handle_paste(&d, &pasted);

        // Full content retained (edit buffer was pre-filled with "x").
        assert!(snap(&d).edit_input.unwrap().contains("cmd line 20"));

        let rendered = buffer_text(&render_to_buffer(&snap(&d), 80, 40));
        assert!(
            rendered.contains("[Enter] execute edit"),
            "edit submit hint must stay visible: {rendered}"
        );
        assert!(rendered.contains("earlier line(s) hidden"));
        assert!(rendered.contains("cmd line 20"));
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
            notify: RedrawNotify::new(),
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
                    full_text: None,
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
            rationale: String::new(),
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
                    rationale: String::new(),
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

        // total_wrapped_lines counts rendered rows (ceil(len/width)), which is
        // 5 for a 50-char line at width 10. The main feed uses entry-based
        // feed_scroll_top (not wrapped rows), so this function is only used
        // for monitor-mode scroll math.
        let s = snap(&d);
        let total = total_wrapped_lines(&s.log, 10);
        assert_eq!(total, 5);
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

    #[test]
    fn test_monitor_scrollback_stable_on_append() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.is_monitor = true;
            s.viewport_height = 3;
        }

        // Append 5 items
        for i in 1..=5 {
            d.append(LineKind::Info, format!("line{i}"), None);
        }

        // Since auto_follow is true initially, feed_scroll_top should advance to 5 - 3 = 2
        assert_eq!(snap(&d).feed_scroll_top, 2);

        // Set auto_follow to false (scrolled away)
        {
            let mut s = d.state.lock().unwrap();
            s.auto_follow = false;
        }

        // Append line 6
        d.append(LineKind::Info, "line6", None);

        // Since auto_follow is false, feed_scroll_top should remain stable at 2 (not advance to 3)
        assert_eq!(snap(&d).feed_scroll_top, 2);
    }

    // ── Global stop key (#638) ─────────────────────────────────────────────

    fn ctrl_q() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
    }

    fn make_dashboard_with_cancel() -> (Arc<RatatuiDashboard>, tokio::sync::watch::Receiver<bool>) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let d = Arc::new(RatatuiDashboard {
            state: Mutex::new(DashboardState::default()),
            notify: RedrawNotify::new(),
            cancel_tx: Some(tx),
            bell: Bell::silent(),
        });
        (d, rx)
    }

    #[test]
    fn stop_key_sets_stop_pending_no_modal() {
        let d = make_dashboard();
        assert!(!snap(&d).stop_pending, "stop_pending starts false");
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending, "Ctrl-Q must set stop_pending");
    }

    #[test]
    fn stop_key_works_in_monitor_mode() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.is_monitor = true;
        }
        handle_key(&d, ctrl_q());
        assert!(
            snap(&d).stop_pending,
            "Ctrl-Q must work in monitor/yolo mode"
        );
    }

    #[test]
    fn stop_key_no_op_when_finished() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.finished = Some("done".into());
        }
        handle_key(&d, ctrl_q());
        assert!(!snap(&d).stop_pending, "Ctrl-Q is no-op after run finished");
    }

    #[test]
    fn stop_pending_n_cancels_stop() {
        let d = make_dashboard();
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending);
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(!snap(&d).stop_pending, "'n' must clear stop_pending");
    }

    #[test]
    fn stop_pending_esc_cancels_stop() {
        let d = make_dashboard();
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending);
        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!snap(&d).stop_pending, "Esc must clear stop_pending");
    }

    #[test]
    fn stop_pending_y_fires_cancel_tx() {
        let (d, rx) = make_dashboard_with_cancel();
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending);
        assert!(!*rx.borrow(), "cancel not yet signalled");
        handle_key(&d, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!snap(&d).stop_pending, "stop_pending cleared after confirm");
        assert!(*rx.borrow(), "'y' must fire cancel_tx");
    }

    #[test]
    fn stop_pending_y_aborts_pending_modal() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending);
        handle_key(&d, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!snap(&d).stop_pending);
        assert!(snap(&d).pending.is_none(), "modal must be cleared on abort");
        assert_eq!(
            rx.try_recv().unwrap(),
            ConfirmDecision::Abort,
            "'y' while modal pending must send Abort"
        );
    }

    #[test]
    fn stop_pending_swallows_other_keys() {
        let (d, rx) = make_dashboard_with_cancel();
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending);
        // Any key other than y/n/Esc should NOT trigger cancel or clear stop_pending
        handle_key(&d, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            snap(&d).stop_pending,
            "other keys must leave stop_pending set"
        );
        assert!(!*rx.borrow(), "other keys must not fire cancel");
    }

    #[test]
    fn single_ctrl_q_does_not_abort() {
        let (d, rx) = make_dashboard_with_cancel();
        handle_key(&d, ctrl_q());
        // After one Ctrl-Q, cancel must NOT have fired yet (confirmation step)
        assert!(
            !*rx.borrow(),
            "single Ctrl-Q must not abort without confirmation"
        );
    }

    #[test]
    fn footer_shows_stop_gesture_when_thinking() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.activity = Activity::Thinking {
                since: std::time::Instant::now(),
            };
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 6);
        let text = buffer_text(&buf);
        assert!(
            text.contains("Ctrl-Q"),
            "footer must advertise Ctrl-Q stop when thinking; got:\n{text}"
        );
    }

    #[test]
    fn footer_shows_stop_gesture_when_idle() {
        let d = make_dashboard();
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 6);
        let text = buffer_text(&buf);
        assert!(
            text.contains("Ctrl-Q"),
            "footer must advertise Ctrl-Q stop when idle; got:\n{text}"
        );
    }

    #[test]
    fn footer_shows_stop_gesture_when_running() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.activity = Activity::Running {
                since: std::time::Instant::now(),
                command: "ls".into(),
            };
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 6);
        let text = buffer_text(&buf);
        assert!(
            text.contains("Ctrl-Q"),
            "footer must advertise Ctrl-Q stop when running; got:\n{text}"
        );
    }

    #[test]
    fn footer_shows_stop_confirmation_when_pending() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.stop_pending = true;
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 6);
        let text = buffer_text(&buf);
        assert!(
            text.contains("[y]") || text.contains("yes"),
            "footer must show stop confirmation prompt; got:\n{text}"
        );
        assert!(
            text.contains("[n]") || text.contains("cancel"),
            "footer must show cancel option in stop prompt; got:\n{text}"
        );
    }

    #[test]
    fn footer_does_not_show_stop_gesture_after_finished() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.finished = Some("done".into());
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 6);
        let text = buffer_text(&buf);
        // finished state uses its own message; stop hint should not appear there
        assert!(
            text.contains("run complete"),
            "finished footer should show run-complete message; got:\n{text}"
        );
    }

    #[test]
    fn stop_pending_ctrl_c_immediately_aborts() {
        let (d, rx) = make_dashboard_with_cancel();
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending);
        handle_key(&d, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!snap(&d).stop_pending, "Ctrl-C must clear stop_pending");
        assert!(*rx.borrow(), "Ctrl-C must fire cancel_tx immediately");
    }

    #[test]
    fn footer_shows_stop_confirmation_even_when_detail_open() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.stop_pending = true;
            s.detail_open = true;
        }
        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 6);
        let text = buffer_text(&buf);
        assert!(
            text.contains("[y]") || text.contains("yes"),
            "stop confirmation must show even with detail_open; got:\n{text}"
        );
        assert!(
            !text.contains("[Esc/q] close"),
            "detail hint must not override stop_pending in footer; got:\n{text}"
        );
    }

    #[test]
    fn modal_hidden_while_stop_pending() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        // Simulate the user pressing Ctrl-Q while a confirm modal is open.
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending, "Ctrl-Q sets stop_pending");
        assert!(
            snap(&d).pending.is_some(),
            "modal context is still retained"
        );

        let s = snap(&d);
        let buf = render_to_buffer(&s, 100, 24);
        let text = buffer_text(&buf);
        // The approval modal must not be visible while stop is pending.
        assert!(
            !text.contains("confirm action"),
            "modal must be hidden while stop_pending; got:\n{text}"
        );
        assert!(
            !text.contains("approve"),
            "approve prompt must not show while stop_pending; got:\n{text}"
        );
        // The stop confirmation must be visible instead.
        assert!(
            text.contains("stop run"),
            "stop confirmation must be visible; got:\n{text}"
        );
    }

    /// If the run ends naturally while stop_pending is true (race condition),
    /// the next keypress must auto-clear stop_pending so the finished-close
    /// path (q/Esc → should_exit) becomes reachable again.
    #[test]
    fn stop_pending_clears_when_run_finishes() {
        let d = make_dashboard();
        // Activate the stop confirmation.
        handle_key(&d, ctrl_q());
        assert!(
            snap(&d).stop_pending,
            "stop_pending must be set after Ctrl-Q"
        );

        // Simulate RunEnded arriving while the operator hasn't answered yet.
        {
            let mut s = d.state.lock().unwrap();
            s.finished = Some("done".into());
        }

        // Any subsequent key should auto-dismiss stop_pending without aborting.
        handle_key(&d, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            !snap(&d).stop_pending,
            "stop_pending must be auto-cleared when finished is set"
        );
    }

    /// Bracketed paste must be silently ignored while the stop confirmation
    /// is pending so pasted text cannot accumulate in a hidden input buffer
    /// and reappear when the operator later cancels the stop prompt.
    #[test]
    fn paste_ignored_while_stop_pending() {
        let d = make_dashboard();
        let _rx = make_pending(&d);

        // Enter reject-feedback mode so there is an active feedback buffer.
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(
            snap(&d).feedback_input.as_deref(),
            Some(""),
            "feedback buffer must be active"
        );

        // Activate the stop confirmation.
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending, "stop_pending must be set");

        // A paste while stop_pending must not append to the feedback buffer.
        handle_paste(&d, "malicious paste");
        assert_eq!(
            snap(&d).feedback_input.as_deref(),
            Some(""),
            "paste must not modify the feedback buffer while stop_pending"
        );
    }

    // ── Help overlay (#639) ─────────────────────────────────────────────────

    fn question_mark() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)
    }

    /// Render `dash`'s *current* state (not a snapshot frozen earlier) onto a
    /// `TestBackend`, returning the rendered text. Unlike `render_to_buffer`
    /// (which draws a frozen `DashboardSnapshot` against an unrelated fresh
    /// dashboard), this drives the real `draw_frame` path against `dash`
    /// itself so viewport metrics (e.g. `help_viewport_height`) get written
    /// back into the same dashboard the test inspects.
    fn render_live(d: &Arc<RatatuiDashboard>, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = RatatuiTerminal::new(backend).unwrap();
        let area = Rect::new(0, 0, w, h);
        // Mirrors the one line of `draw_frame`'s pre-computation this helper
        // needs: `draw_help_overlay` is now a pure function (issue #639
        // review), so `help_viewport_height` must be captured before `draw`
        // renders, exactly as `draw_frame` does while it holds the lock.
        {
            let mut s = d
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let help_overlay = centered_rect(80, 80, area);
            let help_block = Block::default().borders(Borders::ALL);
            s.help_viewport_height = help_block.inner(help_overlay).height;
        }
        let s = snap(d);
        terminal
            .draw(|frame| {
                draw(frame, d, &s);
                let _ = area;
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn question_mark_opens_help_overlay() {
        let d = make_dashboard();
        assert!(!snap(&d).help_open);

        handle_key(&d, question_mark());
        assert!(snap(&d).help_open, "? must open the help overlay");
    }

    /// Terminals that report Shift for printable characters (e.g.
    /// crossterm's Windows parser) deliver `?` as `Char('?')` with
    /// `KeyModifiers::SHIFT`, not empty modifiers. The toggle must still
    /// work there, matching how `handle_key_normal`/`handle_key_search`
    /// already treat plain Shift as equivalent to no modifiers.
    #[test]
    fn shifted_question_mark_opens_help_overlay() {
        let d = make_dashboard();
        assert!(!snap(&d).help_open);

        handle_key(&d, KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
        assert!(
            snap(&d).help_open,
            "a shifted ? must still open the help overlay"
        );
    }

    #[test]
    fn question_mark_again_closes_help_overlay() {
        let d = make_dashboard();
        handle_key(&d, question_mark());
        assert!(snap(&d).help_open);

        handle_key(&d, question_mark());
        assert!(!snap(&d).help_open, "a second ? must close the overlay");
    }

    #[test]
    fn esc_closes_help_overlay() {
        let d = make_dashboard();
        handle_key(&d, question_mark());
        assert!(snap(&d).help_open);

        handle_key(&d, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!snap(&d).help_open, "Esc must close the overlay");
    }

    #[test]
    fn help_overlay_never_resolves_a_pending_prompt() {
        let d = make_dashboard();
        let mut rx = make_pending(&d);

        handle_key(&d, question_mark());
        assert!(snap(&d).help_open);
        assert!(
            snap(&d).pending.is_some(),
            "opening help must not consume the pending prompt"
        );

        // While help is open, decision keys (y/n/e/a/A) and Ctrl-C must be
        // swallowed rather than reaching the pending confirm responder.
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('e'),
            KeyCode::Char('a'),
            KeyCode::Char('A'),
        ] {
            handle_key(&d, KeyEvent::new(code, KeyModifiers::NONE));
        }
        handle_key(&d, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(
            rx.try_recv().is_err(),
            "no decision should have been sent while help is open"
        );
        assert!(snap(&d).help_open, "help must still be open");
        assert!(snap(&d).pending.is_some(), "prompt must still be pending");

        // Closing help must restore the live view with the prompt untouched.
        handle_key(&d, question_mark());
        assert!(!snap(&d).help_open);
        assert!(snap(&d).pending.is_some());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn help_overlay_never_aborts_or_advances_a_monitor_run() {
        // In monitor/yolo mode there is never a pending modal, but Ctrl-Q must
        // still be inert while help is open (AC4: never advances/aborts the
        // run).
        let (d, cancel_rx) = make_dashboard_with_cancel();
        {
            let mut s = d.state.lock().unwrap();
            s.is_monitor = true;
        }

        handle_key(&d, question_mark());
        assert!(snap(&d).help_open);

        handle_key(&d, ctrl_q());
        assert!(
            !snap(&d).stop_pending,
            "Ctrl-Q must be swallowed while help is open"
        );
        assert!(
            !*cancel_rx.borrow(),
            "the run must not be cancelled while help is open"
        );
    }

    #[test]
    fn question_mark_types_literally_while_entering_feedback() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        // Enter reject-feedback mode.
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(snap(&d).feedback_input.as_deref(), Some(""));

        handle_key(&d, question_mark());
        assert!(
            !snap(&d).help_open,
            "? must be a literal character while typing feedback, not the help toggle"
        );
        assert_eq!(snap(&d).feedback_input.as_deref(), Some("?"));
    }

    #[test]
    fn question_mark_types_literally_while_editing_command() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(snap(&d).edit_input.is_some());

        handle_key(&d, question_mark());
        assert!(!snap(&d).help_open);
        assert_eq!(snap(&d).edit_input.as_deref(), Some("x?"));
    }

    #[test]
    fn question_mark_types_literally_while_search_query_is_active() {
        let d = make_dashboard();
        push_lines(&d, 3);
        press(&d, KeyCode::Char('/'));
        assert!(snap(&d).search.is_some());

        handle_key(&d, question_mark());
        assert!(
            !snap(&d).help_open,
            "? must be a literal character in an active search query"
        );
        let query = snap(&d).search.map(|s| s.query);
        assert_eq!(query.as_deref(), Some("?"));
    }

    /// A confirm prompt can arrive asynchronously while a search query is
    /// still being edited — `confirm()` doesn't clear `s.search`. Once that
    /// happens, the modal (not `handle_key_search`) owns input, so the
    /// hidden search-editing state must not block `?` from opening help.
    #[test]
    fn question_mark_opens_help_even_when_a_pending_prompt_shadows_an_editing_search() {
        let d = make_dashboard();
        push_lines(&d, 3);
        press(&d, KeyCode::Char('/'));
        assert!(snap(&d).search.as_ref().is_some_and(|s| s.editing));

        let _rx = make_pending(&d);
        assert!(snap(&d).pending.is_some());

        handle_key(&d, question_mark());
        assert!(
            snap(&d).help_open,
            "? must open help when the editing search is shadowed by a pending prompt"
        );
    }

    #[test]
    fn help_overlay_footer_hint_present_when_pending_shadows_editing_search() {
        let d = make_dashboard();
        push_lines(&d, 3);
        press(&d, KeyCode::Char('/'));
        let _rx = make_pending(&d);
        let s = snap(&d);
        assert!(s.pending.is_some());
        assert!(s.search.as_ref().is_some_and(|se| se.editing));

        let text = buffer_text(&render_to_buffer(&s, 120, 10));
        assert!(
            text.contains("?: help"),
            "footer must not hide the help hint just because a shadowed, non-receiving search exists; got:\n{text}"
        );
    }

    #[test]
    fn question_mark_toggles_help_in_committed_search_mode() {
        let d = make_dashboard();
        push_lines(&d, 3);
        press(&d, KeyCode::Char('/'));
        type_str(&d, "line");
        press(&d, KeyCode::Enter); // commit: editing == false

        handle_key(&d, question_mark());
        assert!(
            snap(&d).help_open,
            "? toggles help once the search query is committed"
        );
    }

    #[test]
    fn question_mark_is_swallowed_during_stop_confirmation() {
        let d = make_dashboard();
        handle_key(&d, ctrl_q());
        assert!(snap(&d).stop_pending);

        handle_key(&d, question_mark());
        assert!(
            !snap(&d).help_open,
            "? must not open help while the stop confirmation owns input"
        );
    }

    #[test]
    fn help_overlay_renders_at_small_terminal_size_without_panicking() {
        let d = make_dashboard();
        handle_key(&d, question_mark());
        // Must not panic at a minimal 80x24 terminal (AC5).
        let text = render_live(&d, 80, 24);
        assert!(text.contains("Navigation"));
        assert!(text.contains("Confirm"));
    }

    #[test]
    fn help_overlay_scrolls_when_content_exceeds_viewport() {
        let d = make_dashboard();
        handle_key(&d, question_mark());
        // Render once at a small size so help_viewport_height is captured.
        let text_top = render_live(&d, 80, 24);
        assert!(
            text_top.contains("Navigation"),
            "top of the list must be visible initially; got:\n{text_top}"
        );

        // Scroll to the end and confirm later categories become visible.
        press(&d, KeyCode::End);
        let text_end = render_live(&d, 80, 24);
        assert!(
            text_end.contains("View"),
            "scrolling to End must reveal the last category; got:\n{text_end}"
        );
    }

    #[test]
    fn help_overlay_footer_hint_present_when_toggle_is_live() {
        let d = make_dashboard();
        let text = render_to_buffer(&snap(&d), 120, 10);
        let text = buffer_text(&text);
        assert!(
            text.contains("?: help"),
            "idle footer must advertise the help affordance; got:\n{text}"
        );
    }

    /// `footer_paragraph`'s `Paragraph` is not wrapped, so content past the
    /// render width is clipped rather than wrapped onto another line. Several
    /// branches (idle, `finished`, a filled-in `pending` scope) are already
    /// wider than an 80-column terminal's inner width (78 cols) on their own,
    /// independent of this hint — a pre-existing limitation this feature
    /// doesn't fully overcome (issue #639 review; see the comment above
    /// `show_help_hint`'s use). This test instead pins the achievable
    /// guarantee: in a state with room to spare (short elapsed "thinking"),
    /// the hint is not clipped.
    #[test]
    fn help_overlay_footer_hint_survives_clipping_at_80_columns() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.activity = Activity::Thinking { since: ago(5) };
        }
        let text = buffer_text(&render_to_buffer(&snap(&d), 80, 10));
        assert!(
            text.contains("?: help"),
            "help hint must not be clipped off an 80-column footer when the state has room; got:\n{text}"
        );
    }

    #[test]
    fn help_overlay_footer_hint_absent_while_typing_feedback() {
        let d = make_dashboard();
        let _rx = make_pending(&d);
        handle_key(&d, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        let s = snap(&d);
        assert!(s.feedback_input.is_some());
        let text = buffer_text(&render_to_buffer(&s, 120, 10));
        assert!(
            !text.contains("?: help"),
            "footer must not advertise help while ? is a literal feedback character; got:\n{text}"
        );
    }

    #[test]
    fn help_overlay_footer_hint_absent_during_stop_confirmation() {
        let d = make_dashboard();
        handle_key(&d, ctrl_q());
        let s = snap(&d);
        assert!(s.stop_pending);
        let text = buffer_text(&render_to_buffer(&s, 120, 10));
        assert!(
            !text.contains("?: help"),
            "footer must not advertise help while the stop confirmation owns input; got:\n{text}"
        );
    }

    /// Every literal keystroke `handle_key` (and its mode-specific helpers)
    /// special-cases must be documented in the `?` help overlay. This is the
    /// drift guard required by the issue: when a new binding is added to
    /// `handle_key`, it must also be added to `KEYBINDINGS`, or this test
    /// fails. Free-text entry (`Char(c)` while typing feedback/edit/search)
    /// is intentionally excluded — that's "any character", not a binding.
    #[test]
    fn every_dispatched_keystroke_is_documented_in_help_overlay() {
        let dispatched: &[(KeyCode, KeyModifiers)] = &[
            (KeyCode::Char('y'), KeyModifiers::NONE),
            (KeyCode::Char('Y'), KeyModifiers::NONE),
            (KeyCode::Char('n'), KeyModifiers::NONE),
            (KeyCode::Char('N'), KeyModifiers::NONE),
            (KeyCode::Char('e'), KeyModifiers::NONE),
            (KeyCode::Char('E'), KeyModifiers::NONE),
            (KeyCode::Char('A'), KeyModifiers::NONE),
            (KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::Down, KeyModifiers::NONE),
            (KeyCode::Char('k'), KeyModifiers::NONE),
            (KeyCode::Char('K'), KeyModifiers::NONE),
            (KeyCode::Char('j'), KeyModifiers::NONE),
            (KeyCode::Char('J'), KeyModifiers::NONE),
            (KeyCode::PageUp, KeyModifiers::NONE),
            (KeyCode::PageDown, KeyModifiers::NONE),
            (KeyCode::Home, KeyModifiers::NONE),
            (KeyCode::End, KeyModifiers::NONE),
            (KeyCode::Char('/'), KeyModifiers::NONE),
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('Q'), KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (KeyCode::Char('q'), KeyModifiers::CONTROL),
            (KeyCode::Char('Q'), KeyModifiers::CONTROL),
            (KeyCode::Char('?'), KeyModifiers::NONE),
        ];
        for (code, mods) in dispatched.iter().copied() {
            let documented = KEYBINDINGS
                .iter()
                .any(|kb| kb.matches.iter().any(|&(c, m)| c == code && m == mods));
            assert!(
                documented,
                "key {code:?} (mods {mods:?}) is handled by handle_key but missing from KEYBINDINGS"
            );
        }
    }

    #[test]
    fn help_overlay_lines_cover_every_keybindings_entry() {
        let rendered: Vec<String> = help_overlay_lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let text = rendered.join("\n");
        for kb in KEYBINDINGS {
            assert!(
                text.contains(kb.keys),
                "help text missing keys label {:?}",
                kb.keys
            );
            assert!(
                text.contains(kb.description),
                "help text missing description for {:?}",
                kb.keys
            );
        }
    }

    /// `help_overlay_lines_count()` (allocation-free, used on every keystroke
    /// while help is open) must always agree with the actual line count
    /// `help_overlay_lines()` renders (issue #639 review).
    #[test]
    fn help_overlay_lines_count_matches_actual_line_count() {
        assert_eq!(help_overlay_lines_count(), help_overlay_lines().len());
    }

    // -- Mouse support (issue #734) --------------------------------------
    //
    // These parallel the `handle_key` tests above but drive synthetic
    // `MouseEvent`s through `handle_mouse` instead of `KeyEvent`s through
    // `handle_key`.

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Push `n` log lines and lay out a feed viewport starting at terminal
    /// row `feed_top_row` (mirrors what `draw_frame` would have captured),
    /// `viewport_height` rows tall, scrolled so row 0 of the viewport shows
    /// log index `feed_scroll_top`.
    fn setup_feed(d: &Arc<RatatuiDashboard>, n: usize, feed_top_row: u16, viewport_height: u16) {
        let mut s = d.state.lock().unwrap();
        for i in 0..n {
            push_info(&mut s, &format!("line{i}"));
        }
        s.feed_top_row = feed_top_row;
        s.viewport_height = viewport_height;
        s.selected_index = Some(0);
        s.feed_scroll_top = 0;
    }

    #[test]
    fn test_mouse_scroll_wheel_moves_selection_like_repeated_keyboard_down() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);

        // Default step is 3 rows: one wheel notch down should land exactly
        // where three `Down` keypresses would.
        let d2 = make_dashboard();
        setup_feed(&d2, 10, 4, 5);
        for _ in 0..3 {
            handle_key(&d2, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let expected = snap(&d2).selected_index;

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(snap(&d).selected_index, expected);
        assert_eq!(snap(&d).selected_index, Some(3));
    }

    #[test]
    fn test_mouse_scroll_wheel_up_moves_selection() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.selected_index = Some(5);
        }

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(snap(&d).selected_index, Some(2));
    }

    #[test]
    fn test_mouse_scroll_step_is_configurable() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.mouse_scroll_step = 1;
        }

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(snap(&d).selected_index, Some(1));
    }

    #[test]
    #[allow(clippy::significant_drop_tightening)]
    fn test_mouse_scroll_wheel_scrolls_monitor_feed_like_keyboard() {
        let d = make_dashboard();
        {
            let mut s = d.state.lock().unwrap();
            s.is_monitor = true;
            s.last_log_height = 5;
            s.last_log_width = 10;
            for i in 0..8 {
                push_info(&mut s, &format!("line{i}"));
            }
        }

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollUp, 0, 0));
        let s = snap(&d);
        assert!(!s.auto_follow);
        assert_eq!(s.scroll_offset, 0); // starts auto-followed at max (3), -3 clamps to 0
    }

    #[test]
    fn test_mouse_scroll_wheel_scrolls_detail_inspector() {
        let d = make_dashboard();
        let multiline_text = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        d.append(LineKind::Info, "summary", Some(multiline_text));
        {
            let mut s = d.state.lock().unwrap();
            s.detail_open = true;
            s.detail_viewport_height = 3;
        }
        assert_eq!(snap(&d).detail_scroll_top, 0);

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(snap(&d).detail_scroll_top, 3);

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(snap(&d).detail_scroll_top, 0);
    }

    #[test]
    fn test_mouse_left_click_selects_visible_row() {
        let d = make_dashboard();
        // feed content starts at terminal row 4, 5 rows tall, showing log
        // indices [2, 7) since feed_scroll_top = 2.
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.feed_scroll_top = 2;
        }

        // Click the 3rd visible row (row 4+2=6) -> log index 2+2=4.
        handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 6),
        );
        assert_eq!(snap(&d).selected_index, Some(4));
    }

    #[test]
    fn test_mouse_left_click_above_feed_is_ignored() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.selected_index = Some(0);
        }

        // Row 1 is inside the header, above the feed's top row (4).
        handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 1),
        );
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    #[test]
    fn test_mouse_left_click_below_feed_viewport_is_ignored() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.selected_index = Some(0);
        }

        // feed_top_row=4, viewport_height=5 -> visible rows are 4..9; row 9
        // is one past the last visible row.
        handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 9),
        );
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    #[test]
    fn test_mouse_click_ignored_in_monitor_mode() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.is_monitor = true;
            s.selected_index = Some(0);
        }

        handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 6),
        );
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    #[test]
    fn test_mouse_ignored_while_confirm_modal_pending() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        let _rx = make_pending(&d);
        {
            let mut s = d.state.lock().unwrap();
            s.selected_index = Some(0);
        }

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollDown, 0, 0));
        handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 6),
        );
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    /// Opening the detail inspector (Enter) while a confirm prompt is
    /// pending hides the modal behind it and `handle_key` already lets
    /// scroll keys reach the detail pane in that state; the mouse wheel must
    /// match, or it silently does nothing over a pane that's visibly
    /// scrollable by keyboard (issue #734 review).
    #[test]
    fn test_mouse_wheel_scrolls_detail_inspector_while_confirm_pending() {
        let d = make_dashboard();
        let multiline_text = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        d.append(LineKind::Info, "summary", Some(multiline_text));
        let _rx = make_pending(&d);
        {
            let mut s = d.state.lock().unwrap();
            s.detail_open = true;
            s.detail_viewport_height = 3;
        }
        assert_eq!(snap(&d).detail_scroll_top, 0);

        assert!(handle_mouse(
            &d,
            mouse_event(MouseEventKind::ScrollDown, 0, 0)
        ));
        assert_eq!(snap(&d).detail_scroll_top, 3);
        assert!(
            snap(&d).pending.is_some(),
            "scrolling the detail pane must not disturb the pending prompt"
        );

        assert!(handle_mouse(
            &d,
            mouse_event(MouseEventKind::ScrollUp, 0, 0)
        ));
        assert_eq!(snap(&d).detail_scroll_top, 0);
    }

    /// A click still can't select a main-feed row in this state: the detail
    /// pane (not the feed) is what's visible, mirroring the keyboard's
    /// `s.detail_open` gate on the click branch.
    #[test]
    fn test_mouse_click_ignored_in_detail_inspector_while_confirm_pending() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        let _rx = make_pending(&d);
        {
            let mut s = d.state.lock().unwrap();
            s.detail_open = true;
            s.selected_index = Some(0);
        }

        handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 6),
        );
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    /// `confirm()` doesn't clear an active `/` search, so a prompt can
    /// arrive while `s.search` is still `Some`; opening the detail inspector
    /// with Enter in that state hides both the modal and the search bar
    /// behind it, and keyboard scroll keys still reach the detail pane. The
    /// mouse wheel must match — `search.is_some()` alone must not block it
    /// once `detail_open` is true (issue #734 review).
    #[test]
    fn test_mouse_wheel_scrolls_detail_inspector_with_pending_and_search_active() {
        let d = make_dashboard();
        let multiline_text = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        d.append(LineKind::Info, "summary", Some(multiline_text));
        let _rx = make_pending(&d);
        {
            let mut s = d.state.lock().unwrap();
            s.search = Some(SearchState::new());
            s.detail_open = true;
            s.detail_viewport_height = 3;
        }
        assert_eq!(snap(&d).detail_scroll_top, 0);

        assert!(handle_mouse(
            &d,
            mouse_event(MouseEventKind::ScrollDown, 0, 0)
        ));
        assert_eq!(snap(&d).detail_scroll_top, 3);
        assert!(snap(&d).pending.is_some());
        assert!(snap(&d).search.is_some());
    }

    #[test]
    fn test_mouse_ignored_while_help_overlay_open() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.help_open = true;
            s.selected_index = Some(0);
        }

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollDown, 0, 0));
        handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 6),
        );
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    #[test]
    fn test_mouse_ignored_while_stop_confirmation_pending() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.stop_pending = true;
            s.selected_index = Some(0);
        }

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    #[test]
    fn test_mouse_ignored_while_search_input_active() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.search = Some(SearchState::new());
            s.selected_index = Some(0);
        }

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollDown, 0, 0));
        handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 6),
        );
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    #[test]
    fn test_mouse_ignored_while_feedback_input_active() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        {
            let mut s = d.state.lock().unwrap();
            s.feedback_input = Some(String::new());
            s.selected_index = Some(0);
        }

        handle_mouse(&d, mouse_event(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(snap(&d).selected_index, Some(0));
    }

    #[test]
    fn test_mouse_scroll_step_default_is_three() {
        assert_eq!(DashboardState::default().mouse_scroll_step, 3);
    }

    #[test]
    fn test_mouse_scroll_step_from_env() {
        // No env var: default of 3.
        // SAFETY: test-only env mutation, no other thread reads this var
        // concurrently within this process's test harness for this key.
        unsafe {
            std::env::remove_var("MAXWELL_MOUSE_SCROLL_STEP");
        }
        assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);

        unsafe {
            std::env::set_var("MAXWELL_MOUSE_SCROLL_STEP", "7");
        }
        assert_eq!(mouse_scroll_step_from_env(), 7);

        // Unparsable/zero falls back to the default.
        unsafe {
            std::env::set_var("MAXWELL_MOUSE_SCROLL_STEP", "not-a-number");
        }
        assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);
        unsafe {
            std::env::set_var("MAXWELL_MOUSE_SCROLL_STEP", "0");
        }
        assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);
        unsafe {
            std::env::remove_var("MAXWELL_MOUSE_SCROLL_STEP");
        }
    }

    // `renderer_loop` uses `handle_mouse`'s return value to decide whether to
    // redraw, specifically to skip the redraw for the `Moved`/`Drag` events a
    // terminal's any-event mouse tracking emits on every idle pointer move
    // (issue #734 review). These tests pin down that contract directly,
    // since `renderer_loop` itself needs a live terminal and isn't unit
    // tested.

    #[test]
    fn test_handle_mouse_returns_true_for_actionable_events() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        assert!(handle_mouse(
            &d,
            mouse_event(MouseEventKind::ScrollDown, 0, 0)
        ));
        assert!(handle_mouse(
            &d,
            mouse_event(MouseEventKind::ScrollUp, 0, 0)
        ));
        assert!(handle_mouse(
            &d,
            mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 6)
        ));
    }

    #[test]
    fn test_handle_mouse_returns_false_for_ignored_event_kinds() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        let ignored = [
            MouseEventKind::Moved,
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Down(MouseButton::Middle),
        ];
        for kind in ignored {
            assert!(
                !handle_mouse(&d, mouse_event(kind, 10, 6)),
                "expected {kind:?} to be a no-op that skips the redraw"
            );
        }
    }

    #[test]
    fn test_handle_mouse_returns_false_when_modal_owns_focus() {
        let d = make_dashboard();
        setup_feed(&d, 10, 4, 5);
        let _rx = make_pending(&d);
        assert!(!handle_mouse(
            &d,
            mouse_event(MouseEventKind::ScrollDown, 0, 0)
        ));
    }
}
