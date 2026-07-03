//! `bench tail`: live aggregate sweep snapshots.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use chrono::{TimeZone, Utc};
use maxwells_daemon::run::tail::{
    InstanceStatus, SnapshotOptions, instance_rows, render_text, snapshot,
};
use maxwells_daemon::trajectory::{
    FailureCategory, FallbackAttemptRecord, FallbackSummary, TokenUsage, Trajectory, outcome,
};

mod support;
use support::binary_path;

fn write_results(dir: &Path, value: &serde_json::Value) {
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn write_traj(
    dir: &Path,
    instance_id: &str,
    outcome_value: &str,
    failure_category: Option<FailureCategory>,
    cost: f64,
    started_at: &str,
    ended_at: &str,
) {
    let mut traj = Trajectory::new();
    traj.info.outcome = Some(outcome_value.into());
    traj.info.exit_reason = Some(outcome_value.into());
    traj.info.failure_category = failure_category;
    traj.info.total_cost_usd = Some(cost);
    traj.info.token_usage = Some(TokenUsage {
        prompt_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 20,
    });
    traj.info.started_at = Some(started_at.into());
    traj.info.ended_at = Some(ended_at.into());
    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
}

fn opts_at(ts: chrono::DateTime<Utc>) -> SnapshotOptions {
    SnapshotOptions {
        now: ts,
        burn_rate_window: chrono::Duration::minutes(5),
    }
}

#[test]
fn snapshot_keeps_zero_actual_cost_separate_from_baseline_cost() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 1,
            "submitted": 1,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 1,
            "actual_cost_usd": 0.0,
            "actual_cost_source": "free_tier_inferred",
            "baseline_cost_usd": 1.8,
            "baseline_cost_model": "claude-3-5-sonnet",
            "instances": []
        }),
    );
    let mut traj = Trajectory::new();
    traj.info.outcome = Some(outcome::SUBMITTED.into());
    traj.info.exit_reason = Some("submitted".into());
    traj.info.total_cost_usd = Some(0.0);
    traj.info.actual_cost_usd = Some(0.0);
    traj.info.actual_cost_source = Some(maxwells_daemon::cost::CostSource::FreeTierInferred);
    traj.info.baseline_cost_usd = Some(1.8);
    traj.info.baseline_cost_model = Some("claude-3-5-sonnet".into());
    traj.info.token_usage = Some(TokenUsage {
        prompt_tokens: 100_000,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 100_000,
    });
    traj.info.started_at = Some("2026-04-30T01:55:00Z".into());
    traj.info.ended_at = Some("2026-04-30T01:56:00Z".into());
    std::fs::write(
        dir.path().join("free-tier.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    let snap = snapshot(
        dir.path(),
        &opts_at(Utc.with_ymd_and_hms(2026, 4, 30, 2, 0, 0).unwrap()),
    )
    .unwrap();

    assert!(snap.cumulative_cost_usd.abs() < f64::EPSILON);
    assert!((snap.baseline_cumulative_cost_usd - 1.8).abs() < 1e-9);
    let text = render_text(&snap);
    assert!(text.contains("Actual cost: $0.0000"), "{text}");
    assert!(text.contains("Baseline:    $1.8000"), "{text}");
}

#[test]
fn snapshot_prefers_legacy_trajectory_actual_over_results_estimate() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "artifact_kind": "sweep_results",
            "schema_version": {"major": 1, "minor": 0},
            "total": 1,
            "submitted": 0,
            "skipped": 0,
            "errored": 1,
            "budget_halted": 0,
            "with_patch": 0,
            "total_cost_usd": 1.8,
            "instances": [{
                "instance_id": "legacy-free",
                "exit_reason": "error",
                "outcome": "error",
                "cost_usd": 1.8,
                "total_input_tokens": 100_000,
                "total_completion_tokens": 100_000
            }]
        }),
    );
    let mut traj = Trajectory::new();
    traj.info.model_name = Some("openrouter/baidu/cobuddy:free".into());
    traj.info.outcome = Some(outcome::ERROR.into());
    traj.info.exit_reason = Some("error".into());
    traj.info.token_usage = Some(TokenUsage {
        prompt_tokens: 100_000,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 100_000,
    });
    std::fs::write(
        dir.path().join("legacy-free.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    let snap = snapshot(
        dir.path(),
        &opts_at(Utc.with_ymd_and_hms(2026, 4, 30, 2, 0, 0).unwrap()),
    )
    .unwrap();

    assert!(snap.cumulative_cost_usd.abs() < f64::EPSILON, "{snap:#?}");
    assert!(
        (snap.baseline_cumulative_cost_usd - 1.8).abs() < 1e-9,
        "{snap:#?}"
    );
}

#[test]
fn snapshot_counts_running_sweep_and_ignores_half_written_trajectory() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 3,
            "submitted": 0,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "estimated_cost_usd": 0.0,
            "cost_limit_usd": 2.0,
            "manifest": {
                "runtime": {
                    "started_at_utc": "2026-04-30T01:50:00Z",
                    "finished_at_utc": null,
                    "host_os": "linux",
                    "resume_mode": false
                },
                "cli": {"argv": ["max", "bench", "swebench", "--parallel", "2"]}
            },
            "instances": []
        }),
    );
    write_traj(
        dir.path(),
        "done",
        outcome::ERROR,
        Some(FailureCategory::ModelApi),
        0.40,
        "2026-04-30T01:55:00Z",
        "2026-04-30T01:58:00Z",
    );
    std::fs::write(dir.path().join("half.traj.json"), "{\"trajectory_format\"").unwrap();

    let snap = snapshot(
        dir.path(),
        &opts_at(Utc.with_ymd_and_hms(2026, 4, 30, 2, 0, 0).unwrap()),
    )
    .unwrap();

    assert_eq!(snap.total, 3);
    assert_eq!(snap.completed, 1);
    assert_eq!(snap.in_flight, 2);
    assert_eq!(snap.pending, 0);
    assert!((snap.cumulative_cost_usd - 0.40).abs() < f64::EPSILON);
    assert_eq!(snap.budget_cap_usd, Some(2.0));
    assert_eq!(snap.pct_of_cap_used, Some(20.0));
    assert_eq!(snap.eta_seconds, Some(1_200));
    assert_eq!(snap.started_at.as_deref(), Some("2026-04-30T01:50:00Z"));
    assert_eq!(snap.last_event_at.as_deref(), Some("2026-04-30T01:58:00Z"));
    assert_eq!(
        snap.failure_counts,
        BTreeMap::from([(FailureCategory::ModelApi, 1)])
    );
    assert!(
        (snap.burn_rate_usd_per_min - 0.08).abs() < f64::EPSILON,
        "{snap:#?}"
    );
    assert!(
        snap.warnings.iter().any(|w| w.contains("half.traj.json")),
        "{snap:#?}"
    );
}

#[test]
fn snapshot_marks_complete_when_all_instances_are_accounted_for() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 2,
            "submitted": 1,
            "skipped": 0,
            "errored": 1,
            "budget_halted": 0,
            "with_patch": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "estimated_cost_usd": 0.3,
            "instances": [
                {"instance_id": "a", "exit_reason": "submitted", "outcome": "submitted", "cost_usd": 0.1},
                {"instance_id": "b", "exit_reason": "error", "outcome": "error", "failure_category": "step_limit", "cost_usd": 0.2}
            ]
        }),
    );

    let snap = snapshot(
        dir.path(),
        &opts_at(Utc.with_ymd_and_hms(2026, 4, 30, 2, 0, 0).unwrap()),
    )
    .unwrap();

    assert!(snap.is_complete);
    assert_eq!(snap.completed, 2);
    assert_eq!(snap.in_flight, 0);
    assert_eq!(snap.pending, 0);
    assert_eq!(snap.eta_seconds, Some(0));
}

#[test]
fn snapshot_marks_budget_halt_as_abort() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 3,
            "submitted": 2,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 1,
            "with_patch": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "estimated_cost_usd": 1.2,
            "cost_limit_usd": 1.0,
            "instances": [
                {"instance_id": "a", "exit_reason": "submitted", "outcome": "submitted", "cost_usd": 0.6},
                {"instance_id": "b", "exit_reason": "submitted", "outcome": "submitted", "cost_usd": 0.6},
                {"instance_id": "c", "exit_reason": "budget_halt"}
            ]
        }),
    );

    let snap = snapshot(
        dir.path(),
        &opts_at(Utc.with_ymd_and_hms(2026, 4, 30, 2, 0, 0).unwrap()),
    )
    .unwrap();

    assert_eq!(
        snap.abort_reason.as_deref(),
        Some("budget cap hit: 1 instance(s) never started")
    );
}

#[test]
fn snapshot_renders_cancelling_deadline_state() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 3,
            "submitted": 1,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "estimated_cost_usd": 0.0,
            "sweep_status": "cancelling",
            "cancelled_at": "2026-04-30T01:59:30Z",
            "cancel_deadline_at": "2026-04-30T02:00:00Z",
            "completed": 1,
            "in_flight_at_cancel": 1,
            "not_started": 1,
            "instances": [
                {"instance_id": "a", "exit_reason": "submitted", "outcome": "submitted", "cost_usd": 0.0}
            ],
            "manifest": {
                "runtime": {
                    "started_at_utc": "2026-04-30T01:50:00Z",
                    "finished_at_utc": null,
                    "host_os": "linux",
                    "resume_mode": false
                },
                "cli": {"argv": ["max", "bench", "swebench", "--parallel", "2"]}
            }
        }),
    );

    let snap = snapshot(
        dir.path(),
        &opts_at(Utc.with_ymd_and_hms(2026, 4, 30, 1, 59, 45).unwrap()),
    )
    .unwrap();

    assert_eq!(snap.status, "cancelling");
    assert_eq!(snap.cancelling_seconds_left, Some(15));
    assert_eq!(snap.completed, 1);
    assert_eq!(snap.in_flight, 1);
    assert_eq!(snap.pending, 1);
    assert!(!snap.is_complete);
    let text = maxwells_daemon::run::tail::render_text(&snap);
    assert!(
        text.contains("Status:      cancelling (0m:15s left)"),
        "{text}"
    );
}

#[test]
fn snapshot_uses_total_cost_usd_metadata_when_no_records_exist() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 2,
            "submitted": 0,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 0,
            "total_input_tokens": 0,
            "total_completion_tokens": 0,
            "total_cost_usd": 1.75,
            "instances": []
        }),
    );

    let snap = snapshot(
        dir.path(),
        &opts_at(Utc.with_ymd_and_hms(2026, 4, 30, 2, 0, 0).unwrap()),
    )
    .unwrap();

    assert!((snap.cumulative_cost_usd - 1.75).abs() < f64::EPSILON);
}

#[test]
fn snapshot_reports_zero_actual_and_baseline_cache_repricing() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 2,
            "submitted": 1,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 0,
            "cost_limit_usd": 1.0,
            "manifest": {
                "model": {
                    "name": "anthropic/claude-sonnet-4-6"
                }
            },
            "instances": [{
                "instance_id": "cached",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "cost_usd": 0.0,
                "total_input_tokens": 0,
                "total_cache_read_tokens": 1_000_000,
                "total_cache_creation_tokens": 0,
                "total_completion_tokens": 0
            }]
        }),
    );

    let snap = snapshot(
        dir.path(),
        &opts_at(Utc.with_ymd_and_hms(2026, 4, 30, 2, 0, 0).unwrap()),
    )
    .unwrap();

    assert!(snap.cumulative_cost_usd.abs() < f64::EPSILON, "{snap:#?}");
    assert!(
        (snap.baseline_cumulative_cost_usd - 0.3).abs() < 1e-9,
        "{snap:#?}"
    );
    assert_eq!(snap.abort_reason, None, "{snap:#?}");
    assert!(
        snap.pct_of_cap_used.is_some_and(|pct| pct.abs() < 1e-9),
        "{snap:#?}"
    );
}

#[test]
fn cli_once_json_outputs_single_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 1,
            "submitted": 0,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "estimated_cost_usd": 0.0,
            "instances": []
        }),
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "tail",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--once",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["total"], 1);
    assert_eq!(v["completed"], 0);
    assert_eq!(v["pending"], 1);
}

#[test]
fn cli_once_budget_abort_exits_nonzero_with_reason() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 2,
            "submitted": 1,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 1,
            "with_patch": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "estimated_cost_usd": 1.2,
            "cost_limit_usd": 1.0,
            "instances": [
                {"instance_id": "a", "exit_reason": "submitted", "outcome": "submitted", "cost_usd": 1.2},
                {"instance_id": "b", "exit_reason": "budget_halt"}
            ]
        }),
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "tail",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--once",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("budget cap hit: 1 instance(s) never started"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        v["abort_reason"],
        "budget cap hit: 1 instance(s) never started"
    );
}

fn write_fallback_traj(dir: &Path, filename: &str, final_model: &str, fallback_count: u32) {
    let mut traj = Trajectory::new();
    traj.info.outcome = Some(outcome::SUBMITTED.into());
    traj.info.exit_reason = Some(outcome::SUBMITTED.into());
    traj.info.total_cost_usd = Some(0.01);
    traj.info.fallback_summary = Some(FallbackSummary {
        primary_model: "primary".into(),
        final_model: final_model.into(),
        fallback_happened: fallback_count > 0,
        fallback_count,
        attempted_models: vec!["primary".into(), final_model.into()],
        failed_attempts: (0..fallback_count)
            .map(|_| FallbackAttemptRecord {
                model: "primary".into(),
                failure_reason: "rate_limited".into(),
                retry_after_secs: None,
            })
            .collect(),
        all_failed: false,
    });
    std::fs::write(
        dir.join(filename),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
}

#[test]
fn model_mix_counts_all_rerun_slots_not_just_first() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();

    // Instance "task-a" ran twice: slot 0 used "secondary", slot 1 used "primary".
    // After merge, final_model is "secondary" (first non-None). Without the fix,
    // only "secondary" would appear in model_mix; with the fix, both appear.
    let task_dir = dir.path().join("task-a");
    std::fs::create_dir_all(&task_dir).unwrap();
    write_fallback_traj(&task_dir, "run-0.traj.json", "secondary", 1);
    write_fallback_traj(&task_dir, "run-1.traj.json", "primary", 0);

    let snap = snapshot(dir.path(), &opts_at(now)).unwrap();
    assert!(
        snap.model_mix.contains_key("primary"),
        "primary should appear in model_mix: {:?}",
        snap.model_mix
    );
    assert!(
        snap.model_mix.contains_key("secondary"),
        "secondary should appear in model_mix: {:?}",
        snap.model_mix
    );
    assert_eq!(
        snap.model_mix["primary"] + snap.model_mix["secondary"],
        2,
        "total slot count should be 2"
    );
}

#[test]
fn fallback_count_is_summed_across_rerun_slots() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();

    let task_dir = dir.path().join("task-b");
    std::fs::create_dir_all(&task_dir).unwrap();
    write_fallback_traj(&task_dir, "run-0.traj.json", "secondary", 2);
    write_fallback_traj(&task_dir, "run-1.traj.json", "secondary", 1);

    let snap = snapshot(dir.path(), &opts_at(now)).unwrap();
    assert_eq!(
        snap.total_fallbacks, 3,
        "fallback_count should be summed across slots"
    );
}

fn write_all_failed_traj(dir: &Path, filename: &str) {
    let mut traj = Trajectory::new();
    traj.info.outcome = Some("error".into());
    traj.info.exit_reason = Some("error".into());
    traj.info.total_cost_usd = Some(0.0);
    traj.info.fallback_summary = Some(FallbackSummary {
        primary_model: "primary".into(),
        final_model: "secondary".into(),
        fallback_happened: true,
        fallback_count: 2,
        attempted_models: vec!["primary".into(), "secondary".into()],
        failed_attempts: vec![
            FallbackAttemptRecord {
                model: "primary".into(),
                failure_reason: "rate_limited".into(),
                retry_after_secs: None,
            },
            FallbackAttemptRecord {
                model: "secondary".into(),
                failure_reason: "rate_limited".into(),
                retry_after_secs: None,
            },
        ],
        all_failed: true,
    });
    std::fs::write(
        dir.join(filename),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
}

#[test]
fn all_failed_trajectory_excluded_from_model_mix() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();

    let task_dir = dir.path().join("task-c");
    std::fs::create_dir_all(&task_dir).unwrap();
    write_all_failed_traj(&task_dir, "run-0.traj.json");

    let snap = snapshot(dir.path(), &opts_at(now)).unwrap();
    assert!(
        snap.model_mix.is_empty(),
        "all_failed trajectories should not appear in model_mix: {:?}",
        snap.model_mix
    );
}

// ---- instance_rows() — per-instance rows for the sweep dashboard (issue #641) ----

fn write_partial_traj(dir: &Path, filename: &str, steps: u32) {
    let mut traj = Trajectory::new();
    traj.info.partial = true;
    traj.info.partial_reason = Some("in_progress".into());
    traj.info.steps = Some(steps);
    std::fs::write(
        dir.join(filename),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
}

#[test]
fn instance_rows_errors_when_sweep_dir_missing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");
    assert!(instance_rows(&missing).is_err());
}

#[test]
fn instance_rows_reports_terminal_instance_from_flat_trajectory() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "alpha",
        outcome::SUBMITTED,
        None,
        0.5,
        "2026-04-30T01:00:00Z",
        "2026-04-30T01:05:00Z",
    );

    let rows = instance_rows(dir.path()).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.instance_id, "alpha");
    assert_eq!(row.run_index, 1);
    assert_eq!(row.status, InstanceStatus::Terminal);
    assert_eq!(row.outcome.as_deref(), Some(outcome::SUBMITTED));
}

#[test]
fn instance_rows_reports_in_flight_instance_from_partial_nested_trajectory() {
    let dir = tempfile::tempdir().unwrap();
    let instance_dir = dir.path().join("bravo");
    std::fs::create_dir_all(&instance_dir).unwrap();
    write_partial_traj(&instance_dir, "run-1.traj.json", 4);

    let rows = instance_rows(dir.path()).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.instance_id, "bravo");
    assert_eq!(row.run_index, 1);
    assert_eq!(row.status, InstanceStatus::InFlight);
    assert_eq!(row.current_step, Some(4));
    assert!(row.outcome.is_none());
}

#[test]
fn instance_rows_distinguishes_run_index_for_reruns() {
    let dir = tempfile::tempdir().unwrap();
    let instance_dir = dir.path().join("charlie");
    std::fs::create_dir_all(&instance_dir).unwrap();
    // run 1 already finished; run 2 is still in flight.
    let mut traj1 = Trajectory::new();
    traj1.info.outcome = Some(outcome::SUBMITTED.into());
    traj1.info.exit_reason = Some(outcome::SUBMITTED.into());
    traj1.info.steps = Some(9);
    std::fs::write(
        instance_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&traj1).unwrap(),
    )
    .unwrap();
    write_partial_traj(&instance_dir, "run-2.traj.json", 3);

    let mut rows = instance_rows(dir.path()).unwrap();
    rows.sort_by_key(|r| r.run_index);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].run_index, 1);
    assert_eq!(rows[0].status, InstanceStatus::Terminal);
    assert_eq!(rows[0].current_step, Some(9));
    assert_eq!(rows[1].run_index, 2);
    assert_eq!(rows[1].status, InstanceStatus::InFlight);
    assert_eq!(rows[1].current_step, Some(3));
}

#[test]
fn instance_rows_reports_pending_instance_when_filter_spec_names_it() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 2,
            "submitted": 1,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 1,
            "filter_spec": {
                "original_count": 2,
                "selected_count": 2,
                "instance_ids": ["delta", "echo"]
            },
            "instances": [{
                "instance_id": "delta",
                "exit_reason": "submitted",
                "outcome": "submitted"
            }]
        }),
    );

    let rows = instance_rows(dir.path()).unwrap();
    let echo = rows.iter().find(|r| r.instance_id == "echo").unwrap();
    assert_eq!(echo.status, InstanceStatus::Pending);
    assert!(echo.outcome.is_none());
    assert!(echo.current_step.is_none());

    let delta = rows.iter().find(|r| r.instance_id == "delta").unwrap();
    assert_eq!(delta.status, InstanceStatus::Terminal);
    assert_eq!(delta.outcome.as_deref(), Some("submitted"));
}

#[test]
fn instance_rows_has_no_pending_rows_without_explicit_instance_ids() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        &serde_json::json!({
            "total": 5,
            "submitted": 1,
            "skipped": 0,
            "errored": 0,
            "budget_halted": 0,
            "with_patch": 1,
            "instances": [{
                "instance_id": "foxtrot",
                "exit_reason": "submitted",
                "outcome": "submitted"
            }]
        }),
    );

    let rows = instance_rows(dir.path()).unwrap();
    assert!(rows.iter().all(|r| r.status != InstanceStatus::Pending));
    assert_eq!(rows.len(), 1);
}

#[test]
fn snapshot_counts_partial_trajectories_across_multiple_instances() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();

    for name in ["golf", "hotel"] {
        let instance_dir = dir.path().join(name);
        std::fs::create_dir_all(&instance_dir).unwrap();
        write_partial_traj(&instance_dir, "run-1.traj.json", 2);
    }
    let terminal_dir = dir.path().join("india");
    std::fs::create_dir_all(&terminal_dir).unwrap();
    let mut traj = Trajectory::new();
    traj.info.outcome = Some(outcome::SUBMITTED.into());
    traj.info.exit_reason = Some(outcome::SUBMITTED.into());
    std::fs::write(
        terminal_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    let snap = snapshot(dir.path(), &opts_at(now)).unwrap();
    assert_eq!(snap.partial_persisted, 2, "{snap:#?}");
}

#[test]
fn instance_rows_prefers_nested_layout_over_legacy_flat_file_deterministically() {
    let dir = tempfile::tempdir().unwrap();
    // A legacy flat file (run 1, terminal/error) coexists with a freshly
    // resumed nested run-1.traj.json (in-flight) for the same instance —
    // the nested (current) layout must always win, regardless of directory
    // iteration order.
    let mut legacy = Trajectory::new();
    legacy.info.outcome = Some(outcome::ERROR.into());
    legacy.info.exit_reason = Some(outcome::ERROR.into());
    std::fs::write(
        dir.path().join("juliet.traj.json"),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .unwrap();
    let instance_dir = dir.path().join("juliet");
    std::fs::create_dir_all(&instance_dir).unwrap();
    write_partial_traj(&instance_dir, "run-1.traj.json", 5);

    let rows = instance_rows(dir.path()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, InstanceStatus::InFlight);
    assert_eq!(rows[0].current_step, Some(5));
}

#[test]
fn instance_rows_does_not_fall_back_to_legacy_flat_when_nested_file_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    // Legacy flat file with a real (stale) terminal outcome.
    let mut legacy = Trajectory::new();
    legacy.info.outcome = Some(outcome::ERROR.into());
    legacy.info.exit_reason = Some(outcome::ERROR.into());
    std::fs::write(
        dir.path().join("kilo.traj.json"),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .unwrap();
    // A nested file exists for the same instance/run (current layout takes
    // precedence) but is mid-write / corrupt and fails to parse.
    let instance_dir = dir.path().join("kilo");
    std::fs::create_dir_all(&instance_dir).unwrap();
    std::fs::write(instance_dir.join("run-1.traj.json"), "{not valid json").unwrap();

    let rows = instance_rows(dir.path()).unwrap();
    assert!(
        rows.iter().all(|r| r.instance_id != "kilo"),
        "must not resurrect the stale legacy flat row when the nested file exists but is \
         unreadable: {rows:?}"
    );
}
