# `bench compare` — Significance Testing

`bench compare` computes a paired statistical significance test on the
resolved-rate delta whenever two sweeps share at least one `instance_id`.
The result is emitted in the JSON report as `resolved_rate_significance` and
rendered as a single `Significance:` line in text output.

## Why paired testing?

`bench compare` already joins sweeps on `instance_id`. Pairing exploits the
correlation between sweeps run on the same instances and provides far more
power to detect real deltas than unpaired (two-proportion z-test) comparisons.
A swing from 18% → 22% resolved on N≈50 is indistinguishable from noise with
unpaired tests; paired McNemar makes that explicit.

## Test: exact McNemar

The chosen test is the **two-sided exact McNemar test** on the paired overlap
subset.

Define:
- **n₀₁** (`pass_to_fail`): instances that passed in the baseline but failed in the candidate.
- **n₁₀** (`fail_to_pass`): instances that failed in the baseline but passed in the candidate.
- **n** = n₀₁ + n₁₀ (total discordant pairs).

Under H₀ (no difference), each discordant pair is equally likely to go either
direction, so the smaller count follows Binomial(n, 0.5). The two-sided
p-value is:

```
p = 2 × Σ_{k=0}^{min(n₀₁,n₁₀)} C(n,k) × 0.5ⁿ, capped at 1.0
```

Computation is performed in log-space to avoid floating-point overflow for
large n. The test is always exact (no chi-square approximation); it is valid
for any n > 0.

**Default alpha**: there is no default alpha. Operators must opt in to gating
via `--min-significance` or `--regression-significance`.

## 95% Confidence Interval

The `ci95_lower_pp` / `ci95_upper_pp` fields (and the text `[95% CI: …]` line)
are computed using the Wilson-score delta method on the **paired** overlap
subset, not on the full sweep populations. This is the same Wilson CI method
used by the existing `resolved_delta_ci95` field (which uses the full
populations).

## Underpowered threshold

When the paired test has fewer than **10 discordant pairs** (or `paired_n = 0`)
the `underpowered` flag is set to `true` with a `underpowered_reason`.

This threshold reflects the minimum number of discordant pairs needed for the
exact McNemar test to have any practical power to distinguish signal from noise
at conventional alpha levels. Below this threshold the p-value is still
computed (and shown), but operators should interpret it with extreme caution.

Text output appends `(underpowered)` to the significance line. JSON consumers
should check `underpowered` before acting on `p_value`.

## Instances excluded from the test

Instances present in only one sweep are **excluded** from the paired test.
They are counted in `only_in_baseline` and `only_in_candidate` in the JSON
block. The paired counts (`paired_n`, `pass_to_fail`, `fail_to_pass`) cover
only the overlap.

## CLI flags

### `--min-significance <alpha>`

Exit non-zero when the resolved-rate delta is **positive** but `p_value >
alpha`. This gate blocks suspected-noise wins from passing CI.

- The gate only fires when `resolved_delta_rate > 0`.
- If the test is underpowered and `--allow-underpowered` is not set, the gate
  exits non-zero regardless of p-value.
- Example: `--min-significance 0.05`

### `--regression-significance <alpha>`

Exit non-zero when the resolved-rate delta is **negative** and `p_value <=
alpha` (significant regression). Insignificant regressions are not blocked by
this flag, preserving the existing `--max-regressions` behavior independently.

- The gate only fires when `resolved_delta_rate < 0`.
- If the test is underpowered and `--allow-underpowered` is not set, the gate
  exits non-zero regardless of p-value.
- Example: `--regression-significance 0.05`

### `--allow-underpowered`

When set, significance-based gating proceeds using only the p-value (ignoring
the `underpowered` flag). Without this flag, any underpowered result causes
gating to exit non-zero when a significance gate is active.

Use this flag when you understand the low-power context and still want to gate
on the computed p-value (e.g., 7 discordant pairs, p=0.016 < 0.05).

## Operator decision recipes

| Scenario | Recommendation |
|---|---|
| `underpowered=true` | Run more instances before gating on significance. |
| `p > 0.05`, positive delta | Likely noise; do not ship. Use `--min-significance 0.05`. |
| `p < 0.05`, positive delta | Evidence of real improvement. Safe to promote. |
| `p < 0.05`, negative delta | Significant regression; block with `--regression-significance 0.05`. |
| `p > 0.05`, negative delta | Insignificant regression; use `--max-regressions` for a raw count gate instead. |

## JSON schema

```json
{
  "resolved_rate_significance": {
    "test_name": "mcnemar_exact",
    "p_value": 0.00195,
    "ci95_lower_pp": 12.34,
    "ci95_upper_pp": 45.67,
    "paired_n": 100,
    "pass_to_fail": 2,
    "fail_to_pass": 12,
    "underpowered": false,
    "underpowered_reason": null,
    "only_in_baseline": 0,
    "only_in_candidate": 0
  }
}
```

Fields:

| Field | Type | Description |
|---|---|---|
| `test_name` | string | Always `"mcnemar_exact"`. |
| `p_value` | number or null | Two-sided exact McNemar p-value. `null` when `paired_n == 0`. |
| `ci95_lower_pp` | number | Lower bound of 95% Wilson-score CI on rate delta (percentage points). |
| `ci95_upper_pp` | number | Upper bound of 95% Wilson-score CI on rate delta (percentage points). |
| `paired_n` | integer | Instances present in both sweeps (overlap). |
| `pass_to_fail` | integer | Discordant pairs: baseline pass, candidate fail. |
| `fail_to_pass` | integer | Discordant pairs: baseline fail, candidate pass. |
| `underpowered` | boolean | True when `discordant < 10` or `paired_n == 0`. |
| `underpowered_reason` | string or null | Human-readable reason; null when powered. |
| `only_in_baseline` | integer | Instances present only in the baseline sweep. |
| `only_in_candidate` | integer | Instances present only in the candidate sweep. |
