# Trajectory Format Specification

Trajectories are the primary per-run artifacts produced by the harness. Each
trajectory file records the complete conversation between the agent and its
environment, together with telemetry for audit and reproducibility.

## File naming

| Mode | Path |
|---|---|
| Single-shot | `{output_dir}/{instance_id}.traj.json` |
| Multi-run | `{output_dir}/{instance_id}/run-{n}.traj.json` |

## Top-level shape

```json
{
  "trajectory_format": "mini-swe-agent-1.2",
  "artifact_kind": "trajectory",
  "schema_version": {"major": 1, "minor": 8},
  "info": { ... },
  "messages": [ ... ]
}
```

`artifact_kind` and `schema_version` follow the
[artifact contract](artifact-contract.md). Readers should call
`classify_json_value` before deserializing to get a compatibility class and any
warnings.

## `info` block

Contains run-level metadata and aggregate telemetry. All fields are optional
(defaulting to `null`) so readers can handle both legacy and current artifacts:

| Field | Type | Description |
|---|---|---|
| `model_name` | `string?` | Configured primary model name |
| `outcome` | `string?` | `submitted` \| `step_limit_reached` \| `error` |
| `failure_category` | `string?` | Coarse failure kind when `outcome=error` |
| `token_usage` | `TokenUsage?` | Aggregate token counts for the run |
| `actual_cost_usd` | `number?` | Measured USD cost |
| `fallback_summary` | `FallbackSummary?` | Fallback chain telemetry (present only when `model.fallback_models` was configured) |

## `messages` array

Each element is a `MessageRecord`:

```json
{
  "role": "assistant",
  "content": "...",
  "extra": { ... }
}
```

`role` is one of `system`, `user`, `assistant`, `tool`.

### `extra` block per message

| Field | Type | Present on | Description |
|---|---|---|---|
| `cost` | `number?` | assistant | USD cost for this turn |
| `model_latency_ms` | `number?` | assistant | Wall-clock inside the provider |
| `tool_latency_ms` | `number?` | user/tool | Wall-clock running the tool |
| `harness_overhead_ms` | `number?` | assistant | Harness bookkeeping time |
| `timestamp` | `string?` | assistant | ISO-8601 timestamp |
| `actions` | `string[]?` | assistant | Parsed tool call strings |
| `response` | `object?` | assistant | Raw provider response JSON |
| `sampling` | `SamplingParams?` | assistant | Per-call sampling parameters (**schema 1.8+**) |

## `sampling` field (schema 1.8+)

Every assistant turn produced by a live model call carries a `sampling` block
that records the exact parameters sent to the model for that call:

```json
{
  "model": "claude-opus-4-7",
  "temperature": 0.0,
  "max_tokens": 4096,
  "top_p": null,
  "seed": null,
  "extra": {}
}
```

| Field | Type | Description |
|---|---|---|
| `model` | `string` | Resolved model name — the fallback model when a fallback occurred |
| `temperature` | `number?` | Sampling temperature, `null` when not set |
| `top_p` | `number?` | Nucleus sampling threshold, `null` when not set |
| `max_tokens` | `number?` | Maximum completion tokens, `null` when not set |
| `seed` | `integer?` | Deterministic seed, `null` when not set |
| `extra` | `object` | Provider-specific knobs forwarded opaquely |

### Legacy trajectories (schema < 1.8)

Trajectories written before schema 1.8 do not contain the `sampling` field.
Readers must treat its absence as `sampling: null` rather than failing. The
deserializer automatically surfaces this as `None` via `#[serde(default)]`.

### Redaction of `sampling.extra`

`sampling.extra` is a redaction-eligible region. Before the trajectory is
persisted, any key whose name matches the existing sensitive-key pattern
(names containing `token`, `secret`, `password`, `credential`, or ending in
`key`) has its value replaced with a stable `[REDACTED:kind:size:hash]` marker.
This prevents provider authentication headers or API keys from leaking through
the `extra` pass-through.

## Version history

| Version | Change |
|---|---|
| 1.0 | Initial versioned artifact |
| 1.4 | Replay fingerprinting (`input_fingerprint` in assistant extra) |
| 1.5 | Per-turn wall-clock attribution (`model_latency_ms`, `tool_latency_ms`, `harness_overhead_ms`) |
| 1.6 | Sweep-halt artifact kind |
| 1.7 | Render-only preview artifact kind |
| 1.8 | Per-call sampling parameters (`sampling` in assistant extra, issue #177) |

## `bench inspect` output

In default text mode, `bench inspect --instance <id>` shows a one-line
sampling summary for each assistant step:

```
[step 1] assistant
sampling: model=claude-opus-4-7 temp=0 max_tokens=4096
```

In `--json` mode, the full `sampling` object is included in each step's
`InspectStep.sampling` field.

## `bench compare` sampling drift

When trajectory files are available for both sweeps, `bench compare` reports
sampling drift as a named category in the output:

```json
{
  "sampling_drift": {
    "steps_drifted": 42,
    "example": {
      "instance_id": "task-001",
      "baseline_sampling": {"model": "claude-opus-4-7", "temperature": 0.0, ...},
      "candidate_sampling": {"model": "claude-opus-4-7", "temperature": 0.7, ...}
    }
  }
}
```

`steps_drifted` counts all steps across all instance pairs where sampling
params differ. `null` is reported when no trajectory files were loadable.

## `bench reproduce` sampling drift

`bench reproduce` writes a `sampling_drift` block in `reproducibility.json`
when trajectory files from the source and replay sweeps are available:

```json
{
  "sampling_drift": {
    "instances_drifted": 3,
    "steps_drifted": 9
  }
}
```

A mismatch here is classified as a **soft** divergence: the run is still
considered reproducible for outcome purposes, but the drift is recorded for
audit. Pass `--strict-sampling` to escalate this to a hard divergence (future
work).
