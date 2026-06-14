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
| `failure_category` | `string?` | Coarse failure kind when `outcome=error`; see [`docs/failure-categories.md`](failure-categories.md) for all values |
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
audit. Pass `--strict-sampling` to escalate this to a hard divergence.

## Provenance manifest (schema 1.9+)

Single-task (`bench mini`) runs carry a `manifest` field inside the `info`
block. It records everything needed to understand *where the run came from* and
*how it was configured* without having to reconstruct it from logs.

```json
{
  "info": {
    "manifest": {
      "harness_git_sha": "a1b2c3d4e5f6...",
      "harness_binary_version": "1.3.0",
      "started_at_utc": "2026-05-27T12:00:00+00:00",
      "ended_at_utc": "2026-05-27T12:04:32+00:00",
      "env_kind": "live",
      "working_dir": "/workspaces/my-repo",
      "config_sha256": "deadbeef01234567...",
      "config_redacted": { "model": { "name": "claude-opus-4-7" }, "redaction": { "enabled": true } },
      "cli_invocation": ["bench", "mini", "--task", "Fix bug", "--api-key", "[REDACTED]"],
      "extra_context_present": false,
      "task_timeout_secs": 300,
      "step_limit": 30,
      "model_name": "claude-opus-4-7",
      "fallback_models": ["claude-sonnet-4-6"],
      "redaction_policy_id": "sha256:3f4a1b2c9e8d7f6a",
      "deterministic_mode": false,
      "parent_sweep_run_id": null
    }
  }
}
```

### Field reference

| Field | Type | Description | Example |
|---|---|---|---|
| `harness_git_sha` | `string?` | Git HEAD SHA of the harness source tree at build time; `"inherits:parent_sweep"` when called from a sweep run | `"a1b2c3d4e5f6..."` |
| `harness_binary_version` | `string` | `CARGO_PKG_VERSION` of the harness binary | `"1.3.0"` |
| `started_at_utc` | `string` | ISO 8601 timestamp captured immediately before the agent loop begins | `"2026-05-27T12:00:00+00:00"` |
| `ended_at_utc` | `string?` | ISO 8601 timestamp stamped at the first trajectory `save_pretty` call; `null` if the run never reached a save | `"2026-05-27T12:04:32+00:00"` |
| `env_kind` | `string` | `"deterministic"` when `deterministic_responses` were injected; `"live"` otherwise | `"live"` |
| `working_dir` | `string?` | Absolute path of the local workdir handed to the agent; `null` when none was configured | `"/workspaces/my-repo"` |
| `config_sha256` | `string` | Full SHA-256 hex of the raw config JSON; `"inherits:parent_sweep"` when called from a sweep run | `"deadbeef01234567..."` |
| `config_redacted` | `object` | Full config object serialized to JSON after sensitive values have been redacted | `{ "model": { "name": "claude-opus-4-7" }, ... }` |
| `cli_invocation` | `string[]` | Process `argv` with values following `--api-key`, `--token`, `--secret`, `--password`, and `--credential` replaced by `"[REDACTED]"` | `["bench", "mini", "--api-key", "[REDACTED]"]` |
| `extra_context_present` | `bool` | `true` when `extra_context` was non-empty; the content itself is not recorded | `false` |
| `task_timeout_secs` | `integer?` | Task timeout in seconds; `null` when not set | `300` |
| `step_limit` | `integer` | Maximum number of agent steps allowed for this run | `30` |
| `model_name` | `string` | Primary model name from config | `"claude-opus-4-7"` |
| `fallback_models` | `string[]` | Ordered list of fallback models from config; empty when no fallbacks are configured | `["claude-sonnet-4-6"]` |
| `redaction_policy_id` | `string` | Short fingerprint of the redaction config (enabled flag, unsafe flag, literal count, custom patterns); never includes secret values | `"sha256:3f4a1b2c9e8d7f6a"` |
| `deterministic_mode` | `bool` | `true` when `deterministic_responses` were provided (scripted/test runs) | `false` |
| `parent_sweep_run_id` | `string?` | Run ID of the parent sweep when this task was launched by `bench sweep`; `null` for standalone `bench mini` invocations | `"sweep-2026-05-27-abc123"` |

### Sweep-child sentinel values

When a `bench mini` run is launched as a child of `bench sweep`, some
per-run fields would duplicate sweep-level information. To avoid redundancy,
those fields are set to the sentinel string `"inherits:parent_sweep"`:

| Field | Sentinel condition |
|---|---|
| `harness_git_sha` | Always when `parent_sweep_run_id` is set |
| `config_sha256` | Always when `parent_sweep_run_id` is set |

Readers should treat `"inherits:parent_sweep"` as "see the sweep manifest for
the authoritative value" rather than as a real SHA.

### Absent `manifest` field

Trajectories written before this feature (schema < 1.9) do not contain a
`manifest` field inside `info`. Readers must handle its absence gracefully via
`#[serde(default)]`.
