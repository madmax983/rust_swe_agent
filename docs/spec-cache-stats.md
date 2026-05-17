# `bench cache-stats` — Cache Effectiveness Summary

## Overview

`bench cache-stats` reads a completed sweep directory and surfaces prompt-cache
hit rate, estimated savings, and realized spend at both the sweep level and per
instance. It reads **only on-disk artifacts and never calls a model provider**
(zero-cost guarantee).

Prompt caching is the single largest cost lever when using the Anthropic API:
cache reads cost roughly 10× less than fresh input tokens. Silent cache
invalidation — caused by history-bounding rewrites, system-message edits,
sampling-parameter drift, or hook-injected content — can 10× a sweep's cost
without changing its solve rate. `bench cache-stats` turns the token counters
already recorded in `results.json` into an operator-readable ratio so that a
regression is visible before it shows up on a billing statement.

When `cache_creation + cache_read == 0` for the whole sweep (e.g. a non-
Anthropic provider that does not expose cache tokens), the command prints
`cache disabled or unsupported by provider` and exits 0 as an informational
notice, not an error.

## Usage

```
bench cache-stats --sweep <DIR> [OPTIONS]
```

### Required

| Argument | Description |
|---|---|
| `--sweep <DIR>` | Completed sweep directory produced by `bench swebench` |

### Optional

| Flag | Default | Description |
|---|---|---|
| `--format <FMT>` | `text` | Output format: `text` or `json` |
| `--top <N>` | `10` | Number of per-instance rows to display (worst first) |
| `--baseline <DIR>` | — | Baseline sweep directory; adds Δ hit_rate and Δ realized_spend_usd columns |

## Key Concepts

### `cache_hit_rate`

```
cache_hit_rate = cache_read_tokens / (input_tokens + cache_read_tokens + cache_creation_tokens)
```

A value of 1.0 means every prompt token was served from cache (ideal). A value
of 0.0 means no cache reads — all tokens were either fresh input or cache
creation (cold run).

### `estimated_savings_usd_vs_cold`

The USD saved compared to a hypothetical cold run where every cached-read token
was instead billed as fresh input:

```
estimated_savings_usd_vs_cold =
    cache_read_tokens × (input_price_per_Mtok / 1 000 000) × (1 − cache_read_multiplier)
```

Uses `claude-3-5-sonnet` pricing (`$3/Mtok` input, `0.10×` for cache reads).

### `realized_cache_spend_usd`

The actual cost of all cache operations (reads + creations):

```
realized_cache_spend_usd =
    cache_read_tokens     × input_price × cache_read_multiplier     (0.10×)
  + cache_creation_tokens × input_price × cache_creation_multiplier (1.25×)
```

## JSON Schema

`--format json` prints to stdout and also writes `cache-stats.json` in the
sweep directory. The artifact is schema-versioned and suitable for snapshot
diffing in CI.

```json
{
  "artifact_kind": "cache_stats_report",
  "schema_version": { "major": 1, "minor": 8 },
  "sweep": "/path/to/sweep",
  "generated_at": "2026-01-01T00:00:00Z",
  "cache_disabled": false,
  "sweep_totals": {
    "total_input_tokens": 80000,
    "total_cache_read_tokens": 120000,
    "total_cache_creation_tokens": 80000,
    "cache_hit_rate": 0.4286,
    "estimated_savings_usd_vs_cold": 0.000324,
    "realized_cache_spend_usd": 0.000336
  },
  "instances": [
    {
      "instance_id": "cold-1",
      "total_input_tokens": 50000,
      "total_cache_read_tokens": 0,
      "total_cache_creation_tokens": 50000,
      "cache_hit_rate": 0.0,
      "estimated_savings_usd_vs_cold": 0.0,
      "realized_cache_spend_usd": 0.0001875
    }
  ],
  "baseline": {
    "baseline_sweep": "/path/to/baseline",
    "delta_hit_rate": 0.0714,
    "delta_realized_spend_usd": -0.000042
  }
}
```

`baseline` is `null` / omitted when `--baseline` is not supplied.

### Schema version

`cache_stats_report` artifacts carry the harness-wide `schema_version` (currently
`{ "major": 1, "minor": 8 }`), the same value written into every other artifact
produced by this binary. There is no per-artifact override. The version contract
follows the rules in `docs/artifact-contract.md`: major bumps are breaking, minor
bumps are additive. Snapshot tests should match against `schema_version.major`
rather than the full `major.minor` pair to remain stable across minor releases.

### Redaction safety

Cache stats contain no message content — only token counts and derived USD
values. Output is safe to paste into a PR description or a Slack thread.

## Exit Codes

| Code | Meaning |
|---|---|
| 0 | Success (including cache-disabled informational case) |
| 2 | Usage or configuration error (bad `--format`, missing `--sweep`) |

## Examples

```bash
# Text summary — identify which instances had poor cache efficiency
bench cache-stats --sweep ./results

# JSON artifact for CI snapshot diffing
bench cache-stats --sweep ./results --format json

# Top 5 worst-cache instances only
bench cache-stats --sweep ./results --top 5

# Compare two configs — did the prompt change invalidate the cache?
bench cache-stats --sweep ./results-v2 --baseline ./results-v1

# JSON comparison for programmatic diffing
bench cache-stats --sweep ./results-v2 --baseline ./results-v1 --format json
```

## Performance Notes

`bench cache-stats` reads only `results.json` — it does not open individual
trajectory files. Runtime is proportional to the number of instances in the
sweep and is typically under 100 ms for sweeps of any practical size.

## Scope

- **In scope**: observability of already-recorded cache token counts.
- **Out of scope**: modifying how `cache_control` blocks are sent to the
  provider; real-time display during a live sweep; per-turn cache attribution;
  non-Anthropic provider cache semantics; recommending target hit rates.
