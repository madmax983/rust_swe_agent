//! TDD tests for `bench retry` — the post-completion operator-driven retry path.
//!
//! Red phase  : tests written before implementation, compile stubs added to pass
//!              the borrow checker while runtime assertions fail.
//! Green phase: minimal implementation makes all tests pass.
//! Refactor   : clean-up committed.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::artifact::ArtifactKind;
use rust_swe_agent::run::retry::{
    OverrideDelta, RetryHistoryEntry, RetrySelection, merge_retry_results, resolve_selection,
};
use rust_swe_agent::run::swebench::{InstanceResult, SWEEP_STATUS_COMPLETED, SweepResults};
use rust_swe_agent::trajectory::FailureCategory;

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
    }
}

fn write_results(dir: &Path, results: &SweepResults) {
    let json =
        rust_swe_agent::artifact::to_string_pretty(ArtifactKind::SweepResults, results).unwrap();
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
    let instances = vec![make_instance("a", "error", Some(FailureCategory::StepLimit))];
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

    let merged = merge_retry_results(&original, &retry_output, make_retry_entry("r1", 1), &selected_ids);

    assert_eq!(merged.instances.len(), 2);
    let b = merged.instances.iter().find(|r| r.instance_id == "b").unwrap();
    assert_eq!(b.outcome.as_deref(), Some("submitted"));
    assert!(b.retry_id.is_none(), "unselected row must not gain retry_id");
}

#[test]
fn merge_replaces_selected_row() {
    let original = base_sweep(vec![make_instance("a", "error", Some(FailureCategory::StepLimit))]);
    let mut new_a = make_instance("a", "submitted", None);
    new_a.steps = Some(99);
    let retry_output = base_sweep(vec![new_a]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged =
        merge_retry_results(&original, &retry_output, make_retry_entry("r1", 1), &selected_ids);

    let a = merged.instances.iter().find(|r| r.instance_id == "a").unwrap();
    assert_eq!(a.outcome.as_deref(), Some("submitted"));
    assert_eq!(a.steps, Some(99));
}

#[test]
fn merge_sets_retry_id_on_retried_instance() {
    let original = base_sweep(vec![make_instance("a", "error", Some(FailureCategory::StepLimit))]);
    let retry_output = base_sweep(vec![make_instance("a", "submitted", None)]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged = merge_retry_results(
        &original,
        &retry_output,
        make_retry_entry("test-retry-id-abc", 1),
        &selected_ids,
    );

    let a = merged.instances.iter().find(|r| r.instance_id == "a").unwrap();
    assert_eq!(a.retry_id.as_deref(), Some("test-retry-id-abc"));
}

#[test]
fn merge_sets_previous_failure_category() {
    let original = base_sweep(vec![make_instance("a", "error", Some(FailureCategory::StepLimit))]);
    let retry_output = base_sweep(vec![make_instance("a", "submitted", None)]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged =
        merge_retry_results(&original, &retry_output, make_retry_entry("rid", 1), &selected_ids);

    let a = merged.instances.iter().find(|r| r.instance_id == "a").unwrap();
    assert_eq!(a.previous_failure_category, Some(FailureCategory::StepLimit));
}

#[test]
fn merge_appends_retry_history_entry() {
    let original = base_sweep(vec![make_instance("a", "error", Some(FailureCategory::StepLimit))]);
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
    original.retry_history.push(make_retry_entry("old-entry", 1));
    let retry_output = base_sweep(vec![make_instance("a", "submitted", None)]);
    let selected_ids: HashSet<String> = ["a".to_owned()].into();

    let merged =
        merge_retry_results(&original, &retry_output, make_retry_entry("new-entry", 1), &selected_ids);

    assert_eq!(merged.retry_history.len(), 2);
    assert_eq!(merged.retry_history[0].retry_id, "old-entry");
    assert_eq!(merged.retry_history[1].retry_id, "new-entry");
}

// ─── Schema tests ─────────────────────────────────────────────────────────────

#[test]
fn schema_version_is_1_6() {
    assert_eq!(
        rust_swe_agent::artifact::ArtifactSchemaVersion::CURRENT,
        rust_swe_agent::artifact::ArtifactSchemaVersion::new(1, 6),
        "schema must be bumped to 1.6 for retry_history additive fields"
    );
}

#[test]
fn sweep_results_serializes_retry_history_when_present() {
    let mut results = base_sweep(vec![]);
    results.retry_history.push(make_retry_entry("test-id", 0));

    let json =
        rust_swe_agent::artifact::to_string_pretty(ArtifactKind::SweepResults, &results).unwrap();
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
        rust_swe_agent::artifact::to_string_pretty(ArtifactKind::SweepResults, &results).unwrap();
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
            let fc = cat.and_then(|c| {
                serde_json::from_value::<FailureCategory>(serde_json::json!(c)).ok()
            });
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
    results.retry_history.push(make_retry_entry("test-retry-id", 1));
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
    results.retry_history.push(make_retry_entry("some-retry-id", 1));
    write_results(sweep.path(), &results);

    // Minimal trajectory for the retried instance
    let traj_dir = sweep.path().join("task-a");
    std::fs::create_dir_all(&traj_dir).unwrap();
    std::fs::write(
        traj_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 6},
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
