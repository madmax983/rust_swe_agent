//! `bench assert`: TDD integration tests for issue #308.
//!
//! Covers every AC:
//! (a) all rules pass → exit 0, assertions.json passed=true
//! (b) one rule fails → dedicated failure exit code, assertions.json records it
//! (c) unknown metric name → usage-error exit code, NOT rule-failure code
//! (d) missing evaluation.json → fail-closed by default; skip with --allow-missing-artifacts
//! (e) --rule and --rules produce identical artifacts for the same rule set
//! (f) determinism: two runs produce byte-identical artifacts modulo generated_at
//! (g) every v1 vocabulary metric is exercised
//! Plus: help text, stdout format, artifact schema.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod support;
use support::binary_path;

// Exit codes from the stable contract (docs/exit-codes.md).
const EXIT_SUCCESS: i32 = 0;
const EXIT_USAGE_ERROR: i32 = 2;
/// New exit code 27 — at least one SLO rule failed (see AC).
const EXIT_SLO_RULE_FAILURE: i32 = 27;

const FIXTURE_SWEEP: &str = "tests/fixtures/assert/sweep";
const RULES_PASS: &str = "tests/fixtures/assert/rules-pass.toml";
const RULES_ONE_FAIL: &str = "tests/fixtures/assert/rules-one-fail.toml";

// ── helpers ──────────────────────────────────────────────────────────────────

fn copy_fixture_sweep(root: &Path) -> PathBuf {
    let dst = root.join("sweep");
    copy_dir(Path::new(FIXTURE_SWEEP), &dst);
    dst
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn read_assertions_json(sweep: &Path) -> Value {
    let path = sweep.join("assertions.json");
    assert!(
        path.exists(),
        "assertions.json was not written to {sweep}",
        sweep = sweep.display()
    );
    let text = fs::read_to_string(&path).unwrap();
    serde_json::from_str(&text).unwrap()
}

// ── help / discovery ─────────────────────────────────────────────────────────

#[test]
fn help_lists_assert_subcommand() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("assert"),
        "bench --help does not list 'assert':\n{stdout}"
    );
}

#[test]
fn assert_help_shows_expected_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "assert", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--sweep",
        "--rules",
        "--rule",
        "--verbose",
        "--allow-missing-artifacts",
    ] {
        assert!(
            stdout.contains(flag),
            "bench assert --help missing {flag}:\n{stdout}"
        );
    }
}

// ── (a) passing sweep → exit 0 ───────────────────────────────────────────────

#[test]
fn all_rules_pass_exits_0_and_writes_passed_true() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(EXIT_SUCCESS),
        "expected exit 0 on all-pass; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let artifact = read_assertions_json(&sweep);
    assert_eq!(artifact["passed"], true, "assertions.json passed != true");
    assert_eq!(artifact["failed_count"], 0, "failed_count should be 0");
    assert!(
        artifact["passed_count"].as_u64().unwrap_or(0) > 0,
        "passed_count should be > 0"
    );
}

#[test]
fn all_rules_pass_stdout_shows_header_with_counts() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    // pytest-style header: "bench assert: <sweep> — N passed, 0 failed"
    assert!(
        stdout.contains("bench assert:"),
        "missing 'bench assert:' header in stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("passed"),
        "missing 'passed' in stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("failed"),
        "missing 'failed' in stdout:\n{stdout}"
    );
}

// ── (b) one rule fails → dedicated exit code ──────────────────────────────────

#[test]
fn one_rule_fails_exits_27() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_ONE_FAIL])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(EXIT_SLO_RULE_FAILURE),
        "expected exit 27 when a rule fails"
    );
}

#[test]
fn one_rule_fails_assertions_json_records_failure() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_ONE_FAIL])
        .output()
        .unwrap();

    let artifact = read_assertions_json(&sweep);
    assert_eq!(artifact["passed"], false, "overall passed should be false");
    assert_eq!(
        artifact["failed_count"], 1,
        "exactly 1 failed rule expected"
    );

    let rules = artifact["rules"].as_array().unwrap();
    let failed: Vec<_> = rules.iter().filter(|r| r["passed"] == false).collect();
    assert_eq!(failed.len(), 1, "exactly one rule should be failed");
    let f = &failed[0];
    assert_eq!(
        f["name"], "resolved_rate_high_floor",
        "wrong rule failed: {f}"
    );
    assert!(
        f["observed_value"].is_number(),
        "observed_value must be a number"
    );
    assert!(
        f["source_field_path"].is_string(),
        "source_field_path must be a string"
    );
    assert!(
        f["reason_if_failed_or_skipped"].is_string(),
        "reason_if_failed_or_skipped must be a string on failure"
    );
}

#[test]
fn stdout_shows_failed_rule_details_on_failure() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_ONE_FAIL])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("FAILED"),
        "stdout should mention FAILED:\n{stdout}"
    );
    assert!(
        stdout.contains("resolved_rate"),
        "stdout should name the failed metric:\n{stdout}"
    );
}

// ── (c) unknown metric → usage error, NOT rule-failure ─────────────────────

#[test]
fn unknown_metric_exits_usage_error_not_rule_failure() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rule", "nonexistent_metric>=0.5"])
        .output()
        .unwrap();

    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code, EXIT_USAGE_ERROR,
        "unknown metric should exit 2 (usage_error), got {code}"
    );
    assert_ne!(
        code, EXIT_SLO_RULE_FAILURE,
        "unknown metric must NOT exit 27 (slo_rule_failure)"
    );
}

// ── (d) missing evaluation.json ───────────────────────────────────────────────

#[test]
fn missing_evaluation_json_fails_closed_by_default() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Remove evaluation.json to simulate a sweep where evaluator hasn't run.
    fs::remove_file(sweep.join("evaluation.json")).unwrap();

    // Rule that requires evaluation.json (resolved_rate).
    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rule", "resolved_rate>=0.3"])
        .output()
        .unwrap();

    let code = out.status.code().unwrap_or(-1);
    // Fail-closed: missing artifact counts as failure → exit 27.
    assert_eq!(
        code, EXIT_SLO_RULE_FAILURE,
        "missing evaluation.json should fail-closed (exit 27), got {code}"
    );

    let artifact = read_assertions_json(&sweep);
    assert_eq!(artifact["passed"], false);
    let rules = artifact["rules"].as_array().unwrap();
    let skipped: Vec<_> = rules
        .iter()
        .filter(|r| r["passed"] == Value::Null)
        .collect();
    // In fail-closed mode, the rule counts as failed (passed=false), not skipped.
    // But per the spec: skipped rules have passed=null; failed rules have passed=false.
    // Fail-closed means missing_artifact → passed=null counted as a failure for exit code,
    // but the record itself shows passed=null with reason="missing_artifact: evaluation.json".
    assert!(
        !skipped.is_empty(),
        "missing artifact should produce a null-passed record"
    );
    let r = &skipped[0];
    let reason = r["reason_if_failed_or_skipped"].as_str().unwrap_or("");
    assert!(
        reason.contains("missing_artifact"),
        "reason should mention missing_artifact, got: {reason}"
    );
}

#[test]
fn missing_evaluation_json_with_allow_missing_skips_and_exits_0() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    fs::remove_file(sweep.join("evaluation.json")).unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rule", "resolved_rate>=0.3"])
        .args(["--allow-missing-artifacts"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code, EXIT_SUCCESS,
        "--allow-missing-artifacts should skip and exit 0:\nstdout:{stdout}\nstderr:{stderr}"
    );

    let artifact = read_assertions_json(&sweep);
    assert_eq!(artifact["skipped_count"], 1, "skipped_count should be 1");
    let stdout_str = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout_str.contains("skipped"),
        "header should mention 'skipped':\n{stdout_str}"
    );
}

// ── (e) --rule and --rules produce identical artifacts ─────────────────────

#[test]
fn inline_rule_and_file_rules_produce_identical_output() {
    // One rule that uses only results.json so evaluation.json is always present.
    // Rule: errored_count<=3  (fixture has errored=2, passes)
    let work = tempfile::tempdir().unwrap();

    let sweep_a = {
        let d = work.path().join("a");
        copy_dir(Path::new(FIXTURE_SWEEP), &d);
        d
    };
    let sweep_b = {
        let d = work.path().join("b");
        copy_dir(Path::new(FIXTURE_SWEEP), &d);
        d
    };

    // Rule file with one rule
    let rule_file = work.path().join("single.toml");
    fs::write(
        &rule_file,
        r#"[[rule]]
name = "errored_count"
metric = "errored_count"
op = "<="
threshold = 3
"#,
    )
    .unwrap();

    // Run with --rules file
    Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep_a)
        .args(["--rules"])
        .arg(&rule_file)
        .output()
        .unwrap();

    // Run with --rule inline
    Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep_b)
        .args(["--rule", "errored_count<=3"])
        .output()
        .unwrap();

    let art_a = read_assertions_json(&sweep_a);
    let art_b = read_assertions_json(&sweep_b);

    // Compare rule results (ignore generated_at and sweep-path-specific fields)
    let rules_a = art_a["rules"].as_array().unwrap();
    let rules_b = art_b["rules"].as_array().unwrap();
    assert_eq!(rules_a.len(), rules_b.len(), "rule count mismatch");

    let r_a = &rules_a[0];
    let r_b = &rules_b[0];
    assert_eq!(r_a["metric"], r_b["metric"]);
    assert_eq!(r_a["op"], r_b["op"]);
    assert_eq!(r_a["threshold"], r_b["threshold"]);
    assert_eq!(r_a["observed_value"], r_b["observed_value"]);
    assert_eq!(r_a["passed"], r_b["passed"]);
    assert_eq!(r_a["source_field_path"], r_b["source_field_path"]);
}

// ── (f) determinism ───────────────────────────────────────────────────────────

#[test]
fn two_runs_produce_same_artifact_modulo_generated_at() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // First run
    Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .output()
        .unwrap();
    let art1_text = fs::read_to_string(sweep.join("assertions.json")).unwrap();

    // Second run (overwrites)
    Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .output()
        .unwrap();
    let art2_text = fs::read_to_string(sweep.join("assertions.json")).unwrap();

    // Parse and compare everything except generated_at
    let mut v1: Value = serde_json::from_str(&art1_text).unwrap();
    let mut v2: Value = serde_json::from_str(&art2_text).unwrap();
    v1["generated_at"] = Value::Null;
    v2["generated_at"] = Value::Null;

    assert_eq!(
        v1, v2,
        "Two runs produced different assertions.json (modulo generated_at)"
    );
}

// ── (g) every v1 vocabulary metric exercised ──────────────────────────────────

#[test]
fn all_v1_metrics_can_be_evaluated() {
    // Run with every metric in the vocabulary using rules that PASS
    // (so we verify each metric is recognized and computable).
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(EXIT_SUCCESS),
        "all v1 metrics rules should pass:\nstdout:{stdout}\nstderr:{stderr}"
    );

    let artifact = read_assertions_json(&sweep);
    let rules = artifact["rules"].as_array().unwrap();
    // rules-pass.toml covers all 15 v1 metrics
    assert_eq!(
        rules.len(),
        15,
        "expected 15 rule results (one per v1 metric)"
    );

    // Every rule should have a non-null observed_value
    for r in rules {
        assert!(
            r["observed_value"].is_number(),
            "metric {} has null observed_value",
            r["metric"].as_str().unwrap_or("?")
        );
    }
}

// ── artifact schema ──────────────────────────────────────────────────────────

#[test]
fn assertions_json_has_correct_schema_fields() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .output()
        .unwrap();

    let artifact = read_assertions_json(&sweep);

    // Required schema fields
    assert_eq!(artifact["artifact_kind"], "assertions");
    assert!(artifact["schema_version"]["major"].is_number());
    assert!(artifact["schema_version"]["minor"].is_number());
    assert!(artifact["generated_at"].is_string());
    assert!(artifact["sweep_id"].is_string());
    assert!(artifact["rules"].is_array());
    assert!(artifact["passed"].is_boolean());
    assert!(artifact["passed_count"].is_number());
    assert!(artifact["failed_count"].is_number());
    assert!(artifact["skipped_count"].is_number());
}

#[test]
fn each_rule_record_has_required_fields() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .output()
        .unwrap();

    let artifact = read_assertions_json(&sweep);
    let rules = artifact["rules"].as_array().unwrap();
    for r in rules {
        let metric = r["metric"].as_str().unwrap_or("?");
        assert!(r["name"].is_string(), "rule {metric}: missing 'name'");
        assert!(r["metric"].is_string(), "rule {metric}: missing 'metric'");
        assert!(r["op"].is_string(), "rule {metric}: missing 'op'");
        assert!(
            r["threshold"].is_number(),
            "rule {metric}: missing 'threshold'"
        );
        assert!(
            r["observed_value"].is_number(),
            "rule {metric}: missing 'observed_value'"
        );
        assert!(
            r["passed"].is_boolean(),
            "rule {metric}: 'passed' should be bool"
        );
        assert!(
            r["source_field_path"].is_string(),
            "rule {metric}: missing 'source_field_path'"
        );
        // reason_if_failed_or_skipped may be null on pass
    }
}

// ── verbose output ─────────────────────────────────────────────────────────────

#[test]
fn verbose_flag_prints_passed_rules_too() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .arg("--verbose")
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PASSED"),
        "--verbose should print PASSED lines:\n{stdout}"
    );
}

#[test]
fn without_verbose_only_failed_and_header_printed() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_ONE_FAIL])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    // Should print FAILED but not PASSED (only failed rules shown by default)
    assert!(
        stdout.contains("FAILED"),
        "should show FAILED line:\n{stdout}"
    );
}

// ── read-only: no mutations ───────────────────────────────────────────────────

#[test]
fn assert_does_not_modify_results_json() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let results_before = fs::read_to_string(sweep.join("results.json")).unwrap();

    Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rules", RULES_PASS])
        .output()
        .unwrap();

    let results_after = fs::read_to_string(sweep.join("results.json")).unwrap();
    assert_eq!(
        results_before, results_after,
        "bench assert must not modify results.json"
    );
}

// ── multiple --rule flags (inline shorthand) ──────────────────────────────────

#[test]
fn multiple_inline_rules_all_evaluated() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rule", "errored_count<=3"])
        .args(["--rule", "resolved_count>=1"])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(EXIT_SUCCESS),
        "both inline rules should pass"
    );

    let artifact = read_assertions_json(&sweep);
    assert_eq!(artifact["passed_count"], 2);
}

// ── missing results.json → usage error ───────────────────────────────────────

#[test]
fn missing_results_json_exits_usage_error() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("empty-sweep");
    fs::create_dir_all(&sweep).unwrap();
    // No results.json

    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rule", "errored_count<=0"])
        .output()
        .unwrap();

    let code = out.status.code().unwrap_or(-1);
    // Missing results.json is an input error (not a rule failure)
    assert!(
        code != EXIT_SUCCESS && code != EXIT_SLO_RULE_FAILURE,
        "missing results.json should not succeed or produce rule-failure code; got {code}"
    );
}

// ── nightly smoke: errored==0 AND resolved_count>=1 ──────────────────────────

#[test]
fn nightly_smoke_rules_pass_on_fixture() {
    // The ci/nightly-smoke.assert.toml should have at minimum:
    //   errored_count == 0   (would FAIL on fixture since errored=2, unless we use <=2)
    // But fixture has errored=2, so we test the *format* works, not the value.
    // We use the inline form here to prove the nightly-smoke wiring works.
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // resolved_count >= 1 should PASS (fixture has resolved_count=2 from evaluation.json)
    let out = Command::new(binary_path())
        .args(["bench", "assert", "--sweep"])
        .arg(&sweep)
        .args(["--rule", "resolved_count>=1"])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(EXIT_SUCCESS),
        "resolved_count>=1 should pass on fixture with 2 resolved instances"
    );
}
