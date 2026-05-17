# `bench instance-history` — Longitudinal Instance Stability Analysis

## Overview

`bench instance-history` is a read-only command that joins multiple historical
sweep runs on `instance_id` and reports, per instance, its resolution history,
stability class, and flip provenance.

This is the **longitudinal** complement to the point-in-time views already
provided by `bench inspect`, `bench compare`, and `bench matrix`.

## Usage

```
bench instance-history \
  --sweeps <DIR> [--sweeps <DIR> ...] \
  [--output <PATH>]            # default: instance-history.json in cwd
  [--format text|json]         # default: text
  [--stable-threshold <FLOAT>] # default: 1.0
  [--require-full-coverage]
  [--max-partial-share <FLOAT>]
  [--class <CLASS>]
  [--top <N>]                  # default: 50
  [--focus]
```

## Sweep Discovery

`--sweeps` accepts the path of a completed sweep directory (i.e. a directory
that contains a readable `results.json`). The flag may be repeated any number
of times. At least **2 distinct, readable sweeps** are required; with fewer
the command exits non-zero with a clear message.

Sweep directories that lack a readable `results.json` are **skipped** with a
warning emitted to stderr. They are listed under `skipped_sweeps` in the output
artifact and are not counted toward `sweep_count`.

## Dataset Coherence

The command computes the **intersection** of `instance_id`s across all selected
sweeps and runs the longitudinal analysis on that intersection only. Instances
that appear in only a strict subset of sweeps are summarised in the
`partial_coverage` block:

```json
{
  "instance_id": "django__django-12345",
  "sweeps_seen_in": ["sweep-a", "sweep-b"],
  "sweeps_missing_from": ["sweep-c"]
}
```

Instances in `partial_coverage` are excluded from stability classification so
the math remains honest.

### `--require-full-coverage`

When this flag is set the command exits non-zero if:
- the intersection is empty, OR
- the fraction of partial instances exceeds `--max-partial-share` (default 0.0,
  i.e. zero tolerance for partial coverage).

## Stability Classification

Each instance in the intersection is assigned a `stability_class` based on its
resolved rate across the selected sweeps.

### Default threshold (`--stable-threshold 1.0`)

| Class | Condition |
|-------|-----------|
| `stable_win` | resolved in **all** N sweeps (rate = 1.0) |
| `stable_loss` | resolved in **no** sweep (rate = 0.0) |
| `flipper` | resolved in some but not all sweeps (0 < rate < 1) |

The `unstable_minority_win` and `unstable_minority_loss` classes do not appear
at the default threshold — every non-extreme instance is called a `flipper`.

### Relaxed threshold (`--stable-threshold <T>`, where T < 1.0)

The middle band of the default `flipper` bucket is subdivided:

| Class | Condition |
|-------|-----------|
| `stable_win` | rate == 1.0 (always N/N) or rate > T |
| `stable_loss` | rate == 0.0 (always 0/N) or rate < (1 − T) |
| `unstable_minority_win` | rate > 0.5 and rate ≤ T |
| `unstable_minority_loss` | rate ≤ 0.5 and rate ≥ (1 − T) |
| `flipper` | rate == 0.5 exactly |

**Example:** `--stable-threshold 0.9`

An instance resolved in 7 of 10 sweeps (rate = 0.70):
- At T = 1.0: `flipper` (not N/N or 0/N)
- At T = 0.9: `unstable_minority_win` (0.70 > 0.5, 0.70 ≤ 0.9)

An instance resolved in 9 of 10 sweeps (rate = 0.90):
- At T = 1.0: `flipper`
- At T = 0.9: `unstable_minority_win` (0.90 > 0.5, 0.90 ≤ 0.9 — boundary uses `≤`)

## Per-instance Schema (`instances[]`)

```json
{
  "instance_id": "django__django-12345",
  "resolved_count": 2,
  "total_runs": 3,
  "resolved_rate": 0.6666,
  "stability_class": "flipper",
  "sweep_outcomes": [
    {
      "sweep_id": "sweep-a",
      "sweep_path": "/runs/sweep-a",
      "finished_at": "2026-05-01T01:00:00Z",
      "resolved": true,
      "errored": false
    }
  ],
  "flip_events": [
    {
      "from_sweep": "sweep-a",
      "to_sweep": "sweep-b",
      "direction": "win→loss"
    }
  ],
  "last_flip": { ... }
}
```

`sweep_outcomes` is ordered by `finished_at` ascending; ties are broken by
`sweep_id` lexicographic order. `flip_events` is derived from consecutive pairs
in `sweep_outcomes`, so it inherits the same ordering guarantee.

## Top-level Schema (`instance-history.json`)

```json
{
  "generated_at": "2026-05-17T12:00:00Z",
  "tool_version": "0.1.0",
  "sweep_count": 3,
  "intersection_size": 28,
  "stability_counts": {
    "stable_win": 18,
    "stable_loss": 8,
    "flipper": 2,
    "unstable_minority_win": 0,
    "unstable_minority_loss": 0
  },
  "flipper_share": 0.0714,
  "dataset_signature": "3f2a1b9e8c7d4f5a",
  "partial_coverage_count": 2,
  "skipped_sweeps": [],
  "instances": [ ... ],
  "partial_coverage": [ ... ]
}
```

`dataset_signature` is a content hash of the sorted intersection `instance_id`
list. Two `instance-history.json` artifacts over the same dataset and the same
sweeps will carry the same signature (assuming the same sweep set).

## Text Output and Ranking

The text table ranks instances by **operator value**:

1. `flipper` — sorted by `|rate − 0.5|` ascending (most-balanced first), then
   by `instance_id` lex.
2. `unstable_minority_win`
3. `unstable_minority_loss`
4. `stable_loss`
5. `stable_win`

The table is truncated at `--top N` rows (default 50). Use `--class <CLASS>` to
restrict the table to a specific stability class.

### `--focus` (operator quick-glance mode)

`--focus` is equivalent to `--class flipper`. It prints only the flipper subset
and is the recommended copy-paste-into-prompt-iteration view. Example:

```
bench instance-history \
  --sweeps runs/sweep-a --sweeps runs/sweep-b --sweeps runs/sweep-c \
  --focus
```

## Exit Codes

Follows the stable contract from `docs/exit-codes.md`:

| Code | Condition |
|------|-----------|
| 0 | Success |
| 2 | Fewer than 2 valid sweeps, invalid flags, `--require-full-coverage` failure |
| 1 | Unreadable artifacts, I/O failure |

## Determinism

Running `bench instance-history` twice on the same set of sweep paths produces
**byte-identical** `instance-history.json` (modulo `generated_at`). Sweep
ordering inside per-instance `sweep_outcomes` is by `finished_at` ascending,
with ties broken by `sweep_id` lex order.

## Non-goals

- **Causal attribution** of a flip to a specific prompt/model/config change.
  `flip_events` names the adjacent sweeps; isolating _which_ knob moved is left
  to the operator combined with `bench compare`.
- **Significance gating** on flip frequency. This command emits descriptive
  statistics only; statistical tests are a future extension.
- **Cost-weighted ranking** (e.g. "flippers ranked by $/flip").
- **Cross-dataset analysis.** `dataset_signature` rejects joins across datasets
  with different instance sets by construction.
- **Mutating input sweeps** or copying flippers into a new dataset slice.
- **Live/streaming view** during a running sweep; `bench tail` owns that channel.
- **Sampling noise within a single sweep** (`--samples N` / pass@k territory).

## Worked Examples

### Example 1: Three sweeps, one known flipper

```
$ bench instance-history \
    --sweeps runs/2026-05-01 \
    --sweeps runs/2026-05-02 \
    --sweeps runs/2026-05-03
```

Output excerpt:
```
instance-history: 3 sweeps, 5 instances in intersection
  stable_win=2  stable_loss=2  flipper=1  minority_win=0  minority_loss=0
  flipper_share=20.0%

instance_id                          stability_class         resolved    rate  last_flip
----------------------------------------------------------------------...
django__django-99999                 flipper          2/3    66.7%  loss→win
```

### Example 2: Partial coverage

Suppose sweep-c only contains a subset of the instances in sweep-a and sweep-b:

```json
"partial_coverage": [
  {
    "instance_id": "psf__requests-1234",
    "sweeps_seen_in": ["sweep-a", "sweep-b"],
    "sweeps_missing_from": ["sweep-c"]
  }
]
```

Use `--require-full-coverage --max-partial-share 0.1` to exit non-zero when
more than 10% of unique instances have partial coverage.

### Example 3: Relaxed threshold

```
bench instance-history \
  --sweeps runs/sweep-{a..j} \   # 10 sweeps
  --stable-threshold 0.9
```

An instance resolved 7/10 times (rate = 0.70) will appear as
`unstable_minority_win` instead of `flipper`.
