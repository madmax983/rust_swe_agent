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
| 11   | `systemic_halt`          | `bench swebench` mid-sweep circuit breaker tripped: at least N completed instances share the same actionable failure category at or above the configured share threshold. |
| 12   | `agent_stagnation`       | The agent repeated the same action at least K times within a trailing window of W steps. |
| 13   | `env_preview_warning`    | `agent env preview` found at least one risky finding (wide host path, sensitive env var forwarded to agent, MCP server outside workdir, etc.). The preview itself was printed; the non-zero exit signals the operator should review findings before running a sweep. |
| 14   | `skills_preview_warning` | `agent skills-preview` completed but found at least one warning: a task hit `max_active`, an activated manifest has no `version` field, or `auto_load = true` resolved an implicitly-matched skill. See `docs/spec-skills-preview.md`. |
| 15   | `resume_already_terminal` | `mini --resume` target trajectory already has a terminal outcome (`submitted`, `error`, `step_limit_reached`, `budget_exhausted`, `cancelled`, `wallclock_timeout`). Cannot continue a run that already completed. |
| 16   | `resume_manifest_missing` | `mini --resume` target trajectory is missing required fields (`task` and/or `model_name`). The file may pre-date the run-manifest schema; create a fresh run instead. |
| 17   | `resume_invalid_prefix` | `mini --resume` target trajectory is structurally invalid for resume: message sequence is empty, too short (fewer than 2 messages), or ends in a partial assistant turn. |
| 21   | `eval_gaming_gate_failure` | `bench compare --max-test-only-resolved-rate` threshold was exceeded by the candidate sweep. |
| 22   | `artifact_integrity_violation` | `bench export-ci` detected that JUnit XML aggregate attributes do not match `results.json` counts. The export wrote whatever it had; the mismatch signals a corrupt or incomplete sweep artifact. |
| 23   | `scriptability_check_failure` | `bench scriptability-check` found at least one misconfigured MCP server or hook. Zero model calls were made; this is a wiring preflight. Distinct from `preflight_failure` (3) so CI can route scriptability misconfig separately from infrastructure failures. |
| 24   | `feature_unavailable`    | A subcommand requires a Cargo feature that was not compiled in. The error message names the missing feature and the relevant docs link. |
| 25   | `redact_check_stale_literals` | `agent redact-check` found that at least one configured `secret_literals` entry produced zero matches against the sample input. The literal is likely stale and may not be protecting anything. |
| 26   | `redact_check_strict_fail` | `agent redact-check --strict` found that at least one `custom_patterns` entry compiled successfully but produced zero matches. Use to catch regex typos or renamed token formats in CI. Exit 26 takes priority over exit 25 when both conditions hold. |
| 27   | `slo_rule_failure`       | `bench assert` evaluated all rules and at least one rule (or missing-artifact in fail-closed mode) did not pass. The command ran correctly and wrote `assertions.json`; the non-zero exit means the sweep did not meet the declared SLO. Distinct from `usage_error` (2) so CI can distinguish "your gate failed" from "your invocation is broken". |
| 28   | `continue_non_terminal` | `mini --continue` target trajectory is non-terminal (still partial/in-progress). Use `--resume` to continue an in-progress run; `--continue` only accepts terminal trajectories (`submitted`, `error`, `step_limit_reached`, `budget_exhausted`, `cancelled`, `wallclock_timeout`). |
| 29   | `apply_check_failed`     | `agent apply` ran `git apply --check` and the patch cannot be applied cleanly to the current tree. The working tree is left unchanged. |
| 30   | `apply_redacted_refused` | `agent apply` detected `[REDACTED:…]` markers in the patch content or the source trajectory recorded patch-submission redaction. Pass `--allow-redacted` to override. |
| 31   | `apply_dirty_tree_refused` | `agent apply` found uncommitted changes in the target working tree. Pass `--allow-dirty` to override. |
| 32   | `redact_audit_findings` | `agent redact-audit` found at least one new finding at `medium` or higher severity. Suitable as a publish gate (`redact-audit <dir> && publish`). Distinct from a scan error so CI can route a leak vs. an incomplete scan. |
| 33   | `redact_audit_scan_error` | `agent redact-audit` could not read or extract an artifact (unreadable file/subtree, corrupt bundle member), so the scan is incomplete and a clean verdict cannot be trusted. Findings take precedence: exit 32 is returned instead when any new finding is also present. |
| 34   | `github_issue_missing_token` | The `GITHUB_TOKEN` environment variable was empty or missing. |
| 35   | `github_issue_not_found` | The API returned `404 Not Found` (or a `403` indicating a private/unauthorized repository). |
| 36   | `github_issue_rate_limited` | The GitHub API returned a `403 Rate Limit Exceeded` status. |
| 37   | `injection_audit_hits` | `agent injection-audit` found at least one hit at or above the configured `--fail-on` severity threshold. The audit completed; use as a publish gate (`injection-audit <dir> && publish`). Distinct from `internal_error` (1) so CI can route "injection signals found" separately from a crash. |
| 38   | `injection_audit_scan_error` | `agent injection-audit` could not read the sweep directory, a trajectory file, or a subdirectory of the sweep (missing path, unreadable file, mid-stream walk failure). The scan is incomplete so a clean verdict cannot be trusted. Hits take precedence: exit 37 is returned instead when any actionable hit is also present. |
| 39   | `stability_gate_failure` | `agent stability --fail-under <F>` found `pass_at_k < F`. All runs completed; the gate is wired correctly but the measured pass rate did not meet the declared threshold. |
| 40   | `dataset_verify_mismatch` | `bench dataset-verify` detected a mismatch between the candidate dataset and the canonical reference. |
| 41   | `best_of_all_failed`     | `agent best-of` completed all runs and no run passed all verify checks. The best-scoring run was still selected and its patch emitted; `all_failed: true` is set in `best-of-results.json`. Pass `--allow-no-pass` to downgrade to exit 0 while keeping `all_failed: true`. |
| 42   | `config_override_warning` | `agent config resolve` detected at least one clap-default override hazard: a `--config` file sets a field (`model.name` or `agent.step_limit`) that a clap default in `mini` or `bench swebench` will silently overwrite unless the corresponding flag is also passed explicitly. The resolved config was printed; exit 0 when no hazards are detected. See `docs/spec-config-resolve.md`. |
| 43   | `eval_parity_gate_failure` | `bench eval-parity --min-agreement <F>` measured an offline-vs-canonical agreement rate below the declared threshold. The report was written; the non-zero exit gates CI on evaluator parity. Distinct from `slo_rule_failure` (27). |
| 44   | `utilization_gate_failure` | `bench utilization --min-utilization <PCT>` measured a concurrency utilization below the declared floor. The report was printed; the non-zero exit gates CI on sweep concurrency efficiency. Distinct from `slo_rule_failure` (27) so automation can route "concurrency under-utilized" separately from generic SLO failures. See `docs/spec-utilization.md`. |
| 45   | `fs_audit_findings`      | `agent fs-audit` found at least one bash command that accessed a path outside the configured workdir (absolute path, `..` traversal, `$HOME`/`~/` reference, or known system directory). The audit completed and the report was printed; use as a publish gate. Distinct from `internal_error` (1) so CI can route "filesystem boundary violated" separately from a crash. See `docs/spec-fs-audit.md`. |
| 46   | `fs_audit_scan_error`    | `agent fs-audit` could not read or parse one or more trajectory files (missing path, unreadable file, invalid JSON, missing sweep directory). The scan is incomplete, so a "clean" verdict cannot be trusted. Findings take precedence: exit 45 is returned when both findings and scan errors are present. |
| 47   | `artifact_check_failure` | `agent artifact-check` found at least one artifact that is `invalid` or `unsupported_major`. With `--strict`, also triggers on `legacy_unversioned` and `valid_with_warnings`. Zero model calls are made; the check is purely a structural conformance gate. Distinct from `internal_error` (1) so CI can route "artifact does not conform to contract" separately from an unexpected infrastructure failure. See `docs/spec-artifact-check.md`. |
| 48   | `host_not_ready`         | `agent doctor` found at least one failing host-readiness check: `git` missing from PATH, the provider credential env var absent, the Docker daemon unreachable when `--env docker` is selected, the runs/output dir not writable, or the active toolchain below the crate `rust-version`. Zero model calls and no provider network probe are made; the check is a pure host preflight. Skipped checks never trigger this. Distinct from `preflight_failure` (3) so CI can route "host not ready before any run" separately from sweep-time dependency failures. See `docs/spec-agent-doctor.md`. |
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
coarse sweep-level result.  See [`docs/failure-categories.md`](failure-categories.md)
for the complete reference of every `failure_category` value, its definition,
and the recommended triage action.

## Per-Command Outcome Class Table

| Command                         | Possible outcome classes |
|---------------------------------|--------------------------|
| `mini`                          | `success`, `usage_error`, `preflight_failure`, `task_unsuccessful`, `verification_failure`, `resume_already_terminal`, `resume_manifest_missing`, `resume_invalid_prefix`, `continue_non_terminal`, `github_issue_missing_token`, `github_issue_not_found`, `github_issue_rate_limited`, `internal_error` |
| `replay`                        | `success`, `usage_error`, `replay_prompt_drift`, `replay_response_exhausted`, `task_unsuccessful`, `internal_error` |
| `bench swebench`                | `success`, `usage_error`, `preflight_failure`, `budget_halt`, `internal_error`, `interrupted`, `killed` |
| `bench forecast`                | `success`, `usage_error`, `budget_halt`, `internal_error`, `interrupted` |
| `bench calibrate`               | `success`, `usage_error`, `calibration_optimistic`, `internal_error` |
| `bench doctor`                  | `success`, `usage_error`, `preflight_failure`, `internal_error` |
| `bench compare`                 | `success`, `usage_error`, `regression_gate_failure`, `eval_gaming_gate_failure`, `internal_error` |
| `bench evaluate`                | `success`, `usage_error`, `internal_error` |
| `bench inspect`                 | `success`, `usage_error`, `internal_error` |
| `bench tail`                    | `success`, `usage_error`, `internal_error` |
| `bench triage`                  | `success`, `usage_error`, `internal_error` |
| `bench frontier`                | `success`, `usage_error`, `internal_error` |
| `bench bundle`                  | `success`, `usage_error`, `verification_failure`, `internal_error` |
| `agent env preview`             | `success`, `usage_error`, `env_preview_warning`, `internal_error` |
| `agent skills-preview`          | `success`, `usage_error`, `skills_preview_warning`, `internal_error` |
| `bench scriptability-check`     | `success`, `usage_error`, `scriptability_check_failure`, `internal_error` |
| `agent redact-check`            | `success`, `usage_error`, `redact_check_stale_literals`, `redact_check_strict_fail`, `internal_error` |
| `agent redact-audit`            | `success`, `usage_error`, `redact_audit_findings`, `redact_audit_scan_error`, `internal_error` |
| `bench assert`                  | `success`, `usage_error`, `slo_rule_failure`, `internal_error` |
| `agent apply`                   | `success`, `usage_error`, `apply_check_failed`, `apply_redacted_refused`, `apply_dirty_tree_refused`, `internal_error` |
| `agent artifact-check`          | `success`, `usage_error`, `artifact_check_failure`, `internal_error` |
| `agent doctor`                  | `success`, `host_not_ready`, `usage_error`, `internal_error` |
| `bench export-otlp`             | `success`, `usage_error`, `preflight_failure`, `internal_error` (see `docs/spec-export-otlp.md`) |

> **Note:** `hello-world` does not produce distinct outcome classes beyond
> `success` / `internal_error`; it is an interactive debugging surface.
>
> **Future surfaces:** verification surfaces must extend this table before
> merging.

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
