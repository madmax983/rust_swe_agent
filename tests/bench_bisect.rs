//! `bench bisect`: integration tests.
//!
//! Covers the acceptance criteria from issue #288.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::map_unwrap_or,
    clippy::unnecessary_map_or,
    clippy::redundant_closure_for_method_calls
)]

use std::fs;
use std::process::Command;

mod support;
use support::binary_path;

fn create_mock_sweep_results(dir: &std::path::Path, git_sha: &str) {
    fs::create_dir_all(dir).unwrap();
    let json = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {
            "major": 1,
            "minor": 10
        },
        "total": 5,
        "sweep_status": "completed",
        "submitted": 5,
        "skipped": 0,
        "errored": 0,
        "instances": [
            { "instance_id": "inst-1", "exit_reason": "ok", "outcome": "submitted", "steps": 5 },
            { "instance_id": "inst-2", "exit_reason": "ok", "outcome": "submitted", "steps": 5 },
            { "instance_id": "inst-3", "exit_reason": "ok", "outcome": "submitted", "steps": 5 },
            { "instance_id": "inst-4", "exit_reason": "ok", "outcome": "submitted", "steps": 5 },
            { "instance_id": "inst-5", "exit_reason": "ok", "outcome": "submitted", "steps": 5 }
        ],
        "manifest": {
            "purpose": "test",
            "harness": {
                "name": "maxwells-daemon",
                "version": "0.1.0",
                "git_sha": git_sha,
                "git_dirty": false,
                "git_resolution": "clean"
            },
            "dataset": {
                "path": "data/lite.jsonl",
                "sha256": "fake_sha",
                "instance_count": 5,
                "source_kind": "local"
            },
            "prompt_template": {
                "source": "fake_source",
                "sha256": "fake_prompt_sha"
            },
            "config": {
                "resolved": "",
                "overlay_paths": []
            },
            "model": {
                "name": "model-a",
                "backend": "mock"
            },
            "runtime": {
                "started_at_utc": "2026-05-24T12:00:00Z",
                "finished_at_utc": "2026-05-24T12:30:00Z",
                "host_os": "windows"
            },
            "cli": {
                "argv": []
            }
        }
    });
    fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&json).unwrap(),
    )
    .unwrap();
}

#[test]
fn bisect_subcommand_appears_in_bench_help() {
    let output = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("bisect") || stderr.contains("bisect"),
        "bench --help should list 'bisect'\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn bisect_fails_with_missing_args() {
    let output = Command::new(binary_path())
        .args(["bench", "bisect"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "expected clap error for missing args"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 (usage error)"
    );
}

#[test]
fn bisect_reproducible_mock_binary_search_finds_culprit() {
    let tmp = tempfile::tempdir().unwrap();
    let good_dir = tmp.path().join("good_sweep");
    let bad_dir = tmp.path().join("bad_sweep");
    create_mock_sweep_results(&good_dir, "good_sha_123");
    create_mock_sweep_results(&bad_dir, "bad_sha_456");

    let bisect_json_path = tmp.path().join("bisect.json");

    let mut cmd = Command::new(binary_path());
    cmd.args([
        "bench",
        "bisect",
        "--good",
        good_dir.to_str().unwrap(),
        "--bad",
        bad_dir.to_str().unwrap(),
        "--resume",
        bisect_json_path.to_str().unwrap(),
    ]);

    // Setup Mock Environment
    cmd.env("MAX_BISECT_TEST_ENV", "1");
    cmd.env("MAX_BISECT_MOCK_COMMITS", "c1,c2,c3,c4,c5,c6,c7,c8");

    // c1..c4 are good (resolved = 5/5), c5..c8 are bad (resolved = 2/5)
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c1", "5");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c2", "5");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c3", "5");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c4", "5");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c5", "2");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c6", "2");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c7", "2");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c8", "2");

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bench bisect failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Verify culprit in bisect.json
    let content = fs::read_to_string(&bisect_json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(parsed["suspect_commit"], serde_json::json!("c5"));
    assert_eq!(parsed["outcome"], serde_json::json!("success"));
}

#[test]
fn bisect_budget_exhausted_exits_code_18() {
    let tmp = tempfile::tempdir().unwrap();
    let good_dir = tmp.path().join("good_sweep");
    let bad_dir = tmp.path().join("bad_sweep");
    create_mock_sweep_results(&good_dir, "good_sha_123");
    create_mock_sweep_results(&bad_dir, "bad_sha_456");

    let bisect_json_path = tmp.path().join("bisect.json");

    let mut cmd = Command::new(binary_path());
    cmd.args([
        "bench",
        "bisect",
        "--good",
        good_dir.to_str().unwrap(),
        "--bad",
        bad_dir.to_str().unwrap(),
        "--resume",
        bisect_json_path.to_str().unwrap(),
        "--max-cost-usd",
        "0.25", // 3 steps * $0.10 = $0.30 > $0.25
    ]);

    cmd.env("MAX_BISECT_TEST_ENV", "1");
    cmd.env("MAX_BISECT_MOCK_COMMITS", "c1,c2,c3,c4,c5,c6,c7,c8");

    cmd.env("MAX_BISECT_MOCK_RESOLVED_c1", "5");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c2", "5");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c3", "5");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c4", "5");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c5", "2");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c6", "2");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c7", "2");
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c8", "2");

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(18),
        "expected exit code 18 for budget exhausted\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Verify partial json state
    let content = fs::read_to_string(&bisect_json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(parsed["outcome"], serde_json::json!("budget_exhausted"));
}

#[test]
fn bisect_all_schema_breaks_exits_code_19() {
    let tmp = tempfile::tempdir().unwrap();
    let good_dir = tmp.path().join("good_sweep");
    let bad_dir = tmp.path().join("bad_sweep");
    create_mock_sweep_results(&good_dir, "good_sha_123");
    create_mock_sweep_results(&bad_dir, "bad_sha_456");

    let bisect_json_path = tmp.path().join("bisect.json");

    let mut cmd = Command::new(binary_path());
    cmd.args([
        "bench",
        "bisect",
        "--good",
        good_dir.to_str().unwrap(),
        "--bad",
        bad_dir.to_str().unwrap(),
        "--resume",
        bisect_json_path.to_str().unwrap(),
    ]);

    cmd.env("MAX_BISECT_TEST_ENV", "1");
    cmd.env("MAX_BISECT_MOCK_COMMITS", "c1,c2,c3,c4");

    // Mock schema breaks for all candidates
    cmd.env("MAX_BISECT_MOCK_SCHEMA_BREAK_c1", "1");
    cmd.env("MAX_BISECT_MOCK_SCHEMA_BREAK_c2", "1");
    cmd.env("MAX_BISECT_MOCK_SCHEMA_BREAK_c3", "1");
    cmd.env("MAX_BISECT_MOCK_SCHEMA_BREAK_c4", "1");

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(19),
        "expected exit code 19 for schema breaks\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Verify outcome is schema_break in bisect.json
    let content = fs::read_to_string(&bisect_json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(parsed["outcome"], serde_json::json!("schema_break"));
    assert_eq!(parsed["schema_breaks"].as_array().unwrap().len(), 4);
}

#[test]
fn bisect_systemic_halt_trips_circuit_breaker() {
    let tmp = tempfile::tempdir().unwrap();
    let good_dir = tmp.path().join("good_sweep");
    let bad_dir = tmp.path().join("bad_sweep");
    create_mock_sweep_results(&good_dir, "good_sha_123");
    create_mock_sweep_results(&bad_dir, "bad_sha_456");

    let bisect_json_path = tmp.path().join("bisect.json");

    let mut cmd = Command::new(binary_path());
    cmd.args([
        "bench",
        "bisect",
        "--good",
        good_dir.to_str().unwrap(),
        "--bad",
        bad_dir.to_str().unwrap(),
        "--resume",
        bisect_json_path.to_str().unwrap(),
    ]);

    cmd.env("MAX_BISECT_TEST_ENV", "1");
    cmd.env("MAX_BISECT_MOCK_COMMITS", "c1,c2");

    cmd.env("MAX_BISECT_MOCK_RESOLVED_c1", "5");
    // c2 halts with stagnation
    cmd.env("MAX_BISECT_MOCK_HALT_c2", "stagnation");

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bench bisect failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Verify halt is recorded and counts as bad
    let content = fs::read_to_string(&bisect_json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(parsed["suspect_commit"], serde_json::json!("c2"));
    assert_eq!(
        parsed["per_commit"]["c2"]["systemic_halt_category"],
        serde_json::json!("AgentStagnation")
    );
    assert_eq!(
        parsed["per_commit"]["c2"]["status"],
        serde_json::json!("bad")
    );
}

#[test]
fn bisect_resume_loads_existing_results_and_bypasses_evaluation() {
    let tmp = tempfile::tempdir().unwrap();
    let good_dir = tmp.path().join("good_sweep");
    let bad_dir = tmp.path().join("bad_sweep");
    create_mock_sweep_results(&good_dir, "good_sha_123");
    create_mock_sweep_results(&bad_dir, "bad_sha_456");

    let bisect_json_path = tmp.path().join("bisect.json");

    // Pre-populate bisect.json state
    let initial_state = serde_json::json!({
        "schema_version": "1.0.0",
        "good_sha": "good_sha_123",
        "bad_sha": "bad_sha_456",
        "commits_visited": [],
        "per_commit": {
            "c4": {
                "commit_sha": "c4",
                "resolved": 5,
                "errored": 0,
                "cost_usd": 0.10,
                "wallclock_secs": 1.0,
                "smoke_artifact_path": "runs/bisect_smoke/c4/results.json",
                "cache_reuse_count": 0,
                "status": "good",
                "systemic_halt_category": null
            },
            "c6": {
                "commit_sha": "c6",
                "resolved": 2,
                "errored": 0,
                "cost_usd": 0.10,
                "wallclock_secs": 1.0,
                "smoke_artifact_path": "runs/bisect_smoke/c6/results.json",
                "cache_reuse_count": 0,
                "status": "bad",
                "systemic_halt_category": null
            }
        },
        "suspect_commit": null,
        "total_cost": 0.20,
        "total_wallclock": 2.0,
        "cache_reuse_count": 0,
        "schema_breaks": [],
        "outcome": null
    });
    fs::write(
        &bisect_json_path,
        serde_json::to_string_pretty(&initial_state).unwrap(),
    )
    .unwrap();

    let mut cmd = Command::new(binary_path());
    cmd.args([
        "bench",
        "bisect",
        "--good",
        good_dir.to_str().unwrap(),
        "--bad",
        bad_dir.to_str().unwrap(),
        "--resume",
        bisect_json_path.to_str().unwrap(),
    ]);

    cmd.env("MAX_BISECT_TEST_ENV", "1");
    cmd.env("MAX_BISECT_MOCK_COMMITS", "c1,c2,c3,c4,c5,c6,c7,c8");

    // c5 is regressed
    cmd.env("MAX_BISECT_MOCK_RESOLVED_c5", "2");

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bench bisect failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Verify culprit in bisect.json is c5 (first regressed commit)
    let content = fs::read_to_string(&bisect_json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(parsed["suspect_commit"], serde_json::json!("c5"));
    assert_eq!(parsed["outcome"], serde_json::json!("success"));

    // Check that we visited c4, c6, and c5
    let visited = parsed["commits_visited"].as_array().unwrap();
    let visited_strs: Vec<&str> = visited.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(visited_strs.contains(&"c4"), "should record c4 visited");
    assert!(visited_strs.contains(&"c6"), "should record c6 visited");
    assert!(visited_strs.contains(&"c5"), "should record c5 visited");
}
