# Resource Telemetry — Per-Run Peak Memory and CPU Usage

## Overview

Every agent run records peak memory and CPU usage in its trajectory file under the existing
`info` telemetry envelope. A sweep-level rollup (max, median peak memory; total CPU seconds)
is written to `results.json` so operators can right-size Docker resource caps from observed
data without host-level `docker stats` spelunking.

This closes the measurement gap created by the container resource cap flags from #529:
you can now derive a working `--memory` value from observed sweep data on the first try.

## Field Schema

### Per-Run (`TrajectoryInfo` / trajectory `.info` block)

| Field | Type | Unit | Description |
|---|---|---|---|
| `peak_memory_bytes` | `integer \| null` | bytes | High-water RSS for the agent process. Read from cgroup v2 `memory.peak` when available (Docker), otherwise from `/proc/self/status` `VmHWM`. `null` when unavailable. |
| `cpu_seconds` | `number \| null` | seconds | Cumulative CPU time (user + system) consumed by the agent process. Read from cgroup v2 `cpu.stat` `usage_usec` when available, otherwise computed from `/proc/self/stat` at 100 Hz. `null` when unavailable. |

### Sweep-Level Rollup (`SweepResults` / `results.json` top level)

| Field | Type | Unit | Description |
|---|---|---|---|
| `max_peak_memory_bytes` | `integer \| null` | bytes | Maximum `peak_memory_bytes` across all instances. `null` if no instance reported a non-null value. |
| `median_peak_memory_bytes` | `integer \| null` | bytes | Median `peak_memory_bytes` across instances with a non-null value. `null` if no instance reported a non-null value. |
| `total_cpu_seconds` | `number \| null` | seconds | Sum of `cpu_seconds` across all instances with a non-null value. `null` if no instance reported a non-null value. |

## Availability and Null Reasons

| Environment | `peak_memory_bytes` | `cpu_seconds` | Notes |
|---|---|---|---|
| Docker (Linux, cgroup v2) | Non-null (cgroup `memory.peak`) | Non-null (cgroup `cpu.stat`) | Primary read path. |
| Linux, no cgroup v2 | Non-null (`/proc/self/status` VmHWM) | Non-null (`/proc/self/stat`) | Fallback read path. |
| Non-Linux (macOS, Windows) | `null` | `null` | Platform APIs not implemented; graceful degradation. |
| Measurement failure | `null` | `null` | File read or parse error; run continues unaffected. |
| Value reads as zero | `null` | `null` | A zero from the OS indicates the counter was not populated; treated as unavailable rather than a fabricated zero. |

## Backward Compatibility

Both fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`. When
`null`, they are **omitted entirely from serialized JSON** — existing trajectory
consumers that do not expect the fields see no change. The schema minor version was
bumped (1.12 → 1.13) per the project's additive-minor-bump rule. Artifacts at 1.12
are accepted by the checker as `SupportedLegacy`.

## Example JSON

### Trajectory (`trajectory.traj.json` → `.info`)

```json
{
  "schema_version": { "major": 1, "minor": 13 },
  "info": {
    "duration_secs": 42.3,
    "peak_memory_bytes": 314572800,
    "cpu_seconds": 7.84
  }
}
```

### Sweep results (`results.json` top level)

```json
{
  "schema_version": { "major": 1, "minor": 13 },
  "max_peak_memory_bytes": 524288000,
  "median_peak_memory_bytes": 314572800,
  "total_cpu_seconds": 183.2
}
```

## Deriving a Docker `--memory` Cap

Given a completed sweep, use `max_peak_memory_bytes` from `results.json` to set a safe
cap for #529's `--memory` flag:

```
cap = max_peak_memory_bytes × safety_factor
```

A safety factor of **1.5×** is a reasonable starting point; increase to 2×–3× for
tasks with high variance. Example:

```
max_peak_memory_bytes = 524 288 000  (~500 MiB)
cap = 524288000 × 1.5 = 786 432 000  (~750 MiB) → set --memory 768m
```

This eliminates the OOM-kill trial-and-error loop: measure once, cap once.

## Implementation Notes

- **Read location**: `src/resource.rs` — `measure()` returns a `ResourceUsage` struct.
- **Capture point**: `src/agent/default.rs` — `finalize_run_metadata` calls `resource::measure()` after the run completes.
- **Rollup logic**: `src/run/swebench.rs` — `compute_resource_rollup()` / `recompute_aggregates()`.
- **Tests**: `tests/resource_telemetry.rs` — 12 unit tests covering schema version, struct fields, graceful degradation, and no-fabricated-zero invariant.
