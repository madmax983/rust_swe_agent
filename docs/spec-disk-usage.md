# `bench du` — Run-Directory Disk Usage & Safe Reclamation

## Overview

`bench du` reports disk footprint under a `--root` runs directory — ranked
per sweep and per artifact category — and, with `--prune`, safely reclaims
stale sweeps. It reads **only on-disk artifacts and never calls a model
provider or the network** (zero-cost guarantee).

Every `mini`, `bench swebench`, `bench matrix`, and `bench rehearsal` run
writes trajectories, `results.json`, evaluation artifacts, partial
checkpoints, bundles, and logs into `runs/<sweep>/`. A week of sweeps
silently accumulates into many GB with no first-class way to see where the
disk went, or to reclaim it without risking deletion of a sweep that is
still running. `bench du` turns `du -sh runs/*` and hand-picked `rm -rf`
into one lifecycle-aware, artifact-attributed command with a guardrail
that skips anything it cannot confirm is idle within a configured freshness
window — see "Lifecycle states" below for exactly what this guardrail can
and cannot guarantee.

## Usage

```
bench du --root <DIR> [OPTIONS]
```

### Required

| Argument | Description |
|---|---|
| `--root <DIR>` | Runs directory to scan. Each immediate subdirectory is treated as one **sweep**; files directly under `--root` are counted as `unattributed`. |

### Optional

| Flag | Default | Description |
|---|---|---|
| `--format <FMT>` | `text` | Output format: `text` (ranked table) or `json` (schema-versioned artifact) |
| `--prune` | off | Evaluate (and, with `--apply`, delete) stale sweeps. **Dry-run unless combined with `--apply`.** |
| `--apply` | off | Actually delete the sweeps selected by `--prune`. Requires `--prune` and at least one selector. |
| `--older-than <DURATION>` | — | Selector: only sweeps whose most recent on-disk activity is at least this old. Accepts `<N>d`, `<N>h`, `<N>m`, `<N>s`, or a plain integer (seconds). |
| `--keep-last <N>` | — | Selector: never delete the N most recently modified sweeps, regardless of the other selectors. `N` must be `>= 1`; `--keep-last 0` is rejected as a usage error rather than silently treated as "keep nothing" (see "Selector semantics"). |
| `--incomplete-only` | off | Selector: only consider sweeps classified `incomplete` or `interrupted` (never `complete`). |
| `--in-progress-window <SECONDS>` | `900` | A sweep with a partial trajectory checkpoint touched within this many seconds of "now" is classified `in-progress` and is never a prune candidate. |

## Key Concepts

### Sweep

A **sweep** is one immediate subdirectory of `--root` — the output
directory of one `mini`/`bench swebench`/`bench matrix`/`bench rehearsal`
invocation (e.g. `runs/my-sweep/`). Files placed directly under `--root`
(not inside any sweep directory) are not part of any sweep and are counted
in the top-level `unattributed_bytes` bucket.

### Artifact categories

Every file under a sweep is classified into exactly one category by name:

| Category | Matches |
|---|---|
| `trajectories` | `*.traj.json` where `info.partial` is `false`/absent/unreadable |
| `partial_checkpoints` | `*.traj.json` where `info.partial == true` — a mid-run checkpoint (see `docs/spec-checkpointing.md`) |
| `evaluation` | `evaluation.json`, `*.evaluation.json` |
| `bundles` | `*.tar.gz`, `*.tgz`, `*.zip`, `BUNDLE.json` |
| `logs` | `*.log`, `events.jsonl` |
| `other` | everything else: `results.json`, `manifest.json`, `halt-report.json`, `ledger.json`/`cache-stats.json`/other sidecar reports, `*.patch`, `all_preds*.jsonl`, unknown files |

A `.traj.json` file that cannot be parsed (corrupt JSON, unreadable) is
conservatively classified `trajectories`, not `partial_checkpoints` — an
unreadable file cannot be proven to be a live checkpoint.

### Byte attribution invariant

```
total_bytes == sum(sweep.total_bytes for every sweep) + unattributed_bytes
total_bytes == a plain recursive size walk of --root (skipping symlinks)
```

Both hold exactly (0-byte discrepancy) by construction: `total_bytes` for
the whole root is computed the same way `total_bytes` for one sweep is
computed (sum of regular-file sizes, symlinks skipped), so per-sweep totals
and the explicit `unattributed` bucket always reconcile to the top-level
total. `recursive_size_walk` (an independently-implemented walker) is used
in the test suite as an oracle to verify this.

Unlike `bench ledger`, `bench du` does **not** write a side artifact into
`--root` — doing so would immediately change the very byte count the next
invocation reports, self-polluting the directory this command exists to
keep clean. Redirect `--format json` output to a file instead if a
persisted artifact is wanted (e.g. `bench du --root runs --format json >
disk-usage.json`).

### Lifecycle states

Each sweep is classified into exactly one state, in this priority order:

1. **`in_progress`** — at least one `partial_checkpoints` file was modified
   within `--in-progress-window` seconds of "now". Takes priority over every
   other signal, including an existing `results.json` (a `bench retry` can
   resume writing fresh checkpoints into a directory that already has a
   stale `results.json` from an earlier pass).
2. **`complete`** — a non-symlinked `results.json` exists directly under the
   sweep directory, and no fresh checkpoint is present. A *symlinked*
   `results.json` does not count (see below).
3. **`interrupted`** — no `results.json`, but at least one (stale) partial
   checkpoint exists — the process was killed without a clean shutdown.
4. **`incomplete`** — neither `results.json` nor any checkpoint. Includes
   empty or freshly-created sweep directories.

The `results.json` check deliberately does not follow symlinks, matching the
byte-accounting walk (which also skips symlinks): a symlinked `results.json`
contributes zero bytes to any category, so treating it as evidence of
`complete` would make lifecycle and byte accounting disagree about the same
sweep.

#### What "in-progress" detection can and cannot guarantee

There is no lockfile, PID file, or heartbeat mechanism in this codebase (see
`docs/spec-checkpointing.md`); `in_progress` detection reuses the existing
mid-run checkpoint signal (`info.partial: true`, rewritten atomically after
every agent turn) plus mtime freshness — the same signal `bench tail`/`bench
watch` already use to detect a live run. **This is a freshness heuristic, not
a liveness proof.** Concretely:

- If the writing process is paused (e.g. `SIGSTOP`, a paused container) or a
  single agent step (a slow test suite, a slow docker build, a slow model
  call) runs longer than `--in-progress-window` without an intervening
  checkpoint write, the sweep's mtime goes stale and it is **not** classified
  `in_progress` even though the process is still alive. Widen
  `--in-progress-window` if your workload has long steps.
- `--prune --apply` mitigates, but cannot fully close, the gap between "scan
  time" and "delete time": immediately before deleting each candidate it
  re-scans that single sweep directory and re-checks freshness (see
  "Deletion semantics"), so a sweep that resumes checkpointing partway
  through a long `bench du` run is still caught. A checkpoint written in the
  narrow window between that re-check and the actual `remove_dir_all` call
  is the one race this cannot close without a lock/PID mechanism.
- A future or clock-skewed mtime (NFS/container clock drift, a stray
  `touch -d future`) is logged to stderr as a warning and treated as age `0`
  — the affected sweep simply becomes ineligible for `--older-than`-based
  pruning until the clock catches up; it is never a false *reclaim*.

In short: `bench du` will never delete a sweep whose checkpoint looks fresh,
but "looks fresh" is a time-window heuristic, not a guarantee that the
writing process is actually still running.

### `--older-than` / age selector

A sweep's age is `now - last_modified`, where `last_modified` is the
**maximum mtime of any file inside the sweep** (falling back to the sweep
directory's own mtime only when the sweep contains zero files). Because an
`in_progress` sweep's freshest file is, by definition, within
`--in-progress-window`, an `in_progress` sweep can only satisfy
`--older-than <D>` for `D <= --in-progress-window` — i.e. a generous
`--in-progress-window` is what allows an old-but-still-checkpointing sweep
to correctly show up as a *protected*, not silently-ignored, candidate.

### Selector semantics

`--prune` accepts zero or more of `--older-than`, `--keep-last`, and
`--incomplete-only`. When more than one is given they combine with **AND**:
a sweep is only a candidate if it satisfies every given selector. `--apply`
additionally requires **at least one** selector — with none, it refuses and
exits non-zero (usage error) rather than treating "no selector" as "select
everything".

- **`--older-than <D>`**: candidate sweep's age must be `>= D`.
- **`--incomplete-only`**: candidate sweep's lifecycle state must be
  `incomplete` or `interrupted` (never `complete`, never `in_progress`).
- **`--keep-last <N>`**: sweeps are ranked by `last_modified` (all sweeps,
  any state); the N most recent are retained and removed from the candidate
  set regardless of whether they matched the other selectors. Reported
  separately as `retained_by_keep_last`, distinct from the safety-driven
  `protected` bucket. **`N` must be `>= 1`.** `--keep-last 0` is rejected as
  a usage error: it would retain nothing, silently making every
  non-in-progress sweep a candidate — exactly the "no selector means select
  everything" outcome `--apply`'s selector requirement exists to prevent —
  while still technically satisfying "at least one selector was given".

An `in_progress` sweep is **never** a candidate. If it would otherwise have
matched every given selector, it is reported under `protected` instead
(driving the blocked exit code below); if it would not have matched anyway,
it is silently excluded like any other non-matching sweep — reported
neither as a candidate nor as protected.

### Deletion semantics

`--prune` alone (no `--apply`) is a dry run: it computes and reports exactly
which sweeps would be deleted and how many bytes would be reclaimed, and
deletes nothing.

`--prune --apply` walks the `candidates` set one at a time. For each one, it
first **re-scans that single sweep directory** with a fresh timestamp and
re-checks freshness — narrowing (not eliminating; see "Lifecycle states"
above) the gap between the initial full-`--root` scan and the moment of
deletion, since a large `--root` can take real time to scan and a
resumed/retried process could start checkpointing again during that window.
A candidate caught in-progress by this re-check is moved into `protected`
and left untouched, exactly like a sweep that was already in-progress at the
initial scan. Otherwise the sweep is deleted (`std::fs::remove_dir_all`); a
candidate whose deletion fails (permission error, or a non-UTF-8-named
directory it could not resolve) is logged to stderr and added to
`deletion_failed`, not `deleted`.

Partial success (some sweeps deleted, one skipped as `protected`, one
failing to delete) is reported in full, and the process exits non-zero
(`blocked: true`, exit code 50) whenever `protected` or `deletion_failed` is
non-empty after an `--apply` run — flagging that not everything requested
was actually reclaimed, even though every deletion that *did* succeed still
happened.

## JSON Schema

`--format json` prints a schema-versioned object to stdout.

```json
{
  "artifact_kind": "disk_usage_report",
  "schema_version": { "major": 1, "minor": 13 },
  "generated_at": "2026-01-01T00:00:00Z",
  "root": "/path/to/runs",
  "total_bytes": 15728640,
  "unattributed_bytes": 1024,
  "sweeps": [
    {
      "id": "sweep-a",
      "path": "/path/to/runs/sweep-a",
      "total_bytes": 10485760,
      "lifecycle_state": "complete",
      "last_modified": "2025-12-20T10:00:00Z",
      "last_modified_unix": 1766224800,
      "age_seconds": 950400,
      "categories": {
        "trajectories": 9000000,
        "evaluation": 1000000,
        "bundles": 0,
        "partial_checkpoints": 0,
        "logs": 200000,
        "other": 285760
      }
    }
  ],
  "prune": {
    "apply": false,
    "dry_run": true,
    "selectors": { "older_than_secs": 604800, "keep_last": null, "incomplete_only": false },
    "candidates": [
      { "id": "sweep-a", "path": "/path/to/runs/sweep-a", "bytes": 10485760, "lifecycle_state": "complete" }
    ],
    "protected": [],
    "retained_by_keep_last": [],
    "deleted": [],
    "deletion_failed": [],
    "would_reclaim_bytes": 10485760,
    "reclaimed_bytes": 0,
    "blocked": false
  }
}
```

`prune` is `null`/omitted when `--prune` was not passed. `lifecycle_state`
is one of `complete`, `interrupted`, `incomplete`, `in_progress`.
`deletion_failed` is omitted when empty; it lists candidates where
`std::fs::remove_dir_all` itself returned an error (permission error, a
non-UTF-8-named directory, etc.) — distinct from `protected`, which lists
candidates skipped because they were not confirmed idle. Both populate
`blocked`.

### Schema version

`disk_usage_report` artifacts carry the harness-wide `schema_version` (see
`docs/artifact-contract.md`); major bumps are breaking, minor bumps are
additive. Snapshot tests should match against `schema_version.major` rather
than the full pair to stay stable across minor releases.

### Redaction safety

The report contains only file paths, byte counts, timestamps, and lifecycle
labels — no message content. Safe to paste into a PR description or a Slack
thread, subject to the usual caveat that sweep directory names/paths are
operator-chosen and may themselves be sensitive.

## Exit Codes

| Code | Meaning |
|---|---|
| 0 | Success: a clean report, a dry-run `--prune`, or an `--apply` that deleted every matching candidate with nothing protected and no deletion failure |
| 2 | Usage or configuration error: bad `--format`, `--root` not a directory, unparseable `--older-than`, `--keep-last 0`, `--apply` without `--prune`, or `--apply` without any of `--older-than`/`--keep-last`/`--incomplete-only` |
| 50 | `disk_usage_prune_blocked` — `--prune --apply` skipped at least one matching sweep, either not confirmed idle or because deletion itself failed. See `docs/exit-codes.md`. |

## Examples

```bash
# Ranked text report of a runs directory
bench du --root runs/

# JSON artifact for CI diffing
bench du --root runs/ --format json

# Preview what a week-old cleanup would reclaim — deletes nothing
bench du --root runs/ --prune --older-than 7d

# Actually reclaim sweeps older than 7 days
bench du --root runs/ --prune --apply --older-than 7d

# Keep only the 5 most recent sweeps, regardless of lifecycle state
bench du --root runs/ --prune --apply --keep-last 5

# Reclaim only incomplete/interrupted sweeps, keep every complete one
bench du --root runs/ --prune --apply --incomplete-only
```

## Performance Notes

`bench du` walks every file under `--root` once (byte-size + category
classification) and additionally opens each `.traj.json` file to read its
`info.partial` flag. Runtime is proportional to the number of files on
disk, not the number of sweeps; very large sweep archives with many
trajectory files will take proportionally longer than the JSON-only reads
used by `bench ledger`/`bench cache-stats`.

## Scope

- **In scope**: local filesystem `--root` runs directories; per-sweep,
  per-category byte attribution; lifecycle classification; guarded,
  selector-driven deletion at the sweep-directory grain.
- **Out of scope**: compression or repacking of retained artifacts;
  remote/object-store (S3/GCS) backends; a background/daemon GC; Docker
  image/container cleanup (see the top-level `cleanup` command); deletion
  at a finer grain than one sweep directory (e.g. deleting one instance's
  trajectory within an otherwise-kept sweep).
