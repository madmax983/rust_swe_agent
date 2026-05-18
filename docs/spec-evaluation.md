# `bench evaluate` / `evaluation.json` contract

`bench swebench` writes sweep execution artifacts (`results.json`,
per-instance trajectories, patches, and `all_preds.jsonl`). `bench evaluate`
is a post-processing step that scores those artifacts using an evaluation
backend and writes `evaluation.json` in the same sweep directory.

`bench swebench --rerun N` (alias: `--samples N`) runs each instance `N` times.
The default is `1`. Fresh runs write deterministic per-run artifacts:

```text
<sweep>/<instance_id>/run-<k>.traj.json
<sweep>/<instance_id>/run-<k>.patch
<sweep>/all_preds.run-<k>.jsonl
```

Resume mode skips completed run files and launches only missing run slots.
Legacy flat `<sweep>/<instance_id>.traj.json` and `<sweep>/<instance_id>.patch`
files are still accepted for run 1 resume/inspection compatibility.

For rerun sweeps, aggregate `<sweep>/all_preds.jsonl` uses unique prediction
IDs and includes `original_instance_id` plus `run_index`. The per-run
`all_preds.run-<k>.jsonl` files keep original SWE-bench `instance_id` values
and are what `bench evaluate --backend sb-cli` submits, because sb-cli rejects
duplicate `instance_id` rows within one predictions file.

## Command

```bash
max bench evaluate \
  --sweep <sweep_dir> \
  [--dataset <dataset.jsonl>] \
  [--sb-subset swe-bench-m] \
  [--sb-split dev] \
  [--run-id <custom_run_id>] \
  [--backend sb-cli|none] \
  [--cost-attribution on|off] \
  [--timeout-per-instance 600] \
  [--parallel 4]
```

- `--backend sb-cli` shells out via `sb-cli submit ...` (and `get-report` as a
  fallback) then parses the generated report JSON.
- `--backend none` writes placeholder unresolved rows (useful where `sb-cli`
  is unavailable).

## `evaluation.json` schema

```json
{
  "instances": [
    {
      "instance_id": "<id>",
      "resolved": true,
      "runs": 3,
      "resolved_count": 1,
      "pass_at_1": false,
      "tests_passed": ["..."],
      "tests_failed": ["..."],
      "eval_exit_reason": "resolved|unresolved|patch_apply_failed|eval_error|skipped_no_patch",
      "eval_log_path": "optional/path/or/url",
      "patch_error_log": "captured git apply stderr (only when eval_exit_reason == patch_apply_failed; null/absent otherwise)"
    }
  ],
  "cost_attribution": [
    {
      "bucket": "resolved|env_setup|model_api|model_parse|step_limit|cost_limit|wallclock_timeout|agent_internal|unknown|uncategorized|TOTAL",
      "n": 12,
      "total_usd": 4.321,
      "mean_usd": 0.3601,
      "share_pct": 37.42
    }
  ]
}
```

`evaluation.json` is keyed by `instance_id` logically (stored as a list).
Every instance in `results.json` should have a corresponding evaluation row.
Rows missing from backend output are synthesized as:

- `skipped_no_patch` when the instance was not submitted (or had no patch), or
- `eval_error` when it was submitted but evaluator output was missing.

`bench evaluate` reports:

- `resolved_rate`: equivalent to `pass@1` for one-run sweeps.
- `pass@1`: fraction of instances whose first run resolved.
- `pass@k`: fraction of instances with at least one resolved run.
- `cost_attribution` (default `on`): a deterministic table that attributes
  per-trajectory `cost_usd` into terminal buckets. Resolved rows always land
  in `resolved` even if a legacy `failure_category` is also present. Rows with
  no `failure_category` and not resolved land in `uncategorized`. Missing
  `cost_usd` contributes `n` but is summed as `$0.00`; the CLI prints a
  warning line so operators know spend is understated.

When a sweep has only one run per instance, `resolved_rate`, `pass@1`, and
`pass@k` are the same value.

For rerun sweeps evaluated through sb-cli, each run is submitted separately
with run id `<run_id>-run-<k>`, then the run reports are merged back into one
evaluation row per original instance.

## `results.json` rerun fields

Per-instance rows include:

- `runs`: requested run count for that instance.
- `resolved_count`: number of resolved runs.
- `pass_at_1`: whether run 1 resolved.

The sweep summary includes `pass_at_k`, computed as the fraction of instances
with `resolved_count > 0`. The text summary footer also prints the effective
task count as `instances * runs` when reruns are used, so cost caps and resume
behavior can be interpreted against the actual number of launched run slots.

## `bench compare` interaction

`bench compare` now reads `evaluation.json` when present and uses `resolved`
as the pass/fail source of truth. If no `evaluation.json` exists, compare uses
the sweep rerun fields and falls back to the legacy heuristic
`outcome == submitted && failure_category == null` for older result files.

This affects:

- resolved counts (`baseline_resolved`, `candidate_resolved`, `resolved_delta`)
- transition classification (`pass->fail`, etc.)
- regression gating (`--max-regressions`)
- resolved-rate difference (`resolved_delta_rate`)
- `resolved_delta_ci95`, a 95% Wilson/Newcombe confidence interval
- `within_noise`, true when the confidence interval crosses zero
- optional cost-attribution deltas (`--cost-attribution on|off`), which compare
  baseline and candidate bucket spend with fields
  `n_baseline,total_usd_baseline,n_candidate,total_usd_candidate,delta_usd,share_pp_delta`

So a sweep with high submission-rate but zero resolved instances is treated as
fully regressed against a baseline that resolved everything.

Regression gating exits non-zero only when the confidence interval is entirely
below zero. If the raw delta is negative but the interval crosses zero, the
report is marked `within_noise: true` and the command exits successfully.
