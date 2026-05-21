#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::process::Command;

fn run_mini(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let out = Command::new(support::binary_path())
        .args(["--log", "error", "mini"])
        .args(args)
        .output()
        .expect("failed to spawn binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status, stdout, stderr)
}

// ── AC 2: Validation of non-existent/invalid workdirs ────────────────────────

#[test]
fn nonexistent_workdir_fails() {
    let (status, _stdout, stderr) = run_mini(&[
        "--workdir",
        "nonexistent_dir_xyz_123",
        "--task",
        "hello",
        "--render-only",
    ]);
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains(
            "error: --workdir nonexistent_dir_xyz_123 does not exist or is not a directory"
        ),
        "stderr must report invalid path: {stderr}"
    );
    assert!(
        stderr.contains("outcome_class: usage_error"),
        "stderr must print stable outcome class usage_error: {stderr}"
    );
}

#[test]
fn file_workdir_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("not_a_dir.txt");
    fs::write(&file_path, "content").unwrap();

    let path_str = file_path.to_str().unwrap();
    let (status, _stdout, stderr) =
        run_mini(&["--workdir", path_str, "--task", "hello", "--render-only"]);
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains(&format!(
            "error: --workdir {path_str} does not exist or is not a directory"
        )),
        "stderr must report invalid path: {stderr}"
    );
}

// ── AC 3: Docker Mutex Validation ────────────────────────────────────────────

#[test]
fn docker_workdir_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let (status, _stdout, stderr) = run_mini(&[
        "--workdir",
        temp_dir.path().to_str().unwrap(),
        "--env",
        "docker",
        "--task",
        "hello",
        "--render-only",
    ]);
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("docker container workdir is fixed"),
        "stderr must report docker mutual exclusion: {stderr}"
    );
}

// ── AC 5: Render-Only Outputs ────────────────────────────────────────────────

#[test]
fn render_only_prints_workdir_text_and_json() {
    let temp_dir = tempfile::tempdir().unwrap();
    let canonicalized = fs::canonicalize(temp_dir.path()).unwrap();
    let canonicalized_str = canonicalized.to_str().unwrap();

    // 1. Check JSON output
    let (status, stdout_json, _stderr) = run_mini(&[
        "--workdir",
        temp_dir.path().to_str().unwrap(),
        "--task",
        "hello",
        "--render-only",
        "--format",
        "json",
    ]);
    assert!(status.success());
    let parsed: serde_json::Value = serde_json::from_str(&stdout_json).unwrap();
    let local_workdir_val = parsed
        .get("local_workdir")
        .and_then(|v| v.as_str())
        .unwrap();

    // Convert to uppercase for case-insensitive comparison on Windows drive letters
    assert_eq!(
        local_workdir_val.to_uppercase(),
        canonicalized_str.to_uppercase()
    );

    // 2. Check Text output
    let (status_text, stdout_text, _stderr_text) = run_mini(&[
        "--workdir",
        temp_dir.path().to_str().unwrap(),
        "--task",
        "hello",
        "--render-only",
        "--format",
        "text",
    ]);
    assert!(status_text.success());
    assert!(
        stdout_text
            .to_uppercase()
            .contains(&canonicalized_str.to_uppercase()),
        "text output must contain local_workdir: {stdout_text}"
    );
}

// ── AC: Resumption Workdir Validation ────────────────────────────────────────

#[test]
fn resume_fails_if_trajectory_workdir_nonexistent() {
    let temp_dir = tempfile::tempdir().unwrap();
    let traj_path = temp_dir.path().join("resume-test.traj.json");

    // Write a mock trajectory with a nonexistent local_workdir
    let traj_json = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.3",
        "info": {
            "partial": true,
            "outcome": null,
            "exit_reason": null,
            "task": "some task",
            "model_name": "claude-opus-4-7",
            "local_workdir": "nonexistent_resume_dir_abc_123"
        },
        "messages": [
            { "role": "system", "content": "system" },
            { "role": "user", "content": "user" }
        ]
    });
    fs::write(
        &traj_path,
        serde_json::to_string_pretty(&traj_json).unwrap(),
    )
    .unwrap();

    let (status, _stdout, stderr) = run_mini(&["--resume", traj_path.to_str().unwrap()]);

    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("nonexistent_resume_dir_abc_123 does not exist or is not a directory"),
        "stderr must report invalid trajectory workdir: {stderr}"
    );
}

#[test]
fn resume_cli_workdir_overrides_trajectory() {
    let temp_dir = tempfile::tempdir().unwrap();
    let traj_path = temp_dir.path().join("resume-test.traj.json");

    // Write a mock trajectory with a valid (empty) local_workdir
    let traj_json = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.3",
        "info": {
            "partial": true,
            "outcome": null,
            "exit_reason": null,
            "task": "some task",
            "model_name": "claude-opus-4-7",
            "local_workdir": null
        },
        "messages": [
            { "role": "system", "content": "system" },
            { "role": "user", "content": "user" }
        ]
    });
    fs::write(
        &traj_path,
        serde_json::to_string_pretty(&traj_json).unwrap(),
    )
    .unwrap();

    // Passing a nonexistent CLI workdir must override/fail before checking the traj workdir
    let (status, _stdout, stderr) = run_mini(&[
        "--resume",
        traj_path.to_str().unwrap(),
        "--workdir",
        "nonexistent_cli_dir_override_456",
    ]);

    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("nonexistent_cli_dir_override_456 does not exist or is not a directory"),
        "stderr must report invalid overridden CLI workdir: {stderr}"
    );
}
