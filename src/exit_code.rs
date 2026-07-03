//! Stable CLI exit-code contract.
//!
//! Each variant corresponds to a documented outcome class. The `outcome_class()`
//! string is written to stderr on failure (human-readable label) and may appear
//! in JSON output (machine-readable) so automation can route failures without
//! parsing prose.
//!
//! ## Compatibility policy
//!
//! Assigned codes are **stable**. Renumbering a published outcome class is a
//! breaking change and requires a major version bump. New outcome classes may
//! be added with previously-unused numbers in a minor release; consumers must
//! treat an unknown code as `internal_error`.
//!
//! See `docs/exit-codes.md` for the full contract including per-command tables
//! and shell examples.

use crate::error::{EnvError, Error};

/// Stable numeric process exit code for each terminal outcome class.
///
/// The integer discriminant is guaranteed stable across releases.
/// New variants may be added in minor releases with previously-unused numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// 0 — success, or no-op success (e.g. `bench doctor` with all checks passing).
    Success = 0,
    /// 1 — unexpected internal error (I/O failure, JSON parse, unclassified panic).
    InternalError = 1,
    /// 2 — usage or configuration error (bad flag, unknown value, missing required arg,
    /// invalid config file).
    UsageError = 2,
    /// 3 — preflight or dependency failure (Docker not installed, model endpoint
    /// unreachable, container start failed).
    PreflightFailure = 3,
    /// 4 — agent task unsuccessful (step limit reached, environment setup failed,
    /// repeated model API errors, wallclock timeout).
    TaskUnsuccessful = 4,
    /// 5 — budget or cost halt (`bench forecast --fail-over-cap` projected an
    /// over-cap run, or the sweep stopped because `--sweep-cost-limit-usd` was hit).
    BudgetHalt = 5,
    /// 6 — regression gate failure (`bench compare --max-regressions` or
    /// `--max-patch-size-regression` threshold exceeded).
    RegressionGateFailure = 6,
    /// 7 — verification failure (one or more `--verify` checks did not pass).
    VerificationFailure = 7,
    /// 8 - calibration gate failure (`bench calibrate --fail-on-optimistic`
    /// found actuals above the forecast interval).
    CalibrationOptimistic = 8,
    /// 9 — replay prompt-drift detected: the agent's current input messages do
    /// not match the fingerprint stored in the cassette trajectory.
    ReplayPromptDrift = 9,
    /// 10 — replay response exhausted: the scripted-response queue ran out
    /// before the agent finished (trajectory is structurally incompatible).
    ReplayResponseExhausted = 10,
    /// 11 — sweep halted by the systemic-failure circuit breaker: at least N
    /// completed instances shared the same operator-actionable failure category
    /// at or above the configured share threshold (default 80%). See
    /// `docs/spec-systemic-halt.md` for the full contract.
    SystemicHalt = 11,
    /// 12 — agent stagnation detected: the agent repeated the same action at
    /// least K times within a trailing window of W steps. See
    /// `docs/spec-stagnation.md` for the full contract.
    AgentStagnation = 12,
    /// 13 — `agent env preview` found at least one risky finding (wide host
    /// path, sensitive env var, MCP server outside workdir, etc.). The preview
    /// itself was printed successfully; the non-zero exit signals that the
    /// operator should review the findings before running a sweep.
    EnvPreviewWarning = 13,
    /// 14 — `agent skills-preview` completed but found at least one warning
    /// condition: a task hit `max_active`, an activated manifest has no
    /// `version` field, or `auto_load` resolved an implicitly-matched skill.
    /// See `docs/spec-skills-preview.md` for the full contract.
    SkillsPreviewWarning = 14,
    /// 15 — `mini --resume` target trajectory already has a terminal outcome;
    /// cannot continue a run that already completed, was cancelled, or hit a cap.
    ResumeAlreadyTerminal = 15,
    /// 16 — `mini --resume` target trajectory pre-dates the required manifest schema;
    /// `task` or `model_name` fields are missing so the run configuration cannot be
    /// reconstructed.
    ResumeManifestMissing = 16,
    /// 17 — `mini --resume` target trajectory is structurally invalid for resume;
    /// the message sequence is empty, too short, or ends in a partial assistant turn.
    ResumeInvalidPrefix = 17,
    /// 18 — `bench bisect` budget exhausted before identifying the regressing commit.
    BisectBudgetExhausted = 18,
    /// 19 — `bench bisect` found only trajectory-schema breaks in the remaining search space.
    BisectSchemaBreak = 19,
    /// 20 — `bench audit` detected a divergence exceeding tolerance or a bijection/evaluator contradiction.
    AuditFailure = 20,
    /// 21 — `bench compare --max-test-only-resolved-rate` threshold was exceeded.
    EvalGamingGateFailure = 21,
    /// 22 — `bench export-ci` detected that the JUnit XML aggregate attributes
    /// (`tests`, `failures`, `errors`) do not match the corresponding counts in
    /// `results.json`. The export still wrote whatever it had, but the mismatch
    /// signals a corrupt or incomplete sweep artifact.
    ArtifactIntegrityViolation = 22,
    /// 23 — `bench scriptability-check` found at least one misconfigured MCP
    /// server or hook. Zero model calls were made; the check is purely a wiring
    /// preflight. Distinct from `preflight_failure` (3) so CI can route
    /// scriptability misconfig separately from infrastructure failures.
    ScriptabilityCheckFailure = 23,
    /// 24 — a subcommand requires a Cargo feature that was not compiled in.
    /// The error message names the missing feature and the docs link.
    FeatureUnavailable = 24,
    /// 25 — `agent redact-check` found that at least one configured `secret_literals`
    /// entry produced zero matches against the sample input. This signals that the
    /// configured literal is likely stale and may not be redacting anything.
    RedactCheckStaleLiterals = 25,
    /// 26 — `agent redact-check --strict` found that at least one `custom_patterns`
    /// entry compiled successfully but produced zero matches against the sample input.
    /// Useful in CI to catch a regex typo or a renamed token format before a sweep.
    RedactCheckStrictFail = 26,
    /// 27 — `bench assert` evaluated all rules and at least one rule failed.
    /// Distinct from `usage_error` (2) so CI can distinguish "your gate failed"
    /// from "your invocation is broken". All rules were evaluated; the gate is wired
    /// correctly but the sweep did not meet the declared SLO.
    SloRuleFailure = 27,
    /// 28 — `mini --continue` target trajectory is non-terminal (still
    /// partial/in-progress); cannot issue a follow-up instruction to an
    /// unfinished run. Use `--resume` to recover a partial run instead.
    ContinueNonTerminal = 28,
    /// 29 — `agent apply` ran `git apply --check` and the patch was rejected.
    /// The tree is byte-for-byte unchanged; the rejected hunks were printed to
    /// stderr. Distinct from `internal_error` (1) so automation can distinguish
    /// "the patch is inapplicable" from "something else went wrong".
    ApplyCheckFailed = 29,
    /// 30 — `agent apply` refused because the source trajectory recorded
    /// redaction on the `patch_submission` surface, meaning the patch file
    /// contains `[REDACTED:…]` markers that would corrupt the working tree.
    /// Pass `--allow-redacted` to override.
    ApplyRedactedRefused = 30,
    /// 31 — `agent apply` refused because the target working tree has
    /// uncommitted changes. Pass `--allow-dirty` to override.
    ApplyDirtyTreeRefused = 31,
    /// 32 — `agent redact-audit` found at least one new finding at `medium`+
    /// severity in the scanned sweep artifacts. The audit completed and wrote
    /// its report; the non-zero exit is the CI publish gate. Distinct from
    /// `internal_error` (1) so automation can route "a secret leaked into the
    /// artifacts" separately from an unexpected crash.
    RedactAuditFindings = 32,
    /// 33 — `agent redact-audit` could not read or extract one or more
    /// artifacts (unreadable file, corrupt bundle). The scan is incomplete, so
    /// a "clean" verdict cannot be trusted. Distinct from `usage_error` (2) so
    /// CI can tell "the scan broke" from "your invocation is broken".
    RedactAuditScanError = 33,
    /// 34 — missing GitHub token for issue ingestion.
    GithubIssueMissingToken = 34,
    /// 35 — GitHub issue or repository not found.
    GithubIssueNotFound = 35,
    /// 36 — GitHub API rate limited during issue ingestion.
    GithubIssueRateLimited = 36,
    /// 37 — `agent injection-audit` found at least one hit at or above the
    /// configured `--fail-on` severity threshold. The audit completed; the
    /// non-zero exit is the CI publish gate. Distinct from `internal_error` (1)
    /// so automation can route "injection signals found" separately from a crash.
    InjectionAuditHits = 37,
    /// 38 — `agent injection-audit` could not read the sweep directory or one
    /// or more trajectory files (missing path, unreadable file). The scan is
    /// incomplete, so a "clean" verdict cannot be trusted.
    InjectionAuditScanError = 38,
    /// 39 — `agent stability --fail-under <F>` found `pass_at_k < F`. All
    /// runs completed; the gate is wired correctly but the measured pass rate
    /// did not meet the declared threshold.
    StabilityGateFailure = 39,
    /// 40 — `bench dataset-verify` detected a mismatch between the candidate dataset and the canonical reference.
    DatasetVerifyMismatch = 40,
    /// 41 — `agent best-of` completed all runs and no run passed all verify
    /// checks. The best-scoring run was still selected and its patch emitted;
    /// `all_failed: true` is set in `best-of-results.json`. Pass
    /// `--allow-no-pass` to downgrade to exit 0 while keeping `all_failed: true`.
    BestOfAllFailed = 41,
    /// 42 — `agent config resolve` detected at least one clap-default override
    /// hazard: a `--config` file sets a field (`model.name` or
    /// `agent.step_limit`) that a clap default in `mini` or `bench swebench`
    /// will silently overwrite unless the corresponding flag is also passed
    /// explicitly. The resolved config was printed successfully; the non-zero
    /// exit allows CI to gate on silent override detection. Exit 0 when the
    /// resolved config matches operator intent (no hazards detected).
    ConfigOverrideWarning = 42,
    /// 43 — `bench eval-parity --min-agreement <F>` measured an agreement rate
    /// below the operator-declared threshold. The report was written; the
    /// non-zero exit gates CI on offline-vs-canonical parity. Distinct from
    /// `slo_rule_failure` (27) so automation can route "evaluator parity
    /// degraded" separately from generic SLO failures.
    EvalParityGateFailure = 43,
    /// 44 — `bench utilization --min-utilization <PCT>` measured a concurrency
    /// utilization below the operator-declared floor. The report was written;
    /// the non-zero exit gates CI on sweep concurrency efficiency. Distinct from
    /// `slo_rule_failure` (27) so automation can route "concurrency
    /// under-utilized" separately from generic SLO failures.
    UtilizationGateFailure = 44,
    /// 45 — `agent fs-audit` found at least one finding (a bash command accessed a
    /// path outside the configured workdir). The audit completed and its report was
    /// printed; the non-zero exit is the CI publish gate. Distinct from
    /// `internal_error` (1) so automation can route "filesystem boundary violated"
    /// separately from an unexpected crash.
    FsAuditFindings = 45,
    /// 46 — `agent fs-audit` could not read or parse one or more trajectory files
    /// (unreadable file, invalid JSON, missing sweep directory). The scan is
    /// incomplete, so a "clean" verdict cannot be trusted. Distinct from
    /// `usage_error` (2) so CI can tell "the scan broke" from "your invocation is
    /// broken".
    FsAuditScanError = 46,
    /// 47 — `agent artifact-check` found at least one artifact that is `invalid`
    /// or `unsupported_major`. With `--strict`, also triggers on
    /// `legacy_unversioned` and `valid_with_warnings`. Zero model calls are made;
    /// the check is purely a structural conformance gate. Distinct from
    /// `internal_error` (1) so CI can route "artifact does not conform to
    /// contract" separately from an unexpected infrastructure failure.
    ArtifactCheckFailure = 47,
    /// 48 — `agent doctor` found at least one failing host-readiness check
    /// (git missing, provider credential absent, Docker daemon unreachable when
    /// a docker environment is selected, runs/output dir not writable, or the
    /// active toolchain below the crate `rust-version`). Zero model calls and no
    /// provider network probe were made; the check is a pure host preflight.
    /// Skipped checks never trigger this. Distinct from `preflight_failure` (3)
    /// so CI can route "host not ready before any run" separately from
    /// sweep-time dependency failures. See `docs/spec-agent-doctor.md`.
    HostNotReady = 48,
    /// 49 — `bench ledger --budget-usd <N>` found that the grand total actual
    /// spend across discovered trajectories meets or exceeds N. The report is
    /// printed before exit; the non-zero exit allows CI to gate on budget
    /// exhaustion. Distinct from `budget_halt` (5) which is a forecast/sweep
    /// cap, not a post-hoc accounting check.
    LedgerBudgetExceeded = 49,
    /// 50 — `bench du --prune --apply` skipped at least one sweep that
    /// matched the given selectors: either its partial trajectory checkpoint
    /// was touched within `--in-progress-window` (not confirmed idle — this
    /// is an mtime-freshness heuristic, not a lock/PID liveness check, see
    /// `docs/spec-disk-usage.md`), or `std::fs::remove_dir_all` itself
    /// failed. That sweep was left on disk and reported under `protected`/
    /// `deletion_failed`; every other matching sweep was still deleted. The
    /// report is printed before exit.
    DiskUsagePruneBlocked = 50,
    /// 130 — user interruption (graceful SIGINT / Ctrl-C; 128 + SIGINT(2)).
    Interrupted = 130,
    /// 137 — forced kill (SIGKILL escalation after graceful-cancel deadline; 128 + SIGKILL(9)).
    Killed = 137,
}

impl ExitCode {
    /// Return the raw integer exit code.
    #[must_use]
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Stable outcome class label for machine-readable and human-readable output.
    ///
    /// The returned string is guaranteed stable for the lifetime of this variant;
    /// it uses only lowercase ASCII letters and underscores.
    #[must_use]
    pub fn outcome_class(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::InternalError => "internal_error",
            Self::UsageError => "usage_error",
            Self::PreflightFailure => "preflight_failure",
            Self::TaskUnsuccessful => "task_unsuccessful",
            Self::BudgetHalt => "budget_halt",
            Self::RegressionGateFailure => "regression_gate_failure",
            Self::VerificationFailure => "verification_failure",
            Self::CalibrationOptimistic => "calibration_optimistic",
            Self::ReplayPromptDrift => "replay_prompt_drift",
            Self::ReplayResponseExhausted => "replay_response_exhausted",
            Self::SystemicHalt => "systemic_halt",
            Self::AgentStagnation => "agent_stagnation",
            Self::EnvPreviewWarning => "env_preview_warning",
            Self::SkillsPreviewWarning => "skills_preview_warning",
            Self::ResumeAlreadyTerminal => "resume_already_terminal",
            Self::ResumeManifestMissing => "resume_manifest_missing",
            Self::ResumeInvalidPrefix => "resume_invalid_prefix",
            Self::BisectBudgetExhausted => "bisect_budget_exhausted",
            Self::BisectSchemaBreak => "bisect_schema_break",
            Self::AuditFailure => "audit_failure",
            Self::EvalGamingGateFailure => "eval_gaming_gate_failure",
            Self::ArtifactIntegrityViolation => "artifact_integrity_violation",
            Self::ScriptabilityCheckFailure => "scriptability_check_failure",
            Self::FeatureUnavailable => "feature_unavailable",
            Self::RedactCheckStaleLiterals => "redact_check_stale_literals",
            Self::RedactCheckStrictFail => "redact_check_strict_fail",
            Self::SloRuleFailure => "slo_rule_failure",
            Self::ContinueNonTerminal => "continue_non_terminal",
            Self::ApplyCheckFailed => "apply_check_failed",
            Self::ApplyRedactedRefused => "apply_redacted_refused",
            Self::ApplyDirtyTreeRefused => "apply_dirty_tree_refused",
            Self::RedactAuditFindings => "redact_audit_findings",
            Self::RedactAuditScanError => "redact_audit_scan_error",
            Self::GithubIssueMissingToken => "github_issue_missing_token",
            Self::GithubIssueNotFound => "github_issue_not_found",
            Self::GithubIssueRateLimited => "github_issue_rate_limited",
            Self::InjectionAuditHits => "injection_audit_hits",
            Self::InjectionAuditScanError => "injection_audit_scan_error",
            Self::StabilityGateFailure => "stability_gate_failure",
            Self::DatasetVerifyMismatch => "dataset_verify_mismatch",
            Self::BestOfAllFailed => "best_of_all_failed",
            Self::ConfigOverrideWarning => "config_override_warning",
            Self::EvalParityGateFailure => "eval_parity_gate_failure",
            Self::UtilizationGateFailure => "utilization_gate_failure",
            Self::FsAuditFindings => "fs_audit_findings",
            Self::FsAuditScanError => "fs_audit_scan_error",
            Self::ArtifactCheckFailure => "artifact_check_failure",
            Self::HostNotReady => "host_not_ready",
            Self::LedgerBudgetExceeded => "ledger_budget_exceeded",
            Self::DiskUsagePruneBlocked => "disk_usage_prune_blocked",
            Self::Interrupted => "interrupted",
            Self::Killed => "killed",
        }
    }

    /// Best-effort mapping from a runtime `Error` to the appropriate outcome class.
    ///
    /// A small number of outcomes (regression gate, sweep cancellation, budget-halt
    /// forecast) are driven by explicit `process::exit` calls in the CLI layer and
    /// do not go through this function. See `docs/exit-codes.md` for the full table.
    #[must_use]
    pub fn from_error(e: &Error) -> Self {
        match e {
            Error::Preflight(_) => Self::PreflightFailure,
            Error::Config(_) => Self::UsageError,
            Error::VerificationFailed(..) => Self::VerificationFailure,
            Error::Env(env_e) => Self::from_env_error(env_e),
            Error::Model(model_e) => match model_e {
                crate::error::ModelError::ReplayDrift(_) => Self::ReplayPromptDrift,
                crate::error::ModelError::ScriptedResponsesExhausted(_) => {
                    Self::ReplayResponseExhausted
                }
                crate::error::ModelError::ReplayUnfingerprintedLegacy(_) => Self::UsageError,
                // ResponsesExhausted (generic DeterministicModel exhaustion used in
                // non-replay contexts) and all other model errors → task unsuccessful.
                // Replay translates ResponsesExhausted → ScriptedResponsesExhausted
                // before this function is called, so exit-10 is replay-only.
                _ => Self::TaskUnsuccessful,
            },
            Error::AgentStagnation { .. } => Self::AgentStagnation,
            Error::BisectBudgetExhausted => Self::BisectBudgetExhausted,
            Error::BisectSchemaBreak => Self::BisectSchemaBreak,
            Error::Audit(_) => Self::AuditFailure,
            Error::Template(_)
            | Error::Trajectory(_)
            | Error::Github(_)
            | Error::Io(_)
            | Error::Json(_) => Self::InternalError,
            Error::GithubIssue(issue_e) => match issue_e {
                crate::error::GithubIssueError::MissingToken(_) => Self::GithubIssueMissingToken,
                crate::error::GithubIssueError::NotFound(_) => Self::GithubIssueNotFound,
                crate::error::GithubIssueError::RateLimited(_) => Self::GithubIssueRateLimited,
                crate::error::GithubIssueError::RequestFailed(_) => Self::TaskUnsuccessful,
            },
        }
    }

    fn from_env_error(e: &EnvError) -> Self {
        match e {
            EnvError::DockerNotInstalled
            | EnvError::DockerDaemonUnreachable(_)
            | EnvError::ContainerStartFailed(_) => Self::PreflightFailure,
            EnvError::CommandFailed(_)
            | EnvError::Timeout(_)
            | EnvError::UnexpectedExit(_)
            | EnvError::Io(_) => Self::TaskUnsuccessful,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ConfigError, EnvError, ModelError};

    #[test]
    fn from_error_config_is_usage_error() {
        assert_eq!(
            ExitCode::from_error(&Error::Config(ConfigError::Invalid("x".into()))),
            ExitCode::UsageError
        );
    }

    #[test]
    fn from_error_preflight_is_preflight_failure() {
        assert_eq!(
            ExitCode::from_error(&Error::Preflight("drift".into())),
            ExitCode::PreflightFailure
        );
    }

    #[test]
    fn from_error_verification_failed_is_verification_failure() {
        assert_eq!(
            ExitCode::from_error(&Error::VerificationFailed(1, 2)),
            ExitCode::VerificationFailure
        );
    }

    #[test]
    fn from_error_docker_not_installed_is_preflight_failure() {
        assert_eq!(
            ExitCode::from_error(&Error::Env(EnvError::DockerNotInstalled)),
            ExitCode::PreflightFailure
        );
    }

    #[test]
    fn from_error_command_failed_is_task_unsuccessful() {
        assert_eq!(
            ExitCode::from_error(&Error::Env(EnvError::CommandFailed("exit 1".into()))),
            ExitCode::TaskUnsuccessful
        );
    }

    #[test]
    fn from_error_timeout_is_task_unsuccessful() {
        assert_eq!(
            ExitCode::from_error(&Error::Env(EnvError::Timeout(
                std::time::Duration::from_secs(1)
            ))),
            ExitCode::TaskUnsuccessful
        );
    }

    #[test]
    fn from_error_unexpected_exit_is_task_unsuccessful() {
        assert_eq!(
            ExitCode::from_error(&Error::Env(EnvError::UnexpectedExit("1".into()))),
            ExitCode::TaskUnsuccessful
        );
    }

    #[test]
    fn from_error_docker_daemon_unreachable_is_preflight_failure() {
        assert_eq!(
            ExitCode::from_error(&Error::Env(EnvError::DockerDaemonUnreachable(
                "connection refused".into()
            ))),
            ExitCode::PreflightFailure
        );
    }

    #[test]
    fn from_error_container_start_failed_is_preflight_failure() {
        assert_eq!(
            ExitCode::from_error(&Error::Env(EnvError::ContainerStartFailed(
                "image not found".into()
            ))),
            ExitCode::PreflightFailure
        );
    }

    #[test]
    fn from_error_env_io_is_task_unsuccessful() {
        assert_eq!(
            ExitCode::from_error(&Error::Env(EnvError::Io(std::io::Error::other(
                "disk full"
            )))),
            ExitCode::TaskUnsuccessful
        );
    }

    #[test]
    fn from_error_model_is_task_unsuccessful() {
        assert_eq!(
            ExitCode::from_error(&Error::Model(ModelError::Request("t/o".into()))),
            ExitCode::TaskUnsuccessful
        );
    }

    #[test]
    fn from_error_io_is_internal_error() {
        assert_eq!(
            ExitCode::from_error(&Error::Io(std::io::Error::other("disk full"))),
            ExitCode::InternalError
        );
    }

    #[test]
    fn from_error_replay_response_exhausted() {
        assert_eq!(
            ExitCode::from_error(&Error::Model(ModelError::ScriptedResponsesExhausted(3))),
            ExitCode::ReplayResponseExhausted
        );
    }

    #[test]
    fn from_error_replay_unfingerprinted_legacy_is_usage_error() {
        assert_eq!(
            ExitCode::from_error(&Error::Model(ModelError::ReplayUnfingerprintedLegacy(0))),
            ExitCode::UsageError
        );
    }

    #[test]
    fn from_error_responses_exhausted_is_task_unsuccessful() {
        // Generic DeterministicModel exhaustion (non-replay) must not exit 10.
        assert_eq!(
            ExitCode::from_error(&Error::Model(ModelError::ResponsesExhausted(0))),
            ExitCode::TaskUnsuccessful
        );
    }

    // ── RED-phase: config override warning exit code ──────────────────────────

    #[test]
    fn config_override_warning_exit_code_is_42() {
        assert_eq!(ExitCode::ConfigOverrideWarning.as_i32(), 42);
        assert_eq!(
            ExitCode::ConfigOverrideWarning.outcome_class(),
            "config_override_warning"
        );
    }

    #[test]
    fn agent_stagnation_exit_code_is_12() {
        assert_eq!(ExitCode::AgentStagnation.as_i32(), 12);
        assert_eq!(
            ExitCode::AgentStagnation.outcome_class(),
            "agent_stagnation"
        );
    }

    // ── RED-phase: resume exit codes ─────────────────────────────────────

    #[test]
    fn resume_already_terminal_exit_code_is_15() {
        assert_eq!(ExitCode::ResumeAlreadyTerminal.as_i32(), 15);
        assert_eq!(
            ExitCode::ResumeAlreadyTerminal.outcome_class(),
            "resume_already_terminal"
        );
    }

    #[test]
    fn resume_manifest_missing_exit_code_is_16() {
        assert_eq!(ExitCode::ResumeManifestMissing.as_i32(), 16);
        assert_eq!(
            ExitCode::ResumeManifestMissing.outcome_class(),
            "resume_manifest_missing"
        );
    }

    #[test]
    fn resume_invalid_prefix_exit_code_is_17() {
        assert_eq!(ExitCode::ResumeInvalidPrefix.as_i32(), 17);
        assert_eq!(
            ExitCode::ResumeInvalidPrefix.outcome_class(),
            "resume_invalid_prefix"
        );
    }

    #[test]
    fn bisect_budget_exhausted_exit_code_is_18() {
        assert_eq!(ExitCode::BisectBudgetExhausted.as_i32(), 18);
        assert_eq!(
            ExitCode::BisectBudgetExhausted.outcome_class(),
            "bisect_budget_exhausted"
        );
    }

    #[test]
    fn bisect_schema_break_exit_code_is_19() {
        assert_eq!(ExitCode::BisectSchemaBreak.as_i32(), 19);
        assert_eq!(
            ExitCode::BisectSchemaBreak.outcome_class(),
            "bisect_schema_break"
        );
    }

    #[test]
    fn audit_failure_exit_code_is_20() {
        assert_eq!(ExitCode::AuditFailure.as_i32(), 20);
        assert_eq!(ExitCode::AuditFailure.outcome_class(), "audit_failure");
    }

    // ── RED-phase: continue exit codes ───────────────────────────────────

    #[test]
    fn continue_non_terminal_exit_code_is_28() {
        assert_eq!(ExitCode::ContinueNonTerminal.as_i32(), 28);
        assert_eq!(
            ExitCode::ContinueNonTerminal.outcome_class(),
            "continue_non_terminal"
        );
    }

    #[test]
    fn redact_audit_findings_exit_code_is_32() {
        assert_eq!(ExitCode::RedactAuditFindings.as_i32(), 32);
        assert_eq!(
            ExitCode::RedactAuditFindings.outcome_class(),
            "redact_audit_findings"
        );
    }

    #[test]
    fn redact_audit_scan_error_exit_code_is_33() {
        assert_eq!(ExitCode::RedactAuditScanError.as_i32(), 33);
        assert_eq!(
            ExitCode::RedactAuditScanError.outcome_class(),
            "redact_audit_scan_error"
        );
    }

    #[test]
    fn utilization_gate_failure_exit_code_is_44() {
        assert_eq!(ExitCode::UtilizationGateFailure.as_i32(), 44);
        assert_eq!(
            ExitCode::UtilizationGateFailure.outcome_class(),
            "utilization_gate_failure"
        );
    }

    // ── RED-phase: agent doctor host-readiness exit code (issue #526) ─────────

    #[test]
    fn host_not_ready_exit_code_is_48() {
        assert_eq!(ExitCode::HostNotReady.as_i32(), 48);
        assert_eq!(ExitCode::HostNotReady.outcome_class(), "host_not_ready");
    }

    #[test]
    fn ledger_budget_exceeded_exit_code_is_49() {
        assert_eq!(ExitCode::LedgerBudgetExceeded.as_i32(), 49);
        assert_eq!(
            ExitCode::LedgerBudgetExceeded.outcome_class(),
            "ledger_budget_exceeded"
        );
    }

    #[test]
    fn disk_usage_prune_blocked_exit_code_is_50() {
        assert_eq!(ExitCode::DiskUsagePruneBlocked.as_i32(), 50);
        assert_eq!(
            ExitCode::DiskUsagePruneBlocked.outcome_class(),
            "disk_usage_prune_blocked"
        );
    }
}
