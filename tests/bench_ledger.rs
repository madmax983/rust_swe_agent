//! `bench ledger`: roll up cumulative actual spend across runs/sweeps.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

mod support;

// ── unit tests for pure helper functions ──────────────────────────────────────

use maxwells_daemon::run::ledger::{
    day_bucket, group_subtotals, logical_run_key, select_chain_cost,
};
use maxwells_daemon::trajectory::ResumeRecord;

// day_bucket

#[test]
fn day_bucket_utc_rfc3339() {
    assert_eq!(day_bucket(Some("2024-03-15T10:30:00Z")), "2024-03-15");
}

#[test]
fn day_bucket_converts_offset_to_utc() {
    // 2024-03-15T23:00:00-02:00 → 2024-03-16T01:00:00Z
    assert_eq!(day_bucket(Some("2024-03-15T23:00:00-02:00")), "2024-03-16");
}

#[test]
fn day_bucket_none_is_unknown() {
    assert_eq!(day_bucket(None), "unknown");
}

#[test]
fn day_bucket_invalid_is_unknown() {
    assert_eq!(day_bucket(Some("not-a-date")), "unknown");
}

// select_chain_cost

#[test]
fn select_chain_cost_picks_max() {
    let costs = [Some(0.10_f64), Some(0.20), Some(0.30)];
    let result = select_chain_cost(&costs);
    assert!((result.unwrap() - 0.30).abs() < 1e-9, "expected 0.30");
}

#[test]
fn select_chain_cost_all_none_is_none() {
    let costs = [None::<f64>, None, None];
    assert!(select_chain_cost(&costs).is_none());
}

#[test]
fn select_chain_cost_mixed_some_none() {
    let costs = [None::<f64>, Some(0.15), None];
    assert!((select_chain_cost(&costs).unwrap() - 0.15).abs() < 1e-9);
}

// logical_run_key

#[test]
fn logical_run_key_uses_resume_anchor() {
    let path = Path::new("/sweeps/run-1.traj.json");
    let resumed_at = "2024-01-02T00:00:00Z".to_owned();
    let resume_history = vec![ResumeRecord {
        original_started_at: Some("2024-01-01T00:00:00Z".to_owned()),
        resumed_at,
        prior_steps: 5,
        prior_cost_usd: 0.10,
        harness_git_sha_at_resume: None,
    }];
    let key = logical_run_key(path, Some("2024-01-02T00:00:00Z"), &resume_history);
    assert_eq!(key.1, "2024-01-01T00:00:00Z", "anchor should be original");
}

#[test]
fn logical_run_key_no_resume_uses_started_at() {
    let path = Path::new("/sweeps/run-1.traj.json");
    let key = logical_run_key(path, Some("2024-01-01T00:00:00Z"), &[]);
    assert_eq!(key.1, "2024-01-01T00:00:00Z");
}

#[test]
fn logical_run_key_missing_started_at_uses_path() {
    let path = Path::new("/sweeps/run-1.traj.json");
    let key = logical_run_key(path, None, &[]);
    assert!(key.1.contains("run-1.traj.json"));
}

// group_subtotals

#[test]
fn group_subtotals_sorted_cost_desc_then_key_asc() {
    let mut map = HashMap::new();
    map.insert("model-b".to_owned(), (0.50, 2));
    map.insert("model-a".to_owned(), (1.00, 1));
    map.insert("model-c".to_owned(), (0.50, 3));
    let subtotals = group_subtotals(&map);
    assert_eq!(subtotals[0].key, "model-a");
    assert!((subtotals[0].total_cost_usd - 1.00).abs() < 1e-9);
    // model-b and model-c have same cost; alphabetical tiebreak
    assert_eq!(subtotals[1].key, "model-b");
    assert_eq!(subtotals[2].key, "model-c");
}

// ── trajectory fixture helpers ────────────────────────────────────────────────

fn traj_json(
    model: &str,
    cost: Option<f64>,
    started_at: Option<&str>,
    resume_history: &[serde_json::Value],
) -> serde_json::Value {
    let cost_val = cost.map_or(serde_json::Value::Null, |c| serde_json::json!(c));
    let mut info = serde_json::json!({
        "task": "test-task",
        "model_name": model,
        "outcome": "submitted",
        "total_cost_usd": cost_val,
        "steps": 1,
        "test_invocations": []
    });
    if let Some(sa) = started_at {
        info["started_at"] = serde_json::json!(sa);
    }
    if !resume_history.is_empty() {
        info["resume_history"] = serde_json::json!(resume_history);
    }
    serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.3",
        "artifact_kind": "trajectory",
        "schema_version": {"major": 1, "minor": 4},
        "info": info,
        "messages": []
    })
}

fn write_traj(dir: &Path, name: &str, traj: &serde_json::Value) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(traj).unwrap()).unwrap();
    path
}

fn write_results_json(dir: &Path, dataset_alias: Option<&str>, dataset_path: &str) {
    // Minimal results.json that contains just enough for dataset label extraction.
    // Uses a raw JSON Value so we only need to satisfy the ledger's serde_json::Value
    // parsing path (not the strongly-typed SweepResults deserializer).
    let manifest = serde_json::json!({
        "dataset": {
            "path": dataset_path,
            "sha256": "abc123",
            "instance_count": 1,
            "source_kind": "huggingface",
            "alias": dataset_alias
        }
    });

    let results = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 12},
        "total": 1,
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "manifest": manifest
    });
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();
}

fn run_ledger(dirs: &[&Path], extra_args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(support::binary_path());
    cmd.args(["--log", "error", "bench", "ledger"]);
    for d in dirs {
        cmd.arg(d);
    }
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.output().unwrap()
}

fn run_ledger_json(dirs: &[&Path], extra_args: &[&str]) -> serde_json::Value {
    let mut args: Vec<&str> = vec!["--format", "json"];
    args.extend_from_slice(extra_args);
    let output = run_ledger(dirs, &args);
    assert!(
        output.status.success(),
        "bench ledger --format json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

// ── AC1: grand total ──────────────────────────────────────────────────────────

#[test]
fn grand_total_sums_all_trajectories() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );
    write_traj(
        dir.path(),
        "b.traj.json",
        &traj_json("model-b", Some(0.20), Some("2024-01-01T01:00:00Z"), &[]),
    );
    write_traj(
        dir.path(),
        "c.traj.json",
        &traj_json("model-a", Some(0.05), Some("2024-01-01T02:00:00Z"), &[]),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    let total = report["grand_total_usd"].as_f64().unwrap();
    assert!(
        (total - 0.35).abs() < 1e-6,
        "expected grand total ~0.35, got {total}"
    );
    assert_eq!(
        report["counted_trajectories"].as_u64().unwrap(),
        3,
        "3 trajectories counted"
    );
    assert_eq!(
        report["discovered_trajectories"].as_u64().unwrap(),
        3,
        "3 trajectories discovered"
    );
    assert_eq!(report["uncosted"].as_u64().unwrap(), 0);
}

// ── AC2: by_model, by_dataset, by_day each sum to grand total ─────────────────

#[test]
fn by_model_groups_sum_to_grand_total() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );
    write_traj(
        dir.path(),
        "b.traj.json",
        &traj_json("model-b", Some(0.20), Some("2024-01-01T01:00:00Z"), &[]),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    let grand_total = report["grand_total_usd"].as_f64().unwrap();

    let by_model = report["by_model"].as_array().unwrap();
    let model_sum: f64 = by_model
        .iter()
        .map(|g| g["total_cost_usd"].as_f64().unwrap())
        .sum();
    assert!(
        (model_sum - grand_total).abs() < 1e-6,
        "by_model sum {model_sum} != grand_total {grand_total}"
    );
}

#[test]
fn by_dataset_groups_sum_to_grand_total() {
    let dir = tempfile::tempdir().unwrap();
    let sub1 = dir.path().join("sweep1");
    let sub2 = dir.path().join("sweep2");
    std::fs::create_dir_all(&sub1).unwrap();
    std::fs::create_dir_all(&sub2).unwrap();
    write_results_json(&sub1, Some("lite"), "swebench_lite");
    write_results_json(&sub2, Some("verified"), "swebench_verified");
    write_traj(
        &sub1,
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );
    write_traj(
        &sub2,
        "b.traj.json",
        &traj_json("model-b", Some(0.20), Some("2024-01-01T01:00:00Z"), &[]),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    let grand_total = report["grand_total_usd"].as_f64().unwrap();
    let by_dataset = report["by_dataset"].as_array().unwrap();
    let dataset_sum: f64 = by_dataset
        .iter()
        .map(|g| g["total_cost_usd"].as_f64().unwrap())
        .sum();
    assert!(
        (dataset_sum - grand_total).abs() < 1e-6,
        "by_dataset sum {dataset_sum} != grand_total {grand_total}"
    );
    let labels: Vec<&str> = by_dataset
        .iter()
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    assert!(labels.contains(&"lite"), "expected 'lite' dataset label");
    assert!(
        labels.contains(&"verified"),
        "expected 'verified' dataset label"
    );
}

#[test]
fn by_dataset_unknown_when_no_results_json() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    let by_dataset = report["by_dataset"].as_array().unwrap();
    assert!(
        by_dataset
            .iter()
            .map(|g| g["key"].as_str().unwrap())
            .any(|x| x == "unknown"),
        "expected 'unknown' when no results.json"
    );
}

#[test]
fn by_day_groups_sum_to_grand_total_with_unknown_bucket() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );
    write_traj(
        dir.path(),
        "b.traj.json",
        &traj_json("model-b", Some(0.15), Some("2024-01-02T00:00:00Z"), &[]),
    );
    // No started_at → unknown bucket
    write_traj(
        dir.path(),
        "c.traj.json",
        &traj_json("model-a", Some(0.05), None, &[]),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    let grand_total = report["grand_total_usd"].as_f64().unwrap();
    let by_day = report["by_day"].as_array().unwrap();
    let day_sum: f64 = by_day
        .iter()
        .map(|g| g["total_cost_usd"].as_f64().unwrap())
        .sum();
    assert!(
        (day_sum - grand_total).abs() < 1e-6,
        "by_day sum {day_sum} != grand_total {grand_total}"
    );
    let keys: Vec<&str> = by_day.iter().map(|g| g["key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"2024-01-01"), "expected 2024-01-01");
    assert!(keys.contains(&"2024-01-02"), "expected 2024-01-02");
    assert!(keys.contains(&"unknown"), "expected unknown bucket");
}

// ── AC3: budget_usd ───────────────────────────────────────────────────────────

#[test]
fn budget_remaining_under_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );

    let output = run_ledger(&[dir.path()], &["--budget-usd", "1.00"]);
    assert!(
        output.status.success(),
        "expected exit 0 when under budget\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    // Text mode stdout isn't JSON — check the written artifact on disk instead.
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("ledger.json")).unwrap())
            .unwrap();
    assert_eq!(artifact["over_budget"], serde_json::json!(false));
    let remaining = artifact["remaining_usd"].as_f64().unwrap();
    assert!(remaining > 0.0, "remaining_usd should be positive");
    drop(report);
}

#[test]
fn budget_exceeded_exits_49() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.50), Some("2024-01-01T00:00:00Z"), &[]),
    );

    let output = run_ledger(&[dir.path()], &["--budget-usd", "0.10"]);
    let exit_code = output.status.code().unwrap();
    assert_eq!(
        exit_code,
        49,
        "expected exit 49 when budget exceeded\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Report must still be printed before exit.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.5") || stdout.contains("grand total") || stdout.contains("ledger"),
        "report must be printed before exit-49\nstdout: {stdout}"
    );
    // Stderr must contain outcome_class.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ledger_budget_exceeded"),
        "stderr must contain outcome_class: ledger_budget_exceeded\nstderr: {stderr}"
    );
}

// ── AC4: --format json ────────────────────────────────────────────────────────

#[test]
fn json_format_shape() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    assert_eq!(
        report["artifact_kind"],
        serde_json::json!("ledger_report"),
        "artifact_kind must be 'ledger_report'"
    );
    assert_eq!(
        report["schema_version"]["major"].as_u64().unwrap(),
        1,
        "schema_version.major must be 1"
    );
    // by_model, by_dataset, by_day must be arrays of {key, total_cost_usd, trajectory_count}
    for field in &["by_model", "by_dataset", "by_day"] {
        let arr = report[field]
            .as_array()
            .unwrap_or_else(|| panic!("{field} must be array"));
        if !arr.is_empty() {
            let item = &arr[0];
            assert!(item["key"].is_string(), "{field}[0].key must be string");
            assert!(
                item["total_cost_usd"].is_f64() || item["total_cost_usd"].is_u64(),
                "{field}[0].total_cost_usd must be number"
            );
            assert!(
                item["trajectory_count"].is_u64(),
                "{field}[0].trajectory_count must be u64"
            );
        }
    }
    assert!(
        report["grand_total_usd"].is_f64() || report["grand_total_usd"].is_u64(),
        "grand_total_usd must be number"
    );
    assert!(
        report["counted_trajectories"].is_u64(),
        "counted_trajectories required"
    );
    assert!(
        report["chained_trajectories"].is_u64(),
        "chained_trajectories required"
    );
    assert!(
        report["discovered_trajectories"].is_u64(),
        "discovered_trajectories required"
    );
    assert!(report["uncosted"].is_u64(), "uncosted required");
}

// ── AC5: resume chain counted once ───────────────────────────────────────────

#[test]
fn resume_chain_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    // Checkpoint: cost 0.10, no resume_history
    write_traj(
        dir.path(),
        "checkpoint.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );
    // Resumed: cumulative cost 0.30, resume_history points back to original started_at
    let resume_record = serde_json::json!({
        "original_started_at": "2024-01-01T00:00:00Z",
        "resumed_at": "2024-01-01T01:00:00Z",
        "prior_steps": 5,
        "prior_cost_usd": 0.10
    });
    write_traj(
        dir.path(),
        "resumed.traj.json",
        &traj_json(
            "model-a",
            Some(0.30),
            Some("2024-01-01T01:00:00Z"),
            &[resume_record],
        ),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    let total = report["grand_total_usd"].as_f64().unwrap();
    assert!(
        (total - 0.30).abs() < 0.01,
        "resume chain should be counted once at 0.30, got {total}"
    );
    assert_eq!(
        report["counted_trajectories"].as_u64().unwrap(),
        1,
        "resume chain = 1 logical run"
    );
    assert_eq!(
        report["chained_trajectories"].as_u64().unwrap(),
        1,
        "one chain member absorbed (checkpoint); discovered == counted + chained + uncosted"
    );
    assert_eq!(
        report["discovered_trajectories"].as_u64().unwrap(),
        report["counted_trajectories"].as_u64().unwrap()
            + report["chained_trajectories"].as_u64().unwrap()
            + report["uncosted"].as_u64().unwrap(),
        "invariant: discovered == counted + chained + uncosted"
    );
}

// ── AC5: independent reruns counted separately ────────────────────────────────

#[test]
fn independent_reruns_counted_separately() {
    let dir = tempfile::tempdir().unwrap();
    // Two independent reruns — distinct started_at, no resume_history
    write_traj(
        dir.path(),
        "run-1.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );
    write_traj(
        dir.path(),
        "run-2.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-02T00:00:00Z"), &[]),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    let total = report["grand_total_usd"].as_f64().unwrap();
    assert!(
        (total - 0.20).abs() < 1e-6,
        "two independent reruns should sum to 0.20, got {total}"
    );
    assert_eq!(
        report["counted_trajectories"].as_u64().unwrap(),
        2,
        "two independent logical runs"
    );
}

// ── AC6: uncosted surfaced ────────────────────────────────────────────────────

#[test]
fn uncosted_trajectories_surfaced() {
    let dir = tempfile::tempdir().unwrap();
    // One costed, one with no cost (null)
    write_traj(
        dir.path(),
        "costed.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );
    write_traj(
        dir.path(),
        "no-cost.traj.json",
        &traj_json("model-b", None, Some("2024-01-01T01:00:00Z"), &[]),
    );

    let report = run_ledger_json(&[dir.path()], &[]);
    assert_eq!(report["uncosted"].as_u64().unwrap(), 1, "one uncosted");
    let total = report["grand_total_usd"].as_f64().unwrap();
    assert!(
        (total - 0.10).abs() < 1e-6,
        "uncosted should not affect grand total"
    );
    assert_eq!(
        report["counted_trajectories"].as_u64().unwrap(),
        1,
        "only one costed trajectory counted"
    );
}

// ── AC7: zero network / zero model calls (implicit — tested via running the command) ──

#[test]
fn bad_format_is_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );

    let output = run_ledger(&[dir.path()], &["--format", "xml"]);
    let code = output.status.code().unwrap();
    assert_eq!(code, 2, "unknown format should be exit 2 (usage_error)");
}

#[test]
fn multiple_dirs_aggregate() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    write_traj(
        dir1.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );
    write_traj(
        dir2.path(),
        "b.traj.json",
        &traj_json("model-b", Some(0.20), Some("2024-01-01T01:00:00Z"), &[]),
    );

    let report = run_ledger_json(&[dir1.path(), dir2.path()], &[]);
    let total = report["grand_total_usd"].as_f64().unwrap();
    assert!(
        (total - 0.30).abs() < 1e-6,
        "two dirs should aggregate to 0.30, got {total}"
    );
    assert_eq!(report["counted_trajectories"].as_u64().unwrap(), 2);
}

#[test]
fn writes_ledger_json_artifact() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );

    let output = run_ledger(&[dir.path()], &[]);
    assert!(output.status.success());

    let artifact_path = dir.path().join("ledger.json");
    assert!(artifact_path.exists(), "ledger.json must be written");
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifact_path).unwrap()).unwrap();
    assert_eq!(
        artifact["artifact_kind"],
        serde_json::json!("ledger_report")
    );
}

#[test]
fn text_format_shows_ledger_header() {
    let dir = tempfile::tempdir().unwrap();
    write_traj(
        dir.path(),
        "a.traj.json",
        &traj_json("model-a", Some(0.10), Some("2024-01-01T00:00:00Z"), &[]),
    );

    let output = run_ledger(&[dir.path()], &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("bench ledger"),
        "expected 'bench ledger' header\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("grand total") || stdout.contains("Grand total"),
        "expected grand total in output\nstdout: {stdout}"
    );
}
