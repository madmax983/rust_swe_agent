# `bench contamination-check` — Spec

## Purpose

Surface training-leakage signals on resolved SWE-bench instances.

SWE-bench resolved-rates are increasingly distrusted because public benchmark
datasets overlap with model pretraining data. An agent that "resolves"
`django__django-12345` may have recited a memorised patch rather than reasoned
to the fix. This command does not *prove* contamination — it *surfaces signals*
operators can investigate. See the Non-Goals section.

**Supporting failure-mode citation**: SWE-bench+ contamination audits
(Yang et al., 2024) show non-trivial resolved-rate inflation from benchmark
leakage. Trajectory-observable proxies (read order, patch similarity, step
velocity) correlate with that inflation.

---

## Command

```
bench contamination-check --sweep <DIR>
  [--output <PATH>]
  [--config <TOML>]
  [--fail-on-high <THRESHOLD>]
```

- **Zero model calls. Zero network.** Reads only on-disk trajectory artifacts
  and writes `contamination.json` to the sweep directory.
- Only *resolved* instances are scored; unresolved and errored instances are
  excluded because patch quality cannot be evaluated.

---

## Signals

Four deterministic signals are extracted from the trajectory:

### 1. `edit_before_read_ratio` (weight: 0.40)

Fraction of edited files that were never `cat`/`head`/`tail`-read in the
trajectory **before** the first edit of that file.

| Value | Interpretation |
|-------|----------------|
| 0.0   | All edited files were inspected before patching (exploratory) |
| 1.0   | No edited file was read beforehand (recited patch) |

**Why**: A genuine solver reads the relevant source locations before modifying
them. An agent reciting a memorised patch often edits without reading.

### 2. `patch_similarity_to_gold` (weight: 0.30)

Normalised similarity between the agent's submitted `.patch` and the dataset's
gold patch (field `patch` in the dataset JSONL row). Computed as
`1 − normalised_edit_distance(agent_patch, gold_patch)`.

| Value | Interpretation |
|-------|----------------|
| 0.0   | Patches are completely different |
| 1.0   | Agent patch is identical to gold patch |

**Why**: Near-identical patches are a direct leakage indicator. A high score
combined with a short time-to-first-edit is the strongest contamination signal.

> **Note**: This signal requires the dataset JSONL to be available. When absent
> or not supplied, the signal defaults to `0.0` (conservative / no penalty).
> In the current trajectory-only implementation the signal is always `0.0`.

### 3. `time_to_first_edit` (weight: 0.20)

Suspicion contribution from how early the first file edit occurs.

Formula: `max(0, 1 − first_edit_step / (total_steps − 1))`

| Value | Interpretation |
|-------|----------------|
| 0.0   | First edit at the last step (lots of exploration first) |
| 1.0   | First edit at step 0 (immediate, no exploration) |

**Why**: Genuine problem-solving requires understanding the codebase before
writing a fix. Agents editing at step 0 have skipped that exploration.

### 4. `verbatim_recall` (weight: 0.10)

`1.0` when an assistant message contains a verbatim token run of length ≥ N
(default 20 whitespace-split tokens) that matches the gold patch surface form,
and this occurs **before** the trajectory shows the agent reading the relevant
file location. `0.0` otherwise.

**Why**: Reproducing exact patch text before observing the source is a strong
signal that the patch was recalled from training data rather than derived from
the trajectory.

> **Note**: This signal requires a gold patch to be available. In the current
> trajectory-only implementation the signal is always `0.0`.

---

## Leakage Score

```
leakage_score = clamp(
    w1 * edit_before_read_ratio
  + w2 * patch_similarity_to_gold
  + w3 * time_to_first_edit
  + w4 * verbatim_recall,
  0.0, 1.0
)
```

Default weights sum to 1.0; the clamping handles edge cases where individual
signals push the sum outside `[0.0, 1.0]`.

---

## Risk Tiers

| Tier   | Score range              | Interpretation |
|--------|--------------------------|----------------|
| `low`  | score < 0.30             | No strong leakage signals |
| `medium` | 0.30 ≤ score < 0.60  | Some signals; manual review recommended |
| `high` | score ≥ 0.60             | Multiple strong signals; prioritise for audit |

---

## Configuration (TOML)

Weights and thresholds are configurable via a TOML file passed with `--config`.
All keys are optional; omitted keys fall back to their defaults.

```toml
# contamination-check.toml

# Signal weights (all default to values shown)
weight_edit_before_read  = 0.40
weight_patch_similarity  = 0.30
weight_time_to_first_edit = 0.20
weight_verbatim_recall   = 0.10

# Risk tier thresholds (inclusive lower bounds)
medium_threshold = 0.30
high_threshold   = 0.60

# Minimum token run for verbatim-recall detection
verbatim_recall_min_tokens = 20
```

---

## Output: `contamination.json`

```json
{
  "schema": "contamination-check-v1",
  "sweep_path": "/abs/path/to/sweep",
  "instances": [
    {
      "instance_id": "django__django-12345",
      "leakage_score": 0.82,
      "risk_tier": "high",
      "signals": {
        "edit_before_read_ratio": 1.0,
        "patch_similarity_to_gold": 0.95,
        "time_to_first_edit": 1.0,
        "verbatim_recall": 0.0
      }
    }
  ],
  "summary": {
    "total_resolved": 50,
    "low_count": 38,
    "medium_count": 8,
    "high_count": 4,
    "high_risk_share": 0.08,
    "contamination_adjusted_resolved_rate": 0.92
  }
}
```

The `instances` array is sorted lexicographically by `instance_id` for
determinism. The `metadata` block (not shown) may contain a run timestamp and
is the only non-deterministic section across repeated runs on identical input.

---

## CI Gate: `--fail-on-high <THRESHOLD>`

```bash
bench contamination-check --sweep runs/my-sweep --fail-on-high 0.10
```

Exits non-zero (exit code 3 / `PreflightFailure`) when
`high_risk_share > threshold`. Default is exit 0 (informational only).

Use `--fail-on-high 0.0` to fail on any high-risk instance.

---

## Integration with `bench compare`

```bash
bench compare \
  --baseline runs/baseline \
  --candidate runs/candidate \
  --contamination runs/candidate/contamination.json
```

When `--contamination` is supplied, the compare report appends a
contamination-adjusted resolved-rate section:

```
contamination-adjusted resolved-rate (candidate):
  raw resolved-rate       : 62.0%
  high-risk instances     : 4 of 50 resolved (8.0%)
  contamination-adjusted  : 57.0%
```

Formula: `raw_resolved_rate × (1 − high_risk_share)`

---

## Non-Goals

This command **surfaces signals** for operator investigation. It does **not**:

- **Prove contamination** at the model-weights level. Trajectory signals are
  correlates, not proof. A fast patch may be the result of a simple bug.
- **Auto-disqualify** resolved instances. Report only; the operator decides.
- **Detect model memorisation** via membership inference or perplexity probes.
  Zero new model calls is a hard constraint.
- **Track cross-sweep contamination** (historical instance patterns). That
  belongs in a follow-up joining `instance-history` with this output.
- **Detect evaluator contamination** — covered by `bench eval-flake` (issue
  #294).
- **Probe the model** with synthetic prompts to elicit memorisation. Trajectory
  signals only.
