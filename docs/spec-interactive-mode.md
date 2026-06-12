# 🔭 Vantage: Spec for Interactive Mode

## 👤 User Story
"As a Developer running the agent locally, I want an interactive confirmation prompt before each bash command is executed, so that I can prevent destructive operations, guide the agent when it gets stuck, or safely run the agent in my host environment."

## ❓ The "So What?" (Business Problem)
Autonomous coding agents inherently carry the risk of executing unintended or harmful commands. While Docker environments mitigate this, many developers prefer running the agent directly in their local environment (`--env local`) for speed or to leverage local tools. Without a confirmation prompt, doing so is highly dangerous. An interactive mode bridges this gap, providing a "copilot" experience rather than a black-box autonomous run, increasing user trust and reducing the risk of catastrophic side effects.

## 🎯 Metric Definition
Success = 0 accidental destructive commands executed during a local run when Interactive Mode is enabled. The interaction loop must be frictionless.

## ✅ Acceptance Criteria
- Must prompt the user explicitly before executing any bash command proposed by the agent.
- Must clearly display the proposed action (command) alongside run context (current step, cumulative cost).
- Must provide options to: Approve, Reject, or Abort the run safely.
- Must allow a "YOLO" override flag to bypass prompts for automated runs while still using the interactive status line.
- **`c` — yank to clipboard (issue #651):** pressing `c` copies the full redacted proposed command to the clipboard when the confirm modal is open, or the most recent log line when no modal is open (including monitor mode). Delivery is via OSC 52 (`ESC ] 52 ; c ; <base64> BEL`), which works over SSH and inside tmux (`set-clipboard on`). A transient footer notice confirms the copy (`copied N chars`) or reports a non-fatal failure (`copy failed: …`); the run continues regardless. In feedback and edit input modes `c` still types the character. Copying never affects trajectory state or the step budget.

## 🚫 Out of Scope
- Full "Edit/Modify" capability where the user types a completely new command to replace the agent's (Phase 2).
- Web UI integration for interactive prompts (CLI terminal only for now).
- Complex conversation state injection during the interactive pause.

## 🕳️ Gap Analysis
- **SWE-agent**: Offers robust interactive modes allowing users to intercede.
- **maxwells-daemon today**: shipped in `mini --interactive` (issue #312) with yank-to-clipboard (issue #651). `--interactive` enables a stderr y/n/a prompt before every bash/tool action; `--interactive --ui ratatui` swaps it for a full-screen dashboard with a modal prompt and live trajectory feed; `--yolo` alone prints a per-step status line without prompting. Non-TTY stdin without `--yolo` fails fast with "interactive mode requires a TTY; pass --yolo for unattended runs." Hook ordering: PreToolUse hooks fire before the prompt so the operator sees the same command the hook layer evaluated; a denied hook short-circuits the prompt. Rejections write `interactive_decision: "reject"` (+ proposed command, tool name, timestamp) onto the trajectory observation; aborts stamp `interactive_abort` onto trajectory info before `finalize_cancelled` runs.

## Implementation notes
- Confirmation lives on `DefaultAgent.confirm_callback: Option<Arc<dyn ConfirmCallback>>`. `ConfirmCallback::confirm(ctx) -> ConfirmDecision { Approve, Reject, Abort }` is the only entry point; tests use `ScriptedConfirmer`, the CLI uses `StderrCliConfirmer` (crossterm raw mode with line-buffered fallback), the dashboard uses `RatatuiDashboard` (also a `StreamSink`).
- `ConfirmContext` carries `tool_name`, redacted `command`, `step`, `step_limit`, `cost_usd`, and the cache marker (`cache:explicit` / `cache:auto-or-none`).
- Reject returns a synthetic `Exit code: 1\nCommand rejected by operator (interactive mode). …` user observation so the model can revise within the same step budget.
- Mutually exclusive with `--render-only`; enforced at the clap layer.
- **Yank-to-clipboard (`c`):** OSC 52 sequence (`ESC ] 52 ; c ; <base64(text)> BEL`) written to stdout between frames, so it cannot interleave with ratatui draw calls. Abstraction point is `ClipboardSink` (`src/agent/clipboard.rs`) on `RatatuiDashboard`; swap that field to add a native-clipboard back-end without touching key handling. Copied text is always the already-redacted `ConfirmContext.command` or log line — redaction is never bypassed. In feedback/edit input modes `c` types the character normally (tested by regression tests in `confirm_tui.rs`).
