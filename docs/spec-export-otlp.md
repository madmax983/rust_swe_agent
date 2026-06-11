# `bench export-otlp` — Backfill OTLP Traces from a Completed Sweep

## Overview

`bench export-otlp` reads a completed sweep directory and re-exports
reconstructed **sweep + instance spans** to an OTLP/HTTP collector
(Jaeger / Tempo / Honeycomb / any OTLP-compatible backend). It exists because
OTLP trace export is otherwise **live-only**: spans are emitted to the collector
*during* a sweep, and only when an endpoint is configured *at launch*. The
common headless / cron case — including this repo's own nightly smoke — runs
without a collector, and a collector outage mid-sweep drops spans the same way.
In both cases the spans are lost permanently even though the canonical
trajectories persist on disk.

This command closes that gap. It walks the sweep directory, reconstructs each
instance's span tree from the persisted trajectory (the same
`telemetry::instance_span_data_from_trajectory` primitive the live path uses),
and exports them through the same OTLP/HTTP/JSON exporter (`export_sweep`) — so
a backfilled run and a live-exported run are **indistinguishable** in the
collector.

It is **read-only with respect to inputs**: it never re-runs instances, never
calls a model, never starts a container, and never modifies the sweep
directory. The only side effect is the OTLP POST (skipped entirely under
`--dry-run`).

## Usage

```
bench export-otlp --sweep <DIR> [--otlp-endpoint <URL>] [--dry-run]
```

### Arguments

| Argument | Required | Description |
|---|---|---|
| `--sweep <DIR>` | yes | Completed sweep directory produced by `bench swebench` |
| `--otlp-endpoint <URL>` | no¹ | OTLP/HTTP collector base URL, e.g. `http://localhost:4318`. `/v1/traces` is appended |
| `--dry-run` | no | Reconstruct and count spans, print the summary, and open no socket |

¹ Required for a real export unless an endpoint is supplied via the OTel env
vars below. Not required for `--dry-run`.

## Endpoint resolution

The endpoint resolves with the **same precedence as the live exporter**
(`telemetry::resolve_endpoint`), first match wins:

1. `--otlp-endpoint <URL>` — treated as a base URL; `/v1/traces` is appended.
2. `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` — used as-is (already a full URL per the
   OTel spec).
3. `OTEL_EXPORTER_OTLP_ENDPOINT` — base URL; `/v1/traces` is appended.

If none resolve (and this is not a dry-run), the command exits `2`
(`usage_error`) with a message naming the missing input.

## Authentication headers

Export headers honour the existing `resolve_otlp_headers()` env contract:
`OTEL_EXPORTER_OTLP_TRACES_HEADERS` takes precedence, then
`OTEL_EXPORTER_OTLP_HEADERS` (comma-separated `key=value` pairs, values
percent-decoded). This lets authenticated SaaS collectors (e.g. Honeycomb's
`x-honeycomb-team`) work without any new flags.

## ID determinism guarantee

Trace and span IDs for a given sweep are **deterministic and identical** to what
the live exporter would have produced for the same run:

* The sweep-level ID is recomputed with `swebench::compute_sweep_id`, the exact
  function the live path uses — `sha256(output_dir_path + started_at_utc)`
  truncated to 64 bits. The `started_at_utc` timestamp is read from the
  persisted provenance manifest, and the output-directory path is the `--sweep`
  directory, so the value matches the live run.
* The sweep span's trace/span IDs derive from that sweep ID via
  `telemetry::new_trace_id("sweep", sweep_id)` /
  `telemetry::new_span_id("sweep_span", …)`.
* Each instance reuses the **persisted** `trace_id` (written to `results.json`
  and the trajectory by the live run) when present, and otherwise recomputes it
  with `telemetry::new_trace_id(instance_id, sweep_id)` — exactly as the live
  path does for instances that completed before OTLP was enabled.
* Child model-call / tool-call span IDs derive deterministically from the
  instance trace ID inside the shared `export_sweep` serialiser.

The result: exporting the same fixture sweep twice — once via the live path,
once via `export-otlp` — emits byte-identical trace/span IDs (verified by the
test `live_and_backfilled_exports_have_identical_ids`). Span *timestamps* on the
sweep root reflect the manifest's recorded start/finish, not wall-clock at
export time.

## Span shape

The exported spans use the same shape and attributes as the live path (no new
attributes or schema):

* one **`sweep`** root span (attributes: `sweep_id`, `dataset`, `model`,
  `instance_count`, `resolved_count`, `total_cost_usd`, `harness_version`,
  optional `git_sha`);
* one **`instance`** span per instance (its own trace, linked back to the sweep
  span via an OTel span link), with `model_call` and `tool_call` child spans
  reconstructed from the trajectory.

Instances whose trajectory is missing on disk (e.g. build-env failures before
the agent initialised) get a minimal instance-only span via
`instance_span_data_from_result`. Fields not present in persisted trajectories
are not reconstructed — the command exports what is on disk and never re-runs
instances.

## Output and exit codes

On success the command prints a one-paragraph summary to stdout reporting the
count of instances and spans sent, the sweep ID, and the resolved endpoint, and
writes **no files**.

Exit codes reuse the stable contract in [`docs/exit-codes.md`](exit-codes.md) —
no new codes are introduced:

| Exit | Outcome class | Condition |
|---|---|---|
| `0` | `success` | Export (or `--dry-run`) completed; summary reports instance and span counts |
| `2` | `usage_error` | Missing/invalid sweep directory, corrupt/absent `results.json`, no provenance manifest, or no endpoint resolved for a real export |
| `3` | `preflight_failure` | The collector was unreachable or rejected the export (one or more spans dropped) |
| `1` | `internal_error` | Unexpected I/O failure |

`--dry-run` always reconstructs, counts, prints the summary, and exits `0`
without opening a socket — so operators can validate before sending.

## Examples

```bash
# Backfill a nightly cron sweep into a local collector.
bench export-otlp --sweep runs/nightly-2026-06-08 --otlp-endpoint http://localhost:4318

# Validate span counts without sending anything.
bench export-otlp --sweep runs/nightly-2026-06-08 --dry-run

# Authenticated SaaS collector via env (no new flags).
export OTEL_EXPORTER_OTLP_ENDPOINT=https://api.honeycomb.io
export OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=$HONEYCOMB_API_KEY"
bench export-otlp --sweep runs/nightly-2026-06-08
```

## Out of scope

* OTLP **metrics** export (see `docs/spec-otlp-metrics.md`) and OTLP **logs**.
* Any change to the live in-sweep exporter (see `docs/spec-otlp-traces.md`).
* New span attributes or schema beyond what the live path emits.
* Reconstructing spans for fields not present in persisted trajectories.
