# `mini --result-format json` — Machine-Readable Run Result

**Issue:** #537  
**Status:** Implemented  
**Complexity tier:** S

## Problem

`max mini` is the entry point for single-task runs and the README's primary persona
("operators who own the SWE agent loop"). Yet on completion it emits nothing
machine-readable to stdout — only a `.traj.json` on disk plus tracing logs on stderr.
To learn the outcome, cost, or patch location a CI wrapper must reconstruct the
trajectory path and parse the full `mini-swe-agent-1.1` schema. The sweep path already
ships machine-readable summaries; this closes the single-task parity gap.

## Usage

```
max mini --task "<TASK>" --result-format json [OTHER FLAGS]
```

Add `--result-format json` to any existing `max mini` invocation. The flag is strictly
additive; omitting it preserves today's behaviour exactly.

### Flags

| Flag | Description |
|------|-------------|
| `--result-format text\|json` | Output format for the run result. Default: `text` (no change). |

## JSON output (`--result-format json`)

Exactly one schema-versioned JSON object is printed to stdout after a terminal run.
All human/log text (tracing, progress, stderr diagnostics) continues to go to stderr,
so stdout is clean JSON with no leading noise.

### Schema

```json
{
  "artifact_kind": "mini_result",
  "schema_version": { "major": 1, "minor": 10 },
  "outcome": "submitted",
  "exit_code": 0,
  "exit_outcome_class": "success",
  "total_cost_usd": 0.0012,
  "steps": 3,
  "input_tokens": 8420,
  "output_tokens": 213,
  "trajectory_path": "/runs/fix-the-bug.traj.json",
  "patch_path": null,
  "failure_category": null
}
```

### Field reference

| Field | Type | Notes |
|-------|------|-------|
| `artifact_kind` | string | Always `"mini_result"`. |
| `schema_version` | object | `{ "major": N, "minor": N }` — version when binary was built. |
| `outcome` | string \| null | From `trajectory.info.outcome` (e.g. `"submitted"`, `"error"`). |
| `exit_code` | integer | Numeric exit code the process produces (0, 7, …). |
| `exit_outcome_class` | string | Stable class label (e.g. `"success"`, `"verification_failure"`). |
| `total_cost_usd` | number \| null | Actual + baseline cost. `null` when not captured (deterministic/free runs). |
| `steps` | integer \| null | Number of agent steps taken. |
| `input_tokens` | integer \| null | Prompt tokens (`token_usage.prompt_tokens`). `null` when not captured. |
| `output_tokens` | integer \| null | Completion tokens (`token_usage.completion_tokens`). `null` when not captured. |
| `trajectory_path` | string | Absolute path to the written `.traj.json` file. |
| `patch_path` | string \| null | Absolute path to the `.patch` file, or `null` when no patch was captured (no `--open-pr` flags). |
| `failure_category` | string \| null | Machine-readable failure category (see `FailureCategory` enum). `null` on success. |

### Emission scope

The JSON object is emitted **only** for:

1. **Submitted runs** — `mini::run` returns `Ok(())` and `trajectory.info.outcome == "submitted"`.
2. **Verification-failure runs** — `mini::run` returns `Err(VerificationFailed)` (exit code 7).

It is **not** emitted for:
- Runs that return `Ok(())` but did not submit (step-limit, budget-exhausted, stagnation, user-interrupt).
  These are operator-actionable signals; the trajectory is the authoritative record.
- Hard errors before a trajectory exists (env setup, model API failure, pre-trajectory I/O).
  For those the process exits with the appropriate `outcome_class` printed to stderr.

### Redaction guarantee

All string fields in the JSON object pass through the active `Redactor` policy
(same surface as `trajectory` artifacts — consistent with `agent skills-preview --format json`).
Numbers, booleans, `schema_version`, and JSON structure keys are never altered.
This guarantees that `secret_literals` and `custom_patterns` configured in
`[redaction]` cannot leak via stdout.

### Schema compatibility

The `mini_result` artifact follows the same additive-only minor-version contract as all
other harness artifacts. New fields may be added in minor releases; consumers must accept
unknown additive fields in the current major version.

## Exit codes

| Code | Class | Condition |
|------|-------|-----------|
| `0` | `success` | Agent submitted and all verify checks passed (or no checks configured). |
| `4` | `task_unsuccessful` | Agent hit step limit, budget, or env error. No JSON emitted. |
| `7` | `verification_failure` | Agent submitted but at least one `--verify` check failed. JSON emitted. |
| `12` | `agent_stagnation` | Stagnation detected. No JSON emitted. |

## Falsifiable success metric

```sh
max mini --task "fix the typo" --result-format json | jq -e '.outcome'
```

Exits 0 and prints the outcome string in one invocation, with zero post-run file lookups.

## Out of scope

- Per-step streaming (already served by `--stream`, `--event-log`, `--webhook-url`).
- Multi-run listing / summarisation (tracked by #509).
- Any change to the on-disk `mini-swe-agent-1.1` trajectory schema.
- A JSON result for the `bench` sweep path (already has `bench tail --once --format json`).
