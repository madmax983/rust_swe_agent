# Agent Scriptability

## User Story

As an agent developer, I want scriptable tool boundaries, so that experiments
can add diagnostics, policy checks, and workflow context without rebuilding the
Rust core.

## Current Slice: MCP Servers And Tool Hooks

MCP servers are invocation-time providers. They let an experiment choose a
different active toolset per run without rebuilding the Rust core. A server is a
process that speaks MCP JSON-RPC over stdio:

```toml
[[agent.mcp_servers]]
command = "diagnostic-mcp"
timeout_secs = 30
```

```bash
max mini --task "Fix it" --mcp-server diagnostic-mcp
```

For each MCP server, the runner sends the standard lifecycle handshake:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"maxwells-daemon","version":"0.1.0"}}}
```

If the server responds with an older supported revision, the runner records that
negotiated version and uses it for subsequent stdio exchanges with that server.

Then it sends `notifications/initialized` and asks the server to list tools:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

The current MCP stdio adapter is a one-shot transport built on the stateless
`Environment` command runner: discovery and each tool call start a fresh process
and perform the lifecycle handshake. A future persistent MCP session mode should
use a separate configuration field from `agent.mcp_servers`, so one-shot and
long-lived behavior can be A/B tested without a breaking semantic change.

The MCP server returns definitions that are advertised to the model:

```json
{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"diagnose","description":"Run repository diagnostics.","inputSchema":{"type":"object","properties":{"query":{"type":"string"}}}}]}}
```

The assistant calls a tool with a fenced block whose language tag matches a
runtime tool name. For MCP-backed tools, the block body should be a JSON object
matching the tool's `inputSchema`:

````markdown
```diagnose
{"query":"check flaky test"}
```
````

The runner sends the MCP server a `tools/call` request:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"diagnose","arguments":{"query":"check flaky test"}}}
```

The MCP server returns content, which is converted into the next model-visible
observation:

```json
{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"diagnostic output"}],"isError":false}}
```

The built-in `bash` tool remains always available. The active toolset is
recorded in trajectory metadata under `info.toolset`, so A/B runs can be
compared against the actual tools shown to the model.

`[[agent.tools]]` still exists as a low-level command adapter escape hatch. It
maps a fenced tool block directly to one configured command and passes the block
body through `MAXWELL_TOOL_INPUT`. Prefer `agent.mcp_servers` for toolset
experiments; command aliases are not the MCP path. Tiny footgun, now labeled.

## Tool Hooks

Tool hooks are ordinary shell commands configured under `agent.hooks`. They are
rendered as MiniJinja templates, then executed in the same `Environment` as the
agent's tool processes. Hook results are model-visible in the next observation and
recorded in trajectory message extra data.

```toml
[agent]
tool_hook_timeout_secs = 10

[[agent.hooks.pre_tool_use]]
name = "guard"
command = 'test "$MAXWELL_COMMAND" != "rm -rf /"'
timeout_secs = 5

[[agent.hooks.post_tool_use]]
name = "git-status"
command = "git status --short"
```

`PreToolUse` hooks run before the assistant's tool call. A nonzero exit code or
timeout blocks the tool process and sends a blocked-tool observation back to the
model. `PostToolUse` hooks run only after the tool executes; their exit codes
are reported but do not abort the agent run.

## Context Contract

Hook commands can use MiniJinja variables (note: prefer environment variables for shell safety):

- `{{ hook.phase }}`: `pre_tool_use` or `post_tool_use`
- `{{ hook.name }}`
- `{{ tool.name }}`: `bash` or a runtime tool name
- `{{ task }}`
- `{{ model }}`
- `{{ step }}`
- `{{ command }}`: bash command or non-bash tool input
- `{{ tool_input }}`: bash command or non-bash tool input
- `{{ returncode }}`: `null` for `PreToolUse`
- `{{ stdout }}`: empty for `PreToolUse`
- `{{ stderr }}`: empty for `PreToolUse`
- `{{ output }}`: empty for `PreToolUse`
- `{{ timed_out }}`: `false` for `PreToolUse`
- `{{ total_cost_usd }}`

The same data is exposed as environment variables:

- `MAXWELL_HOOK_NAME`
- `MAXWELL_HOOK_PHASE`
- `MAXWELL_TOOL_NAME`
- `MAXWELL_TASK`
- `MAXWELL_MODEL`
- `MAXWELL_STEP`
- `MAXWELL_COMMAND`
- `MAXWELL_TOOL_INPUT`
- `MAXWELL_EXIT_CODE`
- `MAXWELL_STDOUT`
- `MAXWELL_STDERR`
- `MAXWELL_OUTPUT`
- `MAXWELL_TIMED_OUT`
- `MAXWELL_TOTAL_COST_USD`
- `MAXWELL_CONTEXT_JSON`

For compatibility, the old `RUST_SWE_AGENT_*` names are still populated with
the same values. New hooks should use `MAXWELL_*`.

Environment variable values are capped before spawning hook processes to avoid
OS argv+env size limits. Large strings include a truncation marker such as
`[truncated: original_bytes=200000]`. `MAXWELL_CONTEXT_JSON` remains
valid JSON, but its string fields are capped the same way. Hooks that need
lossless large output should read artifacts from the workspace rather than the
environment.

## Preflight: `bench scriptability-check`

`bench scriptability-check` is a zero-cost command that validates the full
scriptability configuration before any model call. It:

1. **Spawns each MCP server** in `agent.mcp_servers`, performs the full
   `initialize` → `notifications/initialized` → `tools/list` handshake, and
   records per-server `{name, command, ok, negotiated_protocol_version,
   tools: [{name, has_input_schema, schema_valid}], duration_ms, error?}`.

2. **Dry-runs each hook** in `agent.hooks.pre_tool_use` and
   `agent.hooks.post_tool_use`: renders the MiniJinja template against a
   deterministic synthetic context (see below), then executes the rendered
   command with the full `MAXWELL_*` / `RUST_SWE_AGENT_*` environment
   contract. Per-hook result: `{name, phase, ok, exit_code, duration_ms,
   template_render_ok, blocking (for pre_tool_use), stdout_bytes,
   stderr_bytes, error?}`. Uses `agent.tool_hook_timeout_secs`.

3. **Exits 0** when all servers and hooks pass; **exits 23**
   (`scriptability_check_failure`) on any failure — distinct from
   `preflight_failure` (3) so CI can route scriptability misconfig separately
   from infrastructure failures.

4. Makes **no model calls** and no network calls beyond what an MCP server
   itself initiates. The JSON artifact (`scriptability_check.json`) intentionally
   omits `total_cost_usd`.

5. **Redacts secrets** from hook command output and MCP server error messages
   using the existing redaction layer before writing to the artifact or stdout.

### Synthetic Hook Context

During preflight, hooks are executed with the following deterministic placeholder
values:

| Variable | Preflight value |
|----------|-----------------|
| `hook.phase` | `pre_tool_use` or `post_tool_use` |
| `hook.name` | hook name from config |
| `tool.name` | `bash` |
| `task` | `scriptability-check-preflight` |
| `model` | `preflight` |
| `step` | `0` |
| `command` | *(empty)* |
| `tool_input` | *(empty)* |
| `returncode` | `null` (pre) / `0` (post) |
| `stdout`, `stderr`, `output` | *(empty)* |
| `timed_out` | `false` |
| `total_cost_usd` | `0.0` |

### Usage

```bash
# Check the default scriptability config:
max bench scriptability-check

# Check a specific config file:
max bench scriptability-check --config myconfig.toml

# Write JSON artifact to a directory:
max bench scriptability-check --config myconfig.toml --output runs/preflight/

# CI: fail fast if any server or hook is misconfigured:
max bench scriptability-check --config ci-config.toml || exit 1
```

### Artifact

The `scriptability_check.json` artifact is schema-versioned and consistent with
the artifact-versioning conventions used in this repo:

```json
{
  "artifact_kind": "scriptability_check",
  "schema_version": {"major": 1, "minor": 10},
  "generated_at": "2026-01-01T00:00:00Z",
  "config": "ci-config.toml",
  "servers": [
    {
      "name": "mcp-0",
      "command": "diagnostic-mcp",
      "ok": true,
      "negotiated_protocol_version": "2025-11-25",
      "tools": [{"name": "diagnose", "has_input_schema": true, "schema_valid": true}],
      "duration_ms": 42
    }
  ],
  "hooks": [
    {
      "name": "guard",
      "phase": "pre_tool_use",
      "ok": true,
      "exit_code": 0,
      "duration_ms": 3,
      "template_render_ok": true,
      "blocking": false,
      "stdout_bytes": 0,
      "stderr_bytes": 0
    }
  ],
  "all_ok": true
}
```

## Future Hooks

Good next slices are pre-model context hooks, model-response hooks, and a
scripted model backend. Each should have a small typed contract, red tests, and
trajectory evidence before becoming part of the loop.
