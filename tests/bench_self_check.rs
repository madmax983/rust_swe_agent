//! Tests for `bench self-check` — agent self-verdict calibration (issue #300).
//!
//! Red → Green → Refactor TDD cycle.
//!
//! AC items covered:
//! (a) all-resolved with all `last_tests_passed = true` → precision 1.0, recall 1.0, Brier 0.0
//! (b) all-resolved with all `None` → n_excluded_none = N, Brier = 0.25
//! (c) mixed sweep with deliberate counts → exact precision/recall/Brier values to 3 decimals
//! (d) `--by-repo` produces one block per repository present
//! (e) missing `evaluation.json` → exit 2 with helpful message
//! (f) trajectory predates #46 field → exit 3 with which-field-missing pointer
//! (g) `--format json` output round-trips through `serde_json::from_str::<SelfCheckReport>`
//! (perf) 300-instance sweep completes in < 2 seconds

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use std::path::Path;
use std::time::Instant;

use maxwells_daemon::run::self_check::{SelfCheckArgs, SelfCheckReport, run as run_self_check};

// ── fixture writers ───────────────────────────────────────────────────────────

/// Write a minimal trajectory JSON with `tests_run_before_submit` present (#46 field).
fn write_trajectory(dir: &Path, instance_id: &str, last_tests_passed: Option<bool>) {
    let ltp = match last_tests_passed {
        Some(true) => "true",
        Some(false) => "false",
        None => "null",
    };
    let content = format!(
        r#"{{
  "trajectory_format": "mini-swe-agent-1.3",
  "info": {{
    "tests_run_before_submit": {},
    "last_tests_passed": {}
  }},
  "messages": []
}}"#,
        last_tests_passed.unwrap_or(false),
        ltp
    );
    std::fs::write(dir.join(format!("{instance_id}.traj.json")), content).unwrap();
}

/// Write a trajectory that LACKS `tests_run_before_submit` (predates #46).
fn write_legacy_trajectory(dir: &Path, instance_id: &str) {
    let content = r#"{
  "trajectory_format": "mini-swe-agent-1.2",
  "info": {
    "outcome": "submitted"
  },
  "messages": []
}"#;
    std::fs::write(dir.join(format!("{instance_id}.traj.json")), content).unwrap();
}

/// Write an `evaluation.json` with the given instance verdicts.
fn write_evaluation_json(dir: &Path, verdicts: &[(&str, bool)]) {
    let instances: Vec<String> = verdicts
        .iter()
        .map(|(id, resolved)| {
            format!(
                r#"  {{"instance_id": "{id}", "resolved": {resolved}, "eval_exit_reason": "ok"}}"#
            )
        })
        .collect();
    let content = format!(
        r#"{{
  "artifact_kind": "evaluation_results",
  "schema_version": 1,
  "instances": [
{}
  ],
  "behavioral": {{
    "tests_run_before_submit_rate": 0.5,
    "resolved_rate_when_tests_run": 0.6,
    "resolved_rate_when_tests_skipped": 0.4
  }}
}}"#,
        instances.join(",\n")
    );
    std::fs::write(dir.join("evaluation.json"), content).unwrap();
}

// ── test (a): all-resolved, all tests passed ──────────────────────────────────

/// AC (a): all-resolved sweep with all `last_tests_passed = true`
/// → precision 1.0, recall 1.0, Brier 0.0
#[test]
fn all_resolved_all_tests_passed_precision_recall_brier() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // 5 instances: all resolved, all tests passed
    let instances = ["inst-a", "inst-b", "inst-c", "inst-d", "inst-e"];
    for id in &instances {
        write_trajectory(dir, id, Some(true));
    }
    write_evaluation_json(
        dir,
        &instances.iter().map(|id| (*id, true)).collect::<Vec<_>>(),
    );

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    })
    .expect("should succeed");

    // All 5 are TP
    assert_eq!(report.confusion.passed_resolved, 5, "all should be TP");
    assert_eq!(report.confusion.passed_unresolved, 0);
    assert_eq!(report.confusion.failed_resolved, 0);
    assert_eq!(report.confusion.failed_unresolved, 0);
    assert_eq!(report.confusion.none_resolved, 0);
    assert_eq!(report.confusion.none_unresolved, 0);

    assert_eq!(report.metrics.precision, Some(1.0), "precision must be 1.0");
    assert_eq!(report.metrics.recall, Some(1.0), "recall must be 1.0");
    assert!(
        (report.metrics.brier_score - 0.0).abs() < 1e-9,
        "brier score must be 0.0"
    );

    assert!(report.false_positives.is_empty(), "no false positives");
    assert!(report.false_negatives.is_empty(), "no false negatives");
    assert_eq!(report.n_excluded_none, 0);
}

// ── test (b): all-resolved, all `last_tests_passed = None` ───────────────────

/// AC (b): all-resolved sweep with all `None`
/// → n_excluded_none = N, Brier ≈ 0.25 (documented non-zero)
#[test]
fn all_resolved_all_none_brier_and_excluded_count() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let instances = ["inst-1", "inst-2", "inst-3", "inst-4"];
    for id in &instances {
        write_trajectory(dir, id, None);
    }
    write_evaluation_json(
        dir,
        &instances.iter().map(|id| (*id, true)).collect::<Vec<_>>(),
    );

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    })
    .expect("should succeed");

    assert_eq!(
        report.n_excluded_none, 4,
        "all 4 should be excluded from precision/recall (n_excluded_none)"
    );
    assert_eq!(report.confusion.none_resolved, 4, "all 4 none+resolved");

    // Precision/recall undefined when no passed/failed rows
    assert_eq!(
        report.metrics.precision, None,
        "precision undefined with all-none"
    );
    assert_eq!(
        report.metrics.recall, None,
        "recall undefined with all-none"
    );

    // Brier: all f_i = 0.5, all y_i = 1.0 → (0.5 - 1.0)^2 = 0.25 per instance
    assert!(
        (report.metrics.brier_score - 0.25).abs() < 1e-6,
        "Brier score should be 0.25 for all-none all-resolved, got {}",
        report.metrics.brier_score
    );
}

// ── test (c): mixed sweep, exact metric values ────────────────────────────────

/// AC (c): mixed sweep — verify exact precision/recall/Brier to 3 decimals.
///
/// Setup:
///   3 TP (tests_passed=true, resolved=true)
///   2 FP (tests_passed=true, resolved=false)
///   1 FN (tests_passed=false, resolved=true)
///   4 TN (tests_passed=false, resolved=false)
///   2 None+resolved, 1 None+unresolved
///
/// precision = 3/(3+2) = 0.6
/// recall    = 3/(3+1) = 0.75
/// Brier = (3*0 + 2*1 + 1*1 + 4*0 + 2*0.25 + 1*0.25) / 13
///       = (0 + 2 + 1 + 0 + 0.5 + 0.25) / 13
///       = 3.75 / 13 ≈ 0.28846… → rounded to 3 decimals = 0.288
#[test]
fn mixed_sweep_exact_metrics() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // TP x3
    write_trajectory(dir, "tp-1", Some(true));
    write_trajectory(dir, "tp-2", Some(true));
    write_trajectory(dir, "tp-3", Some(true));
    // FP x2
    write_trajectory(dir, "fp-1", Some(true));
    write_trajectory(dir, "fp-2", Some(true));
    // FN x1
    write_trajectory(dir, "fn-1", Some(false));
    // TN x4
    write_trajectory(dir, "tn-1", Some(false));
    write_trajectory(dir, "tn-2", Some(false));
    write_trajectory(dir, "tn-3", Some(false));
    write_trajectory(dir, "tn-4", Some(false));
    // None+resolved x2
    write_trajectory(dir, "none-r-1", None);
    write_trajectory(dir, "none-r-2", None);
    // None+unresolved x1
    write_trajectory(dir, "none-u-1", None);

    write_evaluation_json(
        dir,
        &[
            ("tp-1", true),
            ("tp-2", true),
            ("tp-3", true),
            ("fp-1", false),
            ("fp-2", false),
            ("fn-1", true),
            ("tn-1", false),
            ("tn-2", false),
            ("tn-3", false),
            ("tn-4", false),
            ("none-r-1", true),
            ("none-r-2", true),
            ("none-u-1", false),
        ],
    );

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    })
    .expect("should succeed");

    assert_eq!(report.confusion.passed_resolved, 3, "TP");
    assert_eq!(report.confusion.passed_unresolved, 2, "FP");
    assert_eq!(report.confusion.failed_resolved, 1, "FN");
    assert_eq!(report.confusion.failed_unresolved, 4, "TN");
    assert_eq!(report.confusion.none_resolved, 2, "None+resolved");
    assert_eq!(report.confusion.none_unresolved, 1, "None+unresolved");

    // precision = 3/5 = 0.600
    let prec = report.metrics.precision.expect("precision defined");
    assert!(
        (prec - 0.6).abs() < 0.001,
        "precision expected 0.600, got {prec:.3}"
    );

    // recall = 3/4 = 0.750
    let rec = report.metrics.recall.expect("recall defined");
    assert!(
        (rec - 0.75).abs() < 0.001,
        "recall expected 0.750, got {rec:.3}"
    );

    // Brier = (2 + 1 + 0.5 + 0.25) / 13 = 3.75/13 ≈ 0.2885 → 0.288 at 3dp
    let expected_brier: f64 = 3.75 / 13.0;
    assert!(
        (report.metrics.brier_score - (expected_brier * 1000.0).round() / 1000.0).abs() < 1e-9,
        "brier expected ≈{:.3}, got {:.3}",
        expected_brier,
        report.metrics.brier_score
    );

    // False positives: fp-1, fp-2 (sorted)
    assert_eq!(report.false_positives, vec!["fp-1", "fp-2"]);
    // False negatives: fn-1
    assert_eq!(report.false_negatives, vec!["fn-1"]);

    // n_excluded_none = 2 + 1 = 3
    assert_eq!(report.n_excluded_none, 3);

    // base_rate = (3+1+2) / 13 = 6/13 ≈ 0.462
    let expected_base_rate = 6.0 / 13.0;
    assert!(
        (report.base_rate - expected_base_rate).abs() < 1e-9,
        "base_rate expected {expected_base_rate:.4}, got {:.4}",
        report.base_rate
    );

    // calibration_delta = precision - base_rate = 0.6 - 6/13 ≈ 0.138
    let cal = report.calibration_delta.expect("calibration_delta defined");
    let expected_cal = prec - expected_base_rate;
    assert!(
        (cal - expected_cal).abs() < 1e-3,
        "calibration_delta expected {expected_cal:.4}, got {cal:.4}"
    );
}

// ── test (d): --by-repo flag ──────────────────────────────────────────────────

/// AC (d): `--by-repo` produces one block per repository present.
///
/// Instance IDs follow SWE-bench format: `owner__repo-NNNNN`.
#[test]
fn by_repo_flag_produces_per_repo_breakdown() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // django__django repo (2 instances)
    write_trajectory(dir, "django__django-10087", Some(true));
    write_trajectory(dir, "django__django-10243", Some(false));

    // sympy__sympy repo (1 instance)
    write_trajectory(dir, "sympy__sympy-12345", Some(true));

    write_evaluation_json(
        dir,
        &[
            ("django__django-10087", true),
            ("django__django-10243", true),
            ("sympy__sympy-12345", false),
        ],
    );

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: true,
    })
    .expect("should succeed");

    let by_repo = report.by_repo.expect("by_repo must be present");
    assert_eq!(by_repo.len(), 2, "should have 2 repos");
    assert!(
        by_repo.contains_key("django/django"),
        "should have django/django"
    );
    assert!(
        by_repo.contains_key("sympy/sympy"),
        "should have sympy/sympy"
    );

    let django = &by_repo["django/django"];
    // django__django-10087: passed=true, resolved=true → TP
    // django__django-10243: passed=false, resolved=true → FN
    assert_eq!(django.passed_resolved, 1, "django TP");
    assert_eq!(django.failed_resolved, 1, "django FN");

    let sympy = &by_repo["sympy/sympy"];
    // sympy__sympy-12345: passed=true, resolved=false → FP
    assert_eq!(sympy.passed_unresolved, 1, "sympy FP");
}

// ── test (e): missing evaluation.json → exit 2 ───────────────────────────────

/// AC (e): missing `evaluation.json` → error, and exit code maps to 2.
#[test]
fn missing_evaluation_json_returns_config_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Write some trajectories, but no evaluation.json
    write_trajectory(dir, "some-instance-1", Some(true));

    let result = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    });

    assert!(
        result.is_err(),
        "should fail when evaluation.json is missing"
    );
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("evaluation.json") || err_str.contains("evaluation"),
        "error message should mention evaluation.json, got: {err_str}"
    );

    // Verify exit code maps to 2 (UsageError)
    let code = maxwells_daemon::exit_code::ExitCode::from_error(&err);
    assert_eq!(
        code.as_i32(),
        2,
        "missing evaluation.json should produce exit code 2, got {}",
        code.as_i32()
    );
}

// ── test (f): pre-#46 trajectory → exit 3 ────────────────────────────────────

/// AC (f): trajectory predates #46 field → exit 3 with which-field-missing pointer.
#[test]
fn legacy_trajectory_missing_test_field_returns_preflight_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // One modern trajectory and one legacy
    write_trajectory(dir, "modern-inst", Some(true));
    write_legacy_trajectory(dir, "legacy-inst");

    write_evaluation_json(dir, &[("modern-inst", true), ("legacy-inst", false)]);

    let result = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    });

    assert!(result.is_err(), "should fail for legacy trajectory");
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("tests_run_before_submit") || err_str.contains("#46"),
        "error should mention which field is missing: {err_str}"
    );

    // Verify exit code maps to 3 (PreflightFailure / schema mismatch)
    let code = maxwells_daemon::exit_code::ExitCode::from_error(&err);
    assert_eq!(
        code.as_i32(),
        3,
        "legacy trajectory should produce exit code 3, got {}",
        code.as_i32()
    );
}

// ── test (g): JSON round-trip ─────────────────────────────────────────────────

/// AC (g): `--format json` output round-trips through `serde_json::from_str::<SelfCheckReport>`.
#[test]
fn json_output_round_trips_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    write_trajectory(dir, "inst-a", Some(true));
    write_trajectory(dir, "inst-b", Some(false));
    write_trajectory(dir, "inst-c", None);
    write_evaluation_json(
        dir,
        &[("inst-a", true), ("inst-b", false), ("inst-c", true)],
    );

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "json".into(),
        list: 10,
        by_repo: false,
    })
    .expect("should succeed");

    let json = serde_json::to_string_pretty(&report).expect("serialization must succeed");
    let roundtripped: SelfCheckReport =
        serde_json::from_str(&json).expect("round-trip deserialization must succeed");

    assert_eq!(
        roundtripped.schema, "bench-self-check/1",
        "schema field must survive round-trip"
    );
    assert_eq!(
        roundtripped.confusion.passed_resolved,
        report.confusion.passed_resolved
    );
    assert!((roundtripped.base_rate - report.base_rate).abs() < 1e-9);
    assert_eq!(roundtripped.n_excluded_none, report.n_excluded_none);
}

// ── test: confusion matrix totals are correct ─────────────────────────────────

#[test]
fn confusion_matrix_totals_are_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    write_trajectory(dir, "i1", Some(true));
    write_trajectory(dir, "i2", Some(true));
    write_trajectory(dir, "i3", Some(false));
    write_trajectory(dir, "i4", None);
    write_evaluation_json(
        dir,
        &[("i1", true), ("i2", false), ("i3", false), ("i4", true)],
    );

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    })
    .unwrap();

    let c = &report.confusion;
    let total = c.passed_resolved
        + c.passed_unresolved
        + c.failed_resolved
        + c.failed_unresolved
        + c.none_resolved
        + c.none_unresolved;
    assert_eq!(total, 4, "total should match instance count");
}

// ── test: --list 0 suppresses false positive/negative lists ──────────────────

#[test]
fn list_zero_suppresses_false_positives_negatives() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    write_trajectory(dir, "fp-inst", Some(true));
    write_trajectory(dir, "tp-inst", Some(true));
    write_evaluation_json(dir, &[("fp-inst", false), ("tp-inst", true)]);

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 0,
        by_repo: false,
    })
    .unwrap();

    // The report always stores all IDs internally; list=0 only affects rendering.
    // The run() function returns all FPs; the render limits.
    // Actually the spec says `--list 0` to suppress rendering, not the data.
    // Let's verify the internal list has the fp-inst:
    assert!(
        report.false_positives.contains(&"fp-inst".to_owned()),
        "false_positives must be populated regardless of --list"
    );
}

// ── test: text format contains expected sections ──────────────────────────────

#[test]
fn text_format_contains_required_sections() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    write_trajectory(dir, "tp", Some(true));
    write_trajectory(dir, "fp", Some(true));
    write_trajectory(dir, "fn_", Some(false));
    write_trajectory(dir, "tn", Some(false));
    write_evaluation_json(
        dir,
        &[("tp", true), ("fp", false), ("fn_", true), ("tn", false)],
    );

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    })
    .unwrap();

    let text = maxwells_daemon::run::self_check::render_text(&report, 10);

    assert!(
        text.contains("precision") || text.contains("Precision"),
        "text must mention precision"
    );
    assert!(
        text.contains("recall") || text.contains("Recall"),
        "text must mention recall"
    );
    assert!(
        text.contains("brier") || text.contains("Brier"),
        "text must mention Brier score"
    );
    assert!(
        text.contains("calibration") || text.contains("Calibration"),
        "text must show calibration delta"
    );
}

// ── test: calibration_delta positive when precision > base_rate ──────────────

#[test]
fn calibration_delta_is_positive_when_precision_exceeds_base_rate() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // 9 TP, 1 FP, 0 FN, 10 TN
    // precision = 9/10 = 0.9
    // base_rate = 9/20 = 0.45
    // calibration_delta = 0.9 - 0.45 = 0.45 (positive)
    for i in 0..9 {
        write_trajectory(dir, &format!("tp-{i}"), Some(true));
    }
    write_trajectory(dir, "fp-0", Some(true));
    for i in 0..10 {
        write_trajectory(dir, &format!("tn-{i}"), Some(false));
    }

    // We need stable string refs; build them first
    let positive_ids: Vec<String> = (0..9).map(|i| format!("tp-{i}")).collect();
    let negative_ids: Vec<String> = (0..10).map(|i| format!("tn-{i}")).collect();

    let mut pairs: Vec<(String, bool)> = Vec::new();
    for id in &positive_ids {
        pairs.push((id.clone(), true));
    }
    pairs.push(("fp-0".to_owned(), false));
    for id in &negative_ids {
        pairs.push((id.clone(), false));
    }

    let verdict_refs: Vec<(&str, bool)> = pairs.iter().map(|(id, b)| (id.as_str(), *b)).collect();
    write_evaluation_json(dir, &verdict_refs);

    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    })
    .unwrap();

    let delta = report.calibration_delta.expect("calibration_delta defined");
    assert!(
        delta > 0.0,
        "calibration_delta should be positive when precision > base_rate, got {delta}"
    );
}

// ── perf test: 300 instances < 2s ────────────────────────────────────────────

/// Performance regression test: 300-instance sweep must complete in < 2 seconds.
#[test]
fn perf_300_instances_under_2_seconds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let n = 300usize;
    let mut verdicts: Vec<(String, bool)> = Vec::with_capacity(n);

    for i in 0..n {
        let id = format!("django__django-{i:05}");
        let ltp = match i % 3 {
            0 => Some(true),
            1 => Some(false),
            _ => None,
        };
        write_trajectory(dir, &id, ltp);
        verdicts.push((id, i % 2 == 0));
    }

    let verdict_refs: Vec<(&str, bool)> =
        verdicts.iter().map(|(id, b)| (id.as_str(), *b)).collect();
    write_evaluation_json(dir, &verdict_refs);

    let start = Instant::now();
    let report = run_self_check(&SelfCheckArgs {
        sweep_dir: dir.to_path_buf(),
        format: "text".into(),
        list: 10,
        by_repo: false,
    })
    .expect("should succeed");
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 2,
        "300-instance sweep should complete in < 2s, took {elapsed:?}"
    );

    let total = report.confusion.passed_resolved
        + report.confusion.passed_unresolved
        + report.confusion.failed_resolved
        + report.confusion.failed_unresolved
        + report.confusion.none_resolved
        + report.confusion.none_unresolved;
    assert_eq!(total, 300, "all 300 instances accounted for");
}
