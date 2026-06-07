//! Tests for the `docker-tests` offline evaluator backend (issue #497).
//!
//! Test structure: Red → Green → Refactor (TDD).
//! Tests are written before the feature exists and drive the implementation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use maxwells_daemon::run::evaluate::{
    BreakdownSelection, EvalExitReason, EvaluateArgs, EvaluateBackend,
};
use maxwells_daemon::run::swebench::{InstanceResult, SweepResults};
use maxwells_daemon::trajectory::outcome;

// ---------------------------------------------------------------------------
// Test helpers
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
        resolved_count: 0,
        pass_at_1: false,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
        trace_id: None,
    }
}

fn write_results(dir: &Path, instances: Vec<InstanceResult>) {
    let sweep = SweepResults {
        total: instances.len(),
        sweep_status: maxwells_daemon::run::swebench::SWEEP_STATUS_COMPLETED.into(),
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
        partial: 0,
        span_export_dropped: 0,
    };
    let file = std::fs::File::create(dir.join("results.json")).unwrap();
    maxwells_daemon::artifact::to_writer_pretty(
        file,
        maxwells_daemon::artifact::ArtifactKind::SweepResults,
        &sweep,
    )
    .unwrap();
}

fn default_evaluate_args(sweep_dir: &Path) -> EvaluateArgs {
    EvaluateArgs {
        sweep_dir: sweep_dir.to_path_buf(),
        dataset_path: None,
        backend: EvaluateBackend::DockerTests,
        timeout_per_instance_secs: 60,
        parallel: 1,
        sb_subset: "swe-bench-m".into(),
        sb_split: "dev".into(),
        run_id: None,
        breakdown: BreakdownSelection::none(),
        cost_attribution: false,
    }
}

// ---------------------------------------------------------------------------
// AC1: Backend variant and CLI parsing
// ---------------------------------------------------------------------------

/// AC1: `EvaluateBackend::DockerTests` variant exists.
#[test]
fn evaluate_backend_docker_tests_variant_exists() {
    let backend = EvaluateBackend::DockerTests;
    // If this compiles, the variant exists.
    assert_ne!(backend, EvaluateBackend::None);
    assert_ne!(backend, EvaluateBackend::SbCli);
    assert_ne!(backend, EvaluateBackend::Rehearsal);
}

/// AC1: CLI string `"docker-tests"` is accepted and maps to `DockerTests`.
#[test]
fn cli_backend_string_docker_tests_is_valid() {
    // Simulate what bench_evaluate() does when parsing --backend
    let backend_str = "docker-tests";
    let backend = match backend_str {
        "sb-cli" => EvaluateBackend::SbCli,
        "none" => EvaluateBackend::None,
        "rehearsal" => EvaluateBackend::Rehearsal,
        "docker-tests" => EvaluateBackend::DockerTests,
        other => panic!("unknown backend: {other}"),
    };
    assert_eq!(backend, EvaluateBackend::DockerTests);
}

// ---------------------------------------------------------------------------
// AC2: Verdict schema matches sb-cli output
// ---------------------------------------------------------------------------

/// AC2: `EvalExitReason::SkippedNoImage` variant exists and serializes correctly.
#[test]
fn eval_exit_reason_skipped_no_image_exists_and_serializes() {
    let reason = EvalExitReason::SkippedNoImage;
    let json = serde_json::to_string(&reason).unwrap();
    assert_eq!(json, r#""skipped_no_image""#);
}

/// AC2: `EvalExitReason::SkippedNoImage` deserializes from `"skipped_no_image"`.
#[test]
fn eval_exit_reason_skipped_no_image_deserializes() {
    let reason: EvalExitReason = serde_json::from_str(r#""skipped_no_image""#).unwrap();
    assert_eq!(reason, EvalExitReason::SkippedNoImage);
}

/// AC2: Evaluation results with docker-tests backend have `backend = "docker-tests"` in provenance.
#[test]
fn docker_tests_backend_sets_provenance_backend_name() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);

    // Write a minimal patch file so it's not skipped
    let patch_dir = dir.path().join("task-a");
    std::fs::create_dir_all(&patch_dir).unwrap();
    std::fs::write(patch_dir.join("run-1.patch"), "# no-op patch").unwrap();

    let args = default_evaluate_args(dir.path());
    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    let prov = eval.provenance.expect("provenance should be present");
    assert_eq!(prov.backend, "docker-tests");
}

/// AC2: docker-tests provenance emits eval timestamps (started_at, ended_at).
#[test]
fn docker_tests_provenance_has_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![]);

    let args = default_evaluate_args(dir.path());
    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    let prov = eval.provenance.expect("provenance should be present");
    assert!(
        prov.eval_started_at.is_some(),
        "eval_started_at should be set"
    );
    assert!(prov.eval_ended_at.is_some(), "eval_ended_at should be set");
}

// ---------------------------------------------------------------------------
// AC3: Instances without canonical image → `SkippedNoImage` (never unresolved)
// ---------------------------------------------------------------------------

/// AC3: Instance submitted but dataset not provided → SkippedNoImage (cannot run tests without image info).
#[test]
fn instance_without_dataset_image_info_is_skipped_not_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);

    // Write a non-empty patch file so instance is not SkippedNoPatch
    let patch_dir = dir.path().join("task-a");
    std::fs::create_dir_all(&patch_dir).unwrap();
    std::fs::write(
        patch_dir.join("run-1.patch"),
        "diff --git a/foo.py b/foo.py\n--- a/foo.py\n+++ b/foo.py\n@@ -1 +1 @@\n-x\n+y\n",
    )
    .unwrap();

    // No dataset → no image info available
    let mut args = default_evaluate_args(dir.path());
    args.dataset_path = None;

    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    assert_eq!(eval.instances.len(), 1);
    let inst = &eval.instances[0];
    assert_eq!(inst.instance_id, "task-a");
    assert!(
        !inst.resolved,
        "instance without image should not be resolved"
    );
    assert_eq!(
        inst.eval_exit_reason,
        EvalExitReason::SkippedNoImage,
        "instance without image info should be SkippedNoImage, not unresolved"
    );
}

///// AC3 (revised): Instance whose dataset entry has no explicit `image` field gets the
/// conventional SWE-bench image name derived and Docker is attempted.
/// With no Docker daemon (or image) available the result is EvalError, which is more
/// informative than SkippedNoImage for operators who pre-pull standard images.
#[test]
fn instance_with_dataset_but_no_image_field_derives_conventional_name() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);

    // Write a non-empty patch
    let patch_dir = dir.path().join("task-a");
    std::fs::create_dir_all(&patch_dir).unwrap();
    std::fs::write(
        patch_dir.join("run-1.patch"),
        "diff --git a/foo.py b/foo.py\n--- a/foo.py\n+++ b/foo.py\n",
    )
    .unwrap();

    // Write dataset without image field
    let dataset_path = dir.path().join("dataset.jsonl");
    std::fs::write(
        &dataset_path,
        r#"{"instance_id":"task-a","FAIL_TO_PASS":["test_foo"],"PASS_TO_PASS":[]}"#,
    )
    .unwrap();

    let mut args = default_evaluate_args(dir.path());
    args.dataset_path = Some(dataset_path);

    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    assert_eq!(eval.instances.len(), 1);
    let inst = &eval.instances[0];
    // The backend derives swebench/sweb.eval.x86_64.task-a and attempts docker run.
    // Without a real Docker daemon or a pre-pulled image the result is EvalError.
    assert!(
        inst.eval_exit_reason == EvalExitReason::EvalError
            || inst.eval_exit_reason == EvalExitReason::Unresolved
            || inst.eval_exit_reason == EvalExitReason::Resolved,
        "expected EvalError/Unresolved/Resolved when derived image is attempted, got {:?}",
        inst.eval_exit_reason
    );
    assert_ne!(
        inst.eval_exit_reason,
        EvalExitReason::SkippedNoImage,
        "should not be SkippedNoImage when a conventional image name can be derived"
    );
}

/// AC3: Instance with no patch file → SkippedNoPatch (existing behavior preserved).
#[test]
fn instance_with_no_patch_gives_skipped_no_patch() {
    let dir = tempfile::tempdir().unwrap();
    // Instance marked submitted but patch_present=false
    let mut inst = minimal_instance_result("task-a");
    inst.patch_present = false;
    inst.non_empty_patch = false;
    write_results(dir.path(), vec![inst]);

    let args = default_evaluate_args(dir.path());
    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    assert_eq!(eval.instances.len(), 1);
    let result = &eval.instances[0];
    assert_eq!(result.eval_exit_reason, EvalExitReason::SkippedNoPatch);
}

// ---------------------------------------------------------------------------
// AC4: Per-instance timeout semantics match existing --eval-timeout-per-instance-secs
// ---------------------------------------------------------------------------

/// AC4: EvaluateArgs.timeout_per_instance_secs is used by docker-tests backend.
/// (Structural check — full timeout integration requires actual docker).
#[test]
fn docker_tests_args_carry_timeout_field() {
    let args = EvaluateArgs {
        sweep_dir: std::path::PathBuf::from("/tmp/sweep"),
        dataset_path: None,
        backend: EvaluateBackend::DockerTests,
        timeout_per_instance_secs: 120,
        parallel: 2,
        sb_subset: "swe-bench-m".into(),
        sb_split: "dev".into(),
        run_id: None,
        breakdown: BreakdownSelection::none(),
        cost_attribution: false,
    };
    assert_eq!(args.timeout_per_instance_secs, 120);
    assert!(matches!(args.backend, EvaluateBackend::DockerTests));
}

/// AC4: docker-tests provenance records timeout_per_instance_secs.
#[test]
fn docker_tests_provenance_records_timeout() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![]);

    let mut args = default_evaluate_args(dir.path());
    args.timeout_per_instance_secs = 300;

    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();
    let prov = eval.provenance.expect("provenance present");

    // The backend provenance should record the timeout
    let docker_prov = prov
        .docker_tests
        .expect("docker_tests provenance should be present");
    assert_eq!(docker_prov.timeout_per_instance_secs, 300);
}

// ---------------------------------------------------------------------------
// AC5: evaluator-selftest can run against docker-tests backend
// ---------------------------------------------------------------------------

/// AC5: evaluator_selftest::run() accepts "docker-tests" as a backend string without panicking.
#[test]
fn evaluator_selftest_accepts_docker_tests_backend() {
    let dir = tempfile::tempdir().unwrap();
    let dataset_path = dir.path().join("dataset.jsonl");
    std::fs::write(
        &dataset_path,
        "{\"instance_id\":\"task-a\",\"patch\":\"diff --git a/f b/f\\n\",\"FAIL_TO_PASS\":[],\"PASS_TO_PASS\":[]}\n",
    )
    .unwrap();

    let args = maxwells_daemon::run::evaluator_selftest::SelftestArgs {
        dataset_path,
        output_dir: dir.path().to_path_buf(),
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        format: "text".into(),
        backend: "docker-tests".into(),
        sb_subset: "swe-bench-m".into(),
        sb_split: "dev".into(),
        timeout_per_instance: 60,
        parallel: 1,
    };

    // Should not panic on "unknown backend" assertion
    let result = maxwells_daemon::run::evaluator_selftest::run(args);
    // With docker-tests backend and no image field, instances should be skipped (not errored)
    assert_eq!(result.output.evaluator_backend, "docker-tests");
}

// ---------------------------------------------------------------------------
// AC: Schema compatibility — InstanceEvaluation with SkippedNoImage round-trips
// ---------------------------------------------------------------------------

/// Schema: InstanceEvaluation with SkippedNoImage serializes to JSON and back.
#[test]
fn instance_evaluation_skipped_no_image_round_trips() {
    use maxwells_daemon::run::evaluate::InstanceEvaluation;

    let inst = InstanceEvaluation {
        instance_id: "task-a".into(),
        resolved: false,
        runs: 1,
        resolved_count: 0,
        pass_at_1: false,
        tests_passed: vec![],
        tests_failed: vec![],
        eval_exit_reason: EvalExitReason::SkippedNoImage,
        eval_log_path: None,
        patch_stats: None,
        patch_error_log: None,
    };

    let json = serde_json::to_string(&inst).unwrap();
    assert!(json.contains("skipped_no_image"));

    let parsed: InstanceEvaluation = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.eval_exit_reason, EvalExitReason::SkippedNoImage);
}

/// Schema: EvaluationResults with docker-tests provenance includes docker_tests block.
#[test]
fn evaluation_results_json_includes_docker_tests_provenance() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![]);

    let args = default_evaluate_args(dir.path());
    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    let json = serde_json::to_string(&eval).unwrap();
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();

    // Provenance backend field should be "docker-tests"
    assert_eq!(
        val["provenance"]["backend"].as_str().unwrap_or(""),
        "docker-tests"
    );
}

// ---------------------------------------------------------------------------
// AC: Downstream commands consume docker-tests evaluation unchanged
// ---------------------------------------------------------------------------

/// downstream: bench inspect loads evaluation.json produced by docker-tests.
#[test]
fn inspect_loads_docker_tests_evaluation_json() {
    use maxwells_daemon::run::inspect::{InspectArgs, InspectOutput};

    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);

    // Write a minimal trajectory
    let mut traj = maxwells_daemon::trajectory::Trajectory::new();
    traj.info.outcome = Some(outcome::SUBMITTED.into());
    std::fs::write(
        dir.path().join("task-a.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    // Write evaluation.json with docker-tests backend provenance
    let eval_json = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 1},
        "instances": [{
            "instance_id": "task-a",
            "resolved": false,
            "runs": 1,
            "resolved_count": 0,
            "pass_at_1": false,
            "tests_passed": [],
            "tests_failed": [],
            "eval_exit_reason": "skipped_no_image",
        }],
        "behavioral": {
            "tests_run_before_submit_rate": 0.0,
            "resolved_rate_when_tests_run": 0.0,
            "resolved_rate_when_tests_skipped": 0.0
        },
        "provenance": {
            "backend": "docker-tests",
        }
    });
    std::fs::write(
        dir.path().join("evaluation.json"),
        serde_json::to_string_pretty(&eval_json).unwrap(),
    )
    .unwrap();

    let args = InspectArgs {
        sweep: dir.path().to_path_buf(),
        instance: None,
        filter: Some("resolved=false".into()),
        full: false,
        show_expected: false,
        flake_report: None,
    };

    let output = maxwells_daemon::run::inspect::run(&args).unwrap();
    match output {
        InspectOutput::Summary(summary) => {
            let prov = summary
                .evaluator_provenance
                .expect("provenance should load");
            assert_eq!(prov.backend, "docker-tests");
        }
        InspectOutput::Instance(_) => panic!("expected Summary"),
    }
}

// ---------------------------------------------------------------------------
// AC: evaluate::run() writes evaluation.json on disk with docker-tests backend
// ---------------------------------------------------------------------------

/// Disk write: evaluation.json is written when docker-tests backend runs.
#[test]
fn docker_tests_backend_writes_evaluation_json() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);

    let args = default_evaluate_args(dir.path());
    maxwells_daemon::run::evaluate::run(&args).unwrap();

    let eval_path = dir.path().join("evaluation.json");
    assert!(eval_path.exists(), "evaluation.json should be written");

    let content = std::fs::read_to_string(eval_path).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        val["provenance"]["backend"].as_str().unwrap_or(""),
        "docker-tests"
    );
}
