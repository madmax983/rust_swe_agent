# spec-assert: `bench assert` SLO Gate

## Overview

`bench assert` is a **read-only, post-sweep CI primitive** that evaluates a
declared set of pass/fail thresholds against a completed sweep's artifacts and
exits 0 only when all rules pass.

It never re-runs instances, never calls a model, and never mutates the sweep
directory (except to write `assertions.json` next to `results.json`).

```
max bench assert \
  --sweep runs/ \
  --rules ci/my-slos.toml
```

Or with inline shorthand:

```
max bench assert \
  --sweep runs/ \
  --rule "resolved_rate>=0.38" \
  --rule "total_cost_usd<=50.00"
```

## Motivation

The harness today has ad-hoc gates (`errored == 0` in the nightly workflow,
`bench compare --max-regressions`) but no single declarative way for an
operator to answer: "Did this sweep meet my published bar?"

`bench assert` is the missing primitive. Because it reads the same structured
artifacts that every other bench command produces, **any** metric that appears
in `results.json` or `evaluation.json` can become a CI gate without writing
bash glue.

---

## Rule File (TOML)

Rules are declared in a TOML file with one `[[rule]]` table per assertion:

```toml
# ci/my-slos.toml

[[rule]]
name = "resolved_rate_floor"
metric = "resolved_rate"
op = ">="
threshold = 0.38

[[rule]]
name = "cost_ceiling"
metric = "total_cost_usd"
op = "<="
threshold = 50.00

[[rule]]
name = "step_cap_floor"
metric = "at_cap_count[steps]"
op = "<="
threshold = 5
```

Each rule has four required fields:

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Human-readable label shown in output and `assertions.json`. |
| `metric` | string | One of the closed vocabulary below. |
| `op` | string | One of `==`, `!=`, `<`, `<=`, `>`, `>=`. |
| `threshold` | number | Numeric threshold. Ratios / shares are 0.0–1.0; USD has up to 4 decimal places. |

Comments (`#`) are supported anywhere in the file (standard TOML).

---

## Inline Shorthand (`--rule`)

For one-off or scripted use, rules can be passed inline:

```
--rule resolved_rate>=0.38 --rule total_cost_usd<=50.00
```

The metric key becomes the rule name. `--rule` and `--rules` are mutually
exclusive.

---

## Metric Vocabulary (v1)

All metrics are **point estimates** from already-computed sweep artifacts.
`bench assert` never re-aggregates raw trajectories.

| Metric | Source file | Field / derivation |
|--------|------------|-------------------|
| `resolved_rate` | `evaluation.json` | `resolved_count / total_instances` (instances missing from evaluation.json count against the denominator) |
| `resolved_count` | `evaluation.json` | Count of instances where `resolved == true` |
| `unresolved_count` | `evaluation.json` | Count of instances where `resolved == false` |
| `errored_count` | `results.json` | `.errored` |
| `total_cost_usd` | `results.json` | `.total_cost_usd` |
| `mean_cost_per_instance_usd` | `results.json` | `.total_cost_usd / .total` |
| `cost_per_resolved_instance_usd` | `results.json` + `evaluation.json` | `.total_cost_usd / resolved_count` |
| `mean_steps` | `results.json` | Mean of `.instances[].steps` |
| `p95_steps` | `results.json` | 95th-percentile of `.instances[].steps` |
| `wallclock_total_s` | `results.json` | `.manifest.runtime.finished_at_utc − .manifest.runtime.started_at_utc` (seconds) |
| `failure_category_count[<cat>]` | `results.json` | `.failures_by_category.<cat>` (0 if absent) |
| `failure_category_share[<cat>]` | `results.json` | `.failures_by_category.<cat> / .total` |
| `at_cap_count[steps]` | `results.json` | Count of instances where `exit_reason == "step_limit"` |
| `at_cap_count[cost]` | `results.json` | Count of instances where `exit_reason == "budget_halt"` |
| `at_cap_count[wallclock]` | `results.json` | Count of instances where `exit_reason == "wallclock_timeout"` |

**Contract:** one metric, one canonical source file, one canonical field path.
`bench assert` never re-derives or re-aggregates beyond the derivations listed
above.

---

## Exit Codes

| Code | Class | When emitted |
|------|-------|-------------|
| 0 | `success` | All rules passed (and all required artifacts were present). |
| 27 | `slo_rule_failure` | At least one rule failed, OR at least one artifact was missing in fail-closed mode. The command ran correctly and wrote `assertions.json`. |
| 2 | `usage_error` | Bad invocation: unknown metric name, invalid operator, unparseable threshold, missing `--sweep`, empty rule set, malformed rule file. |
| 1 | `internal_error` | I/O failure, JSON parse error. |

See `docs/exit-codes.md` for the full stable contract.

---

## Standard Output

By default: one header line + one line per **failed** rule.

```
bench assert: runs/ — 5 passed, 2 failed
FAILED resolved_rate_floor: resolved_rate>=0.38: observed 0.250000 (evaluation.json: resolved instances / total instances)
FAILED cost_ceiling: total_cost_usd<=50.00: observed 75.230000 (results.json: .total_cost_usd)
```

With `--verbose`: every rule is printed (PASSED rules listed individually).

When skips occur (--allow-missing-artifacts), the header shows:
```
bench assert: runs/ — 4 passed, 1 failed, 1 skipped
```

---

## Output Artifact: `assertions.json`

Written to `<sweep>/assertions.json` alongside `results.json`.

```json
{
  "artifact_kind": "assertions",
  "schema_version": { "major": 1, "minor": 0 },
  "generated_at": "2026-05-01T08:00:00Z",
  "sweep_id": "abc123",
  "rules": [
    {
      "name": "resolved_rate_floor",
      "metric": "resolved_rate",
      "op": ">=",
      "threshold": 0.38,
      "observed_value": 0.5,
      "passed": true,
      "source_field_path": "evaluation.json: resolved instances / total instances",
      "reason_if_failed_or_skipped": null
    },
    {
      "name": "errored_zero",
      "metric": "errored_count",
      "op": "==",
      "threshold": 0.0,
      "observed_value": null,
      "passed": null,
      "source_field_path": "results.json: .errored",
      "reason_if_failed_or_skipped": "missing_artifact: evaluation.json"
    }
  ],
  "passed": false,
  "passed_count": 1,
  "failed_count": 0,
  "skipped_count": 1
}
```

Each rule record:

| Field | Description |
|-------|-------------|
| `name` | Rule name from the rule file or the metric key for inline rules. |
| `metric` | Metric name (including bracket parameter for parametric metrics). |
| `op` | Comparison operator string. |
| `threshold` | Declared threshold value. |
| `observed_value` | Extracted metric value. `null` when the rule was skipped. |
| `passed` | `true`, `false`, or `null` (skipped). |
| `source_field_path` | Canonical source location string for the metric. |
| `reason_if_failed_or_skipped` | Human-readable reason string when `passed` is `false` or `null`; `null` otherwise. |

**Redaction:** `assertions.json` only contains aggregate metric values, not
instance content. It passes through the existing redaction pipeline
(`docs/spec-secret-redaction.md`) without modification.

**Determinism:** running `bench assert` twice on the same sweep with the same
rules produces byte-identical `assertions.json` modulo `generated_at`.

---

## Missing Artifact Behavior

When a rule requires `evaluation.json` and the file is absent:

- **Default (fail-closed):** the rule record has `passed: null` and
  `reason_if_failed_or_skipped: "missing_artifact: evaluation.json"`. The rule
  counts as a failure for the exit-code determination. This is the safe default:
  operators who want `bench assert` to gate their CI should know when the
  evaluator hasn't run.

- **With `--allow-missing-artifacts`:** the rule record is the same (passed:
  null, reason set), but the rule is treated as a **skip** rather than a
  failure. The exit code is 0 if there are no `false` rules. Use this in
  pipelines where evaluation is optional.

---

## Nightly Smoke Wiring

`.github/workflows/swe-bench-nightly.yml` uses `bench assert` with a
checked-in rule file (`ci/nightly-smoke.assert.toml`) that asserts at minimum:

```toml
[[rule]]
name = "errored_zero"
metric = "errored_count"
op = "=="
threshold = 0

[[rule]]
name = "resolved_floor"
metric = "resolved_count"
op = ">="
threshold = 1
```

This graduates the smoke from "did it catch fire?" (`errored == 0`) to "did it
resolve at least one instance?", catching regressions where the harness
silently unresolved instances without erroring.

The auto-filed `nightly-smoke` issue body reads the failed-rule list out of
`assertions.json` to populate the failure digest.

---

## Non-Goals

- **Statistical significance gates** (e.g., "resolved-rate is significantly
  higher than baseline"). Follow-up on #176.
- **Cross-sweep rules** (e.g., "resolved-rate did not drop more than 2 pp vs
  the last 3 sweeps"). See #264 and #270.
- **Auto-suggesting rules** from a sweep's distributions.
- **Hardcoded SLOs.** Every threshold is operator-declared.
- **Slack/PagerDuty integration.** Exit codes + `assertions.json` are the
  integration surface.
- **Closed-loop remediation** (auto-retry, auto-tune caps based on failures).
- **Rule composition** (AND/OR/NOT trees). v1 is a flat conjunction.

---

## Fixture Tests

The integration tests in `tests/bench_assert.rs` cover:

| Test | AC |
|------|-----|
| `all_rules_pass_exits_0_and_writes_passed_true` | (a) |
| `one_rule_fails_exits_27` | (b) |
| `one_rule_fails_assertions_json_records_failure` | (b) |
| `unknown_metric_exits_usage_error_not_rule_failure` | (c) |
| `missing_evaluation_json_fails_closed_by_default` | (d) |
| `missing_evaluation_json_with_allow_missing_skips_and_exits_0` | (d) |
| `inline_rule_and_file_rules_produce_identical_output` | (e) |
| `two_runs_produce_same_artifact_modulo_generated_at` | (f) |
| `all_v1_metrics_can_be_evaluated` | (g) |

Fixture sweep data is in `tests/fixtures/assert/sweep/`.

---

## See Also

- `docs/exit-codes.md` — stable exit-code contract
- `docs/spec-secret-redaction.md` — redaction pipeline
- `docs/artifact-contract.md` — artifact schema versioning
- Issue #308 — original specification
