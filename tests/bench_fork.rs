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

#[test]
fn bench_fork_invalid_args_in_mcp_config_fails() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    let legacy_path = temp.path().join("legacy.traj.json");
    make_legacy_trajectory(&legacy_path);
    record_fingerprinted_trajectory(&legacy_path, &sweep_dir, "myinstance");

    // Write a malformed mcp-config (args is a string, not an array)
    let mcp_cfg_path = temp.path().join("mcp_config.json");
    let mcp_cfg_content = serde_json::json!({
        "mcpServers": {
            "test-server": {
                "command": "node",
                "args": "--version" // Invalid: should be ["--version"]
            }
        }
    });
    fs::write(
        &mcp_cfg_path,
        serde_json::to_string_pretty(&mcp_cfg_content).unwrap(),
    )
    .unwrap();

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
            temp.path().join("out").to_str().unwrap(),
            "--mcp-config",
            mcp_cfg_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected failure for malformed mcp-config"
    );
    assert!(
        stderr.contains("must be an array")
            || stderr.contains("invalid")
            || stderr.contains("args"),
        "expected error about args array requirement, got: {}",
        stderr
    );
}

#[test]
fn bench_fork_corrupt_manifest_fails() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    let legacy_path = temp.path().join("legacy.traj.json");
    make_legacy_trajectory(&legacy_path);
    record_fingerprinted_trajectory(&legacy_path, &sweep_dir, "myinstance");

    // Write a corrupt results.json in sweep_dir (e.g. invalid JSON)
    let results_path = sweep_dir.join("results.json");
    fs::write(&results_path, "corrupt JSON content } }").unwrap();

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
            temp.path().join("out").to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected failure for corrupt manifest results.json"
    );
    assert!(
        stderr.contains("results.json") || stderr.contains("JSON") || stderr.contains("manifest"),
        "expected error about results.json parsing, got: {}",
        stderr
    );
}

#[test]
fn bench_fork_lineage_records_only_effective_mcp_override() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    let legacy_path = temp.path().join("legacy.traj.json");
    make_legacy_trajectory(&legacy_path);
    record_fingerprinted_trajectory(&legacy_path, &sweep_dir, "myinstance");

    // Write a mock MCP server python script that implements handshake and tools/list
    let mock_mcp_path = temp.path().join("mock_mcp.py");
    fs::write(
        &mock_mcp_path,
        "import sys\n\
         sys.stdin.readline()\n\
         sys.stdin.readline()\n\
         print('{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mock\",\"version\":\"1.0\"}}}')\n\
         print('{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}')\n"
    ).unwrap();
    let mcp_cmd = format!("python {}", mock_mcp_path.to_str().unwrap());

    // Write a valid mcp-config
    let mcp_cfg_path = temp.path().join("mcp_config.json");
    let mcp_cfg_content = serde_json::json!({
        "mcpServers": {
            "test-server": {
                "command": "node",
                "args": ["--version"]
            }
        }
    });
    fs::write(
        &mcp_cfg_path,
        serde_json::to_string_pretty(&mcp_cfg_content).unwrap(),
    )
    .unwrap();

    // Run fork with BOTH --mcp-server and --mcp-config
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
            "0",
            "--mcp-server",
            &mcp_cmd,
            "--mcp-config",
            mcp_cfg_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected success but got status={:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout,
        stderr
    );

    let fork_traj_path = out_dir.join("myinstance-fork.traj.json");
    let traj_content = fs::read_to_string(&fork_traj_path).unwrap();
    let traj: serde_json::Value = serde_json::from_str(&traj_content).unwrap();

    let lineage = &traj["fork_lineage"];
    let overrides = &lineage["tail_overrides"];

    // Should contain mcp_servers
    assert!(
        overrides["mcp_servers"].is_array(),
        "mcp_servers must be recorded as it was effective"
    );
    assert_eq!(overrides["mcp_servers"][0].as_str().unwrap(), mcp_cmd);

    // Should NOT contain mcp_config because it was ignored in favor of mcp_servers
    assert!(
        overrides["mcp_config"].is_null(),
        "mcp_config must not be recorded as it was not effective"
    );
}

fn make_valid_manifest_in_sweep(sweep_dir: &Path, resolved_toml: &str) {
    let results = serde_json::json!({
        "total": 0,
        "submitted": 0,
        "submitted_with_tests": 0,
        "skipped": 0,
        "errored": 0,
        "sweep_status": "completed",
        "instances": [],
        "manifest": {
            "harness": {
                "name": "test-harness",
                "version": "1.0",
                "git_sha": null,
                "git_dirty": null,
                "git_resolution": "test"
            },
            "dataset": {
                "path": "test-dataset",
                "sha256": "test-sha",
                "instance_count": 0,
                "dataset_kind": "local"
            },
            "prompt_template": {
                "source": "test-template",
                "sha256": "template-sha"
            },
            "config": {
                "resolved": resolved_toml,
                "overlay_paths": []
            },
            "model": {
                "name": "test-model",
                "backend": "test-backend"
            },
            "runtime": {
                "started_at_utc": "2026-05-22T00:00:00Z",
                "host_os": "linux"
            },
            "cli": {
                "argv": []
            }
        }
    });
    fs::write(
        sweep_dir.join("results.json"),
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();
}

#[test]
fn bench_fork_preserves_parent_step_limit_without_override() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    let legacy_path = temp.path().join("legacy.traj.json");
    make_legacy_trajectory(&legacy_path);
    record_fingerprinted_trajectory(&legacy_path, &sweep_dir, "myinstance");

    // Write a valid results.json manifest specifying agent.step_limit = 1
    let resolved_toml = "\
[agent]
step_limit = 1
";
    make_valid_manifest_in_sweep(&sweep_dir, resolved_toml);

    // Run fork starting from step 1, WITHOUT --step-limit override
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
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected fork to succeed, got: {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout,
        stderr
    );

    let fork_traj_path = out_dir.join("myinstance-fork.traj.json");
    assert!(fork_traj_path.exists());
    let traj_content = fs::read_to_string(&fork_traj_path).unwrap();
    let traj: serde_json::Value = serde_json::from_str(&traj_content).unwrap();

    // Verify it terminated due to the inherited step_limit of 1
    let info = &traj["info"];
    assert_eq!(
        info["exit_reason"].as_str().unwrap(),
        "step_limit",
        "should terminate with step_limit due to inherited step_limit from parent manifest"
    );

    // Verify no step_limit is recorded in tail_overrides
    let lineage = &traj["fork_lineage"];
    let overrides = &lineage["tail_overrides"];
    assert!(
        overrides["step_limit"].is_null(),
        "step_limit override should not be recorded since it wasn't overridden"
    );
}

#[test]
fn bench_fork_lineage_redaction() {
    let temp = tempdir().unwrap();
    let sweep_dir = temp.path().join("sweep");
    fs::create_dir_all(&sweep_dir).unwrap();

    let legacy_path = temp.path().join("legacy.traj.json");
    make_legacy_trajectory(&legacy_path);

    let _fp_traj = record_fingerprinted_trajectory(&legacy_path, &sweep_dir, "myinstance");

    // Run fork starting from step 1, with a sensitive environment variable "MY_SECRET_KEY" = "supersecret123"
    // and pass this secret in the --model override.
    let out_dir = temp.path().join("out");
    let out = Command::new(binary_path())
        .env("MY_SECRET_KEY", "supersecret123")
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
            "claude-with-supersecret123",
        ])
        .output()
        .unwrap();

    assert!(out.status.success());

    // Verify output trajectory exists and has fork_lineage
    let fork_traj_path = out_dir.join("myinstance-fork.traj.json");
    assert!(fork_traj_path.exists());

    let traj_content = fs::read_to_string(&fork_traj_path).unwrap();

    // The raw secret "supersecret123" should NOT be present anywhere in the trajectory file on disk!
    assert!(
        !traj_content.contains("supersecret123"),
        "Persisted trajectory should have redacted the sensitive override value, but found: {}",
        traj_content
    );

    // Let's also run bench inspect and make sure that is redacted.
    let inspect_out = Command::new(binary_path())
        .env("MY_SECRET_KEY", "supersecret123")
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
        !inspect_stdout.contains("supersecret123"),
        "Inspect output should have redacted the sensitive override value, but found: {}",
        inspect_stdout
    );
}
