# `agent fs-audit` — Post-hoc Filesystem Boundary Audit (issue #511)

## Summary

`agent fs-audit --sweep <DIR>` (or `--trajectory <FILE>`) scans bash commands
recorded in trajectory files for path references that resolve outside the
configured workdir: absolute paths not under the workdir, `..` traversals,
`$HOME`/`~/` references, and references to well-known system directories.

The command is **zero-cost**: read-only over trajectory files, no model calls, no
network access, no mutation of sweep artifacts.

## Motivation

When an agent run executes inside a container, its bash commands should access
only the workdir (typically `/repo`). Commands that read `/etc/passwd`, write to
`/tmp`, or traverse `../../` may indicate unexpected behavior, a prompt-injection
side-effect, or a misconfigured tool. This command provides a fast,
deterministic audit of stored trajectories so operators can detect and
investigate out-of-workdir access before publishing results.

## Related Commands

| Command | Purpose |
|---------|---------|
| `agent redact-audit` | Detects *secret leaks* in sweep artifacts (issue #342) |
| `agent injection-audit` | Detects *injection signals* in sweep trajectories (issue #343) |
| `agent fs-audit` | Detects *filesystem boundary violations* in trajectories (this spec) |

## Usage

```
max agent fs-audit (--trajectory <FILE> | --sweep <DIR>) [OPTIONS]
```

Exactly one of `--trajectory` or `--sweep` is required.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--trajectory <FILE>` | — | Single `.traj.json` file to audit (mutually exclusive with `--sweep`) |
| `--sweep <DIR>` | — | Directory containing `*.traj.json` files (mutually exclusive with `--trajectory`) |
| `--workdir <PATH>` | from trajectory | Override the workdir boundary for all trajectories scanned |
| `--allow <PATH>` | none | Suppress findings whose `matched_path` starts with this prefix. Repeatable. |
| `--format <FMT>` | `text` | Output format: `text` or `json` |

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | No findings — all accessed paths are within the workdir |
| 45 (`fs_audit_findings`) | At least one out-of-workdir path access found |
| 46 (`fs_audit_scan_error`) | Could not read or parse a trajectory file or the sweep directory |
| 1 (`internal_error`) | Unexpected internal error |
| 2 (`usage_error`) | Invalid flag value or configuration error |

When both findings and scan errors are present, exit 45 takes precedence so CI
gates on the more actionable signal.

## Workdir Resolution

The workdir boundary for each trajectory is resolved in priority order:

1. `--workdir <PATH>` flag (applies to all trajectories in the sweep)
2. `info.local_workdir` field in the trajectory's JSON header
3. Default: `/repo`

## Path Detection

The audit extracts potential path references from each bash command using
a regular expression that matches:

- `$HOME` or `${HOME}` references (always flagged — always outside workdir)
- `~/path` tilde home references (always flagged)
- `../` and `../../` dotdot traversals (always flagged — conservative)
- Absolute paths starting with `/` (flagged when not under the workdir)

Single-quoted strings are stripped before path extraction to avoid false
positives from shell string literals (e.g. `sed 's/foo/bar/'`).

## Access Classification

Each finding is classified as `read`, `write`, or `ambiguous` based on the
primary command head and the presence of shell write-redirect operators (`>`, `>>`):

| Classification | Trigger |
|---------------|---------|
| `read` | Command head in: `cat`, `head`, `tail`, `less`, `more`, `wc`, `file`, `stat`, `ls`, `du`, `find`, `diff`, `cmp`, `strings`, `hexdump`, `xxd`, `grep`, `rg`, `egrep`, `fgrep`, `readlink`, `od`, `cut`, `sort`, `uniq`, `md5sum`, `sha256sum`, `sha1sum` |
| `write` | Command head in: `rm`, `rmdir`, `mkdir`, `touch`, `chmod`, `chown`, `ln`, `install`, `truncate`, `dd`, `tee`, `mktemp`, `mkfifo`, `mknod`; or command contains `>` redirect |
| `ambiguous` | All other commands |

Known prefix wrappers (`sudo`, `time`, `env`, `nohup`, `nice`) and leading
`VAR=val` environment assignments are stripped before the head is determined.

## Allowlist

The `--allow <PATH>` flag suppresses findings whose `matched_path` starts with
the given prefix. Repeatable:

```
max agent fs-audit --sweep ./runs --allow /etc --allow /tmp
```

This is useful for sweep configurations where the agent legitimately reads
system headers or writes to `/tmp` for build artifacts.

## Finding Schema

Each finding (in `--format json` findings array):

```json
{
  "instance_id": "django__django-11422",
  "step_index": 2,
  "command_head": "cat",
  "matched_path": "/etc/passwd",
  "access": "read"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `instance_id` | string | Trajectory filename stem (e.g. `django__django-11422`) |
| `step_index` | integer | 0-based index of the assistant message in `messages[]` |
| `command_head` | string | Primary command head after stripping prefix wrappers |
| `matched_path` | string | The path string extracted from the command |
| `access` | string | `read`, `write`, or `ambiguous` |

## JSON Report Schema

```json
{
  "artifact_kind": "fs_audit",
  "schema_version": {"major": 1, "minor": 0},
  "source": "./runs",
  "workdir": "/repo",
  "trajectories_scanned": 500,
  "total_findings": 3,
  "findings": [...],
  "scan_errors": []
}
```

## Output Formats

### `--format text` (default)

Human-readable summary printed to stdout. Includes:
- Summary line: trajectory count, finding count, effective workdir
- Per-finding details: instance ID, step index, command head, path, access kind
- Scan errors (if any)

### `--format json`

Machine-readable JSON report (full schema above). Suitable for CI artifact
storage and diffing.

## Performance Budget

The audit scans each trajectory file once, running a single compiled regex over
each bash command. Expected throughput is ≥ 500 trajectories/second on a
developer laptop.

## Security Properties

- **Read-only**: The audit never modifies trajectory files or the sweep directory.
- **No model calls**: Pure Rust; no network, no subprocess.
- **Zero-cost**: Suitable for use as a mandatory CI gate on every sweep publish.

## Out of Scope

- Inline blocking at run time (post-hoc only)
- Process-level syscall tracing (requires kernel instrumentation)
- Network access auditing
- Cross-sweep trending
