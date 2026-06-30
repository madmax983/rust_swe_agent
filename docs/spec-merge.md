# `bench merge` — Shard Aggregation Spec

## Purpose

Horizontal scaling: run independent sharded sweeps across multiple workers or
machines, then recombine them offline into one canonical aggregate that is a
drop-in for `bench evaluate`, `bench audit`, `bench report`, and `bench triage`.

This command is **offline and read-only** — it never launches workers, does not
continue an interrupted sweep (`--resume` exists for that), and does not merge
different models or configs (`bench matrix` covers cross-config comparison).

## Invocation

```
bench merge \
  --shard /path/to/shard-a \
  --shard /path/to/shard-b \
  [--shard /path/to/shard-c ...] \
  --output /path/to/merged \
  [--on-collision error|first-wins|last-wins] \
  [--label A --label B [--label C ...]] \
  [--force] \
  [--format text|json]
```

## Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--shard PATH` | *(required, repeatable)* | Path to a completed sweep directory. Minimum 2. |
| `--output PATH` | *(required)* | Output directory (must be empty unless `--force`). |
| `--on-collision POLICY` | `error` | How to handle duplicate instance IDs across shards (see below). |
| `--label STRING` | *(repeatable, optional)* | Human-readable label for each shard in provenance/report. If provided, must match `--shard` count. |
| `--force` | `false` | Allow writing into a non-empty output directory. |
| `--format text\|json` | `text` | Output format for the summary report. |

## Collision Policies

When the same `instance_id` appears in more than one shard:

| Policy | Behaviour |
|--------|-----------|
| `error` (default) | Collect **all** colliding IDs and exit non-zero, listing every duplicate and the shard pair that owns it. |
| `first-wins` | Keep the instance from the first shard that claimed it; the later shard's copy is dropped and `duplicates` counter is incremented. |
| `last-wins` | Keep the instance from the last shard that claimed it; earlier copies are dropped and `duplicates` counter is incremented. |

## Provenance Requirements

All shards must share identical values for:

- `manifest.dataset.sha256` — ensures all shards ran against the same dataset
- `manifest.model.name` — ensures all shards used the same model
- `manifest.config` (SHA-256 of the resolved TOML string) — ensures identical agent configuration

On divergence the command fails with a descriptive error naming the differing
field and both values. Cross-config or cross-model experiments belong in
`bench matrix`.

## `merged_from` Manifest Field

The merged `results.json` carries a new field on the provenance manifest:

```json
{
  "manifest": {
    "merged_from": [
      {
        "label": "shard-a",
        "dir": "/path/to/shard-a",
        "model": "gpt-4o",
        "instance_count": 200
      },
      {
        "label": "shard-b",
        "dir": "/path/to/shard-b",
        "model": "gpt-4o",
        "instance_count": 200
      }
    ],
    "source": "merge"
  }
}
```

All existing manifest fields (dataset, model, config, harness, runtime) are
inherited from shard 0. `merged_from` is additive and `skip_serializing_if =
None`, so tools that read `results.json` without understanding this field are
unaffected.

## Audit Compatibility

`bench merge` copies every per-instance artifact directory from each shard into
the output sweep directory, preserving the source shard's on-disk layout:

- **Nested layout** `<instance_id>/run-N.traj.json` — entire per-instance
  subdirectory is copied recursively.
- **Legacy flat layout** `<instance_id>.traj.json` — individual files are
  copied to the output root.
- **Bundled layout** `trajectories/<instance_id>.traj.json` and
  `patches/<instance_id>.patch` — as produced by an extracted `bench bundle`.

This preserves the bijection that `bench audit` enforces: every `instance_id`
in `results.json` must have a corresponding trajectory file on disk, and every
trajectory file must be referenced by `results.json`.

## Aggregate Recomputation

All top-line metrics are recomputed from the union instance list — no numbers
are summed from per-shard headers, which prevents double-counting:

- `total_cost_usd`, token counts — summed over the union
- `with_patch`, `failures_by_category` — counted over the union
- `pass_at_k`, resolved rate — derived per-instance from the union

The outcome **slot** counts (`submitted`, `errored`, `skipped`, `budget_halted`)
are recounted from the copied run trajectories — exactly the way `bench audit`
recomputes them — rather than from the collapsed per-task rows. For a pass@k /
multi-run sweep one task spans several run slots (e.g. 2 runs both submitted →
2 submitted slots), so a per-task count would disagree with the on-disk
trajectories and fail audit. Counting per slot keeps the merged `results.json`
reconciled with the trajectories for both single-run and multi-run sweeps.

Only `retry_history` is carried from shard 0 (documented limitation; retries
are per-sweep, not per-merge).

## Predictions (`bench evaluate` compatibility)

`bench evaluate` scores a sweep from `<sweep>/all_preds.jsonl`. Merge produces a
complete prediction set for the union:

- `all_preds.jsonl` — the aggregate file, restricted to union-owned rows. For
  multi-run shards each row keeps its unique `<id>::run-k` id plus
  `original_instance_id`, and the regenerated metadata marks
  `swebench_evaluator_compatible: false` (duplicate ids are not sb-cli-safe).
- `all_preds.run-k.jsonl` — one per run index, original SWE-bench ids, safe to
  hand directly to sb-cli.
- `all_preds.metadata.json` / `all_preds.run-k.metadata.json` — regenerated with
  the merged row counts.

Prediction rows are filtered by the same owning shard used for trajectories, so
collision-dropped instances never appear twice.

## Subset Metadata

Both `results.json.filter_spec` and `manifest.dataset.filter_spec` are rebuilt
to describe the merged union (the sorted union instance-id list), not shard 0's
per-shard filter. This keeps `bench compare`'s "same subset?" check honest when
shards used different `--instance-ids`/filters.

## Output Isolation

`--output` must not be equal to, contain, or be contained by any input shard.
Because `--force` clears a non-empty output directory with `remove_dir_all`, an
overlapping output path would delete shard artifacts before they are copied;
merge rejects such paths up front with a clear error.

## `evaluation.json` Handling

Evaluation is merged **all-or-nothing**: the merged output gets an
`evaluation.json` only when *every* shard carries one. A modern `evaluation.json`
requires an entry for every on-disk trajectory, so a partial merge (some shards
evaluated, some not) would turn the unevaluated shards' trajectories into
`audit:orphan:evaluation` failures. When evaluation is present in only some
shards, merge warns and omits the file so the merged sweep still audits cleanly
as an unevaluated sweep — re-run `bench evaluate` on the merged output, or
evaluate every shard first.

When all shards are evaluated, both modern (`instances[].resolved`) and legacy
(`resolved_ids` / `submitted_ids`) formats are normalized to the modern format
(legacy rows gain the required `eval_exit_reason`). Instances dropped by the
collision policy are excluded.

## JSON Summary Format (AC6)

With `--format json`:

```json
{
  "shards": [
    { "label": "shard-a", "dir": "/path/to/shard-a", "instance_count": 200 },
    { "label": "shard-b", "dir": "/path/to/shard-b", "instance_count": 200 }
  ],
  "total_instances": 400,
  "duplicates": 0,
  "collision_policy": "Error",
  "output_dir": "/path/to/merged",
  "total_cost_usd": 42.50,
  "submitted": 400,
  "errored": 12,
  "resolved": 180,
  "pass_at_k": 0.45
}
```

## Error Conditions (AC7)

| Condition | Exit code | Message |
|-----------|-----------|---------|
| Fewer than 2 `--shard` args | non-zero | "bench merge requires at least 2 shards" |
| `--label` count ≠ `--shard` count | non-zero | "number of --label values must match --shard count" |
| `results.json` missing or unreadable | non-zero | "merge: shard '…' results.json: …" |
| `sweep_status` ≠ `completed` | non-zero | "merge: shard '…' sweep_status is '…'; only completed sweeps can be merged" |
| Shard has no provenance manifest | non-zero | "merge: shard '…' has no provenance manifest" |
| Dataset / model / config divergence | non-zero | "merge: shard '…' dataset_sha256 '…' differs from shard '…' '…'" |
| Collision with `--on-collision error` | non-zero | "merge: N duplicate instance IDs found: …" |
| Output dir non-empty without `--force` | non-zero | "merge: output directory '…' is not empty; use --force to overwrite" |

## Worked 2-Shard Example

```
# Run two independent shards
bench swebench --dataset swe-bench-lite.jsonl --filter-even --output shard-a
bench swebench --dataset swe-bench-lite.jsonl --filter-odd  --output shard-b

# Combine them
bench merge \
  --shard shard-a --label "even-instances" \
  --shard shard-b --label "odd-instances" \
  --output merged

# All downstream commands work on the merged output
bench audit  --sweep merged          # exit 0 — bijection intact
bench report --sweep merged          # full markdown summary
bench triage --sweep merged          # failure cluster table
bench evaluate --sweep merged        # pass@k, cost-per-resolved
```

Sample text output:

```
bench merge
  shard even-instances  /path/to/shard-a  200 instances
  shard odd-instances   /path/to/shard-b  200 instances
  ─────────────────────────────────────────────────────
  total                                   400 instances
  duplicates                              0
  output                                  /path/to/merged

merged results:
  cost      $42.50
  submitted 400
  errored   12
  resolved  180
  pass@k    0.4500
```
