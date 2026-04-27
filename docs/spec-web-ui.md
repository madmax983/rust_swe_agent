# 🔭 Vantage: Spec for Web UI for Inspecting Trajectories

## 👤 User Story
"As a Developer running the agent locally, I want a full UI web application to inspect trajectories, so that I can easily debug, visualize, and analyze agent behavior without digging through raw JSON files."

## ❓ The "So What?" (Business Problem)
The current CLI-based trajectory inspection (`bench inspect`) requires scanning through long terminal outputs or parsing JSON arrays manually. As trajectories grow to 50+ steps with large code blocks, the terminal interface becomes a severe bottleneck. A visual UI dramatically speeds up debugging and review, ultimately reducing developer iteration time on prompts and tools. This directly reduces the cost of debugging complex agent behavior.

## 🎯 Metric Definition
Success = An operator can find the specific step where the agent failed (or executed an unexpected command) within 30 seconds of opening the UI for a 50-step trajectory, compared to >2 minutes currently spent grepping JSON files.

## ✅ Acceptance Criteria
- Must provide a web-based interface (e.g., served locally or built as a static app) capable of loading `.traj.json` files.
- Must visualize the timeline of steps (System Prompt, Assistant Message, Bash Command, Bash Output, etc.).
- Must support collapsible/expandable sections for large text blocks (e.g., long standard output, multi-file diffs).
- Must display run metadata prominently (Outcome, Total Cost USD, Token Usage, Failure Category).
- Must be zero-config to run (e.g. `rust-swe-agent ui --port 8080`).

## 🚫 Out of Scope
- Real-time streaming UI integration (Phase 3).
- Remote backend for aggregating sweeps (This is local-first).
- Editing or modifying trajectories directly.

## 🕳️ Gap Analysis
- **mini-swe-agent**: Has a basic Python/Flask viewer.
- **SWE-agent**: Has an interactive web UI.
- **rust_swe_agent today**: Only has the terminal-based `bench inspect` subcommand. While functional, it is not scalable for reading full multi-turn coding sessions. Adding a built-in UI brings it to parity with SWE-agent for developer experience.
