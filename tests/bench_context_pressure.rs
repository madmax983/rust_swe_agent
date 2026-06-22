//! `bench context-pressure`: report context-window pressure telemetry.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![recursion_limit = "256"]

use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

fn run_context_pressure(sweep: &Path, extra_args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.args(["--log", "error", "bench", "context-pressure", "--sweep"])
        .arg(sweep);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.output().unwrap()
}

fn create_synthetic_sweep() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let results_json = serde_json::json!({
        "total": 2,
        "submitted": 1,
        "skipped": 0,
        "errored": 1,
        "estimated_cost_usd": 0.07,
        "instances": [
            {
                "instance_id": "inst-1",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "failure_category": null,
                "steps": 4,
                "cost_usd": 0.05,
                "prompt_tokens": 500,
                "cache_read_tokens": 0,
                "cache_creation_tokens": 0,
                "completion_tokens": 100,
                "duration_secs": 8.0,
                "error": null,
                "github_pr_error": null,
                "patch_present": true,
                "non_empty_patch": true,
                "attempts": 1,
                "retry_reasons": [],
                "runs": 1,
                "resolved_count": 1,
                "pass_at_1": true,
                "tests_run_before_submit": false,
                "last_tests_passed": null,
                "fallback_count": null,
                "final_model": null,
                "retry_id": null,
                "previous_failure_category": null,
                "trace_id": null,
                "context_pressure": {
                    "elision_trigger_count": 2,
                    "observations_elided": 3,
                    "bytes_elided": 1500,
                    "peak_projected_tokens": 12000,
                    "token_ceiling": 8000,
                    "compaction_failed": false
                }
            },
            {
                "instance_id": "inst-2",
                "exit_reason": "history_compaction_failed",
                "outcome": "error",
                "failure_category": "history_compaction_failed",
                "steps": 2,
                "cost_usd": 0.02,
                "prompt_tokens": 300,
                "cache_read_tokens": 0,
                "cache_creation_tokens": 0,
                "completion_tokens": 20,
                "duration_secs": 4.0,
                "error": "compaction failed",
                "github_pr_error": null,
                "patch_present": false,
                "non_empty_patch": false,
                "attempts": 1,
                "retry_reasons": [],
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false,
                "tests_run_before_submit": false,
                "last_tests_passed": null,
                "fallback_count": null,
                "final_model": null,
                "retry_id": null,
                "previous_failure_category": null,
                "trace_id": null,
                "context_pressure": {
                    "elision_trigger_count": 4,
                    "observations_elided": 8,
                    "bytes_elided": 4500,
                    "peak_projected_tokens": 9000,
                    "token_ceiling": 8000,
                    "compaction_failed": true
                }
            }
        ]
    });

    let path = temp.path().join("results.json");
    std::fs::write(&path, serde_json::to_string_pretty(&results_json).unwrap()).unwrap();
    temp
}

#[test]
fn cli_exits_zero_with_context_pressure_data() {
    let sweep = create_synthetic_sweep();
    let output = run_context_pressure(sweep.path(), &[]);
    assert!(
        output.status.success(),
        "bench context-pressure failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_text_output_shows_header_and_totals() {
    let sweep = create_synthetic_sweep();
    let output = run_context_pressure(sweep.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("bench context-pressure"));
    assert!(stdout.contains("Total Runs:          2"));
    assert!(stdout.contains("Runs with Elision:   2 (100.00%)"));
    assert!(stdout.contains("Compaction Failures: 1 (50.00%)"));
    assert!(stdout.contains("p50: 3000 bytes"));
    assert!(stdout.contains("p99: 4470 bytes") || stdout.contains("p99: 4500 bytes"));
}

#[test]
fn cli_json_emits_valid_report_and_writes_file() {
    let sweep = create_synthetic_sweep();
    let output = run_context_pressure(sweep.path(), &["--format", "json"]);
    assert!(output.status.success());

    let stdout_json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout_json["artifact_kind"], "context_pressure_report");
    assert_eq!(stdout_json["total_runs"].as_u64().unwrap(), 2);
    assert_eq!(stdout_json["runs_with_elision"].as_u64().unwrap(), 2);
    assert_eq!(stdout_json["compaction_failures"].as_u64().unwrap(), 1);

    // Verify context-pressure.json was written to the directory
    let written_file = sweep.path().join("context-pressure.json");
    assert!(written_file.exists());
    let written_content = std::fs::read_to_string(&written_file).unwrap();
    let written_json: serde_json::Value = serde_json::from_str(&written_content).unwrap();
    assert_eq!(written_json["artifact_kind"], "context_pressure_report");
}
