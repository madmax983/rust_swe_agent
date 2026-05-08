# `bench tail` spec

`bench tail` is a read-only operator view over a `bench swebench` output
directory. It does not talk to the writer process, open sockets, or create new
files. Every snapshot is derived from `results.json` plus valid flat
`*.traj.json` files or nested `<instance_id>/run-*.traj.json` files that
already exist in the sweep directory.

## Usage

```bash
rust-swe-agent bench tail --sweep runs/sweep-001
rust-swe-agent bench tail --sweep runs/sweep-001 --interval-ms 1000
rust-swe-agent bench tail --sweep runs/sweep-001 --once
rust-swe-agent bench tail --sweep runs/sweep-001 --once --format json
```

Default mode is a long-lived text view refreshed every two seconds. In a TTY,
the view is redrawn in place. `--once` prints a single snapshot and exits.
`--format json` prints one compact JSON object per line in streaming mode, and
one object in `--once` mode.

## Snapshot Fields

Each snapshot reports:

* `completed`, `in_flight`, `pending`, `total`
* `failure_counts`, keyed by `failure_category`
* `cumulative_cost_usd` for actual run spend
* `baseline_cumulative_cost_usd` for the configured baseline model counterfactual
* `burn_rate_usd_per_min` over the last five minutes
* `eta_seconds`, or `null` until a useful completion rate exists
* `budget_cap_usd` and `pct_of_cap_used` when a cap is configured
* `started_at` and `last_event_at`
* `is_complete`
* `abort_reason`
* `warnings` for ignored partial or malformed JSON files

`completed` means "accounted for by a final result row or a valid trajectory."
While a sweep is running, `in_flight` is inferred from the writer's `--parallel`
argument recorded in the sweep manifest, capped by remaining work. This keeps
the command zero-IPC while still giving the operator a useful live estimate.

## Completion And Abort

`bench tail` exits 0 when `results.json` has `status: "completed"`, when the
manifest has `finished_at_utc`, or when all known instances are accounted for.

It exits non-zero after printing a one-line reason to stderr when it detects:

* `budget_halted > 0`
* a fatal/abort/error status in `results.json`
* actual cumulative cost at or above the configured cap before all work is accounted
  for

## Partial Files

Partially-written `results.json`, flat `*.traj.json`, or nested
`run-*.traj.json` files are ignored for the current tick and reported in
`warnings`. The next refresh retries them. This is intentional: the tailer must
never panic just because the writer was halfway through flushing JSON.

