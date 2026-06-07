# Session-Scoped Auto-Approve for Command Classes in Interactive Mode Design

## Overview
Interactive mode requires manual approval for every command, which can be exhausting for safe, repetitive commands. This design introduces session-scoped auto-approve rules to automatically approve subsequent commands sharing the same "scope" once the operator approves the first command with the auto-approve option.

## Scope Extraction
The scope of a proposed tool execution is defined as:
- For `bash` commands, the first whitespace-separated token (e.g. `cargo` from `cargo test`). If empty, falls back to `"bash"`.
- For non-bash tools, the tool name (e.g. `read_file`).

## Data Flow & Architecture
1. **Decision Extension**: `ConfirmDecision` gains an `AutoApprove(String)` variant holding the scope.
2. **Rule Store**: `DefaultAgent` maintains a session-scoped `HashSet<String>` of auto-approved scopes.
3. **Execution Gate**: Before prompting the confirmer:
   - Check if the command/tool matches any active rule.
   - If matched, execute it immediately without human prompt.
   - Record in trajectory as `interactive_decision: "auto-approve"` along with the matched scope.
4. **Safety Posture**: The auto-approve check happens *after* policy engine validation and `PreToolUse` hooks. Thus, a command blocked by policy or hooks will still be blocked and never auto-executed.

## UI & Dashboard
1. **CLI Prompt**: Display `(A)auto-approve <scope>` as a choice. Pressing `A` returns `ConfirmDecision::AutoApprove(scope)`.
2. **TUI Modal**: Show `(A) auto-approve <scope>` option. Pressing `A` returns the variant.
3. **TUI Persistent Indicator**: The agent emits a `StreamEvent::AutoApproveRuleCreated { scope }` when a rule is created. The `RatatuiDashboard` handles this event to maintain its active rules list and renders it on screen.
