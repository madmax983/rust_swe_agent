# Artifact Contract

Run artifacts use explicit top-level metadata:

```json
{
  "artifact_kind": "sweep_results",
  "schema_version": { "major": 1, "minor": 3 }
}
```

`schema_version.major` is breaking. `schema_version.minor` is additive within the same major. Unknown additive fields in major `1` are ignored by readers unless a specific command documents otherwise. Missing `artifact_kind` and `schema_version` means a pre-versioning legacy artifact; supported readers load it with a warning. Future major versions fail before metrics are printed.

## Current Artifacts

| Kind | File(s) | Required fields | Optional/additive fields |
| --- | --- | --- | --- |
| `trajectory` | `*.traj.json`, `<instance>/run-k.traj.json` | `trajectory_format`, `artifact_kind`, `schema_version`, `info`, `messages` | `actual_cost_usd`, `actual_cost_source`, `baseline_cost_usd`, `baseline_cost_model`, active `toolset`, `verification_status` (`verified`/`unverified`/`verification_failed`), `verification_results` (array of per-check evidence), `info.manifest` (provenance manifest — see `docs/spec-trajectory.md`), extra `info` fields, message `extra` fields |
| `sweep_results` | `results.json` | `artifact_kind`, `schema_version`, `total`, `submitted`, `skipped`, `errored`, `failures_by_category`, `instances` | `actual_cost_usd`, `actual_cost_source`, `baseline_cost_usd`, `baseline_cost_model`, manifest, filter spec, token, retry, rate-limit, cancellation fields |
| `evaluation_results` | `evaluation.json` | `artifact_kind`, `schema_version`, `instances` | behavioral metrics, breakdown rows, cost attribution |
| `forecast_report` | `bench forecast --format json` | `artifact_kind`, `schema_version`, `calibration`, `per_instance`, `forecast`, `resolution_rate`, `threshold` | `forecast.target_instance_ids` for exact calibration comparability, additional forecast diagnostics |
| `calibration_report` | `bench calibrate` / `calibration.json` | `artifact_kind`, `schema_version`, `forecast_path`, `results_path`, `verdict`, `comparability`, `metrics`, `per_instance` | mismatch details, warnings, future calibration diagnostics |
| `preflight_report` | `bench doctor/swebench --format json` | `artifact_kind`, `schema_version`, `mode`, `checks` | additional check metadata |
| `swebench_predictions_metadata` | `all_preds.metadata.json`, `all_preds.run-k.metadata.json` | `artifact_kind`, `schema_version`, `predictions_file`, `aggregate`, `row_count`, `swebench_evaluator_compatible` | `run_index`, future provenance fields |
| `bundle_manifest` | `BUNDLE.json` inside `bench bundle` archives | `artifact_kind`, `schema_version`, `source_sweep_dir`, `source_manifest_hash`, `harness_git_sha`, `bundle_generated_at`, `instance_scope`, `files` | future integrity metadata |
| `cache_stats_report` | `cache-stats.json` (written by `bench cache-stats`) | `artifact_kind`, `schema_version` (harness-wide current), `sweep`, `generated_at`, `cache_disabled`, `sweep_totals`, `instances` | `baseline` (delta vs prior sweep) |

`all_preds*.jsonl` rows intentionally do not carry artifact metadata. They stay compatible with SWE-bench evaluators; version metadata lives in the companion metadata JSON files.

Cost fields are split deliberately:

- `actual_cost_usd` is the cost recorded for the model/provider actually used.
- `actual_cost_source` is typed as `provider_reported`, `rate_card_estimate`, `free_tier_inferred`, or `unknown`.
- `baseline_cost_usd` is a counterfactual estimate for the same token usage using `baseline_cost_model`.
- Legacy `total_cost_usd` remains present for compatibility and should be treated as historical/summary cost, not as the only cost signal.

## Reader Policy

`bench inspect`, `bench tail`, `bench compare`, `bench evaluate`, `bench calibrate`, and trajectory diff loading classify artifacts before reporting metrics.

Compatibility classes:

- `supported-current`: exact current version, or a supported same-major additive minor.
- `supported-legacy`: pre-versioning artifacts or older supported versions. Readers warn and may default missing fields.
- `unsupported-future`: a version with `major > 1`. Readers fail fast and do not emit resolved-rate, cost, or comparison metrics.

Any future schema change needs either a documented no-bump rationale in the relevant change description or a schema-version bump plus fixture updates under `tests/fixtures/artifact_schema`.

### No-bump change log

| Issue | Change | Rationale |
|-------|--------|-----------|
| #329 | Added `info.manifest` (`Option<MiniProvenanceManifest>`) to trajectory | Field is optional (`skip_serializing_if = "is_none"`); absent from pre-#329 trajectories (reads as `None` in new code); silently ignored by all existing `bench inspect`/`tail`/`compare` readers; fully reader-transparent under the major-1 additive policy. |

## Trajectory `failure_category` values

The `info.failure_category` field in a trajectory artifact uses a stable string
taxonomy.  Readers should treat unrecognised values as `unknown`.  The
**canonical reference** for all values, their definitions, triage actions, and
compatibility policy is [`docs/failure-categories.md`](failure-categories.md).

### `agent_stagnation` diagnostics

When `failure_category` is `agent_stagnation`, `info.other["stagnation"]`
contains a diagnostic object:

```json
{
  "action_hash":   "<32-hex-char SHA-256 prefix of canonical action>",
  "count":         4,
  "window":        8,
  "step_indices":  [2, 4, 6, 8]
}
```

This field was introduced in trajectory format `mini-swe-agent-1.2`.
Older trajectories will not contain it.  See `docs/spec-stagnation.md` for
the full detection rule and configuration reference.
