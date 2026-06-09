# Read-only Ratatui Monitor for Autonomous (--yolo) Runs Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Provide a live, read-only full-screen Ratatui dashboard for unattended runs using `mini --yolo --ui ratatui` that monitors the trajectory and run outcome without prompting the user for approval.

**Architecture:** Add a new `RatatuiMonitor` variant to `InteractiveMode`. Resolve `--yolo --ui ratatui` (and `--interactive --yolo --ui ratatui`) to this variant. In the runner, start the existing `RatatuiDashboard` without a confirmation callback (`confirm_callback` is `None`), passing an `is_monitor: bool` flag to show "monitor" in the header instead of "interactive", and adjust footer hints to omit interactive instructions.

**Tech Stack:** Rust, Ratatui, Clap

---

## User Review Required

> [!NOTE]
> The read-only monitor retains the TTY check of the interactive Ratatui dashboard. If the stdout/stdin of the run is not a TTY (for example, piped or redirected), the run will fail fast with a clear message rather than corrupting the redirected stream.

## Open Questions

*No open questions at this time.*

## Proposed Changes

### CLI & Runner Setup

#### [MODIFY] [args.rs](file:///c:/Users/markm/rust_swe_agent/src/cli/args.rs)
- No functional changes needed; the existing CLI arguments for `--ui ratatui` and `--yolo` are sufficient. The help documentation for `--ui` already allows `ratatui` as a choice.

#### [MODIFY] [mod.rs](file:///c:/Users/markm/rust_swe_agent/src/cli/mod.rs)
- Update `resolve_interactive_mode` to map `--yolo --ui ratatui` (and `--interactive --yolo --ui ratatui`) to `InteractiveMode::RatatuiMonitor`.
- Add test coverage for this resolution.

#### [MODIFY] [mini.rs](file:///c:/Users/markm/rust_swe_agent/src/run/mini.rs)
- Add `RatatuiMonitor` to `InteractiveMode` enum.
- Update `build_interactive_pieces` to handle `InteractiveMode::RatatuiMonitor`. It should enforce the same TTY guard as `InteractiveMode::Ratatui` but return `Ok((None, Some(handle)))` so that no `confirm_callback` is registered on the agent.
- Update `RatatuiDashboard::start` calls to pass the `is_monitor` boolean.

---

### Dashboard (TUI) Component

#### [MODIFY] [confirm_tui.rs](file:///c:/Users/markm/rust_swe_agent/src/agent/confirm_tui.rs)
- Update `RatatuiDashboard::start` to accept `is_monitor: bool` and store it in `DashboardState`.
- Update `header_paragraph` to display `maxwell's daemon — monitor` instead of `interactive` when `is_monitor` is true.
- Adjust `footer_paragraph` hints to omit confirmation keys when `is_monitor` is true (though since `pending` is never set in monitor mode, it will display the normal step/waiting message or the finished/close message, which naturally satisfies the requirement).

---

## Verification Plan

### Automated Tests
- Run `cargo test --package maxwells-daemon --lib cli::tests::resolve_interactive_mode` to check resolution logic.
- Run `cargo test --package maxwells-daemon --lib agent::confirm_tui::tests` to verify dashboard buffer rendering.

### Manual Verification
- Run a dummy `mini --yolo --ui ratatui` command on a small task to verify that it starts, prints the progress without any prompt, and waits for a close key (`q`/`Esc`) on completion.

---

## TDD Step-by-Step Tasks

### Task 1: Add Unit Tests for CLI Resolution (RED)

**Files:**
- Modify: `src/cli/mod.rs` (adding failing tests to `resolve_interactive_mode_yolo_overrides_interactive` and new tests)

**Step 2: Run test to verify it fails**
Run: `cargo test --package maxwells-daemon --lib cli::tests`
Expected: Compilation failure or test failure because `InteractiveMode::RatatuiMonitor` does not exist yet.

---

### Task 2: Implement CLI and Runner Enum Changes (GREEN)

**Files:**
- Modify: `src/run/mini.rs`
- Modify: `src/cli/mod.rs`

**Step 3: Run tests to verify they pass**
Run: `cargo test --package maxwells-daemon --lib cli::tests`
Expected: PASS

---

### Task 3: Support RatatuiMonitor in Runner & Dashboard (RED)

**Files:**
- Modify: `src/agent/confirm_tui.rs` (add test checking that when `is_monitor` is true, the header renders "monitor")

**Step 2: Run test to verify it fails**
Run: `cargo test --package maxwells-daemon --lib agent::confirm_tui::tests`
Expected: Fails to compile because `is_monitor` is not a field on `DashboardState` or `DashboardSnapshot`.

---

### Task 4: Implement Dashboard changes and Runner wiring (GREEN)

**Files:**
- Modify: `src/agent/confirm_tui.rs`
- Modify: `src/run/mini.rs`

**Step 4: Run tests to verify they pass**
Run: `cargo test`
Expected: PASS

---

### Task 5: Refactor and Verify (REFACTOR)

**Step 1: Clean up any warnings, duplicate code, and format**
Run: `cargo fmt`
Run: `cargo clippy --all-targets`

**Step 2: Run all tests**
Run: `cargo test`
Expected: All tests green.
