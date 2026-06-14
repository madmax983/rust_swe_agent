# `bench report` — Sweep Summary Report

## Overview

`bench report` turns a completed sweep directory into a self-contained, shareable
report file (Markdown or HTML). It is designed to be dropped into a PR description,
Slack thread, or paper appendix without any copy-paste-massage from JSON.

## Usage

```
max bench report \
  --sweep ./my-sweep \
  --output ./my-sweep/report.md
```

With an optional baseline for delta comparison:

```
max bench report \
  --sweep ./my-sweep \
  --output ./my-sweep/report.md \
  --baseline ./baseline-sweep
```

## Options

| Flag | Default | Description |
|---|---|---|
| `--sweep <DIR>` | required | Completed sweep directory (must contain `results.json`). |
| `--output <FILE>` | required | Output file path. |
| `--baseline <DIR>` | — | Baseline sweep for resolved-delta comparison. |
| `--top-failures <N>` | `10` | Number of top failed instances to include. |
| `--format <FMT>` | `markdown` | Output format: `markdown` or `html`. |

## Report Sections

### Provenance

Records the run's origin: model name, harness git SHA, dataset path and split,
sweep start/end timestamps, total wallclock time, runs per instance, and total runs.

### Top-Line Metrics

| Metric | Notes |
|---|---|
| Total instances | All instances in the sweep. |
| Resolved | Count and rate (%). |
| Pass@1 | Fraction of instances where the first run resolved. |
| Pass@k | Fraction of instances resolved in any run. |
| Total cost USD | Sum of per-instance `cost_usd`. |
| $/resolved instance | Total cost divided by resolved count. |
| Mean lines changed (resolved) | From `evaluation.json` patch stats; `_no evaluation data_` when absent. |

### Failure Mix

A table with one row per outcome category: `resolved`, then each `failure_category`
(see [`docs/failure-categories.md`](failure-categories.md) for all values)
or `eval_exit_reason` when `evaluation.json` is present. Columns:

- `n` — instance count
- `share%` — share of total instances
- `cost_usd` — total cost for this category
- `cost%` — share of total sweep cost

### Top Failed Instances

The N most expensive unresolved instances (by `cost_usd`). Columns:

- `Instance` — `instance_id`
- `Category` — `failure_category`
- `Resolved/Total` — resolved run count / total runs
- `Cost USD` — per-instance cost
- `Excerpt` — last assistant message, truncated to 120 chars

Use `--top-failures N` to change the table size (default: 10).

### Evaluation

If `evaluation.json` is present, this section confirms data is available.
When absent, it renders: `_no evaluation data — run bench evaluate to populate_`.

### Delta (with `--baseline`)

When `--baseline` is supplied, the report appends a delta section produced by
the existing `bench compare` machinery: resolved delta, 95% CI, within-noise
verdict, top regressions, and top improvements.

## Output Formats

| Format | Description |
|---|---|
| `markdown` | (default) GitHub-flavored Markdown. |
| `html` | Single-file HTML with inline CSS, no external assets, no JavaScript. |

## Guarantees

- **Deterministic**: identical byte output for the same input sweep.
- **Exit code**: always 0 on success; non-zero only on I/O or schema-version errors.
  The command never gates CI — use `bench compare --max-regressions` for gating.
  For surfacing failures inline on a PR check, use `bench export-ci` to produce
  JUnit XML and GitHub Actions annotations (see [`spec-export-ci.md`](spec-export-ci.md)).
- **Redaction**: all text fields pass through the existing redaction pipeline
  before being written; the report never contains secrets that the underlying
  trajectories would have redacted.
- **Missing evaluation**: when `evaluation.json` is absent, eval-only sections
  render a graceful placeholder instead of failing.

## Artifact Schema

The report header includes the artifact schema version of the source sweep
(e.g., `v1.5`) so future readers know how the sweep was generated.

## Examples

Generate a markdown report:

```sh
max bench report \
  --sweep ./runs/sweep-2026-05-01 \
  --output ./runs/sweep-2026-05-01/report.md
```

Generate an HTML report with 20 top failures:

```sh
max bench report \
  --sweep ./runs/sweep-2026-05-01 \
  --output ./runs/sweep-2026-05-01/report.html \
  --format html \
  --top-failures 20
```

Compare against a baseline:

```sh
max bench report \
  --sweep ./runs/candidate \
  --baseline ./runs/baseline \
  --output ./runs/candidate/report.md
```

## Out of Scope

- Live-updating reports during a running sweep — use `bench tail`.
- Failure clustering / actionable triage — use `bench triage`.
- Portable, redacted, verifiable bundles — use `bench bundle`.
- Multi-sweep chronological history views.
- Embedded charts or sparkline graphics.
- A web dashboard.
