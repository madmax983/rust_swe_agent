# Evaluator Provenance

`bench evaluate` stamps every `evaluation.json` artifact with a `provenance`
block describing *which* evaluator produced the results and *how*. This enables
`bench compare` to detect when two sweeps were scored by incompatible setups,
preventing misleading apples-to-oranges comparisons.

## What gets recorded

Every `evaluation.json` contains a top-level `provenance` object:

```json
{
  "artifact_kind": "evaluation_results",
  "provenance": {
    "backend": "sb-cli",
    "backend_version": "sb-cli 0.5.2",
    "dataset_subset": "swe-bench-m",
    "dataset_split": "dev",
    "run_id": "my-sweep-2026-01-15",
    "prediction_path": "/sweeps/run-a/all_preds.jsonl",
    "prediction_sha256": "e3b0c44298fc1c149afb...",
    "eval_started_at": "2026-01-15T10:00:00Z",
    "eval_ended_at": "2026-01-15T11:30:00Z",
    "sb_cli": {
      "submit_command": "sb-cli submit swe-bench-m dev --predictions_path ... --run_id ... --output_dir ... --wait_for_evaluation 1 --gen_report 1 --timeout-per-instance 600 --parallel 4",
      "report_command": "sb-cli get-report swe-bench-m dev --run_id ... --output_dir ... --overwrite 1",
      "report_paths": ["/sweeps/run-a/sb_cli_reports/swe-bench-m__dev__my-sweep.json"],
      "report_hashes": ["f00dba..."],
      "verify_submission": false,
      "wait_for_evaluation": true,
      "overwrite": true,
      "timeout_per_instance_secs": 600,
      "parallel": 4
    }
  },
  "instances": [...]
}
```

Fields with sensitive values (API keys in command strings) are automatically
redacted using the same `Redactor` that protects trajectory logs.

Legacy artifacts produced before this feature existed have no `provenance`
field; they deserialize successfully with `provenance: null`.

## Evaluator comparability

`bench compare` classifies every pair as one of three statuses:

| Status | Meaning |
|---|---|
| `matching` | Both evaluations used the same backend, version, dataset subset, and split |
| `mismatched` | A scoring-affecting field differs; comparison may be unreliable |
| `unavailable` | One or both sides lack provenance (legacy artifacts) |

Fields checked for comparability:
- `backend` — evaluator name (`"sb-cli"` vs `"none"`)
- `backend_version` — version string when both sides recorded one
- `dataset_subset` — e.g. `"swe-bench-m"` vs `"swe-bench_lite"`
- `dataset_split` — e.g. `"dev"` vs `"test"`
- `sb_cli.timeout_per_instance_secs` — when both used `sb-cli`
- `sb_cli.parallel` — when both used `sb-cli`

Fields intentionally **not** checked (do not affect scoring):
- `run_id`, `prediction_path`, `prediction_sha256` — identify the sweep, not the evaluator
- `eval_started_at`, `eval_ended_at` — timestamps only
- `report_paths`, `report_hashes` — output file locations

The status and any warnings appear in `bench compare` text output:

```text
Evaluator provenance: matching
```

or with warnings:

```text
Evaluator provenance: mismatched
  ! evaluator provenance: backend_version differs (baseline="sb-cli 0.5.0", candidate="sb-cli 0.6.1")
```

and in the compare JSON report as `evaluator_provenance_status` and
`evaluator_provenance_warnings`.

## Reproducible evaluation example

To ensure two sweeps are scored by the same evaluator, use identical flags:

```bash
# Baseline sweep
max bench evaluate \
  --sweep ./sweeps/baseline \
  --backend sb-cli \
  --sb-subset swe-bench-m \
  --sb-split dev \
  --timeout-per-instance 600 \
  --parallel 4

# Candidate sweep — identical evaluator flags
max bench evaluate \
  --sweep ./sweeps/candidate \
  --backend sb-cli \
  --sb-subset swe-bench-m \
  --sb-split dev \
  --timeout-per-instance 600 \
  --parallel 4

# Compare — will report evaluator_provenance_status: matching
max bench compare ./sweeps/baseline ./sweeps/candidate
```

If `sb-cli` is upgraded between evaluations, the `backend_version` mismatch
will surface as a warning in `bench compare` output, alerting you that the
results may not be directly comparable.

## Inspect provenance

`bench inspect` shows a summary line for the sweep's evaluation provenance:

```text
evaluator_provenance: backend=sb-cli version=sb-cli 0.5.2 subset=swe-bench-m split=dev run_id=my-sweep started=2026-01-15T10:00:00Z
```

When no `evaluation.json` exists or it predates provenance recording:

```text
evaluator_provenance: unavailable
```
