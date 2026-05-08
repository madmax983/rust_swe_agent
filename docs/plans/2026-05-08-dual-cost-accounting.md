# Dual Cost Accounting Plan

## Goal

Persist and report two separate cost numbers for run artifacts:

- **Actual cost**: the cost reported or inferred for the model/provider that was actually used.
- **Baseline cost**: a counterfactual estimate for the same token usage using the configured baseline model rate card.

This prevents free or discounted providers from being displayed as if they incurred the baseline model cost, while still preserving normalized comparisons.

## Boundary

Observable behavior:

- A stored actual cost of `0.0` stays `0.0` even when token counts are non-zero.
- Reports show actual cost and baseline cost separately where sweep totals are displayed.
- Existing artifacts that only have `cost_usd` or `total_cost_usd` still load.
- Current artifacts include typed source metadata for actual cost and the baseline model used for the counterfactual.

Schema behavior:

- Additive artifact fields require a minor schema bump.
- Readers continue to accept older minor versions as legacy-compatible artifacts.

## TDD Steps

1. Add red tests proving zero actual cost is not replaced by token-derived baseline cost.
2. Add red tests for sweep summary output showing both actual and baseline totals.
3. Add red tests for serialized current artifacts containing the new cost metadata.
4. Implement the smallest typed cost model and writers to satisfy the tests.
5. Refactor call sites away from ambiguous `effective_cost_usd` where the UI/report needs explicit actual vs baseline behavior.
6. Run targeted tests, `cargo fmt`, clippy, and the broad test suite with the known unrelated local-git failures skipped if they still fail.

## Types

Use a Rust enum for actual cost provenance:

```rust
pub enum CostSource {
    ProviderReported,
    RateCardEstimate,
    FreeTierInferred,
    Unknown,
}
```

Keep baseline provenance explicit through `baseline_cost_model`.
