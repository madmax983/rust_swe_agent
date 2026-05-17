//! Stable CLI exit-code contract — TDD tests for issue #98.
//!
//! RED phase: tests compile but assertions / types do not yet exist.
//! GREEN phase: implement `src/exit_code.rs` and wire it into `main.rs`.
//! REFACTOR phase: docs/exit-codes.md documents the contract.

#![allow(clippy::unwrap_used)]

use maxwells_daemon::error::{ConfigError, EnvError, Error, ModelError};
use maxwells_daemon::exit_code::ExitCode;

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
    assert_eq!(
        ExitCode::PreflightFailure.outcome_class(),
        "preflight_failure"
    );
}

#[test]
fn outcome_class_task_unsuccessful() {
    assert_eq!(
        ExitCode::TaskUnsuccessful.outcome_class(),
        "task_unsuccessful"
    );
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
    assert_eq!(
        classes.len(),
        sorted.len(),
        "duplicate outcome classes detected"
    );
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
    let e = Error::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "file gone",
    ));
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
    let e = Error::Template(minijinja::Error::new(
        minijinja::ErrorKind::InvalidOperation,
        "render failed",
    ));
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
        assert!(
            !s.starts_with('_'),
            "outcome_class {s:?} starts with underscore"
        );
        assert!(
            !s.ends_with('_'),
            "outcome_class {s:?} ends with underscore"
        );
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
    assert_eq!(ExitCode::Interrupted.as_i32(), 128 + 2); // SIGINT = 2
    assert_eq!(ExitCode::Killed.as_i32(), 128 + 9); // SIGKILL = 9
}

/// Constants exported by swebench match the ExitCode values so sweep
/// cancellation produces a documented outcome class.
#[test]
fn sweep_cancel_codes_align_with_exit_code_contract() {
    use maxwells_daemon::run::swebench::{CANCEL_EXIT_CODE_ESCALATED, CANCEL_EXIT_CODE_GRACEFUL};
    assert_eq!(CANCEL_EXIT_CODE_GRACEFUL, ExitCode::Interrupted.as_i32());
    assert_eq!(CANCEL_EXIT_CODE_ESCALATED, ExitCode::Killed.as_i32());
}

// ── cross-surface: text label agrees with exit code ───────────────────────────

/// For every Error variant, the outcome_class() string from from_error()
/// and the integer from as_i32() describe the same outcome. This verifies
/// the text and numeric surfaces agree rather than diverging.
#[test]
fn text_and_numeric_surfaces_agree_for_every_error_variant() {
    let error_cases: Vec<(&str, Error)> = vec![
        (
            "usage_error",
            Error::Config(ConfigError::Invalid("x".into())),
        ),
        ("verification_failure", Error::VerificationFailed(1, 2)),
        (
            "preflight_failure",
            Error::Env(EnvError::DockerNotInstalled),
        ),
        (
            "preflight_failure",
            Error::Env(EnvError::DockerDaemonUnreachable("down".into())),
        ),
        (
            "preflight_failure",
            Error::Env(EnvError::ContainerStartFailed("oom".into())),
        ),
        (
            "task_unsuccessful",
            Error::Env(EnvError::CommandFailed("exit 1".into())),
        ),
        (
            "task_unsuccessful",
            Error::Env(EnvError::Timeout(std::time::Duration::from_secs(60))),
        ),
        (
            "task_unsuccessful",
            Error::Model(ModelError::Request("t/o".into())),
        ),
        (
            "task_unsuccessful",
            Error::Model(ModelError::Malformed("bad".into())),
        ),
        ("internal_error", Error::Trajectory("corrupt".into())),
        ("internal_error", Error::Github("api 500".into())),
        (
            "internal_error",
            Error::Template(minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                "render",
            )),
        ),
    ];

    for (expected_class, e) in error_cases {
        let code = ExitCode::from_error(&e);
        assert_eq!(
            code.outcome_class(),
            expected_class,
            "outcome_class mismatch for {:?}: expected {expected_class}, got {}",
            e,
            code.outcome_class()
        );
        assert_ne!(
            code.as_i32(),
            0,
            "failure outcome {expected_class} must not exit 0"
        );
        // Text surface (outcome_class string) and numeric surface (as_i32)
        // must describe the same variant — verified by re-deriving class from code.
        let expected_code = match expected_class {
            "usage_error" => 2,
            "verification_failure" => 7,
            "preflight_failure" => 3,
            "task_unsuccessful" => 4,
            "internal_error" => 1,
            "budget_halt" => 5,
            "regression_gate_failure" => 6,
            other => panic!("unexpected class {other} in test table"),
        };
        assert_eq!(
            code.as_i32(),
            expected_code,
            "numeric code for {expected_class} should be {expected_code}"
        );
    }
}

/// The budget-halt outcome class (5) is distinct from the usage-error class (2)
/// so that `bench swebench --sweep-cost-limit-usd` and
/// `bench forecast --fail-over-cap` can be distinguished from bad invocations.
#[test]
fn budget_halt_is_distinct_from_usage_error() {
    assert_ne!(ExitCode::BudgetHalt.as_i32(), ExitCode::UsageError.as_i32());
    assert_ne!(
        ExitCode::BudgetHalt.outcome_class(),
        ExitCode::UsageError.outcome_class()
    );
    assert_eq!(ExitCode::BudgetHalt.as_i32(), 5);
    assert_eq!(ExitCode::BudgetHalt.outcome_class(), "budget_halt");
}

/// The regression-gate outcome class (6) is distinct from internal error (1)
/// so that `bench compare --max-regressions` failures can be distinguished
/// from infrastructure failures.
#[test]
fn regression_gate_is_distinct_from_internal_error() {
    assert_ne!(
        ExitCode::RegressionGateFailure.as_i32(),
        ExitCode::InternalError.as_i32()
    );
    assert_eq!(ExitCode::RegressionGateFailure.as_i32(), 6);
    assert_eq!(
        ExitCode::RegressionGateFailure.outcome_class(),
        "regression_gate_failure"
    );
}

/// Preflight probe errors use EnvError::DockerDaemonUnreachable so they route
/// to PreflightFailure (3) rather than InternalError (1). Verify the mapping
/// works for the messages written by run_preflight in swebench.rs.
#[test]
fn preflight_probe_errors_map_to_preflight_failure() {
    let cases = [
        "preflight: required binary missing: bash",
        "docker preflight timed out",
        "preflight: dataset.read: timeout exceeded",
        "preflight total timeout exceeded",
    ];
    for msg in cases {
        let e = Error::Env(EnvError::DockerDaemonUnreachable(msg.into()));
        assert_eq!(
            ExitCode::from_error(&e),
            ExitCode::PreflightFailure,
            "message {msg:?} should map to PreflightFailure"
        );
        assert_eq!(ExitCode::from_error(&e).as_i32(), 3);
    }
}

/// Model probe failures (run_preflight's model availability check) map to
/// PreflightFailure (3) — the probe happens before the sweep starts, so it
/// is a preflight condition, not a task execution failure.
#[test]
fn model_probe_failure_maps_to_preflight_failure() {
    let e = Error::Env(EnvError::DockerDaemonUnreachable(
        "preflight: model probe failed: 401 unauthorized".into(),
    ));
    assert_eq!(ExitCode::from_error(&e), ExitCode::PreflightFailure);
    assert_eq!(ExitCode::from_error(&e).as_i32(), 3);
}
