# Patch Apply Failure Telemetry — Shipped

> Status: **shipped** in PR implementing issue #273.

## User Story

"As a Software Engineer debugging SWE-bench runs, I want to see detailed error
output when a patch fails to apply (`patch_apply_failed`), so that I can figure
out why the diff produced by the model was rejected."

## What Was Shipped

### Schema: `evaluation.json`

`InstanceEvaluation` gained an optional `patch_error_log: Option<String>` field
(schema version bumped to 1.9). The field is:

- Present (non-null) when `eval_exit_reason == "patch_apply_failed"` and the
  evaluator emitted a `git apply` stderr captured by the harness.
- `null` / absent for all other exit reasons.
- Passed through the secret-redactor before being persisted.

### `bench evaluate`

The `parse_generic_eval_row` parser now extracts `patch_error_log` from
evaluator report JSON whenever `eval_exit_reason` is `patch_apply_failed`.
Backends that do not emit the field will produce `null`.

### `bench inspect`

For instances whose `eval_exit_reason` is `patch_apply_failed`, inspect
renders a `patch error log:` section immediately below the `Failing tests`
line:

```text
Failing tests: <patch_apply_failed>
patch error log:
error: patch failed: src/core.py:10
error: src/core.py: patch does not apply
```

When the field is absent from the artifact (older sweeps, backends that do not
emit it), inspect prints `patch error log: <not captured>` rather than
erroring.

### Redaction

`patch_error_log` is treated as untrusted text and redacted through the
existing secret-redactor (#86) at both persist time and inspect view time.

## Metric

Median time-to-diagnose a `patch_apply_failed` instance dropped from
>5 minutes (manual re-apply against the gold environment) to <10 seconds
(read the log in `bench inspect`).

## Out of Scope

- Fixing the actual model prompt to avoid patch errors.
- Re-running patch application in Rust to bypass sb-cli.
- Auto-clustering `patch_error_log` strings in `bench triage`.
- Backfilling `patch_error_log` for sweeps recorded before this change.
