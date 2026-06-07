# `bench import` — Ingest External SWE-bench Predictions

`bench import` reads an external SWE-bench predictions file (JSONL or JSON
array) and materialises a normalised sweep directory that is fully compatible
with all downstream `bench` tooling (`bench compare`, `bench triage`,
`bench report`, `bench inspect`, …) **without any model spend**.

## Synopsis

```bash
# Import only — resolved flags left at false until a separate `bench evaluate` pass
max bench import \
  --predictions experiments/mini-swe-agent-v1.0.jsonl \
  --dataset-path data/swe-bench-verified.jsonl \
  --output runs/imported/mini-swe-agent-v1.0

# Fused import + evaluate in one command (--backend none for zero-cost smoke mode)
max bench import \
  --predictions experiments/mini-swe-agent-v1.0.jsonl \
  --dataset-path data/swe-bench-verified.jsonl \
  --output runs/imported/mini-swe-agent-v1.0 \
  --evaluate \
  --backend none

# Fused import + evaluate against sb-cli
max bench import \
  --predictions mini.jsonl \
  --dataset-path swe-bench.jsonl \
  --output runs/mini \
  --evaluate \
  --sb-subset swe-bench_verified \
  --sb-split test

# Print import summary as JSON (useful for CI or scripting)
max bench import \
  --predictions mini.jsonl \
  --dataset-path swe-bench.jsonl \
  --output runs/mini \
  --format json
```

## Flags

| Flag | Required | Description |
|------|----------|-------------|
| `--predictions <PATH>` | yes | Path to the SWE-bench predictions file (JSONL or JSON array). |
| `--dataset-path <PATH>` | yes | Path to the matching SWE-bench dataset JSONL. Used to validate instance IDs and populate dataset metadata. |
| `--output <DIR>` | yes | Output directory for the normalised sweep. Created if it does not exist. |
| `--evaluate` | no | Run the evaluator pipeline after import to populate `pass_at_1` / `resolved_count`. The sweep directory is written first; if the evaluate stage fails it is left intact for a manual `bench evaluate` re-run. |
| `--backend sb-cli\|none\|rehearsal\|docker-tests` | no | Evaluator backend used when `--evaluate` is set. Default: `sb-cli`. Matches `bench evaluate --backend`. |
| `--sb-subset <NAME>` | no | SWE-bench subset for sb-cli when `--evaluate` is set. Default: `swe-bench-m`. Matches `bench evaluate --sb-subset`. |
| `--sb-split <NAME>` | no | SWE-bench split when `--evaluate` is set. Default: `dev`. Matches `bench evaluate --sb-split`. |
| `--timeout-per-instance <SECS>` | no | Per-instance evaluation timeout in seconds when `--evaluate` is set. Default: `600`. |
| `--parallel <N>` | no | Parallel worker count for the evaluation backend when `--evaluate` is set. Default: `4`. |
| `--run-id <ID>` | no | Optional sb-cli run id when `--evaluate` is set. |
| `--format text\|json` | no | Output format for the stdout summary. Default: `text`. |

## Input Format

The predictions file may be either:

* **JSONL** — one JSON object per line:
  ```jsonl
  {"instance_id": "django__django-11001", "model_patch": "diff ...", "model_name_or_path": "my-model"}
  {"instance_id": "django__django-11002", "model_patch": "", "model_name_or_path": "my-model"}
  ```
* **JSON array** — a top-level array of the same objects:
  ```json
  [
    {"instance_id": "django__django-11001", "model_patch": "diff ...", "model_name_or_path": "my-model"},
    ...
  ]
  ```

This matches the standard SWE-bench submission format used by the public
[`experiments/`](https://github.com/swe-bench/experiments) repository.

Required fields per record:

| Field | Type | Notes |
|-------|------|-------|
| `instance_id` | `string` | SWE-bench instance identifier. Records with a missing or blank `instance_id` are skipped and counted in `records_skipped`. |
| `model_patch` | `string` | The model-generated unified diff. Empty string means no patch was produced. |
| `model_name_or_path` | `string` | Optional. Used to populate the manifest `model.name` field. |

## Output Layout

```text
<output>/
  results.json             # SweepResults artifact (see below)
  <instance_id>/
    run-1.patch            # written only when model_patch is non-empty
```

## `results.json` Schema

The written artifact is a standard `SweepResults` with the following import-specific
values:

| Field | Value |
|-------|-------|
| `artifact_kind` | `"sweep_results"` |
| `total_cost_usd` | `0.0` |
| `instances[*].steps` | `null` / absent |
| `instances[*].pass_at_1` | `false` (until `bench evaluate` fills them in) |
| `instances[*].resolved_count` | `0` |
| `instances[*].cost_usd` | `0.0` |
| `manifest.source` | `"external_import"` |
| `manifest.import_predictions_path` | canonical filesystem path of the source predictions file |
| `manifest.import_predictions_sha256` | `sha256:<hex>` content digest of the predictions file |

Records whose `instance_id` is not found in the dataset JSONL are still
imported (their `error` field is set to `"unknown_instance_id"`) so that
downstream tooling sees all entries without silently dropping them.

## Unknown Instance IDs

When an imported `instance_id` is not present in `--dataset-path`, the
instance is still included in `results.json` but its `InstanceResult.error`
field is set to `"unknown_instance_id: <id>"`. This surfaces the issue in
`bench inspect` and `bench triage` while still producing a usable sweep
directory.

## Stdout Summary

### `--format text` (default)

```
=== bench import ===
Records imported:   4
Records skipped:    0
Output:             /abs/path/to/output
Source hash:        sha256:abc123...
```

### `--format json`

```json
{
  "records_imported": 4,
  "records_skipped": 0,
  "output_path": "/abs/path/to/output",
  "source_hash": "sha256:abc123..."
}
```

The JSON output is suitable for CI pipelines that need to extract the output
path or hash programmatically.

## Populating Resolution Results

`bench import` without `--evaluate` sets `pass_at_1 = false` for all instances
(the zero-cost guarantee). There are two ways to get a fully evaluated sweep:

**Option A — Fused one-shot (recommended):** Use `--evaluate` to run the
evaluator immediately after import:

```bash
max bench import \
  --predictions mini.jsonl \
  --dataset-path swe-bench.jsonl \
  --output runs/mini \
  --evaluate \
  --sb-subset swe-bench_verified \
  --sb-split test
```

This produces `results.json` *and* `evaluation.json` in a single invocation.

**Option B — Two-step:** Import first, then evaluate separately:

```bash
max bench import \
  --predictions mini.jsonl \
  --dataset-path swe-bench.jsonl \
  --output runs/mini

max bench evaluate \
  --sweep runs/mini \
  --sb-subset swe-bench_verified \
  --sb-split test
```

When `--evaluate` is used, the sweep directory is written before the evaluator
runs. If the evaluate stage fails (e.g. sb-cli/network error), `results.json`
is already on disk and the operator can re-run `bench evaluate` without
re-importing.

## Round-Trip Comparison

After import you can compare the imported sweep against a native harness run
just like any two sweeps:

```bash
max bench compare \
  --baseline runs/native/my-sweep \
  --candidate runs/imported/mini-swe-agent-v1.0 \
  --format json
```

## Provenance Tracking

The three `manifest.*` fields written to `results.json` allow reproducibility
auditing:

| Field | Purpose |
|-------|---------|
| `manifest.source` | Marks the sweep as `"external_import"` so tools can distinguish it from native harness runs. |
| `manifest.import_predictions_path` | The canonical filesystem path of the predictions file at import time. |
| `manifest.import_predictions_sha256` | `sha256:<hex>` of the file bytes; re-running import on the same file produces the same hash. |

## Exit Codes

| Code | Meaning |
|------|---------|
| `0` | Import succeeded; `results.json` written. When `--evaluate` is set, `evaluation.json` is also written. |
| `1` (config/usage) | `--predictions`, `--dataset-path`, or `--output` are missing or invalid; `--backend` value is unrecognised. |
| `1` (I/O) | Predictions file could not be read or `results.json` could not be written. |
| `1` (evaluate) | Import succeeded (sweep dir on disk) but the evaluate stage failed (e.g. sb-cli/network error). Re-run `bench evaluate --sweep <OUTPUT>` to retry evaluation without re-importing. |
