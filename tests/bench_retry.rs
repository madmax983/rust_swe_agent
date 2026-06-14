//! TDD tests for `bench retry` — the post-completion operator-driven retry path.
//!
//! Red phase  : tests written before implementation, compile stubs added to pass
//!              the borrow checker while runtime assertions fail.
//! Green phase: minimal implementation makes all tests pass.
//! Refactor   : clean-up committed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use maxwells_daemon::artifact::ArtifactKind;
use maxwells_daemon::run::retry::{
    OverrideDelta, RetryHistoryEntry, RetrySelection, archive_trajectories,
    detect_harness_mismatch_with_sha, merge_retry_results, resolve_selection,
    restore_missing_trajectories, save_pre_retry_backup,
};
use maxwells_daemon::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest, HarnessManifest, InstanceResult, ModelManifest,
    PromptTemplateManifest, ProvenanceManifest, RuntimeManifest, SWEEP_STATUS_COMPLETED,
    SweepResults,
};
use maxwells_daemon::trajectory::FailureCategory;

mod support;
use support::binary_path;

// ─── test helpers ─────────────────────────────────────────────────────────────

fn make_instance(id: &str, out: &str, cat: Option<FailureCategory>) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: out.into(),
        outcome: Some(out.into()),
        failure_category: cat,
        steps: Some(1),
        cost_usd: Some(0.0),
        prompt_tokens: None,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        completion_tokens: None,
        duration_secs: None,
        error: None,
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
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

fn base_sweep(instances: Vec<InstanceResult>) -> SweepResults {
    let submitted = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some("submitted"))
        .count();
    let errored = instances
        .iter()
        .filter(|r| r.outcome.as_deref() != Some("submitted"))
        .count();
    SweepResults {
        total: instances.len(),
        sweep_status: SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: instances.len(),
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted,
        submitted_with_tests: 0,
        skipped: 0,
        errored,
        failures_by_category: Default::default(),
        budget_halted: 0,
        with_patch: submitted,
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
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: Default::default(),
        systemic_halt_category: None,
        retry_history: vec![],
        partial: 0,
        span_export_dropped: 0,
    }
}

fn write_results(dir: &Path, results: &SweepResults) {
    let json =
        maxwells_daemon::artifact::to_string_pretty(ArtifactKind::SweepResults, results).unwrap();
    std::fs::write(dir.join("results.json"), json).unwrap();
}

fn make_retry_entry(retry_id: &str, count: usize) -> RetryHistoryEntry {
    RetryHistoryEntry {
        retry_id: retry_id.into(),
        timestamp_utc: "2026-05-14T00:00:00Z".into(),
        selection: RetrySelection::default(),
        override_delta: OverrideDelta::default(),
        count,
        harness_mismatch: false,
        pre_submitted: 1,
        pre_errored: 1,
        pre_resolved_count: 1,
        post_submitted: 2,
        post_errored: 0,
        post_resolved_count: 2,
    }
}

// ─── Unit: resolve_selection ──────────────────────────────────────────────────

#[test]
fn resolve_selection_refuse_no_selector() {
    let instances = vec![make_instance(
        "a",
        "error",
        Some(FailureCategory::StepLimit),
    )];
    let err = resolve_selection(&instances, None, None, None, None, false)
        .expect_err("must fail with no selector");
    assert!(err.to_string().contains("at least one of"), "{err}");
}

#[test]
fn resolve_selection_by_failure_category() {
    let instances = vec![
        make_instance("a", "error", Some(FailureCategory::StepLimit)),
        make_instance("b", "error", Some(FailureCategory::PatchEmpty)),
        make_instance("c", "submitted", None),
    ];
    let selected = resolve_selection(
        &instances,
        Some(&[FailureCategory::StepLimit]),
        None,
        None,
        None,
        false,
    )
    .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].instance_id, "a");
}

#[test]
fn resolve_selection_by_outcome() {
    let instances = vec![
        make_instance("a", "step_limit_reached", Some(FailureCategory::StepLimit)),
        make_instance("b", "error", Some(FailureCategory::PatchEmpty)),
        make_instance("c", "submitted", None),
    ];
    let selected = resolve_selection(
        &instances,
        None,
        Some(&["step_limit_reached".into(), "error".into()]),
        None,
        None,
        false,
    )
    .unwrap();
    assert_eq!(selected.len(), 2);
}

#[test]
fn resolve_selection_by_instance_ids() {
    let instances = vec![
        make_instance("a", "error", Some(FailureCategory::StepLimit)),
        make_instance("b", "error", Some(FailureCategory::PatchEmpty)),
        make_instance("c", "error", Some(FailureCategory::StepLimit)),
    ];
    let selected = resolve_selection(
        &instances,
        None,
        None,
        Some(&["a".into(), "c".into()]),
        None,
        false,
    )
    .unwrap();
    assert_eq!(selected.len(), 2);
    let ids: Vec<&str> = selected.iter().map(|r| r.instance_id.as_str()).collect();
    assert!(ids.contains(&"a"), "{ids:?}");
    assert!(ids.contains(&"c"), "{ids:?}");
}

#[test]
fn resolve_selection_limit_caps_after_filters() {
    let instances: Vec<_> = (0..10)
        .map(|i| make_instance(&format!("{i}"), "error", Some(FailureCategory::StepLimit)))
        .collect();
    let selected = resolve_selection(
        &instances,
        Some(&[FailureCategory::StepLimit]),
        None,
        None,
        Some(3),
        false,
    )
    .unwrap();
    assert_eq!(selected.len(), 3);
}

#[test]
fn resolve_selection_composition_order() {
    // failure_category=StepLimit AND outcome=step_limit_reached → a, d
    // then instance_ids intersect with {a,d} → both kept
    // then limit → only 1
    let instances = vec![
        make_instance("a", "step_limit_reached", Some(FailureCategory::StepLimit)),
        make_instance("b", "error", Some(FailureCategory::StepLimit)),
        make_instance("c", "step_limit_reached", Some(FailureCategory::PatchEmpty)),
        make_instance("d", "step_limit_reached", Some(FailureCategory::StepLimit)),
    ];
    let selected = resolve_selection(
        &instances,
        Some(&[FailureCategory::StepLimit]),
        Some(&["step_limit_reached".into()]),
        Some(&["a".into(), "d".into()]),
        Some(1),
        false,
    )
    .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].instance_id, "a");
}

#[test]
fn resolve_selection_refuse_submitted_without_allow_flag() {
    let instances = vec![make_instance("a", "submitted", None)];
    let err = resolve_selection(
        &instances,
        None,
        Some(&["submitted".into()]),
        None,
        None,
        false,
    )
    .expect_err("must refuse submitted without allow flag");
    assert!(err.to_string().contains("allow-resolved-retry"), "{err}");
}

#[test]
fn resolve_selection_submitted_allowed_with_flag() {
    let instances = vec![make_instance("a", "submitted", None)];
    let selected = resolve_selection(
        &instances,
        None,
        Some(&["submitted".into()]),
        None,
        None,
        true,
    )
    .unwrap();
    assert_eq!(selected.len(), 1);
}

// ─── Unit: merge_retry_results ────────────────────────────────────────────────

#[test]
fn merge_preserves_unselected_rows() {
    let original = base_sweep(vec![
        make_instance("a", "error", Some(FailureCategory::StepLimit)),
        make_instance("b", "submitted", None),
    ]);
    let retry_output = base_sweep(vec![make_instance("a", "submitted", None)]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged = merge_retry_results(
        &original,
        &retry_output,
        make_retry_entry("r1", 1),
        &selected_ids,
    );

    assert_eq!(merged.instances.len(), 2);
    let b = merged
        .instances
        .iter()
        .find(|r| r.instance_id == "b")
        .unwrap();
    assert_eq!(b.outcome.as_deref(), Some("submitted"));
    assert!(
        b.retry_id.is_none(),
        "unselected row must not gain retry_id"
    );
}

#[test]
fn merge_replaces_selected_row() {
    let original = base_sweep(vec![make_instance(
        "a",
        "error",
        Some(FailureCategory::StepLimit),
    )]);
    let mut new_a = make_instance("a", "submitted", None);
    new_a.steps = Some(99);
    let retry_output = base_sweep(vec![new_a]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged = merge_retry_results(
        &original,
        &retry_output,
        make_retry_entry("r1", 1),
        &selected_ids,
    );

    let a = merged
        .instances
        .iter()
        .find(|r| r.instance_id == "a")
        .unwrap();
    assert_eq!(a.outcome.as_deref(), Some("submitted"));
    assert_eq!(a.steps, Some(99));
}

#[test]
fn merge_sets_retry_id_on_retried_instance() {
    let original = base_sweep(vec![make_instance(
        "a",
        "error",
        Some(FailureCategory::StepLimit),
    )]);
    let retry_output = base_sweep(vec![make_instance("a", "submitted", None)]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged = merge_retry_results(
        &original,
        &retry_output,
        make_retry_entry("test-retry-id-abc", 1),
        &selected_ids,
    );

    let a = merged
        .instances
        .iter()
        .find(|r| r.instance_id == "a")
        .unwrap();
    assert_eq!(a.retry_id.as_deref(), Some("test-retry-id-abc"));
}

#[test]
fn merge_sets_previous_failure_category() {
    let original = base_sweep(vec![make_instance(
        "a",
        "error",
        Some(FailureCategory::StepLimit),
    )]);
    let retry_output = base_sweep(vec![make_instance("a", "submitted", None)]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged = merge_retry_results(
        &original,
        &retry_output,
        make_retry_entry("rid", 1),
        &selected_ids,
    );

    let a = merged
        .instances
        .iter()
        .find(|r| r.instance_id == "a")
        .unwrap();
    assert_eq!(
        a.previous_failure_category,
        Some(FailureCategory::StepLimit)
    );
}

#[test]
fn merge_appends_retry_history_entry() {
    let original = base_sweep(vec![make_instance(
        "a",
        "error",
        Some(FailureCategory::StepLimit),
    )]);
    let retry_output = base_sweep(vec![make_instance("a", "submitted", None)]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged = merge_retry_results(
        &original,
        &retry_output,
        make_retry_entry("rid-42", 1),
        &selected_ids,
    );

    assert_eq!(merged.retry_history.len(), 1);
    assert_eq!(merged.retry_history[0].retry_id, "rid-42");
    assert_eq!(merged.retry_history[0].count, 1);
}

#[test]
fn merge_preserves_prior_retry_history_entries() {
    let mut original = base_sweep(vec![
        make_instance("a", "error", Some(FailureCategory::StepLimit)),
        make_instance("b", "error", Some(FailureCategory::StepLimit)),
    ]);
    original
        .retry_history
        .push(make_retry_entry("old-entry", 1));
    let retry_output = base_sweep(vec![make_instance("a", "submitted", None)]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged = merge_retry_results(
        &original,
        &retry_output,
        make_retry_entry("new-entry", 1),
        &selected_ids,
    );

    assert_eq!(merged.retry_history.len(), 2);
    assert_eq!(merged.retry_history[0].retry_id, "old-entry");
    assert_eq!(merged.retry_history[1].retry_id, "new-entry");
}

// ─── Schema tests ─────────────────────────────────────────────────────────────

#[test]
fn schema_version_is_1_11() {
    assert_eq!(
        maxwells_daemon::artifact::ArtifactSchemaVersion::CURRENT,
        maxwells_daemon::artifact::ArtifactSchemaVersion::new(1, 11),
        "schema bumped to 1.11 for parse_retries in TrajectoryInfo (issue #517)"
    );
}

#[test]
fn sweep_results_serializes_retry_history_when_present() {
    let mut results = base_sweep(vec![]);
    results.retry_history.push(make_retry_entry("test-id", 0));

    let json =
        maxwells_daemon::artifact::to_string_pretty(ArtifactKind::SweepResults, &results).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        value.get("retry_history").is_some(),
        "retry_history must serialize when present"
    );
    assert!(value["retry_history"].is_array());
}

#[test]
fn sweep_results_omits_retry_history_when_empty() {
    let results = base_sweep(vec![]);
    let json =
        maxwells_daemon::artifact::to_string_pretty(ArtifactKind::SweepResults, &results).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        value.get("retry_history").is_none(),
        "empty retry_history must be omitted to stay additive: {value}"
    );
}

#[test]
fn instance_result_serializes_retry_fields_when_set() {
    let mut instance = make_instance("a", "submitted", None);
    instance.retry_id = Some("rid-123".into());
    instance.previous_failure_category = Some(FailureCategory::StepLimit);

    let json = serde_json::to_string(&instance).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["retry_id"], "rid-123");
    assert!(value.get("previous_failure_category").is_some());
}

#[test]
fn instance_result_omits_retry_fields_when_none() {
    let instance = make_instance("a", "submitted", None);
    let json = serde_json::to_string(&instance).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        value.get("retry_id").is_none(),
        "retry_id must be omitted when None"
    );
    assert!(
        value.get("previous_failure_category").is_none(),
        "previous_failure_category must be omitted when None"
    );
}

// ─── CLI integration tests ────────────────────────────────────────────────────

fn write_fixture_sweep(dir: &Path, instances: &[(&str, &str, Option<&str>)]) {
    let list: Vec<InstanceResult> = instances
        .iter()
        .map(|(id, out, cat)| {
            let fc = cat
                .and_then(|c| serde_json::from_value::<FailureCategory>(serde_json::json!(c)).ok());
            make_instance(id, out, fc)
        })
        .collect();
    write_results(dir, &base_sweep(list));
}

#[test]
fn cli_refuse_without_selector() {
    let sweep = tempfile::tempdir().unwrap();
    write_fixture_sweep(sweep.path(), &[("a", "error", Some("step_limit"))]);

    let out = Command::new(binary_path())
        .args(["bench", "retry", "--sweep", sweep.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!out.status.success(), "must exit non-zero when no selector");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Either clap error (unknown subcommand/flag) or our own validation error
    assert!(
        !stderr.is_empty() || !String::from_utf8_lossy(&out.stdout).is_empty(),
        "must emit some error output"
    );
}

#[test]
fn cli_dry_run_no_yes_exits_nonzero() {
    let sweep = tempfile::tempdir().unwrap();
    write_fixture_sweep(sweep.path(), &[("instance-a", "error", Some("step_limit"))]);

    let out = Command::new(binary_path())
        .args([
            "bench",
            "retry",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--failure-category",
            "step_limit",
        ])
        .output()
        .unwrap();

    // Without --yes, must exit non-zero (dry-run preview)
    assert!(
        !out.status.success(),
        "must exit non-zero in dry-run without --yes; stdout={}; stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // And the preview must mention the instance id
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("instance-a"),
        "dry-run preview must mention the instance id: {combined}"
    );
}

#[test]
fn cli_round_trip_compare_reads_retry_history() {
    let candidate = tempfile::tempdir().unwrap();
    let mut results = base_sweep(vec![{
        let mut r = make_instance("a", "submitted", None);
        r.retry_id = Some("test-retry-id".into());
        r.previous_failure_category = Some(FailureCategory::StepLimit);
        r
    }]);
    results
        .retry_history
        .push(make_retry_entry("test-retry-id", 1));
    write_results(candidate.path(), &results);

    let baseline = tempfile::tempdir().unwrap();
    write_results(
        baseline.path(),
        &base_sweep(vec![make_instance(
            "a",
            "error",
            Some(FailureCategory::StepLimit),
        )]),
    );

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

    assert!(
        out.status.success(),
        "bench compare must succeed on retry-amended results.json; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_round_trip_inspect_retried_instance() {
    let sweep = tempfile::tempdir().unwrap();
    let mut results = base_sweep(vec![{
        let mut r = make_instance("task-a", "submitted", None);
        r.retry_id = Some("some-retry-id".into());
        r.previous_failure_category = Some(FailureCategory::StepLimit);
        r
    }]);
    results
        .retry_history
        .push(make_retry_entry("some-retry-id", 1));
    write_results(sweep.path(), &results);

    // Minimal trajectory for the retried instance
    let traj_dir = sweep.path().join("task-a");
    std::fs::create_dir_all(&traj_dir).unwrap();
    std::fs::write(
        traj_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 7},
            "trajectory_format": "mini-swe-agent-1.1",
            "info": {
                "model_name": "test",
                "outcome": "submitted",
                "exit_reason": "submitted",
                "total_cost_usd": 0.0
            },
            "messages": []
        }))
        .unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "task-a",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "bench inspect must succeed on retried instance; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_round_trip_evaluate_reads_retry_history() {
    let sweep = tempfile::tempdir().unwrap();
    let mut results = base_sweep(vec![{
        let mut r = make_instance("task-a", "submitted", None);
        r.retry_id = Some("rid".into());
        r.previous_failure_category = Some(FailureCategory::StepLimit);
        r
    }]);
    results.retry_history.push(make_retry_entry("rid", 1));
    write_results(sweep.path(), &results);

    let out = Command::new(binary_path())
        .args([
            "bench",
            "evaluate",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--backend",
            "none",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "bench evaluate must succeed on retry-amended results.json; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ─── helpers for manifest-based tests ────────────────────────────────────────

fn make_manifest_with_sha(sha: &str) -> ProvenanceManifest {
    ProvenanceManifest {
        purpose: None,
        harness: HarnessManifest {
            name: "test".into(),
            version: "0.1.0".into(),
            git_sha: Some(sha.into()),
            git_dirty: None,
            git_resolution: "exact".into(),
        },
        dataset: DatasetManifest::default(),
        prompt_template: PromptTemplateManifest {
            source: "builtin".into(),
            path: None,
            sha256: "abc".into(),
        },
        config: ConfigManifest {
            resolved: "{}".into(),
            overlay_paths: vec![],
        },
        model: ModelManifest {
            name: "test-model".into(),
            backend: "anthropic".into(),
            backend_version: None,
            base_url: None,
        },
        runtime: RuntimeManifest {
            started_at_utc: "2026-01-01T00:00:00Z".into(),
            finished_at_utc: None,
            host_os: "linux".into(),
            resume_mode: false,
            rust_version: None,
        },
        cli: CliManifest { argv: vec![] },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
    }
}

fn sweep_with_sha(sha: &str) -> SweepResults {
    let mut r = base_sweep(vec![]);
    r.manifest = Some(make_manifest_with_sha(sha));
    r
}

// ─── Unit: detect_harness_mismatch_with_sha ──────────────────────────────────

#[test]
fn detect_harness_mismatch_when_shas_differ() {
    let results = sweep_with_sha("aaaaaa");
    assert!(
        detect_harness_mismatch_with_sha(&results, Some("bbbbbb")),
        "must detect mismatch when manifest sha != current sha"
    );
}

#[test]
fn detect_harness_mismatch_false_when_shas_match() {
    let results = sweep_with_sha("deadbeef");
    assert!(
        !detect_harness_mismatch_with_sha(&results, Some("deadbeef")),
        "must not flag mismatch when shas are identical"
    );
}

#[test]
fn detect_harness_mismatch_false_when_no_manifest() {
    let results = base_sweep(vec![]);
    assert!(
        !detect_harness_mismatch_with_sha(&results, Some("some-sha")),
        "no manifest → no mismatch"
    );
}

#[test]
fn detect_harness_mismatch_false_when_no_current_sha() {
    let results = sweep_with_sha("abc123");
    assert!(
        !detect_harness_mismatch_with_sha(&results, None),
        "current_sha=None means git unavailable → treat as no mismatch"
    );
}

// ─── Unit: archive_trajectories (flat path) ──────────────────────────────────

#[test]
fn archive_uses_flat_path() {
    let sweep = tempfile::tempdir().unwrap();
    let instance_id = "django__django-001";

    // Create trajectory in nested layout (sweep_dir/{id}/run-1.traj.json)
    let traj_dir = sweep.path().join(instance_id);
    std::fs::create_dir_all(&traj_dir).unwrap();
    std::fs::write(traj_dir.join("run-1.traj.json"), b"{}").unwrap();

    let inst = make_instance(instance_id, "error", Some(FailureCategory::StepLimit));
    let selected = vec![&inst];

    archive_trajectories(sweep.path(), &selected, "retry-flat-test").unwrap();

    // Archive must be at flat path: .retry/{retry_id}/{instance_id}.traj.json
    let flat = sweep
        .path()
        .join(".retry")
        .join("retry-flat-test")
        .join(format!("{instance_id}.traj.json"));
    assert!(
        flat.exists(),
        "archived trajectory must be at flat path {}",
        flat.display()
    );

    // Nested path inside archive must NOT exist
    let nested = sweep
        .path()
        .join(".retry")
        .join("retry-flat-test")
        .join(instance_id)
        .join("run-1.traj.json");
    assert!(
        !nested.exists(),
        "nested archive path must not be created: {}",
        nested.display()
    );
}

// ─── Unit: save_pre_retry_backup ─────────────────────────────────────────────

#[test]
fn save_pre_retry_backup_writes_json() {
    let sweep = tempfile::tempdir().unwrap();
    let results = base_sweep(vec![make_instance(
        "a",
        "error",
        Some(FailureCategory::StepLimit),
    )]);

    save_pre_retry_backup(sweep.path(), &results, "backup-test-id").unwrap();

    let path = sweep
        .path()
        .join(".retry")
        .join("backup-test-id")
        .join("pre-retry.json");
    assert!(
        path.exists(),
        "pre-retry.json must be written to archive dir"
    );

    let json = std::fs::read_to_string(&path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        v.get("instances").is_some(),
        "pre-retry.json must contain full sweep data"
    );
}

// ─── Unit: restore_missing_trajectories ──────────────────────────────────────

#[test]
fn restore_missing_trajectories_restores_when_absent() {
    let sweep = tempfile::tempdir().unwrap();
    let id = "instance-absent";
    let inst = make_instance(id, "error", Some(FailureCategory::StepLimit));
    let selected = vec![&inst];

    // Write archive copy
    let archive_dir = sweep.path().join(".retry").join("r1");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::write(
        archive_dir.join(format!("{id}.traj.json")),
        b"{\"info\":{\"exit_reason\":\"submitted\"}}",
    )
    .unwrap();

    // live path does not exist
    restore_missing_trajectories(sweep.path(), &selected, "r1").unwrap();

    let live = sweep.path().join(id).join("run-1.traj.json");
    assert!(
        live.exists(),
        "missing trajectory must be restored from archive"
    );
}

#[test]
fn restore_missing_trajectories_keeps_valid_trajectory() {
    let sweep = tempfile::tempdir().unwrap();
    let id = "instance-valid";
    let inst = make_instance(id, "submitted", None);
    let selected = vec![&inst];

    // Write a valid live trajectory
    let live_dir = sweep.path().join(id);
    std::fs::create_dir_all(&live_dir).unwrap();
    let valid_content = b"{\"info\":{\"exit_reason\":\"submitted\",\"outcome\":\"submitted\"}}";
    std::fs::write(live_dir.join("run-1.traj.json"), valid_content).unwrap();

    // Write a different archive copy
    let archive_dir = sweep.path().join(".retry").join("r1");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::write(
        archive_dir.join(format!("{id}.traj.json")),
        b"{\"info\":{\"exit_reason\":\"error\"}}",
    )
    .unwrap();

    restore_missing_trajectories(sweep.path(), &selected, "r1").unwrap();

    // Live trajectory must be unchanged
    let content = std::fs::read(sweep.path().join(id).join("run-1.traj.json")).unwrap();
    assert_eq!(
        content, valid_content,
        "valid trajectory must not be overwritten"
    );
}

#[test]
fn restore_missing_trajectories_restores_cancelled() {
    let sweep = tempfile::tempdir().unwrap();
    let id = "instance-cancelled";
    let inst = make_instance(id, "cancelled", None);
    let selected = vec![&inst];

    // Write a live trajectory with exit_reason = "cancelled"
    let live_dir = sweep.path().join(id);
    std::fs::create_dir_all(&live_dir).unwrap();
    std::fs::write(
        live_dir.join("run-1.traj.json"),
        b"{\"info\":{\"exit_reason\":\"cancelled\"}}",
    )
    .unwrap();

    // Write the archived copy
    let archive_dir = sweep.path().join(".retry").join("r1");
    std::fs::create_dir_all(&archive_dir).unwrap();
    let archived_content = b"{\"info\":{\"exit_reason\":\"submitted\"}}";
    std::fs::write(
        archive_dir.join(format!("{id}.traj.json")),
        archived_content,
    )
    .unwrap();

    restore_missing_trajectories(sweep.path(), &selected, "r1").unwrap();

    let content = std::fs::read(sweep.path().join(id).join("run-1.traj.json")).unwrap();
    assert_eq!(
        content, archived_content,
        "cancelled trajectory must be replaced with archive"
    );
}

// ─── CLI: harness mismatch bypassed with --allow-harness-mismatch ─────────────

#[test]
fn cli_harness_mismatch_bypassed_with_flag() {
    let sweep = tempfile::tempdir().unwrap();

    // Build results with a fake manifest SHA that differs from any real SHA
    let mut results = base_sweep(vec![make_instance(
        "inst-a",
        "error",
        Some(FailureCategory::StepLimit),
    )]);
    results.manifest = Some(make_manifest_with_sha(
        "0000000000000000000000000000000000000000",
    ));
    write_results(sweep.path(), &results);

    // With --allow-harness-mismatch, the mismatch gate is bypassed.
    // Without --yes and not a TTY, we expect dry-run exit (non-zero due to no --yes)
    // but NOT the harness-mismatch error message.
    let out = Command::new(binary_path())
        .args([
            "bench",
            "retry",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--failure-category",
            "step_limit",
            "--allow-harness-mismatch",
        ])
        .output()
        .unwrap();

    // Must exit non-zero (dry-run without --yes), but must NOT say "SHA mismatch"
    assert!(
        !out.status.success(),
        "dry-run without --yes must be non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("SHA mismatch") && !stderr.contains("harness git SHA mismatch"),
        "mismatch gate must be bypassed with --allow-harness-mismatch: {stderr}"
    );
}

// ─── CLI: dry-run non-interactive hint ────────────────────────────────────────

#[test]
fn cli_dry_run_non_interactive_hint() {
    let sweep = tempfile::tempdir().unwrap();
    write_fixture_sweep(sweep.path(), &[("inst-x", "error", Some("step_limit"))]);

    // Running without --yes and piping stdin (non-TTY) must emit the --yes hint
    let out = Command::new(binary_path())
        .args([
            "bench",
            "retry",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--failure-category",
            "step_limit",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "non-interactive without --yes must fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--yes") || stderr.contains("non-interactively"),
        "must hint about --yes in non-interactive mode: {stderr}"
    );
}

// ─── CLI: bench tail round-trip on retry-amended results ─────────────────────

#[test]
fn cli_round_trip_tail_reads_retry_history() {
    let sweep = tempfile::tempdir().unwrap();
    let mut results = base_sweep(vec![{
        let mut r = make_instance("task-b", "submitted", None);
        r.retry_id = Some("tail-retry-id".into());
        r.previous_failure_category = Some(FailureCategory::StepLimit);
        r
    }]);
    results
        .retry_history
        .push(make_retry_entry("tail-retry-id", 1));
    write_results(sweep.path(), &results);

    let out = Command::new(binary_path())
        .args([
            "bench",
            "tail",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--once",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "bench tail --once must succeed on retry-amended results.json; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
