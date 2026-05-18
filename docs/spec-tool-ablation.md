# spec-tool-ablation — `bench tool-ablation`

## Overview

`bench tool-ablation` answers the question *"does tool X actually matter for resolved-rate?"* with a single command. It runs the same instance set under N+1 conditions: a full-tool **baseline** arm, and one **no\_\<tool\>** arm for each tool removed. Results are written to `tool-ablation.json` and a ranked `tool-ablation-summary.txt` ordered by absolute `delta_resolved_vs_baseline`.

## Motivation

`bench matrix` already supports multi-arm experiments, but authoring N nearly-identical TOML manifests (one per removed tool) is high-friction. `bench tool-coverage` (#263) is descriptive (which tools were *called*), not prescriptive (which tools *matter*). This command removes the friction and produces the prescriptive ranking operators need to confidently prune tool lists.

## User Story

> As an agent operator iterating on the tool set, I want a one-command ablation that runs the same instance set with each tool removed in turn, so that I can rank tools by their contribution to resolved-rate and confidently drop the ones that don't earn their context cost.

## Acceptance Criteria

| # | Criterion | Status |
|---|-----------|--------|
| 1 | `bench tool-ablation` subcommand with spec doc | ✅ |
| 2 | Reuses dataset selection (`--limit`/`--sample`/`--seed`), `--sweep-cost-limit-usd`, `--resume`, `--parallel`, `--matrix-parallelism` | ✅ |
| 3 | Auto-generates baseline + one `no_<tool>` arm per tool; `--ablate` restricts the set | ✅ |
| 4 | `--render-only` (text) and `--render-only --format json` print manifest, $0, no network | ✅ |
| 5 | Writes `tool-ablation.json` (schema `tool-ablation-1.0`) with per-arm fields | ✅ |
| 6 | Writes `tool-ablation-summary.txt` ranked by `|delta_resolved_vs_baseline|` | ✅ |
| 7 | All arms run the same deterministic instance set; `instance_ids` recorded in JSON | ✅ |
| 8 | Trajectory artifacts live under `{output}/{arm_name}/` (compatible with `bench inspect`, `bench grep`, `bench compare`) | ✅ |
| 9 | Exits non-zero only on harness failure; budget exhaustion → `skipped_budget`, not error | ✅ |
| 10 | `--include-pair-ablation` opt-in flag; CLI prints arm count + cost warning before starting | ✅ |
| 11 | Redaction guarantees extend to all output (no API keys, provider tokens, or prompt secrets) | ✅ (inherits from sweep infrastructure) |

## Command

```
bench tool-ablation \
  --config base.toml \
  --dataset-path data.jsonl \
  --output ./ablation-run \
  [--ablate tool_name] ...         # default: all user tools
  [--render-only [--format json]]
  [--sweep-cost-limit-usd N]
  [--matrix-parallelism N]
  [--resume]
  [--limit N] [--sample N] [--seed N]
  [--parallel N]
  [--include-pair-ablation]
  [--skip-preflight] [--skip-model-probe]
  [--cancel-deadline-secs N]
```

## How Arms Are Generated

Tools to ablate are drawn from `agent.tools` in the base config (user-defined bash-wrapper tools). Built-in tools and MCP server tools are not enumerable from the config and are out of scope for this slice.

1. **`baseline`**: full tool set, no removal.
2. **`no_<tool_name>`**: config cloned with that tool removed from `agent.tools` and the raw JSON template context, so the system prompt also omits the tool.
3. **pair arms** (opt-in): one `no_<t1>_and_<t2>` arm per unordered pair.

`--ablate <name>` (repeatable) restricts ablation to the named subset. Unnamed tools are not ablated but are always present in the baseline and all other arms.

## Output Artifacts

### `tool-ablation.json`

Schema version: `"tool-ablation-1.0"`.

```jsonc
{
  "schema_version": "tool-ablation-1.0",
  "config_path": "/path/to/base.toml",
  "generated_at": "2026-05-18T12:34:56Z",
  "instance_ids": ["django__django-123", "..."],
  "arms": [
    {
      "name": "baseline",
      "ablated_tool": null,
      "status": "complete",
      "resolved": 10,
      "errored": 2,
      "total": 25,
      "cost_usd": 5.2300,
      "step_mean": 12.5,
      "step_p95": 35.0,
      "delta_resolved_vs_baseline": 0.0,
      "delta_cost_per_resolve_vs_baseline": 0.0
    },
    {
      "name": "no_bash_exec",
      "ablated_tool": "bash_exec",
      "status": "complete",
      "resolved": 7,
      "errored": 3,
      "total": 25,
      "cost_usd": 4.1000,
      "step_mean": 10.2,
      "step_p95": 28.0,
      "delta_resolved_vs_baseline": -0.12,
      "delta_cost_per_resolve_vs_baseline": -0.0873
    }
  ]
}
```

Arms that exceeded the shared budget appear with `"status": "skipped_budget"` and zero numeric fields.

### `tool-ablation-summary.txt`

Ranked by `|delta_resolved_vs_baseline|`, sign preserved. Example:

```
=== bench tool-ablation summary ===
config:     base.toml
instances:  25

╭──────────────────┬─────────────┬──────────┬──────────┬──────────┬───────────┬───────────────╮
│ Arm              │ Ablated Tool│ Status   │ Resolved │ Cost($)  │ Δresolved │ Δcost/resolve │
├──────────────────┼─────────────┼──────────┼──────────┼──────────┼───────────┼───────────────┤
│ no_bash_exec     │ bash_exec   │ complete │ 7        │ 4.1000   │ -0.1200   │ -0.0873       │
│ baseline         │ (baseline)  │ complete │ 10       │ 5.2300   │ +0.0000   │ +0.0000       │
│ no_search_files  │ search_files│ complete │ 10       │ 4.8000   │ +0.0000   │ -0.0123       │
╰──────────────────┴─────────────┴──────────┴──────────┴──────────┴───────────┴───────────────╯
```

### Per-arm sweep artifacts

Each arm's trajectory files, `results.json`, and other sweep artifacts live at `{output}/{arm_name}/`, identical to a standard `bench swebench` run. `bench inspect`, `bench grep`, and `bench compare` work without modification.

## `--render-only` Mode

Prints the planned manifest with no network calls, no API keys consumed, and no sweep directories created. Exit code: 0.

**Text (default):**
```
=== bench tool-ablation arm manifest ===
config:     base.toml
total arms: 3

╭──────────────────┬────────────────╮
│ Arm              │ Ablated Tool(s)│
├──────────────────┼────────────────┤
│ baseline         │ (baseline)     │
│ no_tool_a        │ tool_a         │
│ no_tool_b        │ tool_b         │
╰──────────────────┴────────────────╯
```

**JSON (`--format json`):**
```json
{
  "schema_version": "tool-ablation-1.0",
  "config_path": "/path/to/base.toml",
  "arms": [
    { "name": "baseline" },
    { "name": "no_tool_a", "ablated_tool": "tool_a" },
    { "name": "no_tool_b", "ablated_tool": "tool_b" }
  ]
}
```

## Budget Behavior

`--sweep-cost-limit-usd` is a shared ceiling across all arms. Once cumulative cost reaches the limit, remaining arms are recorded as `"status": "skipped_budget"` and the command exits with code 0 (budget exhaustion is not a harness failure). Skipped arms have zero numeric fields in `tool-ablation.json`.

## Pair Ablation (`--include-pair-ablation`)

Opt-in. Adds one arm per unordered pair of ablated tools. With N tools this adds N*(N-1)/2 arms. The CLI prints the arm count and a cost warning before any sweep starts. Use only when per-tool ablation results suggest interaction effects worth investigating.

## Redaction

All sweep artifacts, including `tool-ablation.json` and `tool-ablation-summary.txt`, inherit the redaction guarantees from `spec-secret-redaction.md`. Tool names in the output are drawn from the config (never from model responses) and do not contain API keys or provider tokens.

## Out of Scope

- Statistical-significance gating (covered by #176).
- Replacing or substituting tools (only *removal* ablation).
- Per-instance tool-level recommendations.
- Cross-model ablations (use `bench matrix` for the model axis).
- MCP server-level ablation (MCP tool names are not enumerable from the config alone).
- Auto-pruning the config (report is read-only; operators choose what to remove).
- Statistical confidence intervals on the delta (#176 covers methodology).

## Complexity

**M** — wraps the existing `bench matrix` engine. Adds an auto-manifest generator that introspects the configured tool list and defines a new reduction (delta vs. baseline) over per-arm sweep results. No new sweep mechanics, no new evaluator surface, no new trajectory schema fields.
