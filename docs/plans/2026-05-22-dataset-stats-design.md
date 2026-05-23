# Zero-Cost Preview of SWE-bench Dataset Composition (`bench dataset-stats`)

This design document outlines the architecture, components, data flows, and testing strategy for implementing the `bench dataset-stats` subcommand in a test-driven (TDD) manner.

## Design Highlights

### 1. Token Estimation Strategy
Instead of naive word counting or character division, we leverage `litellm-rs`'s built-in `TokenCounter`:
```rust
use litellm_rs::utils::ai::counter::token_counter::TokenCounter;
```
We initialize the counter and invoke `count_completion_tokens(model, problem_statement)` to calculate accurate, model-specific tokens for each instance's problem statement. This provides highly realistic token counts without making any network/model calls.

### 2. Multi-Tiered Language Detection
We implement a highly accurate, extension-based language detection mechanism:
1. **Patch Diff Inspection**: We inspect `patch` and `test_patch` strings in `other` map. We scan for diff header lines starting with `--- a/` or `+++ b/`.
2. **Extension Extraction**: We extract the file extensions (e.g. `.py` -> Python, `.rs` -> Rust, `.js`/`.ts` -> JavaScript/TypeScript, `.go` -> Go, `.java` -> Java).
3. **Fallback Registry**: If no diff is present or no standard extensions are found, we fall back to:
   - Case-insensitive substrings of the `repo` name (e.g., `django`, `sympy`, `pytest` -> Python).
   - If still undetermined, defaults gracefully to `Python` (as is typical in standard SWE-bench).

### 3. Historical Result Integration
We scan the local `runs` root directory (defaulting to `./runs`) recursively for completed sweep runs:
1. Identify all directories containing `results.json`.
2. Parse `results.json` to verify the `manifest.dataset.sha256` matches the current dataset's hash.
3. If matched, extract each instance's `resolved_count` and `runs` attempts.
4. Calculate per-instance historical resolved rates (`sum(resolved_count) / sum(runs)`).
5. For all instances in the selected slice that have historical data, calculate the `min`, `p50` (median), and `max` resolved rates.

### 4. Representativeness & Skew Analysis
We compare the selected slice against the entire dataset:
- **Repo Skew**: If the slice unique repos count is less than 50% of the entire dataset's unique repos count, we warn of a skew.
- **Median Token Skew**: If the slice median problem-statement token length differs by more than 25% from the entire dataset's median token length, we warn of a skew.
- In JSON mode, a boolean `slice_skew` field is included. In Text mode, a warning is printed to stdout.

---

## Command Interface

```bash
bench dataset-stats --dataset-path <PATH> [SUBSET_SELECTORS...] [--format <text|json>]
```
or with a dataset alias:
```bash
bench dataset-stats --dataset <lite|verified|full> --split <test|dev|train> [SUBSET_SELECTORS...] [--format <text|json>]
```

## JSON Schema Output
A stable, schema-versioned, deterministically ordered `dataset-stats.json`:
```json
{
  "schema_version": 1,
  "dataset_hash": "...",
  "dataset_path": "...",
  "subset_selector": {
    "limit": null,
    "sample": 10,
    "seed": 42,
    "instance_ids": null,
    "stratify_by": null,
    "stratify_mode": null
  },
  "total_instances": 10,
  "repos": [
    { "repo": "django/django", "count": 8, "percent_share": 80.0 },
    { "repo": "pytest-dev/pytest", "count": 2, "percent_share": 20.0 }
  ],
  "problem_statement_tokens": {
    "min": 10,
    "p50": 100,
    "p90": 250,
    "max": 500
  },
  "expected_tests": {
    "min": 0,
    "p50": 4,
    "p90": 12,
    "max": 20
  },
  "languages": ["Python"],
  "historical_resolved_rate": {
    "min": 0.0,
    "p50": 0.5,
    "max": 1.0
  },
  "slice_skew": false
}
```
