//! `bench du`: report run-directory disk footprint and safely reclaim stale
//! sweep artifacts. See `docs/spec-disk-usage.md` and GitHub issue #533.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

mod support;

// ── unit tests for pure helper functions ──────────────────────────────────────

use maxwells_daemon::run::du::{
    Category, CategoryBytes, LifecycleState, PruneSelectors, SweepReport, classify_category,
    classify_lifecycle, evaluate_prune_candidates, parse_age_selector, recursive_size_walk,
};

// classify_lifecycle (AC3: complete / interrupted / incomplete / in-progress)

#[test]
fn lifecycle_complete_has_results_json_only() {
    assert_eq!(
        classify_lifecycle(true, false, false),
        LifecycleState::Complete
    );
}

#[test]
fn lifecycle_interrupted_only_stale_partial_checkpoints() {
    assert_eq!(
        classify_lifecycle(false, true, false),
        LifecycleState::Interrupted
    );
}

#[test]
fn lifecycle_incomplete_neither_results_nor_checkpoint() {
    assert_eq!(
        classify_lifecycle(false, false, false),
        LifecycleState::Incomplete
    );
}

#[test]
fn lifecycle_in_progress_when_fresh_checkpoint_present() {
    assert_eq!(
        classify_lifecycle(false, true, true),
        LifecycleState::InProgress
    );
}

#[test]
fn lifecycle_in_progress_overrides_complete() {
    // A results.json can already exist (e.g. from a prior sweep) while a
    // `bench retry` continues writing fresh checkpoints into the same dir.
    // In-progress must win — never safe to delete.
    assert_eq!(
        classify_lifecycle(true, true, true),
        LifecycleState::InProgress
    );
}

// classify_category

#[test]
fn classify_category_final_trajectory() {
    assert_eq!(
        classify_category("run-0.traj.json", Some(false)),
        Category::Trajectories
    );
}

#[test]
fn classify_category_partial_trajectory_is_checkpoint() {
    assert_eq!(
        classify_category("run-0.traj.json", Some(true)),
        Category::PartialCheckpoints
    );
}

#[test]
fn classify_category_unreadable_trajectory_defaults_to_trajectories() {
    assert_eq!(
        classify_category("run-0.traj.json", None),
        Category::Trajectories
    );
}

#[test]
fn classify_category_evaluation_json() {
    assert_eq!(
        classify_category("evaluation.json", None),
        Category::Evaluation
    );
}

#[test]
fn classify_category_bundle_archive() {
    assert_eq!(
        classify_category("sweep-export.tar.gz", None),
        Category::Bundles
    );
    assert_eq!(classify_category("BUNDLE.json", None), Category::Bundles);
}

#[test]
fn classify_category_logs() {
    assert_eq!(classify_category("events.jsonl", None), Category::Logs);
    assert_eq!(classify_category("agent.log", None), Category::Logs);
}

#[test]
fn classify_category_other_fallback() {
    assert_eq!(classify_category("results.json", None), Category::Other);
    assert_eq!(classify_category("manifest.json", None), Category::Other);
    assert_eq!(classify_category("run-0.patch", None), Category::Other);
}

// parse_age_selector

#[test]
fn parse_age_selector_days() {
    assert_eq!(
        parse_age_selector("7d").unwrap(),
        Duration::from_secs(7 * 86400)
    );
}

#[test]
fn parse_age_selector_hours() {
    assert_eq!(
        parse_age_selector("24h").unwrap(),
        Duration::from_secs(24 * 3600)
    );
}

#[test]
fn parse_age_selector_minutes() {
    assert_eq!(
        parse_age_selector("30m").unwrap(),
        Duration::from_secs(30 * 60)
    );
}

#[test]
fn parse_age_selector_seconds_suffix() {
    assert_eq!(parse_age_selector("45s").unwrap(), Duration::from_secs(45));
}

#[test]
fn parse_age_selector_plain_number_is_seconds() {
    assert_eq!(parse_age_selector("100").unwrap(), Duration::from_secs(100));
}

#[test]
fn parse_age_selector_rejects_garbage() {
    assert!(parse_age_selector("banana").is_err());
    assert!(parse_age_selector("").is_err());
    assert!(parse_age_selector("7x").is_err());
}

// recursive_size_walk

#[test]
fn recursive_size_walk_sums_nested_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"hello").unwrap(); // 5 bytes
    let nested = dir.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("b.txt"), b"world!").unwrap(); // 6 bytes
    assert_eq!(recursive_size_walk(dir.path()), 11);
}

#[test]
fn recursive_size_walk_empty_dir_is_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(recursive_size_walk(dir.path()), 0);
}

// evaluate_prune_candidates (pure, synthetic fixtures — no filesystem)

fn fixture_sweep(id: &str, bytes: u64, state: LifecycleState, age_secs: u64) -> SweepReport {
    SweepReport {
        id: id.to_owned(),
        path: format!("/runs/{id}"),
        total_bytes: bytes,
        lifecycle_state: state,
        last_modified: "2024-01-01T00:00:00Z".to_owned(),
        last_modified_unix: 1_704_067_200 - age_secs,
        age_seconds: age_secs,
        categories: CategoryBytes::default(),
    }
}

fn no_selectors() -> PruneSelectors {
    PruneSelectors {
        older_than_secs: None,
        keep_last: None,
        incomplete_only: false,
    }
}

#[test]
fn evaluate_prune_in_progress_never_a_candidate() {
    let sweeps = vec![fixture_sweep("live", 100, LifecycleState::InProgress, 10)];
    let eval = evaluate_prune_candidates(&sweeps, &no_selectors());
    assert!(eval.candidates.is_empty());
    assert_eq!(eval.protected.len(), 1);
    assert_eq!(eval.protected[0].id, "live");
}

#[test]
fn evaluate_prune_complete_sweep_is_plain_candidate_with_no_selectors() {
    let sweeps = vec![fixture_sweep("done", 100, LifecycleState::Complete, 10)];
    let eval = evaluate_prune_candidates(&sweeps, &no_selectors());
    assert_eq!(eval.candidates.len(), 1);
    assert!(eval.protected.is_empty());
}

#[test]
fn evaluate_prune_older_than_filters_by_age() {
    let sweeps = vec![
        fixture_sweep("old", 100, LifecycleState::Complete, 10 * 86400),
        fixture_sweep("new", 100, LifecycleState::Complete, 3600),
    ];
    let selectors = PruneSelectors {
        older_than_secs: Some(7 * 86400),
        keep_last: None,
        incomplete_only: false,
    };
    let eval = evaluate_prune_candidates(&sweeps, &selectors);
    let ids: Vec<&str> = eval.candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["old"]);
}

#[test]
fn evaluate_prune_incomplete_only_excludes_complete() {
    let sweeps = vec![
        fixture_sweep("complete", 100, LifecycleState::Complete, 100),
        fixture_sweep("interrupted", 100, LifecycleState::Interrupted, 100),
        fixture_sweep("incomplete", 100, LifecycleState::Incomplete, 100),
    ];
    let selectors = PruneSelectors {
        older_than_secs: None,
        keep_last: None,
        incomplete_only: true,
    };
    let eval = evaluate_prune_candidates(&sweeps, &selectors);
    let mut ids: Vec<&str> = eval.candidates.iter().map(|c| c.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["incomplete", "interrupted"]);
}

#[test]
fn evaluate_prune_keep_last_retains_most_recent() {
    let sweeps = vec![
        fixture_sweep("oldest", 100, LifecycleState::Complete, 300),
        fixture_sweep("middle", 100, LifecycleState::Complete, 200),
        fixture_sweep("newest", 100, LifecycleState::Complete, 10),
    ];
    let selectors = PruneSelectors {
        older_than_secs: None,
        keep_last: Some(1),
        incomplete_only: false,
    };
    let eval = evaluate_prune_candidates(&sweeps, &selectors);
    let ids: Vec<&str> = eval.candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["middle", "oldest"], "newest is retained");
    let retained_ids: Vec<&str> = eval.retained.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(retained_ids, vec!["newest"]);
}

#[test]
fn evaluate_prune_in_progress_that_matches_selectors_is_protected_not_candidate() {
    let sweeps = vec![fixture_sweep(
        "live-but-old",
        100,
        LifecycleState::InProgress,
        30 * 86400,
    )];
    let selectors = PruneSelectors {
        older_than_secs: Some(7 * 86400),
        keep_last: None,
        incomplete_only: false,
    };
    let eval = evaluate_prune_candidates(&sweeps, &selectors);
    assert!(eval.candidates.is_empty());
    assert_eq!(eval.protected.len(), 1);
}

#[test]
fn evaluate_prune_in_progress_that_does_not_match_selectors_is_neither() {
    // Fresh in-progress sweep, but --older-than 7d wouldn't have matched it
    // anyway — not a "blocked candidate", just irrelevant.
    let sweeps = vec![fixture_sweep(
        "live-fresh",
        100,
        LifecycleState::InProgress,
        60,
    )];
    let selectors = PruneSelectors {
        older_than_secs: Some(7 * 86400),
        keep_last: None,
        incomplete_only: false,
    };
    let eval = evaluate_prune_candidates(&sweeps, &selectors);
    assert!(eval.candidates.is_empty());
    assert!(
        eval.protected.is_empty(),
        "should not be flagged protected: it never matched the selector"
    );
}

// ── fixture helpers for the CLI-level integration tests ────────────────────────

fn write_file(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Backdates a file or directory's mtime. Opened read-only so this also
/// works on directories (which cannot be opened with `.write(true)`).
fn set_mtime(path: &Path, when: SystemTime) {
    let file = std::fs::OpenOptions::new().read(true).open(path).unwrap();
    file.set_modified(when).unwrap();
}

fn traj_bytes(partial: bool) -> Vec<u8> {
    let v = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.3",
        "artifact_kind": "trajectory",
        "schema_version": {"major": 1, "minor": 4},
        "info": {
            "task": "test-task",
            "model_name": "model-a",
            "outcome": "submitted",
            "steps": 1,
            "test_invocations": [],
            "partial": partial,
        },
        "messages": []
    });
    serde_json::to_vec_pretty(&v).unwrap()
}

fn write_results_json(sweep_dir: &Path) {
    let v = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 12},
        "total": 1,
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
    });
    write_file(
        &sweep_dir.join("results.json"),
        serde_json::to_vec_pretty(&v).unwrap().as_slice(),
    );
}

/// A complete sweep: `results.json` + one final trajectory.
fn make_complete_sweep(root: &Path, name: &str) -> PathBuf {
    let sweep = root.join(name);
    write_results_json(&sweep);
    write_file(
        &sweep.join("inst-1").join("run-0.traj.json"),
        &traj_bytes(false),
    );
    sweep
}

/// An interrupted sweep: only a stale partial checkpoint, no `results.json`.
fn make_interrupted_sweep(root: &Path, name: &str) -> PathBuf {
    let sweep = root.join(name);
    let traj_path = sweep.join("inst-1").join("run-0.traj.json");
    write_file(&traj_path, &traj_bytes(true));
    set_mtime(&traj_path, SystemTime::now() - Duration::from_secs(3600));
    sweep
}

/// An in-progress sweep: a partial checkpoint with a fresh (just-now) mtime.
fn make_in_progress_sweep(root: &Path, name: &str) -> PathBuf {
    make_stale_in_progress_sweep(root, name, 0)
}

/// An in-progress sweep whose checkpoint is `age_secs` old. Still classified
/// in-progress as long as `--in-progress-window` is configured >= `age_secs`.
/// Because a sweep's `last_modified` is the max mtime of its files, this is
/// the only way to construct an in-progress sweep that is also "old" enough
/// to match an `--older-than` selector.
fn make_stale_in_progress_sweep(root: &Path, name: &str, age_secs: u64) -> PathBuf {
    let sweep = root.join(name);
    let traj_path = sweep.join("inst-1").join("run-0.traj.json");
    write_file(&traj_path, &traj_bytes(true));
    set_mtime(
        &traj_path,
        SystemTime::now() - Duration::from_secs(age_secs),
    );
    sweep
}

/// An incomplete sweep: an empty directory, neither `results.json` nor any checkpoint.
fn make_incomplete_sweep(root: &Path, name: &str) -> PathBuf {
    let sweep = root.join(name);
    std::fs::create_dir_all(&sweep).unwrap();
    sweep
}

fn run_du(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(support::binary_path());
    cmd.args(["--log", "error", "bench", "du"]);
    cmd.args(args);
    cmd.output().unwrap()
}

fn run_du_json(args: &[&str]) -> serde_json::Value {
    let mut all_args: Vec<&str> = args.to_vec();
    all_args.extend_from_slice(&["--format", "json"]);
    let output = run_du(&all_args);
    assert!(
        output.status.success(),
        "bench du --format json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

// ── AC1 + AC2: per-sweep / per-category attribution, ranked, reconciles ────────

#[test]
fn report_attributes_every_byte_and_reconciles_to_recursive_walk() {
    let root = tempfile::tempdir().unwrap();
    make_complete_sweep(root.path(), "sweep-a");
    make_interrupted_sweep(root.path(), "sweep-b");
    make_incomplete_sweep(root.path(), "sweep-c");
    // A loose file directly under root — must land in `unattributed_bytes`.
    write_file(&root.path().join("stray.txt"), b"loose bytes");

    let report = run_du_json(&["--root", root.path().to_str().unwrap()]);

    let total = report["total_bytes"].as_u64().unwrap();
    let unattributed = report["unattributed_bytes"].as_u64().unwrap();
    let sweeps = report["sweeps"].as_array().unwrap();
    let sweep_sum: u64 = sweeps
        .iter()
        .map(|s| s["total_bytes"].as_u64().unwrap())
        .sum();

    assert_eq!(
        sweep_sum + unattributed,
        total,
        "per-sweep totals + unattributed must equal the on-disk total"
    );

    let walked = recursive_size_walk(root.path());
    assert_eq!(
        total, walked,
        "total_bytes must reconcile to within 0 bytes of a plain recursive size walk"
    );

    // Every category present per sweep and summing to the sweep total.
    for s in sweeps {
        let cats = &s["categories"];
        let cat_sum = cats["trajectories"].as_u64().unwrap()
            + cats["evaluation"].as_u64().unwrap()
            + cats["bundles"].as_u64().unwrap()
            + cats["partial_checkpoints"].as_u64().unwrap()
            + cats["logs"].as_u64().unwrap()
            + cats["other"].as_u64().unwrap();
        assert_eq!(cat_sum, s["total_bytes"].as_u64().unwrap());
    }
}

#[test]
fn sweeps_ranked_by_size_descending() {
    let root = tempfile::tempdir().unwrap();
    let small = root.path().join("small-sweep");
    write_file(&small.join("inst-1").join("run-0.traj.json"), b"x");
    let big = root.path().join("big-sweep");
    write_file(
        &big.join("inst-1").join("run-0.traj.json"),
        &vec![b'y'; 5000],
    );

    let report = run_du_json(&["--root", root.path().to_str().unwrap()]);
    let sweeps = report["sweeps"].as_array().unwrap();
    assert_eq!(sweeps[0]["id"], serde_json::json!("big-sweep"));
    assert_eq!(sweeps[1]["id"], serde_json::json!("small-sweep"));
}

// ── AC3: lifecycle classification end-to-end ────────────────────────────────────

#[test]
fn lifecycle_states_reported_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    make_complete_sweep(root.path(), "complete-sweep");
    make_interrupted_sweep(root.path(), "interrupted-sweep");
    make_incomplete_sweep(root.path(), "incomplete-sweep");
    make_in_progress_sweep(root.path(), "in-progress-sweep");

    let report = run_du_json(&["--root", root.path().to_str().unwrap()]);
    let mut states = std::collections::HashMap::new();
    for s in report["sweeps"].as_array().unwrap() {
        states.insert(
            s["id"].as_str().unwrap().to_owned(),
            s["lifecycle_state"].as_str().unwrap().to_owned(),
        );
    }
    assert_eq!(states["complete-sweep"], "complete");
    assert_eq!(states["interrupted-sweep"], "interrupted");
    assert_eq!(states["incomplete-sweep"], "incomplete");
    assert_eq!(states["in-progress-sweep"], "in_progress");
}

// ── AC4: --format json schema + default text table ─────────────────────────────

#[test]
fn json_format_has_schema_version_and_artifact_kind() {
    let root = tempfile::tempdir().unwrap();
    make_complete_sweep(root.path(), "sweep-a");
    let report = run_du_json(&["--root", root.path().to_str().unwrap()]);
    assert_eq!(
        report["artifact_kind"],
        serde_json::json!("disk_usage_report")
    );
    assert!(report["schema_version"]["major"].is_u64());
    assert!(report["schema_version"]["minor"].is_u64());
}

#[test]
fn text_format_is_default_and_shows_ranked_table() {
    let root = tempfile::tempdir().unwrap();
    make_complete_sweep(root.path(), "sweep-a");
    let output = run_du(&["--root", root.path().to_str().unwrap()]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("sweep-a"), "stdout:\n{stdout}");
    assert!(
        stdout.to_lowercase().contains("total"),
        "expected a total in text output\nstdout:\n{stdout}"
    );
}

#[test]
fn bad_format_is_usage_error() {
    let root = tempfile::tempdir().unwrap();
    make_complete_sweep(root.path(), "sweep-a");
    let output = run_du(&["--root", root.path().to_str().unwrap(), "--format", "xml"]);
    assert_eq!(output.status.code().unwrap(), 2);
}

// ── AC5: --prune dry-run by default (deletes nothing) ───────────────────────────

#[test]
fn prune_without_apply_deletes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let sweep = make_complete_sweep(root.path(), "old-sweep");
    set_mtime(
        &sweep.join("inst-1").join("run-0.traj.json"),
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );
    set_mtime(
        &sweep.join("results.json"),
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );

    let report = run_du_json(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--older-than",
        "7d",
    ]);

    assert!(sweep.exists(), "dry-run prune must not delete anything");
    let prune = &report["prune"];
    assert_eq!(prune["apply"], serde_json::json!(false));
    assert_eq!(prune["dry_run"], serde_json::json!(true));
    let candidates = prune["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["id"], serde_json::json!("old-sweep"));
    assert!(prune["would_reclaim_bytes"].as_u64().unwrap() > 0);
    assert_eq!(prune["reclaimed_bytes"].as_u64().unwrap(), 0);
}

// ── AC6: --apply requires --prune + a selector ──────────────────────────────────

#[test]
fn apply_without_prune_flag_is_usage_error() {
    let root = tempfile::tempdir().unwrap();
    make_complete_sweep(root.path(), "sweep-a");
    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--apply",
        "--older-than",
        "7d",
    ]);
    assert_eq!(output.status.code().unwrap(), 2);
}

#[test]
fn apply_without_any_selector_is_usage_error_and_refuses() {
    let root = tempfile::tempdir().unwrap();
    let sweep = make_complete_sweep(root.path(), "sweep-a");
    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--apply",
    ]);
    assert_eq!(output.status.code().unwrap(), 2);
    assert!(
        sweep.exists(),
        "must not delete when refusing for lack of a selector"
    );
}

// ── AC7 + AC8: in-progress sweeps are never deleted; documented non-zero exit ──

#[test]
fn apply_deletes_matching_candidates_and_reclaims_bytes() {
    let root = tempfile::tempdir().unwrap();
    let sweep = make_complete_sweep(root.path(), "old-sweep");
    set_mtime(
        &sweep.join("inst-1").join("run-0.traj.json"),
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );
    set_mtime(
        &sweep.join("results.json"),
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );

    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--apply",
        "--older-than",
        "7d",
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "expected exit 0, nothing was blocked\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!sweep.exists(), "stale sweep must be deleted by --apply");

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let prune = &report["prune"];
    assert_eq!(prune["apply"], serde_json::json!(true));
    let deleted = prune["deleted"].as_array().unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(prune["reclaimed_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn apply_never_deletes_in_progress_sweep_even_if_it_matches_selectors() {
    let root = tempfile::tempdir().unwrap();
    // 20 days old but still inside a 25-day --in-progress-window: old enough
    // to satisfy --older-than 7d, yet still provably in-progress.
    let live = make_stale_in_progress_sweep(root.path(), "live-sweep", 20 * 86400);
    let window_secs = (25_u64 * 86400).to_string();

    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--apply",
        "--older-than",
        "7d",
        "--in-progress-window",
        window_secs.as_str(),
        "--format",
        "json",
    ]);

    assert!(live.exists(), "in-progress sweep must never be deleted");
    let exit_code = output.status.code().unwrap();
    assert_ne!(exit_code, 0, "blocked apply must exit non-zero");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("outcome_class:"),
        "stderr must carry the documented outcome_class\nstderr: {stderr}"
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let prune = &report["prune"];
    assert_eq!(prune["blocked"], serde_json::json!(true));
    let protected = prune["protected"].as_array().unwrap();
    assert_eq!(protected.len(), 1);
    assert_eq!(protected[0]["id"], serde_json::json!("live-sweep"));
    let deleted = prune["deleted"].as_array().unwrap();
    assert!(deleted.is_empty());
}

#[test]
fn apply_deletes_safe_candidates_even_when_another_sweep_is_blocked() {
    let root = tempfile::tempdir().unwrap();
    let stale = make_complete_sweep(root.path(), "stale-sweep");
    set_mtime(
        &stale.join("inst-1").join("run-0.traj.json"),
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );
    set_mtime(
        &stale.join("results.json"),
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );
    let live = make_stale_in_progress_sweep(root.path(), "live-sweep", 20 * 86400);
    let window_secs = (25_u64 * 86400).to_string();

    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--apply",
        "--older-than",
        "7d",
        "--in-progress-window",
        window_secs.as_str(),
        "--format",
        "json",
    ]);

    assert!(!stale.exists(), "safe candidate must still be deleted");
    assert!(live.exists(), "in-progress sweep must be skipped");
    assert_ne!(output.status.code().unwrap(), 0);
}

// ── AC8: exit 0 on a clean report ───────────────────────────────────────────────

#[test]
fn plain_report_exits_zero_even_with_in_progress_sweep() {
    let root = tempfile::tempdir().unwrap();
    make_in_progress_sweep(root.path(), "live-sweep");
    let output = run_du(&["--root", root.path().to_str().unwrap()]);
    assert!(
        output.status.success(),
        "a plain report is not a prune request; in-progress is just informational"
    );
}

#[test]
fn prune_dry_run_with_in_progress_candidate_still_exits_zero() {
    let root = tempfile::tempdir().unwrap();
    make_in_progress_sweep(root.path(), "live-sweep");
    // No --apply: even though this sweep would be reported (or, with a
    // matching selector, listed as protected), dry-run never blocks.
    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--incomplete-only",
    ]);
    assert!(
        output.status.success(),
        "dry-run never blocks — nothing was actually going to be deleted"
    );
}

// ── keep-last / incomplete-only selectors end-to-end ────────────────────────────

#[test]
fn keep_last_selector_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let a = make_complete_sweep(root.path(), "a-oldest");
    set_mtime(
        &a.join("inst-1").join("run-0.traj.json"),
        SystemTime::now() - Duration::from_secs(300),
    );
    set_mtime(
        &a.join("results.json"),
        SystemTime::now() - Duration::from_secs(300),
    );
    let b = make_complete_sweep(root.path(), "b-newest");
    set_mtime(
        &b.join("inst-1").join("run-0.traj.json"),
        SystemTime::now() - Duration::from_secs(10),
    );
    set_mtime(
        &b.join("results.json"),
        SystemTime::now() - Duration::from_secs(10),
    );

    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--apply",
        "--keep-last",
        "1",
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    assert!(!a.exists(), "oldest sweep should be pruned");
    assert!(b.exists(), "newest sweep retained by --keep-last 1");
}

#[test]
fn incomplete_only_selector_spares_complete_sweeps() {
    let root = tempfile::tempdir().unwrap();
    let complete = make_complete_sweep(root.path(), "complete-sweep");
    set_mtime(
        &complete.join("inst-1").join("run-0.traj.json"),
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );
    set_mtime(
        &complete.join("results.json"),
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );
    let incomplete = make_incomplete_sweep(root.path(), "incomplete-sweep");
    set_mtime(
        &incomplete,
        SystemTime::now() - Duration::from_secs(30 * 86400),
    );

    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--apply",
        "--incomplete-only",
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    assert!(
        complete.exists(),
        "complete sweep spared by --incomplete-only"
    );
    assert!(!incomplete.exists(), "incomplete sweep pruned");
}

// ── Success Metric (issue #533): >=10 mixed-lifecycle sweeps, one live,
//    single invocation attributes 100% of bytes, and
//    `--prune --older-than 7d --apply` reclaims every stale sweep while
//    deleting zero in-progress sweeps (zero false reclaims). ─────────────────

#[test]
fn success_metric_ten_plus_mixed_sweeps_full_attribution_and_zero_false_reclaims() {
    let root = tempfile::tempdir().unwrap();
    // Stale sweeps sit outside the 25-day --in-progress-window used below (so
    // they classify as complete/interrupted, not in-progress); the live
    // sweep sits inside it (20d < 25d) while still being old enough (20d >=
    // 7d) to match --older-than — the only way to construct a sweep that is
    // simultaneously "old" and "provably still live" (see make_stale_in_progress_sweep).
    let stale_age = Duration::from_secs(30 * 86400);

    // 6 stale complete sweeps (older than 7d) — must all be reclaimed.
    let mut stale_complete = Vec::new();
    for i in 0..6 {
        let sweep = make_complete_sweep(root.path(), &format!("complete-stale-{i}"));
        set_mtime(
            &sweep.join("inst-1").join("run-0.traj.json"),
            SystemTime::now() - stale_age,
        );
        set_mtime(&sweep.join("results.json"), SystemTime::now() - stale_age);
        stale_complete.push(sweep);
    }
    // 2 fresh complete sweeps (younger than 7d) — must be spared.
    let mut fresh_complete = Vec::new();
    for i in 0..2 {
        let sweep = make_complete_sweep(root.path(), &format!("complete-fresh-{i}"));
        fresh_complete.push(sweep);
    }
    // 2 stale interrupted sweeps — must be reclaimed (interrupted matches --older-than).
    let mut stale_interrupted = Vec::new();
    for i in 0..2 {
        let sweep = root.path().join(format!("interrupted-stale-{i}"));
        let traj_path = sweep.join("inst-1").join("run-0.traj.json");
        write_file(&traj_path, &traj_bytes(true));
        set_mtime(&traj_path, SystemTime::now() - stale_age);
        stale_interrupted.push(sweep);
    }
    // 1 live, actively-checkpointing sweep, backdated 20 days but still
    // inside a 25-day --in-progress-window: old enough to match
    // --older-than 7d, yet must survive as the sole protected sweep.
    let live = make_stale_in_progress_sweep(root.path(), "live-sweep", 20 * 86400);

    // 12 sweeps total, mixed lifecycle state, one of them live.
    let total_sweep_count =
        stale_complete.len() + fresh_complete.len() + stale_interrupted.len() + 1;
    assert_eq!(
        total_sweep_count, 11,
        "sanity: test fixture has >= 10 sweeps"
    );

    // A single invocation attributes 100% of on-disk bytes.
    let window_secs = (25_u64 * 86400).to_string();
    let report = run_du_json(&["--root", root.path().to_str().unwrap()]);
    let total = report["total_bytes"].as_u64().unwrap();
    let unattributed = report["unattributed_bytes"].as_u64().unwrap();
    let sweeps = report["sweeps"].as_array().unwrap();
    assert_eq!(sweeps.len(), total_sweep_count);
    let sweep_sum: u64 = sweeps
        .iter()
        .map(|s| s["total_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(
        sweep_sum + unattributed,
        total,
        "0-byte discrepancy required"
    );
    assert_eq!(total, recursive_size_walk(root.path()));

    // bench du --prune --older-than 7d --apply reclaims every matching stale
    // sweep and deletes 0 in-progress sweeps.
    let output = run_du(&[
        "--root",
        root.path().to_str().unwrap(),
        "--prune",
        "--apply",
        "--older-than",
        "7d",
        "--in-progress-window",
        window_secs.as_str(),
        "--format",
        "json",
    ]);
    let prune_report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let prune = &prune_report["prune"];

    for sweep in &stale_complete {
        assert!(!sweep.exists(), "stale complete sweep must be reclaimed");
    }
    for sweep in &stale_interrupted {
        assert!(!sweep.exists(), "stale interrupted sweep must be reclaimed");
    }
    for sweep in &fresh_complete {
        assert!(sweep.exists(), "fresh complete sweep must be spared");
    }
    assert!(live.exists(), "the live sweep must never be deleted");

    assert_eq!(
        prune["deleted"].as_array().unwrap().len(),
        stale_complete.len() + stale_interrupted.len(),
        "every stale sweep reclaimed, nothing more"
    );
    let protected = prune["protected"].as_array().unwrap();
    assert_eq!(
        protected.len(),
        1,
        "zero false reclaims: exactly the live sweep is protected"
    );
    assert_eq!(protected[0]["id"], serde_json::json!("live-sweep"));
    assert_ne!(
        output.status.code().unwrap(),
        0,
        "blocked apply (a protected candidate existed) must exit non-zero"
    );
}
