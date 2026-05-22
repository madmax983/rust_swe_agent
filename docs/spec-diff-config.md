# Configuration Diffing and Drift Detection (`bench diff-config`)

## Overview

The `bench diff-config` command enables developers and CI pipelines to compare the provenance and configuration `manifest` block of two different sweeps (baseline and candidate) and identify any configuration drift.

To maintain sweep integrity and reproducibility, this command recursively flattens and deep-compares all configuration groups, supports advanced exclusion lists (`--ignore`), and provides deterministic gating with stable exit codes.

---

## Quick-Start Examples

### 1. Simple Text Comparison (Default)

Compare two sweep directories to see a structured text diff:

```bash
bench diff-config --baseline runs/sweep-sonnet-v1 --candidate runs/sweep-sonnet-v2
```

Example Output:
```text
HEADLINE: model.name changed

[harness] identical
[dataset] identical
[prompt_template] identical
[model]
  .name: "anthropic/claude-3-5-sonnet" → "anthropic/claude-3-7-sonnet"
[sampling] identical
[tools] identical
[hooks] identical
[limits] identical
[config.resolved] identical
[cli.argv] identical
```

### 2. Machine-Readable JSON Report

Request a structured JSON output for scriptability and automation:

```bash
bench diff-config --baseline runs/sweep-v1 --candidate runs/sweep-v2 --format json
```

Example Output:
```json
{
  "schema_version": "1.0.0",
  "generated_at": "2026-05-21T23:59:00Z",
  "summary": {
    "identical": false,
    "changed_field_count": 1,
    "ignored_field_count": 0
  },
  "compared_fields": [
    { "group": "harness", "path": ".name" },
    ...
  ],
  "changed_fields": [
    {
      "group": "model",
      "path": ".name",
      "baseline": "anthropic/claude-3-5-sonnet",
      "candidate": "anthropic/claude-3-7-sonnet"
    }
  ],
  "unchanged_groups": [
    "harness",
    "dataset",
    "prompt_template",
    "sampling",
    "tools",
    "hooks",
    "limits",
    "config.resolved",
    "cli.argv"
  ],
  "ignored_fields": []
}
```

### 3. Gating CI on Configuration Drift

Prevent prompt template or model changes from drifting without explicit approval. Excludes logging config level drift using `--ignore`:

```bash
bench diff-config \
  --baseline runs/prod-baseline \
  --candidate runs/ci-candidate \
  --fail-on-change \
  --ignore config.resolved.logging.level,harness.version
```

If drift is detected in non-ignored areas, the command prints the changes, outputs `outcome_class: preflight_failure` to stderr, and exits with code `3`.

---

## Comparison Details & Ordering

`bench diff-config` compares exactly 10 configuration groups in a fixed recursive order:

1. **`harness`**: compares `name`, `version`, `git_sha`, and `git_dirty`.
2. **`dataset`**: compares `path`, `sha256`, `instance_count`, and deep-compares `filter_spec` (including all key-value leaves).
3. **`prompt_template`**: compares `source`, `path`, and `sha256`.
4. **`model`**: compares `name`, `backend`, `backend_version`, and `base_url`.
5. **`sampling`**: deep-compares all sampling parameter leaves.
6. **`tools`**: deep-compares all tools (names, versions, and configurations).
7. **`hooks`**: compares hook paths and hashes.
8. **`limits`**: compares `step_limit`, `per_task_budget_usd`, `task_timeout_secs`, and `sweep_cost_limit_usd`.
9. **`config.resolved`**: parses the resolved TOML configuration and recursively compares all fields via stable JSON paths.
10. **`cli.argv`**: compares the command-line argument tokens sequentially.

### Redaction Safety

Secret values and API keys are redacted inside the sweep manifests (represented as `"<redacted>"`). `bench diff-config` compares these tokens literally:
- Identical `"<redacted>"` values on both sides **do not** trigger a configuration change.
- One-sided redactions or differing unredacted values correctly trigger drift.

---

## Priority Operator Headlines (Text Mode)

To help operators quickly evaluate the severity of config drift, the text mode outputs a top-level `HEADLINE:` block sorted by importance:

| Priority | Group / Path | Example Headline |
|---|---|---|
| 1 | `prompt_template.sha256` | `HEADLINE: prompt_template.sha256 changed` |
| 2 | `model.name` | `HEADLINE: model.name changed` |
| 3 | `harness.git_sha` | `HEADLINE: harness.git_sha changed` |
| 4 | `limits.*` | `HEADLINE: limits changed` |
| 5 | `tools` | `HEADLINE: tools changed` |
| 6 | `hooks` | `HEADLINE: hooks changed` |
| 7 | All other fields | `HEADLINE: other configuration drift detected` |

---

## Ignore Paths Matching Rules

The `--ignore` flag takes a comma-separated list of paths or groups. Fields are ignored if:
- They match an exact group name (e.g. `harness`).
- They match a specific dot-separated path (e.g. `config.resolved.logging.level`).
- They match a dotted path prefix (e.g. `dataset.filter_spec`).

Ignored fields are gathered into `ignored_fields` / `ignored:` footer, and do not trigger exit code `3` even when `--fail-on-change` is active.

---

## Exit Codes Reference

| Exit Code | Classification | Trigger Condition |
|---|---|---|
| **`0`** | Success | Manifests are identical or all changes are covered by `--ignore` rules. |
| **`2`** | Usage / Validation Error | Missing `results.json`, missing `manifest` block, or invalid syntax. |
| **`3`** | Preflight / Gate Failure | `--fail-on-change` is set and non-ignored changes exist. |

---

## Integration with `bench compare`

When running `bench compare` between two sweeps that both have manifests, the manifest delta section will automatically append details pointing to the new tool:

```text
Manifest delta: (run `bench diff-config` for details)
  - model.name: anthropic/claude-3-5-sonnet -> anthropic/claude-3-7-sonnet
```
