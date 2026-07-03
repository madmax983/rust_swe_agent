//! Interactive ratatui dashboard for live sweep monitoring (issue #641).
//!
//! `bench tail --sweep <dir> --ui ratatui` renders a full-screen dashboard
//! derived **only** from the same read-only snapshot layer as `bench tail`'s
//! default text/JSON output: [`crate::run::tail::snapshot`] for aggregate
//! progress and [`crate::run::tail::instance_rows`] for the per-instance
//! list. It opens no sockets and writes no files. Selecting an instance
//! drills into a live-follow pane that reuses `bench watch`'s trajectory
//! polling (`crate::run::watch::resolve_watch_path`) and step rendering
//! (`crate::run::inspect::build_inspect_steps_with_max`).
//!
//! The module is split so the state machine and rendering are pure,
//! deterministic functions ([`handle_key`], [`draw`]) testable with
//! ratatui's `TestBackend` — no real terminal required — while [`run`] is
//! the thin IO shell that owns the terminal and the poll loop.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::error::Error;
use crate::redaction::Redactor;
use crate::run::inspect::{build_inspect_steps_with_max, redact_trajectory_for_inspect};
use crate::run::tail::{InstanceRow, InstanceStatus, SnapshotOptions, TailSnapshot};
use crate::run::watch::{is_terminal_outcome, resolve_watch_path};
use crate::trajectory::Trajectory;

/// Cap on retained drill-down lines, mirroring `confirm_tui`'s log cap so a
/// long-running instance can't grow the dashboard's memory unbounded.
const MAX_DETAIL_LINES: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    List,
    Detail,
}

/// Live state of the selected instance's drill-down pane.
pub(crate) struct DetailState {
    pub(crate) instance_id: String,
    pub(crate) run_index: u32,
    pub(crate) lines: VecDeque<String>,
    pub(crate) scroll_top: usize,
    emitted_steps: usize,
    cached_path: Option<PathBuf>,
    last_file_len: u64,
    last_file_mtime: Option<SystemTime>,
    /// True once the followed trajectory reports a terminal outcome.
    pub(crate) terminal: bool,
    /// True while no trajectory file has been found yet for the selection,
    /// or a previously-found one has since become unreadable (deleted,
    /// pruned, moved) — see `refresh_detail`.
    pub(crate) not_found: bool,
    /// True while the pane should track the live tail of the trajectory as
    /// new turns stream in (the default). Pressing Up/k to look at earlier
    /// output disables it; scrolling back down to the end re-enables it —
    /// see `scroll_detail_up`/`scroll_detail_down` and `render_detail` (PR
    /// #999 review: without this, `scroll_top` never advances on its own, so
    /// a trajectory longer than the pane stays pinned to its earliest steps
    /// while fresh output appends below the visible window).
    pub(crate) auto_follow: bool,
}

impl DetailState {
    fn new(instance_id: String, run_index: u32) -> Self {
        Self {
            instance_id,
            run_index,
            lines: VecDeque::new(),
            scroll_top: 0,
            emitted_steps: 0,
            cached_path: None,
            last_file_len: 0,
            last_file_mtime: None,
            terminal: false,
            not_found: true,
            auto_follow: true,
        }
    }
}

/// Full dashboard state. Pure data — no IO — so [`handle_key`] and [`draw`]
/// are unit-testable without a terminal.
pub(crate) struct DashboardState {
    pub(crate) sweep_dir: PathBuf,
    pub(crate) snapshot: Option<TailSnapshot>,
    pub(crate) rows: Vec<InstanceRow>,
    pub(crate) selected: usize,
    pub(crate) mode: Mode,
    pub(crate) help_open: bool,
    pub(crate) detail: Option<DetailState>,
    pub(crate) should_quit: bool,
}

impl DashboardState {
    pub(crate) fn new(sweep_dir: PathBuf) -> Self {
        Self {
            sweep_dir,
            snapshot: None,
            rows: Vec::new(),
            selected: 0,
            mode: Mode::List,
            help_open: false,
            detail: None,
            should_quit: false,
        }
    }

    /// Replace the aggregate snapshot and instance rows with a freshly
    /// polled read. `rows` is rebuilt from scratch every tick (it's derived
    /// from a `BTreeMap` sorted by `(instance_id, run_index)` — see
    /// `tail::instance_rows`), so its order shifts whenever an instance
    /// earlier in sort order starts or finishes between polls. Re-locate the
    /// previously-selected instance by identity rather than trusting the raw
    /// index to still point at the same row (issue #641 review) — falling
    /// back to clamping only when that instance is no longer present.
    pub(crate) fn apply_refresh(&mut self, snapshot: TailSnapshot, rows: Vec<InstanceRow>) {
        let selected_identity = self
            .rows
            .get(self.selected)
            .map(|row| (row.instance_id.clone(), row.run_index));
        self.snapshot = Some(snapshot);
        self.rows = rows;
        if let Some((id, run_index)) = selected_identity {
            if let Some(new_index) = self
                .rows
                .iter()
                .position(|row| row.instance_id == id && row.run_index == run_index)
            {
                self.selected = new_index;
                return;
            }
        }
        if self.rows.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }
}

/// Dispatch one key event. Ctrl-C always quits; the `?` help overlay takes
/// priority over both view modes and swallows all other keys while open, so
/// closing it never mutates list/detail state underneath.
pub(crate) fn handle_key(state: &mut DashboardState, key: KeyEvent) {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.should_quit = true;
        return;
    }
    if state.help_open {
        if matches!(key.code, KeyCode::Char('?' | 'q') | KeyCode::Esc) {
            state.help_open = false;
        }
        return;
    }
    match state.mode {
        Mode::List => handle_list_key(state, key),
        Mode::Detail => handle_detail_key(state, key),
    }
}

fn handle_list_key(state: &mut DashboardState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') => state.should_quit = true,
        KeyCode::Char('?') => state.help_open = true,
        KeyCode::Up | KeyCode::Char('k') => move_selection_up(state),
        KeyCode::Down | KeyCode::Char('j') => move_selection_down(state),
        KeyCode::Enter => open_detail(state),
        _ => {}
    }
}

fn handle_detail_key(state: &mut DashboardState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') => state.should_quit = true,
        KeyCode::Char('?') => state.help_open = true,
        KeyCode::Esc => {
            state.mode = Mode::List;
            state.detail = None;
        }
        KeyCode::Up | KeyCode::Char('k') => scroll_detail_up(state),
        KeyCode::Down | KeyCode::Char('j') => scroll_detail_down(state),
        _ => {}
    }
}

/// Move the list selection up one row, clamped at the first row. Scroll
/// position is not tracked here — [`list_scroll_top`] derives it from
/// `selected` at draw time instead.
fn move_selection_up(state: &mut DashboardState) {
    state.selected = state.selected.saturating_sub(1);
}

/// Move the list selection down one row, clamped at the last row.
fn move_selection_down(state: &mut DashboardState) {
    if state.rows.is_empty() {
        return;
    }
    let max = state.rows.len() - 1;
    state.selected = (state.selected + 1).min(max);
}

/// Scroll toward earlier output. The first press while auto-following
/// anchors near the current tail (rather than jumping to `scroll_top`'s
/// stale value, which isn't updated while following) before disabling
/// follow, so the pane doesn't visibly jump when the operator starts
/// scrolling back.
fn scroll_detail_up(state: &mut DashboardState) {
    let Some(detail) = state.detail.as_mut() else {
        return;
    };
    if detail.auto_follow {
        detail.auto_follow = false;
        detail.scroll_top = detail.lines.len().saturating_sub(1);
    }
    detail.scroll_top = detail.scroll_top.saturating_sub(1);
}

/// Scroll toward the live tail; re-enables auto-follow once the operator has
/// scrolled all the way back down, so the pane resumes tracking new output
/// without needing a separate keybinding (PR #999 review).
fn scroll_detail_down(state: &mut DashboardState) {
    let Some(detail) = state.detail.as_mut() else {
        return;
    };
    if detail.auto_follow {
        return;
    }
    let max = detail.lines.len().saturating_sub(1);
    detail.scroll_top = (detail.scroll_top + 1).min(max);
    if detail.scroll_top >= max {
        detail.auto_follow = true;
    }
}

fn open_detail(state: &mut DashboardState) {
    let Some(row) = state.rows.get(state.selected) else {
        return;
    };
    state.detail = Some(DetailState::new(row.instance_id.clone(), row.run_index));
    state.mode = Mode::Detail;
}

/// Derive the list's scroll offset from the current selection and viewport
/// height so the selected row is always on screen. Pure so it's directly
/// unit-testable without rendering.
pub(crate) fn list_scroll_top(selected: usize, len: usize, viewport: usize) -> usize {
    if viewport == 0 || len <= viewport {
        return 0;
    }
    let max_top = len - viewport;
    selected
        .saturating_sub(viewport.saturating_sub(1))
        .min(max_top)
}

/// The detail pane's effective scroll offset for this draw: while
/// auto-following, always show the freshest `content_height` lines
/// regardless of the stored (stale, unused-while-following) `scroll_top`;
/// once the operator has scrolled away, honor their absolute position (PR
/// #999 review). Pure so it's directly unit-testable without rendering.
pub(crate) fn detail_scroll_top(detail: &DetailState, content_height: usize) -> usize {
    if detail.auto_follow {
        detail.lines.len().saturating_sub(content_height)
    } else {
        detail.scroll_top
    }
}

/// Whether the followed file can be treated as unchanged since the last
/// successful read — both length AND mtime must match, mirroring
/// `watch::run`'s `skippable` check. Pure so the mtime requirement is
/// directly unit-testable without real file timestamps.
fn file_unchanged(detail: &DetailState, len: u64, mtime: Option<SystemTime>) -> bool {
    detail.emitted_steps > 0
        && len == detail.last_file_len
        && mtime.is_some()
        && mtime == detail.last_file_mtime
}

/// Poll the selected instance's trajectory file for new turns, mirroring
/// `bench watch`'s follow loop (`crate::run::watch::run`) but appending
/// plain-text lines to `detail.lines` instead of writing to stdout.
fn refresh_detail(detail: &mut DetailState, sweep_dir: &Path, redactor: &Redactor) {
    let path = detail
        .cached_path
        .clone()
        .or_else(|| resolve_watch_path(sweep_dir, &detail.instance_id, detail.run_index));
    let Some(path) = path else {
        detail.not_found = true;
        detail.cached_path = None;
        return;
    };

    // Only treated as "found" once `metadata` confirms the file is still
    // there — a path that was resolved on a prior tick but has since been
    // deleted/pruned (e.g. `bench du --prune` against the same sweep dir)
    // must fall back to "waiting for trajectory file…" instead of leaving
    // stale content on screen forever (issue #641 review). Forgetting
    // `cached_path` here also lets the next tick re-resolve it, self-healing
    // if the file reappears under a different path.
    let Ok(metadata) = std::fs::metadata(&path) else {
        detail.not_found = true;
        detail.cached_path = None;
        return;
    };
    detail.cached_path = Some(path.clone());
    detail.not_found = false;

    let len = metadata.len();
    let mtime = metadata.modified().ok();
    // Require both length AND mtime to be unchanged before skipping a
    // re-read, mirroring `watch::run`'s `skippable` check — a length-only
    // check misses an in-place rewrite whose new content happens to be the
    // same byte length as the last checkpoint (issue #641 review).
    if file_unchanged(detail, len, mtime) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut traj) = serde_json::from_str::<Trajectory>(&text) else {
        return;
    };
    detail.last_file_len = len;
    detail.last_file_mtime = mtime;
    redact_trajectory_for_inspect(&mut traj, redactor);
    let steps = build_inspect_steps_with_max(&traj, false, 4096);
    if steps.len() < detail.emitted_steps {
        // The trajectory shrank — a worker restart or retry rewrote the file
        // with fewer steps than we'd already rendered. Re-reading from step 0
        // without clearing the buffer would append the (now stale) old run's
        // lines ahead of the fresh ones, corrupting the log; drop everything
        // and start over, and resume auto-follow so the operator sees the new
        // run's output rather than being pinned to a scroll position that no
        // longer means anything (PR #999 review).
        detail.emitted_steps = 0;
        detail.lines.clear();
        detail.scroll_top = 0;
        detail.auto_follow = true;
    }
    for step in &steps[detail.emitted_steps..] {
        let mut summary = format!("step {} {}", step.index, step.role);
        if let Some(msg) = &step.message {
            let _ = write_first_line(&mut summary, msg);
        }
        if let Some(bash) = &step.bash {
            let _ = write_first_line(&mut summary, bash);
        }
        if let Some(code) = step.exit_code {
            use std::fmt::Write as _;
            let _ = write!(summary, " (exit {code})");
        }
        push_capped(detail, summary);
        if let Some(stdout) = &step.stdout {
            for line in stdout.lines().take(20) {
                push_capped(detail, format!("  {line}"));
            }
        }
        if let Some(stderr) = &step.stderr {
            for line in stderr.lines().take(20) {
                push_capped(detail, format!("  ! {line}"));
            }
        }
    }
    detail.emitted_steps = steps.len();
    detail.terminal = is_terminal_outcome(&traj);
}

fn write_first_line(out: &mut String, text: &str) -> std::fmt::Result {
    use std::fmt::Write as _;
    if let Some(first) = text.lines().next() {
        write!(out, ": {first}")?;
    }
    Ok(())
}

/// Append a line to `detail.lines`, evicting the oldest one once the buffer
/// is at capacity. Popping the front element shifts every remaining line's
/// logical index back by one, so `scroll_top` is decremented in lockstep —
/// otherwise a scrolled-up viewport would silently drift toward newer
/// content on every eviction with no user input (issue #641 review).
fn push_capped(detail: &mut DetailState, line: String) {
    if detail.lines.len() >= MAX_DETAIL_LINES {
        detail.lines.pop_front();
        detail.scroll_top = detail.scroll_top.saturating_sub(1);
    }
    detail.lines.push_back(line);
}

/// Refresh the aggregate/list data and, when a drill-down is open, the
/// followed instance's live turns. The single entry point the IO shell calls
/// once per tick.
pub(crate) fn refresh(state: &mut DashboardState, redactor: &Redactor) -> Result<(), Error> {
    let options = SnapshotOptions::default();
    // `snapshot_and_rows` shares one `results.json` read between the
    // aggregate snapshot and the instance list instead of each reading it
    // independently (issue #641 review).
    let (snapshot, rows) = crate::run::tail::snapshot_and_rows(&state.sweep_dir, &options)?;
    state.apply_refresh(snapshot, rows);
    if let Some(detail) = state.detail.as_mut() {
        refresh_detail(detail, &state.sweep_dir, redactor);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Rendering — pure, `TestBackend`-testable.
// ---------------------------------------------------------------------

pub(crate) fn draw(frame: &mut Frame, state: &DashboardState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(aggregate_panel_height(state)),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(aggregate_paragraph(state), chunks[0]);

    match state.mode {
        Mode::List => render_list(frame, state, chunks[1]),
        Mode::Detail => render_detail(frame, state, chunks[1]),
    }

    frame.render_widget(footer_paragraph(state), chunks[2]);

    if state.help_open {
        draw_help_overlay(frame, area);
    }
}

fn aggregate_panel_height(state: &DashboardState) -> u16 {
    let mut height = 6;
    if let Some(snap) = state.snapshot.as_ref() {
        if snap.is_complete {
            height += 1;
        }
        if !snap.warnings.is_empty() {
            height += 1;
        }
    }
    height
}

fn aggregate_paragraph(state: &DashboardState) -> Paragraph<'_> {
    let mut lines: Vec<Line> = Vec::new();
    let Some(snap) = state.snapshot.as_ref() else {
        lines.push(Line::from("loading sweep snapshot…"));
        return Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" bench tail — sweep dashboard "),
        );
    };

    if snap.is_complete {
        lines.push(Line::from(Span::styled(
            format!(
                "SWEEP COMPLETE — {}/{} completed, ${:.4} total cost",
                snap.completed, snap.total, snap.cumulative_cost_usd
            ),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::from(format!(
        "{}/{} completed   {} in flight   {} pending",
        snap.completed, snap.total, snap.in_flight, snap.pending
    )));

    if snap.failure_counts.is_empty() {
        lines.push(Line::from("failures: none"));
    } else {
        let summary = snap
            .failure_counts
            .iter()
            .map(|(cat, n)| format!("{cat:?}={n}"))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(format!("failures: {summary}")));
    }

    lines.push(Line::from(format!(
        "cost: ${:.4} actual  vs  ${:.4} baseline   burn ${:.2}/min",
        snap.cumulative_cost_usd, snap.baseline_cumulative_cost_usd, snap.burn_rate_usd_per_min
    )));

    if let Some(reason) = &snap.abort_reason {
        lines.push(Line::from(Span::styled(
            format!("abort: {reason}"),
            Style::default().fg(Color::Red),
        )));
    } else if let Some(cb) = &snap.circuit_breaker_status {
        lines.push(Line::from(format!("circuit breaker: {cb}")));
    } else {
        lines.push(Line::from(format!("status: {}", snap.status)));
    }

    // Parse/schema-compat warnings from `results.json`/trajectory files:
    // `bench tail`'s plain-text output surfaces these under "Warning:" lines,
    // but the interactive dashboard had no rendering path for them at all,
    // leaving an operator relying solely on it flying blind on partial/stale
    // data (issue #641 review). Kept compact (a count, not the full list) to
    // respect the panel's fixed-height budget.
    if !snap.warnings.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "{} warning(s) — see `bench tail --format text` for details",
                snap.warnings.len()
            ),
            Style::default().fg(Color::Yellow),
        )));
    }

    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" bench tail — sweep dashboard "),
        )
        .wrap(Wrap { trim: false })
}

fn status_label(status: InstanceStatus) -> &'static str {
    match status {
        InstanceStatus::Pending => "pending",
        InstanceStatus::InFlight => "in-flight",
        InstanceStatus::Terminal => "terminal",
    }
}

fn row_style(status: InstanceStatus) -> Style {
    match status {
        InstanceStatus::Pending => Style::default().fg(Color::DarkGray),
        InstanceStatus::InFlight => Style::default().fg(Color::Yellow),
        InstanceStatus::Terminal => Style::default().fg(Color::White),
    }
}

fn render_list(frame: &mut Frame, state: &DashboardState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" instances ({}) ", state.rows.len()));
    let inner_height = block.inner(area).height as usize;

    let items: Vec<ListItem> = state
        .rows
        .iter()
        .map(|row| {
            let text = format!(
                "{:<40} run {:<3} step {:<5} {:<10} {}",
                row.instance_id,
                row.run_index,
                row.current_step
                    .map_or_else(|| "-".to_owned(), |s| s.to_string()),
                status_label(row.status),
                row.outcome.as_deref().unwrap_or("-"),
            );
            ListItem::new(Line::from(Span::styled(text, row_style(row.status))))
        })
        .collect();

    let mut list_state = ListState::default();
    if !state.rows.is_empty() {
        list_state.select(Some(state.selected));
        *list_state.offset_mut() =
            list_scroll_top(state.selected, state.rows.len(), inner_height.max(1));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_detail(frame: &mut Frame, state: &DashboardState, area: Rect) {
    let Some(detail) = state.detail.as_ref() else {
        return;
    };
    let title = format!(
        " {} (run {}) — live trajectory ",
        detail.instance_id, detail.run_index
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner_height = block.inner(area).height as usize;

    let mut lines: Vec<Line> = Vec::new();
    if detail.not_found {
        lines.push(Line::from("waiting for trajectory file…"));
    } else {
        // Reserve one line for the "[instance complete]" banner so it's
        // never pushed past the pane's rendered height: `Paragraph` (with no
        // `.scroll()` set) silently clips anything beyond `inner_height`, so
        // appending the banner *after* an already-full content window drops
        // it with no on-screen indication (issue #641 review).
        let content_height = if detail.terminal {
            inner_height.saturating_sub(1)
        } else {
            inner_height
        };
        let scroll_top = detail_scroll_top(detail, content_height);
        for line in detail.lines.iter().skip(scroll_top).take(content_height) {
            lines.push(Line::from(line.as_str()));
        }
        if detail.terminal {
            lines.push(Line::from(Span::styled(
                "[instance complete]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn footer_paragraph(state: &DashboardState) -> Paragraph<'static> {
    let text = match state.mode {
        Mode::List => "↑/↓ or j/k navigate   Enter inspect   q/Ctrl-C quit   ? help",
        Mode::Detail => "↑/↓ or j/k scroll   Esc back to list   q/Ctrl-C quit   ? help",
    };
    Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::BOLD),
    )))
}

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let overlay = centered_rect(70, 60, area);
    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help — keybindings (? or Esc to close) ");
    let lines = vec![
        Line::from("↑ / k        move selection up"),
        Line::from("↓ / j        move selection down"),
        Line::from("Enter        drill into the selected instance's live trajectory"),
        Line::from("Esc          return to the instance list from a drill-down"),
        Line::from("?            toggle this help overlay"),
        Line::from("q / Ctrl-C   quit — restores the terminal"),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), overlay);
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

// ---------------------------------------------------------------------
// IO shell
// ---------------------------------------------------------------------

pub struct DashboardArgs {
    pub sweep: PathBuf,
    pub interval_ms: u64,
}

/// Restores raw mode and leaves the alternate screen on drop, so an early
/// return (including a poll/read error) never leaves the operator's
/// terminal half-configured.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Run the interactive sweep dashboard until the operator quits.
///
/// # Errors
/// Returns `Err` if the terminal cannot be configured, or if a snapshot
/// read fails (e.g. the sweep directory disappears mid-run).
pub fn run(args: &DashboardArgs) -> Result<(), Error> {
    crossterm::terminal::enable_raw_mode().map_err(Error::Io)?;
    let mut stdout = std::io::stdout();
    if let Err(err) = crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen) {
        let _ = crossterm::terminal::disable_raw_mode();
        return Err(Error::Io(err));
    }
    let _guard = TerminalGuard;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend).map_err(Error::Io)?;

    let mut state = DashboardState::new(args.sweep.clone());
    let redactor = Redactor::default_enabled();
    let interval = Duration::from_millis(args.interval_ms.max(1));
    let mut last_refresh = Instant::now()
        .checked_sub(interval)
        .unwrap_or_else(Instant::now);

    loop {
        if last_refresh.elapsed() >= interval {
            refresh(&mut state, &redactor)?;
            last_refresh = Instant::now();
        }

        terminal
            .draw(|frame| draw(frame, &state))
            .map_err(Error::Io)?;

        if state.should_quit {
            return Ok(());
        }

        let poll_timeout = interval
            .checked_sub(last_refresh.elapsed())
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(200));
        if crossterm::event::poll(poll_timeout).map_err(Error::Io)? {
            if let crossterm::event::Event::Key(key) =
                crossterm::event::read().map_err(Error::Io)?
            {
                if key.kind == crossterm::event::KeyEventKind::Press {
                    handle_key(&mut state, key);
                }
            }
        }

        if state.should_quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_c() -> KeyEvent {
        let mut k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        k.kind = KeyEventKind::Press;
        k
    }

    fn row(id: &str, status: InstanceStatus) -> InstanceRow {
        InstanceRow {
            instance_id: id.to_owned(),
            run_index: 1,
            status,
            current_step: None,
            outcome: None,
            failure_category: None,
        }
    }

    fn state_with_rows(rows: Vec<InstanceRow>) -> DashboardState {
        let mut state = DashboardState::new(PathBuf::from("/tmp/does-not-matter"));
        state.rows = rows;
        state
    }

    #[test]
    fn q_quits_from_list_mode() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::Pending)]);
        handle_key(&mut state, key(KeyCode::Char('q')));
        assert!(state.should_quit);
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::Pending)]);
        state.mode = Mode::Detail;
        handle_key(&mut state, ctrl_c());
        assert!(state.should_quit);
    }

    #[test]
    fn down_then_up_moves_selection_and_clamps() {
        let mut state = state_with_rows(vec![
            row("a", InstanceStatus::Terminal),
            row("b", InstanceStatus::Terminal),
        ]);
        handle_key(&mut state, key(KeyCode::Down));
        assert_eq!(state.selected, 1);
        handle_key(&mut state, key(KeyCode::Down));
        assert_eq!(state.selected, 1, "must clamp at last row");
        handle_key(&mut state, key(KeyCode::Up));
        assert_eq!(state.selected, 0);
        handle_key(&mut state, key(KeyCode::Up));
        assert_eq!(state.selected, 0, "must clamp at first row");
    }

    #[test]
    fn navigation_on_empty_list_does_not_panic() {
        let mut state = state_with_rows(vec![]);
        handle_key(&mut state, key(KeyCode::Down));
        handle_key(&mut state, key(KeyCode::Up));
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn enter_opens_detail_for_selected_row() {
        let mut state = state_with_rows(vec![
            row("a", InstanceStatus::Terminal),
            row("b", InstanceStatus::InFlight),
        ]);
        state.selected = 1;
        handle_key(&mut state, key(KeyCode::Enter));
        assert_eq!(state.mode, Mode::Detail);
        let detail = state.detail.as_ref().unwrap();
        assert_eq!(detail.instance_id, "b");
    }

    #[test]
    fn enter_on_empty_list_is_a_no_op() {
        let mut state = state_with_rows(vec![]);
        handle_key(&mut state, key(KeyCode::Enter));
        assert_eq!(state.mode, Mode::List);
        assert!(state.detail.is_none());
    }

    #[test]
    fn esc_returns_to_list_from_detail() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::Terminal)]);
        handle_key(&mut state, key(KeyCode::Enter));
        assert_eq!(state.mode, Mode::Detail);
        handle_key(&mut state, key(KeyCode::Esc));
        assert_eq!(state.mode, Mode::List);
        assert!(state.detail.is_none());
    }

    #[test]
    fn question_mark_opens_help_from_list_and_detail() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::Terminal)]);
        handle_key(&mut state, key(KeyCode::Char('?')));
        assert!(state.help_open);

        state.help_open = false;
        state.mode = Mode::Detail;
        handle_key(&mut state, key(KeyCode::Char('?')));
        assert!(state.help_open);
    }

    #[test]
    fn help_overlay_swallows_navigation_keys_underneath() {
        let mut state = state_with_rows(vec![
            row("a", InstanceStatus::Terminal),
            row("b", InstanceStatus::Terminal),
        ]);
        state.help_open = true;
        handle_key(&mut state, key(KeyCode::Down));
        assert_eq!(state.selected, 0, "help overlay must swallow navigation");
        assert!(state.help_open);
    }

    #[test]
    fn help_overlay_closes_on_question_mark_esc_or_q() {
        for closing_key in [KeyCode::Char('?'), KeyCode::Esc, KeyCode::Char('q')] {
            let mut state = state_with_rows(vec![row("a", InstanceStatus::Terminal)]);
            state.help_open = true;
            handle_key(&mut state, key(closing_key));
            assert!(!state.help_open, "{closing_key:?} should close help");
            assert!(!state.should_quit, "closing help must not quit the app");
        }
    }

    #[test]
    fn apply_refresh_clamps_selection_when_rows_shrink() {
        let mut state = state_with_rows(vec![
            row("a", InstanceStatus::Terminal),
            row("b", InstanceStatus::Terminal),
            row("c", InstanceStatus::Terminal),
        ]);
        state.selected = 2;
        state.apply_refresh(test_snapshot(), vec![row("a", InstanceStatus::Terminal)]);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn list_scroll_top_keeps_selection_in_view() {
        assert_eq!(list_scroll_top(0, 10, 5), 0);
        assert_eq!(list_scroll_top(4, 10, 5), 0);
        assert_eq!(list_scroll_top(5, 10, 5), 1);
        assert_eq!(list_scroll_top(9, 10, 5), 5);
        assert_eq!(
            list_scroll_top(2, 3, 5),
            0,
            "no scroll needed when list fits"
        );
    }

    fn test_snapshot() -> TailSnapshot {
        TailSnapshot {
            sweep_dir: PathBuf::from("/tmp/x"),
            status: "running".into(),
            cancelling_seconds_left: None,
            completed: 0,
            in_flight: 0,
            pending: 0,
            total: 0,
            failure_counts: Default::default(),
            cumulative_cost_usd: 0.0,
            baseline_cumulative_cost_usd: 0.0,
            burn_rate_usd_per_min: 0.0,
            eta_seconds: None,
            budget_cap_usd: None,
            pct_of_cap_used: None,
            started_at: None,
            last_event_at: None,
            is_complete: false,
            abort_reason: None,
            warnings: Vec::new(),
            total_fallbacks: 0,
            model_mix: Default::default(),
            circuit_breaker_status: None,
            partial_persisted: 0,
        }
    }

    // ---- rendering (TestBackend) ----

    fn render_to_buffer(state: &DashboardState, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
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
    fn draw_shows_loading_before_first_refresh() {
        let state = state_with_rows(vec![]);
        let buf = render_to_buffer(&state, 100, 20);
        assert!(buffer_text(&buf).contains("loading sweep snapshot"));
    }

    #[test]
    fn draw_shows_aggregate_counts() {
        let mut state = state_with_rows(vec![]);
        let mut snap = test_snapshot();
        snap.completed = 3;
        snap.in_flight = 2;
        snap.pending = 5;
        snap.total = 10;
        state.snapshot = Some(snap);
        let buf = render_to_buffer(&state, 120, 20);
        let text = buffer_text(&buf);
        assert!(text.contains("3/10 completed"), "{text}");
        assert!(text.contains("2 in flight"), "{text}");
        assert!(text.contains("5 pending"), "{text}");
    }

    #[test]
    fn draw_shows_failure_counts_and_cost_vs_baseline() {
        let mut state = state_with_rows(vec![]);
        let mut snap = test_snapshot();
        snap.failure_counts
            .insert(crate::trajectory::FailureCategory::EnvSetup, 2);
        snap.cumulative_cost_usd = 3.5;
        snap.baseline_cumulative_cost_usd = 9.75;
        state.snapshot = Some(snap);
        let buf = render_to_buffer(&state, 120, 20);
        let text = buffer_text(&buf);
        assert!(text.contains("EnvSetup=2"), "{text}");
        assert!(text.contains("3.5000"), "{text}");
        assert!(text.contains("9.7500"), "{text}");
    }

    #[test]
    fn draw_shows_complete_banner_when_snapshot_is_complete() {
        let mut state = state_with_rows(vec![]);
        let mut snap = test_snapshot();
        snap.is_complete = true;
        snap.completed = 10;
        snap.total = 10;
        state.snapshot = Some(snap);
        let buf = render_to_buffer(&state, 120, 20);
        assert!(buffer_text(&buf).contains("SWEEP COMPLETE"));
    }

    #[test]
    fn draw_lists_instance_rows() {
        let mut state = state_with_rows(vec![
            row("alpha-instance", InstanceStatus::InFlight),
            row("bravo-instance", InstanceStatus::Terminal),
        ]);
        state.snapshot = Some(test_snapshot());
        let buf = render_to_buffer(&state, 120, 20);
        let text = buffer_text(&buf);
        assert!(text.contains("alpha-instance"), "{text}");
        assert!(text.contains("bravo-instance"), "{text}");
        assert!(text.contains("in-flight"), "{text}");
        assert!(text.contains("terminal"), "{text}");
    }

    #[test]
    fn draw_lists_run_slot_step_and_outcome_columns() {
        let mut state = state_with_rows(vec![InstanceRow {
            instance_id: "charlie".into(),
            run_index: 2,
            status: InstanceStatus::Terminal,
            current_step: Some(7),
            outcome: Some("submitted".into()),
            failure_category: None,
        }]);
        state.snapshot = Some(test_snapshot());
        let buf = render_to_buffer(&state, 120, 20);
        let text = buffer_text(&buf);
        assert!(text.contains("charlie"), "{text}");
        assert!(text.contains("run 2"), "{text}");
        assert!(text.contains("step 7"), "{text}");
        assert!(text.contains("submitted"), "{text}");
    }

    #[test]
    fn draw_shows_help_overlay_when_open() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::Terminal)]);
        state.snapshot = Some(test_snapshot());
        state.help_open = true;
        let buf = render_to_buffer(&state, 120, 30);
        let text = buffer_text(&buf);
        assert!(text.contains("keybindings"), "{text}");
        assert!(text.contains("quit"), "{text}");
    }

    #[test]
    fn draw_shows_detail_pane_waiting_message_before_trajectory_found() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::InFlight)]);
        state.snapshot = Some(test_snapshot());
        handle_key(&mut state, key(KeyCode::Enter));
        let buf = render_to_buffer(&state, 120, 20);
        assert!(buffer_text(&buf).contains("waiting for trajectory file"));
    }

    #[test]
    fn refresh_detail_reads_new_steps_from_trajectory_file() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("alpha");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let mut traj = Trajectory::new();
        traj.info.steps = Some(1);
        traj.messages.push(crate::trajectory::MessageRecord {
            role: "assistant".into(),
            content: "hello from the agent".into(),
            extra: crate::model::MessageExtra::default(),
        });
        std::fs::write(
            instance_dir.join("run-1.traj.json"),
            serde_json::to_string_pretty(&traj).unwrap(),
        )
        .unwrap();

        let mut detail = DetailState::new("alpha".into(), 1);
        let redactor = Redactor::default_enabled();
        refresh_detail(&mut detail, dir.path(), &redactor);

        assert!(!detail.not_found);
        assert!(
            detail
                .lines
                .iter()
                .any(|l| l.contains("hello from the agent")),
            "{:?}",
            detail.lines
        );
    }

    #[test]
    fn refresh_detail_resets_not_found_when_file_disappears() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("alpha");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let mut traj = Trajectory::new();
        traj.info.steps = Some(1);
        let traj_path = instance_dir.join("run-1.traj.json");
        std::fs::write(&traj_path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();

        let mut detail = DetailState::new("alpha".into(), 1);
        let redactor = Redactor::default_enabled();
        refresh_detail(&mut detail, dir.path(), &redactor);
        assert!(!detail.not_found, "file exists; should be found");

        std::fs::remove_file(&traj_path).unwrap();
        refresh_detail(&mut detail, dir.path(), &redactor);
        assert!(
            detail.not_found,
            "not_found must reset to true once the cached path disappears"
        );
    }

    #[test]
    fn file_unchanged_requires_both_length_and_mtime_match() {
        let mut detail = DetailState::new("x".into(), 1);
        detail.emitted_steps = 3;
        detail.last_file_len = 100;
        let t0 = SystemTime::now();
        detail.last_file_mtime = Some(t0);

        assert!(file_unchanged(&detail, 100, Some(t0)));

        let t1 = t0 + Duration::from_secs(1);
        assert!(
            !file_unchanged(&detail, 100, Some(t1)),
            "same length but different mtime must not be treated as unchanged"
        );
        assert!(
            !file_unchanged(&detail, 101, Some(t0)),
            "different length must not be treated as unchanged"
        );
        assert!(
            !file_unchanged(&detail, 100, None),
            "an unavailable mtime must not be treated as unchanged"
        );
    }

    #[test]
    fn push_capped_decrements_scroll_top_when_evicting() {
        let mut detail = DetailState::new("x".into(), 1);
        for i in 0..MAX_DETAIL_LINES {
            push_capped(&mut detail, format!("line-{i}"));
        }
        detail.scroll_top = 5;

        push_capped(&mut detail, "new-line".into());

        assert_eq!(detail.lines.len(), MAX_DETAIL_LINES);
        assert_eq!(
            detail.scroll_top, 4,
            "scroll_top must shift down in lockstep with the evicted front line"
        );
    }

    #[test]
    fn push_capped_does_not_move_scroll_top_below_the_cap() {
        let mut detail = DetailState::new("x".into(), 1);
        detail.scroll_top = 2;
        push_capped(&mut detail, "first".into());
        assert_eq!(
            detail.scroll_top, 2,
            "no eviction happened yet; scroll_top must be untouched"
        );
    }

    #[test]
    fn apply_refresh_preserves_selected_instance_identity_across_reorder() {
        let mut state = state_with_rows(vec![
            row("bravo", InstanceStatus::Terminal),
            row("charlie", InstanceStatus::Terminal),
        ]);
        state.selected = 0; // "bravo"

        // A new instance "alpha" sorts ahead of "bravo" and appears between
        // polls, shifting every subsequent row's index down by one.
        state.apply_refresh(
            test_snapshot(),
            vec![
                row("alpha", InstanceStatus::InFlight),
                row("bravo", InstanceStatus::Terminal),
                row("charlie", InstanceStatus::Terminal),
            ],
        );

        assert_eq!(
            state.rows[state.selected].instance_id, "bravo",
            "selection must follow the instance the operator was looking at, not the raw index"
        );
    }

    #[test]
    fn apply_refresh_falls_back_to_clamping_when_selected_instance_vanishes() {
        let mut state = state_with_rows(vec![
            row("bravo", InstanceStatus::Terminal),
            row("charlie", InstanceStatus::Terminal),
        ]);
        state.selected = 1; // "charlie"

        state.apply_refresh(
            test_snapshot(),
            vec![row("bravo", InstanceStatus::Terminal)],
        );

        assert_eq!(state.selected, 0);
    }

    #[test]
    fn draw_shows_completion_banner_even_when_pane_is_full() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::Terminal)]);
        state.snapshot = Some(test_snapshot());
        handle_key(&mut state, key(KeyCode::Enter));
        {
            let detail = state.detail.as_mut().unwrap();
            // Fill well past any reasonable pane height with real content.
            for i in 0..50 {
                detail.lines.push_back(format!("line {i}"));
            }
            detail.terminal = true;
            detail.not_found = false;
        }
        let buf = render_to_buffer(&state, 120, 20);
        let text = buffer_text(&buf);
        assert!(
            text.contains("instance complete"),
            "completion banner must not be clipped when the pane is full: {text}"
        );
    }

    #[test]
    fn draw_shows_warning_count_when_snapshot_has_warnings() {
        let mut state = state_with_rows(vec![]);
        let mut snap = test_snapshot();
        snap.warnings = vec!["some-file.traj.json: partial or invalid JSON".into()];
        state.snapshot = Some(snap);
        let buf = render_to_buffer(&state, 120, 20);
        let text = buffer_text(&buf);
        assert!(text.contains("1 warning(s)"), "{text}");
    }

    fn assistant_message(content: &str) -> crate::trajectory::MessageRecord {
        crate::trajectory::MessageRecord {
            role: "assistant".into(),
            content: content.into(),
            extra: crate::model::MessageExtra::default(),
        }
    }

    #[test]
    fn refresh_detail_clears_stale_lines_when_step_count_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("alpha");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let traj_path = instance_dir.join("run-1.traj.json");

        let mut traj = Trajectory::new();
        traj.messages.push(assistant_message("first step"));
        traj.messages.push(assistant_message("second step"));
        std::fs::write(&traj_path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();

        let mut detail = DetailState::new("alpha".into(), 1);
        let redactor = Redactor::default_enabled();
        refresh_detail(&mut detail, dir.path(), &redactor);
        assert_eq!(detail.emitted_steps, 2);
        assert!(detail.lines.iter().any(|l| l.contains("second step")));

        // A worker restart/retry rewrites the file with fewer, different steps.
        let mut restarted = Trajectory::new();
        restarted.messages.push(assistant_message("restarted step"));
        std::fs::write(
            &traj_path,
            serde_json::to_string_pretty(&restarted).unwrap(),
        )
        .unwrap();

        refresh_detail(&mut detail, dir.path(), &redactor);

        assert_eq!(detail.emitted_steps, 1);
        assert_eq!(detail.scroll_top, 0);
        assert!(
            !detail.lines.iter().any(|l| l.contains("second step")),
            "stale lines from the old (longer) run must be cleared, not appended to: {:?}",
            detail.lines
        );
        assert!(detail.lines.iter().any(|l| l.contains("restarted step")));
    }

    #[test]
    fn detail_scroll_top_follows_tail_by_default_and_honors_manual_position_otherwise() {
        let mut detail = DetailState::new("x".into(), 1);
        for i in 0..50 {
            detail.lines.push_back(format!("l{i}"));
        }
        assert_eq!(
            detail_scroll_top(&detail, 10),
            40,
            "auto-follow must show the freshest content_height lines"
        );

        detail.auto_follow = false;
        detail.scroll_top = 5;
        assert_eq!(
            detail_scroll_top(&detail, 10),
            5,
            "manual position must be honored once auto-follow is off"
        );
    }

    #[test]
    fn scroll_detail_up_disables_auto_follow_then_scrolling_back_down_re_enables_it() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::Terminal)]);
        handle_key(&mut state, key(KeyCode::Enter));
        {
            let detail = state.detail.as_mut().unwrap();
            for i in 0..10 {
                detail.lines.push_back(format!("line {i}"));
            }
        }
        assert!(state.detail.as_ref().unwrap().auto_follow);

        handle_key(&mut state, key(KeyCode::Up));
        assert!(
            !state.detail.as_ref().unwrap().auto_follow,
            "scrolling up must disable auto-follow"
        );

        for _ in 0..20 {
            handle_key(&mut state, key(KeyCode::Down));
        }
        assert!(
            state.detail.as_ref().unwrap().auto_follow,
            "scrolling back down to the end must re-enable auto-follow"
        );
    }

    #[test]
    fn draw_detail_pane_follows_the_tail_by_default() {
        let mut state = state_with_rows(vec![row("a", InstanceStatus::InFlight)]);
        state.snapshot = Some(test_snapshot());
        handle_key(&mut state, key(KeyCode::Enter));
        {
            let detail = state.detail.as_mut().unwrap();
            detail.not_found = false;
            for i in 0..100 {
                detail.lines.push_back(format!("line-{i}"));
            }
        }
        let buf = render_to_buffer(&state, 120, 20);
        let text = buffer_text(&buf);
        assert!(
            text.contains("line-99"),
            "must show the latest line by default: {text}"
        );
        assert!(
            !text.contains("line-0"),
            "must not stay pinned to the earliest line: {text}"
        );
    }
}
