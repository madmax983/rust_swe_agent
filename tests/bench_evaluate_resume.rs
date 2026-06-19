//! Tests for resumable `bench evaluate` (issue #530).
//!
//! Test structure: Red → Green → Refactor (TDD).
//! Tests are written before the feature exists and drive the implementation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use maxwells_daemon::run::evaluate::{
    BreakdownSelection, EvaluateArgs, EvaluateBackend, EvaluationResults,
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
        backend: EvaluateBackend::None,
        timeout_per_instance_secs: 60,
        parallel: 1,
        sb_subset: "swe-bench-m".into(),
        sb_split: "dev".into(),
        run_id: None,
        breakdown: BreakdownSelection::none(),
        cost_attribution: false,
        force: false,
    }
}

/// Write a minimal patch file for an instance so it isn't skipped as SkippedNoPatch.
fn write_patch(dir: &Path, instance_id: &str, content: &str) {
    let patch_dir = dir.join(instance_id);
    std::fs::create_dir_all(&patch_dir).unwrap();
    std::fs::write(patch_dir.join("run-1.patch"), content).unwrap();
}

/// Read the persisted evaluation.json back from disk.
fn read_evaluation_json(dir: &Path) -> EvaluationResults {
    let content = std::fs::read_to_string(dir.join("evaluation.json")).unwrap();
    serde_json::from_str(&content).unwrap()
}

// ---------------------------------------------------------------------------
// AC1: Fully-scored sweep does zero new evaluations
// ---------------------------------------------------------------------------

/// AC1: Re-running evaluate on a fully-scored sweep reports 0 evaluated, N reused.
#[test]
fn resume_fully_scored_does_zero_work() {
    let dir = tempfile::tempdir().unwrap();
    let instances = vec![
        minimal_instance_result("task-a"),
        minimal_instance_result("task-b"),
        minimal_instance_result("task-c"),
    ];
    write_results(dir.path(), instances);
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");
    write_patch(dir.path(), "task-c", "--- patch c ---");

    // First run: seeds evaluation.json
    let args = default_evaluate_args(dir.path());
    maxwells_daemon::run::evaluate::run(&args).unwrap();

    // Second run: nothing has changed
    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    let rs = eval.reuse_summary.expect("reuse_summary must be present");
    assert_eq!(
        rs.evaluated, 0,
        "zero new evaluations on fully-scored sweep"
    );
    assert_eq!(rs.reused, 3, "all 3 instances must be reused");
    assert_eq!(rs.invalidated, 0, "no invalidations");
}

// ---------------------------------------------------------------------------
// AC2: Only new instances evaluated; prior verdicts preserved unchanged
// ---------------------------------------------------------------------------

/// AC2: When new instances are added, only they are evaluated; prior verdicts preserved.
#[test]
fn new_instance_only_evaluated_prior_preserved() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
        ],
    );
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");

    // First run: score A and B
    let args = default_evaluate_args(dir.path());
    let first = maxwells_daemon::run::evaluate::run(&args).unwrap();
    let first_a = first
        .instances
        .iter()
        .find(|i| i.instance_id == "task-a")
        .unwrap()
        .clone();
    let first_b = first
        .instances
        .iter()
        .find(|i| i.instance_id == "task-b")
        .unwrap()
        .clone();

    // Add new instance C to the sweep
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
            minimal_instance_result("task-c"),
        ],
    );
    write_patch(dir.path(), "task-c", "--- patch c ---");

    let second = maxwells_daemon::run::evaluate::run(&args).unwrap();

    let rs = second.reuse_summary.expect("reuse_summary must be present");
    assert_eq!(rs.evaluated, 1, "only new instance C evaluated");
    assert_eq!(rs.reused, 2, "A and B reused");

    // Prior verdicts preserved unchanged (including patch_stats and fingerprint)
    let second_a = second
        .instances
        .iter()
        .find(|i| i.instance_id == "task-a")
        .unwrap();
    let second_b = second
        .instances
        .iter()
        .find(|i| i.instance_id == "task-b")
        .unwrap();

    assert_eq!(
        second_a.eval_exit_reason, first_a.eval_exit_reason,
        "A exit reason unchanged"
    );
    assert_eq!(second_a.resolved, first_a.resolved, "A resolved unchanged");
    assert_eq!(
        second_a.submission_fingerprint, first_a.submission_fingerprint,
        "A fingerprint unchanged"
    );
    assert_eq!(
        second_b.eval_exit_reason, first_b.eval_exit_reason,
        "B exit reason unchanged"
    );
}

// ---------------------------------------------------------------------------
// AC3: Interrupted evaluation resumes from first unscored instance
// ---------------------------------------------------------------------------

/// AC3: A partial evaluation.json (A, B scored but not C) resumes at C only.
#[test]
fn interrupted_eval_resumes_from_first_unscored() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
            minimal_instance_result("task-c"),
        ],
    );
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");
    write_patch(dir.path(), "task-c", "--- patch c ---");

    // Simulate interrupted evaluation: only A and B were scored before crash.
    // Run once for A+B only, then manually trim evaluation.json to 2 instances.
    let args_ab = EvaluateArgs {
        sweep_dir: dir.path().to_path_buf(),
        dataset_path: None,
        backend: EvaluateBackend::None,
        timeout_per_instance_secs: 60,
        parallel: 1,
        sb_subset: "swe-bench-m".into(),
        sb_split: "dev".into(),
        run_id: None,
        breakdown: BreakdownSelection::none(),
        cost_attribution: false,
        force: false,
    };

    // Score all three first, then write a partial evaluation.json (only A and B).
    let full = maxwells_daemon::run::evaluate::run(&args_ab).unwrap();
    let partial_instances: Vec<_> = full
        .instances
        .iter()
        .filter(|i| i.instance_id != "task-c")
        .cloned()
        .collect();
    let partial_eval = EvaluationResults {
        instances: partial_instances,
        reuse_summary: None,
        ..Default::default()
    };
    let content = serde_json::to_string_pretty(&partial_eval).unwrap();
    std::fs::write(dir.path().join("evaluation.json"), content).unwrap();

    // Now re-run: should only evaluate C (A and B are cached)
    let eval = maxwells_daemon::run::evaluate::run(&args_ab).unwrap();

    let rs = eval.reuse_summary.expect("reuse_summary must be present");
    assert_eq!(rs.reused, 2, "A and B reused from partial evaluation.json");
    assert_eq!(rs.evaluated, 1, "only C evaluated");
    assert_eq!(
        eval.instances.len(),
        3,
        "all three instances present in output"
    );
}

// ---------------------------------------------------------------------------
// AC4: --force re-evaluates all instances
// ---------------------------------------------------------------------------

/// AC4: With --force, all instances are re-evaluated regardless of cached verdicts.
#[test]
fn force_reevaluates_all() {
    let dir = tempfile::tempdir().unwrap();
    let instances = vec![
        minimal_instance_result("task-a"),
        minimal_instance_result("task-b"),
        minimal_instance_result("task-c"),
    ];
    write_results(dir.path(), instances);
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");
    write_patch(dir.path(), "task-c", "--- patch c ---");

    // Seed
    let args = default_evaluate_args(dir.path());
    maxwells_daemon::run::evaluate::run(&args).unwrap();

    // Re-run with --force
    let forced = EvaluateArgs {
        force: true,
        ..default_evaluate_args(dir.path())
    };
    let eval = maxwells_daemon::run::evaluate::run(&forced).unwrap();

    let rs = eval.reuse_summary.expect("reuse_summary must be present");
    assert_eq!(rs.evaluated, 3, "all 3 re-evaluated under --force");
    assert_eq!(rs.reused, 0, "nothing reused under --force");
    assert_eq!(
        rs.invalidated, 0,
        "invalidated is 0 under --force (force ≠ invalidation)"
    );
}

// ---------------------------------------------------------------------------
// AC5: Changed patch artifact invalidates cached verdict
// ---------------------------------------------------------------------------

/// AC5: When the submission patch changes, cached verdict is invalidated and re-evaluated.
#[test]
fn changed_patch_is_invalidated() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
        ],
    );
    write_patch(dir.path(), "task-a", "--- original patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");

    // First run
    let args = default_evaluate_args(dir.path());
    let first = maxwells_daemon::run::evaluate::run(&args).unwrap();
    let first_a_fp = first
        .instances
        .iter()
        .find(|i| i.instance_id == "task-a")
        .unwrap()
        .submission_fingerprint
        .clone();

    // Modify A's patch
    write_patch(dir.path(), "task-a", "--- CHANGED patch a ---");

    // Second run
    let second = maxwells_daemon::run::evaluate::run(&args).unwrap();

    let rs = second.reuse_summary.expect("reuse_summary must be present");
    assert_eq!(
        rs.invalidated, 1,
        "A must be invalidated after patch change"
    );
    assert_eq!(rs.evaluated, 1, "A must be re-evaluated");
    assert_eq!(rs.reused, 1, "B still reused");

    let second_a = second
        .instances
        .iter()
        .find(|i| i.instance_id == "task-a")
        .unwrap();
    assert_ne!(
        second_a.submission_fingerprint, first_a_fp,
        "fingerprint must change when patch changes"
    );
}

// ---------------------------------------------------------------------------
// AC6: Reuse counts in JSON artifact match in-memory counts
// ---------------------------------------------------------------------------

/// AC6: The reuse_summary field is persisted in evaluation.json for downstream tooling.
#[test]
fn counts_in_json_and_match() {
    let dir = tempfile::tempdir().unwrap();
    // Set up A+B scored, C new, and A invalidated (changed patch) for a mixed run.
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
        ],
    );
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");

    let args = default_evaluate_args(dir.path());
    maxwells_daemon::run::evaluate::run(&args).unwrap();

    // Change A's patch and add C
    write_patch(dir.path(), "task-a", "--- changed patch a ---");
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
            minimal_instance_result("task-c"),
        ],
    );
    write_patch(dir.path(), "task-c", "--- patch c ---");

    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();
    let in_memory = eval.reuse_summary.expect("reuse_summary present in memory");

    // Read from disk and compare
    let from_disk = read_evaluation_json(dir.path());
    let on_disk = from_disk
        .reuse_summary
        .expect("reuse_summary present in evaluation.json");

    assert_eq!(
        in_memory, on_disk,
        "in-memory and on-disk reuse_summary must match"
    );
    assert_eq!(on_disk.reused, 1, "B reused");
    assert_eq!(on_disk.invalidated, 1, "A invalidated");
    assert_eq!(
        on_disk.evaluated, 2,
        "A (invalidated) + C (new) = 2 evaluated"
    );
}

// ---------------------------------------------------------------------------
// AC7: Resume is the default — no flag required
// ---------------------------------------------------------------------------

/// AC7: Resume is default (force:false is enough); also first run with no prior produces
/// evaluated=N, reused=0, invalidated=0.
#[test]
fn resume_is_default_no_flag() {
    let dir = tempfile::tempdir().unwrap();
    let instances = vec![
        minimal_instance_result("task-a"),
        minimal_instance_result("task-b"),
    ];
    write_results(dir.path(), instances);
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");

    // First run — no prior evaluation.json
    let args = default_evaluate_args(dir.path());
    let first = maxwells_daemon::run::evaluate::run(&args).unwrap();
    let rs = first
        .reuse_summary
        .expect("reuse_summary present on first run");
    assert_eq!(rs.evaluated, 2, "first run: all instances evaluated");
    assert_eq!(rs.reused, 0, "first run: nothing reused");
    assert_eq!(rs.invalidated, 0, "first run: no invalidations");

    // Second run — same inputs, no --force flag
    let second = maxwells_daemon::run::evaluate::run(&args).unwrap();
    let rs2 = second
        .reuse_summary
        .expect("reuse_summary present on second run");
    assert_eq!(rs2.evaluated, 0, "second run: 0 evaluated (default resume)");
    assert_eq!(rs2.reused, 2, "second run: 2 reused");
    assert_eq!(rs2.invalidated, 0);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

/// Edge: No prior evaluation.json → all instances treated as new.
#[test]
fn prior_eval_absent_treats_all_as_new() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
        ],
    );
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");

    // Explicitly ensure no prior file
    assert!(!dir.path().join("evaluation.json").exists());

    let args = default_evaluate_args(dir.path());
    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    let rs = eval.reuse_summary.expect("reuse_summary present");
    assert_eq!(rs.evaluated, 2, "all new when no prior file");
    assert_eq!(rs.reused, 0);
    assert_eq!(rs.invalidated, 0);
}

/// Edge: Instance removed from sweep is not carried forward from prior evaluation.json.
#[test]
fn instance_removed_from_sweep_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
        ],
    );
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");

    let args = default_evaluate_args(dir.path());
    maxwells_daemon::run::evaluate::run(&args).unwrap();

    // Remove B from the sweep
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);

    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();

    assert_eq!(eval.instances.len(), 1, "only A in output; B dropped");
    assert_eq!(eval.instances[0].instance_id, "task-a");
}

// ---------------------------------------------------------------------------
// Review hardening (PR #809): fully-cached resume must not enter the backend
// ---------------------------------------------------------------------------

/// A fully-cached resume must not depend on the dataset file, since no instance
/// needs evaluation. Backends eagerly load the dataset up front, so the empty
/// plan must short-circuit before backend dispatch.
#[test]
fn fully_cached_resume_skips_backend_dataset_load() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            minimal_instance_result("task-a"),
            minimal_instance_result("task-b"),
        ],
    );
    write_patch(dir.path(), "task-a", "--- patch a ---");
    write_patch(dir.path(), "task-b", "--- patch b ---");

    // First run seeds evaluation.json (DockerTests backend, dataset present).
    let mut args = default_evaluate_args(dir.path());
    args.backend = EvaluateBackend::DockerTests;
    let dataset = dir.path().join("dataset.jsonl");
    std::fs::write(
        &dataset,
        "{\"instance_id\":\"task-a\"}\n{\"instance_id\":\"task-b\"}\n",
    )
    .unwrap();
    args.dataset_path = Some(dataset.clone());
    maxwells_daemon::run::evaluate::run(&args).unwrap();

    // Dataset disappears, but every instance is cached → resume must still succeed.
    std::fs::remove_file(&dataset).unwrap();
    let eval = maxwells_daemon::run::evaluate::run(&args)
        .expect("fully-cached resume must not require the dataset file");

    let rs = eval.reuse_summary.expect("reuse_summary present");
    assert_eq!(rs.evaluated, 0, "no instance evaluated on full cache hit");
    assert_eq!(rs.reused, 2, "both instances reused");
}

// ---------------------------------------------------------------------------
// Review hardening (PR #809): run-count change invalidates a cached verdict
// ---------------------------------------------------------------------------

/// When the sweep's run count grows (e.g. a pass@k rerun), a single-run cached
/// verdict must not be reused even though the run-1 patch is unchanged.
#[test]
fn increased_run_count_invalidates_cached_verdict() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);
    write_patch(dir.path(), "task-a", "--- patch a ---");

    let args = default_evaluate_args(dir.path());
    maxwells_daemon::run::evaluate::run(&args).unwrap();

    // Bump the sweep to a 2-run pass@k sweep; run-1 patch unchanged.
    let mut multi = minimal_instance_result("task-a");
    multi.runs = 2;
    write_results(dir.path(), vec![multi]);

    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();
    let rs = eval.reuse_summary.expect("reuse_summary present");
    assert_eq!(rs.invalidated, 1, "run-count change must invalidate");
    assert_eq!(rs.evaluated, 1, "instance must be re-evaluated");
    assert_eq!(rs.reused, 0);
}

// ---------------------------------------------------------------------------
// Review hardening (PR #809): submission-state change invalidates reuse
// ---------------------------------------------------------------------------

/// If results.json now marks a previously-submitted instance as non-submitted
/// (while a stale patch file remains on disk), the cached non-skip verdict must
/// be invalidated rather than carried forward.
#[test]
fn submission_state_change_invalidates_cached_verdict() {
    use maxwells_daemon::run::evaluate::EvalExitReason;

    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![minimal_instance_result("task-a")]);
    write_patch(dir.path(), "task-a", "--- patch a ---");

    let args = default_evaluate_args(dir.path());
    let first = maxwells_daemon::run::evaluate::run(&args).unwrap();
    assert_eq!(
        first.instances[0].eval_exit_reason,
        EvalExitReason::EvalError
    );

    // Instance is no longer submitted, but the patch file is still on disk.
    let mut unsubmitted = minimal_instance_result("task-a");
    unsubmitted.outcome = Some("errored".into());
    unsubmitted.patch_present = false;
    unsubmitted.non_empty_patch = false;
    write_results(dir.path(), vec![unsubmitted]);

    let eval = maxwells_daemon::run::evaluate::run(&args).unwrap();
    let rs = eval.reuse_summary.expect("reuse_summary present");
    assert_eq!(rs.invalidated, 1, "submission-state change must invalidate");
    assert_eq!(rs.reused, 0);
    assert_eq!(
        eval.instances[0].eval_exit_reason,
        EvalExitReason::SkippedNoPatch,
        "re-evaluation must reflect the new non-submitted state"
    );
}
