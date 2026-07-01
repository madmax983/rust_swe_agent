//! `agent suite --rerun-failed` — re-run only failed pack tasks (issue #825).
//!
//! CLI-surface tests: flag wiring, the `--resume` mutual-exclusion guard,
//! and the "no prior results" guard. The selection/carry-forward logic
//! itself (which tasks count as "failed", zero-cost carry-forward, cost-cap
//! composition) is covered by the in-crate async tests in
//! `src/run/suite.rs` (`rerun_failed_*`), which can drive a full suite run
//! deterministically without a real model call.
//!
//! RED phase: these tests fail until `--rerun-failed` exists on `agent suite`.
//! GREEN phase: the `--rerun-failed` flag + `suite::run` wiring (issue #825).
//! REFACTOR phase: clean up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

mod support;
use support::binary_path;

use maxwells_daemon::exit_code::ExitCode;

fn write_tasks(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    path
}

/// A `Command` for the `max` binary with no provider credentials set, so any
/// accidental real model call would fail loudly rather than silently spend.
fn no_credentials_command() -> Command {
    let mut cmd = Command::new(binary_path());
    cmd.env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY");
    cmd
}

#[test]
fn cli_rejects_rerun_failed_combined_with_resume() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix the bug\n");
    let output = dir.path().join("runs");

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .arg("--output")
        .arg(&output)
        .args(["--resume", "--rerun-failed"])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "--rerun-failed combined with --resume must exit non-zero\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("resume") || stderr.contains("rerun-failed"),
        "error message must reference the conflicting flags;\nstderr:\n{stderr}"
    );
    // No output directory contents should have been created — clap rejects
    // the combination before the command body ever runs.
    assert!(
        !output.join("t1.traj.json").exists(),
        "no task should have run"
    );
}

#[test]
fn cli_rejects_rerun_failed_with_no_prior_results() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix the bug\n");
    let output = dir.path().join("runs");

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .arg("--output")
        .arg(&output)
        .args(["--rerun-failed", "--suite-name", "never-run-before"])
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(ExitCode::UsageError.as_i32()),
        "expected exit {} (usage_error)\nstdout: {}\nstderr: {}",
        ExitCode::UsageError.as_i32(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no suite-results.json"),
        "stderr should explain the gap: {stderr}"
    );
}

#[test]
fn cli_help_documents_rerun_failed() {
    let out = Command::new(binary_path())
        .args(["agent", "suite", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--rerun-failed"),
        "help text should document --rerun-failed: {stdout}"
    );
}
