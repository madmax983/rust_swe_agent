//! Integration tests for the new `bench fork` subcommand.

#![allow(clippy::unwrap_used, clippy::uninlined_format_args, clippy::float_cmp)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

mod support;
use support::binary_path;

/// Build a minimal 2-step trajectory without fingerprints (simulates legacy).
fn make_legacy_trajectory(path: &Path) {
    let traj = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.1",
        "info": {
            "task": "dummy task",
            "model_name": "deterministic-test",
            "outcome": "submitted",
            "steps": 2,
            "total_cost_usd": 0.50
        },
        "messages": [
            {
                "role": "user",
                "content": "dummy task",
                "extra": {}
            },
            {
                "role": "assistant",
                "content": "```bash\necho step0\n```",
                "extra": {
                    "actions": ["echo step0"]
                }
            },
            {
                "role": "user",
                "content": "Exit code: 0\nOutput:\nstep0",
                "extra": {
                    "run_result": {
                        "stdout": "step0\n",
                        "stderr": "",
                        "exit_code": 0,
                        "timed_out": false
                    }
                }
            },
            {
                "role": "assistant",
                "content": "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nstep1\n```",
                "extra": {
                    "actions": ["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT"]
                }
            }
        ]
    });
    fs::write(path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();
}

/// Helper to run replay on a legacy trajectory to produce a correctly fingerprinted parent trajectory.
fn record_fingerprinted_trajectory(
    legacy_path: &Path,
    sweep_dir: &Path,
    instance_name: &str,
) -> PathBuf {
    let out = Command::new(binary_path())
        .args([
            "replay",
            "--trajectory-path",
            legacy_path.to_str().unwrap(),
            "--output",
            sweep_dir.to_str().unwrap(),
            "--trajectory-name",
            instance_name,
            "--allow-unfingerprinted",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "replay to record fingerprinted trajectory failed: status={:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout,
        stderr
    );

    sweep_dir.join(format!("{instance_name}.traj.json"))
}

#[test]
fn bench_fork_help_works() {
    let out = Command::new(binary_path())
        .args(["bench", "fork", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "expected success but got code {:?}, stderr: {}, stdout: {}",
        out.status.code(),
        stderr,
        stdout
    );
    assert!(
        stdout.contains("bench-fork") || stdout.contains("fork") || stdout.contains("Fork"),
        "expected help output to describe fork command, got: {}",
        stdout
    );
}

#[test]
fn bench_fork_out_of_bounds_step_fails() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    let legacy_path = temp.path().join("legacy.traj.json");
    make_legacy_trajectory(&legacy_path);

    // Produce myinstance.traj.json under sweep_dir (which has 2 steps)
    record_fingerprinted_trajectory(&legacy_path, &sweep_dir, "myinstance");

    // Try to fork at step 2 (out of bounds since parent only has 2 steps: 0 and 1)
    let out = Command::new(binary_path())
        .args([
            "bench",
            "fork",
            "--sweep",
            sweep_dir.to_str().unwrap(),
            "--instance",
            "myinstance",
            "--from-step",
            "2",
            "--output",
            temp.path().join("out").to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit code 2 (usage error) on out-of-bounds fork step, got: {:?}",
        out.status.code()
    );
    assert!(
        stderr.contains("is out of bounds") || stderr.contains("out of bounds"),
        "expected error message about out of bounds, got: {}",
        stderr
    );
}

#[test]
fn bench_fork_legacy_without_flag_fails() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    // Place the legacy unfingerprinted trajectory directly in sweep_dir
    let legacy_path = sweep_dir.join("legacyinstance.traj.json");
    make_legacy_trajectory(&legacy_path);

    // Run fork without --allow-unfingerprinted
    let out = Command::new(binary_path())
        .args([
            "bench",
            "fork",
            "--sweep",
            sweep_dir.to_str().unwrap(),
            "--instance",
            "legacyinstance",
            "--from-step",
            "1",
            "--output",
            temp.path().join("out").to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit code 2 (usage error) for legacy trajectory without flag, got: {:?}",
        out.status.code()
    );
}

#[test]
fn bench_fork_legacy_with_flag_succeeds() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    // Place the legacy unfingerprinted trajectory directly in sweep_dir
    let legacy_path = sweep_dir.join("legacyinstance.traj.json");
    make_legacy_trajectory(&legacy_path);

    // Run fork with --allow-unfingerprinted and --step-limit 0 (exit immediately on step 0)
    let out = Command::new(binary_path())
        .args([
            "bench",
            "fork",
            "--sweep",
            sweep_dir.to_str().unwrap(),
            "--instance",
            "legacyinstance",
            "--from-step",
            "0",
            "--output",
            temp.path().join("out").to_str().unwrap(),
            "--allow-unfingerprinted",
            "--step-limit",
            "0",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected successful fork exit 0, got: {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout,
        stderr
    );
}

#[test]
fn bench_fork_prompt_drift_fails() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    let legacy_path = temp.path().join("legacy.traj.json");
    make_legacy_trajectory(&legacy_path);

    let fp_traj = record_fingerprinted_trajectory(&legacy_path, &sweep_dir, "driftinstance");

    // Tamper with the first assistant message's input fingerprint
    let content = fs::read_to_string(&fp_traj).unwrap();
    let mut traj: serde_json::Value = serde_json::from_str(&content).unwrap();

    let messages = traj["messages"].as_array_mut().unwrap();
    for msg in messages {
        if msg["role"].as_str() == Some("assistant") {
            if let Some(mc) = msg["extra"]["model_call"].as_object_mut() {
                mc.insert(
                    "input_fingerprint".to_owned(),
                    serde_json::json!("deadbeef00000000"),
                );
            }
            break;
        }
    }
    fs::write(&fp_traj, serde_json::to_string_pretty(&traj).unwrap()).unwrap();

    // Run fork starting from step 1 (replays step 0 first, which will fail prompt fingerprint verification)
    let out = Command::new(binary_path())
        .args([
            "bench",
            "fork",
            "--sweep",
            sweep_dir.to_str().unwrap(),
            "--instance",
            "driftinstance",
            "--from-step",
            "1",
            "--output",
            temp.path().join("out").to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(9),
        "expected exit code 9 (replay prompt drift) on fingerprint mismatch, got: {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout,
        stderr
    );
}

#[test]
fn bench_fork_successful_run_and_inspect() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    let legacy_path = temp.path().join("legacy.traj.json");
    make_legacy_trajectory(&legacy_path);

    let _fp_traj = record_fingerprinted_trajectory(&legacy_path, &sweep_dir, "myinstance");

    // Run fork starting from step 1, with --step-limit 1 (so prefix step 0 runs deterministic, then live N=1 hits limit immediately)
    let out_dir = temp.path().join("out");
    let out = Command::new(binary_path())
        .args([
            "bench",
            "fork",
            "--sweep",
            sweep_dir.to_str().unwrap(),
            "--instance",
            "myinstance",
            "--from-step",
            "1",
            "--output",
            out_dir.to_str().unwrap(),
            "--step-limit",
            "1",
            "--model",
            "claude-3-opus-fork-tail-override",
            "--per-task-budget-usd",
            "12.34",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected fork to succeed with 0, got: {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout,
        stderr
    );

    // Verify output trajectory exists and has fork_lineage
    let fork_traj_path = out_dir.join("myinstance-fork.traj.json");
    assert!(
        fork_traj_path.exists(),
        "forked trajectory file should be saved"
    );

    let traj_content = fs::read_to_string(&fork_traj_path).unwrap();
    let traj: serde_json::Value = serde_json::from_str(&traj_content).unwrap();

    // Verify replayed prefix step has $0 cost
    // The first message is assistant message in replay, verify that input/output/cost is zeroed or overall total cost is $0.
    // Let's verify total_cost_usd is Some(0.0) or low (in our setup it's exactly 0.0 because of step-limit exit before live phase).
    let total_cost = traj["info"]["total_cost_usd"].as_f64().unwrap();
    assert_eq!(
        total_cost, 0.0,
        "total cost of fork run should be $0.0 since only step 0 replayed under $0 and step 1 hit limit"
    );

    // Verify fork_lineage block exists
    let lineage = &traj["fork_lineage"];
    assert!(
        lineage.is_object(),
        "fork_lineage field must be a valid JSON object"
    );
    assert_eq!(
        lineage["parent_instance_id"].as_str().unwrap(),
        "myinstance"
    );
    assert_eq!(lineage["fork_step"].as_u64().unwrap(), 1);

    // Verify tail overrides are recorded
    let overrides = &lineage["tail_overrides"];
    assert_eq!(
        overrides["model"].as_str().unwrap(),
        "claude-3-opus-fork-tail-override"
    );
    assert_eq!(overrides["step_limit"].as_u64().unwrap(), 1);
    assert_eq!(overrides["per_task_budget_usd"].as_f64().unwrap(), 12.34);

    // Run bench inspect on output trajectory and verify it displays the lineage
    let inspect_out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            out_dir.to_str().unwrap(),
            "--instance",
            "myinstance-fork",
        ])
        .output()
        .unwrap();

    let inspect_stdout = String::from_utf8_lossy(&inspect_out.stdout);
    assert!(
        inspect_out.status.success(),
        "bench inspect failed: {}",
        String::from_utf8_lossy(&inspect_out.stderr)
    );
    assert!(
        inspect_stdout.contains("fork_lineage:"),
        "expected inspect output to show fork lineage, got: {}",
        inspect_stdout
    );
}
