# 🔭 Vantage: Spec for Interactive Approval Mode

## 👤 User Story
"As a Developer running the agent locally, I want an interactive mode that pauses and asks for my approval before executing shell commands, so that I can prevent the agent from running destructive or unintended actions in my environment."

## ❓ The "So What?" (Business Problem)
Currently, the agent runs fully autonomously. While this is great for speed, it presents a significant risk when running locally outside of a sandboxed container. A hallucinated or malformed command (e.g., `rm -rf`, unexpected git resets, or heavy network requests) could destroy user data or cause system instability. By adding a human-in-the-loop approval step, we mitigate this risk and build the trust necessary for developers to adopt the agent in their sensitive daily workflows.

## 🎯 Metric Definition
Success = 0 reports of accidental data destruction by the agent when interactive mode is enabled. An operator should be able to read the proposed command and approve or reject it within 1-2 seconds with a simple keystroke.

## ✅ Acceptance Criteria
- Must introduce a CLI flag (e.g., `--interactive` or `-i`) to enable this mode.
- Must pause agent execution right before any bash command or tool execution is dispatched to the environment.
- Must clearly display the proposed command to the user in the terminal.
- Must prompt the user for explicit approval (e.g., `[Y/n/edit]`).
- Must allow the user to reject the command, which should safely abort the trajectory or provide negative feedback to the model to try another approach.
- Must support falling back to autonomous mode if the flag is not provided (backwards compatibility).

## 🚫 Out of Scope
- Fine-grained permissions by command type (e.g., auto-approving `ls` or `cat` but prompting for `rm` and `git push`). Phase 2.
- Interactive approval flows via the Web UI or a remote dashboard.
- Modifying the command directly before execution (Phase 2).

## 🕳️ Gap Analysis
- **SWE-agent**: Offers various human-in-the-loop and interactive capabilities.
- **Other Local Agents (e.g. OpenDevin)**: Often provide a confirmation prompt before running shell commands.
- **rust_swe_agent today**: Commands are executed immediately and silently in the background until the trajectory ends or streaming is set up. There is no built-in pause, meaning the user is merely a spectator to potentially destructive actions.
