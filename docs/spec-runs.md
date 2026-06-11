# `agent runs` — single-task trajectory listing (issue #509)

Lists and summarises every `*.traj.json` file in a directory so that operators
who iterate with `mini`/`agent` can inspect outcomes, cost, and failure modes at
a glance — without hand-writing `jq`.

---

## Usage

```
max agent runs [OPTIONS]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--dir <PATH>` | `./runs` | Directory to scan for `*.traj.json` files. |
| `--recursive` | false | Descend into subdirectories. |
| `--format text\|json` | `text` | Output format (see below). |
| `--filter KEY=VALUE` | *(none)* | Narrow rows (repeatable). |
| `--sort task\|cost\|steps\|duration` | `task` | Sort column (ascending). |

---

## Table columns (`--format text`)

| Column | Source field | Notes |
|--------|-------------|-------|
| `task` | `info.task` | Truncated to 40 chars for display; `…` appended when truncated. |
| `outcome` | `info.outcome` | Raw string, e.g. `submitted`, `error`, `step_limit_reached`. |
| `failure_category` | `info.failure_category` | Label string or empty. |
| `steps` | `info.steps` | Integer or `-` when absent. |
| `duration` | `info.duration_secs` | Formatted as `NNN.Ns` or `-`. |
| `cost_usd` | `info.total_cost_usd` | Formatted as `$0.0000` or `-`. |
| `model` | `info.model_name` | Raw string or empty. |

### Footer

Printed below the table:

```
total: N  |  outcomes: submitted=A, error=B, ...
total_cost: $X.XXXX  total_duration: Y.Ys  mean_steps: Z.Z
```

If any files were skipped: `N file(s) skipped due to parse errors`.

### Empty case

When no trajectories are found in the directory:

```
no trajectories found
```

Exit code is still 0.

---

## JSON schema (`--format json`)

Emits a top-level object (schema-versioned) suitable for `jq` / CI snapshot
diffing:

```json
{
  "artifact_kind": "agent_runs_report",
  "schema_version": "1",
  "scanned_dir": "/path/to/runs",
  "recursive": false,
  "rows": [
    {
      "path": "/path/to/runs/my-task.traj.json",
      "task": "Fix the authentication bug in login.py",
      "outcome": "submitted",
      "failure_category": null,
      "steps": 12,
      "duration_secs": 47.3,
      "total_cost_usd": 0.0183,
      "model": "claude-opus-4-7"
    }
  ],
  "footer": {
    "total_rows": 1,
    "by_outcome": { "submitted": 1 },
    "total_cost_usd": 0.0183,
    "total_duration_secs": 47.3,
    "mean_steps": 12.0,
    "skipped_files": 0
  }
}
```

All fields are stable. `null` fields are included explicitly (not omitted) in the
JSON output.

---

## Filter grammar

`--filter KEY=VALUE` uses exact-string matching.

Supported keys:

| Key | Matches | Example |
|-----|---------|---------|
| `outcome` | `info.outcome` string | `--filter outcome=submitted` |
| `failure_category` | `info.failure_category` label | `--filter failure_category=step_limit` |

Filters are ANDed when repeated:

```
max agent runs --filter outcome=error --filter failure_category=model_api
```

Unknown keys are rejected with a non-zero exit and a descriptive message.

### Failure category labels

| Label | Meaning |
|-------|---------|
| `env_setup` | Initial environment/clone failed |
| `model_api` | Repeated API errors |
| `model_parse` | Repeated model response parse failures |
| `step_limit` | Step budget exhausted |
| `cost_limit` | USD budget exhausted |
| `budget_exhausted` | Per-task budget hit |
| `wallclock_timeout` | Wallclock deadline reached |
| `agent_internal` | Internal harness logic error |
| `patch_apply_invalid` | `git apply --check` rejected the patch |
| `patch_empty` | Submitted diff was empty |
| `secret_leak_detected` | Secret literal found in artifact |
| `agent_stagnation` | Agent repeated actions without progress |
| `history_compaction_failed` | History too large to compact |
| `read_only_violation` | Tool call blocked by read-only mode |
| `unknown` | Unrecognised category from a newer harness |

---

## Sort keys

| Key | Sort column |
|-----|------------|
| `task` (default) | File path, alphabetical ascending |
| `cost` | `total_cost_usd`, ascending |
| `steps` | `steps`, ascending |
| `duration` | `duration_secs`, ascending |

Rows with `null` in the sort column sort before rows with values.

---

## Sweep directory behaviour

When `--dir` points to a directory that also contains a `results.json` (i.e., a
sweep output directory), the command still loads each `*.traj.json` file
**independently** — one row per file. The `results.json` aggregate is ignored.
This means counts will match the per-file view, not a double-counted sweep view.

---

## Malformed files

Any `.traj.json` file that cannot be read or parsed as a valid trajectory is
silently skipped. The count of skipped files is reported in the footer
(`skipped_files` in JSON; `N file(s) skipped` in text). The command does not
abort; exit code is still 0.

---

## Exit codes

| Code | Condition |
|------|-----------|
| 0 | Success (including empty directory and skipped-files-only case) |
| 1 | I/O error (directory does not exist or is unreadable) |
| 1 | Invalid `--filter` key or `--sort` value |

---

## Performance

The command performs only local disk reads — no network calls, no model calls,
no API key required. It scans ≥ 100 trajectories in well under 1 second on any
modern filesystem.
