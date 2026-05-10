# CLI Exit-Code Contract

`rust-swe-agent` emits a stable, documented process exit code for every
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
| 7    | `verification_failure`   | One or more `--verify NAME:COMMAND` checks did not pass after a `mini` run. |
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
| `bench swebench`                | `success`, `usage_error`, `preflight_failure`, `budget_halt`, `internal_error`, `interrupted`, `killed` |
| `bench forecast`                | `success`, `usage_error`, `budget_halt`, `internal_error`, `interrupted` |
| `bench doctor`                  | `success`, `usage_error`, `preflight_failure`, `internal_error` |
| `bench compare`                 | `success`, `usage_error`, `regression_gate_failure`, `internal_error` |
| `bench evaluate`                | `success`, `usage_error`, `internal_error` |
| `bench inspect`                 | `success`, `usage_error`, `internal_error` |
| `bench tail`                    | `success`, `usage_error`, `internal_error` |
| `bench frontier`                | `success`, `usage_error`, `internal_error` |

> **Legacy note:** `hello-world` and `replay` do not yet produce distinct
> outcome classes beyond `success` / `internal_error`; they are interactive or
> debugging surfaces not intended for CI automation.
>
> **Future surfaces:** `doctor` (standalone) and verification surfaces must
> extend this table before merging.

## Shell Examples

### Routing outcomes in CI

```bash
#!/usr/bin/env bash
set -uo pipefail

# Run in a conditional so the exit status is captured even under `set -e`.
rust-swe-agent bench compare \
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
rust-swe-agent bench swebench --dataset lite --output runs/
exit_code=$?
if [ $exit_code -eq 3 ]; then
  echo "Preflight failed — is Docker running?"
  exit 1
fi
```

### Handling budget halt

```bash
rust-swe-agent bench forecast --dataset lite --output /tmp/forecast \
  --fail-over-cap --sweep-cost-limit-usd 50
exit_code=$?
if [ $exit_code -eq 5 ]; then
  echo "Forecast exceeds cost cap — increase cap or reduce dataset"
  exit 1
fi
```

### Handling interrupted sweep

```bash
# A Ctrl-C during a sweep exits 130; treat as a known outcome, not error.
# Capture exit status before || true masks it.
rust-swe-agent bench swebench --dataset lite --output runs/
exit_code=$?
if [ $exit_code -eq 130 ] || [ $exit_code -eq 137 ]; then
  echo "Sweep was cancelled — partial results in runs/"
fi
```

### Handling verification failure

```bash
rust-swe-agent mini --task "Fix the bug" \
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
