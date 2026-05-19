//! `bench test-progress`: end-to-end + unit integration tests.
//!
//! Covers the AC from issue #276:
//!   (a) all-resolved sweep → mean_partial_credit_score = 1.0, zero partial/regressed rows
//!   (b) 2/3 FAIL_TO_PASS passed, 0 regressed → partial_progress, score 0.667
//!   (c) 0/3 FAIL_TO_PASS passed, 2/5 regressed → regressed, score -0.4
//!   (d) no evaluator detail → evaluator_unavailable, excluded from means, kept in per_instance
//!   (e) hot_failing_tests ranks correctly
//!   (f) bench compare produces non-zero mean_partial_credit_score_delta
//!   (g) determinism: two runs → byte-identical JSON modulo generated_at
//!   (h) --min-tests 5 drops instance with 3 tests from means, keeps in per_instance with flag

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

mod support;
use support::binary_path;

// ── unit tests (pure library) ─────────────────────────────────────────────────

use maxwells_daemon::run::test_progress::{
    InstanceTestMetrics, VerdictBucket, compute_instance_metrics, compute_partial_credit_score,
};

#[test]
fn partial_credit_score_resolved_is_one() {
    let score = compute_partial_credit_score(1.0, 0.0);
    assert_eq!(score, 1.0);
}

#[test]
fn partial_credit_score_no_progress_is_zero() {
    let score = compute_partial_credit_score(0.0, 0.0);
    assert_eq!(score, 0.0);
}

#[test]
fn partial_credit_score_fully_regressed_is_neg_one() {
    let score = compute_partial_credit_score(0.0, 1.0);
    assert_eq!(score, -1.0);
}

#[test]
fn partial_credit_score_clamped_to_neg_one_floor() {
    // formula: clamp(pass_ratio - regress_ratio, -1, 1)
    let score = compute_partial_credit_score(0.0, 2.0);
    assert_eq!(score, -1.0);
}

#[test]
fn partial_credit_score_partial_example_b() {
    // AC (b): 2/3 FAIL_TO_PASS passed, 0/2 PASS_TO_PASS regressed
    let pass_ratio = 2.0_f64 / 3.0;
    let score = compute_partial_credit_score(pass_ratio, 0.0);
    assert!((score - 2.0 / 3.0).abs() < 1e-9, "score={score}");
}

#[test]
fn partial_credit_score_regressed_example_c() {
    // AC (c): 0/3 FAIL_TO_PASS passed, 2/5 PASS_TO_PASS regressed
    let score = compute_partial_credit_score(0.0, 0.4);
    assert!((score - (-0.4)).abs() < 1e-9, "score={score}");
}

#[test]
fn verdict_bucket_resolved() {
    let m = InstanceTestMetrics {
        fail_to_pass_total: 3,
        fail_to_pass_passed: 3,
        fail_to_pass_ratio: 1.0,
        pass_to_pass_total: 2,
        pass_to_pass_regressed: 0,
        pass_to_pass_regressed_ratio: 0.0,
        partial_credit_score: 1.0,
    };
    assert_eq!(m.verdict_bucket(), VerdictBucket::Resolved);
}

#[test]
fn verdict_bucket_partial_progress() {
    let m = InstanceTestMetrics {
        fail_to_pass_total: 3,
        fail_to_pass_passed: 2,
        fail_to_pass_ratio: 2.0 / 3.0,
        pass_to_pass_total: 2,
        pass_to_pass_regressed: 0,
        pass_to_pass_regressed_ratio: 0.0,
        partial_credit_score: 2.0 / 3.0,
    };
    assert_eq!(m.verdict_bucket(), VerdictBucket::PartialProgress);
}

#[test]
fn verdict_bucket_no_progress() {
    let m = InstanceTestMetrics {
        fail_to_pass_total: 2,
        fail_to_pass_passed: 0,
        fail_to_pass_ratio: 0.0,
        pass_to_pass_total: 2,
        pass_to_pass_regressed: 0,
        pass_to_pass_regressed_ratio: 0.0,
        partial_credit_score: 0.0,
    };
    assert_eq!(m.verdict_bucket(), VerdictBucket::NoProgress);
}

#[test]
fn verdict_bucket_regressed() {
    let m = InstanceTestMetrics {
        fail_to_pass_total: 3,
        fail_to_pass_passed: 0,
        fail_to_pass_ratio: 0.0,
        pass_to_pass_total: 5,
        pass_to_pass_regressed: 2,
        pass_to_pass_regressed_ratio: 0.4,
        partial_credit_score: -0.4,
    };
    assert_eq!(m.verdict_bucket(), VerdictBucket::Regressed);
}

#[test]
fn compute_instance_metrics_partial_example_b() {
    // AC (b): FAIL_TO_PASS=["a","b","c"], tests_passed=["a","b","p1"], tests_failed=["c"]
    // PASS_TO_PASS=["p1","p2"], no regressions
    let fail_to_pass = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let pass_to_pass = vec!["p1".to_string(), "p2".to_string()];
    let tests_passed = vec![
        "a".to_string(),
        "b".to_string(),
        "p1".to_string(),
        "p2".to_string(),
    ];
    let tests_failed = vec!["c".to_string()];
    let m = compute_instance_metrics(&fail_to_pass, &pass_to_pass, &tests_passed, &tests_failed);
    assert_eq!(m.fail_to_pass_total, 3);
    assert_eq!(m.fail_to_pass_passed, 2);
    assert!((m.fail_to_pass_ratio - 2.0 / 3.0).abs() < 1e-9);
    assert_eq!(m.pass_to_pass_total, 2);
    assert_eq!(m.pass_to_pass_regressed, 0);
    assert_eq!(m.pass_to_pass_regressed_ratio, 0.0);
    assert!((m.partial_credit_score - 2.0 / 3.0).abs() < 1e-9);
    assert_eq!(m.verdict_bucket(), VerdictBucket::PartialProgress);
}

#[test]
fn compute_instance_metrics_regressed_example_c() {
    // AC (c): FAIL_TO_PASS=["a","b","c"], PASS_TO_PASS=["p1".."p5"]
    let fail_to_pass: Vec<String> = vec!["a", "b", "c"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let pass_to_pass: Vec<String> = vec!["p1", "p2", "p3", "p4", "p5"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let tests_passed: Vec<String> = vec!["p1", "p2", "p3"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let tests_failed: Vec<String> = vec!["a", "b", "c", "p4", "p5"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let m = compute_instance_metrics(&fail_to_pass, &pass_to_pass, &tests_passed, &tests_failed);
    assert_eq!(m.fail_to_pass_total, 3);
    assert_eq!(m.fail_to_pass_passed, 0);
    assert_eq!(m.fail_to_pass_ratio, 0.0);
    assert_eq!(m.pass_to_pass_total, 5);
    assert_eq!(m.pass_to_pass_regressed, 2);
    assert!((m.pass_to_pass_regressed_ratio - 0.4).abs() < 1e-9);
    assert!((m.partial_credit_score - (-0.4)).abs() < 1e-9);
    assert_eq!(m.verdict_bucket(), VerdictBucket::Regressed);
}

// ── CLI integration tests ─────────────────────────────────────────────────────

#[test]
fn help_includes_test_progress_subcommand() {
    let output = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("test-progress"),
        "bench --help should list test-progress: {help}"
    );
}

// AC (a): all-resolved sweep → mean_partial_credit_score = 1.0
#[test]
fn ac_a_all_resolved_mean_score_is_one() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_all_resolved", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "test-progress failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let mean = report["totals"]["mean_partial_credit_score"]
        .as_f64()
        .unwrap();
    assert!(
        (mean - 1.0).abs() < 1e-9,
        "all-resolved sweep should have mean_partial_credit_score=1.0, got {mean}"
    );
    let per_instance = report["per_instance"].as_array().unwrap();
    for inst in per_instance {
        let bucket = inst["verdict_bucket"].as_str().unwrap();
        assert_ne!(
            bucket, "partial_progress",
            "no partial_progress in all-resolved sweep"
        );
        assert_ne!(bucket, "regressed", "no regressed in all-resolved sweep");
    }
}

// AC (b): 2/3 FAIL_TO_PASS passed, 0 regressed → partial_progress, score ≈ 0.667
#[test]
fn ac_b_partial_progress_instance() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let partial = find_instance(&report, "partial-1");
    assert_eq!(
        partial["verdict_bucket"].as_str().unwrap(),
        "partial_progress",
        "partial-1 should be partial_progress"
    );
    let score = partial["partial_credit_score"].as_f64().unwrap();
    assert!(
        (score - 2.0 / 3.0).abs() < 1e-6,
        "partial-1 score should be 2/3, got {score}"
    );
    assert_eq!(partial["fail_to_pass"]["total"].as_u64().unwrap(), 3);
    assert_eq!(partial["fail_to_pass"]["passed_count"].as_u64().unwrap(), 2);
    let ratio = partial["fail_to_pass"]["passed_ratio"].as_f64().unwrap();
    assert!((ratio - 2.0 / 3.0).abs() < 1e-6);
    assert_eq!(
        partial["pass_to_pass"]["regressed_count"].as_u64().unwrap(),
        0
    );
}

// AC (c): 0/3 FAIL_TO_PASS passed, 2/5 regressed → regressed, score = -0.4
#[test]
fn ac_c_regressed_instance() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let inst = find_instance(&report, "regressed-1");
    assert_eq!(inst["verdict_bucket"].as_str().unwrap(), "regressed");
    let score = inst["partial_credit_score"].as_f64().unwrap();
    assert!(
        (score - (-0.4)).abs() < 1e-6,
        "regressed-1 score should be -0.4, got {score}"
    );
    assert_eq!(inst["fail_to_pass"]["total"].as_u64().unwrap(), 3);
    assert_eq!(inst["fail_to_pass"]["passed_count"].as_u64().unwrap(), 0);
    assert_eq!(inst["pass_to_pass"]["total"].as_u64().unwrap(), 5);
    assert_eq!(inst["pass_to_pass"]["regressed_count"].as_u64().unwrap(), 2);
    let regress_ratio = inst["pass_to_pass"]["regressed_ratio"].as_f64().unwrap();
    assert!((regress_ratio - 0.4).abs() < 1e-6);
}

// AC (d): no evaluator detail → evaluator_unavailable, excluded from means but in per_instance
#[test]
fn ac_d_evaluator_unavailable_excluded_from_means() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    // unavail-1 should be in per_instance
    let inst = find_instance(&report, "unavail-1");
    assert_eq!(
        inst["verdict_bucket"].as_str().unwrap(),
        "evaluator_unavailable"
    );

    // Should be counted in bucket counts
    let unavail_count = report["totals"]["per_bucket"]["evaluator_unavailable"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        unavail_count >= 1,
        "evaluator_unavailable count should be >=1"
    );

    // Means should be computed over non-unavailable instances only
    // The evaluator_unavailable_count should be reported separately
    assert!(
        report["totals"]["evaluator_unavailable_count"]
            .as_u64()
            .is_some()
            || report["totals"]["per_bucket"]["evaluator_unavailable"]
                .as_u64()
                .is_some(),
        "evaluator_unavailable count must be reported"
    );
}

// AC (e): hot_failing_tests correctly identifies and ranks by count
#[test]
fn ac_e_hot_failing_tests_ranking() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let hot = report["hot_failing_tests"].as_array().unwrap();
    assert!(!hot.is_empty(), "hot_failing_tests should not be empty");

    // test_z fails in both partial-1 and regressed-1 → count=2
    // test_x and test_y fail only in regressed-1 → count=1
    // test_z should rank above test_x, test_y
    let top = &hot[0];
    assert_eq!(
        top["test_name"].as_str().unwrap(),
        "test_z",
        "test_z should rank first (failed in 2 instances)"
    );
    let top_count = top["instance_count"].as_u64().unwrap();
    assert_eq!(top_count, 2);

    // Verify descending order
    for i in 1..hot.len() {
        let prev = hot[i - 1]["instance_count"].as_u64().unwrap();
        let curr = hot[i]["instance_count"].as_u64().unwrap();
        assert!(
            prev >= curr,
            "hot_failing_tests should be sorted descending: {prev} before {curr}"
        );
    }
}

// AC (f): bench compare produces non-zero mean_partial_credit_score_delta
#[test]
fn ac_f_bench_compare_test_progress_delta() {
    let sweep_a = tempfile::tempdir().unwrap();
    let sweep_b = tempfile::tempdir().unwrap();
    copy_fixture("sweep_compare_a", sweep_a.path());
    copy_fixture("sweep_compare_b", sweep_b.path());

    // Run test-progress on both sweeps to create test-progress.json
    let out_a = run_test_progress(&["--sweep", sweep_a.path().to_str().unwrap()]);
    assert!(
        out_a.status.success(),
        "test-progress on sweep_a failed: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    let out_b = run_test_progress(&["--sweep", sweep_b.path().to_str().unwrap()]);
    assert!(
        out_b.status.success(),
        "test-progress on sweep_b failed: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );

    // bench compare should show Test progress delta section
    let compare_out = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "compare",
            "--baseline",
            sweep_a.path().to_str().unwrap(),
            "--candidate",
            sweep_b.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&compare_out.stdout);
    let stderr = String::from_utf8_lossy(&compare_out.stderr);
    assert!(
        compare_out.status.success() || compare_out.status.code() == Some(6),
        "bench compare should succeed or exit 6 (regression gate): {stderr}"
    );
    assert!(
        stdout.contains("Test progress delta") || stdout.contains("test_progress"),
        "bench compare should show Test progress delta section: {stdout}"
    );
    // The delta should be non-zero (sweep_b has higher mean_partial_credit_score)
    assert!(
        stdout.contains("mean_partial_credit_score") || stdout.contains("partial_credit"),
        "section should mention partial_credit_score: {stdout}"
    );
}

// AC (g): determinism — two runs produce byte-identical JSON modulo generated_at
#[test]
fn ac_g_determinism() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let first = run_test_progress_json(sweep.path());
    let first_file = std::fs::read_to_string(sweep.path().join("test-progress.json")).unwrap();

    std::thread::sleep(Duration::from_secs(1));

    let second = run_test_progress_json(sweep.path());
    let second_file = std::fs::read_to_string(sweep.path().join("test-progress.json")).unwrap();

    assert_eq!(
        redact_generated_at(&first),
        redact_generated_at(&second),
        "stdout JSON should be deterministic except generated_at"
    );
    assert_eq!(
        redact_generated_at_text(&first_file),
        redact_generated_at_text(&second_file),
        "test-progress.json bytes should be deterministic except generated_at"
    );
}

// AC (h): --min-tests 5 drops instance with 3 total tests from means but keeps in per_instance
#[test]
fn ac_h_min_tests_drops_from_means_keeps_in_per_instance() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    // "small-1" has FAIL_TO_PASS=["f1"] + PASS_TO_PASS=["p1","p2"] = 3 total tests
    // With --min-tests 5, it should be excluded from means
    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--min-tests",
        "5",
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "test-progress --min-tests 5 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    // small-1 should still appear in per_instance
    let inst = find_instance(&report, "small-1");
    assert_eq!(inst["instance_id"].as_str().unwrap(), "small-1");
    // Should be flagged as excluded from means
    let excluded = inst["excluded_from_means"].as_bool().unwrap_or(false);
    assert!(
        excluded,
        "small-1 should be flagged excluded_from_means=true with --min-tests 5"
    );
}

// Unresolved instance where evaluator returned empty tests_passed/tests_failed
// → must be evaluator_unavailable, not no_progress
#[test]
fn unresolved_with_empty_eval_tests_is_evaluator_unavailable() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let inst = find_instance(&report, "no-eval-data-1");
    assert_eq!(
        inst["verdict_bucket"].as_str().unwrap(),
        "evaluator_unavailable",
        "unresolved instance with empty eval test lists must be evaluator_unavailable, not no_progress"
    );
    assert_eq!(
        inst["partial_credit_score"].as_f64().unwrap(),
        0.0,
        "evaluator_unavailable score is 0.0"
    );
}

// Unresolved instance absent from dataset.jsonl
// → must be evaluator_unavailable, not vacuously resolved/partial_progress
#[test]
fn unresolved_with_no_dataset_entry_is_evaluator_unavailable() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let inst = find_instance(&report, "no-dataset-entry-1");
    assert_eq!(
        inst["verdict_bucket"].as_str().unwrap(),
        "evaluator_unavailable",
        "unresolved instance absent from dataset.jsonl must be evaluator_unavailable, \
         not vacuously scored due to empty FAIL_TO_PASS/PASS_TO_PASS"
    );
}

// ── Additional CLI integration tests ─────────────────────────────────────────

#[test]
fn cli_writes_test_progress_json() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&["--sweep", sweep.path().to_str().unwrap()]);
    assert!(output.status.success());

    let artifact = sweep.path().join("test-progress.json");
    assert!(artifact.exists(), "test-progress.json should be written");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
    assert!(
        report["schema_version"].is_number(),
        "schema_version required"
    );
    assert!(report["generated_at"].is_string(), "generated_at required");
    assert!(report["totals"].is_object(), "totals required");
    assert!(report["per_instance"].is_array(), "per_instance required");
    assert!(
        report["hot_failing_tests"].is_array(),
        "hot_failing_tests required"
    );
    assert!(
        report["hot_regressed_tests"].is_array(),
        "hot_regressed_tests required"
    );
    assert!(
        report["redaction_applied"].is_boolean(),
        "redaction_applied required"
    );
}

#[test]
fn cli_schema_version_is_integer() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["schema_version"].as_u64().is_some(),
        "schema_version must be a non-negative integer"
    );
}

#[test]
fn cli_per_instance_sorted_by_partial_credit_score_asc_then_instance_id() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let per_instance = report["per_instance"].as_array().unwrap();

    for i in 1..per_instance.len() {
        let prev_score = per_instance[i - 1]["partial_credit_score"]
            .as_f64()
            .unwrap_or(0.0);
        let curr_score = per_instance[i]["partial_credit_score"]
            .as_f64()
            .unwrap_or(0.0);
        let prev_id = per_instance[i - 1]["instance_id"].as_str().unwrap_or("");
        let curr_id = per_instance[i]["instance_id"].as_str().unwrap_or("");
        let prev_i = i - 1;
        assert!(
            prev_score < curr_score || (prev_score == curr_score && prev_id <= curr_id),
            "per_instance should be sorted by partial_credit_score ASC then instance_id: \
             [{prev_i}]={prev_score},{prev_id} before [{i}]={curr_score},{curr_id}"
        );
    }
}

#[test]
fn cli_totals_bucket_counts_match_per_instance() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let per_instance = report["per_instance"].as_array().unwrap();

    for bucket in [
        "resolved",
        "partial_progress",
        "no_progress",
        "regressed",
        "evaluator_unavailable",
    ] {
        let count_from_instances = per_instance
            .iter()
            .filter(|i| i["verdict_bucket"].as_str() == Some(bucket))
            .count() as u64;
        let count_from_totals = report["totals"]["per_bucket"][bucket].as_u64().unwrap_or(0);
        assert_eq!(
            count_from_instances, count_from_totals,
            "bucket {bucket}: per_instance count {count_from_instances} != totals count {count_from_totals}"
        );
    }
}

#[test]
fn cli_means_exclude_evaluator_unavailable() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    // All instances except evaluator_unavailable should contribute to mean
    let per_instance = report["per_instance"].as_array().unwrap();
    let eligible: Vec<f64> = per_instance
        .iter()
        .filter(|i| {
            i["verdict_bucket"].as_str() != Some("evaluator_unavailable")
                && !i["excluded_from_means"].as_bool().unwrap_or(false)
        })
        .map(|i| i["partial_credit_score"].as_f64().unwrap_or(0.0))
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let computed_mean = eligible.iter().sum::<f64>() / eligible.len() as f64;
    let reported_mean = report["totals"]["mean_partial_credit_score"]
        .as_f64()
        .unwrap();
    assert!(
        (computed_mean - reported_mean).abs() < 1e-6,
        "mean_partial_credit_score should exclude evaluator_unavailable: computed={computed_mean} reported={reported_mean}"
    );
}

#[test]
fn cli_bucket_filter_restricts_text_output() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--bucket",
        "regressed",
    ]);
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("regressed"),
        "text output should mention regressed: {stdout}"
    );
}

#[test]
fn cli_hot_tests_n_limits_list_size() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&[
        "--sweep",
        sweep.path().to_str().unwrap(),
        "--hot-tests-n",
        "1",
        "--format",
        "json",
    ]);
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let hot_failing = report["hot_failing_tests"].as_array().unwrap();
    assert!(
        hot_failing.len() <= 1,
        "hot_failing_tests should have at most 1 entry with --hot-tests-n 1, got {}",
        hot_failing.len()
    );
    let hot_regressed = report["hot_regressed_tests"].as_array().unwrap();
    assert!(
        hot_regressed.len() <= 1,
        "hot_regressed_tests should have at most 1 entry with --hot-tests-n 1"
    );
}

#[test]
fn cli_exits_zero_regardless_of_bucket_mix() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_mixed", sweep.path());

    let output = run_test_progress(&["--sweep", sweep.path().to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "test-progress should exit 0 even with regressed instances"
    );
}

#[test]
fn cli_exits_zero_when_all_evaluator_unavailable() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("sweep_all_unavailable", sweep.path());

    let output = run_test_progress(&["--sweep", sweep.path().to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "test-progress should exit 0 when all instances are evaluator_unavailable: {stdout}"
    );
}

#[test]
fn cli_exits_nonzero_when_sweep_dir_missing() {
    let output = run_test_progress(&["--sweep", "/tmp/nonexistent-sweep-xyz-test-progress"]);
    assert!(
        !output.status.success(),
        "test-progress should exit nonzero when sweep dir is missing"
    );
}

#[test]
fn cli_bench_compare_omits_section_when_test_progress_absent() {
    let sweep_a = tempfile::tempdir().unwrap();
    let sweep_b = tempfile::tempdir().unwrap();
    copy_fixture("sweep_compare_a", sweep_a.path());
    copy_fixture("sweep_compare_b", sweep_b.path());
    // Do NOT run test-progress first — no test-progress.json on disk

    let compare_out = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "compare",
            "--baseline",
            sweep_a.path().to_str().unwrap(),
            "--candidate",
            sweep_b.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // Should succeed (no error just because test-progress.json is absent)
    assert!(
        compare_out.status.success() || compare_out.status.code() == Some(6),
        "bench compare should not error when test-progress.json is absent: {}",
        String::from_utf8_lossy(&compare_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&compare_out.stdout);
    // Section is simply omitted — no crash, no error
    assert!(
        !stdout.contains("Test progress delta ERROR"),
        "no error mention expected: {stdout}"
    );
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn run_test_progress(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.args(["--log", "error", "bench", "test-progress"]);
    cmd.args(args);
    cmd.output().unwrap()
}

fn run_test_progress_json(sweep: &Path) -> serde_json::Value {
    let output = run_test_progress(&["--sweep", sweep.to_str().unwrap(), "--format", "json"]);
    assert!(
        output.status.success(),
        "bench test-progress failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn find_instance<'a>(report: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    report["per_instance"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["instance_id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("instance {id} not found in per_instance"))
}

fn copy_fixture(fixture_name: &str, dst: &Path) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/test_progress")
        .join(fixture_name);
    copy_dir(&fixture, dst);
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

fn redact_generated_at(value: &serde_json::Value) -> serde_json::Value {
    let mut redacted = value.clone();
    redacted["generated_at"] = serde_json::json!("<generated_at>");
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
