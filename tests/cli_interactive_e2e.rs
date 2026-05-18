//! End-to-end CLI behaviour for `mini --interactive` (issue #312).
//!
//! Spawns the compiled `max` binary as a subprocess to exercise the
//! non-TTY rejection path. The happy in-process path is covered by
//! `tests/interactive_confirm.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Stdio};

mod support;
use support::binary_path;

#[test]
fn non_tty_without_yolo_rejects_with_usage_error() {
    let bin = binary_path();
    let out = Command::new(&bin)
        .args([
            "mini",
            "--task",
            "noop",
            "--model",
            "deterministic",
            "--env",
            "local",
            "--step-limit",
            "1",
            "--interactive",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn `max mini`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "expected non-zero exit; stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("requires a TTY"),
        "stderr missing TTY guidance: {stderr}"
    );
    assert!(
        stderr.contains("--yolo"),
        "stderr missing --yolo hint: {stderr}"
    );
}

#[test]
fn yolo_alone_does_not_require_tty() {
    let bin = binary_path();
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(&bin)
        .args([
            "mini",
            "--task",
            "noop",
            "--model",
            "deterministic",
            "--env",
            "local",
            "--step-limit",
            "1",
            "--yolo",
            "--output",
            dir.path().to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn `max mini --yolo`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("requires a TTY"),
        "yolo run should not surface the TTY error; stderr={stderr}"
    );
}
