//! Build-time coverage test for docs/failure-categories.md (issue #330).
//!
//! RED phase: test fails because docs/failure-categories.md does not exist.
//! GREEN phase: create docs/failure-categories.md with all variant entries.
//! REFACTOR phase: update links in README.md, exit-codes.md, and spec files.
//!
//! The exhaustive match in `serde_string` ensures that
//! adding or renaming a FailureCategory variant without updating this test
//! causes a compile error, making documentation drift impossible to merge.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use maxwells_daemon::trajectory::FailureCategory;

/// Returns the operator-visible serde JSON string for every FailureCategory variant.
///
/// The match statement is intentionally exhaustive: if a new variant is added to
/// FailureCategory without adding a corresponding arm here, this function fails
/// to compile, which blocks the merge until the doc is updated.
fn serde_string(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::BudgetExhausted => "budget_exhausted",
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::AgentStagnation => "agent_stagnation",
        FailureCategory::HistoryCompactionFailed => "history_compaction_failed",
        FailureCategory::ReadOnlyViolation => "read_only_violation",
        FailureCategory::Unknown => "unknown",
    }
}

#[test]
fn all_failure_category_variants_documented_in_reference_page() {
    let doc = std::fs::read_to_string("docs/failure-categories.md").expect(
        "docs/failure-categories.md must exist — see issue #330 and the GREEN phase instructions",
    );

    // All known variants; the exhaustive serde_string() match ensures compile-time coverage.
    let variants = [
        FailureCategory::EnvSetup,
        FailureCategory::ModelApi,
        FailureCategory::ModelParse,
        FailureCategory::StepLimit,
        FailureCategory::CostLimit,
        FailureCategory::BudgetExhausted,
        FailureCategory::WallclockTimeout,
        FailureCategory::AgentInternal,
        FailureCategory::PatchApplyInvalid,
        FailureCategory::PatchEmpty,
        FailureCategory::SecretLeakDetected,
        FailureCategory::AgentStagnation,
        FailureCategory::HistoryCompactionFailed,
        FailureCategory::ReadOnlyViolation,
        FailureCategory::Unknown,
    ];

    // Cross-check: serde_string() must agree with the actual serde serialization.
    for cat in variants {
        let expected = serde_string(cat);
        let actual_json = serde_json::to_string(&cat).unwrap();
        let actual = actual_json.trim_matches('"');
        assert_eq!(
            expected, actual,
            "serde_string() returned '{expected}' but serde_json serialized {cat:?} as '{actual}'; \
             update serde_string() to match the enum's serde attributes"
        );
    }

    // Runtime check: each serde string must appear as a backtick-wrapped entry in the reference
    // page (e.g. `env_setup`). Backtick-wrapping avoids false positives where one variant name
    // is a substring of another (e.g. a hypothetical `api` matching `model_api`).
    let missing: Vec<String> = variants
        .iter()
        .map(|&v| serde_string(v))
        .filter(|&s| !doc.contains(&format!("`{s}`")))
        .map(str::to_owned)
        .collect();

    assert!(
        missing.is_empty(),
        "The following failure_category serde strings are not documented in \
         docs/failure-categories.md:\n  - {}\n\nAdd an entry for each missing value \
         following the template in that file.",
        missing.join("\n  - ")
    );
}

#[test]
fn failure_categories_doc_reserves_unknown_string() {
    let doc = std::fs::read_to_string("docs/failure-categories.md")
        .expect("docs/failure-categories.md must exist — see issue #330");

    assert!(
        doc.contains("unknown"),
        "docs/failure-categories.md must document the reserved 'unknown' string"
    );

    // The compatibility section must state that 'unknown' is reserved.
    assert!(
        doc.to_lowercase().contains("reserved") && doc.contains("unknown"),
        "docs/failure-categories.md must state that 'unknown' is reserved for \
         forward-compatible parsing"
    );
}

#[test]
fn failure_categories_doc_links_to_relevant_commands() {
    let doc = std::fs::read_to_string("docs/failure-categories.md")
        .expect("docs/failure-categories.md must exist — see issue #330");

    for cmd in ["bench triage", "bench inspect", "bench retry", "bench tail"] {
        assert!(
            doc.contains(cmd),
            "docs/failure-categories.md must reference '{cmd}'"
        );
    }
}

#[test]
fn readme_links_to_failure_categories_doc() {
    let readme = std::fs::read_to_string("README.md").unwrap();
    assert!(
        readme.contains("failure-categories.md"),
        "README.md must link to docs/failure-categories.md (Troubleshooting or Advanced Specs section)"
    );
}

#[test]
fn exit_codes_doc_links_to_failure_categories_doc() {
    let doc = std::fs::read_to_string("docs/exit-codes.md").unwrap();
    assert!(
        doc.contains("failure-categories.md"),
        "docs/exit-codes.md must link to docs/failure-categories.md at the failure_category mention"
    );
}
