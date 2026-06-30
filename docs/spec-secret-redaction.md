# Secret Redaction Contract

`max` redacts secrets by default before content reaches model-visible observations, saved trajectories, live streams, `bench inspect`, Markdown/CSV/Mermaid exports, GitHub PR text built from artifacts, and SWE-bench prediction files.

## Threat Model

The redactor is designed for local and CI agent runs where useful debugging artifacts may be shared with teammates, issues, PRs, or evaluation tooling. It masks:

* configured literal secrets for the run;
* custom regex pattern matches for the run;
* current-process environment variable values whose names contain `TOKEN`, `SECRET`, `KEY`, `PASSWORD`, or `CREDENTIAL`;
* common structured shapes such as bearer tokens, GitHub tokens, API keys, and PEM private-key blocks;
* `.env`-style assignments whose names look sensitive.

Markers are stable within a run, so repeated occurrences of the same secret get the same marker. The marker uses a salted digest and a coarse size class (`short`, `medium`, `long`); it does not include the raw value or exact length.

## Configuration

```toml
[redaction]
enabled = true
secret_literals = ["actual-token-value"]
custom_patterns = ["INTERNAL-TOKEN-[A-Za-z0-9]{24}"]
unsafe_allow_secret_leaks = false
```

Disabling redaction is a run-time decision (`enabled = false` in the run config). `bench inspect` has no flag to print raw secrets from stored artifacts.

If a submitted patch or prediction artifact contains a configured literal or structured secret shape, the run is downgraded to `failure_category = "secret_leak_detected"` unless `unsafe_allow_secret_leaks = true`.  See [`docs/failure-categories.md`](failure-categories.md) for the full `secret_leak_detected` runbook.

## Verification Check Output

When operator-supplied verification checks are run after the agent finishes (see `--verify`), the stdout and stderr of each check are captured as bounded previews and saved in the trajectory artifact. `bench inspect` applies the same view-time redactor to these previews before display.

**Warning:** verification command output is subject to the same redaction contract as agent observations, but only configured literals, structured secret shapes, and current-process environment variables are masked. If a verification command prints a secret that does not match any of those patterns (e.g. a raw password from a test fixture file), it will appear in plain text in the trajectory artifact and `bench inspect` output. Until issue #86 (comprehensive DLP) is complete, do not run verification commands that may print secrets that fall outside the configured redaction patterns.

## Verifying your redaction config

Use `agent redact-check` to confirm that your configured literals, custom regex patterns,
and structured shapes actually match a representative payload before paying for a sweep.
The command makes no model call and launches no environment.

```bash
# Check a literal string
max agent redact-check --text "token: ghp_AAAAAAAAAAAAAAAAAAAAAA"

# Check a file (e.g. a shell env dump or log snippet)
max agent redact-check --file /tmp/env-dump.txt

# Audit an existing trajectory artifact without mutating it
max agent redact-check --trajectory ./runs/my-sweep/i1/trajectory.json

# Pipe from stdin
env | max agent redact-check

# Machine-readable output with byte offsets
max agent redact-check --text "..." --json

# Fail CI if any custom_patterns entry had zero matches
max agent redact-check --file sample.log --strict
```

### Input sources

Exactly one source must be provided; supplying multiple is a usage error.

| Flag | Description |
|------|-------------|
| `--text TEXT` | Literal string on the command line |
| `--file PATH` | Path to a file |
| `--trajectory PATH` | Trajectory artifact (read-only; same view-time pass as `bench inspect`) |
| *(none)* | Read from stdin |

### Output formats

**Human (default):** Shows each match with byte range, source label, and the stable
redaction marker.  The redacted output follows.  Raw secret values are never printed.

**JSON (`--json`):** Emits a stable structured document.  Schema:

```json
{
  "artifact_kind": "redact_check",
  "schema_version": { "major": 1, "minor": 0 },
  "matches": [
    {
      "start": 7,
      "end": 47,
      "marker": "[REDACTED:github_token:medium:abc123def456]",
      "source": "structured:github_token"
    }
  ],
  "unmatched_literal_indices": [],
  "unmatched_pattern_indices": [],
  "redacted": "token: [REDACTED:github_token:medium:abc123def456]"
}
```

Source label values:

| Source label | Meaning |
|---|---|
| `literal` | Matched a `secret_literals` entry |
| `custom_pattern[N]` | Matched `custom_patterns[N]` (0-based index) |
| `structured:pem` | PEM private-key block |
| `structured:bearer` | `Bearer <token>` header value |
| `structured:github_token` | GitHub token (`ghp_*`, `github_pat_*`) |
| `structured:api_key` | API key (`sk-*`, `sk-ant-*`, `AKIA*`) |
| `structured:env_assignment` | `.env`-style assignment with a sensitive name |
| `env:NAME` | Current-process env var whose name looks sensitive |

Source labels **never** contain raw secret values.  For env vars, the label references
the variable *name* only; for custom patterns, the *index* only.

### Exit codes

| Code | Outcome class | Meaning |
|------|--------------|---------|
| `0` | `success` | All configured `secret_literals` entries matched at least once (or none were configured) |
| `25` | `redact_check_stale_literals` | One or more `secret_literals` entries produced **zero** matches — likely stale |
| `26` | `redact_check_strict_fail` | `--strict`: one or more `custom_patterns` entries produced zero matches |

Exit 26 takes priority over exit 25 when both conditions hold.

### `--strict` flag

Pass `--strict` to additionally exit non-zero if any compiled entry in `custom_patterns`
produced zero matches.  Useful in CI to catch a regex typo or a renamed token format
before burning model spend:

```bash
max agent redact-check --file sample.log --strict || exit 1
```

## Limitations

This is deterministic masking for known values and structured secrets, not an enterprise DLP system. It does not provide semantic PII detection, license scanning, retroactive rewriting of old artifacts, or a guarantee that a model cannot infer a secret from surrounding context.
