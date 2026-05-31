# `--chaos-fail-every` — Deterministic Environment Fault Injection

**Issue:** #340
**Status:** Implemented
**Complexity tier:** S

## Problem

Researchers comparing agent designs need a *reproducible* way to perturb the
environment and measure recovery rate. Maxwell's Daemon already ships
`ChaosEnvironment` (`src/env/chaos.rs`) — a decorator that deterministically
injects timeout failures into bash executions — but until this slice there was
no way to reach it from the CLI. Operators had to fork the codebase or write
Rust harness glue. This wires the decorator to a single flag so anyone can ask
"does my prompt/tool/policy recover from a flaky sandbox?" at $0, with no extra
model calls.

Peer harnesses (mini-swe-agent, SWE-agent, OpenHands, Codex, OpenCode) ship
retry knobs but no programmable, deterministic fault injection. This makes
Maxwell's Daemon the only operator-owned SWE-agent harness with first-class,
reproducible agent-resilience experiments.

## Semantics

`--chaos-fail-every N` wraps the underlying `Environment` (local or docker) in
`ChaosEnvironment` with `fail_every = N`.

- `N = 0` (the default) or omitting the flag: **no wrapping, no behavior
  change.** The environment is exactly what it was before.
- `N > 0`: every Nth call to `Environment::run` is replaced with a synthesized
  timeout result instead of delegating to the real environment:

  ```text
  stdout:     ""
  stderr:     "simulated chaos failure: timed out"
  exit_code:  -1
  timed_out:  true
  ```

  The decorator counts invocations starting at 1, so with `N = 3` the 3rd, 6th,
  9th, … invocations are injected. The schedule is a pure function of the
  invocation count — there is no randomness and no wall-clock dependence.

Each agent step that observes an injected timeout records `chaos_injected: true`
directly on the step's recorded env result
(`messages[i].extra.run_result.chaos_injected`). This lets readers distinguish a
deterministically injected timeout from a genuine sandbox timeout, which has
different stderr and no `chaos_injected` marker.

The available surfaces:

| Command | Flag | Scope |
|---------|------|-------|
| `mini` | `--chaos-fail-every N` | single task |
| `bench swebench` | `--chaos-fail-every N` | every instance in the sweep |
| `bench reproduce` | (inherited from manifest) | re-applies the source sweep's cadence |

The flag is a thin overlay onto `environment.chaos_fail_every` in the config
tree, so it can also be set in a TOML config file:

```toml
[environment]
chaos_fail_every = 3
```

## Reproducibility guarantees

- **Recorded first-class.** The effective cadence is written to the run manifest
  as `chaos_fail_every`:
  - `mini`: `info.manifest.chaos_fail_every` (per-trajectory provenance).
  - `bench swebench`: the sweep `ProvenanceManifest.chaos_fail_every`.
- **Round-trips via `bench reproduce`.** A reproduction reads
  `chaos_fail_every` from the source sweep's manifest and re-applies it, so the
  reproduced run injects the same failures at the same indices.
- **Per-step provenance.** Every injected step carries `chaos_injected: true` on
  its env result, so a trajectory is self-describing.
- **Byte-identical modulo timestamps.** Running the same task twice with the
  same `--chaos-fail-every` value against the deterministic model produces
  trajectories that differ only in wall-clock timestamps and measured latencies.

## Inspecting results

`bench inspect` surfaces chaos counts:

- **Per instance:** `chaos_injected_steps` and `chaos_recoveries`.
- **Per sweep:** a `chaos` block with `fail_every`, total `injected_steps`, and
  total `recoveries`, plus per-row `chaos_injected_steps` / `chaos_recoveries`.

A **recovery** is defined as: the bash step immediately following an injected
timeout was itself non-injected and forward-progress-shaped (exit code `0`).
This is a deterministic, view-time heuristic for "the agent re-issued a command
that succeeded rather than spiralling on the synthetic failure."

`--render-only` surfaces the effective cadence in its dry-run output (text and
JSON, via the `chaos_fail_every` field) so a misconfiguration is visible at $0
before any model call.

## Interaction with retry and policy

- **Per-task / sweep retry** (`bench swebench --max-retries`) operates at the
  *instance* level, after the agent loop has terminated. Chaos injection happens
  *inside* the agent loop, at the *bash* level. They compose: a retried instance
  starts a fresh `ChaosEnvironment` with the invocation counter reset, so the
  same deterministic schedule applies to each attempt.
- **Command policy** runs *before* `env.run`. An injected timeout only replaces
  the execution of a command the policy already allowed; chaos never bypasses
  policy and policy never sees the synthetic result.
- **Stagnation detection** is unaffected — it keys on action signatures, not on
  whether a result was injected.

## Non-goals (explicitly out of scope for this slice)

- **No randomness / non-deterministic chaos.** Reproducibility is the entire
  point; the schedule is a pure function of the invocation count.
- **No network-layer or filesystem-layer chaos.** This slice wraps only the
  bash invocation environment.
- **No additional failure modes** beyond timeout (e.g. nonzero-exit,
  slow-response, partial-stdout). Worth a follow-up once timeout is wired and
  used.
- **No docker-specific refactor.** The decorator wraps whatever inner
  environment is built (local or docker); no docker-only work is in scope.
- **No sweep-level chaos schedules** or `agent suite` exposure. Single knob,
  per-run only, strict opt-in. Normal sweeps never inject unless asked.

## Feature gating

`ChaosEnvironment` lives behind the `chaos` Cargo feature, which is **enabled by
default** so the flag works out of the box. Building with
`--no-default-features` (without re-adding `chaos`) compiles the decorator out;
in that configuration passing `--chaos-fail-every N` with `N > 0` is a
configuration error rather than a silent no-op.
