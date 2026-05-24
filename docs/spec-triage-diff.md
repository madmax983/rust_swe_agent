# `bench triage-diff` Spec

`bench triage-diff` is a zero-cost, read-only post-processor that compares failure-cluster composition between two completed SWE-bench sweeps.

## CLI

```bash
max bench triage-diff --baseline <dir> --candidate <dir>
```

Options:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--baseline <dir>` | required | Baseline sweep directory. |
| `--candidate <dir>` | required | Candidate sweep directory. |
| `--auto-triage` | `false` | Run `bench triage` on baseline and/or candidate if `triage.json` is missing. |
| `--min-cluster-size <k>` | `1` | Filter cluster deltas; a cluster surfaces if baseline OR candidate count is `>= k`. |
| `--top <n>` | `10` | Number of entries to print in text lists/tables. |
| `--output <path>` | `triage-diff.json` in candidate sweep dir | Output path for the JSON report. |
| `--format text\|json` | `text` | Output format: `text` or `json` (stdout). |
| `--fail-on-regression` | `false` | Make the command exit with non-zero code `6` when the regression set is non-empty. |

Exit behavior:

- `0`: Success, regardless of whether candidate resolved rate is better, worse, or identical.
- `6`: When `--fail-on-regression` is enabled and regressions are detected (corresponds to `ExitCode::RegressionGateFailure`).
- Non-zero: Missing sweeps, missing `triage.json` artifacts when `--auto-triage` is false, or invalid arguments.

## Inputs

The command reads:
- `triage.json` from each sweep (generating it if missing and `--auto-triage` is true).
- `results.json` and `evaluation.json` from each sweep to determine accurate instance resolved/unresolved status.
- Trajectory files if necessary to dynamically reconstruct signatures for unclustered instances.

## Regression and Win Sets

- **Regression Set**: Instances `resolved: true` in baseline AND `resolved: false` in candidate. They are grouped by the candidate-side failure cluster they fell into.
- **Win Set**: Instances `resolved: false` in baseline AND `resolved: true` in candidate. They are grouped by the baseline-side failure cluster they escaped.

## Output JSON Schema

Writes `triage-diff.json`:

```json
{
  "schema_version": "triage-diff-1.0",
  "baseline_sweep": "<dir>",
  "candidate_sweep": "<dir>",
  "cluster_deltas": [
    {
      "cluster_id": "<stable-hash>",
      "failure_category": "<category>",
      "signature_summary": "<summary>",
      "baseline_count": 2,
      "candidate_count": 5,
      "delta": 3,
      "delta_pct": 150.0
    }
  ],
  "new_clusters": [
    {
      "cluster_id": "<stable-hash>",
      "failure_category": "<category>",
      "signature_summary": "<summary>",
      "instance_count": 3,
      "instance_ids": ["<id1>", "<id2>", "<id3>"]
    }
  ],
  "resolved_clusters": [],
  "regression_instances": [],
  "win_instances": []
}
```

Field names are locked at `v1` to ensure downstream tool integration remains stable.
