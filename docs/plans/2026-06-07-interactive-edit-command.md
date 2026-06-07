# Operator command-edit in interactive mode Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the "Edit/Modify" capability (Phase 2 of #312) in interactive mode, allowing operators to edit proposed commands in place before execution.

**Architecture:** Introduce `ConfirmDecision::Edit(String)` which is returned by confirmers when the operator chooses to edit the command. In `DefaultAgent::step`, if the operator edits a command, the edited command is re-evaluated by the policy engine and `PreToolUse` hooks. The environment runs the edited command, and the trajectory observation captures the decision as `"edit"`, retaining both the original proposed command and the substituted command. Stderr UI uses crossterm to provide an inline single-line editor, and the Ratatui dashboard opens a text input modal.

**Tech Stack:** Rust, crossterm, ratatui, tokio.

---

## User Review Required

> [!NOTE]
> For the single-line inline editor in `stderr` UI, we use raw terminal mode to support interactive cursor navigation, character insertion, home/end, backspace, and delete. The proposed command is pre-filled. If the edited command is empty, or the user cancels with Esc or Ctrl-C, the UI returns to the main Approve/Reject/Abort prompt.

> [!IMPORTANT]
> A command edited by the operator is strictly re-run through the policy engine and `PreToolUse` hooks. A policy-blocked or hook-blocked edit will generate the standard block/denial observation instead of executing.

## Open Questions

None at this time. The requirements are fully detailed in issue #627.

## Proposed Changes

### Confirm core

#### [MODIFY] [confirm.rs](file:///c:/Users/markm/rust_swe_agent/src/agent/confirm.rs)
- Add `Edit(String)` variant to `ConfirmDecision`.
- Update `.label()` to map `ConfirmDecision::Edit(_)` to `"edit"`.
- Update `tests::decision_labels_are_stable` to cover `"edit"`.

### Stderr UI Confirmer

#### [MODIFY] [confirm_cli.rs](file:///c:/Users/markm/rust_swe_agent/src/agent/confirm_cli.rs)
- Update `render_banner` to show `(e)edit` as a choice.
- In `read_single_keystroke(ctx: &ConfirmContext)`, if `e` or `E` is pressed, enter `run_inline_editor(&mut err, &ctx.command)`.
- Implement `run_inline_editor` using crossterm raw mode inputs (handling `Enter`, `Esc`, `Ctrl-C`, `Backspace`, `Delete`, `Left`, `Right`, `Home`, `End`, and character input).
- Return to choice selection if editing is cancelled (empty line, Esc, or Ctrl-C).

### Ratatui Dashboard TUI Confirmer

#### [MODIFY] [confirm_tui.rs](file:///c:/Users/markm/rust_swe_agent/src/agent/confirm_tui.rs)
- Add `edit_input: Option<String>` to `DashboardState` and `DashboardSnapshot`.
- In `handle_key`, if not in edit mode and `e` or `E` is pressed, set `edit_input = Some(pending.ctx.command.clone())`.
- In `handle_key`, if in edit mode (`edit_input.is_some()`), handle `Enter` (submits `ConfirmDecision::Edit(buffer)` if non-empty, cancels back to choice if empty), `Esc` (cancels back to choice), `Backspace` (pops from buffer), and character input (appends to buffer).
- Update `draw_modal` and `draw` to render the single-line text-entry field pre-filled with the command when `edit_input` is active.
- Update `footer_paragraph` hints to reflect the edit mode.
- Add unit/integration tests to verify the TUI edit keystroke flows.

### Agent loop execution

#### [MODIFY] [default.rs](file:///c:/Users/markm/rust_swe_agent/src/agent/default.rs)
- In `step()`, handle `ConfirmDecision::Edit(edited_command)` from `confirm_operator_action`.
- If edited:
  - Recheck the edited command against the policy engine. If denied, record policy denial and include `interactive_decision: "edit"` and both commands in trajectory `MessageExtra`, then return `StepOutcome::Continue`.
  - Re-run `PreToolUse` hooks on the edited command. Update `pre_hook_results` and `tool_use_blocked` status.
  - Run/execute the edited command (using the edited string instead of the original `tool_input`).
  - Set `tool_input_for_observation` to the edited command.
- In `obs_extra` logging for the executed step, if the command was edited, record:
  - `interactive_decision: "edit"`
  - `interactive_proposed_command` (redacted original command)
  - `interactive_substituted_command` (redacted edited command)
  - `interactive_tool_name`
  - `interactive_timestamp`

### Integration Tests

#### [MODIFY] [interactive_confirm.rs](file:///c:/Users/markm/rust_swe_agent/tests/interactive_confirm.rs)
- Add integration test `edit_executes_edited_command_and_records_trajectory` to verify:
  - An edit decision runs the policy gate and hook gate on the edited command.
  - It executes the edited command.
  - The trajectory observation records `interactive_decision: "edit"`, original proposed command, and substituted command.
- Add integration test `edit_blocked_by_policy_fails_with_denial` to verify:
  - An edited command that violates policy is refused with a policy-denied observation.
  - The trajectory records the denial details and `interactive_decision: "edit"`.

---

## Verification Plan

### Automated Tests
- Run new and existing integration tests:
  ```powershell
  cargo test --test interactive_confirm
  ```
- Run confirming unit tests:
  ```powershell
  cargo test agent::confirm
  cargo test agent::confirm_cli
  cargo test agent::confirm_tui
  ```

### Manual Verification
- Not applicable for non-interactive runner, but automated integration tests will mock interactive confirmer callbacks to prove all interactive paths, policy gates, and hook validations.
