# Spec: `agent stability` — Single-Task Run-to-Run Variance Measurement

## Overview

`agent stability` runs one task N times (up to 10) through the same `mini`
execution path and emits a schema-versioned `stability-results.json` artifact
with pass@k, cost/step statistics, and patch-identity rate.

It fills the gap between `--render-only` (zero-cost, prompt preview only) and
`bench swebench` (full paid sweep): "does my prompt actually help, or was that
one good run just luck?"

Slots in the operator inner-feedback loop:

```
--render-only ($0)  →  agent stability (cheap, N ≤ 10)  →  agent suite  →  bench swebench
```

---

## Quick Start

```sh
# Run a task 5 times and see pass@k
max agent stability \
    --task "Fix the null dereference in src/handler.rs" \
    --runs 5 \
    --model claude-haiku-4-5-20251001

# Read task from a file
max agent stability \
    --task-file my-task.txt \
    --runs 3 \
    --model claude-opus-4-7

# Read task from stdin
echo "Fix the bug in src/main.rs" | max agent stability \
    --task-file - \
    --runs 3

# Gate a CI job: fail if fewer than 80% of runs pass
max agent stability \
    --task "Fix the parser edge case" \
    --runs 5 \
    --verify "tests:cargo test parser" \
    --fail-under 0.8 \
    --model claude-haiku-4-5-20251001

# Print JSON artifact to stdout for CI capture
max agent stability \
    --task "Fix the bug" \
    --runs 3 \
    --format json
```

---

## Flags

| Flag | Required | Default | Description |
|------|----------|---------|-------------|
| `--task <TASK>` | ✓ (or `--task-file`) | — | Task description to run N times |
| `--task-file <PATH>` | ✓ (or `--task`) | — | Read task from file (`-` for stdin) |
| `--runs <N>` | ✓ | — | Number of runs (1..=10) |
| `--model <MODEL>` | — | `claude-opus-4-7` | Model name |
| `--config <PATH>` | — | — | TOML config overlay |
| `--verify <NAME:COMMAND>` | — | — | Success oracle; repeatable |
| `--verify-timeout-secs <N>` | — | `60` | Per-check timeout |
| `--output <DIR>` | — | `./runs` | Root output directory |
| `--format <FORMAT>` | — | `text` | `text` or `json` |
| `--fail-under <FLOAT>` | — | — | Exit 39 when `pass_at_k < FLOAT` |
| `--per-task-budget-usd <F>` | — | — | Per-run USD ceiling |
| `--cost-limit-usd <F>` | — | — | Total USD ceiling (stops remaining runs) |
| `--step-limit <N>` | — | — | Max steps per run |
| `--task-timeout-secs <N>` | — | — | Per-run wallclock timeout |
| `--stability-name <NAME>` | — | (task slug) | Output subdirectory name |
| `--env local\|docker` | — | — | Execution environment |
| `--docker-image <IMAGE>` | — | — | Docker image (with `--env docker`) |
| `--mcp-server <CMD>` | — | — | Register an MCP stdio server; repeatable |
| `--detect-stagnation [true\|false]` | — | — | In-loop stagnation detection |
| `--history-max-input-tokens <N>` | — | — | Prompt token budget |
| `--history-keep-last-observations <N>` | — | — | Observation window size |

---

## Success Predicate

A run **passes** according to the following logic:

- **With `--verify`**: all `NAME:COMMAND` checks exit 0 after the agent
  finishes (same oracle as `mini --verify`).
- **Without `--verify`**: the run passes iff the agent's terminal outcome is
  `"submitted"`. The `pass_predicate` field in the artifact is set to
  `"outcome_submitted"` and the report notes this explicitly.

---

## `--runs` Constraint

`--runs` accepts values in **1..=10**. Values outside this range exit **2**
(`usage_error`) with a clear message. The upper bound keeps costs predictable
and the command in its intended niche (fast inner-loop check, not a sweep).

---

## Cost Caps

Two independent caps are supported:

- `--per-task-budget-usd <F>`: Per-run USD ceiling enforced inside the agent
  loop (same as `mini --per-task-budget-usd`).
- `--cost-limit-usd <F>`: Total USD ceiling for the stability run. When the
  cumulative cost of completed runs reaches this threshold, remaining runs are
  recorded with `outcome: "skipped_budget_exhausted"` and `skipped: true` in
  `runs_detail`. Skipped runs are excluded from the `pass_at_k` denominator.

---

## Output Artifacts

Artifacts land in `<output>/<stability-name>/`:

```
<output>/<stability-name>/
  stability-results.json   ← aggregated results (schema-versioned)
  run_01.traj.json         ← per-run trajectory
  run_02.traj.json
  ...
```

The `--stability-name` defaults to a 40-character alphanumeric slug of the
task text. Use `--stability-name my-fix` to override.

---

## `stability-results.json` Schema

```json
{
  "schema_version": {"major": 1, "minor": 10},
  "artifact_kind": "stability_results",
  "task": "Fix the null dereference in src/handler.rs",
  "runs": 5,
  "pass_count": 3,
  "pass_at_k": 0.6,
  "patch_identical_rate": 0.667,
  "pass_predicate": "outcome_submitted",
  "cost_usd_min": 0.0012,
  "cost_usd_max": 0.0034,
  "cost_usd_mean": 0.0021,
  "cost_usd_stddev": 0.0008,
  "step_count_min": 3.0,
  "step_count_max": 8.0,
  "step_count_mean": 5.4,
  "step_count_stddev": 1.8,
  "started_at": "2026-06-01T12:00:00Z",
  "finished_at": "2026-06-01T12:03:12Z",
  "runs_detail": [
    {
      "run_number": 1,
      "outcome": "submitted",
      "passed": true,
      "cost_usd": 0.0012,
      "step_count": 3,
      "skipped": false
    },
    ...
  ]
}
```

### Field Reference

| Field | Type | Description |
|-------|------|-------------|
| `schema_version` | `{major, minor}` | Artifact schema version |
| `artifact_kind` | `"stability_results"` | Always `"stability_results"` |
| `task` | `string` | The task description that was run |
| `runs` | `uint` | Total runs attempted (including skipped) |
| `pass_count` | `uint` | Non-skipped runs that passed the predicate |
| `pass_at_k` | `float` | `pass_count / non_skipped_count` (0.0–1.0) |
| `patch_identical_rate` | `float` | Fraction of submitted runs matching the modal patch (0.0 when no patches captured) |
| `pass_predicate` | `"verify"` \| `"outcome_submitted"` | Which predicate was used |
| `cost_usd_{min,max,mean,stddev}` | `float` | Population statistics over non-skipped run costs |
| `step_count_{min,max,mean,stddev}` | `float` | Population statistics over non-skipped step counts |
| `started_at` | `ISO 8601` | Wall time when the first run started |
| `finished_at` | `ISO 8601` | Wall time when the last run finished |
| `runs_detail` | `array` | Per-run records (see below) |

#### `runs_detail` entry

| Field | Type | Description |
|-------|------|-------------|
| `run_number` | `uint` | 1-based run index |
| `outcome` | `string` | Terminal outcome from trajectory (`"submitted"`, `"step_limit_reached"`, `"error"`, `"skipped_budget_exhausted"`) |
| `passed` | `bool` | Whether this run passed the success predicate |
| `cost_usd` | `float?` | Billed cost in USD; `null` when skipped |
| `step_count` | `uint?` | Agent steps executed; `null` when skipped |
| `skipped` | `bool` | `true` when cost cap was exceeded before this run started |
| `skip_reason` | `string?` | Reason for skipping (present only when `skipped == true`) |

---

## Exit-Code Matrix

| Code | Class | Meaning |
|------|-------|---------|
| 0 | `success` | All runs finished; `pass_at_k` ≥ `--fail-under` (or no gate set) |
| 2 | `usage_error` | Invalid flag (e.g. `--runs 0`, `--runs 11`, missing `--task`/`--task-file`) |
| 3 | `preflight_failure` | Docker not installed, model endpoint unreachable, container failed to start |
| 4 | `task_unsuccessful` | One or more runs hit step limit, timeout, or model errors |
| 5 | `budget_halt` | `--cost-limit-usd` was exceeded; remaining runs were skipped |
| 7 | `verification_failure` | One or more `--verify` checks failed |
| **39** | **`stability_gate_failure`** | `pass_at_k < --fail-under`; all runs completed, gate wired correctly |
| 130 | `interrupted` | SIGINT (Ctrl-C) |

Exit code **39** is distinct from code 7 (`verification_failure`): it signals
"the gate fired because pass_at_k was below your threshold", not that checks
themselves failed in an unexpected way.

---

## Determinism Guarantee

On the no-key scripted-model path (used in CI and tests), two invocations with
identical inputs produce byte-identical aggregate statistics in
`stability-results.json`. Timestamps (`started_at`, `finished_at`) differ
between invocations but are not part of the deterministic aggregation. The
statistics fields (`pass_count`, `pass_at_k`, `patch_identical_rate`,
`cost_usd_*`, `step_count_*`) are pure functions of the run outcomes.

---

## Text Summary Format

When `--format text` (default), a human-readable summary is printed:

```
agent stability: "Fix the null dereference in src/handler.rs" — 3/5 runs passed (pass_at_k=0.600)
  pass_predicate    : verify
  patch_identical   : 0.667
  cost_usd          : min=0.0012  max=0.0034  mean=0.0021  stddev=0.0008
  step_count        : min=3.0    max=8.0    mean=5.4    stddev=1.8

  RUN   OUTCOME                 PASS    COST($)   STEPS
  ────────────────────────────────────────────────────────
    1   submitted               yes     0.0012       3
    2   submitted               yes     0.0020       5
    3   step_limit_reached      no      0.0034       8
    4   submitted               yes     0.0018       4
    5   error                   no      0.0021       7
```

---

## Relationship to Other Commands

| Command | What it measures |
|---------|-----------------|
| `mini --render-only` | $0 prompt preview — zero cost, no model call |
| **`agent stability`** | **Pass@k for one task — cheap, N ≤ 10, signal vs noise** |
| `agent suite` | Multi-task pack — regression across operator-defined tasks |
| `bench swebench` | Full dataset sweep — statistical power with hypothesis testing |
