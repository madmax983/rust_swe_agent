//! `bench watch`: live single-instance trajectory follower.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

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

// Test (a): Attach to a trajectory being written by a test harness; observe turns in order
#[test]
fn streams_turns_from_in_flight_trajectory() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = "streaming-instance";
    let traj_path = dir.path().join(format!("{instance_id}.traj.json"));

    // Write an initial non-terminal trajectory with one turn (still in-flight)
    {
        let mut t = Trajectory::new();
        let mut asst = rust_swe_agent::model::Message::assistant("turn-one-content");
        asst.extra.actions = Some(vec!["echo turn1".into()]);
        t.record_message(&asst);
        std::fs::write(&traj_path, serde_json::to_string_pretty(&t).unwrap()).unwrap();
    }

    // Start bench watch in background
    let child = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            instance_id,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Let watch read and print the first turn
    std::thread::sleep(Duration::from_millis(400));

    // Write a second turn and mark terminal — simulates the worker finishing
    {
        let mut t = Trajectory::new();
        t.info.outcome = Some(outcome::SUBMITTED.into());
        let mut asst1 = rust_swe_agent::model::Message::assistant("turn-one-content");
        asst1.extra.actions = Some(vec!["echo turn1".into()]);
        t.record_message(&asst1);
        let mut asst2 = rust_swe_agent::model::Message::assistant("turn-two-content");
        asst2.extra.actions = Some(vec!["echo turn2".into()]);
        t.record_message(&asst2);
        std::fs::write(&traj_path, serde_json::to_string_pretty(&t).unwrap()).unwrap();
    }

    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("turn-one-content"),
        "expected turn 1 in stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("turn-two-content"),
        "expected turn 2 in stdout, got: {stdout}"
    );
}

// Test (a) also: complete pre-written trajectory exits 0 and shows turns
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
    assert!(
        stdout.contains("assistant") || stdout.contains("step"),
        "expected turn output in stdout, got: {stdout}"
    );
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
    assert!(
        found_turn,
        "expected at least one NDJSON turn line, stdout: {stdout}"
    );
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

// Test: Invalid sweep directory exits 2
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

// Test: --sweep pointing at a file (not a directory) exits 2
#[test]
fn sweep_is_file_not_dir_exits_2() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let out = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            file.path().to_str().unwrap(),
            "--instance",
            "some-instance",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 when sweep is a file, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Test: --run-index 2 attaches to the correct run slot
#[test]
fn run_index_2_reads_correct_slot() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = "rerun-instance";
    let instance_dir = dir.path().join(instance_id);
    std::fs::create_dir_all(&instance_dir).unwrap();

    // Write run-1 as already-complete with a different outcome to ensure
    // bench watch selects run-2 and not run-1.
    write_traj(dir.path(), instance_id, None);
    // write_traj writes to <sweep>/<id>.traj.json (flat); also write a nested run-1
    std::fs::write(
        instance_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&{
            let mut t = rust_swe_agent::trajectory::Trajectory::new();
            t.info.outcome = Some(rust_swe_agent::trajectory::outcome::SUBMITTED.into());
            t
        })
        .unwrap(),
    )
    .unwrap();
    // run-2: complete trajectory with a unique assistant message
    let mut t2 = rust_swe_agent::trajectory::Trajectory::new();
    t2.info.outcome = Some(rust_swe_agent::trajectory::outcome::SUBMITTED.into());
    let mut asst = rust_swe_agent::model::Message::assistant("run-two-unique-content");
    asst.extra.actions = Some(vec!["echo run2".into()]);
    t2.record_message(&asst);
    std::fs::write(
        instance_dir.join("run-2.traj.json"),
        serde_json::to_string_pretty(&t2).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            instance_id,
            "--run-index",
            "2",
            "--wait-secs",
            "0",
        ])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0 for run-index 2, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("run-two-unique-content"),
        "expected run-2 content in stdout, got: {stdout}"
    );
}

// Test: --run-index N for missing slot exits 1 when --wait-secs 0
#[test]
fn run_index_missing_slot_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            "some-instance",
            "--run-index",
            "3",
            "--wait-secs",
            "0",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 for missing run-3 slot, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Test (e): Stall warning fires after --stall-secs and watch keeps following
#[test]
fn stall_warning_fires_and_watch_keeps_following() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = "stall-instance";

    // Write an in-flight (non-terminal) trajectory with one turn
    let mut t = Trajectory::new();
    let mut asst = rust_swe_agent::model::Message::assistant("initial turn");
    asst.extra.actions = Some(vec!["echo hi".into()]);
    t.record_message(&asst);
    std::fs::write(
        dir.path().join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();

    // Start bench watch with stall-secs=1
    let mut child = Command::new(binary_path())
        .args([
            "bench",
            "watch",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            instance_id,
            "--stall-secs",
            "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait long enough for the stall to fire (stall-secs=1, wait 2.5s)
    std::thread::sleep(Duration::from_millis(2500));

    // Watch should still be running — stall must not cause an exit
    assert!(
        child.try_wait().unwrap().is_none(),
        "bench watch should still be running after a stall — stall must not exit"
    );

    // Kill the process and collect output
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[stalled:"),
        "expected stall warning in stderr, got: {stderr}"
    );
}
