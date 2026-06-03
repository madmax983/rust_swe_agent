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
     --allowedTools "Bash Edit MultiEdit Write Read Glob Grep NotebookEdit"
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
- **Cost caps** (`agent.cost_limit_usd` / `--per-task-budget-usd`) are forwarded
  to Claude Code's `--max-budget-usd`, and re-checked post-hoc: an over-budget
  run is recorded as `budget_exhausted`, never `submitted`.
- **Cancellation** (sweep Ctrl-C via the run's cancellation token) is raced
  against the stream; when it fires the child is killed and the run is finalized
  as an interrupt, matching the built-in loop.
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

### Deferred (not yet at built-in parity)

These built-in behaviors are not yet mirrored by the driver and remain open:

- **Pre-submit test telemetry** (`tests_run_before_submit`, `last_tests_passed`)
  — the built-in loop matches test-command patterns and records a
  `TestInvocation` per run; the driver only records the action label.
- **Per-step partial checkpoints** — the built-in loop writes a resumable
  partial trajectory after each step; the driver appends in memory and writes
  once at the end, so an interrupted long run loses streamed progress.

## Testing

The `claude` binary path is overridable via the `MAXWELLS_CLAUDE_BIN`
environment variable. `tests/claude_driver.rs` points it at a fixture shell
script that makes a real edit and emits a canned `stream-json` transcript
(shapes captured from a real `claude` v2.1 run), exercising the full
spawn → parse → finalize → trajectory path deterministically at $0, plus the
docker-rejection guard.
