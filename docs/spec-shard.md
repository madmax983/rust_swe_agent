# `bench shard` — Dataset Partitioning Spec

`bench shard` deterministically partitions a source dataset JSONL into N
disjoint, balanced, provenance-stamped shard files.  It executes completely
offline with **zero** network, model provider, or Docker daemon calls.

---

## Purpose

Horizontal scaling with `bench merge` (see [spec-merge.md](spec-merge.md))
requires running independent per-shard sweeps and recombining them.  Until now,
operators had to hand-split the dataset JSONL with `head`/`tail`/`awk`, which
silently produces overlapping or gappy shards, unbalanced shards, and slices
with no provenance.  `bench shard` closes that gap: it is the **producer** that
feeds the **consumer** (`bench merge`).

---

## Invocation

```
bench shard \
  --dataset-path <PATH>   (or --dataset <ALIAS> [--split SPLIT]) \
  --shards <N> \
  --output <DIR> \
  [--seed <SEED>] \
  [--stratify-by repo] \
  [--stratify-mode balanced|proportional] \
  [--balance-by <KEY>] \
  [--force] \
  [--format text|json]
```

---

## Flags

| Flag | Default | Description |
|---|---|---|
| `--dataset-path <PATH>` | *(required, or `--dataset`)* | Local JSONL dataset file.  Mutually exclusive with `--dataset`. |
| `--dataset <ALIAS>` | *(mutually exclusive with `--dataset-path`)* | Named SWE-bench alias: `full`, `lite`, or `verified`. |
| `--split <SPLIT>` | `test` | Dataset split (`train`, `test`, `dev`).  Used only with `--dataset`. |
| `--dataset-cache-dir <DIR>` | *(auto)* | On-disk cache directory for named datasets. |
| `--shards <N>` | *(required)* | Number of output shards.  Must be ≥ 1 and ≤ the dataset instance count. |
| `--output <DIR>` | *(required)* | Destination directory.  Created if absent; must be empty unless `--force`. |
| `--seed <SEED>` | `0` | Determinism seed.  Identical `(dataset, N, seed)` → byte-identical output.  Recorded in every shard's manifest. |
| `--stratify-by repo` | *(off)* | Guarantee every repo is spread **evenly** across shards (a repo of size `q·N` lands exactly `q` per shard). When off, instances are partitioned by a single uniform global shuffle instead. |
| `--stratify-mode balanced\|proportional` | `balanced` | Controls rotation start point for leftover assignment (see below). |
| `--balance-by <KEY>` | *(not yet implemented)* | Reserved for future cost/size-proxy balancing.  Exits non-zero now; use `--stratify-by repo` instead. |
| `--force` | `false` | Overwrite non-empty output directory. |
| `--format text\|json` | `text` | `json` emits a machine-readable summary (see below). |

---

## Output Files

### `shard-000.jsonl` … `shard-(N-1).jsonl`

One SWE-bench instance per line, directly consumable by:

```
bench swebench --dataset-path shard-000.jsonl …
```

### `shard-NNN.manifest.json` — per-shard provenance sidecar

Schema-versioned JSON sidecar (`schema_version: "shard-manifest-v1"`):

| Field | Description |
|---|---|
| `source_dataset_sha256` | SHA-256 of the **whole** source dataset bytes.  Identical across every shard in the same partition. |
| `shard_index` | Zero-based index of this shard (`0 … N-1`). |
| `shard_count` | Total number of shards (`N`). |
| `seed` | Determinism seed used for this partition. |
| `stratify_by` | Stratification key (`repo`), if set. |
| `stratify_mode` | Allocation mode (`balanced` or `proportional`); always recorded, since the mode affects leftover placement even when `stratify_by` is off. |
| `instance_count` | Instances written to this shard's JSONL. |
| `resolved_instance_ids` | Ordered list of instance IDs in this shard. |
| `per_stratum_counts` | Per-repo instance counts; present only when `--stratify-by` was used. |
| `alias` | Named alias, if `--dataset` was used. |
| `split` | Split selector, if `--dataset` was used. |

---

## Partition Algorithm

1. **Order** all instances deterministically into one list:
   - With `--stratify-by repo`: **group** by `repo` in `BTreeMap` order
     (`"<unknown>"` fallback), **shuffle** each group with
     `XorShift64(seed ^ hash(group_index))` (the same RNG as the existing
     stratified sampler), then **flatten** in `BTreeMap` order. Because each
     repo's instances are contiguous, the round-robin in step 2 spreads every
     repo evenly across shards.
   - Without `--stratify-by`: a single uniform global shuffle with
     `XorShift64(seed ^ hash("shard-global-shuffle"))`. Balance is still
     guaranteed by step 2; repos are partitioned uniformly at random rather
     than deliberately spread.
2. **Assign** item at position `k` to shard `(start + k) % N` with a
   **continuous global cursor** that does not reset at repo boundaries —
   the only assignment that guarantees `max − min ≤ 1` regardless of repo sizes.
   - `balanced` (default): `start = seed % N` — rotates leftover placement.
   - `proportional`: `start = 0` — API symmetry with `bench subset`.
   For a **full** partition (no instances dropped), both modes guarantee
   `max − min ≤ 1`; they differ only in which shards receive the `T mod N`
   leftover instances.
3. **Assert** invariants before writing:
   - union of all shard IDs == source IDs (no dropped instance)
   - pairwise intersections empty (no duplicated instance)
   - `max − min ≤ 1` (balance)

---

## Guarantees

- **Coverage:** union of all shard instance-IDs equals the source dataset's
  instance-IDs exactly (0 dropped, 0 duplicated).  Asserted before writing.
- **Determinism:** same `(dataset, N, seed)` triple → byte-identical shard
  files across runs and hosts.
- **Balance:** `max − min` per-shard instance count ≤ 1; repo families are
  spread rather than clustered when `--stratify-by repo` is set.

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Partition written successfully. |
| `2` (UsageError) | Invalid arguments, missing required flag, `--balance-by` (not yet implemented). |
| non-zero | Unreadable/empty dataset, `N > instance count`, duplicate IDs in source, non-empty output without `--force`, I/O failure. |

---

## `--format json` Summary

```json
{
  "shard_count": 8,
  "total_instances": 300,
  "per_shard_counts": [38, 37, 38, 37, 38, 38, 37, 37],
  "balance_spread": 1,
  "source_dataset_sha256": "3a7b…"
}
```

Fields: `shard_count`, `total_instances`, `per_shard_counts` (one entry per
shard), `balance_spread` (`max − min` of `per_shard_counts`),
`source_dataset_sha256`.

---

## Worked Example: split → run → merge

```bash
# 1. Partition SWE-bench Lite into 8 shards (offline, < 2s)
bench shard \
  --dataset-path swe-bench-lite.jsonl \
  --shards 8 \
  --output shards/ \
  --seed 42 \
  --stratify-by repo \
  --format json

# 2. Run each shard independently (on separate workers / machines)
for i in $(seq -f "%03g" 0 7); do
  bench swebench \
    --dataset-path shards/shard-${i}.jsonl \
    --output results/shard-${i}/
done

# 3. Recombine with bench merge
bench merge \
  $(for i in $(seq -f "%03g" 0 7); do echo --shard results/shard-${i}; done) \
  --output results/merged/
```

**Provenance note:** each per-shard sweep's `results.json` records the shard
JSONL's content hash, not the source dataset hash.  `bench merge` requires
identical `dataset.sha256` across shards; this constraint will be resolved in a
future update that wires `bench swebench` to carry the source sha through.
The shard manifests carry `source_dataset_sha256` (identical across all shards)
for that future integration.  The offline partition round-trip (step 1 alone)
is fully verifiable: re-concatenating all shard JSONLs reproduces the source
instance-ID set exactly.

---

## Reproducibility Guarantees

- Re-running `bench shard` with identical flags against a dataset with the same
  content always produces **byte-identical JSONL** and equivalent manifests.
- The manifest `source_dataset_sha256` field enables downstream tools (`bench
  audit`, `bench reproduce`) to detect dataset drift.
- No timestamps or wall-clock values are written to either artifact.

---

## Relationship to Other Commands

| Command | Purpose |
|---|---|
| [`bench subset`](spec-subset.md) | Sample a *slice* of the dataset (not a full coverage partition). |
| [`bench merge`](spec-merge.md) | Recombine independent sharded sweep results into one canonical aggregate — the consumer this command feeds. |
| `bench swebench --dataset-path` | Consume a shard JSONL for evaluation. |
| [`bench audit`](spec-audit.md) | Re-derive and verify sweep aggregates; uses `dataset_sha256` for provenance. |
| [`bench dataset-stats`](spec-dataset-stats.md) | Preview dataset composition before partitioning. |
