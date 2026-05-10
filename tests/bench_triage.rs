//! `bench triage`: cluster unresolved sweep failures into ranked signatures.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::json;

use rust_swe_agent::run::triage::{FailureSignature, normalize_signature_text};

mod support;
use support::binary_path;

#[test]
fn signature_normalization_replaces_numbers_paths_and_whitespace() {
    let normalized = normalize_signature_text(
        "  ERROR at C:\\tmp\\run-42\\src\\lib.rs:101\n\tthen /tmp/swe/run-77/main.py value=999  ",
    );

    assert_eq!(normalized, "error at <path> then <path> value=<num>");

    let left = FailureSignature::from_parts(
        "model_parse",
        "Parser failed in /tmp/swe/task-101/src/main.py line 33",
        Some(2),
        "SyntaxError: unexpected token 404 in /tmp/swe/task-101/src/main.py",
    );
    let right = FailureSignature::from_parts(
        "model_parse",
        " parser FAILED in C:\\tmp\\task-202\\src\\main.py line 44 ",
        Some(2),
        "syntaxerror: unexpected token 505 in C:\\tmp\\task-202\\src\\main.py",
    );

    assert_eq!(left.stable_key(), right.stable_key());
    assert_eq!(left.cluster_id(), right.cluster_id());
}

#[test]
fn cli_clusters_unresolved_failures_and_writes_ranked_triage_json() {
    let sweep = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--top",
            "3",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench triage failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("=== bench triage ==="), "{stdout}");
    assert!(stdout.contains("model_parse"), "{stdout}");
    assert!(stdout.contains("model-a-1"), "{stdout}");
    assert!(stdout.contains("env-1"), "{stdout}");

    let model_rank = stdout.find("model-a-1").unwrap();
    let env_rank = stdout.find("env-1").unwrap();
    let singleton_rank = stdout.find("model-b-1").unwrap();
    assert!(
        model_rank < env_rank && env_rank < singleton_rank,
        "clusters should rank by instance_count * total_cost_usd:\n{stdout}"
    );

    let report_path = sweep.path().join("triage.json");
    assert!(report_path.exists(), "triage.json should be written");
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();

    assert_eq!(report["totals"]["clusters"], 4);
    assert_eq!(report["totals"]["instances"], 5);
    assert_eq!(report["totals"]["unclustered_instances"], 0);
    assert_eq!(report["totals"]["unresolved_cost_usd"], json!(15.5));

    let clusters = report["clusters"].as_array().unwrap();
    assert_eq!(clusters.len(), 4);
    assert_eq!(clusters[0]["failure_category"], "model_parse");
    assert_eq!(clusters[0]["instance_count"], 2);
    assert_eq!(clusters[0]["total_cost_usd"], json!(3.5));
    assert_eq!(clusters[0]["exemplar_instance_id"], "model-a-1");
    assert_eq!(
        clusters[0]["exemplar_trajectory_path"],
        "model-a-1.traj.json"
    );
    assert_eq!(
        clusters[0]["instance_ids"],
        json!(["model-a-1", "model-a-2"])
    );
    assert!(
        clusters[0]["cluster_id"].as_str().unwrap().len() >= 12,
        "stable hash should be operator-friendly but collision-resistant enough"
    );
}

#[test]
fn cli_json_format_honors_bucket_and_min_cluster_size_filters() {
    let sweep = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--bucket",
            "model_parse",
            "--min-cluster-size",
            "2",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench triage failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let clusters = report["clusters"].as_array().unwrap();
    assert_eq!(clusters.len(), 1);
    assert_eq!(clusters[0]["failure_category"], "model_parse");
    assert_eq!(clusters[0]["instance_count"], 2);
    assert_eq!(report["totals"]["instances"], 2);
    assert_eq!(report["totals"]["unclustered_instances"], 1);
    assert_eq!(report["totals"]["unresolved_cost_usd"], json!(8.5));
}

#[test]
fn triage_json_is_deterministic_except_generated_at() {
    let sweep = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(sweep.path());

    let first = run_triage_json(sweep.path());
    let first_file = std::fs::read_to_string(sweep.path().join("triage.json")).unwrap();

    std::thread::sleep(Duration::from_secs(1));

    let second = run_triage_json(sweep.path());
    let second_file = std::fs::read_to_string(sweep.path().join("triage.json")).unwrap();

    assert_eq!(
        redact_generated_at(&first),
        redact_generated_at(&second),
        "stdout JSON should be deterministic except generated_at"
    );
    assert_eq!(
        redact_generated_at_text(&first_file),
        redact_generated_at_text(&second_file),
        "triage.json bytes should be deterministic except generated_at"
    );
}

#[test]
fn cli_includes_errored_results_rows_missing_from_evaluation_json() {
    let sweep = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(sweep.path());
    inject_results_only_errored_instance(sweep.path());

    let report = run_triage_json(sweep.path());
    let clusters = report["clusters"].as_array().unwrap();
    let Some(result_only) = clusters.iter().find(|cluster| {
        cluster["instance_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "eval-missing-error")
    }) else {
        panic!("errored result row missing from evaluation.json should still be triaged");
    };

    assert_eq!(result_only["failure_category"], "model_api");
    assert_eq!(report["totals"]["instances"], 6);
    assert_eq!(report["totals"]["unresolved_cost_usd"], json!(19.5));
}

#[test]
fn cli_excludes_resolved_rerun_aggregates_from_results_fallback_candidates() {
    let sweep = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(sweep.path());
    inject_resolved_rerun_aggregate_with_run1_failure(sweep.path());

    let report = run_triage_json(sweep.path());
    let clusters = report["clusters"].as_array().unwrap();
    assert!(
        clusters.iter().all(|cluster| {
            cluster["instance_ids"]
                .as_array()
                .unwrap()
                .iter()
                .all(|id| id != "resolved-rerun")
        }),
        "resolved rerun aggregate should not be triaged: {report:#}"
    );
    assert_eq!(report["totals"]["instances"], 5);
    assert_eq!(report["totals"]["unresolved_cost_usd"], json!(15.5));
}

#[test]
fn cli_errors_when_evaluation_json_is_missing() {
    let sweep = tempfile::tempdir().unwrap();
    copy_triage_fixture_sweep(sweep.path());
    std::fs::remove_file(sweep.path().join("evaluation.json")).unwrap();

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage",
            "--sweep",
            sweep.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "triage should require evaluation.json"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("bench evaluate"),
        "error should point operators at bench evaluate, got:\n{stderr}"
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

fn run_triage_json(sweep: &Path) -> serde_json::Value {
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "triage",
            "--sweep",
            sweep.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bench triage failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn redact_generated_at(value: &serde_json::Value) -> serde_json::Value {
    let mut redacted = value.clone();
    redacted["generated_at"] = json!("<generated_at>");
    redacted
}

fn redact_generated_at_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("\"generated_at\":") {
                let indent_len = line.len() - line.trim_start().len();
                format!(
                    "{}\"generated_at\": \"<generated_at>\",",
                    " ".repeat(indent_len)
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn inject_results_only_errored_instance(sweep: &Path) {
    let results_path = sweep.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
    results["total"] = json!(7);
    results["errored"] = json!(6);
    results["total_cost_usd"] = json!(28.5);
    results["instances"].as_array_mut().unwrap().push(json!({
        "instance_id": "eval-missing-error",
        "exit_reason": "error",
        "outcome": "error",
        "failure_category": "model_api",
        "cost_usd": 4.0,
        "attempts": 1,
        "runs": 1,
        "resolved_count": 0,
        "pass_at_1": false,
        "tests_run_before_submit": false
    }));
    std::fs::write(
        results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    std::fs::write(
        sweep.join("eval-missing-error.traj.json"),
        serde_json::to_string_pretty(&json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 3},
            "info": {
                "task": "eval-missing-error",
                "model_name": "fixture-model",
                "outcome": "error",
                "failure_category": "model_api",
                "total_cost_usd": 4.0,
                "steps": 2,
                "test_invocations": [],
                "tests_run_before_submit": false
            },
            "messages": [
                {"role": "assistant", "content": "Provider returned retry-after 30"},
                {
                    "role": "user",
                    "content": "observation",
                    "extra": {
                        "run_result": {
                            "stdout": "",
                            "stderr": "provider returned HTTP 429 retry-after 30",
                            "exit_code": 1,
                            "timed_out": false
                        }
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn inject_resolved_rerun_aggregate_with_run1_failure(sweep: &Path) {
    let results_path = sweep.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
    results["total"] = json!(7);
    results["errored"] = json!(6);
    results["resolved"] = json!(2);
    results["total_cost_usd"] = json!(31.5);
    results["instances"].as_array_mut().unwrap().push(json!({
        "instance_id": "resolved-rerun",
        "exit_reason": "error",
        "outcome": "error",
        "failure_category": "model_api",
        "cost_usd": 7.0,
        "attempts": 3,
        "runs": 3,
        "resolved_count": 1,
        "pass_at_1": false,
        "tests_run_before_submit": true
    }));
    std::fs::write(
        results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let evaluation_path = sweep.join("evaluation.json");
    let mut evaluation: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&evaluation_path).unwrap()).unwrap();
    evaluation["instances"].as_array_mut().unwrap().push(json!({
        "instance_id": "resolved-rerun",
        "resolved": true,
        "runs": 3,
        "resolved_count": 1,
        "pass_at_1": false,
        "tests_passed": [],
        "tests_failed": [],
        "eval_exit_reason": "resolved"
    }));
    std::fs::write(
        evaluation_path,
        serde_json::to_string_pretty(&evaluation).unwrap(),
    )
    .unwrap();

    std::fs::write(
        sweep.join("resolved-rerun.traj.json"),
        serde_json::to_string_pretty(&json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 3},
            "info": {
                "task": "resolved-rerun",
                "model_name": "fixture-model",
                "outcome": "error",
                "failure_category": "model_api",
                "total_cost_usd": 7.0,
                "steps": 2,
                "test_invocations": [],
                "tests_run_before_submit": true
            },
            "messages": [
                {"role": "assistant", "content": "Run 1 hit provider failure, later run solved it"},
                {
                    "role": "user",
                    "content": "observation",
                    "extra": {
                        "run_result": {
                            "stdout": "",
                            "stderr": "provider failure on run 1",
                            "exit_code": 1,
                            "timed_out": false
                        }
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
}
