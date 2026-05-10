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
rust-swe-agent mini --task "Fix it" --mcp-server diagnostic-mcp
```

For each MCP server, the runner sends the standard lifecycle handshake:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"rust-swe-agent","version":"0.1.0"}}}
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
body through `RUST_SWE_AGENT_TOOL_INPUT`. Prefer `agent.mcp_servers` for toolset
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
command = 'test "$RUST_SWE_AGENT_COMMAND" != "rm -rf /"'
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

- `RUST_SWE_AGENT_HOOK_NAME`
- `RUST_SWE_AGENT_HOOK_PHASE`
- `RUST_SWE_AGENT_TOOL_NAME`
- `RUST_SWE_AGENT_TASK`
- `RUST_SWE_AGENT_MODEL`
- `RUST_SWE_AGENT_STEP`
- `RUST_SWE_AGENT_COMMAND`
- `RUST_SWE_AGENT_TOOL_INPUT`
- `RUST_SWE_AGENT_EXIT_CODE`
- `RUST_SWE_AGENT_STDOUT`
- `RUST_SWE_AGENT_STDERR`
- `RUST_SWE_AGENT_OUTPUT`
- `RUST_SWE_AGENT_TIMED_OUT`
- `RUST_SWE_AGENT_TOTAL_COST_USD`
- `RUST_SWE_AGENT_CONTEXT_JSON`

Environment variable values are capped before spawning hook processes to avoid
OS argv+env size limits. Large strings include a truncation marker such as
`[truncated: original_bytes=200000]`. `RUST_SWE_AGENT_CONTEXT_JSON` remains
valid JSON, but its string fields are capped the same way. Hooks that need
lossless large output should read artifacts from the workspace rather than the
environment.

## Future Hooks

Good next slices are pre-model context hooks, model-response hooks, and a
scripted model backend. Each should have a small typed contract, red tests, and
trajectory evidence before becoming part of the loop.
