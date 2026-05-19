# Spec: `agent env preview` (Issue #313)

`agent env preview` prints a structured summary of the agent's runtime
environment — filesystem paths, network egress, hooks, MCP servers, sensitive
environment variables (redacted), and policy rules — **without running the
agent or calling a model**.  It exits 13 (`env_preview_warning`) when risky
findings are detected so CI gates can block dangerous configurations.

---

## Invocation

```
max agent env preview \
    --env <type> \
    --task <task-description> \
    [--config <path-to-config.toml>] \
    [--format json] \
    [--show-values]
```

### Flags

| Flag | Required | Description |
|------|----------|-------------|
| `--env` | yes | Environment type: `local` or `docker`. |
| `--task` | yes | Task description string (used for context; passed through the redactor). |
| `--config` | no | Path to a TOML config file (overlays defaults). |
| `--format` | no | Output format: `text` (default) or `json`. |
| `--show-values` | no | Show actual env var values instead of `[REDACTED:…]` markers. |

---

## Exit Codes

| Code | Class | Condition |
|------|-------|-----------|
| 0 | `success` | Preview printed; no risky findings. |
| 2 | `usage_error` | Missing required flag (`--env`, `--task`) or invalid config. |
| 13 | `env_preview_warning` | Preview printed but at least one risky finding was detected. |

---

## Text Output Example (docker — clean)

```
=== Agent Environment Preview ===
env_type:       docker
host_paths:     (none)
network_egress: unrestricted

--- Hooks ---
  (none)

--- MCP Servers ---
  mcp-0: /workspace/bin/my-server --port 9000

--- Env Vars (sensitive) ---
  (none)

--- Policy ---
  profile:     safe
  extra_deny:  (none)
  extra_allow: (none)

--- Findings: CLEAN ---
```

> **Note:** Local environment previews (`--env local`) always produce at least
> one finding (`Local environment: bash commands are not confined to workdir`)
> and exit 13, because `LocalEnvironment` does not confine bash commands to the
> configured workdir.

When findings are present (local with a wide path and a sensitive env var):

```
--- Findings ---
  [WARNING] Local environment: bash commands are not confined to workdir (full host filesystem access)
  [WARNING] Local env with wide host path: /
  [WARNING] Sensitive env var 'ANTHROPIC_API_KEY' is set and will be forwarded to the agent
```

---

## JSON Output Schema (schema_version: 1)

```json
{
  "env_preview": {
    "schema_version": 1,
    "env_type": "local",
    "host_paths": ["/workspace"],
    "network_egress": "unrestricted",
    "hooks": {
      "pre_tool_use": [
        { "name": "pre-check", "command": "echo pre" }
      ],
      "post_tool_use": []
    },
    "mcp_servers": [
      {
        "name": "mcp-0",
        "command": "/usr/local/bin/my-server --port 9000",
        "outside_workdir": true
      }
    ],
    "env_vars": [
      {
        "name": "ANTHROPIC_API_KEY",
        "value_or_redacted": "[REDACTED:env_key:short:a1b2c3d4e5f6]",
        "sensitive": true
      }
    ],
    "policy": {
      "profile": "safe",
      "extra_deny": [],
      "extra_allow": []
    },
    "findings": [
      {
        "severity": "warning",
        "message": "MCP server 'mcp-0' binary is outside workdir (/workspace): /usr/local/bin/my-server --port 9000"
      }
    ]
  }
}
```

### Field Descriptions

| Field | Type | Description |
|-------|------|-------------|
| `schema_version` | `u32` | Always `1` in this release. |
| `env_type` | `string` | Environment type: `"local"` or `"docker"`. |
| `host_paths` | `string[]` | Host filesystem paths the agent has access to. |
| `network_egress` | `string` | `"unrestricted"` or a comma-separated list of allowed hosts. |
| `hooks.pre_tool_use` | `HookEntry[]` | Hooks registered to fire before every tool call. |
| `hooks.post_tool_use` | `HookEntry[]` | Hooks registered to fire after every tool call. |
| `mcp_servers` | `McpServerPreview[]` | MCP stdio servers registered for this run. |
| `env_vars` | `EnvVarPreview[]` | Sensitive environment variables detected and (by default) redacted. |
| `policy.profile` | `string` | Policy profile: `"safe"`, `"ask"`, or `"yolo"`. |
| `policy.extra_deny` | `string[]` | Extra deny regex patterns beyond the built-in corpus. |
| `policy.extra_allow` | `string[]` | Extra allow regex patterns (escape hatches). |
| `findings` | `PreviewFinding[]` | Risky findings; empty array means a clean preview. |

#### `HookEntry`

| Field | Type | Description |
|-------|------|-------------|
| `name` | `string` | Hook name from config. |
| `command` | `string` | Shell command run for this hook (passed through redactor). |

#### `McpServerPreview`

| Field | Type | Description |
|-------|------|-------------|
| `name` | `string` | Auto-generated name (`mcp-0`, `mcp-1`, …). |
| `command` | `string` | Full command string (passed through redactor). |
| `outside_workdir` | `bool` | `true` when the executable path is outside `environment.workdir`. |

#### `EnvVarPreview`

| Field | Type | Description |
|-------|------|-------------|
| `name` | `string` | Environment variable name. |
| `value_or_redacted` | `string` | Actual value (with `--show-values`) or `[REDACTED:…]` marker. |
| `sensitive` | `bool` | Always `true` (only sensitive vars are included). |

#### `PreviewFinding`

| Field | Type | Description |
|-------|------|-------------|
| `severity` | `string` | Currently always `"warning"`. |
| `message` | `string` | Human-readable finding description. |

---

## Risky Finding Triggers

| Trigger | Condition | Severity |
|---------|-----------|----------|
| Wide host path | `env_type == "local"` and `workdir` is `/` or empty | `warning` |
| MCP outside workdir | MCP server binary path does not start with `workdir` | `warning` |
| Sensitive env var | Any env var matching `*_API_KEY`, `*_TOKEN`, `*_SECRET` is set in the process | `warning` |

---

## Redaction

All string values that could contain secrets are passed through the trajectory
redactor before being stored or printed.  The redactor is built from the
`[redaction]` section of the loaded config; the `secret_literals` list is
especially useful for testing:

```toml
[redaction]
enabled = true
secret_literals = ["sk-deadbeef"]
```

The value `sk-deadbeef` will never appear verbatim in any output produced by
`agent env preview`.

---

## Integration with `bench doctor`

`bench doctor` runs the env preview as a **non-fatal informational section**
before launching preflight checks.  Any risky findings are printed to stdout
but do NOT cause `bench doctor` to exit non-zero; the operator must act on them
before running a real sweep.

---

## Shell Example

The gate below uses `--env docker` and a docker-capable config. `--env local`
always exits 13 (full host-filesystem access is always a risky finding), so it
cannot be used as a clean gate. The binary must be built with `--features docker`
for docker previews to be fully evaluated.

```bash
# Gate CI on a clean env preview before launching a sweep:
max agent env preview --env docker --task "Fix the bug" --config config.toml
preview_exit=$?

if [ $preview_exit -eq 13 ]; then
  echo "WARNING: risky env findings detected — review output above before proceeding"
  exit 1
fi
if [ $preview_exit -ne 0 ]; then
  echo "ERROR: env preview failed (exit $preview_exit)"
  exit 1
fi

# Preview is clean; launch the sweep.
max bench swebench --env docker --dataset lite --output runs/ --config config.toml
```
