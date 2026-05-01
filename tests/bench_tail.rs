//! `bench tail`: live aggregate sweep snapshots.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use chrono::{TimeZone, Utc};
use rust_swe_agent::run::tail::{SnapshotOptions, snapshot};
use rust_swe_agent::trajectory::{FailureCategory, TokenUsage, Trajectory, outcome};

fn binary_path() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_rust-swe-agent").map_or_else(
        |_| {
            let mut p = std::env::current_exe().unwrap();
            p.pop();
            p.pop();
            p.push("rust-swe-agent");
            p
        },
        std::path::PathBuf::from,
    )
}

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
                "cli": {"argv": ["rust-swe-agent", "bench", "swebench", "--parallel", "2"]}
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
