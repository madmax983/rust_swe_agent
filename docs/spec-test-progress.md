# spec-test-progress

`bench test-progress` produces a **partial-credit score** for every instance
in a completed sweep, aggregates those scores into sweep-wide means, and
identifies which specific tests are most often left failing or newly regressed
across the sweep.

---

## Why partial credit?

The standard SWE-bench binary verdict (`resolved` / `unresolved`) throws away
gradient.  A prompt iteration that moves an instance from 0 of 3 target tests
passing to 2 of 3 is real progress, but `resolved-rate` reports 0 → 0.
`bench test-progress` surfaces that signal.

---

## Data sources

The command reads **two on-disk artifacts** that already exist after a
completed sweep + evaluate run.  It never calls a model or re-runs the
evaluator.

| File | Contents |
|------|----------|
| `evaluation.json` | Per-instance `tests_passed` and `tests_failed` from the evaluator |
| `dataset.jsonl` | Per-instance `FAIL_TO_PASS` and `PASS_TO_PASS` expected test lists |

---

## `partial_credit_score` formula

```
partial_credit_score = clamp(
    fail_to_pass.passed_ratio - pass_to_pass.regressed_ratio,
    -1.0,
    1.0
)
```

Where:
- `fail_to_pass.passed_ratio = fail_to_pass.passed_count / fail_to_pass.total`
- `pass_to_pass.regressed_ratio = pass_to_pass.regressed_count / pass_to_pass.total`
- When `fail_to_pass.total == 0`, `passed_ratio = 1.0` (vacuously all passed)
- When `pass_to_pass.total == 0`, `regressed_ratio = 0.0` (no regressions possible)

### Worked examples

| Scenario | ftp_passed_ratio | ptp_regressed_ratio | score | bucket |
|---|---|---|---|---|
| All FAIL_TO_PASS fixed, none regressed | 1.0 | 0.0 | **1.0** | `resolved` |
| 2/3 fixed, 0/2 regressed | 0.667 | 0.0 | **0.667** | `partial_progress` |
| 0/3 fixed, 0/2 regressed | 0.0 | 0.0 | **0.0** | `no_progress` |
| 0/3 fixed, 2/5 regressed | 0.0 | 0.4 | **-0.4** | `regressed` |
| 0/3 fixed, 5/5 regressed | 0.0 | 1.0 | **-1.0** | `regressed` |
| patch_apply_failed — no test data | — | — | **0.0** | `evaluator_unavailable` |

---

## Verdict buckets

Mutually exclusive and exhaustive.

| Bucket | Condition |
|--------|-----------|
| `resolved` | Evaluator reports `resolved = true` (all FAIL_TO_PASS pass, zero regressions) |
| `partial_progress` | `partial_credit_score > 0` and not resolved |
| `no_progress` | `partial_credit_score == 0` |
| `regressed` | `partial_credit_score < 0` |
| `evaluator_unavailable` | `eval_exit_reason` is `patch_apply_failed`, `eval_error`, or `skipped_no_patch` — per-test data not available |

---

## Output artifact: `test-progress.json`

Written to `<sweep>/test-progress.json`.

### Schema (version 1)

```jsonc
{
  "schema_version": 1,           // integer, bumped on breaking changes
  "generated_at": "...",         // ISO-8601 UTC; only field that varies between runs
  "sweep": "/path/to/sweep",     // sweep directory path
  "sweep_signature": "...",      // SHA-256 of the ProvenanceManifest (null if absent)
  "totals": {
    "instance_count": 50,
    "per_bucket": {              // count per verdict bucket
      "resolved": 5,
      "partial_progress": 12,
      "no_progress": 18,
      "regressed": 3,
      "evaluator_unavailable": 12
    },
    "per_bucket_share": {        // fraction of total instances
      "resolved": 0.10,
      ...
    },
    "evaluator_unavailable_count": 12,
    "mean_partial_credit_score": 0.18,        // over non-unavailable instances
    "mean_fail_to_pass_passed_ratio": 0.23,
    "mean_pass_to_pass_regressed_ratio": 0.05
  },
  "per_instance": [              // sorted ascending by partial_credit_score, ties by instance_id
    {
      "instance_id": "django__django-12345",
      "verdict_bucket": "partial_progress",
      "partial_credit_score": 0.667,
      "fail_to_pass": {
        "total": 3,
        "passed_count": 2,
        "passed_ratio": 0.667
      },
      "pass_to_pass": {
        "total": 2,
        "regressed_count": 0,
        "regressed_ratio": 0.0
      },
      "outcome": "submitted",
      "excluded_from_means": false   // true when --min-tests dropped this instance
    }
  ],
  "hot_failing_tests": [         // top N FAIL_TO_PASS tests still failing across sweep
    { "test_name": "test_foo", "instance_count": 12 }
  ],
  "hot_regressed_tests": [       // top N PASS_TO_PASS tests newly broken across sweep
    { "test_name": "test_bar", "instance_count": 4 }
  ],
  "redaction_applied": false
}
```

---

## `hot_failing_tests` and `hot_regressed_tests`

- **`hot_failing_tests`**: FAIL_TO_PASS test names that remained failing, ranked
  by the number of instances where they failed. Controlled by `--hot-tests-n`
  (default 20). Use this list to spot tests that no instance solved — likely an
  evaluator-config or dataset-side problem.
- **`hot_regressed_tests`**: PASS_TO_PASS test names that newly broke, ranked
  by instance count. A test that regresses in 90% of unresolved instances is a
  fingerprint of a specific failure mode worth a triage issue.

`evaluator_unavailable` instances are excluded from both lists.
Ties within a list are broken by test name lexicographic order.

---

## `bench compare` integration

When `test-progress.json` is present in **both** sweep directories, `bench compare`
appends a **Test progress delta** section to its text output:

```
--- Test progress delta ---
mean_partial_credit_score: 0.1800 -> 0.3400 (+0.1600)
mean_fail_to_pass_passed_ratio: 0.2300 -> 0.4100 (+0.1800)
mean_pass_to_pass_regressed_ratio: 0.0500 -> 0.0700 (+0.0200)

Bucket counts (baseline -> candidate):
  resolved: 5 -> 5 (+0)
  partial_progress: 12 -> 20 (+8)
  no_progress: 18 -> 10 (-8)
  regressed: 3 -> 3 (+0)
  evaluator_unavailable: 12 -> 12 (+0)
```

When `test-progress.json` is absent on either side the section is **omitted
silently** — no error, no warning.  This preserves backward compatibility with
sweeps that predate this command.

---

## CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--sweep <DIR>` | required | Completed sweep directory |
| `--format text\|json` | `text` | Output format |
| `--bucket <BUCKET>` | — | Restrict text table to one verdict bucket |
| `--hot-tests-n <N>` | 20 | Size of both hot-test lists |
| `--filter <KEY=VALUE>` | — | Filter instances (same syntax as `bench inspect --filter`) |
| `--min-tests <N>` | 0 | Exclude from means any instance with fewer than N total tests |

---

## Exit codes

Follows the project-wide stable contract (`src/exit_code.rs`):

| Code | Condition |
|------|-----------|
| 0 | Success — even if all instances are `evaluator_unavailable` or `regressed` |
| 1 | Internal I/O error (unreadable files, JSON parse failure) |
| 2 | Invalid flags or unknown argument values |

A sweep where every instance is `evaluator_unavailable` exits 0 with a note on
stdout — the finding (no per-test data on disk) is not an error of the tool.

---

## Determinism

Two consecutive runs over the same sweep produce **byte-identical
`test-progress.json`** except for the single top-level `generated_at` field,
which carries the RFC-3339 UTC timestamp of the run. This is the same contract
used by `bench command-stats`, `bench behavior`, and other read-only analysis
artifacts.

---

## Redaction

Test names pass through the existing redaction pipeline before being written
to disk or printed. The `redaction_applied` field in the artifact mirrors the
boolean contract from other artifact types.

---

## Non-goals

- **Re-running the evaluator.** `bench test-progress` reads existing artifacts only.
- **A new evaluator backend or per-test instrumentation layer.** The command
  surfaces evaluator output as-is.
- **LLM-driven failure classification.** Counts and ratios only; classification
  belongs to `bench triage`.
- **Adaptive weighting** of `partial_credit_score`. The v1 clamped-subtraction
  formula is the documented definition; alternative weights can be added as a
  future flag once operators have calibrated against the baseline metric.
- **Cross-sweep `hot_failing_tests` joining.** Single-sweep only in this slice;
  `bench ladder` follow-up can join artifacts across sweeps.
- **Significance testing on partial-credit deltas.** Out of scope until issue
  #176 ships; a follow-up will wire significance into the compare section.
- **Surfacing partial credit in the SWE-bench leaderboard submission.** The
  official scoring is binary; this is an operator-side metric only.
