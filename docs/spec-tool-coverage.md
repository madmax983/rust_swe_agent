# `bench tool-coverage` Spec

## User Story

As an operator A/B testing MCP toolsets across SWE-bench sweeps, I want a per-sweep summary of how often each registered tool was invoked, segmented by outcome bucket and broken down per-instance, so that I can see whether the tools I configured were actually used, whether they correlated with resolution, and whether any tool was dead weight in the configured toolset.

## CLI

```bash
rust-swe-agent bench tool-coverage --sweep <dir> [options]
```

### Required flags

| Flag | Description |
|------|-------------|
| `--sweep <dir>` | Completed sweep directory (must contain `results.json` and `*.traj.json` files) |

### Optional flags

| Flag | Default | Description |
|------|---------|-------------|
| `--format text\|json` | `text` | Output format for stdout |
| `--bucket resolved\|unresolved\|errored\|all` | (all shown) | Restrict text output to one bucket |
| `--filter key=value` | (none) | Same filter syntax as `bench inspect --filter` |
| `--min-invocations N` | `0` | Hide tools with fewer than N total invocations from the text table. Never affects `tool-coverage.json`. |
| `--per-instance` | false | Emit per-instance tool call counts in JSON output and artifact |

## Behavior

`bench tool-coverage` is **read-only**: it never re-runs instances, never calls a model, and never modifies existing artifacts. It reads `results.json`, optional `evaluation.json`, and all `*.traj.json` files in the sweep directory.

### Tool universe

The report enumerates every tool advertised to the model for the sweep. The source of truth is `info.toolset` recorded in each trajectory (the `ToolsetManifest` written by the agent at startup). The built-in `bash` tool is included as a first-class row so MCP-vs-bash share is directly visible. Tools that were **registered but never called** appear as zero-invocation rows so dead config is loud, not silent.

The union of all toolsets seen across the sweep is used as the universe when toolset drift exists.

### Per-tool aggregation

For each tool in the universe:

| Field | Description |
|-------|-------------|
| `total_invocations` | Total times the tool was invoked across all instances |
| `instances_used` | Count of distinct instances that called the tool ≥1× |
| `mean_invocations_per_using_instance` | `total_invocations / instances_used` (0 when unused) |
| `share_of_all_tool_calls` | Fraction of all tool invocations this tool accounts for |
| `source` | `"builtin"`, `"mcp"`, or `"config"` |
| `mcp_server` | Server command (only present when `source = "mcp"`) |
| `by_outcome` | Per-bucket metrics (see below) |

### Outcome correlation

For each tool, a `by_outcome` block contains metrics per outcome bucket (`resolved`, `unresolved`, `errored`, `all`):

| Field | Description |
|-------|-------------|
| `instances_used` | Instances in this bucket that called the tool ≥1× |
| `instances_total` | Total instances in this bucket |
| `usage_rate` | `instances_used / instances_total` |
| `resolved_rate_when_used` | Resolved share among instances that called the tool ≥1× (null when unused) |
| `resolved_rate_when_not_used` | Resolved share among instances that did NOT call the tool (null when all used it) |

The delta `resolved_rate_when_used − resolved_rate_when_not_used` is the key operator signal.

### Bash invocation counting

Every non-`__SUBMIT__` action that is not a structured tool call (`name:{json}` format) counts as one `bash` invocation. Tool calls are detected by the pattern: `identifier:{...}` where the identifier contains only word characters.

### Tool call format

MCP and adapter tool calls in trajectories appear in actions as:
```
tool_name:{"key": "value", ...}
```

The tool name is extracted as the prefix before `:{`.

### Toolset drift

When different instances in the same sweep recorded different `info.toolset` values, the report flags this in a `toolset_drift` block:

```json
{
  "toolset_drift": {
    "toolsets": [
      {
        "toolset_fingerprint": "analyze_tool,bash",
        "instance_count": 1,
        "tools": ["analyze_tool", "bash"]
      },
      {
        "toolset_fingerprint": "bash,search_tool",
        "instance_count": 1,
        "tools": ["bash", "search_tool"]
      }
    ]
  }
}
```

Aggregations still proceed using the union universe. The text output prints a drift notice when drift is detected.

### Dead-tool surfacing

The text output always includes an `Unused tools:` line listing every tool in the universe with zero invocations. Empty when all tools were called at least once.

## Output schema

### `tool-coverage.json`

```json
{
  "sweep": "/path/to/sweep",
  "generated_at": "2026-05-17T00:00:00Z",
  "tool_universe": [
    {"name": "bash", "source": "builtin"},
    {"name": "diagnostic_search", "source": "mcp", "mcp_server": "diagnostic-mcp"}
  ],
  "by_tool": {
    "bash": {
      "total_invocations": 5,
      "instances_used": 3,
      "mean_invocations_per_using_instance": 1.67,
      "share_of_all_tool_calls": 0.714,
      "source": "builtin",
      "by_outcome": {
        "resolved": {
          "instances_used": 1,
          "instances_total": 1,
          "usage_rate": 1.0,
          "resolved_rate_when_used": 1.0,
          "resolved_rate_when_not_used": null
        },
        "unresolved": { "..." : "..." },
        "errored":    { "..." : "..." },
        "all":        { "..." : "..." }
      }
    },
    "diagnostic_search": {
      "total_invocations": 2,
      "instances_used": 1,
      "mean_invocations_per_using_instance": 2.0,
      "share_of_all_tool_calls": 0.286,
      "source": "mcp",
      "mcp_server": "diagnostic-mcp",
      "by_outcome": { "..." : "..." }
    }
  },
  "toolset_drift": null,
  "unused_tools": [],
  "per_instance": null
}
```

### Source labels

| Trajectory value | Report label |
|-----------------|--------------|
| `built_in` | `builtin` |
| `mcp_server` | `mcp` |
| `command_adapter` | `config` |
| other | `runtime_provider` |

## Exit codes

Follows the project's stable contract:

| Code | When |
|------|------|
| `0` | Success, regardless of usage mix. "No tools were used" is a finding, not an error. |
| `1` | I/O error, JSON parse error, missing artifacts |
| `2` | Invalid flag values (e.g. unknown `--format`) |

## Determinism

Running `bench tool-coverage` twice on the same sweep produces byte-identical `tool-coverage.json` output, modulo the `generated_at` timestamp. All ordering (tool_universe, by_tool keys, unused_tools, per_instance rows) is fully deterministic.

## Non-goals

- **Tool-argument analysis**: Arguments are already stored in trajectories; a follow-up `bench tool-args` can layer on later.
- **Per-turn cost attribution to specific tool calls**: This slice counts invocations, not dollars.
- **Causal inference**: The report surfaces the `resolved_rate_when_used − resolved_rate_when_not_used` delta; significance gating is orthogonal.
- **Within-turn ordering of tool calls**: Scalar per-tool counts only.
- **Recommending toolsets or auto-pruning**: Surface the data; let the operator decide.
- **Streaming live tool-call counts during a running sweep**: Post-hoc only.

## Worked examples

### Dead-tool case

A sweep where `diagnostic-mcp` is configured but the agent never calls its tools:

```
=== bench tool-coverage ===
Sweep: /runs/sweep-2026-05-17

╭─────────────────────┬─────────┬───────────────────┬────────────────┬──...
│ tool                │ source  │ total_invocations  │ instances_used │  ...
├─────────────────────┼─────────┼───────────────────┼────────────────┼──...
│ bash                │ builtin │ 42                 │ 10             │  ...
╰─────────────────────┴─────────┴───────────────────┴────────────────┴──...

Unused tools: diagnostic_search
```

The `diagnostic_search` row is omitted from the text table because `--min-invocations 1` (or the default) filters it, but it still appears in `tool-coverage.json` and in the `unused_tools` line.

### Toolset-drift case

When a config edit mid-sweep changes the MCP server:

```
=== bench tool-coverage ===
Sweep: /runs/sweep-drifted
Tool universe: 4 tools

Toolset Drift detected: 2 distinct toolsets observed across the sweep.
  [8 instance(s)]: analyze_tool, bash
  [2 instance(s)]: bash, search_tool
```

Aggregations proceed using the union {bash, analyze_tool, search_tool}, with each tool's `instances_total` counting only instances where that tool was in scope.
