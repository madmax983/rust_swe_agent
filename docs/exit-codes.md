# CLI Exit-Code Contract

`max` emits a stable, documented process exit code for every
terminal outcome so CI and orchestration scripts can react correctly without
parsing human-oriented output.

## Outcome Classes

| Code | Outcome Class            | When emitted |
|-----:|--------------------------|--------------|
| 0    | `success`                | Command completed with no errors; all checks passed. Also covers no-op success (e.g. `bench doctor` with every preflight check green). |
| 1    | `internal_error`         | Unexpected failure: I/O error, JSON parse failure, unclassified panic. Treat as infrastructure broken, not a domain result. |
| 2    | `usage_error`            | Bad flag, missing required argument, unknown enum value, or invalid config file. Fix the invocation before retrying. |
| 3    | `preflight_failure`      | Dependency unavailable at sweep start: Docker not installed, Docker daemon unreachable, container failed to start, or model endpoint probe failed. |
| 4    | `task_unsuccessful`      | The agent ran but did not produce a usable result: step limit reached, environment command failed, wallclock timeout, or repeated model API errors. |
| 5    | `budget_halt`            | Cost ceiling triggered: `bench forecast --fail-over-cap` projected an over-cap run, or the sweep stopped because `--sweep-cost-limit-usd` was reached and no new tasks were dispatched. |
| 6    | `regression_gate_failure`| `bench compare --max-regressions` or `--max-patch-size-regression` threshold was exceeded. |
| 7    | `verification_failure`   | One or more `--verify NAME:COMMAND` checks did not pass after a `mini` run, or `bench bundle` detected a redaction retrigger / archive verification mismatch. |
| 8    | `calibration_optimistic`     | `bench calibrate --fail-on-optimistic` found actual sweep metrics above the forecast interval. |
| 9    | `replay_prompt_drift`        | `bench replay` detected that at least one input fingerprint does not match the cassette. |
| 10   | `replay_response_exhausted`  | `bench replay` ran out of scripted responses before the agent finished (structural drift). |
| 11   | `systemic_halt`              | `bench swebench` mid-sweep circuit breaker tripped: at least N completed instances share the same actionable failure category at or above the configured share threshold. |
| 12   | `agent_stagnation`           | The agent repeated the same action at least K times within a trailing window of W steps. |
| 13   | `env_preview_warning`        | `agent env preview` found at least one risky finding (wide host path, sensitive env var forwarded to agent, MCP server outside workdir, etc.). The preview itself was printed; the non-zero exit signals the operator should review findings before running a sweep. |
| 130  | `interrupted`            | Graceful SIGINT / Ctrl-C cancellation (POSIX convention: 128 + SIGINT(2)). |
| 137  | `killed`                 | SIGKILL escalation after the graceful-cancel deadline expired (128 + SIGKILL(9)). |

## Human-Readable Output

On any non-zero exit, the CLI writes two lines to **stderr** before command-specific
detail:

```
outcome_class: <class>
error: <message>
```

Example for a failed `--verify` check:

```
outcome_class: verification_failure
error: verification failed: 1 of 2 check(s) did not pass
```

## Machine-Readable Output

Commands that accept `--format json` write the outcome class to **stderr** using
the same two-line format above. JSON consumers can parse the `outcome_class`
line without inspecting the integer exit code.

For sweep commands the per-instance `failure_category` field in trajectory
JSON files carries fine-grained information; the process exit code gives the
coarse sweep-level result.

## Per-Command Outcome Class Table

| Command                         | Possible outcome classes |
|---------------------------------|--------------------------|
| `mini`                          | `success`, `usage_error`, `preflight_failure`, `task_unsuccessful`, `verification_failure`, `internal_error` |
| `replay`                        | `success`, `usage_error`, `replay_prompt_drift`, `replay_response_exhausted`, `task_unsuccessful`, `internal_error` |
| `bench swebench`                | `success`, `usage_error`, `preflight_failure`, `budget_halt`, `internal_error`, `interrupted`, `killed` |
| `bench forecast`                | `success`, `usage_error`, `budget_halt`, `internal_error`, `interrupted` |
| `bench calibrate`               | `success`, `usage_error`, `calibration_optimistic`, `internal_error` |
| `bench doctor`                  | `success`, `usage_error`, `preflight_failure`, `internal_error` |
| `bench compare`                 | `success`, `usage_error`, `regression_gate_failure`, `internal_error` |
| `bench evaluate`                | `success`, `usage_error`, `internal_error` |
| `bench inspect`                 | `success`, `usage_error`, `internal_error` |
| `bench tail`                    | `success`, `usage_error`, `internal_error` |
| `bench triage`                  | `success`, `usage_error`, `internal_error` |
| `bench frontier`                | `success`, `usage_error`, `internal_error` |
| `bench bundle`                  | `success`, `usage_error`, `verification_failure`, `internal_error` |
| `agent env preview`             | `success`, `usage_error`, `env_preview_warning`, `internal_error` |

> **Note:** `hello-world` does not produce distinct outcome classes beyond
> `success` / `internal_error`; it is an interactive debugging surface.
>
> **Future surfaces:** `doctor` (standalone) and verification surfaces must
> extend this table before merging.

## Shell Examples

### Routing outcomes in CI

```bash
#!/usr/bin/env bash
set -uo pipefail

# Run in a conditional so the exit status is captured even under `set -e`.
max bench compare \
  --baseline runs/before \
  --candidate runs/after \
  --max-regressions 0 \
  || exit_code=$?
exit_code=${exit_code:-0}

case $exit_code in
  0)   echo "Gate passed — no regressions" ;;
  6)   echo "Regression gate FAILED — review bench compare output" ; exit 1 ;;
  2)   echo "Config error — fix the invocation" ; exit 1 ;;
  1)   echo "Infrastructure failure — retry or escalate" ; exit 1 ;;
  *)   echo "Unexpected exit code $exit_code" ; exit 1 ;;
esac
```

### Handling preflight / config failure

```bash
max bench swebench --dataset lite --output runs/
exit_code=$?
if [ $exit_code -eq 3 ]; then
  echo "Preflight failed — is Docker running?"
  exit 1
fi
```

### Handling budget halt

```bash
max bench forecast --dataset lite --output /tmp/forecast \
  --fail-over-cap --sweep-cost-limit-usd 50
exit_code=$?
if [ $exit_code -eq 5 ]; then
  echo "Forecast exceeds cost cap — increase cap or reduce dataset"
  exit 1
fi
```

### Gating optimistic calibration

```bash
max bench calibrate \
  --forecast runs/forecast.json \
  --results runs/sweep/results.json \
  --fail-on-optimistic
exit_code=$?
if [ $exit_code -eq 8 ]; then
  echo "Forecast was optimistic - increase calibration or budget before the next run"
  exit 1
fi
```

### Handling interrupted sweep

```bash
# A Ctrl-C during a sweep exits 130; treat as a known outcome, not error.
# Capture exit status before || true masks it.
max bench swebench --dataset lite --output runs/
exit_code=$?
if [ $exit_code -eq 130 ] || [ $exit_code -eq 137 ]; then
  echo "Sweep was cancelled — partial results in runs/"
fi
```

### Handling verification failure

```bash
max mini --task "Fix the bug" \
  --verify "unit-tests:cargo test -q" \
  --verify "lint:cargo clippy -- -D warnings"
exit_code=$?
if [ $exit_code -eq 7 ]; then
  echo "Verification failed — agent output did not pass checks"
  exit 1
fi
```

## Compatibility Policy

- **Assigned codes are stable.** Renumbering a published outcome class is a
  **breaking change** and requires a major version bump.
- **New outcome classes** may be introduced with previously-unused numbers in a
  minor release. Scripts must treat an unknown non-zero code as `internal_error`
  (code 1).
- **Outcome class strings** (e.g. `regression_gate_failure`) are stable and
  follow the same breaking-change rule as integer codes.
- **`success` is always 0.** No other outcome class will ever use 0.
- **130 and 137** are reserved for signal-based termination and will not be
  reused for domain outcomes.
