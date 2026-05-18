# `bench ladder` — Sweep Trend Ladder

## Overview

`bench ladder` reads a root directory of sweep subdirectories and renders a
chronological trend table showing resolved-rate, cost-per-resolved, and step
count across all valid sweeps. It is the single command that answers the
question: **"am I making progress?"**

The command reads **only on-disk artifacts and never calls a model provider**
(zero-cost guarantee). All output is deterministic given a fixed input
directory and flag set: no wall-clock timestamps appear in any output field.

## Usage

```
bench ladder --root <DIR> [OPTIONS]
```

### Required

| Argument | Description |
|---|---|
| `--root <DIR>` | Root directory; every direct subdirectory containing a valid `results.json` becomes one ladder row |

### Optional

| Flag | Default | Description |
|---|---|---|
| `--format <FMT>` | `text` | Output format: `text`, `json`, or `markdown` |
| `--dataset <ALIAS>` | — | Filter to sweeps whose recorded dataset matches this alias or path |
| `--last <N>` | — | Truncate to the N most recent matching sweeps after sorting |
| `--baseline <SWEEP_ID>` | — | Add a Δ vs baseline column computed against the named sweep row |

## Table Columns

| Column | Source | Notes |
|---|---|---|
| `sweep_id` | Subdirectory basename | Short name of the sweep directory |
| `date` | `manifest.runtime.started_at_utc` | UTC, formatted `YYYY-MM-DD HH:MM` |
| `model` | `manifest.model.name` | Redacted through the secret pipeline |
| `prompt_sha` | First 8 chars of `manifest.prompt_template.sha256` | Config hash for provenance |
| `n` | `results.total` | Total instance count |
| `resolved%` | `(resolved_count_sum / n) × 100` | Computed from per-instance `resolved_count` |
| `$/resolved` | `estimated_cost_usd / resolved_count_sum` | `null` / `—` when resolved == 0 |
| `mean_steps` | Mean of per-instance `steps` | `null` / `—` when no instances have steps |
| `Δ resolved%` | `resolved_pct[i] − resolved_pct[i−1]` | `—` for the first row |
| `Δ vs baseline` | `resolved_pct[i] − resolved_pct[baseline]` | Only shown with `--baseline` |

## Sweep Discovery and Sorting

1. Every **direct subdirectory** of `--root` that contains a `results.json`
   is considered.
2. Valid sweeps are sorted by `manifest.runtime.started_at_utc` ascending
   (lexicographic ISO 8601 sort; UTC timestamps collate correctly).
3. `--dataset` filtering is applied after sorting, `--last N` after filtering.

## Skipped Sweeps

Sweeps that cannot be included are listed in a trailing **Skipped** section
with a one-line reason, never silently dropped and never abort the command.

Causes that produce a skipped entry:

| Condition | Reason text |
|---|---|
| No `results.json` present | `no results.json found` |
| JSON parse failure | `JSON parse error: …` |
| Artifact kind mismatch (e.g. `trajectory` instead of `sweep_results`) | `results.json: artifact kind mismatch: expected sweep_results, found …` |
| Schema major version too new | `results.json: unsupported future artifact schema …` |
| Deserialization error | `deserialization error: …` |
| No provenance manifest | `missing required provenance fields (manifest absent)` |

## Output Formats

### `text` (default)

UTF-8 table using `comfy_table` rounded corners, followed by a Skipped section:

```
=== bench ladder ===
Root: /data/sweeps
Sweeps: 3  Skipped: 1

╭──────────┬──────────────────┬─────────┬────────────┬───┬───────────┬────────────┬────────────┬─────────────╮
│ sweep_id ┆ date             ┆ model   ┆ prompt_sha ┆ n ┆ resolved% ┆ $/resolved ┆ mean_steps ┆ Δ resolved% │
╞══════════╪══════════════════╪═════════╪════════════╪═══╪═══════════╪════════════╪════════════╪═════════════╡
│ sweep_a  ┆ 2026-04-28 00:00 ┆ model-a ┆ aaaa1234   ┆ 4 ┆ 50.00%    ┆ $0.0500    ┆ 4.0        ┆ —           │
│ sweep_b  ┆ 2026-04-29 00:00 ┆ model-a ┆ aaaa1234   ┆ 4 ┆ 75.00%    ┆ $0.0333    ┆ 4.0        ┆ +25.00pp    │
│ sweep_c  ┆ 2026-04-30 00:00 ┆ model-b ┆ bbbb5678   ┆ 4 ┆ 50.00%    ┆ $0.0500    ┆ 4.0        ┆ -25.00pp    │
╰──────────┴──────────────────┴─────────┴────────────┴───┴───────────┴────────────┴────────────┴─────────────╯
Skipped (1):
  sweep_bad: results.json: artifact kind mismatch: expected sweep_results, found trajectory
```

### `json`

A schema-versioned artifact object with stable field names:

```json
{
  "artifact_kind": "ladder_report",
  "schema_version": { "major": 1, "minor": 8 },
  "root": "/data/sweeps",
  "rows": [
    {
      "sweep_id": "sweep_a",
      "date": "2026-04-28 00:00",
      "model": "model-a",
      "prompt_sha": "aaaa1234",
      "n": 4,
      "resolved_pct": 50.0,
      "usd_per_resolved": 0.05,
      "mean_steps": 4.0
    },
    ...
  ],
  "skipped": [
    {
      "dir": "sweep_bad",
      "reason": "results.json: artifact kind mismatch: expected sweep_results, found trajectory"
    }
  ]
}
```

Fields with `null` values (`delta_resolved_pct` for the first row,
`usd_per_resolved` when nothing resolved, `delta_vs_baseline` without
`--baseline`) are omitted from the JSON output.

### `markdown`

GFM-compatible table suitable for pasting into pull request descriptions:

```markdown
## bench ladder

Root: /data/sweeps  Sweeps: 3  Skipped: 1

| sweep_id | date | model | prompt_sha | n | resolved% | $/resolved | mean_steps | Δ resolved% |
|---|---|---|---|---|---|---|---|---|
| sweep_a | 2026-04-28 00:00 | model-a | aaaa1234 | 4 | 50.00% | $0.0500 | 4.0 | — |
| sweep_b | 2026-04-29 00:00 | model-a | aaaa1234 | 4 | 75.00% | $0.0333 | 4.0 | +25.00pp |
| sweep_c | 2026-04-30 00:00 | model-b | bbbb5678 | 4 | 50.00% | $0.0500 | 4.0 | -25.00pp |

### Skipped (1)

| dir | reason |
|---|---|
| sweep_bad | results.json: artifact kind mismatch: expected sweep_results, found trajectory |
```

## Worked Example: One Week of Prompt Iteration

Suppose an operator runs four sweeps over a week, tweaking the prompt each day:

```
/experiments/
  sweep-2026-04-28/   # baseline: naive prompt
  sweep-2026-04-29/   # added code-reading instruction
  sweep-2026-04-30/   # tightened step-limit guard
  sweep-2026-05-01/   # reverted step-limit (regression!)
  sweep-bad/          # accidentally corrupted artifact_kind
```

Running:

```sh
max bench ladder --root /experiments --baseline sweep-2026-04-28 --format markdown
```

Produces a table like:

| sweep_id | date | model | prompt_sha | n | resolved% | $/resolved | mean_steps | Δ resolved% | Δ vs baseline |
|---|---|---|---|---|---|---|---|---|---|
| sweep-2026-04-28 | 2026-04-28 00:00 | claude-opus-4 | a1b2c3d4 | 50 | 30.00% | $0.2000 | 8.2 | — | +0.00pp |
| sweep-2026-04-29 | 2026-04-29 00:00 | claude-opus-4 | e5f6a7b8 | 50 | 36.00% | $0.1944 | 7.9 | +6.00pp | +6.00pp |
| sweep-2026-04-30 | 2026-04-30 00:00 | claude-opus-4 | c9d0e1f2 | 50 | 42.00% | $0.1667 | 7.4 | +6.00pp | +12.00pp |
| sweep-2026-05-01 | 2026-05-01 00:00 | claude-opus-4 | a1b2c3d4 | 50 | 32.00% | $0.1875 | 8.1 | -10.00pp | +2.00pp |

### Skipped (1)

| dir | reason |
|---|---|
| sweep-bad | results.json: artifact kind mismatch: expected sweep_results, found trajectory |

At a glance:
- **Apr 29**: code-reading instruction improved resolve rate by +6pp.
- **Apr 30**: step-limit guard pushed it another +6pp.
- **May 01**: reverting the guard caused a −10pp regression — back to only +2pp vs baseline.

The `Δ vs baseline` column confirms the cumulative gain; the `Δ resolved%` column isolates each day's marginal contribution.

## Determinism Guarantee

For a fixed `--root` and flags, the JSON and text outputs are **byte-for-byte
identical** across runs:

- Sweep rows are sorted by `started_at_utc` (ISO 8601 lexicographic, correct
  for UTC timestamps).
- Skipped entries are sorted by directory name.
- No wall-clock timestamps appear in any output field.
- Numeric formatting uses fixed decimal places (`{:.2}%`, `${:.4}`, `{:.1}`).

## Exit Codes

| Code | Condition |
|---|---|
| `0` | Successful render, including when zero sweeps match or all sweeps are skipped |
| Non-zero | I/O error reading `--root` directory itself, or invalid `--format` value |

The command **never gates CI** — that responsibility stays with `bench compare
--max-regressions`. `bench ladder` is informational only.

## Redaction

All free-text provenance fields rendered in the output (model name, prompt SHA)
are passed through the standard redaction pipeline (`surface::EXPORT`) before
display. A secret accidentally embedded in a provenance manifest will be masked
before it reaches a shareable ladder output.

## Non-Goals

- Web dashboard or interactive UI — text/markdown/JSON output only.
- Charts, sparklines, or non-text visualizations.
- Cross-machine aggregation from remote storage (`--root` is local only).
- Per-instance pass/fail breakdown across sweeps (see `bench instance-history`).
- Pairwise statistical significance (see `bench compare`).
- Causality analysis (future `bench bisect`).
