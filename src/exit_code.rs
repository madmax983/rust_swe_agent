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
            Error::Model(_) => Self::TaskUnsuccessful,
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
    fn from_error_model_is_task_unsuccessful() {
        assert_eq!(
            ExitCode::from_error(&Error::Model(ModelError::Request("t/o".into()))),
            ExitCode::TaskUnsuccessful
        );
    }

    #[test]
    fn from_error_io_is_internal_error() {
        assert_eq!(
            ExitCode::from_error(&Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "disk full"
            ))),
            ExitCode::InternalError
        );
    }
}
