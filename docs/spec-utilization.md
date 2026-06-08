# `bench utilization` — Sweep Concurrency Efficiency Report

## Overview

`bench utilization` reads a completed sweep directory and reports how
efficiently the configured concurrency (`--parallel`) was actually used. It
reads **only on-disk artifacts** (the sweep manifest and per-instance results)
and **never re-runs instances, calls a model, starts a container, or modifies
the input sweep** — it is a zero-cost, read-only measurement.

Operators choose `--parallel <N>` for sweeps by guesswork and pay for it twice:
too low and the sweep runs longer (wasted wallclock); too high and workers idle
while Docker / network / rate-limits throttle real throughput (wasted machine
cost with no speedup). Nothing else in the harness answers *"did my 8 workers
behave like 8 workers, or like 3?"* — `bench tail` is live-only, `bench report`
reports outcomes, and `bench budget-fit` right-sizes per-task *caps*. The inputs
to answer it already exist on disk; this command joins them.

## Usage

```
bench utilization --sweep <DIR> [OPTIONS]
```

### Required

| Argument | Description |
|---|---|
| `--sweep <DIR>` | Completed sweep directory produced by `bench swebench` |

### Optional

| Flag | Default | Description |
|---|---|---|
| `--format <FMT>` | `text` | Output format: `text` or `json` |
| `--min-utilization <PCT>` | *(none)* | CI gate: minimum acceptable utilization percentage (0–100). When measured utilization is below this floor, the command exits non-zero |

## Output

`bench utilization` prints to **stdout** (text or JSON) and writes **no files**
to the sweep directory (fully read-only). Exit code is `0` on success; see
[Exit Codes](#exit-codes).

## Algorithm

Let:

- `D` = Σ per-instance `duration_secs` over all instances that carry one
  (instances missing a duration are excluded from the sum and counted
  separately).
- `W` = sweep wallclock = `finished_at_utc` − `started_at_utc`, in seconds,
  from `manifest.runtime`.
- `P` = configured worker count, recovered from `manifest.cli.argv`
  (`--parallel <N>` or `--parallel=<N>`); falls back to the harness default
  (`4`) when the flag was not passed explicitly. Clamped to a minimum of `1`.

Then:

| Field | Formula |
|---|---|
| `effective_parallelism` | `D / W` |
| `utilization_pct` | `(effective_parallelism / P) × 100` |
| `theoretical_min_wallclock_secs` | `D / P` |
| `idle_waste_secs` | `max(0, W − theoretical_min_wallclock_secs)` |
| `idle_waste_pct` | `(idle_waste_secs / W) × 100` |

`effective_parallelism` is "how many workers' worth of work actually ran":
total CPU-seconds of agent work divided by the wallclock those workers were
alive. Dividing by `P` yields the fraction of configured concurrency that was
realized. `idle_waste_secs` is the wallclock the sweep could have saved had the
workers stayed fully packed.

### Worked example

8 instances × 300 s each = `D` = 2,400 s of work. Sweep wallclock `W` = 600 s.
Configured `P` = 8.

- `effective_parallelism` = 2400 / 600 = **4.0 of 8 configured**
- `utilization_pct` = 4.0 / 8 × 100 = **50.0%**
- `theoretical_min_wallclock_secs` = 2400 / 8 = 300 s
- `idle_waste_secs` = 600 − 300 = **300 s** = **50.0%** of wallclock

The operator reads: *"effective parallelism 4.0 of 8 → 50% utilization, 300 s
idle waste = 50% of wallclock"* and halves `--parallel`, or investigates the
throttle.

## JSON Schema

`--format json` emits a single object. The schema is **stable**; the contract
is versioned by the `schema_version` field (currently `1`). Fields are added
only in a backward-compatible manner; breaking changes bump `schema_version`.

| Field | Type | Description |
|---|---|---|
| `schema_version` | integer | Schema version of this artifact (currently `1`) |
| `sweep` | string | Sweep directory path as supplied on the command line |
| `generated_at` | string | RFC3339 timestamp at which the report was generated |
| `configured_workers` | integer | Configured worker count (`P`) |
| `configured_workers_source` | string | Which manifest field sourced `P` (`manifest.cli.argv[--parallel]` or `default (4)`) |
| `total_instances` | integer | Total instances in the sweep |
| `instances_with_duration` | integer | Instances whose `duration_secs` contributed to `D` |
| `instances_missing_duration` | integer | Instances excluded from `D` for lack of a valid `duration_secs` |
| `sum_instance_duration_secs` | number | `D` — Σ per-instance `duration_secs` |
| `wallclock_secs` | number | `W` — observed sweep wallclock |
| `effective_parallelism` | number | `D / W` |
| `utilization_pct` | number | `(effective_parallelism / P) × 100` |
| `theoretical_min_wallclock_secs` | number | `D / P` |
| `idle_waste_secs` | number | `max(0, W − theoretical_min)` |
| `idle_waste_pct` | number | `(idle_waste_secs / W) × 100` |
| `retry_merged` | boolean | `true` when a run retried within itself (within-run API retries undercount time — see [Approximations](#approximations)). Plain `--rerun`/`--samples` sweeps are **not** flagged: their `duration_secs` is summed across run slots and stays measurable |
| `retry_merged_note` | string | Present only when `retry_merged` is `true`: human-readable caveat |
| `min_utilization` | number | Present only when `--min-utilization` was passed: the floor |
| `min_utilization_met` | boolean | Present only when `--min-utilization` was passed: whether `utilization_pct ≥ min_utilization` |

## Exit Codes

| Code | Class | When |
|---|---|---|
| `0` | `success` | Report computed; gate (if any) passed |
| `2` | `usage_error` | The sweep cannot be measured honestly: missing manifest; absent/unparseable `started_at_utc`/`finished_at_utc` (in-progress or legacy sweep); non-positive wallclock; no instances; no instance carrying a `duration_secs`; a non-`completed` `sweep_status` (cancelled / systemic-halt); a `--resume` sweep; or `--min-utilization` against a retry-merged sweep. Emitted instead of misleading zeros or an untrustworthy verdict |
| `44` | `utilization_gate_failure` | `--min-utilization <PCT>` was set and `utilization_pct` fell below it |

### Rejected sweeps

To keep the measurement honest, the command refuses sweeps whose persisted
inputs are not comparable, rather than emitting a plausible-looking but wrong
number:

- **Non-`completed` sweeps.** A `cancelled` or `systemic_halt` sweep still writes
  a terminal `results.json` with `finished_at_utc`, but only the instances that
  finished before the abort are present. `sweep_status` must be `completed`.
- **`--resume` sweeps.** Instances carried over from the earlier invocation keep
  their prior `duration_secs` (and may appear as `skipped_resume`) while the
  manifest wallclock covers only the resumed run — so summing every duration
  against the resumed wallclock would overstate effective parallelism.
- **`--min-utilization` on a sweep with within-run retries.** The (flagged,
  approximate) report is still produced, but the **gate** is refused:
  `duration_secs` reflects only the terminal attempt of each retried run, so
  earlier attempts and retry backoff consume worker time without contributing to
  the sum, and the gate could fail a sweep that actually kept its workers busy.
  Drop `--min-utilization` for the approximate report, or re-run without
  within-run API retries (e.g. `--max-retries 0`) to gate. **Plain
  `--rerun`/`--samples` sweeps are gateable** — their durations are summed across
  run slots, so the total is full work, not an underestimate.

## Approximations

- **Within-run retries.** When a run retried itself (multiple API-retry
  attempts), `duration_secs` reflects only the terminal attempt. The command
  **flags** this (`retry_merged: true` plus a note) and reports an approximate
  figure rather than attempting exact reconstruction of per-attempt timing.
  (Plain `--rerun`/`--samples` sweeps are not affected: their durations are
  summed across run slots, so the total is full work.)
- **Instances missing a duration.** Excluded from `D` and counted in
  `instances_missing_duration`; they do not contribute misleading zeros to the
  sum.

## Out of Scope

- Per-time-bucket queue-depth / Gantt timeline (needs absolute per-instance
  start timestamps; this command uses durations + sweep wallclock only).
- Exact reconstruction of retry-merged timing (flagged, not reconstructed).
- Live utilization during a running sweep (that is `bench tail`'s territory).
- Any change to the scheduler itself — this is measurement, not tuning.

## Relationship to Other Commands

| Command | Answers |
|---|---|
| `bench tail` | Live progress of a running sweep |
| `bench report` | Outcomes (resolved rate, cost) of a completed sweep |
| `bench budget-fit` | Right-sizes per-task step/cost/wallclock **caps** |
| **`bench utilization`** | Did the configured **concurrency** pay off? |
