# Spec: `agent config resolve` (issue #500)

## Purpose

`agent config resolve` prints the **fully-resolved effective run
configuration** that a given invocation will actually use, annotated with
the precedence layer that set each field.  It is a zero-cost, zero-network
preflight that catches silent overrides — especially the documented
clap-default caveat — before spending money or trusting results.

---

## Invocation

```
max agent config resolve \
  [--config <PATH>]           \
  [--model <NAME>]            \
  [--step-limit <N>]          \
  [--observation-max-bytes N] \
  [--observation-head-ratio F]\
  [--per-task-budget-usd F]   \
  [--format text|json]
```

All flags are optional.  The command performs **no model call** and
**no network I/O** — it reads only local TOML files and compiled-in
defaults.

---

## Configuration Precedence

Resolution order, lowest to highest:

| Layer | Token      | Notes |
|-------|------------|-------|
| 1     | `default`  | Compiled-in defaults from `src/config/defaults/default.toml` |
| 2     | `file`     | Loaded via `--config`; `extends:` chains fully resolved |
| 3     | `env`      | Reserved for future env-var overrides; not yet used for scalar fields |
| 4     | `flag`     | Explicit CLI flags on this invocation |

Each resolved field in the output is annotated with the layer token that
set its winning value.

---

## Output Schema

### Text format (`--format text`, default)

```
=== agent config resolve (no model call made) ===

Field                                      Value                            Layer
----------------------------------------------------------------------------------
model.name                                 claude-opus-4-7                  default
model.max_tokens                           4096                             default
...

--- Hazards: NONE ---
```

When hazards are detected:

```
--- Hazards ---
[CLAP_DEFAULT_OVERRIDE] model.name
  Config file sets model.name='claude-sonnet-4-6' but 'mini' and
  'bench swebench' unconditionally apply the clap default 'claude-opus-4-7'
  when --model is not explicitly passed; your config-file value is silently
  ignored.
  Affected commands: mini, bench swebench
```

### JSON format (`--format json`)

Keys are emitted in a stable, deterministic order so the output can be
committed and diffed in CI:

```json
{
  "config_resolve": {
    "schema_version": 1,
    "fields": [
      {"key": "model.name",    "value": "claude-opus-4-7", "layer": "default"},
      {"key": "model.max_tokens", "value": 4096,           "layer": "default"},
      ...
    ],
    "hazards": [],
    "has_hazards": false
  }
}
```

#### `fields[]` object

| Key     | Type            | Description |
|---------|-----------------|-------------|
| `key`   | string          | Dot-separated config path, e.g. `model.name` |
| `value` | any JSON scalar | The winning value after all layers are applied |
| `layer` | string enum     | `"default"`, `"file"`, `"env"`, or `"flag"` |

#### `hazards[]` object

| Key                  | Type          | Description |
|----------------------|---------------|-------------|
| `field`              | string        | Config path affected by the hazard |
| `file_value`         | any           | The value your config file set |
| `clap_default_value` | any           | The value a clap default will use instead |
| `commands_affected`  | string array  | Which subcommands exhibit the override |
| `message`            | string        | Human-readable description |

---

## Tracked Scalar Fields

The following fields are tracked in the current release:

| Field                          | Notes |
|--------------------------------|-------|
| `model.name`                   | Clap-default hazard for `mini`, `bench swebench` |
| `model.max_tokens`             | |
| `model.temperature`            | |
| `agent.step_limit`             | Clap-default hazard for `bench swebench` |
| `agent.per_task_budget_usd`    | |
| `agent.cost_limit_usd`         | |
| `agent.hide_budget_from_agent` | |
| `agent.observation_max_bytes`  | |
| `agent.observation_head_ratio` | |
| `agent.detect_stagnation`      | |
| `agent.stagnation_repeat_threshold` | |
| `agent.stagnation_window`      | |
| `environment.kind`             | |
| `environment.timeout_secs`     | |
| `environment.workdir`          | |

---

## Clap-Default Override Hazards

Two fields carry a clap default that is unconditionally written back over
any config-file value in certain subcommands:

### `model.name` — affects `mini` and `bench swebench`

Both commands declare `--model` with `default_value = "claude-opus-4-7"`.
Clap sets `cfg.root.model.name` to this default even when the user does not
pass `--model` explicitly, overwriting any value loaded from `--config`.

**Trigger**: config file sets `model.name` to any value other than
`"claude-opus-4-7"` AND `--model` is not passed to `agent config resolve`.

### `agent.step_limit` — affects `bench swebench` only

`bench swebench` declares `--step-limit` with `default_value_t = 50`.

**Trigger**: config file sets `agent.step_limit` to any value other than
`50` AND `--step-limit` is not passed to `agent config resolve`.

---

## Exit-Code Contract

| Exit code | Outcome class             | Condition |
|-----------|---------------------------|-----------|
| 0         | `success`                 | Resolved config printed; no hazards detected |
| 2         | `usage_error`             | Bad flags or invalid config file |
| 42        | `config_override_warning` | At least one clap-default hazard detected |

Exit 42 is emitted **after** the full resolved output is printed, so the
operator always gets the annotated config even when the gate fires.

---

## Secret Redaction

String values in resolved fields are run through the configured
`[redaction]` policy before printing — the same redactor that protects
trajectory output.  This prevents `secret_literals` configured in overlay
files from appearing verbatim in `config resolve` output.

---

## Out of Scope

- Editing, writing, or migrating config files (read-only).
- Semantic validation beyond precedence resolution (e.g. "is this model
  name real?") — that belongs to `bench doctor` / preflight.
- The filesystem / hooks / MCP-server / policy surface already covered by
  `agent env preview`.
- Diffing two configs against each other (covered by `bench diff-config`).
