# bench subset Subcommand Specification

`bench subset` exports a sampled dataset slice as a committable JSONL artifact plus a self-describing provenance manifest.  It executes completely offline with **zero** network, model provider, or Docker daemon calls.

---

## Purpose

Sampling today is computed *inline* and consumed immediately by `bench swebench`.  There is no way to **materialise** the sampled slice as a file you can `git add`, review in a PR, pin in CI, and re-run without re-deriving.

`bench subset` closes that gap.  It wraps the same selection/sampling pipeline already used by `bench swebench`, `bench dataset-stats`, and `bench cascade`, serialises the resolved instance set to JSONL, and writes a sidecar manifest that records every input needed to reproduce the slice exactly.

---

## Output Files

### `<output>` — JSONL slice

One SWE-bench instance per line, identical to the rows the same selectors would feed `bench swebench`.  Directly consumable via:

```
bench swebench --dataset-path <output> …
```

with no transformation.

### `<stem>.manifest.json` — provenance manifest

Schema-versioned JSON sidecar (`schema_version: "subset-manifest-v1"`) that records:

| Field | Description |
|---|---|
| `source_dataset_sha256` | SHA-256 hex digest of the source dataset bytes (pre-filter). |
| `alias` | Named alias (`full`, `lite`, `verified`) when `--dataset` was used. |
| `split` | Split selector (`train`, `test`, `dev`) when `--dataset` was used. |
| `selection` | All selection flag values (`instance_ids`, `limit`, `sample`, `seed`, `stratify_by`, `stratify_mode`, plus `original_count` and `selected_count`). |
| `instance_count` | Number of instances written to the JSONL. |
| `resolved_instance_ids` | Ordered list of all materialised instance IDs. |
| `per_stratum_counts` | Per-repo instance counts (present only when `--stratify-by` was used). |

The manifest contains **no timestamps**.  Re-running `bench subset` with identical inputs produces byte-identical JSONL and an equivalent manifest.

---

## Command Reference

### Usage

```
bench subset --dataset-path <PATH> --output <PATH> [SUBSET_SELECTORS…]
```

or with a named dataset alias:

```
bench subset --dataset <ALIAS> [--split <SPLIT>] --output <PATH> [SUBSET_SELECTORS…]
```

### Options

| Flag | Description |
|---|---|
| `--dataset-path <PATH>` | Local JSONL dataset file.  Mutually exclusive with `--dataset`. |
| `--dataset <ALIAS>` | Named SWE-bench alias: `full`, `lite`, or `verified`.  Mutually exclusive with `--dataset-path`. |
| `--split <SPLIT>` | Dataset split: `train`, `test` (default), or `dev`.  Only used with `--dataset`. |
| `--dataset-cache-dir <DIR>` | On-disk cache directory for named datasets. |
| `--output <PATH>` | **(Required)** Destination JSONL path for the materialised slice. |
| `--instance-ids <IDS>` | Comma-separated instance IDs or `@path/to/file.txt`. |
| `--limit <N>` | Keep at most N instances after all other filters. |
| `--sample <N>` | Randomly select N instances (requires `--seed`). Must not exceed the available post-filter count. |
| `--seed <SEED>` | RNG seed for `--sample`.  Required when `--sample` is given. |
| `--stratify-by repo` | Stratify `--sample` by repository. |
| `--stratify-mode proportional\|balanced` | Allocation mode used with `--stratify-by` (default: `proportional`). |

### Exit Codes

| Code | Meaning |
|---|---|
| `0` | Slice written successfully. |
| non-zero | Invalid arguments, zero instances selected, `--sample` larger than available count, or I/O error. |

---

## Examples

### Pin a stratified 25-instance slice

```bash
bench subset \
  --dataset verified --split test \
  --sample 25 --seed 42 --stratify-by repo \
  --output ci/eval-slice.jsonl
git add ci/eval-slice.jsonl ci/eval-slice.manifest.json
```

### Re-run using the pinned slice

```bash
bench swebench --dataset-path ci/eval-slice.jsonl …
```

### Export an explicit ID list

```bash
bench subset \
  --dataset-path ./data/swe-bench.jsonl \
  --instance-ids @ids.txt \
  --output slices/targeted.jsonl
```

---

## Reproducibility Guarantees

- Re-running `bench subset` with identical flags against a dataset with the same `source_dataset_sha256` always produces **byte-identical JSONL** and an equivalent manifest.
- The manifest `source_dataset_sha256` field enables downstream tools (`bench audit`, `bench reproduce`, `bench dataset-verify`) to detect dataset drift.
- No timestamps or wall-clock values are written to either artifact.

---

## Relationship to Other Commands

| Command | Purpose |
|---|---|
| [`bench dataset-stats`](spec-dataset-stats.md) | Preview composition statistics **before** materialising a slice. |
| `bench swebench --dataset-path` | Consume a materialised slice for evaluation. |
| `bench reproduce` | Replay a sweep from its `results.json` manifest. |
| `bench audit` | Re-derive and verify sweep aggregates; can re-verify a subset's source hash. |
| `bench dataset-verify` | Authenticate the source dataset against its canonical official release. |

---

## Implementation Notes

- Sampling and stratification logic is **reused verbatim** from `apply_subset` / `stratified_sample_by_repo` in `src/run/swebench.rs` — no new sampling semantics are introduced.
- The JSONL is produced by re-serialising each `SweBenchInstance` through `serde_json`; because `serde_json::Map` uses `BTreeMap` (alphabetically sorted keys), output is deterministic regardless of source key order.
- The manifest is formatted with `serde_json::to_string_pretty` for human-readability and diff-friendliness.
