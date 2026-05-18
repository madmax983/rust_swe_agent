# `bench reproduce` — Sweep Reproducibility

`bench reproduce` replays a saved sweep from its embedded `ProvenanceManifest`
and writes a `reproducibility.json` artifact that pairs each original instance
result against the corresponding replay result.

## Synopsis

```
max bench reproduce \
  --from  <source-sweep-dir> \
  --output <output-dir> \
  [--allow-drift <FIELD>]... \
  [--limit N] \
  [--filter INSTANCE_IDS] \
  [--per-task-budget-usd USD] \
  [--skip-model-probe]
```

## Required Arguments

| Flag | Description |
|------|-------------|
| `--from <DIR>` | Source sweep directory. Must contain `results.json` with an embedded `ProvenanceManifest`. |
| `--output <DIR>` | Output directory for the new sweep artifacts and `reproducibility.json`. Created if absent. Never mutates `--from`. |

## Optional Overrides

| Flag | Default | Description |
|------|---------|-------------|
| `--allow-drift <FIELD>` | *(none)* | Whitelist a hard-drift field. May be repeated. See [Drift Policy](#drift-policy). |
| `--limit N` | *(all)* | Replay at most N instances (partial replay). `--limit 0` performs a manifest/drift smoke check and writes an empty `reproducibility.json` without launching tasks. |
| `--filter SPEC` | *(all)* | Comma-separated instance ids, or `@path/to/ids.txt`. |
| `--per-task-budget-usd USD` | *(from manifest)* | Override the per-task USD ceiling for the replay sweep. Recorded in the new manifest. |
| `--skip-model-probe` | `false` | Skip the model-endpoint preflight probe. Useful for CI fixtures and dry-run modes that must not spend model credits. |

## Drift Policy

Before launching the replay sweep, `bench reproduce` compares the source
sweep's `ProvenanceManifest` against the current environment and classifies
every diverging field as **hard** or **soft**:

### Hard Drifts (abort by default)

| Field | Condition |
|-------|-----------|
| `harness.git_sha` | Both manifests have a SHA and they differ. |
| `dataset.sha256` | Both sha256 values are non-empty and differ. |
| `model.name` | Model names differ. |

Hard drifts abort with exit code 2 (`usage_error`) and print a one-line reason
per diverging field. Use `--allow-drift <field>` to whitelist specific fields:

```
# Allow a different harness commit without aborting
bench reproduce --from runs/sweep-1 --output runs/replay-1 \
  --allow-drift harness.git_sha
```

### Soft Drifts (warn only)

Fields such as concurrency settings or output paths that diverge are reported
as `[WARN]` lines but do not prevent the replay from launching.

## Output Artifacts

The replay sweep writes a complete, independent artifact set under `--output`:

```
<output>/
├── results.json          # new sweep results (with manifest + reproduced_from block)
├── reproducibility.json  # per-instance comparison (schema below)
├── <instance-id>/
│   ├── run-0.traj.json
│   ├── run-0.patch
│   └── run-0.output.txt
└── ...
```

The source `--from` directory is never modified.

`--limit 0` writes only `reproducibility.json` under `--output`; it is intended
for CI checks that need to prove a saved sweep or extracted bundle is readable
without spending model credits or requiring the original dataset.

## `reproducibility.json` Schema

```json
{
  "source_sweep": "/abs/path/to/source/sweep",
  "source_manifest_hash": "manifest-hash:deadbeefcafebabe",
  "reproduced_from": {
    "manifest_hash": "manifest-hash:deadbeefcafebabe",
    "sweep_dir": "/abs/path/to/source/sweep"
  },
  "instances": [
    {
      "instance_id": "repo__name-123",
      "original_resolved": true,
      "replay_resolved": true,
      "original_failure_category": null,
      "replay_failure_category": null,
      "patch_identical": true
    }
  ],
  "aggregate": {
    "matched": 4,
    "flipped_to_resolved": 0,
    "flipped_to_unresolved": 1,
    "both_unresolved_same_category": 2,
    "both_unresolved_different_category": 0,
    "errored": 0
  }
}
```

### Aggregate Field Definitions

| Field | Meaning |
|-------|---------|
| `matched` | Both original and replay resolved. |
| `flipped_to_resolved` | Original unresolved; replay resolved. |
| `flipped_to_unresolved` | Original resolved; replay unresolved. |
| `both_unresolved_same_category` | Both unresolved with the same `failure_category`. |
| `both_unresolved_different_category` | Both unresolved but different categories. |
| `errored` | Instance in original not found in replay output (or vice versa). |

## Stdout Summary

On completion, `bench reproduce` prints a non-JSON summary:

```
reproduce: 7 instance(s), 85.7% matched resolved status, 57.1% patch-identical
  matched=5 flipped_to_resolved=0 flipped_to_unresolved=1 both_unresolved_diff_cat=0 errored=0
```

## Exit Codes

`bench reproduce` follows the standard [exit-code contract](exit-codes.md):

| Code | Class | Condition |
|------|-------|-----------|
| 0 | `success` | All instances completed (regardless of resolved-status agreement). |
| 2 | `usage_error` | Hard drift detected without `--allow-drift` whitelist, or manifest missing. |
| 3 | `preflight_failure` | Replay sweep preflight failed (Docker, model endpoint). |
| 1 | `internal_error` | `results.json` parse failure, I/O error. |

## Compatibility Fixtures

Two checked-in fixture sweep directories under `tests/fixtures/reproduce/` are
runnable in CI with `--skip-model-probe` to prove manifest parsing and drift
detection without spending model credits:

- `tests/fixtures/reproduce/legacy_sweep/` — a minimal sweep from a prior
  manifest schema version.
- `tests/fixtures/reproduce/current_sweep/` — a current-version sweep with a
  complete `ProvenanceManifest`.

Both fixtures use a deterministic scripted model and a tiny one-instance
dataset, so they complete in milliseconds.

## Supported Manifest Version Range

`bench reproduce` reads any `ProvenanceManifest` that deserializes successfully
via serde. Unknown fields are ignored (forward-compatibility). Required fields
(`harness`, `dataset`, `model`, `runtime`, `cli`) must be present; optional
fields (`purpose`, `source_revision`, `cache_path`) default to `None`.
