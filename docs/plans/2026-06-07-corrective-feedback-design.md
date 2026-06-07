# Corrective Feedback on Rejection Design Document

**Goal:** Enable operators to steer the agent during interactive mode by supplying corrective feedback upon command rejection.

## Proposed Changes

### 1. `ConfirmDecision` modification

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmDecision {
    Approve,
    Reject(Option<String>),
    Abort,
}
```

We will remove `Copy` from `ConfirmDecision` and update `.label()` to take `&self`.

### 2. CLI UI (`--ui stderr`)

When `prompt_blocking` reads `n`/`N` (reject):
1. Raw mode is disabled.
2. We print a newline, followed by `Provide corrective feedback (optional): `.
3. We read a single line of input from stdin.
4. If the trimmed line is not empty, we return `ConfirmDecision::Reject(Some(feedback))`. Otherwise, we return `ConfirmDecision::Reject(None)`.

### 3. Ratatui TUI (`--ui ratatui`)

We will add a state variable `feedback_input: Option<String>` to track when the user is typing feedback.
- When `feedback_input` is `Some(buffer)`:
  - Any typed character (alphanumeric/spaces/etc.) is appended to `buffer`.
  - `Backspace` removes the last character.
  - `Esc` cancels back to the choices.
  - `Enter` submits the feedback (if empty, maps to `Reject(None)`, otherwise `Reject(Some(buffer))`).
- When `feedback_input` is `None`:
  - `n`/`N` transitions to `feedback_input = Some(String::new())` rather than submitting immediately.

The modal dialog will be updated to display the text entry field when `feedback_input` is active.

### 4. Rejection Handling and Trajectory

In `record_interactive_rejection`:
- The feedback is redacted through the existing `self.redactor.redact_text(feedback, surface)`.
- If feedback is present:
  - The model-facing observation uses `"Exit code: 1\nOutput:\nCommand rejected by operator (interactive mode). The command was not executed. <redacted_feedback>"`.
  - The trajectory record's `MessageExtra` receives a new field `interactive_feedback` containing the trajectory-redacted feedback string.
- If feedback is absent, the behavior remains exactly as it is today.
