//! Anti-rot drift test for `max explain` (issue #535).
//!
//! This is the core acceptance guarantee: the compiled-in `explain` registry,
//! the `ExitCode` enum, `docs/exit-codes.md`, and the `FailureCategory` enum
//! must all stay in sync. The test is bidirectional — a code/class/category
//! present in one source but missing from another fails the build, so contract
//! drift is structurally impossible rather than merely discouraged.
//!
//! The exhaustive `match` arms below (`exit_code_identity` / `failure_serde`)
//! turn *adding a variant without an `explain` entry* into a compile error; the
//! runtime assertions turn *adding a variant without a doc row* into a test
//! failure.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use maxwells_daemon::exit_code::ExitCode;
use maxwells_daemon::explain::{self, Family};
use maxwells_daemon::trajectory::FailureCategory;

/// Every `ExitCode` variant, with a compile-time-exhaustive match so a new
/// variant cannot be added without updating this list.
#[allow(clippy::too_many_lines)]
fn all_exit_codes() -> Vec<ExitCode> {
    // The exhaustive match forces a compile error if a variant is added/removed.
    fn exit_code_identity(e: ExitCode) -> ExitCode {
        match e {
            ExitCode::Success
            | ExitCode::InternalError
            | ExitCode::UsageError
            | ExitCode::PreflightFailure
            | ExitCode::TaskUnsuccessful
            | ExitCode::BudgetHalt
            | ExitCode::RegressionGateFailure
            | ExitCode::VerificationFailure
            | ExitCode::CalibrationOptimistic
            | ExitCode::ReplayPromptDrift
            | ExitCode::ReplayResponseExhausted
            | ExitCode::SystemicHalt
            | ExitCode::AgentStagnation
            | ExitCode::EnvPreviewWarning
            | ExitCode::SkillsPreviewWarning
            | ExitCode::ResumeAlreadyTerminal
            | ExitCode::ResumeManifestMissing
            | ExitCode::ResumeInvalidPrefix
            | ExitCode::BisectBudgetExhausted
            | ExitCode::BisectSchemaBreak
            | ExitCode::AuditFailure
            | ExitCode::EvalGamingGateFailure
            | ExitCode::ArtifactIntegrityViolation
            | ExitCode::ScriptabilityCheckFailure
            | ExitCode::FeatureUnavailable
            | ExitCode::RedactCheckStaleLiterals
            | ExitCode::RedactCheckStrictFail
            | ExitCode::SloRuleFailure
            | ExitCode::ContinueNonTerminal
            | ExitCode::ApplyCheckFailed
            | ExitCode::ApplyRedactedRefused
            | ExitCode::ApplyDirtyTreeRefused
            | ExitCode::RedactAuditFindings
            | ExitCode::RedactAuditScanError
            | ExitCode::GithubIssueMissingToken
            | ExitCode::GithubIssueNotFound
            | ExitCode::GithubIssueRateLimited
            | ExitCode::InjectionAuditHits
            | ExitCode::InjectionAuditScanError
            | ExitCode::StabilityGateFailure
            | ExitCode::DatasetVerifyMismatch
            | ExitCode::BestOfAllFailed
            | ExitCode::ConfigOverrideWarning
            | ExitCode::EvalParityGateFailure
            | ExitCode::UtilizationGateFailure
            | ExitCode::FsAuditFindings
            | ExitCode::FsAuditScanError
            | ExitCode::ArtifactCheckFailure
            | ExitCode::HostNotReady
            | ExitCode::LedgerBudgetExceeded
            | ExitCode::DiskUsagePruneBlocked
            | ExitCode::Interrupted
            | ExitCode::Killed => e,
        }
    }

    [
        ExitCode::Success,
        ExitCode::InternalError,
        ExitCode::UsageError,
        ExitCode::PreflightFailure,
        ExitCode::TaskUnsuccessful,
        ExitCode::BudgetHalt,
        ExitCode::RegressionGateFailure,
        ExitCode::VerificationFailure,
        ExitCode::CalibrationOptimistic,
        ExitCode::ReplayPromptDrift,
        ExitCode::ReplayResponseExhausted,
        ExitCode::SystemicHalt,
        ExitCode::AgentStagnation,
        ExitCode::EnvPreviewWarning,
        ExitCode::SkillsPreviewWarning,
        ExitCode::ResumeAlreadyTerminal,
        ExitCode::ResumeManifestMissing,
        ExitCode::ResumeInvalidPrefix,
        ExitCode::BisectBudgetExhausted,
        ExitCode::BisectSchemaBreak,
        ExitCode::AuditFailure,
        ExitCode::EvalGamingGateFailure,
        ExitCode::ArtifactIntegrityViolation,
        ExitCode::ScriptabilityCheckFailure,
        ExitCode::FeatureUnavailable,
        ExitCode::RedactCheckStaleLiterals,
        ExitCode::RedactCheckStrictFail,
        ExitCode::SloRuleFailure,
        ExitCode::ContinueNonTerminal,
        ExitCode::ApplyCheckFailed,
        ExitCode::ApplyRedactedRefused,
        ExitCode::ApplyDirtyTreeRefused,
        ExitCode::RedactAuditFindings,
        ExitCode::RedactAuditScanError,
        ExitCode::GithubIssueMissingToken,
        ExitCode::GithubIssueNotFound,
        ExitCode::GithubIssueRateLimited,
        ExitCode::InjectionAuditHits,
        ExitCode::InjectionAuditScanError,
        ExitCode::StabilityGateFailure,
        ExitCode::DatasetVerifyMismatch,
        ExitCode::BestOfAllFailed,
        ExitCode::ConfigOverrideWarning,
        ExitCode::EvalParityGateFailure,
        ExitCode::UtilizationGateFailure,
        ExitCode::FsAuditFindings,
        ExitCode::FsAuditScanError,
        ExitCode::ArtifactCheckFailure,
        ExitCode::HostNotReady,
        ExitCode::LedgerBudgetExceeded,
        ExitCode::DiskUsagePruneBlocked,
        ExitCode::Interrupted,
        ExitCode::Killed,
    ]
    .into_iter()
    .map(exit_code_identity)
    .collect()
}

/// Every `FailureCategory` variant with its serde snake_case wire name; the
/// exhaustive match forces a compile error if a variant is added/removed.
fn all_failure_categories() -> Vec<(FailureCategory, &'static str)> {
    fn failure_serde(c: FailureCategory) -> &'static str {
        match c {
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

    [
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
    ]
    .into_iter()
    .map(|c| (c, failure_serde(c)))
    .collect()
}

/// Parse `(code, outcome_class)` rows from the markdown table in
/// `docs/exit-codes.md` (lines shaped like `| 7 | \`verification_failure\` | … |`).
fn parse_exit_code_doc_rows() -> Vec<(i32, String)> {
    let doc = std::fs::read_to_string("docs/exit-codes.md").unwrap();
    let mut rows = Vec::new();
    for line in doc.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() < 3 {
            continue;
        }
        // First cell must be a bare integer (skips the header and separator rows).
        let Ok(code) = cells[0].parse::<i32>() else {
            continue;
        };
        let class = cells[1].trim_matches('`').to_string();
        rows.push((code, class));
    }
    rows
}

// ── AC: every ExitCode variant is resolvable by code and by class ────────────

#[test]
fn every_exit_code_variant_is_resolvable() {
    for ec in all_exit_codes() {
        let code = ec.as_i32();
        let class = ec.outcome_class();

        let by_int = explain::resolve(&code.to_string()).unwrap_or_else(|| {
            panic!("explain cannot resolve exit code {code} ({class}); add a registry entry")
        });
        assert_eq!(by_int.code, Some(code), "code mismatch for {class}");
        assert_eq!(
            by_int.outcome_class, class,
            "class mismatch for code {code}"
        );
        assert!(
            by_int.families.contains(&Family::ExitCode),
            "{class} must be tagged Family::ExitCode"
        );

        let by_name = explain::resolve(class)
            .unwrap_or_else(|| panic!("explain cannot resolve outcome class '{class}'"));
        assert_eq!(
            by_name.code,
            Some(code),
            "name lookup code mismatch for {class}"
        );
    }
}

// ── AC: docs/exit-codes.md ⇄ registry are bidirectionally in sync ────────────

#[test]
fn every_doc_exit_code_row_is_resolvable() {
    for (code, class) in parse_exit_code_doc_rows() {
        let entry = explain::resolve(&code.to_string())
            .unwrap_or_else(|| panic!("docs/exit-codes.md row {code} ({class}) not in explain"));
        assert_eq!(entry.outcome_class, class, "class drift for code {code}");
    }
}

#[test]
fn every_registry_exit_code_appears_in_docs() {
    let doc_rows = parse_exit_code_doc_rows();
    for entry in explain::entries() {
        if !entry.families.contains(&Family::ExitCode) {
            continue;
        }
        let code = entry
            .code
            .expect("exit-code entry must carry an integer code");
        let found = doc_rows
            .iter()
            .any(|(c, class)| *c == code && class == entry.outcome_class);
        assert!(
            found,
            "explain entry {code} ({}) has no matching row in docs/exit-codes.md",
            entry.outcome_class
        );
    }
}

// ── AC: every FailureCategory variant is resolvable (snake + Pascal, ci) ─────

#[test]
fn every_failure_category_variant_is_resolvable() {
    for (cat, wire) in all_failure_categories() {
        let by_snake = explain::resolve(wire)
            .unwrap_or_else(|| panic!("explain cannot resolve failure category '{wire}'"));
        assert!(
            by_snake.families.contains(&Family::FailureCategory),
            "{wire} must be tagged Family::FailureCategory"
        );
        assert_eq!(by_snake.outcome_class, wire);

        // PascalCase form (the enum's display form), case-insensitively.
        let pascal = format!("{cat:?}");
        let by_pascal = explain::resolve(&pascal)
            .unwrap_or_else(|| panic!("explain cannot resolve '{pascal}' (Pascal form of {wire})"));
        assert_eq!(by_pascal.outcome_class, wire);

        // Upper-case snake form must also resolve (case-insensitive).
        let by_upper = explain::resolve(&wire.to_uppercase())
            .unwrap_or_else(|| panic!("explain cannot resolve '{}' ", wire.to_uppercase()));
        assert_eq!(by_upper.outcome_class, wire);
    }
}

#[test]
fn every_registry_failure_category_is_a_real_variant() {
    let wire_names: Vec<&'static str> = all_failure_categories().iter().map(|(_, w)| *w).collect();
    for entry in explain::entries() {
        if !entry.families.contains(&Family::FailureCategory) {
            continue;
        }
        assert!(
            wire_names.contains(&entry.outcome_class),
            "explain failure-category entry '{}' is not a FailureCategory variant",
            entry.outcome_class
        );
    }
}

// ── AC: every entry carries meaning + remediation + docs_ref ─────────────────

#[test]
fn every_entry_has_meaning_remediation_and_docs_ref() {
    for entry in explain::entries() {
        assert!(
            !entry.meaning.trim().is_empty(),
            "{} has empty meaning",
            entry.outcome_class
        );
        assert!(
            !entry.remediation.trim().is_empty(),
            "{} has empty remediation",
            entry.outcome_class
        );
        assert!(
            !entry.docs_ref.trim().is_empty(),
            "{} has empty docs_ref",
            entry.outcome_class
        );
    }
}

#[test]
fn registry_has_no_duplicate_outcome_classes() {
    let mut seen = std::collections::HashSet::new();
    for entry in explain::entries() {
        assert!(
            seen.insert(entry.outcome_class),
            "duplicate explain entry for '{}'",
            entry.outcome_class
        );
    }
}
