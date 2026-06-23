//! Tests for `agent stability` — issue #475.
//!
//! RED phase: these tests reference the not-yet-implemented public API and
//! will fail to compile until the implementation is in place.
//! GREEN phase: implement src/run/stability.rs and wire the CLI.
//! REFACTOR phase: clean up.

#![allow(clippy::unwrap_used, clippy::large_futures)]

use maxwells_daemon::artifact::{ArtifactKind, ArtifactSchemaVersion};
use maxwells_daemon::exit_code::ExitCode;
use maxwells_daemon::run::stability::{
    StabilityResults, StabilityRunDetail, compute_pass_at_k, compute_patch_identical_rate,
    compute_stats, pass_predicate_label, should_fail_under, validate_runs,
};

// ── Exit code unit tests ──────────────────────────────────────────────────────

#[test]
fn stability_gate_failure_exit_code_is_39() {
    assert_eq!(ExitCode::StabilityGateFailure.as_i32(), 39);
    assert_eq!(
        ExitCode::StabilityGateFailure.outcome_class(),
        "stability_gate_failure"
    );
}

// ── ArtifactKind tests ────────────────────────────────────────────────────────

#[test]
fn artifact_kind_stability_results_label() {
    assert_eq!(ArtifactKind::StabilityResults.label(), "stability_results");
}

// ── Struct field existence tests ──────────────────────────────────────────────

#[test]
fn stability_results_has_required_fields() {
    let results = StabilityResults {
        schema_version: ArtifactSchemaVersion::CURRENT,
        artifact_kind: ArtifactKind::StabilityResults,
        task: "Fix the null dereference".into(),
        runs: 3,
        pass_count: 2,
        pass_at_k: 2.0 / 3.0,
        patch_identical_rate: 0.5,
        pass_predicate: "outcome_submitted".into(),
        cost_usd_min: 0.001,
        cost_usd_max: 0.003,
        cost_usd_mean: 0.002,
        cost_usd_stddev: 0.001,
        step_count_min: 3.0,
        step_count_max: 5.0,
        step_count_mean: 4.0,
        step_count_stddev: 1.0,
        started_at: "2026-01-01T00:00:00Z".into(),
        finished_at: "2026-01-01T00:01:00Z".into(),
        runs_detail: vec![],
    };
    assert_eq!(results.runs, 3);
    assert_eq!(results.pass_count, 2);
    assert!((results.pass_at_k - 2.0 / 3.0).abs() < 1e-9);
    assert!((results.patch_identical_rate - 0.5).abs() < 1e-9);
    assert_eq!(results.pass_predicate, "outcome_submitted");
}

#[test]
fn stability_run_detail_has_required_fields() {
    let detail = StabilityRunDetail {
        run_number: 1,
        outcome: "submitted".into(),
        passed: true,
        cost_usd: Some(0.001),
        step_count: Some(5),
        skipped: false,
        skip_reason: None,
    };
    assert_eq!(detail.run_number, 1);
    assert_eq!(detail.outcome, "submitted");
    assert!(detail.passed);
    assert!(!detail.skipped);
    assert!(detail.skip_reason.is_none());
}

// ── validate_runs tests ───────────────────────────────────────────────────────

#[test]
fn validate_runs_zero_is_error() {
    assert!(validate_runs(0).is_err());
}

#[test]
fn validate_runs_eleven_is_error() {
    assert!(validate_runs(11).is_err());
}

#[test]
fn validate_runs_one_is_ok() {
    assert!(validate_runs(1).is_ok());
}

#[test]
fn validate_runs_ten_is_ok() {
    assert!(validate_runs(10).is_ok());
}

#[test]
fn validate_runs_error_message_mentions_range() {
    let err = validate_runs(0).unwrap_err();
    assert!(
        err.contains('1') && err.contains("10"),
        "error message should mention valid range, got: {err}"
    );
}

// ── should_fail_under tests ───────────────────────────────────────────────────

#[test]
fn fail_under_triggers_when_pass_at_k_below_threshold() {
    assert!(should_fail_under(0.5, Some(0.8)));
}

#[test]
fn fail_under_does_not_trigger_when_at_threshold() {
    assert!(!should_fail_under(0.8, Some(0.8)));
}

#[test]
fn fail_under_does_not_trigger_when_above_threshold() {
    assert!(!should_fail_under(0.9, Some(0.8)));
}

#[test]
fn fail_under_does_not_trigger_when_none() {
    assert!(!should_fail_under(0.0, None));
}

// ── pass_predicate_label tests ────────────────────────────────────────────────

#[test]
fn pass_predicate_without_verify_is_outcome_submitted() {
    assert_eq!(pass_predicate_label(&[]), "outcome_submitted");
}

#[test]
fn pass_predicate_with_verify_is_verify() {
    assert_eq!(
        pass_predicate_label(&["tests:cargo test".to_owned()]),
        "verify"
    );
}

// ── compute_pass_at_k tests ───────────────────────────────────────────────────

fn make_detail(passed: bool, skipped: bool) -> StabilityRunDetail {
    StabilityRunDetail {
        run_number: 1,
        outcome: if passed {
            "submitted".into()
        } else {
            "step_limit_reached".into()
        },
        passed,
        cost_usd: Some(0.001),
        step_count: Some(5),
        skipped,
        skip_reason: None,
    }
}

#[test]
fn compute_pass_at_k_all_pass() {
    let details = vec![
        make_detail(true, false),
        make_detail(true, false),
        make_detail(true, false),
    ];
    let (pass_count, pass_at_k) = compute_pass_at_k(&details);
    assert_eq!(pass_count, 3);
    assert!((pass_at_k - 1.0).abs() < 1e-9);
}

#[test]
fn compute_pass_at_k_none_pass() {
    let details = vec![make_detail(false, false), make_detail(false, false)];
    let (pass_count, pass_at_k) = compute_pass_at_k(&details);
    assert_eq!(pass_count, 0);
    assert!((pass_at_k - 0.0).abs() < 1e-9);
}

#[test]
fn compute_pass_at_k_partial() {
    let details = vec![
        make_detail(true, false),
        make_detail(false, false),
        make_detail(true, false),
    ];
    let (pass_count, pass_at_k) = compute_pass_at_k(&details);
    assert_eq!(pass_count, 2);
    assert!((pass_at_k - 2.0 / 3.0).abs() < 1e-9);
}

#[test]
fn compute_pass_at_k_skipped_excluded_from_denominator() {
    let details = vec![
        make_detail(true, false),
        make_detail(false, false),
        StabilityRunDetail {
            run_number: 3,
            outcome: "skipped_budget_exhausted".into(),
            passed: false,
            cost_usd: None,
            step_count: None,
            skipped: true,
            skip_reason: Some("cost_limit_usd".into()),
        },
    ];
    let (pass_count, pass_at_k) = compute_pass_at_k(&details);
    assert_eq!(pass_count, 1);
    // 1 pass out of 2 non-skipped runs
    assert!(
        (pass_at_k - 0.5).abs() < 1e-9,
        "expected 0.5, got {pass_at_k}"
    );
}

#[test]
fn compute_pass_at_k_all_skipped_is_zero() {
    let details = vec![StabilityRunDetail {
        run_number: 1,
        outcome: "skipped_budget_exhausted".into(),
        passed: false,
        cost_usd: None,
        step_count: None,
        skipped: true,
        skip_reason: Some("cost_limit_usd".into()),
    }];
    let (pass_count, pass_at_k) = compute_pass_at_k(&details);
    assert_eq!(pass_count, 0);
    assert!((pass_at_k - 0.0).abs() < 1e-9);
}

// ── compute_patch_identical_rate tests ───────────────────────────────────────

#[test]
fn patch_identical_rate_none_submitted_is_zero() {
    let patches: Vec<Option<String>> = vec![None, None];
    let rate = compute_patch_identical_rate(&patches);
    assert!((rate - 0.0).abs() < 1e-9);
}

#[test]
fn patch_identical_rate_all_identical() {
    let patches = vec![
        Some("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@\n-old\n+new".to_owned()),
        Some("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@\n-old\n+new".to_owned()),
        Some("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@\n-old\n+new".to_owned()),
    ];
    let rate = compute_patch_identical_rate(&patches);
    assert!((rate - 1.0).abs() < 1e-9);
}

#[test]
fn patch_identical_rate_modal_patch_wins() {
    let patches = vec![
        Some("patch-a".to_owned()),
        Some("patch-a".to_owned()),
        Some("patch-b".to_owned()),
    ];
    let rate = compute_patch_identical_rate(&patches);
    // modal is "patch-a" (count=2); rate = 2/3
    assert!(
        (rate - 2.0 / 3.0).abs() < 1e-9,
        "expected 0.6667, got {rate}"
    );
}

#[test]
fn patch_identical_rate_single_submitted() {
    let patches = vec![Some("patch-a".to_owned()), None];
    let rate = compute_patch_identical_rate(&patches);
    // only 1 submitted, modal matches itself → 1.0
    assert!((rate - 1.0).abs() < 1e-9);
}

// ── compute_stats tests ───────────────────────────────────────────────────────

#[test]
fn compute_stats_basic() {
    let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    let stats = compute_stats(&values);
    assert!((stats.min - 1.0).abs() < 1e-9);
    assert!((stats.max - 5.0).abs() < 1e-9);
    assert!((stats.mean - 3.0).abs() < 1e-9);
    // population stddev of [1,2,3,4,5] = sqrt(2) ≈ 1.4142
    let expected_stddev = 2.0_f64.sqrt();
    assert!(
        (stats.stddev - expected_stddev).abs() < 1e-6,
        "expected stddev {expected_stddev}, got {}",
        stats.stddev
    );
}

#[test]
fn compute_stats_single_value_stddev_is_zero() {
    let values = vec![5.0];
    let stats = compute_stats(&values);
    assert!((stats.min - 5.0).abs() < 1e-9);
    assert!((stats.max - 5.0).abs() < 1e-9);
    assert!((stats.mean - 5.0).abs() < 1e-9);
    assert!((stats.stddev - 0.0).abs() < 1e-9);
}

#[test]
fn compute_stats_empty_values_all_zero() {
    let values: Vec<f64> = vec![];
    let stats = compute_stats(&values);
    assert!((stats.min - 0.0).abs() < 1e-9);
    assert!((stats.max - 0.0).abs() < 1e-9);
    assert!((stats.mean - 0.0).abs() < 1e-9);
    assert!((stats.stddev - 0.0).abs() < 1e-9);
}

// ── summary_text tests ────────────────────────────────────────────────────────

#[test]
fn stability_results_summary_contains_pass_at_k() {
    let results = make_full_results();
    let text = results.summary_text();
    assert!(
        text.contains("pass_at_k") || text.contains("pass@k"),
        "summary should mention pass_at_k; got:\n{text}"
    );
}

#[test]
fn stability_results_summary_contains_pass_count_fraction() {
    let results = make_full_results();
    let text = results.summary_text();
    assert!(
        text.contains("2/3") || (text.contains('2') && text.contains('3')),
        "summary should show pass count; got:\n{text}"
    );
}

#[test]
fn stability_results_summary_contains_predicate() {
    let results = make_full_results();
    let text = results.summary_text();
    assert!(
        text.contains("outcome_submitted"),
        "summary should name the pass predicate; got:\n{text}"
    );
}

fn make_full_results() -> StabilityResults {
    StabilityResults {
        schema_version: ArtifactSchemaVersion::CURRENT,
        artifact_kind: ArtifactKind::StabilityResults,
        task: "Fix the null dereference".into(),
        runs: 3,
        pass_count: 2,
        pass_at_k: 2.0 / 3.0,
        patch_identical_rate: 0.5,
        pass_predicate: "outcome_submitted".into(),
        cost_usd_min: 0.001,
        cost_usd_max: 0.003,
        cost_usd_mean: 0.002,
        cost_usd_stddev: 0.001,
        step_count_min: 3.0,
        step_count_max: 5.0,
        step_count_mean: 4.0,
        step_count_stddev: 1.0,
        started_at: "2026-01-01T00:00:00Z".into(),
        finished_at: "2026-01-01T00:01:00Z".into(),
        runs_detail: vec![
            StabilityRunDetail {
                run_number: 1,
                outcome: "submitted".into(),
                passed: true,
                cost_usd: Some(0.001),
                step_count: Some(3),
                skipped: false,
                skip_reason: None,
            },
            StabilityRunDetail {
                run_number: 2,
                outcome: "step_limit_reached".into(),
                passed: false,
                cost_usd: Some(0.002),
                step_count: Some(5),
                skipped: false,
                skip_reason: None,
            },
            StabilityRunDetail {
                run_number: 3,
                outcome: "submitted".into(),
                passed: true,
                cost_usd: Some(0.003),
                step_count: Some(4),
                skipped: false,
                skip_reason: None,
            },
        ],
    }
}

// ── Integration tests (scripted-model path) ───────────────────────────────────

#[cfg(test)]
mod integration {
    use super::*;
    use maxwells_daemon::run::stability::{StabilityArgs, run};

    fn submit_response() -> String {
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfixed\n```".into()
    }

    fn make_args(tmpdir: &std::path::Path, runs: u32, responses: Vec<String>) -> StabilityArgs {
        let mut cfg = maxwells_daemon::config::Config::defaults().unwrap();
        cfg.root.model.name = "test-model".into();
        StabilityArgs {
            task: "Fix the bug".into(),
            runs,
            config: cfg,
            output_dir: tmpdir.to_owned(),
            stability_name: "test-stability".into(),
            verify: vec![],
            verify_timeout_secs: 60,
            fail_under: None,
            cost_limit_usd: None,
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            deterministic_responses: Some(responses),
            deterministic_usage_per_call: None,
            print_summary: false,
        }
    }

    #[tokio::test]
    async fn two_runs_scripted_submit_both_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let args = make_args(tmp.path(), 2, vec![submit_response(), submit_response()]);
        let exit_code = run(args).await.unwrap();
        assert_eq!(
            exit_code,
            ExitCode::Success,
            "expected success when all runs pass"
        );

        let result_path = tmp
            .path()
            .join("test-stability")
            .join("stability-results.json");
        assert!(
            result_path.exists(),
            "stability-results.json must be written"
        );

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();

        assert_eq!(json["runs"].as_u64(), Some(2));
        assert_eq!(json["pass_count"].as_u64(), Some(2));
        let pat = json["pass_at_k"].as_f64().unwrap();
        assert!(
            (pat - 1.0).abs() < 1e-6,
            "expected pass_at_k=1.0, got {pat}"
        );
        assert_eq!(json["pass_predicate"].as_str(), Some("outcome_submitted"));
        assert_eq!(
            json["artifact_kind"].as_str(),
            Some("stability_results"),
            "artifact_kind must be stability_results"
        );
        assert!(
            json.get("schema_version").is_some(),
            "schema_version must be present"
        );
    }

    #[tokio::test]
    async fn fail_under_gate_returns_non_zero_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = make_args(tmp.path(), 2, vec![submit_response(), submit_response()]);
        // Both runs submit → pass_at_k = 1.0; threshold of 1.1 is impossible to meet
        // so the gate fires and returns StabilityGateFailure.
        args.fail_under = Some(1.1);
        let exit_code = run(args).await.unwrap();
        assert_eq!(
            exit_code,
            ExitCode::StabilityGateFailure,
            "expected stability_gate_failure when pass_at_k < fail_under"
        );
    }

    #[tokio::test]
    async fn cost_limit_skips_remaining_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = maxwells_daemon::config::Config::defaults().unwrap();
        cfg.root.model.name = "test-model".into();
        // give it a tiny cost limit; even 1 run should exceed $0 if any cost is recorded
        // but with no deterministic_usage_per_call the cost will be 0 with scripted model
        // so we set cost_limit_usd to a very small positive number but since scripted model
        // reports 0 cost, the cap won't trigger. Instead let's test with per_task_budget_usd.
        // Actually let's just verify cost_limit_usd=0 skips runs 2+.
        let args = StabilityArgs {
            task: "Fix the bug".into(),
            runs: 3,
            config: cfg,
            output_dir: tmp.path().to_owned(),
            stability_name: "test-stability".into(),
            verify: vec![],
            verify_timeout_secs: 60,
            fail_under: None,
            cost_limit_usd: Some(0.0), // zero cap → all runs after first accumulation skipped
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            // enough responses for all 3 runs
            deterministic_responses: Some(vec![
                submit_response(),
                submit_response(),
                submit_response(),
            ]),
            deterministic_usage_per_call: Some(maxwells_daemon::model::ModelUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_usd: Some(0.01),
            }),
            print_summary: false,
        };

        let _exit_code = run(args).await.unwrap();

        let result_path = tmp
            .path()
            .join("test-stability")
            .join("stability-results.json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();

        // With cost_limit_usd=0, after the first run (cost=0.01) the cumulative cost
        // exceeds 0.0, so runs 2 and 3 should be skipped.
        let detail = json["runs_detail"].as_array().unwrap();
        assert_eq!(detail.len(), 3, "all 3 runs should appear in runs_detail");
        let skipped_count = detail
            .iter()
            .filter(|d| d["skipped"].as_bool().unwrap_or(false))
            .count();
        assert!(
            skipped_count >= 2,
            "at least 2 runs should be skipped when cost_limit_usd is exceeded; got {skipped_count} skipped"
        );
    }

    #[tokio::test]
    async fn path_traversal_in_stability_name_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = make_args(tmp.path(), 1, vec![submit_response()]);
        args.stability_name = "../escape".to_owned();
        assert!(
            run(args).await.is_err(),
            "stability name with '..' must be rejected before any I/O"
        );
        // No directory should have been created outside the tmp root.
        assert!(!tmp.path().parent().unwrap().join("escape").exists());
    }

    #[tokio::test]
    async fn task_unsuccessful_exit_code_when_run_fails_without_gate() {
        let tmp = tempfile::tempdir().unwrap();
        // Provide a non-submit response so the run ends without submitting.
        let mut args = make_args(
            tmp.path(),
            1,
            vec!["I am unable to solve this task.".to_owned()],
        );
        // Cap at 1 step so the agent stops immediately after the single response.
        args.step_limit = Some(1);
        let exit_code = run(args).await.unwrap();
        assert_eq!(
            exit_code,
            ExitCode::TaskUnsuccessful,
            "expected task_unsuccessful when a run fails with no --fail-under gate"
        );
    }

    #[tokio::test]
    async fn deterministic_aggregation_same_inputs_yield_same_stats() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();

        let args1 = make_args(tmp1.path(), 2, vec![submit_response(), submit_response()]);
        let args2 = make_args(tmp2.path(), 2, vec![submit_response(), submit_response()]);

        run(args1).await.unwrap();
        run(args2).await.unwrap();

        let read_results = |p: &std::path::Path| -> serde_json::Value {
            let path = p.join("test-stability").join("stability-results.json");
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };

        let r1 = read_results(tmp1.path());
        let r2 = read_results(tmp2.path());

        // Aggregate stats must be identical regardless of when the run happened
        assert_eq!(r1["runs"], r2["runs"], "runs must match");
        assert_eq!(r1["pass_count"], r2["pass_count"], "pass_count must match");
        assert_eq!(r1["pass_at_k"], r2["pass_at_k"], "pass_at_k must match");
        assert_eq!(
            r1["patch_identical_rate"], r2["patch_identical_rate"],
            "patch_identical_rate must match"
        );
        assert_eq!(
            r1["cost_usd_mean"], r2["cost_usd_mean"],
            "cost_usd_mean must match"
        );
        assert_eq!(
            r1["step_count_mean"], r2["step_count_mean"],
            "step_count_mean must match"
        );
        assert_eq!(
            r1["artifact_kind"], r2["artifact_kind"],
            "artifact_kind must match"
        );
        assert_eq!(
            r1["schema_version"], r2["schema_version"],
            "schema_version must match"
        );
    }
}
