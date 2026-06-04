# `agent best-of` — Sample N Runs and Emit the Best Patch

**Issue:** #485  
**Complexity tier:** M

## Overview

`agent best-of` runs one task N times through the existing `mini` execution
path with identical configuration, scores each candidate patch using the
operator's own `--verify` checks, and emits the single best patch using a
documented, deterministic selection policy.

This command turns LLM stochasticity into a quality win: instead of running
`mini` once and hoping, you run it N times and keep the best result.

## Usage

```
max agent best-of \
  --task "Fix the null dereference in src/foo.rs" \
  --runs 3 \
  --model claude-opus-4-7 \
  --verify "tests:cargo test" \
  --verify "lint:cargo clippy -- -D warnings" \
  [--output ./runs] \
  [--output-patch best.patch] \
  [--allow-no-pass] \
  [--cost-limit-usd 2.00] \
  [--format json]
```

Or read the task from a file (or stdin with `-`):

```
max agent best-of \
  --task-file task.txt \
  --runs 5 \
  --verify "tests:make test"
```

## Flag Reference

| Flag | Required | Default | Description |
|---|---|---|---|
| `--task TASK` | one of `--task`/`--task-file` | — | Task description |
| `--task-file PATH` | one of `--task`/`--task-file` | — | Path to task file (`-` for stdin) |
| `--runs N` | yes | — | Number of runs, 2–10 |
| `--model NAME` | no | `claude-opus-4-7` | Model identifier |
| `--verify NAME:COMMAND` | **yes** | — | Verify check (repeatable) |
| `--verify-timeout-secs N` | no | 60 | Per-check timeout |
| `--output DIR` | no | `./runs` | Root output directory |
| `--output-patch PATH` | no | `<output>/<name>/best.patch` | Winner patch path |
| `--allow-no-pass` | no | false | Exit 0 even when all runs fail |
| `--cost-limit-usd F` | no | — | Total USD cap across all runs |
| `--per-task-budget-usd F` | no | — | Per-run USD cap |
| `--step-limit N` | no | — | Max steps per run |
| `--task-timeout-secs N` | no | — | Per-run wallclock timeout |
| `--best-of-name NAME` | no | slug of task | Subdirectory name |
| `--format text\|json` | no | `text` | Output format |
| `--config PATH` | no | — | TOML config overlay |
| `--env local\|docker` | no | — | Environment |
| `--docker-image IMG` | no | — | Docker image |
| `--mcp-server CMD` | no | — | MCP server command (repeatable) |
| `--detect-stagnation` | no | — | Enable/disable stagnation detection |
| `--history-max-input-tokens N` | no | — | Token budget for prompt |
| `--history-keep-last-observations N` | no | — | Keep last N observations |

## `--verify` Requirement

`--verify` is **required**. Without an oracle there is no basis for comparing
runs. If you just want to run a task once, use `mini`. Exit 2 (`usage_error`)
when `--verify` is omitted.

## Selection / Tie-Break Policy

The winner is chosen by the following fully deterministic chain:

1. **Most `verify_checks_passed`** (skipped runs excluded).
2. **Lowest `total_cost_usd`** (tie-break).
3. **Fewest `step_count`** (tie-break).
4. **Smallest patch byte length** (tie-break).
5. **Lexicographically smallest `patch_sha256`** (tie-break).

`tie_break_applied: true` is set in the artifact whenever any tie-break beyond
rule 1 was used. The `selection_rationale` string names the decisive rule.

## Artifacts

All artifacts land in `<output>/<best-of-name>/`:

| File | Description |
|---|---|
| `run_NN.traj.json` | Per-run trajectory (same as `mini`) |
| `run_NN.patch` | Per-run captured patch |
| `best-of-results.json` | Schema-versioned selection report |
| `best.patch` (or `--output-patch`) | Winner's patch file |

### `best-of-results.json` Schema

```json
{
  "schema_version": {"major": 1, "minor": 10},
  "artifact_kind": "best_of_results",
  "task": "Fix the null dereference",
  "runs": 3,
  "winner_run_index": 1,
  "passing_run_count": 2,
  "all_failed": false,
  "tie_break_applied": false,
  "selection_rationale": "most_verify_checks_passed",
  "started_at": "2026-01-01T00:00:00Z",
  "finished_at": "2026-01-01T00:00:30Z",
  "runs_detail": [
    {
      "run_index": 0,
      "outcome": "submitted",
      "verify_checks_passed": 1,
      "verify_checks_total": 2,
      "passed": false,
      "total_cost_usd": 0.0042,
      "step_count": 12,
      "patch_byte_len": 384,
      "patch_sha256": "a3f9..."
    },
    {
      "run_index": 1,
      "outcome": "submitted",
      "verify_checks_passed": 2,
      "verify_checks_total": 2,
      "passed": true,
      "total_cost_usd": 0.0038,
      "step_count": 10,
      "patch_byte_len": 412,
      "patch_sha256": "b7c2..."
    }
  ]
}
```

**Top-level fields:**

| Field | Type | Description |
|---|---|---|
| `schema_version` | object | `{major, minor}` |
| `artifact_kind` | string | Always `"best_of_results"` |
| `task` | string | The task description |
| `runs` | u32 | Configured number of runs |
| `winner_run_index` | u32 \| null | 0-based index of selected winner |
| `passing_run_count` | u32 | Non-skipped runs that passed all checks |
| `all_failed` | bool | True when no run passed all checks |
| `tie_break_applied` | bool | True when any tie-break beyond rule 1 was used |
| `selection_rationale` | string | Name of the decisive selection rule |
| `started_at` / `finished_at` | RFC 3339 | Timestamps |
| `runs_detail` | array | Per-run records |

**Per-run fields:**

| Field | Type | Description |
|---|---|---|
| `run_index` | u32 | 0-based index |
| `outcome` | string | Terminal outcome from trajectory |
| `verify_checks_passed` | u32 | Checks that passed |
| `verify_checks_total` | u32 | Total checks configured |
| `passed` | bool | `verify_checks_passed == verify_checks_total` |
| `total_cost_usd` | f64 \| null | Run cost |
| `step_count` | u32 \| null | Agent steps executed |
| `patch_byte_len` | u64 \| null | Patch file size in bytes |
| `patch_sha256` | string \| null | SHA-256 of the captured patch |
| `skipped` | bool | Run was skipped (budget cap) |
| `skip_reason` | string \| null | Reason for skipping |

## Exit Code Matrix

| Exit code | Label | Condition |
|---|---|---|
| 0 | `success` | At least one run passed all verify checks |
| 0 | `success` | `--allow-no-pass` given, even when all runs failed |
| 2 | `usage_error` | `--verify` omitted; `--runs` outside 2–10; bad flags |
| 5 | `budget_halt` | At least one run was skipped due to `--cost-limit-usd` |
| 41 | `best_of_all_failed` | No run passed all verify checks (no `--allow-no-pass`) |

## `--runs` Range

`--runs` must be 2–10 (inclusive). Best-of-1 is identical to `mini` — exit 2
with a message pointing at `mini` when `--runs 1` is passed.

## Cost Limiting

`--cost-limit-usd F` caps total spend across all runs. When the cumulative cost
reaches the limit, remaining runs are recorded with `skipped: true` and
`skip_reason: "cost_limit_usd"`. The command still selects a winner from the
completed runs and exits with exit code 5 (`budget_halt`).

## All-Failed Behaviour

When no run passes all verify checks:

- `all_failed: true` is set in `best-of-results.json`
- The best-scoring run (most checks passed, then tie-breaks) is still selected
  and its patch is written to `--output-patch`
- Exit code 41 (`best_of_all_failed`) unless `--allow-no-pass` is given

## Determinism on the Scripted-Model Path

On the no-key scripted-model path (used in testing), two invocations with
identical inputs produce byte-identical `best-of-results.json` and
byte-identical `best.patch`. This property is verified by the integration
test suite.

## Composability

The emitted `best.patch` is identical to that run's own captured patch and can
be applied with `agent apply`:

```
max agent best-of --task "..." --runs 3 --verify "tests:cargo test"
max agent apply --patch runs/my-task/best.patch
```

## Related Commands

- `mini` — single task run
- `agent stability` (#475) — measures run-to-run variance without selecting
- `bench cascade` — exploits capability differences across model tiers
- `agent apply` (#473) — applies the emitted patch to a working tree
