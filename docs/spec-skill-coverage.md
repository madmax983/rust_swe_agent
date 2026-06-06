# `bench skill-coverage` Spec

## User Story

As an operator evaluating agent skills across SWE-bench sweeps, I want a per-sweep summary of how often each configured skill activated, why they activated (explicit mention vs auto match), and whether their activation correlated with successful resolution, so that I can see whether my skill library is effective or contains dead weight.

## CLI

```bash
max bench skill-coverage --sweep <dir> [options]
```

### Required flags

| Flag | Description |
|------|-------------|
| `--sweep <dir>` | Completed sweep directory (must contain `results.json` and instance subdirectories containing `trajectory.json` files) |

### Optional flags

| Flag | Default | Description |
|------|---------|-------------|
| `--format text\|json` | `text` | Output format for stdout |
| `--bucket resolved\|unresolved\|errored\|all` | (all shown) | Restrict text output to one bucket |
| `--filter key=value` | (none) | Same filter syntax as `bench inspect --filter` |
| `--per-instance` | false | Emit per-instance skill activation records in JSON output and artifact |

## Behavior

`bench skill-coverage` is **read-only**: it never runs any models, never executes tasks, and never modifies existing trajectories. It reads `results.json`, optional `evaluation.json`, and all `trajectory.json` files in the sweep directory.

### Skill universe

The report enumerates all skills configured for the sweep. The subcommand parses `[skills]` from the sweep's resolved configuration in `results.json`. If skills are disabled (`enabled = false`), the command exits successfully (`0`) and warns that skills were disabled.

If skill-set configuration drift is detected across instances (i.e. different instances ran with different configured skill paths), the command collects the union of all skills across these configurations and groups the configurations by their sorted list of paths.

### Per-skill aggregation

For each skill in the universe:

| Field | Description |
|-------|-------------|
| `total_activations` | Total number of times this skill was activated across all instances |
| `instances_activated` | Count of distinct instances where the skill was activated at least once |
| `activation_rate` | Fraction of instances where this skill was activated (`instances_activated / total_instances`) |
| `share_of_all_activations` | This skill's share of all activations across all skills |
| `reasons` | Count of activations by reason: `explicit_mention` or `auto_match` |
| `by_outcome` | Per-bucket metrics (see below) |

### Outcome correlation

For each skill, a `by_outcome` block contains metrics per outcome bucket (`resolved`, `unresolved`, `errored`, `all`):

| Field | Description |
|-------|-------------|
| `instances_activated` | Instances in this bucket where the skill was activated |
| `instances_total` | Total instances in this bucket |
| `usage_rate` | `instances_activated / instances_total` |
| `resolved_rate_when_active` | Resolved share among instances that activated the skill (0.0 if never activated) |
| `resolved_rate_when_not_active` | Resolved share among instances that did NOT activate the skill |
| `resolved_rate_delta` | `resolved_rate_when_active − resolved_rate_when_not_active` |

### Skill set drift

If different instances run with different configured skill sets, the command flags this in `skill_set_drift`:

```json
{
  "groups": [
    {
      "fingerprint": "a2b53c...",
      "instance_count": 1,
      "paths": ["/path/to/skills/dir/a"]
    }
  ]
}
```

## JSON Output Schema

```json
{
  "artifact_kind": "skill_coverage",
  "schema_version": {
    "major": 1,
    "minor": 10
  },
  "sweep": "/path/to/sweep",
  "generated_at": "2026-06-06T00:00:00Z",
  "skill_universe": [
    {
      "name": "skill_a",
      "description": "Skill A description"
    }
  ],
  "by_skill": {
    "skill_a": {
      "total_activations": 2,
      "instances_activated": 2,
      "activation_rate": 0.6666666666666666,
      "share_of_all_activations": 0.6666666666666666,
      "reasons": {
        "explicit_mention": 1,
        "auto_match": 1
      },
      "by_outcome": {
        "all": {
          "instances_activated": 2,
          "instances_total": 3,
          "usage_rate": 0.6666666666666666,
          "resolved_rate_when_active": 0.5,
          "resolved_rate_when_not_active": 0.0,
          "resolved_rate_delta": 0.5
        }
      }
    }
  },
  "skill_set_drift": {
    "groups": []
  },
  "per_instance": [
    {
      "instance_id": "inst_1",
      "active_skills": {
        "skill_a": "explicit_mention"
      }
    }
  ]
}
```

## Exit codes

| Code | Class | Condition |
|------|-------|-----------|
| `0` | `success` | Clean run (including when skills are disabled) |
| `2` | `usage_error` | Missing `--sweep` / `--sweep-dir` or invalid filter / format |
| non-zero | `error` | Missing or corrupted sweep artifacts / directories |
