# `bench matrix` Spec

`bench matrix` runs multiple sweep arms against the **same** deterministically-
selected instance set so that results are directly comparable. Each arm is a
distinct (model, config) pair defined in a TOML manifest file.

## CLI

```bash
rust-swe-agent bench matrix \
  --config matrix.toml \
  --dataset-path data/swe-bench-verified.jsonl \
  --output runs/matrix-2025-01 \
  --limit 50 \
  --seed 42 \
  --sweep-cost-limit-usd 20.00
```

Options:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--config <path>` | required | TOML matrix manifest containing `[[arm]]` entries. |
| `--dataset-path <file>` | required¹ | Local JSONL dataset file. |
| `--dataset <alias>` | required¹ | Named SWE-bench alias (`verified`, `lite`, `full`). |
| `--output <dir>` | required | Root output directory. Arm artifacts go in `{output}/{arm_name}/`. |
| `--sweep-cost-limit-usd <f>` | unset | Shared USD ceiling across all arms. Arms that would start once the budget is exhausted are recorded as `skipped_budget`. |
| `--matrix-parallelism <n>` | `1` | Number of arms to run concurrently (sequential by default). |
| `--resume` | off | Load existing `matrix.json` and skip `complete`/`skipped_budget` arms. |
| `--limit <n>` | unset | Keep at most N instances after filtering and sampling. |
| `--sample <n>` | unset | Reproducibly random-subset to N instances (requires `--seed`). |
| `--seed <n>` | unset | RNG seed for `--sample`. |
| `--parallel <n>` | 4 | Worker parallelism per arm sweep. |
| `--skip-preflight` | off | Skip startup preflight checks. |
| `--skip-model-probe` | off | Skip model-endpoint probe during preflight. |

¹ Exactly one of `--dataset-path` or `--dataset` must be provided.

## Manifest format

```toml
[[arm]]
name = "baseline"
model = "claude-opus-4-7"

[[arm]]
name = "tuned"
model = "claude-sonnet-4-6"
step_limit = 30
per_task_budget_usd = 0.50
prompt_file = "/path/to/custom-prompt.toml"
extra_args = ["--skip-patch-validation"]
```

Each `[[arm]]` entry supports:

| Field | Required | Type | Meaning |
| --- | --- | --- | --- |
| `name` | yes | string | Unique arm name; used as the output subdirectory. Must not contain `/`, `\`, or `..`. |
| `model` | yes | string | Model name passed to the sweep (e.g. `claude-opus-4-7`). |
| `step_limit` | no | integer | Override the default agent step limit for this arm. |
| `per_task_budget_usd` | no | float | Override per-task USD ceiling for this arm. |
| `prompt_file` | no | path | TOML config file overlaid on defaults for this arm. |
| `extra_args` | no | list of strings | Additional CLI args forwarded to the arm sweep (parsed as overrides). |

## Output artifacts

After a run, the output directory contains:

```
{output}/
  matrix.json           ← machine-readable state (written before every arm, updated after)
  matrix-summary.json   ← ranked summary of all arms
  matrix-summary.txt    ← human-readable ranked table
  {arm_name}/           ← standard sweep artifacts (results.json, trajectories, …)
    results.json
    *.traj.json
  …
```

### `matrix.json`

```json
{
  "artifact_kind": "matrix",
  "config_path": "matrix.toml",
  "instance_ids": ["django__django-1234", "…"],
  "filter_spec": { "original_count": 500, "selected_count": 50, "seed": 42 },
  "cost_limit_usd": 20.0,
  "arms": [
    { "name": "baseline", "model": "claude-opus-4-7", "state": "complete",
      "sweep_dir": "runs/matrix-2025-01/baseline",
      "total_cost_usd": 7.32, "resolved": 12, "submitted": 18 },
    { "name": "tuned", "model": "claude-sonnet-4-6", "state": "skipped_budget",
      "sweep_dir": "runs/matrix-2025-01/tuned",
      "total_cost_usd": 0.0, "resolved": 0, "submitted": 0 }
  ]
}
```

#### Arm states

| State | Meaning |
| --- | --- |
| `pending` | Not yet started. |
| `running` | Currently executing. |
| `complete` | Sweep finished (regardless of individual instance outcomes). |
| `skipped_budget` | Shared cost limit was already reached; arm was not started. |
| `not_started` | Ctrl-C abort arrived before this arm began. |
| `cancelled` | Arm was in-flight when a Ctrl-C abort arrived. |

### `matrix-summary.json`

```json
{
  "arms": [
    {
      "rank": 1,
      "name": "tuned",
      "model": "claude-sonnet-4-6",
      "state": "complete",
      "resolved": 15,
      "resolved_rate": 0.30,
      "total_cost_usd": 8.12,
      "cost_per_resolved_usd": 0.5413,
      "delta_resolved_rate_pp": 0.0,
      "delta_cost_per_resolved_usd": 0.0
    },
    {
      "rank": 2,
      "name": "baseline",
      "model": "claude-opus-4-7",
      "state": "complete",
      "resolved": 12,
      "resolved_rate": 0.24,
      "total_cost_usd": 7.32,
      "cost_per_resolved_usd": 0.61,
      "delta_resolved_rate_pp": -6.0,
      "delta_cost_per_resolved_usd": 0.0687
    }
  ]
}
```

Arms are ranked by resolved rate descending (ties broken by lower total cost).
`skipped_budget` and other incomplete arms appear after all complete arms.
`delta_*` fields are relative to the rank-1 arm.

## Budget enforcement

The `--sweep-cost-limit-usd` ceiling is **shared** across all arms and tracked
cumulatively. The check fires *before* each arm starts:

1. If `cumulative_cost_so_far >= limit`, the arm is marked `skipped_budget` and
   the runner moves to the next arm.
2. There is no mid-arm interruption — once an arm starts, it runs to completion
   (or until its own per-task budget is exhausted).

## Resume behaviour

`--resume` loads the existing `matrix.json` and skips any arm whose state is
`complete` or `skipped_budget`. All other arms (including `running` arms that
were interrupted by a crash) are re-run from the beginning. The instance set is
re-resolved from the manifest parameters on every invocation so that the
`--seed`/`--sample` selection is always reproducible even after a crash.

## Instance set consistency

The instance set is resolved **once** before any arm runs:

1. Load the dataset from `--dataset-path` or the named alias.
2. Apply `--instance-ids` / `--sample` / `--limit` filters.
3. Persist the final list of instance IDs in `matrix.json`.
4. Pass exactly those IDs to every arm sweep via `--instance-ids`.

This guarantees every arm evaluates the identical workload regardless of any
non-determinism in the dataset loading path.

## Graceful abort (Ctrl-C)

When a cancellation signal arrives:

- The in-flight arm is allowed to finish its current in-flight tasks (up to
  `--cancel-deadline-secs`; default 60 s).
- Arms that had not yet started are marked `not_started` in `matrix.json`.
- `--resume` can restart the experiment from where it left off.
