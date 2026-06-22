# `bench context-pressure` — Context-Window Pressure Telemetry

## Overview

`bench context-pressure` reads a completed sweep directory and surfaces context-window pressure metrics (elision triggers, observations elided, bytes elided, peak projected input tokens vs ceilings, and compaction failures) at both the sweep level and per instance. It reads **only on-disk artifacts and never calls a model provider** (zero-cost guarantee).

As agents run, history bounding keeps the context window within limits by eliding older/larger observations. If no budget is active, these telemetry fields remain zero/false. If a budget is active, this telemetry allows the operator to verify how much context pressure the agent was under, how often history was elided, and whether any runs failed because the history overflowed/failed to compact.

## Usage

```
bench context-pressure --sweep <DIR> [OPTIONS]
```

### Required

| Argument | Description |
|---|---|
| `--sweep <DIR>` | Completed sweep directory produced by `bench swebench` |

### Optional

| Flag | Default | Description |
|---|---|---|
| `--format <FMT>` | `text` | Output format: `text` or `json` |

## Key Concepts

### Trajectory `context_pressure` Model

Every trajectory file (`trajectory.json`) and sweep-level result row (`results.json`) includes a `context_pressure` object with the following fields:

- `elision_trigger_count: u32` - Number of times history elision/compaction was triggered.
- `observations_elided: u32` - Total unique observations elided.
- `bytes_elided: u64` - Total bytes of observation content elided.
- `peak_projected_tokens: u64` - Peak un-elided token size projected before elision was run.
- `token_ceiling: u64` - The configured maximum input token limit (if any), or `0`.
- `compaction_failed: bool` - `true` if the agent run terminated because history failed to compact within the ceiling.

## JSON Schema

`--format json` prints to stdout and also writes `context-pressure.json` in the sweep directory.

```json
{
  "artifact_kind": "context_pressure_report",
  "schema_version": { "major": 1, "minor": 8 },
  "sweep": "/path/to/sweep",
  "generated_at": "2026-06-22T20:00:00Z",
  "total_runs": 1,
  "runs_with_elision": 1,
  "pct_runs_with_elision": 100.0,
  "compaction_failures": 0,
  "pct_compaction_failures": 0.0,
  "bytes_elided_p50": 500,
  "bytes_elided_p90": 500,
  "bytes_elided_p95": 500,
  "bytes_elided_p99": 500,
  "instances": [
    {
      "instance_id": "test-instance-1",
      "exit_reason": "submitted",
      "elision_trigger_count": 2,
      "observations_elided": 3,
      "bytes_elided": 1500,
      "peak_projected_tokens": 12000,
      "token_ceiling": 8000,
      "compaction_failed": false
    }
  ]
}
```

## Exit Codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 2 | Usage or configuration error (bad `--format`, missing `--sweep`) |

## Examples

```bash
# Text summary table
bench context-pressure --sweep ./results

# JSON output
bench context-pressure --sweep ./results --format json
```

## Scope

- **In scope**: aggregation and reporting of context elision and pressure stats from trajectories and result files.
- **Out of scope**: changing elision algorithms or settings during reporting; real-time streaming of pressure stats.
