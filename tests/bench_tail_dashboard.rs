//! CLI-level behavior for `bench tail --ui ratatui` (issue #641).
//!
//! Spawns the compiled `max` binary to exercise flag validation and the
//! non-TTY rejection path — the happy interactive path needs a real TTY and
//! is covered instead by `src/run/sweep_dashboard.rs`'s `TestBackend` unit
//! tests and `instance_rows`/`snapshot` coverage in `tests/bench_tail.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Stdio};

mod support;
use support::binary_path;

fn write_minimal_sweep(dir: &std::path::Path) {
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string(&serde_json::json!({
            "total": 1,
            "submitted": 0,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 0,
            "instances": []
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn ratatui_ui_rejects_non_tty_with_clear_message() {
    let bin = binary_path();
    let dir = tempfile::tempdir().unwrap();
    write_minimal_sweep(dir.path());
    let out = Command::new(&bin)
        .args([
            "bench",
            "tail",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--ui",
            "ratatui",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn `max bench tail --ui ratatui`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected non-zero exit; stderr={stderr}"
    );
    assert!(
        stderr.contains("requires a TTY"),
        "stderr missing TTY guidance: {stderr}"
    );
}

#[test]
fn ratatui_ui_rejects_once() {
    let bin = binary_path();
    let dir = tempfile::tempdir().unwrap();
    write_minimal_sweep(dir.path());
    let out = Command::new(&bin)
        .args([
            "bench",
            "tail",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--ui",
            "ratatui",
            "--once",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn `max bench tail --ui ratatui --once`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected non-zero exit; stderr={stderr}"
    );
    assert!(
        stderr.contains("--once"),
        "stderr missing --once guidance: {stderr}"
    );
}

#[test]
fn ratatui_ui_rejects_json_format() {
    let bin = binary_path();
    let dir = tempfile::tempdir().unwrap();
    write_minimal_sweep(dir.path());
    let out = Command::new(&bin)
        .args([
            "bench",
            "tail",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--ui",
            "ratatui",
            "--format",
            "json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn `max bench tail --ui ratatui --format json`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected non-zero exit; stderr={stderr}"
    );
    assert!(
        stderr.contains("--format json"),
        "stderr missing --format json guidance: {stderr}"
    );
}

#[test]
fn default_ui_is_unaffected_by_the_new_flag() {
    let bin = binary_path();
    let dir = tempfile::tempdir().unwrap();
    write_minimal_sweep(dir.path());
    let out = Command::new(&bin)
        .args([
            "bench",
            "tail",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--once",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn `max bench tail --once`");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("=== bench tail ==="), "{stdout}");
}
