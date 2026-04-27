# `bench evaluate` / `evaluation.json` contract

`bench swebench` writes sweep execution artifacts (`results.json`, per-instance
`*.traj.json`, `all_preds.jsonl`). `bench evaluate` is a post-processing step
that scores those artifacts using an evaluation backend and writes
`evaluation.json` in the same sweep directory.

## Command

```bash
rust-swe-agent bench evaluate \
  --sweep <sweep_dir> \
  [--dataset <dataset.jsonl>] \
  [--backend sb-cli|none] \
  [--timeout-per-instance 600] \
  [--parallel 4]
```

- `--backend sb-cli` shells out to `sb-cli` and parses its report.
- `--backend none` writes placeholder unresolved rows (useful where `sb-cli`
  is unavailable).

## `evaluation.json` schema

```json
{
  "instances": [
    {
      "instance_id": "<id>",
      "resolved": true,
      "tests_passed": ["..."],
      "tests_failed": ["..."],
      "eval_exit_reason": "resolved|unresolved|patch_apply_failed|eval_error|skipped_no_patch",
      "eval_log_path": "optional/path/or/url"
    }
  ]
}
```

`evaluation.json` is keyed by `instance_id` logically (stored as a list).
Every instance in `results.json` should have a corresponding evaluation row.
Rows missing from backend output are synthesized as:

- `skipped_no_patch` when the instance was not submitted (or had no patch), or
- `eval_error` when it was submitted but evaluator output was missing.

## `bench compare` interaction

`bench compare` now reads `evaluation.json` when present and uses
`resolved` as the pass/fail source of truth. If no `evaluation.json` exists,
behavior is unchanged and compare falls back to the legacy heuristic
`outcome == submitted && failure_category == null`.

This affects:

- resolved counts (`baseline_resolved`, `candidate_resolved`, `resolved_delta`)
- transition classification (`pass->fail`, etc.)
- regression gating (`--max-regressions`)

So a sweep with high submission-rate but zero resolved instances is treated as
fully regressed against a baseline that resolved everything.
