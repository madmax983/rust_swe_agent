//! Evaluator provenance: end-to-end tests covering issue #95.
//!
//! Test structure follows Red → Green → Refactor TDD.
//! All tests in this file are written before the feature exists (Red phase).

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;

use rust_swe_agent::run::compare::{CompareArgs, CompareFormat, EvaluatorProvenanceStatus};
use rust_swe_agent::run::evaluate::{
    EvalExitReason, EvaluationResults, EvaluatorProvenance, InstanceEvaluation, SbCliProvenance,
    SourceReportEntry,
};
use rust_swe_agent::run::inspect::SummaryReport;
use rust_swe_agent::run::swebench::{InstanceResult, SweepResults};
use rust_swe_agent::trajectory::outcome;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn minimal_instance_result(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(2),
        cost_usd: Some(0.01),
        prompt_tokens: Some(100),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(50),
        duration_secs: Some(3.0),
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: true,
        attempts: 1,
        retry_reasons: vec![],
        runs: 1,
        resolved_count: 1,
        pass_at_1: true,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
    }
}

fn minimal_evaluation_results(resolved: bool) -> EvaluationResults {
    EvaluationResults {
        instances: vec![InstanceEvaluation {
            instance_id: "task-a".into(),
            resolved,
            runs: 1,
            resolved_count: u32::from(resolved),
            pass_at_1: resolved,
            tests_passed: vec![],
            tests_failed: vec![],
            eval_exit_reason: if resolved {
                EvalExitReason::Resolved
            } else {
                EvalExitReason::Unresolved
            },
            eval_log_path: None,
            patch_stats: None,
        }],
        behavioral: Default::default(),
        breakdown: vec![],
        cost_attribution: vec![],
        model_mix_summary: vec![],
        latency_summary: None,
        provenance: None,
    }
}

fn write_results(dir: &Path, instances: Vec<InstanceResult>) {
    let sweep = SweepResults {
        total: instances.len(),
        sweep_status: rust_swe_agent::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: instances
            .iter()
            .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
            .count(),
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 0,
        estimated_cost_usd: 0.0,
        actual_cost_usd: None,
        actual_cost_source: None,
        baseline_cost_usd: None,
        baseline_cost_model: None,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: 0.0,
        filter_spec: Default::default(),
        manifest: None,
        instances,
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: BTreeMap::new(),
        systemic_halt_category: None,
        cost_limit_usd: None,
        retry_history: vec![],
    };
    let file = std::fs::File::create(dir.join("results.json")).unwrap();
    rust_swe_agent::artifact::to_writer_pretty(
        file,
        rust_swe_agent::artifact::ArtifactKind::SweepResults,
        &sweep,
    )
    .unwrap();
}

fn write_evaluation(dir: &Path, eval: &EvaluationResults) {
    let file = std::fs::File::create(dir.join("evaluation.json")).unwrap();
    rust_swe_agent::artifact::to_writer_pretty(
        file,
        rust_swe_agent::artifact::ArtifactKind::EvaluationResults,
        eval,
    )
    .unwrap();
}

fn compare_args(baseline: &Path, candidate: &Path) -> CompareArgs {
    CompareArgs {
        baseline: baseline.to_path_buf(),
        candidate: candidate.to_path_buf(),
        format: CompareFormat::Text,
        max_regressions: None,
        max_patch_size_regression_pct: None,
        breakdown: rust_swe_agent::run::evaluate::BreakdownSelection::none(),
        min_delta_pp: 0.0,
        cost_attribution: false,
        cost_attribution_min_delta_usd: 0.0,
        min_significance: None,
        regression_significance: None,
        allow_underpowered: false,
    }
}

// ---------------------------------------------------------------------------
// 1. EvaluatorProvenance struct construction and serialization
// ---------------------------------------------------------------------------

#[test]
fn evaluator_provenance_struct_exists_and_serializes() {
    let prov = EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: Some("test-run-001".into()),
        prediction_path: Some("/tmp/preds.jsonl".into()),
        prediction_sha256: Some("abc123".into()),
        eval_started_at: Some("2026-01-01T00:00:00Z".into()),
        eval_ended_at: Some("2026-01-01T00:01:00Z".into()),
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    };

    let json = serde_json::to_string(&prov).unwrap();
    assert!(json.contains("\"backend\":\"none\""));
    assert!(json.contains("swe-bench-m"));
    assert!(!json.contains("source_reports")); // empty vecs are skipped
}

// ---------------------------------------------------------------------------
// 2. EvaluationResults round-trips with provenance field present
// ---------------------------------------------------------------------------

#[test]
fn evaluation_results_roundtrip_with_provenance() {
    let prov = EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: None,
        dataset_split: None,
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    };

    let mut eval = minimal_evaluation_results(false);
    eval.provenance = Some(prov);

    let json = serde_json::to_string_pretty(&eval).unwrap();
    let parsed: EvaluationResults = serde_json::from_str(&json).unwrap();

    assert!(parsed.provenance.is_some());
    let parsed_prov = parsed.provenance.unwrap();
    assert_eq!(parsed_prov.backend, "none");
}

// ---------------------------------------------------------------------------
// 3. Legacy artifacts (no provenance field) still deserialize with None
// ---------------------------------------------------------------------------

#[test]
fn legacy_evaluation_results_deserializes_without_provenance() {
    // Simulate a legacy evaluation.json that has no `provenance` field
    let legacy_json = r#"{
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 1},
        "instances": [],
        "behavioral": {
            "tests_run_before_submit_rate": 0.0,
            "resolved_rate_when_tests_run": 0.0,
            "resolved_rate_when_tests_skipped": 0.0
        }
    }"#;

    let eval: EvaluationResults = serde_json::from_str(legacy_json).unwrap();
    assert!(
        eval.provenance.is_none(),
        "legacy artifacts without provenance should deserialize with provenance=None"
    );
}

// ---------------------------------------------------------------------------
// 4. SbCliProvenance struct records expected fields
// ---------------------------------------------------------------------------

#[test]
fn sb_cli_provenance_struct_fields() {
    let sb = SbCliProvenance {
        submit_command: Some("sb-cli submit swe-bench-m dev --predictions_path /tmp/p.jsonl --run_id test-run --output_dir /tmp/reports".into()),
        report_command: Some("sb-cli get-report swe-bench-m dev test-run --output_dir /tmp/reports --overwrite 1".into()),
        report_paths: vec!["/tmp/reports/swe-bench-m__dev__test-run.json".into()],
        report_hashes: vec!["deadbeef1234".into()],
        verify_submission: false,
        wait_for_evaluation: true,
        overwrite: true,
        timeout_per_instance_secs: 300,
        parallel: 4,
    };

    let json = serde_json::to_string(&sb).unwrap();
    assert!(json.contains("wait_for_evaluation"));
    assert!(json.contains("timeout_per_instance_secs"));
}

// ---------------------------------------------------------------------------
// 5. SourceReportEntry struct for rerun/pass@k mappings
// ---------------------------------------------------------------------------

#[test]
fn source_report_entry_struct_fields() {
    let entry = SourceReportEntry {
        run_index: 2,
        report_path: Some("/tmp/reports/run-2.json".into()),
        report_sha256: Some("f00dbabe".into()),
        instance_ids: vec!["task-a".into(), "task-b".into()],
    };

    let json = serde_json::to_string(&entry).unwrap();
    let parsed: SourceReportEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.run_index, 2);
    assert_eq!(parsed.instance_ids.len(), 2);
}

// ---------------------------------------------------------------------------
// 6. EvaluatorProvenanceStatus enum exists and serializes
// ---------------------------------------------------------------------------

#[test]
fn evaluator_provenance_status_enum() {
    let s = serde_json::to_string(&EvaluatorProvenanceStatus::Matching).unwrap();
    assert_eq!(s, "\"matching\"");

    let s = serde_json::to_string(&EvaluatorProvenanceStatus::Mismatched).unwrap();
    assert_eq!(s, "\"mismatched\"");

    let s = serde_json::to_string(&EvaluatorProvenanceStatus::Unavailable).unwrap();
    assert_eq!(s, "\"unavailable\"");
}

// ---------------------------------------------------------------------------
// 7. CompareReport has evaluator_provenance_status field
// ---------------------------------------------------------------------------

#[test]
fn compare_report_has_provenance_status_field() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    // No evaluation.json in either dir → provenance unavailable
    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Unavailable
    );
}

// ---------------------------------------------------------------------------
// 8. Matching provenance → Matching status, no warnings
// ---------------------------------------------------------------------------

#[test]
fn compare_matching_provenance_gives_matching_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let prov = EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: Some("run-001".into()),
        prediction_path: Some("/tmp/preds.jsonl".into()),
        prediction_sha256: Some("abc".into()),
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    };

    let mut eval_b = minimal_evaluation_results(true);
    eval_b.provenance = Some(prov.clone());
    write_evaluation(dir_b.path(), &eval_b);

    // Candidate has same backend/subset/split but different run_id and prediction_path
    let mut prov_c = prov;
    prov_c.run_id = Some("run-002".into()); // different run_id — should NOT trigger mismatch
    prov_c.prediction_path = Some("/tmp/other_preds.jsonl".into()); // different path — should NOT trigger mismatch
    prov_c.prediction_sha256 = Some("xyz".into());

    let mut eval_c = minimal_evaluation_results(true);
    eval_c.provenance = Some(prov_c);
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Matching,
        "run_id and prediction_path differences should not trigger mismatch"
    );
    assert!(
        report.evaluator_provenance_warnings.is_empty(),
        "no warnings expected when only run_id/prediction_path differ: {:?}",
        report.evaluator_provenance_warnings
    );
}

// ---------------------------------------------------------------------------
// 9. Backend mismatch → Mismatched + warning
// ---------------------------------------------------------------------------

#[test]
fn compare_backend_mismatch_gives_mismatched_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let mut eval_b = minimal_evaluation_results(false);
    eval_b.provenance = Some(EvaluatorProvenance {
        backend: "sb-cli".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_b.path(), &eval_b);

    let mut eval_c = minimal_evaluation_results(false);
    eval_c.provenance = Some(EvaluatorProvenance {
        backend: "none".into(), // DIFFERENT
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Mismatched
    );
    assert!(
        report
            .evaluator_provenance_warnings
            .iter()
            .any(|w| w.contains("backend")),
        "expected backend mismatch warning, got: {:?}",
        report.evaluator_provenance_warnings
    );
}

// ---------------------------------------------------------------------------
// 10. Dataset subset mismatch → Mismatched + warning
// ---------------------------------------------------------------------------

#[test]
fn compare_dataset_subset_mismatch_gives_mismatched_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let mut eval_b = minimal_evaluation_results(false);
    eval_b.provenance = Some(EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_b.path(), &eval_b);

    let mut eval_c = minimal_evaluation_results(false);
    eval_c.provenance = Some(EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench_lite".into()), // DIFFERENT
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Mismatched
    );
    assert!(
        report
            .evaluator_provenance_warnings
            .iter()
            .any(|w| w.contains("subset") || w.contains("dataset")),
        "expected dataset subset mismatch warning, got: {:?}",
        report.evaluator_provenance_warnings
    );
}

// ---------------------------------------------------------------------------
// 11. Dataset split mismatch → Mismatched + warning
// ---------------------------------------------------------------------------

#[test]
fn compare_dataset_split_mismatch_gives_mismatched_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let prov_with_split = |split: &str| EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some(split.into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    };

    let mut eval_b = minimal_evaluation_results(false);
    eval_b.provenance = Some(prov_with_split("dev"));
    write_evaluation(dir_b.path(), &eval_b);

    let mut eval_c = minimal_evaluation_results(false);
    eval_c.provenance = Some(prov_with_split("test")); // DIFFERENT
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Mismatched
    );
    assert!(
        report
            .evaluator_provenance_warnings
            .iter()
            .any(|w| w.contains("split")),
        "expected split mismatch warning, got: {:?}",
        report.evaluator_provenance_warnings
    );
}

// ---------------------------------------------------------------------------
// 12. One side missing provenance → Unavailable + warning
// ---------------------------------------------------------------------------

#[test]
fn compare_one_side_missing_provenance_gives_unavailable() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    // Baseline has provenance, candidate does not
    let mut eval_b = minimal_evaluation_results(false);
    eval_b.provenance = Some(EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_b.path(), &eval_b);

    let eval_c = minimal_evaluation_results(false); // no provenance
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Unavailable
    );
    assert!(
        !report.evaluator_provenance_warnings.is_empty(),
        "expected warning about missing provenance"
    );
}

// ---------------------------------------------------------------------------
// 13. EvaluatorProvenanceStatus appears in compare report JSON
// ---------------------------------------------------------------------------

#[test]
fn compare_report_json_includes_provenance_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    let json = report.to_json_pretty().unwrap();
    assert!(
        json.contains("evaluator_provenance_status"),
        "JSON should contain evaluator_provenance_status field"
    );
}

// ---------------------------------------------------------------------------
// 14. SummaryReport has evaluator_provenance field
// ---------------------------------------------------------------------------

#[test]
fn summary_report_has_evaluator_provenance_field() {
    // SummaryReport with provenance should serialize the field
    let prov = EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    };

    let report = SummaryReport {
        sweep_dir: "/tmp/sweep".into(),
        filter: "all".into(),
        manifest: None,
        evaluator_provenance: Some(prov),
        rows: vec![],
    };

    let json = serde_json::to_string(&report).unwrap();
    assert!(
        json.contains("evaluator_provenance"),
        "SummaryReport JSON should include evaluator_provenance"
    );
    assert!(json.contains("swe-bench-m"));
}

// ---------------------------------------------------------------------------
// 15. SummaryReport without provenance: field absent from JSON
// ---------------------------------------------------------------------------

#[test]
fn summary_report_without_provenance_omits_field() {
    let report = SummaryReport {
        sweep_dir: "/tmp/sweep".into(),
        filter: "all".into(),
        manifest: None,
        evaluator_provenance: None,
        rows: vec![],
    };

    let json = serde_json::to_string(&report).unwrap();
    assert!(
        !json.contains("evaluator_provenance"),
        "SummaryReport JSON should omit evaluator_provenance when None"
    );
}

// ---------------------------------------------------------------------------
// 16. Inspect loads provenance from evaluation.json and puts it on SummaryReport
// ---------------------------------------------------------------------------

#[test]
fn inspect_summary_loads_provenance_from_evaluation_json() {
    let dir = tempfile::tempdir().unwrap();

    // Write a traj so inspect can find instances
    let mut traj = rust_swe_agent::trajectory::Trajectory::new();
    traj.info.outcome = Some(outcome::SUBMITTED.into());
    std::fs::write(
        dir.path().join("task-a.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    // Write results.json
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);

    // Write evaluation.json with provenance
    let mut eval = minimal_evaluation_results(true);
    eval.provenance = Some(EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: Some("my-run-id".into()),
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir.path(), &eval);

    let args = rust_swe_agent::run::inspect::InspectArgs {
        sweep: dir.path().to_path_buf(),
        instance: None,
        filter: Some("resolved=true".into()),
        full: false,
        show_expected: false,
    };

    let output = rust_swe_agent::run::inspect::run(&args).unwrap();
    match output {
        rust_swe_agent::run::inspect::InspectOutput::Summary(summary) => {
            assert!(
                summary.evaluator_provenance.is_some(),
                "SummaryReport should carry provenance from evaluation.json"
            );
            let prov = summary.evaluator_provenance.unwrap();
            assert_eq!(prov.backend, "none");
            assert_eq!(prov.dataset_subset.as_deref(), Some("swe-bench-m"));
        }
        rust_swe_agent::run::inspect::InspectOutput::Instance(_) => {
            panic!("expected Summary output, got Instance")
        }
    }
}

// ---------------------------------------------------------------------------
// 17. Inspect renders provenance in text output
// ---------------------------------------------------------------------------

#[test]
fn inspect_text_renders_provenance_summary() {
    let prov = EvaluatorProvenance {
        backend: "sb-cli".into(),
        backend_version: Some("1.2.3".into()),
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: Some("run-42".into()),
        prediction_path: Some("/tmp/preds.jsonl".into()),
        prediction_sha256: None,
        eval_started_at: Some("2026-01-01T00:00:00Z".into()),
        eval_ended_at: Some("2026-01-01T00:10:00Z".into()),
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    };

    let report = SummaryReport {
        sweep_dir: "/tmp/sweep".into(),
        filter: "all".into(),
        manifest: None,
        evaluator_provenance: Some(prov),
        rows: vec![],
    };

    let output = rust_swe_agent::run::inspect::InspectOutput::Summary(Box::new(report));
    let text = rust_swe_agent::run::inspect::render_text(&output);

    assert!(
        text.contains("evaluator_provenance"),
        "text output should mention evaluator_provenance"
    );
    assert!(text.contains("sb-cli"), "text should show backend");
    assert!(text.contains("swe-bench-m"), "text should show subset");
}

// ---------------------------------------------------------------------------
// 18. Secret redaction: API keys in sb-cli commands are redacted
// ---------------------------------------------------------------------------

#[test]
fn sb_cli_provenance_secrets_are_redacted() {
    // Verify that redaction utilities can be applied to command strings.
    // We simulate the kind of redaction that should happen when recording
    // sb-cli command shapes that might contain API keys.
    let redactor = rust_swe_agent::redaction::Redactor::default_enabled();

    let cmd_with_secret = "sb-cli submit swe-bench-m dev --predictions_path /tmp/p.jsonl --api_key sk-ant-secret123456789ABCDEF";
    let outcome = redactor.redact_text(cmd_with_secret, "evaluator_provenance");

    assert!(outcome.redacted, "API key in command should be redacted");
    assert!(
        !outcome.text.contains("sk-ant-secret123456789ABCDEF"),
        "redacted command should not contain the raw API key"
    );
    // The non-secret parts should remain
    assert!(outcome.text.contains("sb-cli submit"));
    assert!(outcome.text.contains("swe-bench-m"));
}

// ---------------------------------------------------------------------------
// 19. EvaluatorProvenance backend_version can be None for backend=none
// ---------------------------------------------------------------------------

#[test]
fn evaluator_provenance_backend_version_optional() {
    let prov = EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None, // None is valid for backend=none
        dataset_subset: None,
        dataset_split: None,
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    };

    let json = serde_json::to_string(&prov).unwrap();
    let parsed: EvaluatorProvenance = serde_json::from_str(&json).unwrap();
    assert!(parsed.backend_version.is_none());
}

// ---------------------------------------------------------------------------
// 20. EvaluatorProvenanceStatus comparison: same backend_version matters
// ---------------------------------------------------------------------------

#[test]
fn compare_backend_version_mismatch_gives_mismatched_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let mut eval_b = minimal_evaluation_results(false);
    eval_b.provenance = Some(EvaluatorProvenance {
        backend: "sb-cli".into(),
        backend_version: Some("1.0.0".into()),
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_b.path(), &eval_b);

    let mut eval_c = minimal_evaluation_results(false);
    eval_c.provenance = Some(EvaluatorProvenance {
        backend: "sb-cli".into(),
        backend_version: Some("2.0.0".into()), // DIFFERENT VERSION
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Mismatched
    );
    assert!(
        report
            .evaluator_provenance_warnings
            .iter()
            .any(|w| w.contains("version")),
        "expected version mismatch warning, got: {:?}",
        report.evaluator_provenance_warnings
    );
}

// ---------------------------------------------------------------------------
// 21. No evaluation.json on both sides → Unavailable (not a hard error)
// ---------------------------------------------------------------------------

#[test]
fn compare_both_missing_evaluation_gives_unavailable() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);
    // No evaluation.json in either dir

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Unavailable,
        "both sides missing evaluation.json should give Unavailable status"
    );
}

// ---------------------------------------------------------------------------
// 22. run_id and prediction_path diffs do NOT cause Mismatched
// ---------------------------------------------------------------------------

#[test]
fn compare_run_id_diff_does_not_warn() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let base_prov = EvaluatorProvenance {
        backend: "none".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: Some("run-AAAA".into()),
        prediction_path: Some("/sweeps/run-a/predictions.jsonl".into()),
        prediction_sha256: Some("aaaa".into()),
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    };

    let mut eval_b = minimal_evaluation_results(true);
    eval_b.provenance = Some(base_prov.clone());
    write_evaluation(dir_b.path(), &eval_b);

    let mut prov_c = base_prov;
    prov_c.run_id = Some("run-BBBB".into());
    prov_c.prediction_path = Some("/sweeps/run-b/predictions.jsonl".into());
    prov_c.prediction_sha256 = Some("bbbb".into());

    let mut eval_c = minimal_evaluation_results(true);
    eval_c.provenance = Some(prov_c);
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Matching,
        "run_id and prediction_path diffs alone should not cause Mismatched"
    );
    assert!(
        report.evaluator_provenance_warnings.is_empty(),
        "no warnings expected for run_id/prediction_path diffs: {:?}",
        report.evaluator_provenance_warnings
    );
}

// ---------------------------------------------------------------------------
// 23. sb-cli timeout_per_instance_secs mismatch → Mismatched + warning
// ---------------------------------------------------------------------------

#[test]
fn compare_timeout_mismatch_gives_mismatched_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let sb_cli_prov = |timeout: u64| SbCliProvenance {
        submit_command: None,
        report_command: None,
        report_paths: vec![],
        report_hashes: vec![],
        verify_submission: false,
        wait_for_evaluation: true,
        overwrite: true,
        timeout_per_instance_secs: timeout,
        parallel: 4,
    };
    let base_prov = |timeout: u64| EvaluatorProvenance {
        backend: "sb-cli".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: Some(sb_cli_prov(timeout)),
        source_reports: vec![],
    };

    let mut eval_b = minimal_evaluation_results(false);
    eval_b.provenance = Some(base_prov(300));
    write_evaluation(dir_b.path(), &eval_b);

    let mut eval_c = minimal_evaluation_results(false);
    eval_c.provenance = Some(base_prov(600)); // different timeout
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Mismatched,
        "different timeout_per_instance_secs should give Mismatched"
    );
    assert!(
        report
            .evaluator_provenance_warnings
            .iter()
            .any(|w| w.contains("timeout")),
        "expected timeout mismatch warning, got: {:?}",
        report.evaluator_provenance_warnings
    );
}

// ---------------------------------------------------------------------------
// 24. sb-cli parallel mismatch → Mismatched + warning
// ---------------------------------------------------------------------------

#[test]
fn compare_parallel_mismatch_gives_mismatched_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let sb_cli_prov = |parallel: usize| SbCliProvenance {
        submit_command: None,
        report_command: None,
        report_paths: vec![],
        report_hashes: vec![],
        verify_submission: false,
        wait_for_evaluation: true,
        overwrite: true,
        timeout_per_instance_secs: 300,
        parallel,
    };
    let base_prov = |parallel: usize| EvaluatorProvenance {
        backend: "sb-cli".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: Some(sb_cli_prov(parallel)),
        source_reports: vec![],
    };

    let mut eval_b = minimal_evaluation_results(false);
    eval_b.provenance = Some(base_prov(4));
    write_evaluation(dir_b.path(), &eval_b);

    let mut eval_c = minimal_evaluation_results(false);
    eval_c.provenance = Some(base_prov(8)); // different parallel
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();

    assert_eq!(
        report.evaluator_provenance_status,
        EvaluatorProvenanceStatus::Mismatched,
        "different parallel should give Mismatched"
    );
    assert!(
        report
            .evaluator_provenance_warnings
            .iter()
            .any(|w| w.contains("parallel")),
        "expected parallel mismatch warning, got: {:?}",
        report.evaluator_provenance_warnings
    );
}

// ---------------------------------------------------------------------------
// 25. human_table() renders evaluator_provenance_status
// ---------------------------------------------------------------------------

#[test]
fn compare_human_table_shows_provenance_status() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);
    // No evaluation.json → Unavailable

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();
    let text = report.human_table();

    assert!(
        text.contains("Evaluator provenance"),
        "human_table() should show evaluator provenance status; got:\n{text}"
    );
    assert!(
        text.contains("unavailable"),
        "human_table() should show 'unavailable' when provenance missing; got:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// 26. human_table() shows provenance warnings when Mismatched
// ---------------------------------------------------------------------------

#[test]
fn compare_human_table_shows_provenance_warnings() {
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    write_results(dir_b.path(), vec![minimal_instance_result("task-a")]);
    write_results(dir_c.path(), vec![minimal_instance_result("task-a")]);

    let mut eval_b = minimal_evaluation_results(false);
    eval_b.provenance = Some(EvaluatorProvenance {
        backend: "sb-cli".into(),
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_b.path(), &eval_b);

    let mut eval_c = minimal_evaluation_results(false);
    eval_c.provenance = Some(EvaluatorProvenance {
        backend: "none".into(), // DIFFERENT
        backend_version: None,
        dataset_subset: Some("swe-bench-m".into()),
        dataset_split: Some("dev".into()),
        run_id: None,
        prediction_path: None,
        prediction_sha256: None,
        eval_started_at: None,
        eval_ended_at: None,
        report_source: None,
        sb_cli: None,
        source_reports: vec![],
    });
    write_evaluation(dir_c.path(), &eval_c);

    let report =
        rust_swe_agent::run::compare::compute(&compare_args(dir_b.path(), dir_c.path())).unwrap();
    let text = report.human_table();

    assert!(
        text.contains("mismatched"),
        "human_table() should show 'mismatched' status; got:\n{text}"
    );
    assert!(
        text.contains("backend"),
        "human_table() should show provenance warning text; got:\n{text}"
    );
}
