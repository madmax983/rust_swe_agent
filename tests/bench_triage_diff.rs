//! `bench triage-diff`: diff failure-cluster composition between two sweeps.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use serde_json::json;

mod support;
use support::binary_path;

#[test]
fn triage_diff_identical_sweeps() {
    let baseline = tempfile::tempdir().unwrap();
    let candidate = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(baseline.path());
    copy_triage_fixture_sweep(candidate.path());

    // Generate triage.json in baseline and candidate first
    run_triage(baseline.path());
    run_triage(candidate.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage-diff",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench triage-diff failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], "triage-diff-1.0");
    assert_eq!(
        report["baseline_sweep"],
        baseline.path().display().to_string()
    );
    assert_eq!(
        report["candidate_sweep"],
        candidate.path().display().to_string()
    );

    let deltas = report["cluster_deltas"].as_array().unwrap();
    assert!(!deltas.is_empty());
    for delta in deltas {
        assert_eq!(delta["delta"], 0);
        assert_eq!(delta["delta_pct"], json!(0.0));
    }

    assert!(report["new_clusters"].as_array().unwrap().is_empty());
    assert!(report["resolved_clusters"].as_array().unwrap().is_empty());
    assert!(
        report["regression_instances"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(report["win_instances"].as_array().unwrap().is_empty());
}

#[test]
fn triage_diff_wins_and_regressions() {
    let baseline = tempfile::tempdir().unwrap();
    let candidate = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(baseline.path());
    copy_triage_fixture_sweep(candidate.path());

    // In candidate, mutate:
    // 1. Solve 'model-a-2' (unresolved in baseline). This makes it a Win!
    // 2. Fail 'resolved-1' (resolved in baseline). This makes it a Regression!
    mutate_candidate_win_and_regression(candidate.path());

    // Generate triage.json in baseline and candidate
    run_triage(baseline.path());
    run_triage(candidate.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage-diff",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench triage-diff failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let win_instances = report["win_instances"].as_array().unwrap();
    assert_eq!(win_instances.len(), 1);
    assert_eq!(win_instances[0]["instance_ids"], json!(["model-a-2"]));

    let regression_instances = report["regression_instances"].as_array().unwrap();
    assert_eq!(regression_instances.len(), 1);
    assert_eq!(
        regression_instances[0]["instance_ids"],
        json!(["resolved-1"])
    );
}

#[test]
fn triage_diff_fail_on_regression_flag() {
    let baseline = tempfile::tempdir().unwrap();
    let candidate = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(baseline.path());
    copy_triage_fixture_sweep(candidate.path());

    mutate_candidate_win_and_regression(candidate.path());

    run_triage(baseline.path());
    run_triage(candidate.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage-diff",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
            "--fail-on-regression",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should have failed with regression set non-empty"
    );
    assert_eq!(
        output.status.code().unwrap(),
        6,
        "Expected exit code 6 (RegressionGateFailure)"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("=== bench triage-diff ==="),
        "Output should still be printed before exiting"
    );
    assert!(
        stdout.contains("resolved-1"),
        "Output should contain the regression instance"
    );
}

#[test]
fn triage_diff_missing_triage_json_errors_if_no_auto_triage() {
    let baseline = tempfile::tempdir().unwrap();
    let candidate = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(baseline.path());
    copy_triage_fixture_sweep(candidate.path());

    // Do NOT generate triage.json
    // Explicitly delete it since the copy_triage_fixture_sweep copies triage.json from the fixture
    std::fs::remove_file(baseline.path().join("triage.json")).unwrap();
    std::fs::remove_file(candidate.path().join("triage.json")).unwrap();

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage-diff",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should have failed due to missing triage.json"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Run `bench triage --sweep"));

    // Now run WITH --auto-triage
    let output2 = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage-diff",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
            "--auto-triage",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output2.status.success(),
        "bench triage-diff --auto-triage failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output2.stdout),
        String::from_utf8_lossy(&output2.stderr)
    );
}

fn copy_triage_fixture_sweep(dir: &Path) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/triage/sweep");
    copy_dir(&fixture, dir);
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

fn run_triage(sweep: &Path) {
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage",
            "--sweep",
            sweep.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to run triage on {}",
        sweep.display()
    );
}

fn mutate_candidate_win_and_regression(sweep: &Path) {
    // 1. Solve 'model-a-2' by changing its entry in evaluation.json to resolved: true
    let eval_path = sweep.join("evaluation.json");
    let mut eval: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&eval_path).unwrap()).unwrap();
    for inst in eval["instances"].as_array_mut().unwrap() {
        if inst["instance_id"] == "model-a-2" {
            inst["resolved"] = json!(true);
            inst["eval_exit_reason"] = json!("resolved");
        }
    }
    std::fs::write(&eval_path, serde_json::to_string_pretty(&eval).unwrap()).unwrap();

    let results_path = sweep.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
    for inst in results["instances"].as_array_mut().unwrap() {
        if inst["instance_id"] == "model-a-2" {
            inst["resolved_count"] = json!(1);
            inst["pass_at_1"] = json!(true);
            inst["outcome"] = json!("submitted");
            inst["exit_reason"] = json!("submitted");
            inst["failure_category"] = json!(null);
        }
    }

    // 2. Fail 'resolved-1' by:
    //   - setting resolved: false in evaluation.json
    //   - and setting outcome: "error" in results.json
    //   - and creating a trajectory file 'resolved-1.traj.json'
    for inst in eval["instances"].as_array_mut().unwrap() {
        if inst["instance_id"] == "resolved-1" {
            inst["resolved"] = json!(false);
            inst["eval_exit_reason"] = json!("unresolved");
        }
    }
    std::fs::write(&eval_path, serde_json::to_string_pretty(&eval).unwrap()).unwrap();

    for inst in results["instances"].as_array_mut().unwrap() {
        if inst["instance_id"] == "resolved-1" {
            inst["resolved_count"] = json!(0);
            inst["pass_at_1"] = json!(false);
            inst["outcome"] = json!("error");
            inst["failure_category"] = json!("step_limit");
        }
    }
    std::fs::write(
        &results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    std::fs::write(
        sweep.join("resolved-1.traj.json"),
        serde_json::to_string_pretty(&json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 4},
            "info": {
                "task": "resolved-1",
                "model_name": "fixture-model",
                "outcome": "error",
                "failure_category": "step_limit",
                "total_cost_usd": 1.0,
                "steps": 20,
                "test_invocations": [],
                "tests_run_before_submit": false
            },
            "messages": [
                {"role": "assistant", "content": "Stuck in a loop"},
                {
                    "role": "user",
                    "content": "observation",
                    "extra": {
                        "run_result": {
                            "stdout": "",
                            "stderr": "Max steps exceeded",
                            "exit_code": 1,
                            "timed_out": true
                        }
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn triage_diff_filtered_triage_json_errors_unless_auto_triage() {
    let baseline = tempfile::tempdir().unwrap();
    let candidate = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(baseline.path());
    copy_triage_fixture_sweep(candidate.path());

    // Generate partial/filtered triage.json by running triage with --min-cluster-size 9999
    // This will result in 0 clustered instances and some unclustered instances, making it non-canonical!
    let output_triage = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage",
            "--sweep",
            baseline.path().to_str().unwrap(),
            "--min-cluster-size",
            "9999",
        ])
        .output()
        .unwrap();
    assert!(output_triage.status.success());

    // Generate normal/canonical triage for candidate
    run_triage(candidate.path());

    // Run triage-diff WITHOUT --auto-triage: should fail because baseline triage.json is not canonical (filtered)!
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage-diff",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should have failed due to filtered/partial triage.json"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("filtered or partial"),
        "Stderr did not contain expected warning: {stderr}"
    );

    // Run WITH --auto-triage: should succeed because it automatically regenerates the canonical report!
    let output_auto = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage-diff",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
            "--auto-triage",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output_auto.status.success(),
        "should have successfully auto-triaged and re-run canonically\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output_auto.stdout),
        String::from_utf8_lossy(&output_auto.stderr)
    );
}
