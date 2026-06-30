# `bench cascade` Spec

`bench cascade` runs instances through an **ordered list of model tiers** and
short-circuits each instance on the first tier that resolves it. Cheaper tiers
run first; expensive premium-tier capacity is consumed only for instances that
cheaper tiers cannot resolve.

## CLI

```bash
max bench cascade \
  --config cascade.toml \
  --dataset-path data/swe-bench-verified.jsonl \
  --output runs/cascade-2025-01 \
  --eval-backend sb-cli \
  --limit 50 \
  --seed 42 \
  --sweep-cost-limit-usd 50.00
```

Options:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--config <path>` | required | TOML cascade manifest containing `[[tier]]` entries. |
| `--dataset-path <file>` | required¹ | Local JSONL dataset file. |
| `--dataset <alias>` | required¹ | Named SWE-bench alias (`verified`, `lite`, `full`). |
| `--output <dir>` | required | Root output directory. Tier results land in `{output}/tier-{name}/`. |
| `--eval-backend <backend>` | required² | Evaluation backend used for per-instance short-circuit (e.g. `sb-cli`). |
| `--sweep-cost-limit-usd <f>` | unset | Shared USD ceiling across all tiers. Instances reached after the limit is exhausted are recorded as `skipped_budget`. |
| `--resume` | off | Load existing `cascade.json` and skip completed instances. |
| `--limit <n>` | unset | Keep at most N instances after filtering and sampling. |
| `--sample <n>` | unset | Reproducibly random-subset to N instances (requires `--seed`). |
| `--seed <n>` | unset | RNG seed for `--sample`. |
| `--parallel <n>` | 4 | Worker parallelism per tier sweep. |
| `--skip-preflight` | off | Skip startup preflight checks. |
| `--skip-model-probe` | off | Skip model-endpoint probe during preflight. |

¹ Exactly one of `--dataset-path` or `--dataset` must be provided.
² A cascade requires an evaluation backend; `--eval-backend none` is rejected.

## Manifest format

```toml
[[tier]]
name = "haiku"
model = "claude-haiku-4-5"
step_limit = 20
per_task_budget_usd = 0.10

[[tier]]
name = "sonnet"
model = "claude-sonnet-4-6"
step_limit = 40
per_task_budget_usd = 0.50
extra_args = ["--skip-patch-validation"]

[[tier]]
name = "opus"
model = "claude-opus-4-8"
step_limit = 80
per_task_budget_usd = 2.00
extra_args = ["--max-rpm", "500"]
```

Each `[[tier]]` entry supports:

| Field | Required | Type | Meaning |
| --- | --- | --- | --- |
| `name` | yes | string | Unique tier name; used as the output subdirectory. Must not contain `/`, `\`, or `..`. |
| `model` | yes | string | Model name passed to the sweep (e.g. `claude-opus-4-8`). |
| `step_limit` | no | integer | Override the default agent step limit for this tier. |
| `per_task_budget_usd` | no | float | Override per-task USD ceiling for this tier. |
| `prompt_file` | no | path | TOML config file overlaid on defaults for this tier. |
| `extra_args` | no | list of strings | Additional sweep args applied as overrides to this tier only (parsed as overrides). |

### `extra_args` reference

`extra_args` accepts a subset of sweep-level flags, applied per-tier in
isolation. The same override semantics apply as for `bench matrix` arm
`extra_args` (see `docs/spec-matrix.md`).

| Arg | Type | Meaning |
| --- | --- | --- |
| `--skip-patch-validation` | flag | Skip `git apply --check` and empty-diff validation after patch capture. Useful for non-git or relaxed-validation tiers. |
| `--max-rpm <n>` | integer | Per-tier aggregate request-rate ceiling (requests/min). |
| `--max-input-tpm <n>` | integer | Per-tier aggregate input-token-rate ceiling (tokens/min). |

**Isolation**: each tier's `extra_args` are applied exclusively to that tier's
sweep run; args from one tier do not carry over to any other tier.

**Precedence**: `extra_args` are applied after the dedicated tier fields
(`model`, `step_limit`, `per_task_budget_usd`). If both a dedicated field and
an `extra_args` entry target the same sweep setting, the dedicated field takes
precedence at config-construction time; `extra_args` override sweep-level args
applied afterward.

**Fail-fast validation**: unrecognized or malformed `extra_args` entries cause
the cascade to fail before any model budget is spent, with an error message that
names the offending tier and arg.

## Routing logic

1. Instances are resolved once from the dataset before any tier runs.
2. Tiers execute in definition order (tier 0 → tier 1 → … → tier N−1).
3. After each tier's sweep, the evaluation backend scores outcomes.
4. Instances resolved by tier k are **removed** from subsequent tiers.
5. Instances not resolved after all tiers are recorded as exhausted.

## Output artifacts

After a run, the output directory contains:

```
{output}/
  cascade.json              ← machine-readable state (updated after every tier)
  cascade-summary.json      ← per-tier stats and cascade-wide summary
  tier-{name}/              ← standard sweep artifacts for each tier
    results.json
    *.traj.json
  …
```

### `cascade.json`

```json
{
  "artifact_kind": "cascade",
  "config_path": "cascade.toml",
  "tier_names": ["haiku", "sonnet", "opus"],
  "instance_ids": ["django__django-1234", "…"],
  "filter_spec": { "original_count": 500, "selected_count": 50, "seed": 42 },
  "cost_limit_usd": 50.0,
  "instances": {
    "django__django-1234": {
      "resolving_tier": "haiku",
      "total_cost_usd": 0.08,
      "attempts": [
        { "tier_name": "haiku", "model": "claude-haiku-4-5",
          "outcome": "submitted", "eval_exit_reason": "resolved",
          "cost_usd": 0.08, "steps": 12 }
      ]
    }
  }
}
```

#### `halted_reason` values

| Value | Meaning |
| --- | --- |
| `skipped_budget` | Instance was not started because the shared cost limit was already reached. |
| `eval_pending` | Model ran but the evaluator failed; will retry on `--resume`. |

### `cascade-summary.json`

```json
{
  "artifact_kind": "cascade-summary",
  "tiers": [
    { "name": "haiku", "model": "claude-haiku-4-5",
      "instances_attempted": 50, "resolved": 20, "resolved_rate": 0.40,
      "total_cost_usd": 4.00, "mean_cost_per_attempt_usd": 0.08,
      "cost_per_resolved_usd": 0.20 }
  ],
  "cascade_resolved": 35,
  "cascade_resolved_rate": 0.70,
  "total_instances": 50,
  "total_cost_usd": 12.50,
  "cost_per_resolved_cascade_usd": 0.357,
  "savings_vs_top_tier_only_usd": 37.50
}
```

## Budget enforcement

The `--sweep-cost-limit-usd` ceiling is **shared** across all tiers:

1. Before each tier sweep, if `cumulative_cost >= limit`, remaining instances
   for that tier are marked `skipped_budget`.
2. There is no mid-sweep interruption — once a tier's sweep starts, it runs to
   completion (or until its own per-task budget fires).
3. Instances already evaluated as `eval_pending` bypass the budget guard — the
   evaluator retry costs no model budget.

## Resume behaviour

`--resume` loads `cascade.json` and skips:
- Instances already resolved (have `resolving_tier` set).
- Instances where all tiers have already completed a non-skipped attempt.

Instances with `halted_reason: "eval_pending"` are re-queued for evaluator
retry (the inner sweep uses `resume: true` so model calls are not re-spent).

## Graceful abort (Ctrl-C)

When a cancellation signal arrives during a tier sweep:

- The in-flight tier is allowed to finish current tasks (up to
  `--cancel-deadline-secs`; default 60 s).
- Unstarted instances for the cancelled tier are recorded and future tiers
  are not started.
- `--resume` can restart the experiment from where it left off.
