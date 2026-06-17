# `bench inspect --format` — Trajectory Export Format Contract

Governs the human/integration export surface of trajectory files. `bench inspect` renders
a single trajectory into a portable format for downstream tooling, review, or notebook
sharing. This spec enumerates every supported export format, its stability tier, the
mandatory redaction invariant that all formats must satisfy, and the acceptance bar for
new exporters.

This family is **not** a substitute for versioned machine artifacts — see the
[relationship to the artifact contract](#relationship-to-the-artifact-contract) section.

## Usage

```
max bench inspect --sweep <SWEEP_DIR> --instance <INSTANCE_ID> --format <FORMAT> [--output <PATH>]
max bench inspect --list-formats
```

### Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--sweep PATH` | required | Completed sweep directory. |
| `--instance ID` | required for export formats | Instance ID to export. |
| `--format FORMAT` | `text` | Export format; see table below. |
| `--output PATH` | stdout | Write output to a file instead of stdout. |
| `--list-formats` | — | Print all formats compiled into this build with their tier and consumer; exit 0. |

## Supported formats

| Format | Stability tier | Cargo feature | Intended downstream consumer |
|--------|---------------|---------------|------------------------------|
| `markdown` | stable | always compiled | docs, PR review, human readers |
| `csv` | stable | `csv-export` | spreadsheets, jq pipelines, tabular tools |
| `html` | stable | `html-export` | self-contained browser view, shared notebooks |
| `mermaid` | experimental | `mermaid-export` | Mermaid sequence-diagram renderers (GitLab, GitHub markdown, mermaid.live) |
| `bash` | stable | `bash-export` | reproducibility testing, local script execution |

Feature-gated formats require rebuilding with the corresponding feature:

```bash
cargo build --features csv-export,html-export,mermaid-export
```

When a format is requested but its Cargo feature was not compiled in, the command exits
with code 2 and a message of the form:

```
format_unavailable: --format html requires the `html-export` Cargo feature; rebuild with `--features html-export`
```

## Stability tiers

### `stable`

Schema and layout are governed. A change that alters the output in a way that would break
an existing downstream consumer (column order, heading names, structural markup) requires:

1. A documented rationale in the PR description.
2. An update to this spec (format version note or change log entry).

This mirrors the minor/major bump policy in [`docs/artifact-contract.md`](artifact-contract.md):
additive changes (new optional fields, new sections at the end) need only a note;
structural rearrangements that break existing consumers need a version bump entry here.

### `experimental`

The output layout may change without notice. Operators should not build stable automation
against `experimental` formats. A format graduates from `experimental` to `stable` when
its layout is considered settled and tested for at least one minor release.

## Mandatory redaction invariant

**Every exporter MUST apply the same `Redactor`/secret-surfacing pass as the canonical
`bench inspect` view path before emitting any output.** Concretely: every `export()`
implementation calls `Redactor::default_enabled()` and passes each piece of user-visible
text through `redactor.redact_text(text, surface::EXPORT)` before writing it to the
output buffer.

`Redactor::default_enabled()` applies the built-in structured-secret pass (it does **not**
load operator-configured literals — see the note below). The following shapes are redacted:

- Structured token patterns: GitHub tokens (`ghp_*`, `ghs_*`, `gho_*`, `github_pat_*`),
  bearer tokens, API keys (`sk-*`, `sk-ant-*`, `AKIA*`), PEM private-key blocks.
- Current-process environment variable values whose names contain `TOKEN`, `SECRET`,
  `KEY`, `PASSWORD`, or `CREDENTIAL`.
- `.env`-style assignments with sensitive names.

Redacted values are replaced with stable digest markers:
`[REDACTED:<kind>:<size_class>:<hash>]`

**Note on configured literals.** Operator-configured `config.redaction.secret_literals`
and `custom_patterns` are part of the broader redaction surface but are **not** applied by
this view/export pass: `Redactor::default_enabled()` is built from `RedactionCfg::default()`.
This matches the canonical `bench inspect` text/JSON view exactly (it uses the same
`default_enabled()` constructor), which is the invariant this spec enforces — export is
neither stronger nor weaker than the canonical human-readable view. Configured-literal
redaction at write time is governed separately by
[`docs/spec-secret-redaction.md`](spec-secret-redaction.md).

### Enforcement

The shared conformance test at `tests/export_redaction_conformance.rs` iterates
`trajectory::export::registry()` — the single source of truth for all compiled-in
exporters — and asserts for every format that:

1. No raw canary secret (`ghp_…`) appears in the output.
2. At least one `[REDACTED:` marker appears in the output.

The test runs in CI with `--all-features` so every feature-gated exporter is covered.
**Adding an exporter that skips redaction fails CI at this test.** Adding an exporter
that is not registered in `registry()` is not covered and must not be done.

## Discoverability

An operator can enumerate all export formats compiled into the current build without
reading source:

```
$ max bench inspect --list-formats
markdown    stable        docs, PR review, human readers
csv         stable        spreadsheets, jq pipelines, tabular tools
html        stable        self-contained browser view, shared notebooks
mermaid     experimental  Mermaid sequence-diagram renderers (GitLab, GitHub markdown, mermaid.live)
```

The output lists only formats compiled into the running binary (feature-gated formats
appear only when their Cargo feature is enabled). The format is plain text, one format
per line, tab-separated columns: `name`, `tier`, `consumer`. Suitable for scripting with
`awk` / `grep`.

## Relationship to the artifact contract

Trajectory exports are **human/integration views**, NOT versioned machine artifacts.

The canonical trajectory artifact is `*.traj.json`, governed by
[`docs/artifact-contract.md`](artifact-contract.md) with explicit `artifact_kind` and
`schema_version` fields, a reader-compatibility class policy, and a no-bump change log.

Export formats defined in this spec:

- Carry **no** `artifact_kind` or `schema_version` metadata.
- Must **not** be used as the input to `bench evaluate`, `bench compare`, `bench triage`,
  or any other command that expects a versioned artifact.
- Are not a substitute for `bench bundle` (archival) or `*.traj.json` (replay,
  evaluation).

The two contracts serve different consumers and must not drift by treating one as a
proxy for the other.

## Acceptance bar for in-flight exporter PRs

PRs #499 (JSONL), #477 (OpenAI fine-tuning), #460 (Jupyter), and #454 (JUnit) each add
an exporter. Each **must** satisfy this bar before merging:

1. **Register** the new `ExportFormat` entry in `trajectory::export::registry()` with a
   declared `StabilityTier`.
2. **Redaction**: the `export()` impl calls `Redactor::default_enabled()` and routes all
   user-visible text through `redactor.redact_text(text, surface::EXPORT)`.
3. **CI green**: `cargo test --all-features --test export_redaction_conformance` passes
   with the new format included.
4. **Spec update**: this document is updated to add the new format row to the supported
   formats table with its tier and consumer.

## Out of scope

- Implementing any new exporter format (JSONL, Jupyter, JUnit, fine-tuning are tracked
  in their own PRs).
- The OTLP trace export path (`bench export-otlp`, `docs/spec-export-otlp.md`) and
  SWE-bench prediction/leaderboard bundles (`bench export-ci`, `docs/spec-export-ci.md`)
  — those are machine-artifact and telemetry surfaces, not this human/integration family.
- Choosing the concrete CLI surface, trait shape, or Cargo feature-flag strategy beyond
  what is already implemented.
- Live streaming or real-time export from in-flight sweeps.
