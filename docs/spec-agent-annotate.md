# spec-agent-annotate — `agent annotate`

Schema-versioned human-verdict sidecar for a trajectory (issue #539).

## Overview

`agent annotate <TRAJECTORY>` attaches a structured, schema-versioned annotation
to a trajectory by writing a sidecar file next to it — **without mutating the
trajectory** (`.traj.json` bytes are identical before and after). The annotation
captures a human verdict, an optional failure-category tag, an optional free-text
note, and zero or more step-level notes. All flags are non-interactive; the
command is fully scriptable from CI or an operator shell script.

**Zero-cost guarantees:** no network calls, no model calls. Typically completes
in under 2 s on a local trajectory.

## CLI Surface

### Write mode

```sh
max agent annotate <TRAJECTORY> \
  --verdict <correct|incorrect|partial|unsure> \
  [--failure-category <STRING>] \
  [--note <TEXT>] \
  [--step-note <INDEX>=<TEXT>] ...  \
  [--force] \
  [--format <text|json>]
```

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `<TRAJECTORY>` | path | required | Path to the `.traj.json` file to annotate |
| `--verdict` | controlled vocabulary | required | `correct`, `incorrect`, `partial`, or `unsure` |
| `--failure-category` | free string | none | Failure-category tag; taxonomy values from `FailureCategory` are recommended but not enforced |
| `--note` | string | none | Free-text note (redacted before writing) |
| `--step-note INDEX=TEXT` | repeatable | none | Step-level note; INDEX is the zero-based message index in the trajectory |
| `--force` | flag | false | Overwrite an existing sidecar |
| `--format` | `text` \| `json` | `text` | Output format for the write confirmation |

#### Example

```sh
max agent annotate runs/django__django-12345.traj.json \
  --verdict incorrect \
  --failure-category patch_apply_invalid \
  --note "agent edited the wrong file; patch rejects cleanly on step 3" \
  --step-note 3="wrong file selected here" \
  --step-note 7="patch generation failed silently"
```

### Read mode (`--show`)

```sh
max agent annotate <TRAJECTORY> --show [--format <text|json>]
```

Prints the existing annotation as text (default) or as pretty-printed JSON. Exits
non-zero if no annotation sidecar exists.

#### Example

```sh
max agent annotate runs/django__django-12345.traj.json --show --format json
```

## Sidecar File

### Location

The sidecar is written next to the trajectory file:

| Trajectory path | Sidecar path |
|----------------|--------------|
| `runs/<id>.traj.json` | `runs/<id>.annotation.json` |
| `runs/<id>/run-1.traj.json` | `runs/<id>/run-1.annotation.json` |
| `runs/<id>/trajectory.json` | `runs/<id>/trajectory.annotation.json` |

### Schema

```jsonc
{
  "artifact_kind": "trajectory_annotation",
  "schema_version": { "major": 1, "minor": 12 },
  "instance_id": "django__django-12345",
  "trajectory_path": "runs/django__django-12345.traj.json",
  "trajectory_sha256": "<64-char hex>",
  "verdict": "incorrect",
  "failure_category": "patch_apply_invalid",
  "note": "agent edited the wrong file",
  "step_notes": [
    { "step": 3, "note": "wrong file selected here" }
  ],
  "annotated_at": "2026-06-22T14:00:00Z"
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `artifact_kind` | yes | Always `"trajectory_annotation"` |
| `schema_version` | yes | Current harness schema version |
| `instance_id` | yes | Derived from trajectory path (stem of `<id>.traj.json`, or parent dir name for nested layouts) |
| `trajectory_path` | yes | Path as supplied by the operator |
| `trajectory_sha256` | yes | Full SHA-256 hex digest of the trajectory file bytes; allows a later reader to detect if the trajectory changed |
| `verdict` | yes | One of `correct`, `incorrect`, `partial`, `unsure` |
| `failure_category` | no | Free-string failure-category tag |
| `note` | no | Free-text note (redacted before writing) |
| `step_notes` | no | Array of `{step, note}` objects |
| `annotated_at` | yes | ISO 8601 UTC timestamp of write |

## Validation

The command exits non-zero with a clear message if:

- The target file cannot be read or is not a parseable trajectory
- `--verdict` is outside the controlled vocabulary (`correct`, `incorrect`, `partial`, `unsure`)
- Any `--step-note` index is out of range for the trajectory (i.e., ≥ number of messages)
- `--step-note` value does not contain `=` (malformed)
- Write mode is invoked without `--verdict`
- `--show` is invoked and no annotation sidecar exists

## Re-run / `--force` behavior

If the sidecar already exists, a write attempt **without `--force`** exits non-zero
with a message like:

```
error: agent annotate: annotation already exists at `runs/foo.annotation.json`; use --force to overwrite
```

With `--force`, the existing sidecar is overwritten atomically in place. The
behavior is deterministic: the winner is the last write that completes.

## Secret Redaction

`Redactor::default_enabled()` is applied on `surface::EXPORT` to `--note` and
every `--step-note` text **before writing**. Redaction also runs on `--show`
emit, consistent with other export/read surfaces. Secrets never reach the sidecar
on disk.

## Instance ID Derivation

| Trajectory path layout | `instance_id` |
|------------------------|---------------|
| `<dir>/<id>.traj.json` (flat / root) | file stem minus `.traj.json`: `<id>` |
| `<dir>/<id>/run-k.traj.json` (nested) | parent directory name: `<id>` |
| `<dir>/<id>/trajectory.json` (nested single) | parent directory name: `<id>` |

## Exit Codes

| Exit code | Outcome |
|-----------|---------|
| 0 | Annotation written (or shown) successfully |
| 2 (`usage_error`) | Invalid flag, bad verdict, out-of-range step index, missing `--verdict`, `--show` with no sidecar, non-trajectory input |

## Out of Scope

- Wiring annotations into any exporter (JSONL/fine-tuning/Jinja): separate downstream slice once the contract exists.
- Aggregating/reporting annotations across a whole sweep: a future `bench annotations` rollup.
- Any interactive/TUI labeling experience: this command is flag-driven and scriptable only.
