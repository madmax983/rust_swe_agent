# `agent artifact-check` — Artifact Contract Conformance Gate

Issue #534. Zero-cost structural conformance validator for artifact files.

## Purpose

`agent artifact-check` validates one or more artifact files (or directories,
recursively) against the [Artifact Contract](artifact-contract.md) without
making any model calls or network requests. It is designed as a preflight gate
for CI pipelines and for operators who receive artifacts from multiple sources
(own runs, teammates, exporters, downloaded leaderboard bundles) and need to
confirm conformance before feeding them into analysis or publishing them.

## Usage

```bash
max agent artifact-check [OPTIONS] <PATH>...
```

| Flag | Description |
|------|-------------|
| `<PATH>...` | One or more file or directory paths. Directories are scanned recursively for `*.json` files. |
| `--format text\|json` | Output format. `text` (default): human-readable table. `json`: emits a schema-versioned `validation_report` artifact. |
| `--strict` | Promote `legacy_unversioned` and `valid_with_warnings` verdicts to failures (in addition to the always-fatal `invalid` and `unsupported_major`). |

## Verdicts

Each artifact receives exactly one verdict:

| Verdict | Meaning | Failure by default? | Failure under `--strict`? |
|---------|---------|--------------------|-----------------------------|
| `valid` | All required fields present; `schema_version` is the current major.minor. | No | No |
| `valid_with_warnings` | All required fields present but `schema_version` is an older supported minor within the same major. Readers may default missing fields. | No | **Yes** |
| `invalid` | One or more required fields are missing or the header is malformed. | **Yes** | **Yes** |
| `legacy_unversioned` | Both `artifact_kind` and `schema_version` are absent (pre-versioning legacy artifact). | No | **Yes** |
| `unsupported_major` | `schema_version.major` exceeds the harness-supported major. | **Yes** | **Yes** |

## Per-kind required fields

The following artifact kinds are covered by full required-field validation.
Unknown additive fields within major `1` do **not** fail, consistent with the
contract reader policy.

| Kind | Required fields (beyond `artifact_kind` + `schema_version`) |
|------|--------------------------------------------------------------|
| `trajectory` | `trajectory_format`, `info`, `messages` |
| `sweep_results` | `total`, `submitted`, `skipped`, `errored`, `failures_by_category`, `instances` |
| `evaluation_results` | `instances` |
| `forecast_report` | `calibration`, `per_instance`, `forecast`, `resolution_rate`, `threshold` |
| `calibration_report` | `forecast_path`, `results_path`, `verdict`, `comparability`, `metrics`, `per_instance` |
| `preflight_report` | `mode`, `checks` |
| `swebench_predictions_metadata` | `predictions_file`, `aggregate`, `row_count`, `swebench_evaluator_compatible` |
| `bundle_manifest` | `source_sweep_dir`, `source_manifest_hash`, `harness_git_sha`, `bundle_generated_at`, `instance_scope`, `files` |
| `cache_stats_report` | `sweep`, `generated_at`, `cache_disabled`, `sweep_totals`, `instances` |

All other known artifact kinds are validated for a well-formed header only.

## Exit codes

| Code | Outcome class | When emitted |
|-----:|---------------|--------------|
| 0 | `success` | All artifacts are `valid` or `valid_with_warnings` (warnings are tolerated by default). |
| 2 | `usage_error` | Bad flag, missing required argument, or unreadable input path. |
| 47 | `artifact_check_failure` | At least one artifact is `invalid` or `unsupported_major`; or, with `--strict`, at least one is `legacy_unversioned` or `valid_with_warnings`. |

See [exit-codes.md](exit-codes.md) for the full contract.

## JSON output (`--format json`)

The `--format json` flag emits a schema-versioned `validation_report` document:

```json
{
  "artifact_kind": "validation_report",
  "schema_version": "1.0",
  "summary": {
    "total": 3,
    "valid": 2,
    "valid_with_warnings": 0,
    "invalid": 1,
    "legacy_unversioned": 0,
    "unsupported_major": 0
  },
  "results": [
    {
      "path": "runs/sweep/traj.json",
      "artifact_kind": "trajectory",
      "schema_version": "1.11",
      "verdict": "valid",
      "missing_fields": [],
      "warnings": []
    }
  ]
}
```

## Examples

Validate a single trajectory:

```bash
max agent artifact-check runs/traj.json
```

Validate an entire sweep directory recursively:

```bash
max agent artifact-check runs/sweep/
```

Validate multiple paths and emit JSON:

```bash
max agent artifact-check --format json runs/ exports/ > validation.json
```

Strict gate (fails on legacy artifacts and older-minor versions):

```bash
max agent artifact-check --strict runs/
```

CI pipeline gate:

```bash
max agent artifact-check runs/ || exit 1
```

## Out of scope

- Repairing or migrating artifacts — report only, no auto-fix.
- Semantic correctness of metrics (e.g. whether aggregates derive from
  trajectories) — that is `bench audit`'s job.
- Validating `all_preds*.jsonl` SWE-bench prediction rows, which intentionally
  carry no artifact metadata.

## Performance

Validation is purely structural (JSON parse + field presence check). A single
trajectory artifact completes in well under 200 ms with zero network or model
calls. Throughput scales linearly with the number of files.
