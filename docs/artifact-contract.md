# Artifact Contract

Run artifacts use explicit top-level metadata:

```json
{
  "artifact_kind": "sweep_results",
  "schema_version": { "major": 1, "minor": 0 }
}
```

`schema_version.major` is breaking. `schema_version.minor` is additive within the same major. Unknown additive fields in major `1` are ignored by readers unless a specific command documents otherwise. Missing `artifact_kind` and `schema_version` means a pre-versioning legacy artifact; supported readers load it with a warning. Future major versions fail before metrics are printed.

## Current Artifacts

| Kind | File(s) | Required fields | Optional/additive fields |
| --- | --- | --- | --- |
| `trajectory` | `*.traj.json`, `<instance>/run-k.traj.json` | `trajectory_format`, `artifact_kind`, `schema_version`, `info`, `messages` | extra `info` fields, message `extra` fields |
| `sweep_results` | `results.json` | `artifact_kind`, `schema_version`, `total`, `submitted`, `skipped`, `errored`, `failures_by_category`, `instances` | manifest, filter spec, cost, token, retry, rate-limit, cancellation fields |
| `evaluation_results` | `evaluation.json` | `artifact_kind`, `schema_version`, `instances` | behavioral metrics, breakdown rows, cost attribution |
| `forecast_report` | `bench forecast --format json` | `artifact_kind`, `schema_version`, `calibration`, `per_instance`, `forecast`, `resolution_rate`, `threshold` | additional forecast diagnostics |
| `preflight_report` | `bench doctor/swebench --format json` | `artifact_kind`, `schema_version`, `mode`, `checks` | additional check metadata |
| `swebench_predictions_metadata` | `all_preds.metadata.json`, `all_preds.run-k.metadata.json` | `artifact_kind`, `schema_version`, `predictions_file`, `aggregate`, `row_count`, `swebench_evaluator_compatible` | `run_index`, future provenance fields |

`all_preds*.jsonl` rows intentionally do not carry artifact metadata. They stay compatible with SWE-bench evaluators; version metadata lives in the companion metadata JSON files.

## Reader Policy

`bench inspect`, `bench tail`, `bench compare`, `bench evaluate`, and trajectory diff loading classify artifacts before reporting metrics.

Compatibility classes:

- `supported-current`: exact current version, or a supported same-major additive minor.
- `supported-legacy`: pre-versioning artifacts or older supported versions. Readers warn and may default missing fields.
- `unsupported-future`: a version with `major > 1`. Readers fail fast and do not emit resolved-rate, cost, or comparison metrics.

Any future schema change needs either a documented no-bump rationale in the relevant change description or a schema-version bump plus fixture updates under `tests/fixtures/artifact_schema`.
