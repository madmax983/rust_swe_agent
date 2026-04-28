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
- **rust_swe_agent today**: The codebase has an `InteractiveAgent` struct that prints a status line and catches `Ctrl-C`, but fails to actually pause and ask for confirmation before executing actions. The CLI also lacks the necessary flags to launch a single-task run in interactive mode.
