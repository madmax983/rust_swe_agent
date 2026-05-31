# Spec: `agent apply`

Applies a captured `.patch` artifact to a working tree with the harness's
full safety-gate stack. Implements issue #473.

## Motivation

`mini` (and every sweep) writes a redaction-clean `.patch` artifact, but
the core operator loop offered no first-class way to apply that patch back
to a checkout. Operators had to locate the artifact manually and craft a raw
`git apply` invocation — with zero clean-tree protection and no guard against
`[REDACTED:…]`-corrupted patches. `agent apply` fills this gap.

## Usage

```
max agent apply --patch <PATH> [OPTIONS]
max agent apply --trajectory <PATH> [OPTIONS]
max agent apply --sweep <DIR> --instance <ID> [OPTIONS]
```

Exactly **one** patch selector must be supplied. All other flags are optional.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--patch <PATH>` | — | Direct path to a `.patch` file. |
| `--trajectory <PATH>` | — | Path to a `.traj.json`; the sibling `.patch` is resolved automatically. |
| `--sweep <DIR> --instance <ID>` | — | Sweep output dir + instance ID; resolves `<dir>/<id>.patch`. |
| `--target <DIR>` | CWD | Git working tree to apply into. |
| `--allow-redacted` | false | Skip the redaction-corruption gate. |
| `--allow-dirty` | false | Skip the clean-tree gate. |
| `--dry-run` | false | Run `--check` only; print what would change; exit 0. |
| `--3way` | false | Delegate to `git apply --3way` for fuzzy application. |
| `--report <PATH>` | `apply-report.json` (next to target) | Where to write the JSON report. |

## Selector Rules

Selector flags are mutually exclusive. `clap` enforces this at parse time.

| Invocation form | Patch resolved from |
|-----------------|---------------------|
| `--patch <PATH>` | `<PATH>` directly |
| `--trajectory <PATH>` | sibling of `<PATH>`: `task.traj.json` → `task.patch` |
| `--sweep <DIR> --instance <ID>` | `<DIR>/<ID>.patch` |

Missing or ambiguous selectors exit **2** (`usage_error`) before any
filesystem mutation.

## Safety Gates (evaluation order)

1. **Selector resolution** — Exactly one selector form must be present.
   Ambiguous or missing selectors exit 2 before any mutation.

2. **Git working tree check** — `--target` (or CWD) must be inside a git
   working tree (`git rev-parse --is-inside-work-tree`). A non-git target
   exits 2 before any mutation.

3. **Dirty-tree check** — The working tree must have no uncommitted changes
   (`git status --porcelain`). A dirty tree exits **31**
   (`apply_dirty_tree_refused`). Override: `--allow-dirty`.

4. **Redaction-corruption check** — Applied when `--patch` is used directly
   *or* when a trajectory is available:
   - The patch content is scanned for `[REDACTED:…]` markers.
   - When a sibling trajectory is available, its `info.redaction.counts`
     is checked for any entry with `surface == "patch_submission"`.
   Either condition exits **30** (`apply_redacted_refused`). Override:
   `--allow-redacted`.

5. **`git apply --check`** — A dry-run application is attempted before
   mutating the tree. On failure the rejected hunks are printed and the
   command exits **29** (`apply_check_failed`). The tree is left unchanged.

6. **`--dry-run` short-circuit** — If `--dry-run`, stop here: print the
   files and hunk counts that would change and exit **0**.

7. **Apply** — `git apply [--3way]` is run. The tree is mutated. An
   `apply-report.json` is written.

## apply-report.json

Written on every non-error outcome (including empty-patch and dry-run).

```json
{
  "schema_version": {"major": 1, "minor": 10},
  "artifact_kind": "apply_report",
  "source_patch_path": "/path/to/task.patch",
  "target_git_sha": "abc123def456...",
  "files_changed": ["src/lib.rs"],
  "lines_added": 5,
  "lines_removed": 2,
  "check_result": "passed",
  "applied": true,
  "dry_run": false
}
```

`check_result` values: `"passed"` | `"empty"` | `"failed"`.

## Exit-Code Matrix

| Code | Label | Meaning |
|------|-------|---------|
| 0 | `success` | Patch applied (or dry-run / empty patch). |
| 1 | `internal_error` | I/O failure or unexpected subprocess error. |
| 2 | `usage_error` | Missing/ambiguous selector, or non-git target. |
| 29 | `apply_check_failed` | `git apply --check` rejected the patch. Tree unchanged. |
| 30 | `apply_redacted_refused` | Patch has `[REDACTED:…]` markers or trajectory records patch-submission redaction. |
| 31 | `apply_dirty_tree_refused` | Working tree has uncommitted changes. |

## Out of Scope

- Opening GitHub PRs (use `mini --open-pr` or sweep `--open-prs`).
- Reverting/unapplying a previously applied patch.
- Interactive conflict resolution (use `--3way` + manual git).
- Applying multiple sweep instances in one invocation.
- Re-running or re-evaluating the agent.
