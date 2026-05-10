//! Stable CLI exit-code contract — TDD tests for issue #98.
//!
//! RED phase: tests compile but assertions / types do not yet exist.
//! GREEN phase: implement `src/exit_code.rs` and wire it into `main.rs`.
//! REFACTOR phase: docs/exit-codes.md documents the contract.

#![allow(clippy::unwrap_used)]

use rust_swe_agent::error::{ConfigError, EnvError, Error, ModelError};
use rust_swe_agent::exit_code::ExitCode;

// ── integer code values ───────────────────────────────────────────────────────

#[test]
fn success_code_is_zero() {
    assert_eq!(ExitCode::Success.as_i32(), 0);
}

#[test]
fn internal_error_code_is_one() {
    assert_eq!(ExitCode::InternalError.as_i32(), 1);
}

#[test]
fn usage_error_code_is_two() {
    assert_eq!(ExitCode::UsageError.as_i32(), 2);
}

#[test]
fn preflight_failure_code_is_three() {
    assert_eq!(ExitCode::PreflightFailure.as_i32(), 3);
}

#[test]
fn task_unsuccessful_code_is_four() {
    assert_eq!(ExitCode::TaskUnsuccessful.as_i32(), 4);
}

#[test]
fn budget_halt_code_is_five() {
    assert_eq!(ExitCode::BudgetHalt.as_i32(), 5);
}

#[test]
fn regression_gate_failure_code_is_six() {
    assert_eq!(ExitCode::RegressionGateFailure.as_i32(), 6);
}

#[test]
fn verification_failure_code_is_seven() {
    assert_eq!(ExitCode::VerificationFailure.as_i32(), 7);
}

#[test]
fn interrupted_code_is_130() {
    assert_eq!(ExitCode::Interrupted.as_i32(), 130);
}

#[test]
fn killed_code_is_137() {
    assert_eq!(ExitCode::Killed.as_i32(), 137);
}

// ── outcome_class strings ─────────────────────────────────────────────────────

#[test]
fn outcome_class_success() {
    assert_eq!(ExitCode::Success.outcome_class(), "success");
}

#[test]
fn outcome_class_internal_error() {
    assert_eq!(ExitCode::InternalError.outcome_class(), "internal_error");
}

#[test]
fn outcome_class_usage_error() {
    assert_eq!(ExitCode::UsageError.outcome_class(), "usage_error");
}

#[test]
fn outcome_class_preflight_failure() {
    assert_eq!(ExitCode::PreflightFailure.outcome_class(), "preflight_failure");
}

#[test]
fn outcome_class_task_unsuccessful() {
    assert_eq!(ExitCode::TaskUnsuccessful.outcome_class(), "task_unsuccessful");
}

#[test]
fn outcome_class_budget_halt() {
    assert_eq!(ExitCode::BudgetHalt.outcome_class(), "budget_halt");
}

#[test]
fn outcome_class_regression_gate_failure() {
    assert_eq!(
        ExitCode::RegressionGateFailure.outcome_class(),
        "regression_gate_failure"
    );
}

#[test]
fn outcome_class_verification_failure() {
    assert_eq!(
        ExitCode::VerificationFailure.outcome_class(),
        "verification_failure"
    );
}

#[test]
fn outcome_class_interrupted() {
    assert_eq!(ExitCode::Interrupted.outcome_class(), "interrupted");
}

#[test]
fn outcome_class_killed() {
    assert_eq!(ExitCode::Killed.outcome_class(), "killed");
}

// ── outcome_class and as_i32 agree (no variant maps two different ways) ───────

#[test]
fn all_variants_have_unique_codes() {
    let variants = [
        ExitCode::Success,
        ExitCode::InternalError,
        ExitCode::UsageError,
        ExitCode::PreflightFailure,
        ExitCode::TaskUnsuccessful,
        ExitCode::BudgetHalt,
        ExitCode::RegressionGateFailure,
        ExitCode::VerificationFailure,
        ExitCode::Interrupted,
        ExitCode::Killed,
    ];
    let codes: Vec<i32> = variants.iter().map(|v| v.as_i32()).collect();
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(codes.len(), sorted.len(), "duplicate exit codes detected");
}

#[test]
fn all_variants_have_unique_outcome_classes() {
    let variants = [
        ExitCode::Success,
        ExitCode::InternalError,
        ExitCode::UsageError,
        ExitCode::PreflightFailure,
        ExitCode::TaskUnsuccessful,
        ExitCode::BudgetHalt,
        ExitCode::RegressionGateFailure,
        ExitCode::VerificationFailure,
        ExitCode::Interrupted,
        ExitCode::Killed,
    ];
    let classes: Vec<&str> = variants.iter().map(|v| v.outcome_class()).collect();
    let mut sorted = classes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(classes.len(), sorted.len(), "duplicate outcome classes detected");
}

// ── from_error mappings ───────────────────────────────────────────────────────

#[test]
fn config_error_maps_to_usage_error() {
    let e = Error::Config(ConfigError::Invalid("bad flag".into()));
    assert_eq!(ExitCode::from_error(&e), ExitCode::UsageError);
}

#[test]
fn config_not_found_maps_to_usage_error() {
    let e = Error::Config(ConfigError::NotFound("cfg.toml".into()));
    assert_eq!(ExitCode::from_error(&e), ExitCode::UsageError);
}

#[test]
fn verification_failed_maps_to_verification_failure() {
    let e = Error::VerificationFailed(1, 3);
    assert_eq!(ExitCode::from_error(&e), ExitCode::VerificationFailure);
}

#[test]
fn model_api_error_maps_to_task_unsuccessful() {
    let e = Error::Model(ModelError::Request("timeout".into()));
    assert_eq!(ExitCode::from_error(&e), ExitCode::TaskUnsuccessful);
}

#[test]
fn model_malformed_maps_to_task_unsuccessful() {
    let e = Error::Model(ModelError::Malformed("no json".into()));
    assert_eq!(ExitCode::from_error(&e), ExitCode::TaskUnsuccessful);
}

#[test]
fn docker_not_installed_maps_to_preflight_failure() {
    let e = Error::Env(EnvError::DockerNotInstalled);
    assert_eq!(ExitCode::from_error(&e), ExitCode::PreflightFailure);
}

#[test]
fn docker_daemon_unreachable_maps_to_preflight_failure() {
    let e = Error::Env(EnvError::DockerDaemonUnreachable("socket closed".into()));
    assert_eq!(ExitCode::from_error(&e), ExitCode::PreflightFailure);
}

#[test]
fn container_start_failed_maps_to_preflight_failure() {
    let e = Error::Env(EnvError::ContainerStartFailed("oom".into()));
    assert_eq!(ExitCode::from_error(&e), ExitCode::PreflightFailure);
}

#[test]
fn env_command_failed_maps_to_task_unsuccessful() {
    let e = Error::Env(EnvError::CommandFailed("exit 1".into()));
    assert_eq!(ExitCode::from_error(&e), ExitCode::TaskUnsuccessful);
}

#[test]
fn env_timeout_maps_to_task_unsuccessful() {
    let e = Error::Env(EnvError::Timeout(std::time::Duration::from_secs(60)));
    assert_eq!(ExitCode::from_error(&e), ExitCode::TaskUnsuccessful);
}

#[test]
fn io_error_maps_to_internal_error() {
    let e = Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "file gone"));
    assert_eq!(ExitCode::from_error(&e), ExitCode::InternalError);
}

#[test]
fn json_error_maps_to_internal_error() {
    let e: Error = serde_json::from_str::<serde_json::Value>("not json")
        .unwrap_err()
        .into();
    assert_eq!(ExitCode::from_error(&e), ExitCode::InternalError);
}

#[test]
fn trajectory_error_maps_to_internal_error() {
    let e = Error::Trajectory("corrupt file".into());
    assert_eq!(ExitCode::from_error(&e), ExitCode::InternalError);
}

#[test]
fn github_error_maps_to_internal_error() {
    let e = Error::Github("api 500".into());
    assert_eq!(ExitCode::from_error(&e), ExitCode::InternalError);
}

#[test]
fn template_error_maps_to_internal_error() {
    let e = Error::Template("render failed".into());
    assert_eq!(ExitCode::from_error(&e), ExitCode::InternalError);
}

// ── cross-surface consistency ─────────────────────────────────────────────────

/// Every outcome_class string is non-empty and uses only snake_case characters.
#[test]
fn outcome_class_strings_are_valid_snake_case_identifiers() {
    let variants = [
        ExitCode::Success,
        ExitCode::InternalError,
        ExitCode::UsageError,
        ExitCode::PreflightFailure,
        ExitCode::TaskUnsuccessful,
        ExitCode::BudgetHalt,
        ExitCode::RegressionGateFailure,
        ExitCode::VerificationFailure,
        ExitCode::Interrupted,
        ExitCode::Killed,
    ];
    for v in variants {
        let s = v.outcome_class();
        assert!(!s.is_empty(), "outcome_class for {v:?} is empty");
        assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "outcome_class {s:?} contains non-snake-case characters"
        );
        assert!(!s.starts_with('_'), "outcome_class {s:?} starts with underscore");
        assert!(!s.ends_with('_'), "outcome_class {s:?} ends with underscore");
    }
}

/// `success` must always be zero; all failure classes must be non-zero.
#[test]
fn only_success_is_zero() {
    let failure_variants = [
        ExitCode::InternalError,
        ExitCode::UsageError,
        ExitCode::PreflightFailure,
        ExitCode::TaskUnsuccessful,
        ExitCode::BudgetHalt,
        ExitCode::RegressionGateFailure,
        ExitCode::VerificationFailure,
        ExitCode::Interrupted,
        ExitCode::Killed,
    ];
    assert_eq!(ExitCode::Success.as_i32(), 0);
    for v in failure_variants {
        assert_ne!(v.as_i32(), 0, "{v:?} must not exit 0");
    }
}

/// Interruption codes match POSIX signal convention (128 + signal number).
#[test]
fn interrupted_and_killed_follow_posix_signal_convention() {
    assert_eq!(ExitCode::Interrupted.as_i32(), 128 + 2);  // SIGINT = 2
    assert_eq!(ExitCode::Killed.as_i32(), 128 + 9);        // SIGKILL = 9
}

/// Constants exported by swebench match the ExitCode values so sweep
/// cancellation produces a documented outcome class.
#[test]
fn sweep_cancel_codes_align_with_exit_code_contract() {
    use rust_swe_agent::run::swebench::{CANCEL_EXIT_CODE_ESCALATED, CANCEL_EXIT_CODE_GRACEFUL};
    assert_eq!(CANCEL_EXIT_CODE_GRACEFUL, ExitCode::Interrupted.as_i32());
    assert_eq!(CANCEL_EXIT_CODE_ESCALATED, ExitCode::Killed.as_i32());
}
