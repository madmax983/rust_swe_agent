//! Verification evidence for local runs — TDD tests for issue #94.
//!
//! RED phase: all tests below assert behaviour that does not yet exist.
//! GREEN phase: implement `run_verification_checks` in `run/mini.rs`.

#![allow(clippy::unwrap_used)]

use maxwells_daemon::{
    config::Config,
    run::mini::{InteractiveMode, MiniArgs, run},
    trajectory::{VerificationCheck, verification_status},
};

mod support;
use support::binary_path;

// ── helpers ─────────────────────────────────────────────────────────────────

fn mini_args(work: &tempfile::TempDir, name: &str, checks: Vec<VerificationCheck>) -> MiniArgs {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    MiniArgs {
        driver: maxwells_daemon::run::mini::RunDriver::Builtin,
        task: "test task".into(),
        extra_context: None,
        config: cfg,
        output_dir: work.path().to_path_buf(),
        trajectory_name: name.into(),
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
        ]),
        deterministic_usage_per_call: None,
        task_timeout_secs: Some(30),
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: checks,
        verification_timeout_secs: 10,
        interactive_mode: InteractiveMode::Off,
        resume_from: None,
        trace_id: None,
        webhook_url: None,
        webhook_headers: vec![],
        event_log: None,
        event_log_instance_id: None,
        local_workdir: None,
        read_only: false,
        allow_mcp_in_read_only: false,
        rehearsal_gold_patch: None,
        no_step_persist: false,
        parent_sweep_run_id: None,
        continue_from: None,
    }
}

fn read_traj(work: &tempfile::TempDir, name: &str) -> serde_json::Value {
    let path = work.path().join(format!("{name}.traj.json"));
    let json = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(&json).unwrap()
}

fn pass_command() -> &'static str {
    if cfg!(windows) { "exit /B 0" } else { "true" }
}

fn fail_command() -> &'static str {
    if cfg!(windows) { "exit /B 1" } else { "false" }
}

fn slow_command() -> &'static str {
    if cfg!(windows) {
        "for /L %i in (1,0,2) do @rem"
    } else {
        "sleep 300"
    }
}

fn big_output_command() -> &'static str {
    if cfg!(windows) {
        "for /L %i in (1,1,1000) do @echo %i"
    } else {
        "seq 1 1000"
    }
}

// ── RED-phase tests ──────────────────────────────────────────────────────────

/// AC: no verification check supplied → trajectory has verification_status: "unverified".
#[tokio::test]
async fn no_verification_check_sets_unverified_status() {
    let work = tempfile::tempdir().unwrap();
    run(mini_args(&work, "no-checks", vec![])).await.unwrap();
    let traj = read_traj(&work, "no-checks");
    assert_eq!(
        traj["info"]["verification_status"].as_str(),
        Some(verification_status::UNVERIFIED),
        "expected verification_status=unverified; traj={traj}"
    );
    // No checks → results array absent or empty
    assert!(
        traj["info"]["verification_results"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "expected empty verification_results; traj={traj}"
    );
}

/// AC: one passing check → trajectory has verification_status: "verified".
#[tokio::test]
async fn single_passing_check_sets_verified_status() {
    let work = tempfile::tempdir().unwrap();
    let checks = vec![VerificationCheck {
        name: "always-pass".into(),
        command: pass_command().into(),
    }];
    let result = run(mini_args(&work, "passing-check", checks)).await;
    assert!(
        result.is_ok(),
        "expected Ok for passing check; got {result:?}"
    );

    let traj = read_traj(&work, "passing-check");
    assert_eq!(
        traj["info"]["verification_status"].as_str(),
        Some(verification_status::VERIFIED),
        "expected verified; traj={traj}"
    );
    let results = traj["info"]["verification_results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "expected 1 check result");
    assert_eq!(results[0]["name"].as_str(), Some("always-pass"));
    assert_eq!(results[0]["passed"].as_bool(), Some(true));
    assert_eq!(results[0]["exit_code"].as_i64(), Some(0));
    assert!(
        results[0]["duration_ms"].as_u64().is_some(),
        "expected duration_ms present"
    );
}

/// AC: one failing check → verification_status: "verification_failed", process exits non-zero.
#[tokio::test]
async fn single_failing_check_sets_verification_failed_and_returns_error() {
    let work = tempfile::tempdir().unwrap();
    let checks = vec![VerificationCheck {
        name: "always-fail".into(),
        command: fail_command().into(),
    }];
    let result = run(mini_args(&work, "failing-check", checks)).await;
    assert!(result.is_err(), "expected Err for failing check");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("verification failed"),
        "expected 'verification failed' in error; got: {err_msg}"
    );

    let traj = read_traj(&work, "failing-check");
    assert_eq!(
        traj["info"]["verification_status"].as_str(),
        Some(verification_status::VERIFICATION_FAILED),
        "expected verification_failed; traj={traj}"
    );
    let results = traj["info"]["verification_results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["passed"].as_bool(), Some(false));
    // Failure command exits non-zero on every supported local shell.
    assert_ne!(
        results[0]["exit_code"].as_i64(),
        Some(0),
        "expected non-zero exit_code"
    );
}

/// AC: multiple checks all pass → verified.
#[tokio::test]
async fn multiple_checks_all_pass_sets_verified() {
    let work = tempfile::tempdir().unwrap();
    let checks = vec![
        VerificationCheck {
            name: "check-1".into(),
            command: pass_command().into(),
        },
        VerificationCheck {
            name: "check-2".into(),
            command: "echo hello".into(),
        },
    ];
    let result = run(mini_args(&work, "all-pass", checks)).await;
    assert!(result.is_ok(), "expected Ok; got {result:?}");

    let traj = read_traj(&work, "all-pass");
    assert_eq!(
        traj["info"]["verification_status"].as_str(),
        Some(verification_status::VERIFIED)
    );
    assert_eq!(
        traj["info"]["verification_results"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "expected 2 check results"
    );
}

/// AC: mixed results → verification_failed.
#[tokio::test]
async fn multiple_checks_mixed_results_sets_verification_failed() {
    let work = tempfile::tempdir().unwrap();
    let checks = vec![
        VerificationCheck {
            name: "pass".into(),
            command: pass_command().into(),
        },
        VerificationCheck {
            name: "fail".into(),
            command: fail_command().into(),
        },
        VerificationCheck {
            name: "pass2".into(),
            command: "echo ok".into(),
        },
    ];
    let result = run(mini_args(&work, "mixed", checks)).await;
    assert!(result.is_err(), "expected Err for mixed (some failing)");

    let traj = read_traj(&work, "mixed");
    assert_eq!(
        traj["info"]["verification_status"].as_str(),
        Some(verification_status::VERIFICATION_FAILED)
    );
    let results = traj["info"]["verification_results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    let passed: usize = results
        .iter()
        .filter(|r| r["passed"].as_bool() == Some(true))
        .count();
    let failed: usize = results
        .iter()
        .filter(|r| r["passed"].as_bool() == Some(false))
        .count();
    assert_eq!(passed, 2);
    assert_eq!(failed, 1);
}

/// AC: verification result captures name, command, exit_code, duration, stdout/stderr preview.
#[tokio::test]
async fn verification_result_captures_required_fields() {
    let work = tempfile::tempdir().unwrap();
    let checks = vec![VerificationCheck {
        name: "with-output".into(),
        command: "echo hello_from_verify".into(),
    }];
    run(mini_args(&work, "capture-output", checks))
        .await
        .unwrap();

    let traj = read_traj(&work, "capture-output");
    let r = &traj["info"]["verification_results"][0];
    assert_eq!(r["name"].as_str(), Some("with-output"));
    assert_eq!(r["command"].as_str(), Some("echo hello_from_verify"));
    assert_eq!(r["exit_code"].as_i64(), Some(0));
    assert!(r["duration_ms"].as_u64().is_some());
    assert_eq!(r["passed"].as_bool(), Some(true));
    assert!(
        r["stdout_preview"]
            .as_str()
            .unwrap_or("")
            .contains("hello_from_verify"),
        "stdout_preview should contain command output; result={r}"
    );
}

/// AC: timed-out check is treated as failure.
#[tokio::test]
async fn timed_out_check_treated_as_failure() {
    let work = tempfile::tempdir().unwrap();
    let mut args = mini_args(
        &work,
        "timeout-check",
        vec![VerificationCheck {
            name: "slow".into(),
            command: slow_command().into(),
        }],
    );
    args.verification_timeout_secs = 1;

    let result = run(args).await;
    assert!(result.is_err(), "expected Err for timed-out check");

    let traj = read_traj(&work, "timeout-check");
    assert_eq!(
        traj["info"]["verification_status"].as_str(),
        Some(verification_status::VERIFICATION_FAILED)
    );
    let results = traj["info"]["verification_results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["passed"].as_bool(), Some(false));
    assert_eq!(results[0]["timed_out"].as_bool(), Some(true));
}

/// AC: Ctrl-C during verification produces `unverified`, not `verification_failed`.
#[tokio::test]
async fn cancellation_during_verification_sets_unverified() {
    use maxwells_daemon::env::CancellationToken;

    let work = tempfile::tempdir().unwrap();
    // Pre-fire the cancellation token so the first verification check is
    // immediately interrupted before it can complete.
    let (tx, rx) = tokio::sync::watch::channel(false);
    tx.send(true).unwrap();
    let cancel = CancellationToken::new(rx);

    let mut args = mini_args(
        &work,
        "cancelled-verify",
        vec![VerificationCheck {
            name: "slow".into(),
            command: slow_command().into(),
        }],
    );
    args.cancellation = Some(cancel);

    // Should NOT return VerificationFailed — the run was cancelled.
    let result = run(args).await;
    assert!(
        result.is_ok(),
        "expected Ok (cancelled, not verification_failed); got {result:?}"
    );

    let traj = read_traj(&work, "cancelled-verify");
    assert_eq!(
        traj["info"]["verification_status"].as_str(),
        Some(verification_status::UNVERIFIED),
        "expected unverified after cancellation; traj={traj}"
    );
}

/// Large verification output is truncated to VERIFICATION_PREVIEW_MAX_BYTES.
#[tokio::test]
async fn large_verification_output_is_truncated() {
    let work = tempfile::tempdir().unwrap();
    // The platform helper produces >2048 bytes.
    let checks = vec![VerificationCheck {
        name: "big-output".into(),
        command: big_output_command().into(),
    }];
    run(mini_args(&work, "big-output", checks)).await.unwrap();

    let traj = read_traj(&work, "big-output");
    let preview = traj["info"]["verification_results"][0]["stdout_preview"]
        .as_str()
        .unwrap_or("");
    assert!(
        preview.len() <= 2048,
        "stdout_preview should be truncated to ≤2048 bytes; got {}",
        preview.len()
    );
    assert!(!preview.is_empty(), "stdout_preview should not be empty");
}

/// inspect renders timed_out=true with a "(timed_out)" note.
#[test]
fn inspect_shows_timed_out_note_in_verification_detail() {
    use maxwells_daemon::trajectory::{Trajectory, VerificationResult, outcome};

    let sweep = tempfile::tempdir().unwrap();
    let mut t = Trajectory::new();
    t.info.outcome = Some(outcome::SUBMITTED.into());
    t.info.verification_status = Some(verification_status::VERIFICATION_FAILED.into());
    t.info.verification_results = vec![VerificationResult {
        name: "slow-check".into(),
        command: "sleep 300".into(),
        exit_code: -1,
        duration_ms: 1000,
        passed: false,
        stdout_preview: String::new(),
        stderr_preview: "timed out after 1s".into(),
        timed_out: true,
    }];
    std::fs::write(
        sweep.path().join("timedout.traj.json"),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "timedout",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("(timed_out)"),
        "expected '(timed_out)' in inspect output; stdout={stdout}"
    );
}

/// AC: `bench inspect` shows verification summary in the run header.
#[test]
fn inspect_shows_verification_summary_in_header() {
    use maxwells_daemon::trajectory::{Trajectory, VerificationResult, outcome};

    let sweep = tempfile::tempdir().unwrap();
    let mut t = Trajectory::new();
    t.info.outcome = Some(outcome::SUBMITTED.into());
    t.info.verification_status = Some(verification_status::VERIFIED.into());
    t.info.verification_results = vec![VerificationResult {
        name: "unit-tests".into(),
        command: "cargo test -q".into(),
        exit_code: 0,
        duration_ms: 1234,
        passed: true,
        stdout_preview: "test ok".into(),
        stderr_preview: String::new(),
        timed_out: false,
    }];
    std::fs::write(
        sweep.path().join("abc.traj.json"),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "abc",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("verification:"),
        "expected 'verification:' in header; stdout={stdout}"
    );
    assert!(
        stdout.contains("verified"),
        "expected status 'verified'; stdout={stdout}"
    );
    assert!(
        stdout.contains("unit-tests"),
        "expected check name 'unit-tests'; stdout={stdout}"
    );
}

/// AC: inspect output for unverified status (no checks supplied).
#[test]
fn inspect_shows_unverified_status_in_header() {
    use maxwells_daemon::trajectory::{Trajectory, outcome};

    let sweep = tempfile::tempdir().unwrap();
    let mut t = Trajectory::new();
    t.info.outcome = Some(outcome::SUBMITTED.into());
    t.info.verification_status = Some(verification_status::UNVERIFIED.into());
    // verification_results stays empty — that's the no-checks case
    std::fs::write(
        sweep.path().join("unverified.traj.json"),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "unverified",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("verification:"),
        "expected 'verification:' in header; stdout={stdout}"
    );
    assert!(
        stdout.contains("unverified"),
        "expected status 'unverified'; stdout={stdout}"
    );
}

/// AC: verification evidence is included in machine-readable run artifacts.
#[test]
fn inspect_json_includes_verification_evidence() {
    use maxwells_daemon::trajectory::{Trajectory, VerificationResult, outcome};

    let sweep = tempfile::tempdir().unwrap();
    let mut t = Trajectory::new();
    t.info.outcome = Some(outcome::SUBMITTED.into());
    t.info.verification_status = Some(verification_status::VERIFICATION_FAILED.into());
    t.info.verification_results = vec![
        VerificationResult {
            name: "pass-check".into(),
            command: "true".into(),
            exit_code: 0,
            duration_ms: 5,
            passed: true,
            stdout_preview: String::new(),
            stderr_preview: String::new(),
            timed_out: false,
        },
        VerificationResult {
            name: "fail-check".into(),
            command: "false".into(),
            exit_code: 1,
            duration_ms: 3,
            passed: false,
            stdout_preview: String::new(),
            stderr_preview: String::new(),
            timed_out: false,
        },
    ];
    std::fs::write(
        sweep.path().join("xyz.traj.json"),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "xyz",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["verification_status"].as_str(),
        Some(verification_status::VERIFICATION_FAILED),
        "expected verification_failed in json; v={v}"
    );
    let results = v["verification_results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["name"].as_str(), Some("pass-check"));
    assert_eq!(results[0]["passed"].as_bool(), Some(true));
    assert_eq!(results[1]["name"].as_str(), Some("fail-check"));
    assert_eq!(results[1]["passed"].as_bool(), Some(false));
}
