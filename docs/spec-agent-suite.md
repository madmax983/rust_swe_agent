# Spec: `agent suite` — Personal Eval Task Pack Runner

## Overview

`agent suite` lets operators run a small, operator-defined pack of micro-tasks
through the same `mini` execution path and collect per-task trajectories and an
aggregated `suite-results.json` artifact in seconds.

It fills the gap between `--render-only` (zero-cost, prompt preview only) and
`bench swebench` (full paid sweep): "does my prompt still solve my own five
common workflows on a cheap model?"

---

## Quick Start

```sh
# YAML pack
max agent suite --tasks-file my-tasks.yaml --model claude-haiku-4-5-20251001

# JSONL pack with suite-level verify check
max agent suite \
    --tasks-file tasks.jsonl \
    --verify "lint:cargo clippy --quiet" \
    --suite-cost-limit-usd 0.50

# TOML pack, resume after interruption
max agent suite --tasks-file pack.toml --resume
```

---

## Task File Schema

Format is auto-detected by file extension. Use `--format yaml|jsonl|toml` to
override.

### YAML (`.yaml`, `.yml`)

```yaml
- id: fix-null-deref
  task: Fix the null dereference in src/handler.rs line 42
  extra_context: "The function is called from the HTTP request path"
  verify:
    - tests:cargo test handler

- id: add-config-docs
  task: Add rustdoc comments to all public types in src/config.rs
```

### JSONL (`.jsonl`, `.ndjson`) — one object per line

```jsonl
{"id": "fix-null-deref", "task": "Fix the null dereference in src/handler.rs"}
{"id": "add-config-docs", "task": "Add rustdoc to src/config.rs", "verify": ["lint:cargo clippy"]}
```

### TOML (`.toml`) — `[[tasks]]` array of tables

```toml
[[tasks]]
id = "fix-null-deref"
task = "Fix the null dereference in src/handler.rs"

[[tasks]]
id = "add-config-docs"
task = "Add rustdoc to src/config.rs"
extra_context = "Focus on public API surface"
verify = ["lint:cargo clippy --quiet"]
```

### Per-task fields

| Field           | Type            | Required | Description                                        |
|-----------------|-----------------|----------|----------------------------------------------------|
| `id`            | `string`        | ✓        | Unique task identifier (used for file naming)      |
| `task`          | `string`        | ✓        | Natural-language task description for the agent    |
| `extra_context` | `string`        | —        | Appended to the task prompt as additional context  |
| `verify`        | `string[]`      | —        | Per-task verify checks in `NAME:COMMAND` format    |

---

## Flag Reference

| Flag                      | Default              | Description                                        |
|---------------------------|----------------------|----------------------------------------------------|
| `--tasks-file PATH`       | (required)           | Task pack file                                     |
| `--format yaml\|jsonl\|toml` | (auto-detect)     | Override format detection                          |
| `--suite-name NAME`       | tasks-file stem      | Suite name; used as output subdirectory            |
| `--model NAME`            | `claude-opus-4-7`    | Model for all tasks                                |
| `--env local\|docker`     | (config default)     | Environment type                                   |
| `--docker-image IMAGE`    | (config default)     | Docker image when `--env docker`                   |
| `--output DIR`            | `./runs`             | Root output directory                              |
| `--step-limit N`          | (config default)     | Max agent steps per task                           |
| `--task-timeout-secs N`   | (unset)              | Wallclock timeout per task                         |
| `--per-task-budget-usd N` | (unset)              | USD ceiling per task                               |
| `--suite-cost-limit-usd N`| (unset)              | Suite-level USD ceiling                            |
| `--verify NAME:COMMAND`   | (none)               | Suite-level verify check (repeatable)              |
| `--verify-timeout-secs N` | 60                   | Per-check timeout                                  |
| `--mcp-server COMMAND`    | (none)               | Register MCP stdio server (repeatable)             |
| `--detect-stagnation`     | (config default)     | Enable/disable stagnation detection                |
| `--history-max-input-tokens N` | (unset)         | Token budget for model-visible prompt              |
| `--history-keep-last-observations N` | (unset)  | Keep only last N observations                      |
| `--config PATH`           | (none)               | TOML config overlay                                |
| `--resume`                | false                | Skip tasks with existing terminal trajectories     |
| `--check`                 | false                | Preflight-only: validate the pack and exit with **zero model calls** (see below) |
| `--check-format text\|json` | `text`             | Output format for `--check`'s report (requires `--check`) |
| `--strict`                | false                | With `--check`, escalate config hazards from warnings to fatal (requires `--check`) |

---

## Output Artifacts

All artifacts land under `<output>/<suite-name>/`.

### Per-task trajectory

`<output>/<suite-name>/<task-id>.traj.json` — the standard canonical
trajectory written by `mini`. No new trajectory shape; existing tools
(`bench inspect`, `bench grep`, redactor) work on it for free.

### `suite-results.json`

Schema-versioned aggregate artifact. Top-level structure:

```json
{
  "artifact_kind": "suite_results",
  "schema_version": {"major": 1, "minor": 10},
  "suite_name": "my-suite",
  "task_count": 5,
  "resolved_count": 3,
  "verified_count": 2,
  "total_cost_usd": 0.18,
  "total_duration_secs": 47.2,
  "started_at": "2026-05-29T10:00:00Z",
  "finished_at": "2026-05-29T10:00:47Z",
  "tasks": [...]
}
```

Per-task entry with **loop-behaviour fields**:

```json
{
  "id": "fix-null-deref",
  "outcome": "submitted",
  "verification_status": "verified",
  "steps": 12,
  "cost_usd": 0.03,
  "duration_secs": 8.5,
  "failure_category": null,
  "trajectory_path": "runs/my-suite/fix-null-deref.traj.json",
  "attempt_count": 12,
  "unchanged_failure_count": 0,
  "verifier_delta": 1,
  "stop_reason": "submitted"
}
```

#### Loop-behaviour fields (issue #322 feedback)

| Field                    | Description                                                                      |
|--------------------------|----------------------------------------------------------------------------------|
| `attempt_count`          | Total agent steps executed (= `trajectory.info.steps`)                           |
| `unchanged_failure_count`| Consecutive tail failures with the same exit code after the last passing test    |
| `verifier_delta`         | (checks passed) − (checks failed); `null` when no verify checks configured       |
| `stop_reason`            | Why the loop stopped (`submitted`, `step_limit`, `agent_stagnation`, etc.)       |

The regression you really want to catch is **same task, same miss, more spend**:
`unchanged_failure_count > 0` with `attempt_count` growing across suite runs
signals wasted budget with no progress.

---

## Exit-Code Matrix

| Code | Class                | When                                              |
|------|----------------------|---------------------------------------------------|
| 0    | `success`            | All tasks submitted **and** all verify checks pass|
| 4    | `task_unsuccessful`  | At least one task did not submit                  |
| 5    | `budget_halt`        | `--suite-cost-limit-usd` was hit                  |
| 7    | `verification_failure` | At least one verify check failed               |

When multiple failure types occur, the **highest-severity code wins**:
`verification_failure` (7) > `budget_halt` (5) > `task_unsuccessful` (4).

---

## `--suite-cost-limit-usd`

When the running cumulative cost reaches the limit, remaining tasks are
skipped and recorded with:

```json
{
  "outcome": "skipped_budget_exhausted",
  "stop_reason": "suite_budget_exhausted"
}
```

The suite exits with code 5 (`budget_halt`). Already-started tasks are
allowed to complete; only tasks not yet dequeued are skipped.

---

## Resume

`--resume` skips any task whose `<task-id>.traj.json` already exists with a
terminal (non-partial) outcome. Only missing or non-terminal tasks are re-run.

```sh
# First run (interrupted at task 3/5)
max agent suite --tasks-file tasks.yaml --suite-cost-limit-usd 0.10

# Second run continues from task 4
max agent suite --tasks-file tasks.yaml --suite-cost-limit-usd 0.10 --resume
```

---

## Redaction

The existing `Redactor` is applied to both per-task trajectories (via `mini`)
and `suite-results.json`. No new redaction-bypass surface.

---

## Stdout Table

```
ID                             OUTCOME                VERIFY        COST($)   STEPS
------------------------------------------------------------------------------------
fix-null-deref                 submitted              verified        0.0300      12
add-config-docs                step_limit_reached     unverified      0.0250      50
------------------------------------------------------------------------------------
1/2 resolved, 1/2 verified, $0.0550, 42.3s elapsed
```

---

## Worked Example

```sh
# 1. Create a 3-task YAML pack
cat > my-regression-pack.yaml <<'EOF'
- id: null-deref
  task: Fix the null dereference in src/handler.rs line 42
  verify:
    - tests:cargo test handler

- id: config-docs
  task: Add rustdoc comments to all public types in src/config.rs

- id: error-messages
  task: Improve the error message in src/error.rs to include the file path
  extra_context: "User reported the current message is 'file not found' with no path"
EOF

# 2. Run against a cheap model with a $1 ceiling
max agent suite \
    --tasks-file my-regression-pack.yaml \
    --model claude-haiku-4-5-20251001 \
    --suite-cost-limit-usd 1.00 \
    --output ./regression-runs

# 3. Inspect results
cat regression-runs/my-regression-pack/suite-results.json | jq '.tasks[] | {id, outcome, attempt_count, unchanged_failure_count, verifier_delta}'
```

---

## `agent suite --check` — Zero-Spend Preflight (issue #821)

The dataset path has `bench swebench --dry-run` and `bench scriptability-check`
to validate inputs before a paid sweep. `agent suite --check` is the
equivalent for the operator personal-eval path: it validates a task pack —
parsing, task fields, verify-check launchability, MCP/hook startup, and
config-provenance hazards — **without making any model call or starting an
agent loop**, so a typo'd verify command or missing test binary is caught
before dozens of paid trajectories are burned.

```sh
max agent suite --tasks-file my-tasks.yaml --check
max agent suite --tasks-file my-tasks.yaml --check --check-format json
max agent suite --tasks-file my-tasks.yaml --check --strict   # hazards become fatal
```

### What it validates

| # | Check                | Fatal?                  | Notes                                                     |
|---|-----------------------|--------------------------|------------------------------------------------------------|
| a | Pack parses for the detected/declared format | yes | `format_detect`, `pack_readable`, `pack_parse` |
| b | Every task has non-empty `id` and `task`     | yes | `task_fields_and_ids` |
| c | Task ids are unique                          | yes | `task_fields_and_ids` (reuses the same validation `agent suite` runs) |
| d | Every `--verify`/per-task `verify` is well-formed `NAME:COMMAND` and its command is **statically launchable** | yes | `verify:<name>` — resolves the first program token against `PATH` / shell builtins; never executes the command |
| e | Configured MCP servers and hooks start       | yes | `mcp_server`, `hook:<phase>` — reuses the `bench scriptability-check` probe (the same transient, non-model subprocess spawn) |
| — | Config-provenance hazards (e.g. a silently overridden `model.name`) | only with `--strict` | `hazard:<field>` — reuses the `agent config resolve` hazard detector |
| — | Worst-case cost vs `--suite-cost-limit-usd`  | never (informational)   | `cost_ceiling` — `--per-task-budget-usd × task_count`, no model call |

Launchability is a *static* check only: the out-of-scope verify-execution
semantics from issue #821 mean `--check` confirms a command's program token
resolves to something runnable, not that the command would actually pass.
Complex shell constructs (subshells, command substitution) are not specially
parsed — keep verify commands to a simple `program args...` or
`VAR=val program args...` shape.

### Output

On success: exit `0` and a one-line summary —

```
agent suite --check [PASS]  tasks-file=my-tasks.yaml  (4 ms)
5 task(s), 6 verify check(s), 0 MCP server(s), 0 hook(s), worst-case $0.5000 (limit $1.0000)
```

On any fatal check failing: exit `3` (`preflight_failure`) and every failure
is listed with its check id and offending task id / target:

```
agent suite --check [FAIL]  tasks-file=my-tasks.yaml  (6 ms)
5 task(s), 6 verify check(s), 0 MCP server(s), 0 hook(s), worst-case cost unknown (no --per-task-budget-usd set)

Failures:
  [FAIL] task_fields_and_ids       id=task-47               task id 'task-47' must not contain path separators or '..'
  [FAIL] verify:tests              id=task-12               'pytset' was not found on PATH
```

`--check-format json` emits a schema-versioned report (`schema_version: 1`)
listing every check performed — including passes — each with
`status: "pass" | "fail" | "warn"`, suitable for CI parsing.

### Guarantees

- **Zero spend**: no model call, no agent loop. Asserted in tests by running
  with no credential env vars set.
- **Read-only**: `--check` takes no `--output` directory and never writes
  `suite-results.json` or per-task artifacts — only the transient MCP-server
  and hook probe subprocesses are spawned (same as `scriptability-check`).
- **< 1s** for a 50-task pack with no MCP servers configured.

---

## Relationship to Other Commands

- **`mini`** — runs a single task; `agent suite` batches N tasks using the same path.
- **`bench swebench`** — full SWE-bench sweep with parallelism, evaluation, and retries.
  `agent suite` is the lightweight personal-pack middle ground.
- **`bench grep` / `bench inspect`** — work on per-task trajectories written by `agent suite`
  without modification (same canonical schema).
- **`bench ladder`** — can ingest `suite-results.json` once the ladder spec supports it
  (the artifact structure is intentionally compatible).
