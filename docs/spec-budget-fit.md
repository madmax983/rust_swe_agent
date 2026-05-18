# `bench budget-fit` — Budget Right-Sizing Recommendations

## Overview

`bench budget-fit` reads a completed sweep directory and produces right-sized
recommendations for three independent budget knobs: **step limit**, **cost cap**,
and **wallclock timeout**. It reads **only on-disk artifacts and never re-runs
instances, calls a model, or modifies the input sweep** (zero-cost guarantee).

Sizing budget caps is the most-iterated configuration decision in a sweep loop:

- **Too tight**: resolvable instances die at the cap, inflating the unresolved
  rate. An operator may misread this as a prompt or model regression.
- **Too loose**: budget pours into instances that were never going to resolve,
  raising cost-per-resolved-instance without moving the resolve rate.

`bench budget-fit` closes the feedback loop between `bench forecast` (pre-flight
cost estimate) and the next sweep's config: given what just happened, what
caps should I set next time?

## Usage

```
bench budget-fit --sweep <DIR> [OPTIONS]
```

### Required

| Argument | Description |
|---|---|
| `--sweep <DIR>` | Completed sweep directory produced by `bench swebench` |

### Optional

| Flag | Default | Description |
|---|---|---|
| `--format <FMT>` | `text` | Output format: `text` or `json` |
| `--axis <AXIS>` | *(all three)* | Restrict to one axis: `steps`, `cost_usd`, `wall_clock_s` |
| `--at-cap-tolerance <FRAC>` | `0.05` | Fraction of cap within which an instance counts as "at-cap" (range 0.0–0.5) |
| `--target-percentile <PCT>` | `95` | Percentile of the resolved distribution used for `recommended_cap` (range 50–99) |
| `--filter <KEY=VALUE>` | *(none)* | Key=value filter applied before analysis (same syntax as `bench inspect --filter`; may repeat) |

## Output

`bench budget-fit` always writes `budget-fit.json` to the sweep directory **and**
prints a text (or JSON) summary to stdout. Exit code is `0` on success regardless
of whether the recommendation is to raise, tighten, or maintain caps.

## Three Budget Axes

Each axis is analyzed independently with the same algorithm.

### `steps`

- **Source value**: `InstanceResult.steps` (per-trajectory step count)
- **Cap source**: `--step-limit` from `manifest.cli.argv`
- **Unit**: steps (rounded up to 1 step)
- **Cap-bound failure category**: `step_limit`

### `cost_usd`

- **Source value**: `InstanceResult.cost_usd` (per-instance USD spend)
- **Cap source**: `results.json[cost_limit_usd]` (sweep-level), or
  `--per-task-budget-usd` from `manifest.cli.argv` (per-instance)
- **Unit**: USD (rounded up to $0.01)
- **Cap-bound failure category**: `cost_limit`

### `wall_clock_s`

- **Source value**: `InstanceResult.duration_secs` (per-instance wall-clock seconds)
- **Cap source**: `--task-timeout-secs` from `manifest.cli.argv`
- **Unit**: seconds (rounded up to 1 second)
- **Cap-bound failure category**: `wallclock_timeout`

## Recommendation Algorithm

For each axis, given the set of instance values split by outcome:

### 1. Outcome Buckets

| Bucket | Definition |
|---|---|
| `resolved` | `resolved_count > 0` |
| `unresolved_cap_bound` | `failure_category` matches this axis's cap category |
| `unresolved_other` | All other unresolved/submitted-but-unresolved outcomes |
| `errored` | Instances with a non-cap failure category |

### 2. Distribution Statistics

For each bucket, compute: `count`, `p10`, `p50`, `p90`, `p95`, `p99`, `max`, `mean`.

Percentiles use linear interpolation: `p = sorted[lo] + frac × (sorted[hi] − sorted[lo])`.

### 3. `recommended_cap` (Primary Recommendation)

**a. No cap configured**: `recommended_cap = null`. Rationale: "no cap configured for this axis."

**b. Behavior enrichment available** (when `behavior.json` is present with `per_instance` data):

- For each `unresolved_cap_bound` instance, look up its dominant action class.
- If the majority are **progress classes** (`write`, `test`, `build`):
  - `recommended_action = raise`
  - `recommended_cap = round_up(configured_cap × 1.5, unit)`
  - Rationale explains that raising is recommended and how many progress instances exist.
- If the majority are **stuck classes** (`noop`, `read`, `nav`):
  - `recommended_action = tighten`
  - `recommended_cap = P{target_percentile}(resolved)`, rounded up
  - Rationale explains that raising is unlikely to help.

**c. No behavior enrichment** (default):

- `recommended_cap = P{target_percentile}(resolved)`, rounded up to unit.
- If `recommended_cap ≥ configured_cap`: cap is already well-sized; rationale notes no tightening needed.
- If `recommended_cap < configured_cap`: rationale describes tightening potential headroom.
- If no resolved instances: `recommended_cap = null`.

### 4. `at_cap_count` / `at_cap_share`

Instances within `at_cap_tolerance` (default 5%) of the configured cap:

```
at_cap if value >= configured_cap × (1 − at_cap_tolerance)
```

Includes resolved instances that finished near the cap (useful to know how much headroom resolved instances actually needed).

### 5. Projected Impact

**`projected_impact_if_recommended`** describes the estimated impact of changing the cap to `recommended_cap`:

- **Raise** (behavior-enriched, progress class): conservative lower bound on resolved delta = number of progress-class cap-bound instances. Cost delta = `mean_cost_per_unit × (new_cap − old_cap) × progress_instance_count`.
- **Tighten** (default): resolved delta = negative count of resolved instances that need more than the new cap. Cost delta = negative savings estimate.

**`projected_impact_if_tightened_to_p95`** always describes tightening to P95 of resolved (differs from above when `--target-percentile ≠ 95`).

### 6. Cross-Axis Summary

| Field | Definition |
|---|---|
| `dominant_axis` | Axis with the most `unresolved_cap_bound` instances. `null` if none. |
| `dominant_axis_reason` | Human-readable explanation. |
| `waste_estimate_usd` | Conservative estimate of USD spent on cap-bound failures (using cost_usd axis). |
| `headline_recommendation` | One sentence suitable for a sweep-config PR. |

## `bench behavior` Integration

When `behavior.json` is present alongside the sweep (produced by `bench behavior --per-instance`), the recommendation for the `steps` axis is enriched:

- **Progress-class cap-bound instances** (`write`/`test`/`build` as dominant action class): raising the cap is likely to help resolve them. The `recommended_cap` is set to `configured_cap × 1.5`.
- **Stuck-class cap-bound instances** (`noop`/`read`/`nav` as dominant action class): raising the cap is unlikely to help. Tighten to P95 of resolved instead.

When `behavior.json` is absent, the enrichment is silently omitted. The recommendation still works on raw distributions.

## Missing-Data Behavior

When an axis has no cap configured (e.g., a sweep run without `--task-timeout-secs`):

- `configured_cap: null`
- `recommended_cap: null`
- `recommended_cap_rationale: "no cap configured for this axis"`
- All distribution stats are still computed from available instance data.
- The remaining axes still produce recommendations.

## Determinism

Running `bench budget-fit` twice on the same sweep produces byte-identical `budget-fit.json` (modulo `generated_at`). Cost computations are rounded to 6 decimal places to prevent floating-point ordering differences.

## Redaction

Instance IDs and path-shaped fields written to `budget-fit.json` are passed through the existing redaction pipeline when used with `bench bundle`. The artifact reads from `results.json` which is already redacted at write time.

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success — recommendation produced (any direction: raise, tighten, or maintain) |
| `0` | Success — no cap-bound failures detected (finding: tighten, not error) |
| `1` | Internal error (I/O failure, JSON parse error) |
| `2` | Usage error (bad flag value, unknown axis, invalid tolerance range) |

`bench budget-fit` **never** exits non-zero just because no instances hit a cap.

## Schema (`budget-fit.json`)

```json
{
  "sweep": "my-sweep-dir",
  "generated_at": "2026-05-01T10:00:00Z",
  "axes": [
    {
      "axis_name": "steps",
      "configured_cap": 60.0,
      "configured_cap_source": "manifest.cli.argv[--step-limit]",
      "unit": "steps",
      "distribution_by_outcome": {
        "resolved": { "count": 24, "p10": 8.0, "p50": 22.0, "p90": 28.0, "p95": 30.0, "p99": 32.0, "max": 35.0, "mean": 20.5 },
        "unresolved_cap_bound": { "count": 6, "p10": 60.0, "p50": 60.0, "p90": 60.0, "p95": 60.0, "p99": 60.0, "max": 60.0, "mean": 60.0 },
        "unresolved_other": { "count": 0, "p10": null, ... },
        "errored": { "count": 0, "p10": null, ... }
      },
      "at_cap_count": 6,
      "at_cap_share": 0.2,
      "recommended_cap": 30.0,
      "recommended_cap_rationale": "P95 of resolved is 30.0000 (current cap: 60.0000); tightening frees 30.0000 steps of headroom.",
      "projected_impact_if_recommended": {
        "estimated_resolved_delta": 0,
        "estimated_cost_delta_usd": -0.045,
        "derivation": "Tightening from 60.0000 to 30.0000 (P95 of resolved): ..."
      },
      "projected_impact_if_tightened_to_p95": { ... }
    },
    ...
  ],
  "summary": {
    "dominant_axis": "steps",
    "dominant_axis_reason": "6 cap-bound unresolved instance(s) on axis 'steps'",
    "waste_estimate_usd": 1.23,
    "headline_recommendation": "Dominant axis 'steps': set cap to 30.0000 (steps); ..."
  }
}
```

## Worked Examples

### Over-provisioned Cap (AC a)

**Setup**: 10 resolved instances all finishing in ≤40 steps; step cap = 80.

**Result**:
- `recommended_cap = 40` (P95 of resolved ≈ 40, rounded up)
- `projected_impact_if_recommended.estimated_resolved_delta = 0` (tightening from 80→40 doesn't lose any resolved instances since all finish in ≤40)
- `projected_impact_if_recommended.estimated_cost_delta_usd ≤ 0` (cost savings or neutral)

### Under-provisioned Cap with Progress (AC b)

**Setup**: 2 resolved, 8 hit `step_limit`, all cap-bound with `write`-class final actions (from `behavior.json`); step cap = 30.

**Result**:
- `recommended_cap = 45` (configured_cap × 1.5 = 30 × 1.5 = 45, rounded up)
- Rationale mentions raising is recommended
- `projected_impact_if_recommended.estimated_resolved_delta = 8` (conservative: all progress-class cap-bound instances may resolve)

### Under-provisioned Cap with Stuck Instances (AC c)

**Setup**: 2 resolved, 8 hit `step_limit`, all cap-bound with `noop`-class final actions; step cap = 30.

**Result**:
- `recommended_cap = 25` (P95 of 2 resolved instances, e.g.)
- Rationale: "8 cap-bound instance(s) had stuck-class actions (noop/read/nav); raising the cap is unlikely to help."
- `projected_impact_if_recommended.estimated_resolved_delta ≤ 0`

### No Wall-Clock Timeout (AC d)

**Setup**: Sweep run without `--task-timeout-secs`.

**Result** (wall_clock_s axis):
- `configured_cap = null`
- `recommended_cap = null`
- `recommended_cap_rationale = "no cap configured for this axis"`
- Steps and cost_usd axes still produce recommendations normally.

## Non-Goals

- **Closed-loop auto-tuning**: descriptive recommendations only; the operator copies the number into config.
- **Cross-sweep budget-fit**: single-sweep analysis only.
- **Per-difficulty stratification**: single global cap recommendation.
- **Significance gating** on `estimated_resolved_delta`: point estimates with conservative bounds.
- **Provider-side rate-limit budgets**: wall_clock_s captures the observable effect.
- **Recommendations above hard budget constraints**: guardrails from the manifest are respected.
