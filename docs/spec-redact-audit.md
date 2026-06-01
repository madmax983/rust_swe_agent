# Redact Audit

Status: Implemented (issue #342)

`agent redact-audit <dir>` scans a finished sweep tree for secret leaks in
**stored artifacts**. The runtime [redactor](spec-secret-redaction.md) masks
secrets *at write-time* and does not retroactively rewrite old artifacts, so a
redaction-config bug or an unanticipated secret shape can ship a secret to disk
silently. This command is the post-hoc safety net: scan before you share.

It **detects, it does not mutate.** Re-running the sweep with a fixed config is
the remediation — `redact-audit` never rewrites an artifact. It also **never
prints raw secret values**: every preview is masked with the same marker scheme
as the runtime redactor.

## Usage

```bash
agent redact-audit runs/my-sweep
agent redact-audit runs/my-sweep --json
agent redact-audit runs/my-sweep --detectors aws,github,jwt --disable-entropy
agent redact-audit runs/my-sweep --baseline known-fps.json
```

As a CI publish gate:

```bash
agent redact-audit runs/my-sweep && publish-the-bundle
```

### Flags

| Flag | Effect |
|------|--------|
| `<dir>` | Directory tree of sweep artifacts to scan (positional, required). |
| `--config <PATH>` | TOML config; its `secret_literals` and `custom_patterns` are audited too. |
| `--output <PATH>` | Where to write `redact_audit.json`. Default: `<dir>/redact_audit.json`. |
| `--format human\|json` | stdout summary format (default `human`). |
| `--json` | Shorthand for `--format json`. |
| `--detectors a,b,c` | Run only the named detectors (default: all). |
| `--disable-entropy` | Drop the noisy high-entropy heuristic. |
| `--baseline <PATH>` | A prior `redact_audit.json`; exit 0 if no *new* findings appear. |

## Audited artifact kinds

The scanner walks the tree and inspects these file kinds:

- `*.traj.json` (trajectories) and the legacy nested `trajectory.json`
- `evaluation.json`, `results.json` (sweep summary)
- `all_preds.jsonl` and rerun variants `all_preds.run-<k>.jsonl`
- `*.output.txt`
- `*.patch` (submitted patches — first-class, frequently-shared artifacts)
- exported `*.md`, `*.html`, `*.csv`, `*.mermaid`
- `*.tar.gz` / `*.tgz` bundles — extracted to a temp dir and scanned member by
  member. Findings inside a bundle are labelled `archive.tar.gz!inner/path`.

> **Note on bundle format.** Issue #342 named `bundle.tar.zst`, but the harness
> produces **gzip** bundles (`flate2`; see `src/run/bundle.rs`) — there is no
> zstd dependency. The auditor therefore scans `.tar.gz` / `.tgz`. If a zstd
> bundle format is ever added, extend the auditor alongside it.

The written `redact_audit.json` is never itself re-scanned.

## Detectors

Detectors run **in addition to** the configured redactor's operator-defined
`secret_literals` and `custom_patterns` (reported as `configured_literal` /
`configured_custom_pattern`). The built-in detectors:

| `--detectors` id | `match_class` | Severity |
|------------------|---------------|----------|
| `aws` | `aws_access_key` | high |
| `gcp` | `gcp_api_key` | high |
| `azure` | `azure_storage_key` | high |
| `anthropic` | `anthropic_api_key` | high |
| `openai` | `openai_api_key` | high |
| `huggingface` | `huggingface_token` | high |
| `github` | `github_pat` (classic + fine-grained) | high |
| `slack` | `slack_token` | high |
| `jwt` | `jwt` | medium |
| `pem` | `pem_private_key` | high |
| `entropy` | `high_entropy_string` | entropy_only |

### Write-time-redaction parity (oracle)

To guarantee the audit catches everything the runtime `Redactor` masks at write
time — not just the high-confidence provider shapes above — the configured
redactor is also run over each artifact as a **detection oracle**. Anything it
would have masked is reported, covering classes the provider registry does not:

| `match_class` | What it catches | Severity |
|---------------|-----------------|----------|
| `bearer_token` | `Bearer <token>` header values | high |
| `sensitive_env_assignment` | any sensitive `NAME=value` assignment (e.g. `DATABASE_PASSWORD=…`, `GITHUB_TOKEN=…`) | medium |
| `sensitive_env_value` | a value matching a sensitive variable from the **audit process's own environment**, found verbatim in an artifact | high |
| `sensitive_json_value` | a string value under a sensitive JSON key (`password`, `*_token`, `*key`, …) regardless of value shape, mirroring `Redactor::redact_json_value` | medium |
| `configured_literal` / `configured_custom_pattern` | the operator's `secret_literals` / `custom_patterns` | high |

The structured provider detectors (`pem`, `api_key`, `github_token`, …) are kept
as the precise, named layer; the oracle only *adds* the classes the registry
lacks (bearer / env-assignment / env-value / sensitive-JSON-key). Env-assignment
and JSON-key findings are `medium` severity — they still drive the failing exit
code (32) and the publish gate, but are distinguished from unambiguous
provider-key leaks. Non-sensitive assignments (`PATH=…`, `HOME=…`) are not
flagged, preserving the false-positive budget.

### Entropy heuristic

The `entropy` detector flags tokens >= 32 chars over a secret-ish alphabet with
Shannon entropy >= 4.0 bits/char and at least two of {lowercase, uppercase,
digit}. It is deliberately conservative to respect the false-positive budget and
is gated by `--disable-entropy`. Entropy findings have severity `entropy_only`
and **never** cause a failing exit code on their own.

Overlapping matches are de-duplicated deterministically (earliest start, longest
span, highest severity), so a structured detector wins over the entropy
heuristic on the same span.

## Output

### JSON report (`redact_audit.json`)

Schema-versioned per [artifact-contract.md](artifact-contract.md):

```json
{
  "artifact_kind": "redact_audit",
  "schema_version": { "major": 1, "minor": 0 },
  "scanned_dir": "runs/my-sweep",
  "files_scanned": 12,
  "lines_scanned": 3480,
  "detectors": ["aws", "gcp", "..."],
  "findings": [
    {
      "file": "inst-1.traj.json",
      "byte_offset": 2048,
      "line": 17,
      "detector_id": "github",
      "severity": "high",
      "match_class": "github_pat",
      "match_fingerprint": "9f2a1c0b4d6e",
      "preview": "token=[REDACTED:github_pat:medium:9f2a1c0b] in step output",
      "is_new": true
    }
  ],
  "scan_errors": [],
  "summary": {
    "total": 1, "high": 1, "medium": 0, "entropy_only": 0, "new_findings": 1
  }
}
```

Findings are ordered by `(file, byte_offset, detector_id, match_class)` so the
report is byte-for-byte deterministic across runs.

### Preview marker scheme

Previews reuse the runtime redactor's format `[REDACTED:KIND:SIZE:HASH]`, with
one deliberate difference: the hash is an **unsalted** SHA-256 content digest
(not the per-process salted hash). This keeps the report deterministic and makes
`--baseline` diffing stable across runs. `match_fingerprint` is the same
unsalted digest (12 hex) and is non-reversible — no raw secret is ever emitted.

## Exit codes

| Code | Name | Meaning |
|------|------|---------|
| `0` | `success` | No new findings at medium+ severity, and no scan errors. |
| `32` | `redact_audit_findings` | At least one **new** finding at `medium`+ severity. |
| `33` | `redact_audit_scan_error` | A file/bundle could not be read or extracted. |

> **On the issue's "0/1/2".** Issue #342 described exit codes 0/1/2. This repo
> uses a single stable `ExitCode` enum where `1 = internal_error` and
> `2 = usage_error` are reserved and every command-specific outcome gets its own
> number (e.g. `redact-check` uses 25/26). To stay consistent with that contract
> and keep automation routing unambiguous, `redact-audit` uses dedicated codes
> 32/33. The behaviour the issue asked for is preserved: clean = 0, findings =
> non-zero, scan error = a distinct non-zero — so `redact-audit && publish`
> still gates correctly.

Findings take precedence over scan errors (a real leak is the headline). With
`--baseline`, only findings **absent** from the baseline count toward exit 32.

## Success metric

Measured by the integration test in `tests/redact_audit.rs`:

- **Recall >= 95%** (>= 19 of 20 planted secrets across the detector classes).
- **<= 2 false positives / 1000 lines** on clean artifacts (the test asserts
  zero `high`/`medium` findings on a secret-free trajectory and tool output).

## Out of scope

- Rewriting / re-redacting artifacts in place (this command detects only).
- Semantic PII detection (names, addresses).
- Network-egress, license, or malware scanning.
- Real-time stream scanning (covered by the runtime redactor).
- Custom user regex packs (use `custom_patterns`, which this command audits).

## See also

- `docs/spec-secret-redaction.md` — the runtime redactor and marker scheme.
- `docs/exit-codes.md` — the full exit-code contract.
- `docs/artifact-contract.md` — artifact schema-versioning rules.
