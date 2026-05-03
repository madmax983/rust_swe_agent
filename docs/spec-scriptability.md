# Agent Scriptability

## User Story

As an agent developer, I want scriptable tool boundaries, so that experiments
can add diagnostics, policy checks, and workflow context without turning the
Rust core into a plugin framework.

## Current Slice: Tool Hooks

Tool hooks are ordinary shell commands configured under `agent.hooks`. They are
rendered as MiniJinja templates, then executed in the same `Environment` as the
agent's bash tool. Hook results are model-visible in the next observation and
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

`PreToolUse` hooks run before the assistant's bash command. A nonzero exit code
or timeout blocks the bash command and sends a blocked-tool observation back to
the model. `PostToolUse` hooks run only after the bash command executes; their
exit codes are reported but do not abort the agent run.

## Context Contract

Hook commands can use MiniJinja variables:

- `{{ hook.phase }}`: `pre_tool_use` or `post_tool_use`
- `{{ hook.name }}`
- `{{ tool.name }}`: currently `bash`
- `{{ task }}`
- `{{ model }}`
- `{{ step }}`
- `{{ command }}`
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
- `RUST_SWE_AGENT_EXIT_CODE`
- `RUST_SWE_AGENT_STDOUT`
- `RUST_SWE_AGENT_STDERR`
- `RUST_SWE_AGENT_OUTPUT`
- `RUST_SWE_AGENT_TIMED_OUT`
- `RUST_SWE_AGENT_TOTAL_COST_USD`
- `RUST_SWE_AGENT_CONTEXT_JSON`

## Future Hooks

Good next slices are pre-model context hooks, model-response hooks, and a
scripted model backend. Each should have a small typed contract, red tests, and
trajectory evidence before becoming part of the loop.
