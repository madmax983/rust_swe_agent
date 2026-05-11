# `bench bundle` - Portable Sweep Archives

`bench bundle` exports one completed sweep into a deterministic, redaction-strict
`tar.gz` archive. The archive is meant for bug reports, audit handoff, and later
`bench inspect`, `bench reproduce`, or `bench triage` use after extraction.

## Synopsis

```bash
rust-swe-agent bench bundle --sweep runs/sweep --output sweep.tar.gz
rust-swe-agent bench bundle --sweep runs/sweep --instance repo__id-123 --output one.tar.gz
rust-swe-agent bench bundle --verify sweep.tar.gz
```

`--instance <ID>` narrows the archive to that instance's trajectory, patch if
present, filtered `results.json`, filtered `evaluation.json` if present, and the
sweep-level manifest. Without `--instance`, every instance listed in
`results.json` is included.

## Fixed Layout

Only these paths are written:

```text
manifest.json
results.json
evaluation.json                 # only when present in the source sweep
trajectories/<instance_id>.traj.json
patches/<instance_id>.patch      # only when present for that instance
BUNDLE.json                      # written last
```

Files outside that allow-list are ignored, including editor swap files, OS
metadata, logs, and ad hoc notes. Extracted bundles are accepted by existing
readers without path flags: `bench inspect --sweep <dir> --instance <id>` reads
`trajectories/<id>.traj.json`, and patch readers accept `patches/<id>.patch`.

## `BUNDLE.json`

`BUNDLE.json` is a versioned artifact:

```json
{
  "artifact_kind": "bundle_manifest",
  "schema_version": { "major": 1, "minor": 3 },
  "source_sweep_dir": ".",
  "source_manifest_hash": "sha256:...",
  "harness_git_sha": "...",
  "bundle_generated_at": "2026-05-10T12:00:00Z",
  "instance_scope": "full",
  "files": [
    { "path": "manifest.json", "sha256": "...", "bytes": 1234 }
  ]
}
```

The `files` list covers every other file in the archive and excludes
`BUNDLE.json` itself. `source_manifest_hash` is computed from the normalized
`manifest.json` bytes in the bundle.

## Redaction Strict Mode

Before any file is written to the archive, the bundle command re-runs the
default export redactor over the normalized artifact bytes. It also merges
redaction settings from the source sweep's resolved config when that config is
available in either `results.json.manifest.config.resolved` or standalone
`manifest.json.config.resolved`. If the redactor would mask anything, bundle
creation aborts before replacing the output path and prints a
`redaction:retrigger:<path>` reason.

This is fail-closed defense in depth: bundle export never silently rewrites a
leaky artifact into a different artifact.

## Determinism

Bundle creation pins:

- tar entry order;
- tar entry mtime, uid, gid, mode, and type;
- gzip mtime;
- archive layout;
- path normalization before hashing.

Two runs from identical source bytes are byte-identical when
`bundle_generated_at` is fixed. For reproducible CI fixtures, set
`SOURCE_DATE_EPOCH` to a Unix timestamp; otherwise the timestamp records the
current UTC second.

Absolute occurrences of the source sweep path inside emitted text artifacts are
rewritten relative to the bundle root before hashing. Obvious Windows and Unix
absolute path shapes are also normalized to avoid leaking local filesystem
details from trajectories, patches, or manifests.

## Extracted-Bundle Compatibility

After extraction, the fixed layout works with:

- `bench inspect --sweep <dir> --instance <id>`;
- `bench triage --sweep <dir>`;
- `bench reproduce --from <dir> --output <out> --limit 0` for manifest drift and
  reproducibility-report smoke checks.

Full `bench reproduce` still needs the original dataset to be available when
the recorded manifest points at a local dataset path. Bundles do not include
dataset rows because that would make the archive much larger and could
redistribute third-party benchmark data unintentionally.

## Verification

`bench bundle --verify <archive>` reads `BUNDLE.json`, recomputes every listed
file hash and byte count, and rejects unlisted files. Success prints:

```text
bundle:ok
```

Failures print one line per problem on stdout and exit non-zero:

```text
extra:<path>
missing:<path>
hash_mismatch:<path>
```

Hash mismatches, missing files, extra files, and redaction retriggers use the
documented `verification_failure` outcome class. Bad invocations, missing source
artifacts, and unsupported future bundle schemas use `usage_error`.
