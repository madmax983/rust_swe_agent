# Operator Configuration Reference

This document is the single configuration reference for `rust_swe_agent`.
It covers every supported top-level section and field, explains configuration
precedence, shows copy-pasteable TOML examples, and describes safe secret
handling.

**Format:** All run configuration files are **TOML** (`.toml`). YAML is not
supported. If you have YAML examples from earlier experiments or community
posts, translate them to TOML before use (migration note: rename `:`
key/value separators to `=` and replace YAML indented blocks with TOML
`[[array-of-tables]]` syntax).

---

## Configuration Precedence

Resolution order, lowest to highest:

| Layer | Source | Notes |
|---|---|---|
| 1 (lowest) | Built-in defaults | Compiled into the binary from `src/config/defaults/default.toml` |
| 2 | Config file | Loaded via `--config <path>`; overlaid on defaults via deep merge |
| 3 | Environment variables | Provider credentials and a small set of harness overrides |
| 4 (highest) | CLI flags | Always win over every other layer |

Deep merge semantics: object keys recurse, leaf values prefer the higher
layer, arrays are replaced wholesale (not appended).

**Example 1 — per-task budget:** The built-in default is
`per_task_budget_usd = null` (no cap). A config file that sets
`per_task_budget_usd = 0.50` applies a $0.50 ceiling per task for every run
using that file. Passing `--per-task-budget-usd 0.10` on the CLI tightens the
cap to $0.10 for that single invocation only.

**Example 2 — observation truncation:** Built-in default is
`observation_max_bytes = 16384`. A config file that sets
`observation_max_bytes = 8192` halves the truncation window for all runs
using that file. Passing `--observation-max-bytes 4096` on the CLI overrides
the file value for that invocation only.

**Example 3 — model name (clap-default caveat):** For `mini` and
`bench swebench`, `--model` has a clap default of `claude-opus-4-7` that is
written back over the config unconditionally. Setting `model.name` in a
config file has **no effect** for those commands unless you also pass `--model`
explicitly. To use a model from config, always supply the flag:
`mini --config cfg.toml --model claude-sonnet-4-6`. The same caveat applies
to `agent.step_limit` (clap default: `50`) for those two commands.
Fields without a clap default — such as `per_task_budget_usd`,
`observation_max_bytes`, and template fields — layer correctly from config.

---

## Secret Handling

Credentials must come from **environment variables** or a local secret store.
Raw secrets must **never** appear in committed config files or TOML examples.

| Provider | Environment variable |
|---|---|
| Anthropic (Claude) | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |

- Do **not** put token values in `[redaction].secret_literals` in committed
  configs; use that field only in local, git-ignored overlay files.
- `unsafe_allow_secret_leaks = false` (the default) ensures submitted patches
  containing configured secret shapes are downgraded to a failure rather than
  leaked. Do **not** set this to `true` in shared configs.
- For the full redaction contract, cross-reference
  [`docs/spec-secret-redaction.md`](spec-secret-redaction.md).

---

## Top-Level Sections

```toml
[agent]        # Agent loop behaviour: step limit, budget, templates, hooks
[model]        # Model selection and parameters
[environment]  # Execution environment: local shell or Docker
[prompts]      # System and instance prompt templates
[sweep]        # Rate-limiting for parallel sweeps
[redaction]    # Secret redaction policy
[policy]       # Pre-execution command policy (safe / ask / yolo)
```

An optional `extends = "<path>"` top-level key (not a section) resolves a
parent config first, then overlays the current file. Maximum include depth: 16.

---

## `[agent]`

Controls the inner agent loop.

| Field | Type | Default | Valid values | Notes |
|---|---|---|---|---|
| `kind` | string | `"default"` | `"default"`, `"interactive"` | **Not yet dispatched** — the `interactive` variant is defined in the schema but `mini`, `bench swebench`, and `hello-world` all construct `DefaultAgent` unconditionally. Setting `kind = "interactive"` has no effect in those commands today. |
| `step_limit` | integer | `50` | `1`–`∞` | Hard cap on conversation turns; prevents runaway loops |
| `per_task_budget_usd` | float \| null | `null` | Any positive float | Per-task spend ceiling; loop terminates with `budget_exhausted` when reached |
| `cost_limit_usd` | float \| null | `null` | Any positive float | Per-task spend ceiling inside the agent loop; terminates the current task with `cost_limit` failure when reached. Does **not** cap the whole sweep — for a sweep-wide aggregate cap use `--sweep-cost-limit-usd` (CLI only, no config equivalent) |
| `hide_budget_from_agent` | bool | `false` | `true`, `false` | When `true`, the budget block is not appended to observations (A/B flag) |
| `budget_block_template` | string | see defaults | Handlebars template | Variables: `budget_used`, `budget_limit`, `budget_remaining_pct`, `turn`, `max_turns` |
| `format_error_template` | string | see defaults | Any string | Rendered when the model response contains no valid bash action |
| `observation_template` | string | see defaults | Jinja2/Handlebars template | Variables: `returncode`, `output`, `tool_use_blocked`, `pre_tool_use_hooks`, `post_tool_use_hooks` |
| `observation_max_bytes` | integer | `16384` | `1`–`∞` | Truncation limit for observations before they reach the model |
| `observation_head_ratio` | float | `0.5` | `0.0`–`1.0` | Head/tail split ratio when truncating; `0.5` = equal halves |
| `tool_hook_timeout_secs` | integer | `10` | `1`–`∞` | Default timeout for pre/post tool hooks unless a hook overrides it |
| `test_command_patterns` | array of strings | `[]` | Valid regexes | Extends the built-in test-command corpus; see `spec-test-telemetry.md` |
| `test_command_patterns_replace` | bool | `false` | `true`, `false` | When `true`, replaces rather than extends the built-in corpus |

### `[[agent.hooks.pre_tool_use]]` / `[[agent.hooks.post_tool_use]]`

Optional inline hooks that run before or after each bash tool invocation.

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Label shown in hook output |
| `command` | string | yes | Shell command to execute |
| `timeout_secs` | integer | no | Overrides `tool_hook_timeout_secs` for this hook |

A non-zero PreToolUse exit code blocks the command and reports the result to
the model. PostToolUse hooks are diagnostic only.

---

## `[model]`

Selects and configures the language model.

| Field | Type | Default | Valid values | Notes |
|---|---|---|---|---|
| `name` | string | `"claude-opus-4-7"` | Any LiteLLM-routable model name | Prefix determines backend: `claude*` → Anthropic, `openai/*` → OpenAI, `openrouter/*` → OpenRouter |
| `temperature` | float \| null | `null` | `0.0`–`2.0` | `null` uses the backend default |
| `max_tokens` | integer | `4096` | `1`–provider limit | Maximum tokens in the model response |
| `fallback_models` | array of strings | `[]` | Any model names | Tried in order on transient provider failures; empty = no fallback |

---

## `[environment]`

Controls where agent commands run.

| Field | Type | Default | Valid values | Notes |
|---|---|---|---|---|
| `kind` | string | `"local"` | `"local"`, `"docker"` | `"docker"` requires the `docker` Cargo feature and a running Docker daemon |
| `timeout_secs` | integer | `60` | `1`–`∞` | Per-**command** wall-clock timeout — applied to each individual shell command. This is **not** the same as `--task-timeout-secs`, which is a whole-task wallclock budget passed via CLI only (no config equivalent). Setting `timeout_secs` only caps individual commands; a multi-step task can still run much longer. |
| `docker_image` | string \| null | `null` | Any Docker image reference | Required when `kind = "docker"` |
| `workdir` | string | `"/workspace"` | Any absolute path | Working directory inside the environment |

---

## `[prompts]`

Handlebars/Jinja2 templates for the agent conversation.

| Field | Type | Default | Notes |
|---|---|---|---|
| `system` | string | see defaults | Sent as the system message; sets agent persona and output format |
| `instance` | string | see defaults | Rendered per-task; variables: `task`, `extra_context` |

---

## `[sweep]`

Adaptive rate-limiting for parallel sweeps (issue #44). Both fields are
opt-in: when unset, no ceiling is applied.

| Field | Type | Default | Valid values | Notes |
|---|---|---|---|---|
| `max_rpm` | integer \| null | `null` | Any positive integer | Cap aggregate provider requests/minute across all workers; maps to `--max-rpm` |
| `max_input_tpm` | integer \| null | `null` | Any positive integer | Cap aggregate input tokens/minute; maps to `--max-input-tpm` |

CLI values always win over config file values for both fields.

---

## `[redaction]`

Controls secret redaction across trajectories, observations, streams,
inspect/export output, and submission artifacts.

| Field | Type | Default | Valid values | Notes |
|---|---|---|---|---|
| `enabled` | bool | `true` | `true`, `false` | When `false`, all redaction is disabled for this run |
| `secret_literals` | array of strings | `[]` | Any strings | Literal values redacted at runtime; these values are themselves redacted from exported config manifests |
| `custom_patterns` | array of strings | `[]` | Valid regex patterns | Full-match patterns redacted at runtime; validated at config load |
| `unsafe_allow_secret_leaks` | bool | `false` | `true`, `false` | When `false` (recommended), submitted patches containing secret shapes are downgraded to `secret_leak_detected` failure |

See [`docs/spec-secret-redaction.md`](spec-secret-redaction.md) for the full
redaction contract.

---

## `[policy]`

Pre-execution command guardrail (issue #90). This is a safety layer, not a
sandbox replacement.

| Field | Type | Default | Valid values | Notes |
|---|---|---|---|---|
| `profile` | string | `"safe"` | `"safe"`, `"ask"`, `"yolo"` | `"safe"` blocks built-in dangerous commands; `"ask"` prompts for human approval (fails closed in CI); `"yolo"` disables all blocking |
| `extra_deny_patterns` | array of strings | `[]` | Valid regexes | Appended after the built-in corpus; always denied regardless of profile |
| `extra_allow_patterns` | array of strings | `[]` | Valid regexes | Always allowed even when the built-in corpus would deny them |

---

## Copy-Pasteable TOML Examples

### Deterministic no-key smoke run

Runs the built-in scripted model — no API credentials required, costs $0.

<!-- config-example:no-key-smoke -->
```toml
# smoke.toml — deterministic no-key run; safe to commit
[agent]
kind = "default"
step_limit = 5

[model]
name = "claude-opus-4-7"
max_tokens = 1024

[environment]
kind = "local"
timeout_secs = 30
workdir = "/workspace"

[redaction]
enabled = true
unsafe_allow_secret_leaks = false
```

Run with:

```bash
cargo run --quiet -- --log error hello-world --output runs/smoke
```

### Local live-model run with budget and timeout guards

Requires `ANTHROPIC_API_KEY` (or the relevant provider key). Sets a per-task
spend cap and a short step limit to bound cost during initial exploration.

<!-- config-example:live-model -->
```toml
# live.toml — local live-model run; do NOT commit raw API keys
[agent]
kind = "default"
step_limit = 10
per_task_budget_usd = 0.25

[model]
name = "claude-opus-4-7"
max_tokens = 4096
temperature = 0.0

[environment]
kind = "local"
timeout_secs = 120
workdir = "/workspace"

[redaction]
enabled = true
unsafe_allow_secret_leaks = false
```

Run with:

```bash
export ANTHROPIC_API_KEY="<your key>"  # never commit this value
cargo run --quiet -- --log info mini \
  --config live.toml \
  --task "Write a hello-world Python script." \
  --output runs/live
```

### Docker SWE-bench sweep with rate limiting and cost cap

Uses Docker isolation and configures sweep-level rate limits. Pair this with
`--resume` (CLI flag) and `bench forecast` to cap total spend before a full
sweep.

<!-- config-example:docker-sweep -->
```toml
# sweep.toml — Docker SWE-bench sweep
[agent]
kind = "default"
step_limit = 50
per_task_budget_usd = 2.00

[model]
name = "claude-opus-4-7"
max_tokens = 4096
fallback_models = ["claude-sonnet-4-6"]

[environment]
kind = "docker"
docker_image = "swebench/sweb.eval.x86_64.django__django:latest"
timeout_secs = 120
workdir = "/workspace"

[sweep]
max_rpm = 500
max_input_tpm = 100000

[redaction]
enabled = true
unsafe_allow_secret_leaks = false
```

Run with:

```bash
export ANTHROPIC_API_KEY="<your key>"
cargo run --quiet -- --log info bench swebench \
  --config sweep.toml \
  --dataset-path ./data/swebench.jsonl \
  --output runs/sweep \
  --limit 10
```

### Interactive local confirmation

Prompts the operator for approval before each bash command. In non-interactive
contexts (CI, sweeps), `"interactive"` kind fails closed (denies all).

<!-- config-example:interactive-local -->
```toml
# interactive.toml — human approval for each command
[agent]
kind = "interactive"
step_limit = 20
per_task_budget_usd = 1.00

[model]
name = "claude-opus-4-7"
max_tokens = 4096

[environment]
kind = "local"
timeout_secs = 300
workdir = "/workspace"

[policy]
profile = "ask"

[redaction]
enabled = true
unsafe_allow_secret_leaks = false
```

---

## Docs Drift Verification

`tests/config_reference.rs` enforces that this document stays in sync with
the implementation:

- Every non-comment field in `src/config/defaults/default.toml` must appear
  verbatim in this document.
- All four TOML examples above must parse without error via `Config::from_toml_str`.
- `README.md` must link to this file.
- Config validation errors must reference `config-reference` so operators can
  self-serve.

When you add or rename a config field, update this document and the examples
before the test suite will pass.
