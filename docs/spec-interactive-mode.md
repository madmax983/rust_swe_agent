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

## 🚫 Out of Scope
- Full "Edit/Modify" capability where the user types a completely new command to replace the agent's (Phase 2).
- Web UI integration for interactive prompts (CLI terminal only for now).
- Complex conversation state injection during the interactive pause.

## 🕳️ Gap Analysis
- **SWE-agent**: Offers robust interactive modes allowing users to intercede.
- **maxwells-daemon today**: shipped in `mini --interactive` (issue #312). `--interactive` enables a stderr y/n/a prompt before every bash/tool action; `--interactive --ui ratatui` swaps it for a full-screen dashboard with a modal prompt and live trajectory feed; `--yolo` alone prints a per-step status line without prompting. Non-TTY stdin without `--yolo` fails fast with "interactive mode requires a TTY; pass --yolo for unattended runs." Hook ordering: PreToolUse hooks fire before the prompt so the operator sees the same command the hook layer evaluated; a denied hook short-circuits the prompt. Rejections write `interactive_decision: "reject"` (+ proposed command, tool name, timestamp) onto the trajectory observation; aborts stamp `interactive_abort` onto trajectory info before `finalize_cancelled` runs.

## Attention signals (issue #648)
When the `--ui ratatui` dashboard blocks on a confirm modal, an operator who has
tabbed away gets no on-screen cue. To close the "I'm not looking at the screen"
gap the dashboard emits a single terminal bell (BEL, `0x07`) on two transitions:
- when a confirm modal goes from absent to present (`DashboardState.pending`
  `None → Some`), rung exactly once per distinct prompt — `confirm()` is called
  once per raise, so redraws never re-ring it; and
- when the run reaches a terminal state (`DashboardState.finished` `None → Some`
  on `StreamEvent::RunEnded`), rung once on the rising edge.

The bell is suppressed entirely when `--no-bell` is passed, when the `NO_BELL`
environment variable is set to any non-empty value, or when stdout is not a TTY
(so piped/CI runs write zero BEL bytes). The read-only `--yolo --ui ratatui`
monitor never blocks on a modal and is out of scope — it stays silent. Resolution
is a pure `bell_enabled(flag, env, is_tty)` gate; the emit path is a small `Bell`
that writes the BEL byte to a sink (stdout in production, a buffer in tests).

## Implementation notes
- Confirmation lives on `DefaultAgent.confirm_callback: Option<Arc<dyn ConfirmCallback>>`. `ConfirmCallback::confirm(ctx) -> ConfirmDecision { Approve, Reject, Abort }` is the only entry point; tests use `ScriptedConfirmer`, the CLI uses `StderrCliConfirmer` (crossterm raw mode with line-buffered fallback), the dashboard uses `RatatuiDashboard` (also a `StreamSink`).
- `ConfirmContext` carries `tool_name`, redacted `command`, `step`, `step_limit`, `cost_usd`, and the cache marker (`cache:explicit` / `cache:auto-or-none`).
- Reject returns a synthetic `Exit code: 1\nCommand rejected by operator (interactive mode). …` user observation so the model can revise within the same step budget.
- Mutually exclusive with `--render-only`; enforced at the clap layer.
