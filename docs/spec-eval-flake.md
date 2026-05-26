# `bench eval-flake` — evaluator-side verdict noise quantification

## Overview

`bench eval-flake` re-runs the existing evaluator pipeline against every
instance's already-captured `.patch` artifact N times and identifies which
instances' verdicts are non-deterministic. The result is an `eval-flake.json`
artifact that operators can pass to `bench compare --flake-report` to exclude
flaky instances from significance testing and resolved-rate deltas.

**Zero new model calls.** Cost is exclusively evaluator (sb-cli) wall-clock
time × replay factor. `total_cost_usd` is always `0.0`.

## Standard operator workflow

```
bench swebench  --sweep my-sweep …
bench evaluate  --sweep my-sweep …
bench eval-flake --sweep my-sweep --replays 3
bench compare   --baseline sweep-a --candidate sweep-b \
                --flake-report my-sweep/eval-flake.json
```

## CLI reference

```
bench eval-flake --sweep <PATH> [--replays <N>] [--output <PATH>] [--concurrency <N>]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--sweep` | required | Completed sweep directory (must contain `.patch` files and `results.json`). |
| `--replays` | `3` | Number of evaluator replays per instance. |
| `--output` | `<sweep>/eval-flake.json` | Output file path. |
| `--concurrency` | `4` | Maximum parallel evaluator workers. |

## `eval-flake.json` schema

```json
{
  "artifact_kind": "eval_flake_report",
  "schema_version": {"major": 1, "minor": 10},
  "instances": [
    {
      "instance_id": "django__django-12345",
      "verdicts": ["resolved", "unresolved", "resolved"],
      "is_flaky": true,
      "flake_rate": 0.667,
      "dominant_verdict": "resolved",
      "original_sweep_verdict": "resolved"
    }
  ],
  "summary": {
    "replays": 3,
    "instances_evaluated": 100,
    "flaky_count": 5,
    "flaky_rate": 0.05,
    "dominant_disagrees_with_sweep_count": 2
  },
  "total_cost_usd": 0.0
}
```

### Per-instance fields

| Field | Type | Description |
|-------|------|-------------|
| `instance_id` | `string` | SWE-bench instance identifier. |
| `verdicts` | `["resolved"\|"unresolved"\|"errored"]` | One entry per replay. Empty for skipped (no-patch) instances. |
| `is_flaky` | `bool` | `true` iff any two verdicts disagree. |
| `flake_rate` | `f32 [0, 1]` | Fraction of verdict-pairs that disagree: `disagreeing_pairs / C(N,2)`. |
| `dominant_verdict` | `string?` | Most common verdict across replays. `null` for skipped instances. |
| `original_sweep_verdict` | `string?` | Verdict from the sweep's `evaluation.json` (or `results.json` fallback). |

### Sweep-level summary fields

| Field | Type | Description |
|-------|------|-------------|
| `replays` | `u32` | Number of evaluator replays requested. |
| `instances_evaluated` | `u32` | Instances with at least one patch file evaluated. |
| `flaky_count` | `u32` | Instances with `is_flaky=true`. |
| `flaky_rate` | `f32 [0, 1]` | `flaky_count / instances_evaluated`. |
| `dominant_disagrees_with_sweep_count` | `u32` | Instances where `dominant_verdict != original_sweep_verdict`, signalling lucky/unlucky original evaluations. |

### Cost field

`total_cost_usd` is always `0.0`. The dominant cost is evaluator wall-clock
time, capped by `--concurrency`. On a 100-instance sweep with
`--replays 3 --concurrency 8`, total wall-clock added stays within
`2× bench evaluate` time.

## Skipped instances

Instances without a `.patch` file (agent errored before producing a patch)
are skipped: `verdicts: []`. They are excluded from `flaky_count` and
`instances_evaluated`.

## `bench compare --flake-report`

```
bench compare --baseline sweep-a --candidate sweep-b \
              --flake-report eval-flake.json
```

When `--flake-report` is supplied:

1. Flaky instances (`is_flaky=true`) are excluded from **both** the
   resolved-rate delta computation and the McNemar paired significance test.
2. The text summary names the exclusion count explicitly:
   ```
   3 flaky instances excluded; 97 paired instances used in significance test
   ```
3. The `CompareReport` JSON gains a `flaky_instances_excluded` field.

### Degenerate case

When **all** paired instances after exclusion are flaky, `bench compare` exits
with `usage_error` (exit code 2) and a message naming the flake count:

```
compare: all 5 paired instance(s) are flagged flaky in the flake report;
no instances remain for the significance test.
```

Without `--flake-report`, behavior is unchanged.

## `bench inspect --flake-report`

```
bench inspect --sweep sweep-dir --instance django__django-12345 \
              --flake-report eval-flake.json
```

When `--flake-report` is supplied and the instance appears in the flake report,
the human-readable output gains an `eval_flake` line:

```
eval_flake:       is_flaky=true flake_rate=0.667 verdicts=[resolved, unresolved, resolved]
```

This lets an operator triaging a single failing instance immediately see
whether they are chasing model behavior or evaluator noise.

## Exit codes

`bench eval-flake` uses the standard exit codes:
- `0` — success
- `2` (`usage_error`) — invalid arguments (e.g. `--replays 0`)
- `1` — unexpected runtime error

`bench compare --flake-report` adds no new exit codes. The degenerate
all-flaky case uses existing `usage_error` (2).

## Flake rate formula

For N replays with verdicts `v[0..N]`:

```
total_pairs       = N*(N-1)/2
disagreeing_pairs = |{(i,j) : i < j, v[i] ≠ v[j]}|
flake_rate        = disagreeing_pairs / total_pairs
```

**Example** — `[resolved, unresolved, resolved]` (N=3):
- Total pairs: 3
- Disagreeing: 2  ((0,1) and (1,2))
- `flake_rate = 2/3 ≈ 0.667`

## Out of scope

- **Re-running the agent.** No new model calls; no new patches.
- **Fixing flaky instances upstream.** We measure and report only.
- **Cross-evaluator-image comparison.** One evaluator image per invocation.
- **Streaming progress to `bench tail`.** Use `--log info` for progress.
- **Automatic re-weighting in McNemar.** v1 is hard-exclude only.
- **Replays on errored instances** (no patch). Those are skipped with
  `verdicts: []`.
