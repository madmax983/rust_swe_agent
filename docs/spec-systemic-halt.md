# Systemic-Failure Circuit Breaker

`bench swebench` includes a mid-sweep circuit breaker that detects when every
instance is failing for the same operator-actionable reason (bad API key, broken
Docker daemon, wrong model name) and halts the sweep early.  A misconfigured run
costs cents to abort instead of dollars to ride out.

## Problem

Per-task budget (#50/#126), sweep-level cost ceiling (#10), wallclock timeouts
(#34), and bounded retries (#23) all protect against *individual* tasks running
away.  None of them protect against a *systemic* failure where every task errors
identically: an invalid `ANTHROPIC_API_KEY` makes every model call 401, a broken
Docker daemon fails every env setup, a misnamed model errors every routing call.
In all those cases the harness will attempt every instance, retry per policy, and
either burn money up to `--sweep-cost-limit-usd` or burn wallclock until
cancellation.

## How It Works

The breaker observes completed instances in the live dispatch loop.  After each
result it checks:

1. **Minimum sample** — at least `--systemic-failure-min-samples` instances have
   completed (default: 5).  Below this threshold the breaker never fires; a few
   early failures are expected noise.

2. **Dominant-category share** — at least `--systemic-failure-share-pct`% of
   those completed instances share the same actionable failure category (default:
   80%).

3. **Actionable category only** — only operator-configurable root causes count
   (see [Actionable categories](#actionable-categories) below).  Non-actionable
   categories such as `step_limit` or `patch_empty` never trip the breaker even
   if they dominate.

When both conditions are satisfied, the breaker:

- Stops dispatching new instances (pending work stays unstarted for later
  `--resume`).
- Drains in-flight tasks to completion using the existing graceful drain path.
- Writes `halt-report.json` (see [Artifact schema](#artifact-schema)).
- Writes a complete `results.json` with `sweep_status: "systemic_halt"` and
  `not_started` set to the count of skipped instances.
- Exits with **code 11** (`systemic_halt`).

## Actionable Categories

Only these `failure_category` values count toward the breaker threshold (see
[`docs/failure-categories.md`](failure-categories.md) for the full vocabulary and
per-category runbook):

| Category | Trigger examples |
|---|---|
| `model_api` | Invalid or missing `ANTHROPIC_API_KEY`, quota exhausted, unreachable model endpoint, wrong model name |
| `env_setup` | Docker daemon not running, dataset repository path not accessible, environment image pull failed |

All other categories (`step_limit`, `cost_limit`, `budget_exhausted`,
`wallclock_timeout`, `agent_internal`, `patch_apply_invalid`, `patch_empty`,
`secret_leak_detected`, `model_parse`, `unknown`) are non-actionable and never
trip the breaker.

## CLI Flags

| Flag | Default | Description |
|---|---|---|
| `--abort-on-systemic-failure` | `true` | Enable the circuit breaker.  Pass `=false` to opt out entirely. |
| `--systemic-failure-min-samples` | `5` | Minimum completed instances before the breaker can fire. |
| `--systemic-failure-share-pct` | `80` | % share of completed instances required on the dominant actionable category. |

## Exit Code

| Code | Meaning |
|---|---|
| `0` | Sweep completed normally |
| `2` | User cancelled |
| `3` | Budget halt |
| `11` | **Systemic halt — circuit breaker tripped** |

Use exit code 11 in CI to distinguish a breaker halt from a normal completion or
a cost-cap stop:

```bash
bench swebench ... ; rc=$?
if [ $rc -eq 11 ]; then
  echo "Sweep halted early — check halt-report.json for root cause"
  exit 1
fi
```

## `halt-report.json` Artifact Schema

Written atomically to `<output-dir>/halt-report.json` when the breaker trips.

```jsonc
{
  "artifact_kind": "sweep_halt_report",   // versioned artifact identifier
  "trip_reason": "5 completed instances with dominant failure_category `ModelApi` (100% ≥ threshold)",
  "dominant_failure_category": "ModelApi",
  "sample_size": 5,                       // # completed instances at trip time
  "share_pct": 100.0,                     // actual share %
  "first_failing_instance_ids": [         // up to 3 IDs for quick inspection
    "django__django-12345",
    "django__django-12346",
    "django__django-12347"
  ],
  "next_step": "Verify ANTHROPIC_API_KEY is set and valid, check model endpoint reachability, and confirm the model name is correct."
}
```

The `next_step` string is category-specific:

- `model_api` → verify `ANTHROPIC_API_KEY`, model endpoint, and model name.
- `env_setup` → verify Docker daemon, dataset repository path, and image pull.

## `bench tail` Display

During a running sweep `bench tail` shows the circuit breaker state:

```
Circuit breaker: armed — 3/5 actionable failures, dominant: model_api (60%)
```

After a systemic halt:

```
Circuit breaker: tripped — sweep halted early
Status:      systemic halt (circuit breaker tripped)
Abort:       circuit breaker tripped: 95 instance(s) not started
```

## `bench reproduce` Integration

`bench reproduce` reads the circuit-breaker configuration from the source sweep's
`ProvenanceManifest` and re-applies the same thresholds during replay:

```jsonc
// results.json → manifest.circuit_breaker
{
  "enabled": true,
  "min_samples": 5,
  "share_pct": 80
}
```

If the source sweep was recorded before this feature shipped and has no
`circuit_breaker` section, reproduce falls back to the defaults (enabled, N=5,
P=80).

## `bench forecast` Interaction

`bench forecast` runs a small calibration sweep (typically 1–3 instances) to
estimate cost.  **The circuit breaker is not run during forecast.**  Forecast
runs are intentionally tiny and the sample size would never reach `min_samples`,
so the flag is silently respected but the breaker cannot fire in practice.  This
is correct behavior: a forecast that finds zero instances is a dry run, not a
systemic failure.

## `bench doctor` Relationship

`bench doctor` detects misconfiguration *before* launch (invalid model, bad
credentials, broken Docker).  The circuit breaker provides the same protection
*after* launch for cases where `bench doctor` passed but a runtime fault emerged
(e.g., API key revoked mid-sweep, Docker daemon restarted, transient quota
exhaustion that becomes permanent).  The two mechanisms are complementary, not
redundant.
