# `agent skills-preview` — Static Skill-Activation Preview

Zero-cost, pre-run enumeration of which skill manifests will activate for one
or more tasks. Wraps the existing `resolve_for_task` entry point and adds
byte-cost accounting, cap-hit detection, and redaction-safe output — with no
model API calls and no shell invocations.

## Motivation

`[skills] enabled = true` silently injects skill bodies into the model prompt
for every task in a sweep. Today the only way to audit which skills fired,
their content hash, and their byte contribution is to launch a paid sweep and
read a finished trajectory. `agent skills-preview` surfaces this information
deterministically from the on-disk manifests and task string alone, at zero
cost.

## Usage

```
max agent skills-preview --task <TASK> [--task-file <FILE>] [--config <CONFIG>] [--format text|json]
```

At least one of `--task` or `--task-file` is required.

### Flags

| Flag | Description |
|------|-------------|
| `--task <TASK>` | Task string to preview. Repeatable. |
| `--task-file <FILE>` | Read one task per line; `#`-prefixed lines ignored. |
| `--config <CONFIG>` | Path to a TOML config overlay. Skill paths are read from `[skills]`. |
| `--format text\|json` | Output format. Default: `text`. |

## Human-readable output (text)

For each task, in order:

1. **task_hash** — SHA-256 of the task string (12-char hex prefix)
2. Per activated skill: `name | reason | sha256[:12] | bytes | path`
3. **total_bytes_injected** — sum of raw SKILL.md file sizes for the task (includes YAML frontmatter; a conservative upper bound on injected bytes)
4. **max_active_cap_hit: true/false** — with count of manifests dropped due to `max_active`
5. **merged_extra_context_bytes** — same as `total_bytes_injected` (raw manifest bytes; excludes any render-context wrapper added at sweep time)

A summary section follows listing task count, unique skills activated, p50/p95 bytes/task, and tasks hitting the cap.

### Example

```
=== agent skills-preview (no model call made) ===

task_hash: 3a4f2c1b8e9d
  rust-router | explicit | 7b3c9f2a1d4e | 812 bytes | /skills/rust-router/SKILL.md
  total_bytes_injected: 812
  max_active_cap_hit: false (dropped: 0)
  merged_extra_context_bytes: 812

--- summary ---
task_count: 1
unique_skills_activated: 1
p50_bytes_per_task: 812
p95_bytes_per_task: 812
tasks_hitting_max_active: 0
```

## JSON output (`--format json`)

Schema-versioned `skills_preview` artifact emitted on stdout. One record per
task plus a top-level `summary`.

```json
{
  "artifact_kind": "skills_preview",
  "schema_version": { "major": 1, "minor": 9 },
  "tasks": [
    {
      "task_hash": "3a4f2c1b8e9d",
      "active_skills": [
        {
          "name": "rust-router",
          "reason": "explicit_mention",
          "sha256_prefix": "7b3c9f2a1d4e",
          "bytes": 812,
          "path": "/skills/rust-router/SKILL.md",
          "version": "1.0.0"
        }
      ],
      "total_bytes_injected": 812,
      "max_active_cap_hit": false,
      "dropped_count": 0,
      "merged_extra_context_bytes": 812
    }
  ],
  "summary": {
    "task_count": 1,
    "unique_skills_activated": 1,
    "p50_bytes_per_task": 812,
    "p95_bytes_per_task": 812,
    "tasks_hitting_max_active": 0
  }
}
```

### Schema compatibility

The `skills_preview` artifact follows the same additive-only minor-version
contract as all other harness artifacts. Consumers must accept unknown additive
fields in supported-current major. A backwards-compatibility unit test
(`agent_skills_preview_json_additive_only_schema_compat`) asserts all required
fields are present.

## Activation reasons

| Reason | `reason` field | Trigger |
|--------|---------------|---------|
| `ExplicitMention` | `explicit_mention` | Task contains `$skill-name`, `@skill-name`, or `/skill-name` with word boundaries |
| `AutoMatch` | `auto_match` | Token overlap score ≥ 2 between task and skill name/description (requires `auto_load = true`) |

### Worked example

Given two skills:

```
/skills/rust-router/SKILL.md   (name: rust-router, description: "Use for Rust questions")
/skills/security/SKILL.md      (name: security-review, description: "Use for security review")
```

Task: `"Use $rust-router to fix this borrow checker issue"`

- **rust-router** → `ExplicitMention` (explicit `$rust-router` with word boundaries)
- **security-review** → not activated (no explicit mention; no token overlap ≥ 2)

Task: `"please perform a security review of the authentication module"`

- **rust-router** → not activated
- **security-review** → `AutoMatch` (tokens: `security`, `review` both present)

## Exit codes

| Code | Class | Condition |
|------|-------|-----------|
| `0` | `success` | Clean preview with no warnings |
| `2` | `usage_error` | Missing `--task` / `--task-file`, or invalid `--format` |
| `14` | `skills_preview_warning` | At least one of: task hit `max_active`, an activated manifest has no `version` field, or `auto_load = true` resolved a skill via `auto_match` |

## `bench doctor` integration

`bench doctor` invokes `agent skills-preview` as a non-fatal informational
section when `[skills] enabled = true`. The section is printed to stdout after
the preflight results table, mirroring the env-preview wiring in `#313`.

## Out of scope

- Editing, generating, or recommending skill manifests
- Co-activation analytics across a corpus (implemented in [bench skill-coverage](spec-skill-coverage.md))
- Live-reload / file watching (preview is a one-shot snapshot)
- Auto-injecting skills into a sweep based on preview output
- TUI / web UI — CLI + JSON only
