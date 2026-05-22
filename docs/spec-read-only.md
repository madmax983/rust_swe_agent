# Spec: mini read-only mode

`mini --read-only` runs the agent in analysis-only mode.

## Behavior
- Tool execution is blocked at dispatch time, including built-in `bash`.
- Invocation-time MCP registration is denied unless `--allow-mcp-in-read-only` is set.
- PR-publish flags are denied in read-only mode.
- Trajectory records mode with `info.other.mode = "read_only"`.
- If the model attempts any tool action in read-only mode, the run terminates with:
  - `info.outcome = "error"`
  - `info.failure_category = "read_only_violation"`.

## Render-only integration
`mini --render-only` includes a `mode` field in JSON (`"default"` or `"read_only"`) and a `Mode:` line in text output.
