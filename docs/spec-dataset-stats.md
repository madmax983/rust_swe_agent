# bench dataset-stats Subcommand Specification

`bench dataset-stats` is a zero-cost, offline diagnostic subcommand designed to analyze and preview a SWE-bench dataset's composition before running a sweep. It executes completely offline with **zero** network, model provider, or Docker daemon calls.

---

## Features

### 1. Zero-Cost Token Estimation
Utilizes `litellm-rs`'s offline `TokenCounter` to compute accurate prompt token estimates for each instance's problem statement based on a chosen model configuration (defaults to `gpt-4`). This provides high-fidelity token statistics without making network calls or incurring API costs.

### 2. Multi-Tiered Language Detection
To accurately report the languages present:
1. **Diff Extensions (Primary)**: Parses changed file pathways out of the `patch` and `test_patch` diff headers (e.g. lines starting with `--- a/` or `+++ b/`) to identify file extensions (e.g. `.py` -> `Python`, `.rs` -> `Rust`, `.js`/`.ts` -> `JavaScript`/`TypeScript`).
2. **Repository Substrings (Fallback)**: Inspects the repository name field (case-insensitively checking against `django`, `pytest`, `pandas`, `sympy`, etc. mapping to `Python`).
3. **Registry Fallback**: Defaults gracefully to `Python` if still undetermined.

### 3. Representativeness & Skew Analysis
Ensures that a selected slice/subset is representative of the complete dataset:
* **Repository Skew**: Flags a skew warning if the unique repositories covered in the slice comprise **less than 50%** of the unique repositories present in the full dataset.
* **Token Length Skew**: Flags a skew warning if the median problem-statement token length of the slice differs from the full dataset's median by **more than 25%**.

### 4. Historical Sweep Integration
Traverses the configured sweeps root directory (`--runs-dir`, defaulting to `./runs`) to find `results.json` files from prior completed sweeps.
* It verifies that the `manifest.dataset.sha256` content hash match.
* Joins historical `resolved_count` and total attempts/runs.
* Calculates per-instance historical resolved rates to present the min, p50, and max historical resolved-rates across the selected instances.

---

## Command Reference

### Usage
```powershell
bench dataset-stats --dataset-path <PATH_TO_JSONL> [SUBSET_SELECTORS...] [--format <text|json>]
```
or with a dataset alias:
```powershell
bench dataset-stats --dataset <full|lite|verified> --split <test|dev|train> [SUBSET_SELECTORS...] [--format <text|json>]
```

### Options
* `--dataset-path`: Paths to a local JSONL file containing SWE-bench instances.
* `--dataset`: Named SWE-bench dataset alias: `full`, `lite`, or `verified`.
* `--split`: Dataset split: `train`, `test`, or `dev`. Defaults to `test`.
* `--dataset-cache-dir`: Cache directory for named datasets.
* `--limit <N>`: Truncates the slice to at most N instances.
* `--sample <N>`: Samples N instances randomly (requires `--seed`).
* `--seed <SEED>`: RNG seed to ensure stable reproducible sampling.
* `--instance-ids <IDS>`: Explicit list of instance IDs to select.
* `--stratify-by <REPO>`: Stratify sampling by repository.
* `--stratify-mode <proportional|balanced>`: Stratification mode.
* `--runs-dir <DIR>`: Directory containing prior completed sweeps. Defaults to `./runs`.
* `--model <MODEL>`: Target model for TokenCounter estimation. Defaults to `gpt-4`.
* `--format <text|json>`: Output format. Defaults to `text`.

---

## Examples

### Previewing a representative 10-instance slice
```powershell
bench dataset-stats --dataset lite --split test --sample 10 --seed 42
```

Output:
```text
=== SWE-bench Dataset Statistics Preview ===
Total Instances: 10
Languages Present: Python

--- Repository Share ---
┌───────────────────┬────────────────┬───────────────┐
│ Repository        │ Instance Count │ Percent Share │
├───────────────────┼────────────────┼───────────────┤
│ django/django     │ 8              │ 80.00%        │
├───────────────────┼────────────────┼───────────────┤
│ sympy/sympy       │ 2              │ 20.00%        │
└───────────────────┴────────────────┴───────────────┘

--- Problem-Statement Tokens Distribution ---
  Min: 120  |  P50: 250  |  P90: 820  |  Max: 1200

--- Expected Tests Distribution ---
  Min: 2  |  P50: 8  |  P90: 16  |  Max: 24

--- Historical Sweep Resolved Rates ---
  Min: 0.0000  |  P50: 0.5000  |  Max: 1.0000
```

---

## Related Commands

- [`bench subset`](spec-subset.md): Materialise a sampled slice as a pinned JSONL artifact and provenance manifest, suitable for `git add` and CI pinning.
