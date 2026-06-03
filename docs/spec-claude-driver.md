# Spec: `--driver claude-code` (Claude Code as the agent backend)

Status: experimental (exploration slice).

## Problem

The harness ships a deliberately small bash-first agent loop that calls a
`Model` directly. That loop is great for measurement, but it is a weak coder
compared to a full agent like Claude Code. Operators have asked the obvious
question: *can a more capable coding agent drive a task, while we still get the
same inspectable trajectory, patch, cost accounting, and verification we get
from the built-in loop?*

This slice answers "yes" by adding a second **driver** for `mini` that shells
out to the Claude Code CLI (`claude`) in headless `stream-json` mode and
translates its message stream into a standard `mini-swe-agent-1.3` trajectory.

## Surface

```
max mini --driver claude-code --task "<task>" --workdir <repo> --output <dir>
```

- `--driver builtin` (default): the native bash-only loop. Unchanged.
- `--driver claude-code`: drive the `claude` CLI. **Local environment only.**

The driver swaps *only* the loop that fills the trajectory. Everything around
it in `mini::run` — provenance manifest, `git diff` patch capture, `--verify`
verification, redaction, and the final trajectory write — is identical for both
backends. As a result the Claude-Code path produces the same artifacts:
`*.traj.json`, `*.output.txt`, and (when a patch spec is supplied) `*.patch`.

## How it works

1. `mini::run` builds the usual `DefaultAgent`, which owns the environment and
   redactor and seeds the trajectory with the system + task messages.
2. Instead of running the built-in loop, `run/claude_driver.rs` spawns:

   ```
   claude -p "<task>" \
     --output-format stream-json --verbose \
     --max-turns <step_limit> \
     --allowedTools "Bash Edit MultiEdit Write Read Glob Grep NotebookEdit" \
     [--append-system-prompt "<rendered system prompt>"]  # when --driver-append-system-prompt
     [--bare --no-session-persistence --tools "<set>"]    # when --driver-isolated
   ```

   with the child's working directory set to `--workdir` (or the process cwd).
3. It reads the newline-delimited JSON stream and records:
   - `assistant` turns → assistant messages, with `tool_use` calls captured in
     `extra.actions` (bash command verbatim; other tools as `Name(target)`),
     and thinking folded into `extra.thinking`.
   - `tool_result` (delivered as a `user` message) → observation messages.
   - the terminal `result` message → cost, tokens, outcome.
4. Patch capture then runs exactly as for the built-in loop: because Claude Code
   edits the real working tree, `git diff` sees the change regardless of whether
   the edit came from `Bash`, `Edit`, or `Write`.

## System prompt forwarding

By default the driver does NOT forward the harness's operator system prompt to
Claude Code. The built-in default `[prompts].system` template contains
bash-block-protocol instructions (`COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT`, etc.)
that Claude Code's own loop already handles differently, so forwarding the
default verbatim would add confusing noise.

Use `--driver-append-system-prompt` to opt in:

```
max mini --driver claude-code --driver-append-system-prompt \
  --config my_cc_config.toml --task "..."
```

When set, the rendered operator system prompt (with any `[prompts].system`
override applied) is forwarded to `claude --append-system-prompt`. This is the
CORE use-case for teams that have Claude Code enterprise access but no direct
API keys: measuring system prompt / skills / tools changes while still getting
the harness's full trajectory artifact.

Pair this with a CC-compatible `[prompts].system` override in your config. The
built-in default contains harness-specific protocol text; an override intended
for Claude Code should omit or replace that.

## Fidelity vs. isolation

The driver has two postures, because it serves two different jobs:

- **Fidelity (default).** Run Claude Code as the team really runs it: OAuth /
  keychain auth, ambient `.claude` auto-discovery (hooks, skills, plugins, MCP,
  memory, `CLAUDE.md`), native session persistence, and the team's own
  permission settings. This is the enterprise-auditing case — teams with
  enterprise Claude Code access who need an inspectable, redacted receipt of
  what their agent actually did. Nothing is stripped; instead the harness
  *records* the config that shaped the run (see **Config discovery** below).

- **Isolation (`--driver-isolated`).** A reproducible measurement sandbox. Adds
  `--bare` (skip ambient `.claude` discovery), `--tools "<ALLOWED_TOOLS>"`
  (restrict the *available* toolset, not just auto-approval), and
  `--no-session-persistence` (keep prompts/transcripts out of Claude Code's
  on-disk history). Use this to vary one thing — system prompt, skills, tools —
  and hold everything else fixed.

  **Auth caveat:** `--bare` forces auth to be strictly `ANTHROPIC_API_KEY` /
  `apiKeyHelper`; OAuth and keychain logins are never read. Isolated runs
  therefore require an API key. (This is why isolation is opt-in, not the
  default — the primary audience uses enterprise OAuth.)

## Config discovery

In fidelity mode, ambient configuration influences the run, so the harness
discovers and records it into `info.other["claude_code_config"]` for audit:

- **Project scope** (workdir): `CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.md`,
  `AGENTS.md`, `.claude/settings.json`, `.claude/settings.local.json`,
  `.mcp.json`, and every file under `.claude/agents/`, `.claude/skills/`, and
  `.claude/rules/`.
- **Ancestor scope**: `CLAUDE.md`/`CLAUDE.local.md` in directories above the
  workdir (Claude Code loads project memory up the hierarchy).
- **User scope** (`~/.claude`): `CLAUDE.md`, `settings.json`, and the `agents/`
  and `skills/` trees.

Each entry records `{path, scope, kind, sha256, bytes, content[, truncated]}`.
The `sha256` is streamed over the *whole* on-disk file (a tamper-evident
fingerprint, constant memory); `content` is the file body **redacted** with the
`TRAJECTORY` surface and then truncated at 256 KiB (redact-before-truncate, so a
secret straddling the cap is still caught). `path` is redacted too — a workdir or
`$HOME` segment can itself be a configured secret. To stay memory-safe against a
large or symlinked asset under `.claude/`, at most 1 MiB is buffered per file for
content/redaction (the hash still covers the whole file), symlinked directory
entries are skipped (no traversal outside the worktree), and at most 200 files
are recorded. In isolated mode `--bare` bypasses discovery, so only
`{isolated: true, discovery: "bypassed…"}` is recorded.

## Outcome mapping

| Claude Code `result`            | Trajectory outcome      | `ExitReason`         |
| ------------------------------- | ----------------------- | -------------------- |
| `subtype == "success"`          | `submitted`             | `Submitted`          |
| subtype contains `max_turns`    | `step_limit_reached`    | `StepLimit`          |
| other / `is_error`              | `error`                 | (Err, finalized)     |
| no `result` line (crash/kill)   | `error`                 | (Err, finalized)     |
| wallclock timeout               | `error` (`wallclock_timeout`) | (Err, finalized) |

`success` is treated as a submission: the working-tree diff is the patch and
`result.result` is the `final_output`.

## Safety-contract enforcement

The driver delegates execution to a CLI it cannot instrument mid-action, so it
enforces the harness's safety contracts at the boundaries instead:

- **`--read-only`** is rejected (the CLI auto-allows `Bash`/`Edit`/`Write`, so
  it cannot offer an analysis-only guarantee).
- **Interactive confirmation** (`--interactive` / `--ui`) is rejected — the
  per-action operator confirmer lives inside `DefaultAgent` and cannot gate
  Claude's tool calls.
- **Policy profile** must be `yolo`. The built-in `safe`/`ask` deny corpus is
  enforced inside `DefaultAgent::step` and never fires when the external CLI
  owns tool execution. Operators must explicitly set `policy.profile = "yolo"`
  in config to acknowledge this. Custom `extra_deny_patterns` /
  `extra_allow_patterns` are also rejected for the same reason.
- **Cost caps** (`agent.cost_limit_usd` / `--per-task-budget-usd`) are forwarded
  to Claude Code's `--max-budget-usd`, and re-checked post-hoc: `cost_limit_usd`
  records `CostLimit` (matching the built-in loop's priority); `per_task_budget_usd`
  records `BudgetExhausted`. Either prevents a `submitted` outcome.
- **Cancellation** (sweep Ctrl-C via the run's cancellation token) is raced
  against the stream; when it fires the child is killed and the run is finalized
  as an interrupt, matching the built-in loop.
- **`--resume` / `--continue`** are rejected — the external CLI cannot be seeded
  with prior message history; those paths stay on the built-in loop.
- **Merged `--extra-context` / active-skill guidance** is appended to the prompt
  Claude Code receives, so the backend actually sees the context the trajectory
  records.
- **Tool-action labels** in `extra.actions` are redacted on the trajectory
  surface, the same as the built-in path.

## Cost & provenance

- Cost is taken verbatim from `result.total_cost_usd` and recorded with
  `actual_cost_source = provider_reported`.
- Token usage is read from `result.usage`
  (`input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`,
  `output_tokens`).
- `info.model_name` is overwritten with the model Claude Code actually used
  (from `system/init` / per-message `model`), and a `claude_driver` block under
  `info.other` records `{driver, session_id, claude_code_version}`.

## Constraints & non-goals (this slice)

- **Local environment only.** `--driver claude-code --env docker` is rejected
  fast: the CLI edits the host tree and has no path into a container.
- **Model selection is delegated to Claude Code.** `--model` is *not* forwarded;
  the actually-responding model is recorded back into the trajectory. A warning
  is logged when `--model` is set.
- **No `--resume` / `--continue`.** Those paths stay on the built-in loop.
- **Step semantics differ.** `steps` counts tool invocations, the closest analog
  to the built-in loop's one-action-per-turn step. It is not Claude Code's
  `num_turns`, and the harness does not hard-stop the run when the tool-use
  count crosses `step_limit` (Claude Code's `--max-turns` is the active bound).

## Testing

The `claude` binary path is overridable via the `MAXWELLS_CLAUDE_BIN`
environment variable. `tests/claude_driver.rs` points it at a fixture shell
script that makes a real edit and emits a canned `stream-json` transcript
(shapes captured from a real `claude` v2.1 run), exercising the full
spawn → parse → finalize → trajectory path deterministically at $0. The test
suite covers: trajectory validity, docker/read-only/interactive/policy/chaos
rejection guards, pre-submit test telemetry, budget downgrade, non-yolo policy
rejection, and resume rejection.
