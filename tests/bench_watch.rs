//! `bench watch`: live single-instance trajectory follower.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use rust_swe_agent::trajectory::{Trajectory, outcome};

mod support;
use support::binary_path;

fn write_traj(dir: &Path, instance_id: &str, outcome_val: Option<&str>) {
    let mut t = Trajectory::new();
    t.info.model_name = Some("test-model".into());
    t.info.outcome = outcome_val.map(str::to_owned);

    let mut asst = rust_swe_agent::model::Message::assistant("```bash\necho hello\n```");
    asst.extra.actions = Some(vec!["echo hello".into()]);
    t.record_message(&asst);

    let mut obs = rust_swe_agent::model::Message::user("output");
    obs.extra.other.insert(
        "run_result".into(),
        serde_json::json!({
            "stdout": "hello\n",
            "stderr": "",
            "exit_code": 0,
            "timed_out": false
        }),
    );
    t.record_message(&obs);

    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();
}

// Test (b): --wait-secs=0 against a missing file exits 1
#[test]
fn wait_secs_zero_missing_file_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            "nonexistent-instance",
            "--wait-secs",
            "0",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 for missing file, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Test (a): Complete trajectory exits 0 and shows turn content
#[test]
fn complete_trajectory_exits_0_with_turns() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(dir.path(), "my-instance", Some(outcome::SUBMITTED));

    let out = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            "my-instance",
        ])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0 for complete trajectory, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Should contain turn output from the assistant step
    assert!(
        stdout.contains("assistant") || stdout.contains("step"),
        "expected turn output in stdout, got: {stdout}"
    );
    // Should contain the final summary
    assert!(
        stdout.contains("complete") || stdout.contains("submitted"),
        "expected completion summary in stdout, got: {stdout}"
    );
}

// Test (c): --ndjson mode produces valid NDJSON that round-trips through serde_json
#[test]
fn ndjson_mode_produces_valid_ndjson() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(dir.path(), "ndjson-instance", Some(outcome::SUBMITTED));

    let out = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            "ndjson-instance",
            "--ndjson",
        ])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0 in ndjson mode, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut found_turn = false;
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSON line: {e}\nline: {line}"));
        assert!(
            parsed.get("schema_version").is_some(),
            "missing schema_version field in: {line}"
        );
        assert!(
            parsed.get("turn_index").is_some(),
            "missing turn_index field in: {line}"
        );
        assert!(
            parsed.get("role").is_some(),
            "missing role field in: {line}"
        );
        found_turn = true;
    }
    assert!(found_turn, "expected at least one NDJSON turn line, stdout: {stdout}");
}

// Test (d): Redaction strips a planted secret token before stdout
#[test]
fn redaction_strips_secret_from_stdout() {
    let dir = tempfile::tempdir().unwrap();

    let mut t = Trajectory::new();
    t.info.model_name = Some("test-model".into());
    t.info.outcome = Some(outcome::SUBMITTED.into());

    let mut asst = rust_swe_agent::model::Message::assistant("```bash\necho test\n```");
    asst.extra.actions = Some(vec!["echo test".into()]);
    t.record_message(&asst);

    let mut obs = rust_swe_agent::model::Message::user("output");
    obs.extra.other.insert(
        "run_result".into(),
        serde_json::json!({
            "stdout": "ghp_SECRETTOKEN12345678901234567890AB output",
            "stderr": "",
            "exit_code": 0,
            "timed_out": false
        }),
    );
    t.record_message(&obs);

    std::fs::write(
        dir.path().join("secret-instance.traj.json"),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            "secret-instance",
        ])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("ghp_SECRETTOKEN12345678901234567890AB"),
        "secret token should be redacted from stdout, but found in: {stdout}"
    );
}

// Test (e): Invalid sweep directory exits 2
#[test]
fn invalid_sweep_dir_exits_2() {
    let out = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            "/nonexistent/path/does/not/exist",
            "--instance",
            "some-instance",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 for invalid sweep dir, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
