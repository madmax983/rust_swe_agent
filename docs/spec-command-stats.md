# `bench command-stats` Spec

`bench command-stats` is a read-only post-processor for completed SWE-bench sweeps.
It consumes the artifacts already written by `bench swebench` and optionally
`bench evaluate`, extracts every bash command executed by the agent, aggregates
command-frequency and cost metrics segmented by outcome bucket, writes
`command-stats.json`, and prints a ranked table per bucket.

It never re-runs instances and never calls a model provider.

## CLI

```bash
rust-swe-agent bench command-stats --sweep runs/sweep
```

Options:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--sweep <dir>` | required | Sweep directory containing `results.json`, optionally `evaluation.json`, and trajectory files. |
| `--bucket resolved\|unresolved\|errored\|all` | unset | Restrict which outcome bucket's rows appear in non-"all" columns of the output. |
| `--min-invocations <n>` | `1` | Hide command heads with fewer than `n` total invocations. Useful for suppressing long-tail noise. |
| `--top <n>` | `15` | Number of ranked rows to print per outcome bucket in text mode. |
| `--compare resolved-vs-unresolved` | unset | Append a delta table sorted by `(resolved_share − unresolved_share)` descending. |
| `--filter <key>=<value>` | unset | Restrict the input set to instances matching the given filter (same syntax as `bench inspect --filter`). Example: `failure_category=model_parse`. |
| `--format text\|json` | `text` | Text table or full JSON report on stdout. `command-stats.json` is written in both modes. |

Exit behavior:

- `0`: aggregation completed, even when the sweep contains failures.
- Non-zero: missing or unreadable `results.json`, malformed JSON, or invalid CLI flags.

## Head-extraction rules

For every assistant turn with `actions`, each action string is decomposed into
one or more **command heads** — the normalized first token of each pipeline
segment, with common shell prefixes stripped.

### Prefix stripping (applied left to right until the actual command is found)

| Prefix | Example | Stripped to |
| --- | --- | --- |
| `sudo` | `sudo apt-get install python3` | `apt-get` |
| `time` | `time cargo test` | `cargo` |
| `env VAR=val ...` | `env RUST_LOG=debug cargo build` | `cargo` |
| Bare `VAR=val` assignments | `RUST_LOG=debug cargo build` | `cargo` |

Multiple prefixes are chained: `sudo time env A=1 pytest -x` → `pytest`.

### Pipeline decomposition

`|` splits a command into segments; each segment's head is extracted
independently. `||` (logical OR) is **not** treated as a pipeline delimiter.

| Command | Heads |
| --- | --- |
| `cat file.txt \| grep error \| sort -u` | `["cat", "grep", "sort"]` |
| `false \|\| true` | `["false"]` |
| `grep 'def test' .` | `["grep"]` |

### Edge cases

- Empty or whitespace-only commands produce no heads.
- Commands whose entire content is prefix tokens (e.g., `sudo`) produce no heads.

## Output schema

### `command-stats.json`

```json
{
  "sweep": "<path to sweep directory>",
  "generated_at": "<ISO-8601 UTC timestamp>",
  "totals": {
    "trajectories": <int>,
    "bash_steps": <int>,
    "unique_command_heads": <int>
  },
  "by_outcome": {
    "<resolved|unresolved|errored|all>": [
      {
        "command_head": "<str>",
        "instance_count": <int>,
        "invocation_count": <int>,
        "mean_calls_per_instance": <float>,
        "nonzero_exit_rate": <float>,
        "attributed_cost_usd": <float>
      }
    ]
  },
  "comparisons": [
    {
      "name": "resolved-vs-unresolved",
      "rows": [
        {
          "command_head": "<str>",
          "resolved_share": <float>,
          "unresolved_share": <float>,
          "delta": <float>
        }
      ]
    }
  ]
}
```

**Field definitions:**

| Field | Meaning |
| --- | --- |
| `command_head` | Normalized first token after prefix stripping, per the rules above. |
| `instance_count` | Number of distinct instances (trajectories) that used this command at least once. |
| `invocation_count` | Total number of times this command head was invoked across all instances in the bucket. |
| `mean_calls_per_instance` | `invocation_count / instance_count`. |
| `nonzero_exit_rate` | Fraction of invocations whose immediately following observation reported a non-zero exit code. `0.0` when all exits were zero; `1.0` when all were non-zero. |
| `attributed_cost_usd` | Sum of the model-call cost (`extra.cost`) for every assistant turn that produced an invocation of this command. When an assistant turn produces multiple commands, the full turn cost is attributed to each. |
| `resolved_share` | Fraction of all resolved-bucket invocations accounted for by this command. |
| `unresolved_share` | Fraction of all unresolved-bucket invocations accounted for by this command. |
| `delta` | `resolved_share − unresolved_share`. Positive values indicate commands more common in resolved runs. |

`comparisons` is omitted from the JSON when `--compare` is not passed.

Rows within each bucket are sorted by `invocation_count` descending, then
alphabetically by `command_head` for determinism. Delta rows are sorted by
`delta` descending.

### Determinism

Running `bench command-stats` twice on the same sweep directory produces
byte-identical `command-stats.json` files (modulo `generated_at`). Row
ordering is fully determined by the sort keys above; no hash-map iteration
order leaks into the output.

## Outcome bucket classification

| Bucket | Criterion |
| --- | --- |
| `resolved` | Instance appears in `evaluation.json` with `resolved: true`. |
| `errored` | Instance has `outcome: "error"` in `results.json` and is not in the resolved set. |
| `unresolved` | All other instances (e.g., `step_limit_reached`, `budget_exhausted`). |
| `all` | All instances regardless of outcome. |

When `evaluation.json` is absent, no instance is classified as resolved.

## Worked example

Given a sweep at `runs/sweep/` with three instances:

```
runs/sweep/
  results.json
  evaluation.json
  resolved-1.traj.json   # submitted, evaluation resolved: true
  unresolved-1.traj.json # step_limit_reached
  errored-1.traj.json    # outcome: error, failure_category: model_parse
```

Running:

```bash
rust-swe-agent bench command-stats \
  --sweep runs/sweep \
  --compare resolved-vs-unresolved \
  --top 5
```

Produces stdout similar to:

```
=== bench command-stats ===
Sweep: runs/sweep
Trajectories: 3  bash_steps: 8  unique_command_heads: 4

--- Outcome: resolved ---
╭──────┬──────────────┬────────────────┬──────────────────┬──────────────────────┬───────────────────┬─────────────────────╮
│ rank │ command_head │ instance_count │ invocation_count │ mean_calls/instance  │ nonzero_exit_rate │ attributed_cost_usd │
├──────┼──────────────┼────────────────┼──────────────────┼──────────────────────┼───────────────────┼─────────────────────┤
│ 1    │ cat          │ 1              │ 2                │ 2.00                 │ 0.000             │ 0.020000            │
│ 2    │ grep         │ 1              │ 1                │ 1.00                 │ 0.000             │ 0.010000            │
│ 3    │ pytest       │ 1              │ 1                │ 1.00                 │ 0.000             │ 0.020000            │
╰──────┴──────────────┴────────────────┴──────────────────┴──────────────────────┴───────────────────┴─────────────────────╯

--- Outcome: unresolved ---
...

--- Comparison: resolved-vs-unresolved ---
╭──────────────┬────────────────┬──────────────────┬─────────╮
│ command_head │ resolved_share │ unresolved_share │ delta   │
├──────────────┼────────────────┼──────────────────┼─────────┤
│ pytest       │ 0.2500         │ 0.0000           │ 0.2500  │
│ cat          │ 0.5000         │ 0.6667           │ -0.1667 │
│ grep         │ 0.2500         │ 0.3333           │ -0.0833 │
╰──────────────┴────────────────┴──────────────────┴─────────╯
```

The `pytest` row has a positive delta (+0.25) because it only appears in
resolved runs. This is the signal an operator needs to form a prompt hypothesis:
"encourage the agent to run `pytest` before submitting."

The JSON artifact is also written to `runs/sweep/command-stats.json`.

## Fixture sweep

A reference fixture sweep is checked in at
`tests/fixtures/command_stats/sweep/`. It contains three instances spanning
resolved, unresolved, and errored outcomes, and is used by the regression tests
in `tests/bench_command_stats.rs`.
