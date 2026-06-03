# Spec: `--driver codex` (OpenAI Codex as the agent backend)

Status: experimental (exploration slice).

## Problem

Same motivation as `--driver claude-code` (see `spec-claude-driver.md`): can a
more capable coding agent drive a task while still producing the same inspectable
trajectory, patch, cost accounting, and verification output as the built-in loop?

This slice answers "yes" for the **OpenAI Codex CLI** (`codex`), which runs
tasks in `--full-auto --json` mode. It mirrors the seam design of the Claude Code
driver: the driver swaps only the loop that fills the trajectory; everything else
in `mini::run` — provenance manifest, `git diff` patch capture, `--verify`
verification, redaction, and the final trajectory write — is identical.

## Surface

```
max mini --driver codex --task "<task>" --workdir <repo> --output <dir>
```

- `--driver builtin` (default): the native bash-only loop. Unchanged.
- `--driver claude-code`: drive the `claude` CLI. Unchanged.
- `--driver codex`: drive the `codex` CLI. **Local environment only.**

## How it works

1. `mini::run` builds the usual `DefaultAgent`, seeding the trajectory with the
   system + task messages.
2. `run/codex_driver.rs` spawns:

   ```
   codex --full-auto --json --max-turns <step_limit> "<task>"
   ```

   with the child's working directory set to `--workdir` (or the process cwd).

3. It reads the NDJSON stream and records:
   - `session` → captures `session_id` and `model` for provenance.
   - `reasoning` → thinking text, folded into an assistant turn's
     `extra.other["thinking"]`.
   - `local_shell_call` → one agent step; the `action.command` is recorded as
     an `extra.actions` label (redacted on the trajectory surface) and the step
     counter is incremented.
   - `local_shell_call_output` → observation message carrying a synthesized
     `extra.run_result` (`exit_code` from `output.exit_code`, output as `stdout`)
     so consumers that key off it (`bench inspect`, command stats, output-byte
     telemetry) treat codex driver runs like built-in ones.
   - `message` (role `assistant`) → assistant text turn, redacted.
   - `completed` → terminal event: exit reason, `result` string, `cost_usd`,
     and `usage.{input_tokens, output_tokens}`.

4. Patch capture runs exactly as for the built-in loop: because Codex edits the
   real working tree, `git diff` sees the change regardless of whether the edit
   came from a shell command or a file-write operation.

## Stream format

Codex emits newline-delimited JSON events in `--full-auto --json` mode:

```jsonc
// Init
{"type":"session","session_id":"sess-abc123","model":"o4-mini"}

// Thinking (optional)
{"type":"reasoning","content":[{"type":"thinking","text":"..."}]}

// Shell tool call
{"type":"local_shell_call","id":"lsc_1","action":{"type":"exec","command":"ls","timeout":30000,"working_directory":"."}}

// Shell tool result
{"type":"local_shell_call_output","id":"lsc_1","output":{"type":"exec_result","output":"file.txt\n","exit_code":0,"metadata":{}}}

// Assistant message
{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}

// Terminal
{"type":"completed","exit_reason":"done","result":"Done.","cost_usd":0.05,"usage":{"input_tokens":100,"output_tokens":50}}
```

## Outcome mapping

| Codex `completed.exit_reason` | Trajectory outcome       | `ExitReason`          |
| ----------------------------- | ------------------------ | --------------------- |
| `"done"` or `"success"`       | `submitted`              | `Submitted`           |
| `"max_turns"`                 | `step_limit_reached`     | `StepLimit`           |
| other                         | `error`                  | (Err, finalized)      |
| no `completed` event          | `error`                  | (Err, finalized)      |
| wallclock timeout             | `error` (`wallclock_timeout`) | (Err, finalized) |

`done`/`success` is treated as a submission: the `completed.result` string is
the `final_output` (falling back to the last assistant text if empty).

## Step counting

Each `local_shell_call` event increments the step counter. This is the closest
analog to the built-in loop's one-action-per-turn semantics. `--max-turns` passed
to Codex bounds its own agentic turns; if the parsed shell-call count exceeds
`agent.step_limit`, the outcome is downgraded to `step_limit` regardless of what
the `completed` event says.

## Cost & provenance

- Cost is taken verbatim from `completed.cost_usd` and recorded with
  `actual_cost_source = provider_reported`.
- Token usage is read from `completed.usage` (`input_tokens`, `output_tokens`).
- `info.model_name` is overwritten with the model Codex actually used (from the
  `session` event or a per-message `model` field).
- A `codex_driver` block under `info.other` records `{driver, session_id}`.

## Toolset recording

`info.other["toolset"]` is overwritten with the Codex `shell` tool so
tool-coverage/drift reports see the real available set rather than the harness
bash-only manifest.

## Safety-contract enforcement

Identical to the Claude Code driver — all the same guards apply since Codex also
runs tools itself and cannot be instrumented mid-action:

- **`--read-only`** rejected.
- **Interactive confirmation** rejected.
- **Policy profile** must be `yolo`; custom `extra_deny_patterns` /
  `extra_allow_patterns` rejected.
- **Cost caps** re-checked post-hoc: `cost_limit_usd` records `CostLimit`;
  `per_task_budget_usd` records `BudgetExhausted`.
- **Cancellation** raced against the stream; child is killed on cancel.
- **`--resume` / `--continue`** rejected.
- **`agent.hooks`** rejected.
- **`environment.chaos_fail_every`** rejected.
- **`agent.mcp_servers`** rejected.
- **`agent.detect_stagnation`** rejected.
- **`environment.timeout_secs` (non-default)** rejected.
- **Budget-visibility** (`per_task_budget_usd` + `hide_budget_from_agent=false`)
  rejected.
- **`agent.tools`** rejected.
- **`--env docker`** rejected (Codex edits the host tree; no path into a
  container).

## Testing

The `codex` binary path is overridable via `MAXWELLS_CODEX_BIN`. `tests/codex_driver.rs`
points it at a fixture shell script that makes a real edit and emits a canned
NDJSON transcript, exercising the full spawn → parse → finalize → trajectory path
deterministically at $0. The test suite covers: trajectory validity, docker/
read-only/interactive/policy/chaos rejection guards, pre-submit test telemetry,
budget downgrade, stagnation/MCP/resume/timeout rejection, and step-overflow
downgrade.

## Constraints & non-goals (this slice)

- **Local environment only.** `--driver codex --env docker` is rejected fast.
- **Model selection is delegated to Codex.** `--model` is not forwarded.
- **No `--resume` / `--continue`.** Those paths stay on the built-in loop.
- **No isolated/fidelity modes.** Unlike the Claude Code driver, Codex has no
  equivalent of `--bare`; there is a single posture (fidelity).
- **No `--append-system-prompt` equivalent.** Codex's system prompt handling is
  internal; the harness does not forward the operator system prompt.
