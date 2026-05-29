# `bench export-ci` — CI Export: JUnit XML and GitHub Actions Annotations

Converts a completed SWE-bench sweep directory into standard CI formats so that
unresolved instances surface inline on pull-request checks and in CI dashboards,
**with zero lines of custom JSON-parsing glue** in the workflow YAML.

## Usage

```
max bench export-ci --sweep <SWEEP_DIR> [--format <FORMAT>] [--output <PATH>]
```

### Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--sweep PATH` | required | Completed sweep directory (`results.json` must exist). |
| `--format FORMAT` | `junit` | Output format: `junit`, `github-annotations`, or `both`. |
| `--output PATH` | `<sweep>/junit.xml` | Output path for the JUnit XML file. Ignored when `--format github-annotations`. |

## Output formats

### `--format junit`

Writes a JUnit XML file that validates against the Jenkins JUnit schema.

```xml
<?xml version="1.0" encoding="UTF-8"?>
<testsuites tests="N" failures="F" errors="E" time="T.T">
  <testsuite name="swe-bench" tests="N" failures="F" errors="E" time="T.T">
    <!-- Resolved instance: clean testcase -->
    <testcase name="django__django-001" classname="django.django" time="12.000"/>
    <!-- Unresolved instance: failure element -->
    <testcase name="django__django-002" classname="django.django" time="30.000" file="django__django-002/run-1.traj.json">
      <failure message="step_limit">agent ran out of steps on a complex migration</failure>
    </testcase>
  </testsuite>
</testsuites>
```

**Testcase fields:**

| Field | Value |
|-------|-------|
| `name` | `instance_id` |
| `classname` | repo name parsed from the instance ID (`owner__repo-NNNN` → `owner.repo`) |
| `time` | wall-clock seconds for the instance trajectory |
| `file` | trajectory path relative to the sweep directory (only on non-resolved testcases) |

**Aggregate attributes** (`tests`, `failures`, `errors`, `time`) are validated against
the counts in `results.json`. A mismatch exits with **code 22** (artifact integrity
violation) to surface corrupt or truncated sweep artifacts before they silently
produce misleading CI results.

- `tests` = `results.json → total`
- `failures` = submitted but unresolved instances
- `errors` = errored instances (`results.json → errored`)
- `time` = sum of `duration_secs` across all instances

### `--format github-annotations`

Writes GitHub Actions [workflow command annotations](https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/workflow-commands-for-github-actions) to stdout — one `::error` line per unresolved instance:

```
::error title=django__django-002,file=django__django-002/run-1.traj.json::step_limit
::error title=django__django-003,file=django__django-003/run-1.traj.json::env_setup: environment setup failed
```

GitHub Actions renders these as inline annotations on the PR diff and check summary
when the `file=` path matches a changed file in the PR.

**Annotation fields:**

| Field | Value |
|-------|-------|
| `title` | `instance_id` (stable; suitable for filtering in notification rules) |
| `file` | trajectory path relative to sweep directory |
| message | `failure_category: excerpt` — the triage cluster category if `triage.json` is present, otherwise the instance's `failure_category`, falling back to `"unresolved"` |

### `--format both`

Writes JUnit XML to file **and** emits GitHub Actions annotations to stdout in one
invocation. The file path defaults to `<sweep>/junit.xml`, overridable with `--output`.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Export succeeded. The CI system's own gating logic decides what to do with the results. `bench export-ci` never gates — use `bench compare` for regression gating. |
| `2` | Usage error (missing `--sweep`, unknown `--format`, etc.). |
| `22` | Artifact integrity violation: the JUnit aggregate attributes do not match `results.json`. Both the XML and annotations were still written for inspection. |

**Important:** exit code `0` is returned regardless of the sweep's resolved rate.
`bench export-ci` is a reporting command, not a gating command. Wire `bench compare`
or your CI system's own threshold logic for merge-blocking.

## Secret redaction

All text excerpts in JUnit failure bodies and GitHub annotation messages pass through
the same secret-redaction pipeline as `bench bundle` and `bench inspect`. Configured
secret literals, structured token shapes (GitHub tokens, bearer tokens, API keys,
PEM private-key blocks), and sensitive environment variable values are replaced with
stable digest markers before any output is written.

See [`docs/spec-secret-redaction.md`](spec-secret-redaction.md) for the full contract.

## Triage integration

When `triage.json` is present in the sweep directory (produced by `bench triage`),
`bench export-ci` uses the cluster's `failure_category` as the failure message for
each instance, providing structured triage categories in the CI surface rather than
a generic "unresolved" label.

Run `bench triage` before `bench export-ci` to get the richest possible CI output:

```bash
max bench triage --sweep ./sweep-out
max bench export-ci --sweep ./sweep-out --format both
```

## GitHub Actions quickstart

```yaml
- name: Run SWE-bench sweep
  run: |
    cargo run --release -- bench swebench \
      --dataset-path ./data/instances.jsonl \
      --output ./sweep-out \
      ...

- name: Triage failures
  run: cargo run --release -- bench triage --sweep ./sweep-out

- name: Export CI artifacts
  run: |
    cargo run --release -- bench export-ci \
      --sweep ./sweep-out \
      --format both \
      --output ./sweep-out/junit.xml

- name: Upload JUnit XML
  uses: actions/upload-artifact@v4
  with:
    name: swe-bench-junit
    path: ./sweep-out/junit.xml

- name: Publish JUnit results
  uses: mikepenz/action-junit-report@v5
  if: always()
  with:
    report_paths: './sweep-out/junit.xml'
```

This ten-line snippet (excluding the sweep step itself) surfaces every unresolved
SWE-bench instance as an inline PR check annotation with zero custom JSON-parsing glue.

## Zero-cost guarantee

`bench export-ci` reads only on-disk artifacts:

- `results.json` — aggregate sweep results
- `triage.json` — optional failure-cluster data (produced by `bench triage`)

No model calls, no network access, no environment setup required.

## Out of scope

- SARIF output (security-tooling-flavored; lower CI integrator demand)
- GitLab / Bitbucket annotation formats (additive; GitHub Actions covers the dominant case)
- Streaming export from in-flight sweeps — works on a completed sweep only
- Gating / merge-blocking — composability principle: `export-ci` reports, `bench compare` gates
