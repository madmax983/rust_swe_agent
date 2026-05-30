//! `bench near-miss`: integration tests for issue #307.
//!
//! TDD red → green → refactor. Tests drive the full CLI binary via `Command`.
//! All test sweep directories are synthetic — no model calls, no network.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

// ── fixture helpers ───────────────────────────────────────────────────────────

/// Write a minimal `results.json` so `load_sweep` doesn't complain.
fn write_results_json(dir: &Path) {
    let payload = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 10},
        "total": 5,
        "sweep_status": "completed",
        "submitted": 5,
        "submitted_with_tests": 0,
        "skipped": 0,
        "errored": 0,
        "failures_by_category": {},
        "budget_halted": 0,
        "with_patch": 4,
        "patch_empty": 1,
        "patch_apply_invalid": 0,
        "github_pr_failures": 0,
        "total_prompt_tokens": 0,
        "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "cache_hit_rate": 0.0,
        "retries": 0,
        "retried_instances": 0,
        "pass_at_k": 0.0,
        "filter_spec": {},
        "instances": [
            {
                "instance_id": "iou_1",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "patch_present": true,
                "non_empty_patch": true,
                "attempts": 1,
                "retry_reasons": [],
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false
            },
            {
                "instance_id": "iou_0_5",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "patch_present": true,
                "non_empty_patch": true,
                "attempts": 1,
                "retry_reasons": [],
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false
            },
            {
                "instance_id": "iou_0_1",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "patch_present": true,
                "non_empty_patch": true,
                "attempts": 1,
                "retry_reasons": [],
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false
            },
            {
                "instance_id": "iou_0_0",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "patch_present": true,
                "non_empty_patch": true,
                "attempts": 1,
                "retry_reasons": [],
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false
            },
            {
                "instance_id": "empty_patch",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "patch_present": false,
                "non_empty_patch": false,
                "attempts": 1,
                "retry_reasons": [],
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false
            }
        ],
        "total_fallbacks": 0,
        "model_mix": {},
        "retry_history": [],
        "partial": 0,
        "span_export_dropped": 0
    });
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&payload).unwrap(),
    )
    .unwrap();
}

/// Write a synthetic `evaluation.json` with five unresolved instances:
/// - `iou_1`:      gold_files_iou=1.0, gold_lines_overlap=0.9, gold_size_ratio=1.1 (top)
/// - `iou_0_5`:    gold_files_iou=0.5, gold_lines_overlap=0.5, gold_size_ratio=1.2 (second)
/// - `iou_0_1`:    gold_files_iou=0.1, gold_lines_overlap=0.1, gold_size_ratio=2.0 (third)
/// - `iou_0_0`:    gold_files_iou=0.0, gold_lines_overlap=0.0, gold_size_ratio=0.0 (fourth / no_gold path via null gold)
/// - `no_gold`:    gold_files_iou=null (missing gold)  → `no_gold` bucket
/// - `empty_patch`: patch_stats.is_empty=true          → `empty_patch` bucket
/// - `no_stats`:   patch_stats=null                    → `no_stats` bucket
fn write_evaluation_json_full(dir: &Path) {
    let payload = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 10},
        "instances": [
            {
                "instance_id": "iou_1",
                "resolved": false,
                "eval_exit_reason": "unresolved",
                "patch_stats": {
                    "files_changed": 2,
                    "hunks": 2,
                    "lines_added": 5,
                    "lines_removed": 3,
                    "is_empty": false,
                    "touches_test_files": false,
                    "touches_lock_or_generated": false,
                    "gold_files_iou": 1.0,
                    "gold_lines_overlap": 0.9,
                    "gold_size_ratio": 1.1
                }
            },
            {
                "instance_id": "iou_0_5",
                "resolved": false,
                "eval_exit_reason": "unresolved",
                "patch_stats": {
                    "files_changed": 1,
                    "hunks": 1,
                    "lines_added": 3,
                    "lines_removed": 1,
                    "is_empty": false,
                    "touches_test_files": false,
                    "touches_lock_or_generated": false,
                    "gold_files_iou": 0.5,
                    "gold_lines_overlap": 0.5,
                    "gold_size_ratio": 1.2
                }
            },
            {
                "instance_id": "iou_0_1",
                "resolved": false,
                "eval_exit_reason": "unresolved",
                "patch_stats": {
                    "files_changed": 1,
                    "hunks": 1,
                    "lines_added": 2,
                    "lines_removed": 0,
                    "is_empty": false,
                    "touches_test_files": false,
                    "touches_lock_or_generated": false,
                    "gold_files_iou": 0.1,
                    "gold_lines_overlap": 0.1,
                    "gold_size_ratio": 2.0
                }
            },
            {
                "instance_id": "iou_0_0",
                "resolved": false,
                "eval_exit_reason": "unresolved",
                "patch_stats": {
                    "files_changed": 1,
                    "hunks": 1,
                    "lines_added": 1,
                    "lines_removed": 0,
                    "is_empty": false,
                    "touches_test_files": false,
                    "touches_lock_or_generated": false,
                    "gold_files_iou": 0.0,
                    "gold_lines_overlap": 0.0,
                    "gold_size_ratio": 0.5
                }
            },
            {
                "instance_id": "no_gold",
                "resolved": false,
                "eval_exit_reason": "unresolved",
                "patch_stats": {
                    "files_changed": 1,
                    "hunks": 1,
                    "lines_added": 1,
                    "lines_removed": 0,
                    "is_empty": false,
                    "touches_test_files": false,
                    "touches_lock_or_generated": false
                }
            },
            {
                "instance_id": "empty_patch",
                "resolved": false,
                "eval_exit_reason": "skipped_no_patch",
                "patch_stats": {
                    "files_changed": 0,
                    "hunks": 0,
                    "lines_added": 0,
                    "lines_removed": 0,
                    "is_empty": true,
                    "touches_test_files": false,
                    "touches_lock_or_generated": false
                }
            },
            {
                "instance_id": "no_stats",
                "resolved": false,
                "eval_exit_reason": "unresolved"
            }
        ],
    });
    std::fs::write(
        dir.join("evaluation.json"),
        serde_json::to_string_pretty(&payload).unwrap(),
    )
    .unwrap();
}

// ── AC: subcommand exists in --help ──────────────────────────────────────────

#[test]
fn near_miss_appears_in_bench_help() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("near-miss"),
        "bench --help should list near-miss subcommand; got:\n{stdout}"
    );
}

// ── AC: exit code 2 when evaluation.json is missing ──────────────────────────

#[test]
fn near_miss_exits_2_when_no_evaluation_json() {
    let dir = tempfile::tempdir().unwrap();
    write_results_json(dir.path());

    let out = Command::new(binary_path())
        .args(["bench", "near-miss", "--sweep", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit code 2 when evaluation.json is missing; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── AC: exit code 0 on success ────────────────────────────────────────────────

#[test]
fn near_miss_exits_0_on_success() {
    let dir = tempfile::tempdir().unwrap();
    write_results_json(dir.path());
    write_evaluation_json_full(dir.path());

    let out = Command::new(binary_path())
        .args(["bench", "near-miss", "--sweep", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit code 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── AC: deterministic ranking ────────────────────────────────────────────────

#[test]
fn near_miss_ranking_is_deterministic_and_correct() {
    let dir = tempfile::tempdir().unwrap();
    write_results_json(dir.path());
    write_evaluation_json_full(dir.path());

    let out = Command::new(binary_path())
        .args(["bench", "near-miss", "--sweep", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // iou_1 must be ranked first (gold_files_iou=1.0)
    let pos_iou1 = stdout.find("iou_1").unwrap_or(usize::MAX);
    let pos_iou05 = stdout.find("iou_0_5").unwrap_or(usize::MAX);
    let pos_iou01 = stdout.find("iou_0_1").unwrap_or(usize::MAX);
    let pos_iou00 = stdout.find("iou_0_0").unwrap_or(usize::MAX);

    assert!(
        pos_iou1 < pos_iou05,
        "iou_1 (IoU=1.0) should appear before iou_0_5 (IoU=0.5); stdout:\n{stdout}"
    );
    assert!(
        pos_iou05 < pos_iou01,
        "iou_0_5 (IoU=0.5) should appear before iou_0_1 (IoU=0.1); stdout:\n{stdout}"
    );
    assert!(
        pos_iou01 < pos_iou00,
        "iou_0_1 (IoU=0.1) should appear before iou_0_0 (IoU=0.0); stdout:\n{stdout}"
    );
}

// ── AC: correct bucket assignment ────────────────────────────────────────────

#[test]
fn near_miss_buckets_are_correct_in_text_output() {
    let dir = tempfile::tempdir().unwrap();
    write_results_json(dir.path());
    write_evaluation_json_full(dir.path());

    let out = Command::new(binary_path())
        .args(["bench", "near-miss", "--sweep", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Eligible: iou_1, iou_0_5, iou_0_1, iou_0_0 (4 instances)
    assert!(
        stdout.contains("eligible: 4"),
        "should show eligible: 4; got:\n{stdout}"
    );
    // no_gold: 1
    assert!(
        stdout.contains("no_gold: 1"),
        "should show no_gold: 1; got:\n{stdout}"
    );
    // empty_patch: 1
    assert!(
        stdout.contains("empty_patch: 1"),
        "should show empty_patch: 1; got:\n{stdout}"
    );
    // no_stats: 1
    assert!(
        stdout.contains("no_stats: 1"),
        "should show no_stats: 1; got:\n{stdout}"
    );
}

// ── AC: --top N limits the output ────────────────────────────────────────────

#[test]
fn near_miss_top_2_limits_eligible_output() {
    let dir = tempfile::tempdir().unwrap();
    write_results_json(dir.path());
    write_evaluation_json_full(dir.path());

    let out = Command::new(binary_path())
        .args([
            "bench",
            "near-miss",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--top",
            "2",
        ])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Should show iou_1 and iou_0_5 but NOT iou_0_1 or iou_0_0 in the table
    assert!(
        stdout.contains("iou_1"),
        "--top 2 should include iou_1; got:\n{stdout}"
    );
    assert!(
        stdout.contains("iou_0_5"),
        "--top 2 should include iou_0_5; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("iou_0_1"),
        "--top 2 should NOT include iou_0_1; got:\n{stdout}"
    );
}

// ── AC: JSON output has versioned artifact ────────────────────────────────────

#[test]
fn near_miss_json_output_is_versioned_artifact() {
    let dir = tempfile::tempdir().unwrap();
    write_results_json(dir.path());
    write_evaluation_json_full(dir.path());

    let out = Command::new(binary_path())
        .args([
            "bench",
            "near-miss",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("JSON output should be valid; err={e}; got:\n{stdout}"));

    assert_eq!(
        json["artifact_kind"].as_str(),
        Some("near_miss_report"),
        "artifact_kind should be near_miss_report; got:\n{json}"
    );
    assert_eq!(
        json["schema_version"]["major"].as_u64(),
        Some(1),
        "schema_version.major should be 1; got:\n{json}"
    );
    assert!(
        json["ranked"].is_array(),
        "JSON should have ranked array; got:\n{json}"
    );
    assert!(
        json["buckets"].is_object(),
        "JSON should have buckets object; got:\n{json}"
    );
    assert!(
        json["sweep_path"].is_string(),
        "JSON should have sweep_path; got:\n{json}"
    );
    assert!(
        json["generated_at"].is_string(),
        "JSON should have generated_at timestamp; got:\n{json}"
    );
}

// ── AC: JSON ranking is correct ───────────────────────────────────────────────

#[test]
fn near_miss_json_ranking_order_is_correct() {
    let dir = tempfile::tempdir().unwrap();
    write_results_json(dir.path());
    write_evaluation_json_full(dir.path());

    let out = Command::new(binary_path())
        .args([
            "bench",
            "near-miss",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    let ranked = json["ranked"].as_array().unwrap();
    assert!(
        ranked.len() >= 4,
        "should have 4 eligible ranked entries; got: {}",
        ranked.len()
    );

    assert_eq!(ranked[0]["instance_id"].as_str(), Some("iou_1"));
    assert_eq!(ranked[1]["instance_id"].as_str(), Some("iou_0_5"));
    assert_eq!(ranked[2]["instance_id"].as_str(), Some("iou_0_1"));
    assert_eq!(ranked[3]["instance_id"].as_str(), Some("iou_0_0"));

    // Verify bucket counts
    assert_eq!(json["buckets"]["eligible"].as_u64(), Some(4));
    assert_eq!(json["buckets"]["no_gold"].as_u64(), Some(1));
    assert_eq!(json["buckets"]["empty_patch"].as_u64(), Some(1));
    assert_eq!(json["buckets"]["no_stats"].as_u64(), Some(1));
}

// ── AC: no patch text in default output (redaction-safe) ─────────────────────

#[test]
fn near_miss_output_contains_no_patch_text() {
    let dir = tempfile::tempdir().unwrap();
    write_results_json(dir.path());
    write_evaluation_json_full(dir.path());

    let out = Command::new(binary_path())
        .args(["bench", "near-miss", "--sweep", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Should not contain diff markers
    assert!(
        !stdout.contains("diff --git"),
        "output should not contain patch text; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("@@"),
        "output should not contain diff hunks; got:\n{stdout}"
    );
}

// ── AC: bench compare text output has near-miss referral line ─────────────────

#[test]
fn bench_compare_text_output_has_near_miss_referral() {
    // Construct a sweep with one regression (pass->fail) to trigger the
    // regressions section, and verify the referral line is present.
    let baseline = tempfile::tempdir().unwrap();
    let candidate = tempfile::tempdir().unwrap();

    // baseline: instance "a" resolved
    let baseline_payload = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 10},
        "total": 1,
        "sweep_status": "completed",
        "submitted": 1,
        "submitted_with_tests": 0,
        "skipped": 0,
        "errored": 0,
        "failures_by_category": {},
        "budget_halted": 0,
        "with_patch": 1,
        "patch_empty": 0,
        "patch_apply_invalid": 0,
        "github_pr_failures": 0,
        "total_prompt_tokens": 0,
        "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "cache_hit_rate": 0.0,
        "retries": 0,
        "retried_instances": 0,
        "pass_at_k": 1.0,
        "filter_spec": {},
        "instances": [{
            "instance_id": "a",
            "exit_reason": "submitted",
            "outcome": "submitted",
            "patch_present": true,
            "non_empty_patch": true,
            "attempts": 1,
            "retry_reasons": [],
            "runs": 1,
            "resolved_count": 1,
            "pass_at_1": true
        }],
        "total_fallbacks": 0,
        "model_mix": {},
        "retry_history": [],
        "partial": 0,
        "span_export_dropped": 0
    });
    std::fs::write(
        baseline.path().join("results.json"),
        serde_json::to_string_pretty(&baseline_payload).unwrap(),
    )
    .unwrap();
    let baseline_eval = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 10},
        "instances": [{"instance_id": "a", "resolved": true, "eval_exit_reason": "resolved"}]
    });
    std::fs::write(
        baseline.path().join("evaluation.json"),
        serde_json::to_string_pretty(&baseline_eval).unwrap(),
    )
    .unwrap();

    // candidate: instance "a" fails (regression)
    let candidate_payload = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 10},
        "total": 1,
        "sweep_status": "completed",
        "submitted": 0,
        "submitted_with_tests": 0,
        "skipped": 0,
        "errored": 1,
        "failures_by_category": {"step_limit": 1},
        "budget_halted": 0,
        "with_patch": 0,
        "patch_empty": 0,
        "patch_apply_invalid": 0,
        "github_pr_failures": 0,
        "total_prompt_tokens": 0,
        "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "cache_hit_rate": 0.0,
        "retries": 0,
        "retried_instances": 0,
        "pass_at_k": 0.0,
        "filter_spec": {},
        "instances": [{
            "instance_id": "a",
            "exit_reason": "step_limit",
            "outcome": "error",
            "failure_category": "step_limit",
            "patch_present": false,
            "non_empty_patch": false,
            "attempts": 1,
            "retry_reasons": [],
            "runs": 1,
            "resolved_count": 0,
            "pass_at_1": false
        }],
        "total_fallbacks": 0,
        "model_mix": {},
        "retry_history": [],
        "partial": 0,
        "span_export_dropped": 0
    });
    std::fs::write(
        candidate.path().join("results.json"),
        serde_json::to_string_pretty(&candidate_payload).unwrap(),
    )
    .unwrap();
    let candidate_eval = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 10},
        "instances": [{"instance_id": "a", "resolved": false, "eval_exit_reason": "unresolved"}]
    });
    std::fs::write(
        candidate.path().join("evaluation.json"),
        serde_json::to_string_pretty(&candidate_eval).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("bench near-miss"),
        "bench compare text output should contain 'bench near-miss' referral; got:\n{stdout}"
    );
}
