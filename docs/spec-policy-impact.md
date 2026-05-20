# spec-policy-impact: `bench policy-impact`

## Overview

`bench policy-impact` is a read-only post-sweep diagnostic subcommand that analyzes sweep results and trajectory records to assess the exact impact of security policy rules, allowed actions, and blocked commands on sweep outcomes (e.g., resolved rates). It outputs structured, formatted text reports using `comfy_table` or schema-versioned, deterministically sorted JSON reports (with sensitive command details redacted via the default `Redactor`).

---

## Output Metrics & Aggregations

The diagnostic aggregates the following data points across all instances processed in the target sweep:

1. **Sweep Policy Totals**:
   - `allowed`: Cumulative number of actions permitted by security policies.
   - `asked`: Cumulative number of human-in-the-loop permission requests.
   - `blocked`: Cumulative number of actions rejected by security policies.
   - `yolo_bypassed`: Cumulative number of actions executed with YOLO/override bypass.

2. **Policy Rule Impact**:
   - Aggregated by rule label (e.g., `policy_rule` field in trajectory message extras).
   - Sorted in descending order by `block_count` and lexicographically ascending by `rule_label`.
   - Fields:
     - `rule_label`: Unique rule identifier.
     - `block_count`: Number of times this rule blocked an action.
     - `affected_instances`: Number of unique sweep instances impacted by this rule.
     - `affected_instance_ids`: Sorted list of instance IDs impacted by this rule.
     - `top_blocked_command`: The most frequently blocked command under this rule (redacted). Ties are broken alphabetically before redaction.

3. **Outcome Correlation**:
   - Divides sweep instances into two groups:
     - **Blocked Group**: Instances encountering at least one `policy_blocked: true` action or having a trajectory `blocked` count > 0.
     - **Unblocked Group**: Instances with no policy blocks.
   - For each group, it calculates:
     - `total_count`: Total instances in the group.
     - `resolved_count`: Total instances in the group that were successfully resolved.
     - `unresolved_count`: Total unresolved instances in the group.
     - `errored_count`: Total instances in the group that encountered tool or platform execution errors.
     - `resolved_rate`: `resolved_count / total_count` (as a ratio).
   - `delta_resolved_rate`: `blocked_group.resolved_rate - unblocked_group.resolved_rate`.

---

## JSON Schema

When called with `--format json`, the subcommand outputs a schema-versioned JSON structure to stdout:

```json
{
  "policy_impact_report": {
    "schema_version": "1.0",
    "timestamp": "2026-05-19T23:13:30Z",
    "totals": {
      "allowed": 10,
      "asked": 1,
      "blocked": 6,
      "yolo_bypassed": 1
    },
    "rules": [
      {
        "rule_label": "rule_b",
        "block_count": 4,
        "affected_instances": 1,
        "affected_instance_ids": ["inst_3"],
        "top_blocked_command": "cat [REDACTED]"
      },
      {
        "rule_label": "rule_a",
        "block_count": 2,
        "affected_instances": 1,
        "affected_instance_ids": ["inst_2"],
        "top_blocked_command": "rm -rf /"
      }
    ],
    "outcome_correlation": {
      "blocked_group": {
        "total_count": 2,
        "resolved_count": 1,
        "unresolved_count": 1,
        "errored_count": 0,
        "resolved_rate": 0.5
      },
      "unblocked_group": {
        "total_count": 1,
        "resolved_count": 1,
        "unresolved_count": 0,
        "errored_count": 0,
        "resolved_rate": 1.0
      },
      "delta_resolved_rate": -0.5
    }
  }
}
```

---

## CLI Reference

```bash
max bench policy-impact --sweep <SWEEP_DIR> [OPTIONS]
```

### CLI Arguments

| Flag/Argument | Description | Default |
|---|---|---|
| `-s`, `--sweep` | Path to the completed sweep directory (containing `results.json` and trajectories). | — (Required) |
| `--format` | Output report format: `text` (beautiful styled tables) or `json` (serialized payload). | `text` |

### Stable Exit Codes

Following the project's exit code guidelines:
- `0` — Success. The policy impact analysis was run successfully and output generated.
- `2` — Usage/Configuration Error. The specified sweep directory does not exist, `results.json` is missing, or trajectory records cannot be located or parsed.

---

## Non-Goals

- Real-time policy blocking feedback or dynamic policy changes (handled exclusively during agent loop executions).
- Support for remote/cloud sweep directory scanning without local mounts.
- Attributing policy blocks to specific LLM models when running mixed sweeps.
