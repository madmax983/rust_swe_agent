# `bench triage` Spec

`bench triage` is a read-only post-processor for completed SWE-bench sweeps.
It consumes the artifacts already written by `bench swebench` and
`bench evaluate`, groups unresolved failures by deterministic terminal
signature, writes `triage.json`, and prints the highest-leverage clusters.

It never re-runs instances and never calls a model provider.

## CLI

```bash
max bench triage --sweep runs/sweep
```

Options:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--sweep <dir>` | required | Sweep directory containing `results.json`, `evaluation.json`, and trajectories. |
| `--bucket <failure_category>` | unset | Restrict output and `triage.json` to one bucket, for example `model_parse`. |
| `--min-cluster-size <k>` | `1` | Hide clusters smaller than `k`. Use `2` to remove singleton noise. |
| `--top <n>` | `10` | Number of ranked clusters printed in text mode. |
| `--format text\|json` | `text` | Text table or full JSON report on stdout. `triage.json` is written in both modes. |

Exit behavior:

- `0`: triage completed, even if there are no unresolved failures.
- Non-zero: missing or unreadable artifacts, unsupported future artifact schema,
  malformed JSON, or invalid CLI flags.
- If `evaluation.json` is absent, the error points the operator at
  `bench evaluate --sweep <dir>`.

## Inputs

The command reads:

- `<sweep>/results.json` for instance IDs, failure categories, and per-instance
  cost.
- `<sweep>/evaluation.json` for the resolved/unresolved decision.
- Per-instance trajectories at one of:
  - `<sweep>/<instance_id>.traj.json`
  - `<sweep>/<instance_id>/trajectory.json`
  - `<sweep>/<instance_id>/run-1.traj.json`

The candidate set is the union of:

- rows where `evaluation.json` says `resolved: false`
- still-unresolved rows in `results.json` whose outcome is `error` or whose
  `failure_category` is present; pass@k aggregate rows with
  `resolved_count > 0` are excluded even when they preserve the run-1
  failure category for pass@1 compatibility

This keeps evaluator omissions from hiding harness/runtime failures. Duplicate
instance IDs are de-duplicated before clustering.

## Signature Function

Each unresolved instance produces one pure `FailureSignature` from four fields:

1. `failure_category` (see [`docs/failure-categories.md`](failure-categories.md) for the full vocabulary)
2. The last assistant message tail, capped at the final 500 Unicode scalar
   values.
3. The last bash exit code, or `none` when no bash result is present.
4. The last non-empty stderr line from the last bash result.

Text fields are normalized before hashing:

- Lowercase the text.
- Split on whitespace and rejoin with single spaces.
- Replace tokens that look like filesystem paths with `<path>`.
- Replace ASCII number runs, including simple decimals, with `<num>`.

Examples:

| Raw | Normalized |
| --- | --- |
| `Error at /tmp/run-77/src/main.py line 42` | `error at <path> line <num>` |
| `C:\tmp\task-202\src\main.py:44` | `<path>` |
| `value=999` | `value=<num>` |
| `python:3.11` | `python:<num>` |

The stable hash is the first 16 lowercase hex characters of SHA-256 over the
normalized key:

```text
failure_category=<bucket>
assistant_tail=<normalized assistant tail>
bash_exit_code=<code or none>
stderr_line=<normalized stderr line>
```

Two failures collide only when all four normalized fields match.

## Output Schema

`bench triage` writes `<sweep>/triage.json`:

```json
{
  "sweep": "<dir>",
  "generated_at": "<utc-iso8601>",
  "clusters": [
    {
      "cluster_id": "<stable-hash>",
      "failure_category": "<bucket>",
      "signature_summary": "<one-line>",
      "instance_count": 2,
      "total_cost_usd": 3.5,
      "exemplar_instance_id": "<id>",
      "exemplar_trajectory_path": "<rel-path>",
      "instance_ids": ["<id>"]
    }
  ],
  "totals": {
    "clusters": 1,
    "instances": 2,
    "unclustered_instances": 1,
    "unresolved_cost_usd": 8.5
  }
}
```

`generated_at` is the only intentionally time-varying field. Cluster IDs,
ordering, summaries, exemplars, and totals are deterministic for the same
input artifacts and flags.

`totals.instances` counts instances in emitted clusters.
`totals.unclustered_instances` counts candidate instances hidden by
`--min-cluster-size`. `totals.unresolved_cost_usd` includes all candidates after
`--bucket` filtering, including clusters hidden by `--min-cluster-size`, so the
text table's cost share denominator remains the unresolved cost under review.

## Ranking

Clusters are sorted by:

1. `instance_count * total_cost_usd` descending.
2. `total_cost_usd` descending.
3. `failure_category` ascending.
4. `signature_summary` ascending.
5. `cluster_id` ascending.

The text table prints the top `N` clusters across the whole sweep using that
same order. The `% unresolved cost` column is
`cluster.total_cost_usd / totals.unresolved_cost_usd * 100`.

## Worked Fixture

The checked-in fixture at `tests/fixtures/triage/sweep` contains:

- Three failure buckets: `model_parse`, `env_setup`, and `step_limit`.
- A `model_parse` cluster with two instances whose raw paths and numbers differ
  but normalize to the same signature.
- A `model_parse` singleton plus one singleton in each other bucket.
- One resolved instance that must not appear in triage output.

Run it without mutating the fixture by copying it first:

```bash
cp -R tests/fixtures/triage/sweep /tmp/rust-swe-triage-fixture
max bench triage --sweep /tmp/rust-swe-triage-fixture --top 3
```

Expected top three clusters:

| Rank | Bucket | Count | Cost | Exemplar |
| ---: | --- | ---: | ---: | --- |
| 1 | `model_parse` | 2 | `3.5000` | `model-a-1` |
| 2 | `env_setup` | 1 | `6.0000` | `env-1` |
| 3 | `model_parse` | 1 | `5.0000` | `model-b-1` |

Rank 1 wins because `2 * 3.5 = 7.0`, which outranks `1 * 6.0 = 6.0`.
