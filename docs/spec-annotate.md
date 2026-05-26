# spec-annotate — `bench annotate`

Persistent, redaction-aware operator triage notes for SWE-bench instances.

## Overview

`bench annotate` lets operators attach short tags and notes to individual
SWE-bench instances and have them surface in future `bench inspect` and
`bench triage` passes. Judgments (e.g. "this is an evaluator flake, ignore
it") compound across sweeps instead of being re-derived on every run.

**Zero-cost guarantees:** no network calls, no model calls, no new runtime
dependencies beyond those already in `Cargo.toml`.

## CLI Surface

### `bench annotate add <INSTANCE_ID>`

Append (or update) an annotation record.

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--tag <TAG>` | string, repeatable | required | Kebab-case tag; see Tag Format |
| `--note <TEXT>` | string | none | Freeform note (≤ 1024 chars) |
| `--store <PATH>` | path | `./annotations.json` | Override store location |

#### Example

```sh
bench annotate add pytest__pytest-7234 \
  --tag evaluator-flake \
  --note "Intermittent on CI; confirmed not a real regression"

bench annotate add pytest__pytest-7234 --tag ignore
```

### `bench annotate list`

Print existing annotations, with optional filters.

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--instance <ID>` | string | none | Filter to one instance |
| `--tag <TAG>` | string | none | Filter to one tag |
| `--store <PATH>` | path | `./annotations.json` | Override store location |
| `--format <FMT>` | `text`\|`json` | `text` | Output format |

### `bench annotate rm <INSTANCE_ID>`

Remove annotation record(s).

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--tag <TAG>` | string | none | Remove only this tag; omit to remove all tags for the instance |
| `--store <PATH>` | path | `./annotations.json` | Override store location |

## Store Path Resolution

Priority (highest wins):

1. `--store <PATH>` flag
2. `BENCH_ANNOTATIONS_PATH` environment variable
3. `./annotations.json` in the current working directory

## Store Schema

`schema_version: "annotations-1.0"` — no migration logic in v1.0; a future
`annotations-2.0` would be handled by a separate migration path.

```json
{
  "schema_version": "annotations-1.0",
  "instances": {
    "<instance_id>": {
      "<tag>": {
        "note": "<text or null>",
        "created_at": "2026-05-26T12:00:00Z",
        "updated_at": "2026-05-26T12:01:00Z"
      }
    }
  }
}
```

- Keys at both levels are strings.
- `note` is optional; omitted from JSON when absent.
- `created_at` and `updated_at` are RFC 3339 UTC timestamps.
- Multiple tags per instance are supported; one `note` per `(instance_id, tag)` pair.
- Last-writer-wins on the same `(instance_id, tag)` pair, with `updated_at`
  reflecting the winning write.

## Tag Format

```
^[a-z0-9][a-z0-9-]{0,31}$
```

- Lowercase letters, digits, and hyphens only.
- Must start with a letter or digit (not a hyphen).
- 1–32 characters total (kebab-case, at most 31 hyphens after the first char).

**Valid examples:** `ignore`, `evaluator-flake`, `real-regression`, `0xdeadbeef`

**Invalid examples:** `-bad`, `BadTag`, `bad_tag`, `bad.tag`, `a` repeated 33 times

## Note Format

- Maximum 1024 Unicode characters.
- Longer input is rejected with an actionable error before any write occurs.
- Notes are passed through the same secret-redaction pipeline used by
  `bench inspect` and `bench bundle` before being written to disk.

## Atomic Writes

`annotations.json` is written atomically via write-to-temp-then-rename.  Two
concurrent `annotate add` calls targeting the same file will both land;
last-writer-wins on the same `(instance_id, tag)` pair, with `updated_at`
reflecting the winner.

## Redaction Guarantees

Notes are redacted by the default `Redactor` (using the `export` surface) before
being written to disk and before being displayed via `bench inspect` or `bench
triage`.  The same patterns used by `bench inspect` and `bench bundle` apply:

- Configured literal secrets from `TOML [redaction]`
- Environment variables with sensitive names (`*TOKEN`, `*KEY`, `*SECRET`, …)
- Structured patterns (GitHub tokens, Bearer tokens, API keys, PEM private keys)
- Custom patterns from the operator's redaction config

## Integration with Existing Commands

### `bench inspect --instance <ID>`

When annotations exist for the requested instance, an `=== Operator notes ===`
section is rendered **above** the trajectory dump:

```
=== Operator notes ===
  [evaluator-flake] — Intermittent on CI; confirmed not a real regression
  [ignore]
```

When no annotations exist, this section is omitted entirely.

### `bench triage`

The triage table gains an `annotations` column showing a compact comma-separated
tag list for the exemplar instance of each cluster.  Annotation lookup is
best-effort: if the store is missing or unreadable, the column is empty and
triage proceeds normally.

### `bench bundle`

When `annotations.json` exists in the sweep output directory, it is included in
the tarball with the same redaction checks applied to all other bundled files.
If the file is absent, the bundle is created without it — no error.

## Migration Policy for `schema_version` Bumps

v1.0 is the initial version.  Future versions will introduce a new
`schema_version` value (e.g. `"annotations-2.0"`) and document a migration
path.  v1.0 readers reject unknown major versions with an actionable error.

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Invalid tag format or note too long |
| 1 | Store file cannot be parsed |
| 1 | I/O error writing store |

## Performance

Tag write (`annotate add`) + `bench triage` lookup adds well under 50 ms to
triage runtime for sweeps up to 500 instances (annotation lookup is a single
file read + in-memory map lookup per cluster, performed best-effort).
