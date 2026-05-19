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
            Error::Template(_)
            | Error::Trajectory(_)
            | Error::Github(_)
            | Error::Io(_)
            | Error::Json(_) => Self::InternalError,
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
}
