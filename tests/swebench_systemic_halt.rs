//! Circuit-breaker tests: "halt sweeps early on systemic failure".
//!
//! These tests exercise the `--abort-on-systemic-failure` feature end-to-end,
//! using the in-process `run()` API with scripted model responses so no
//! network or Docker access is needed.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::artifact::ArtifactKind;
use rust_swe_agent::run::swebench::{
    SWEEP_STATUS_COMPLETED, SWEEP_STATUS_SYSTEMIC_HALT, SwebenchArgs, SweepHaltReport,
    SweepResults, circuit_breaker::CircuitBreaker, run,
};
use rust_swe_agent::trajectory::FailureCategory;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn write_dataset(path: &Path, instance_ids: &[&str]) {
    let mut s = String::new();
    for id in instance_ids {
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\"}}"
        );
    }
    std::fs::write(path, s).unwrap();
}

fn init_repo(dir: &Path) {
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@test"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
}

fn config_with_workdir(dir: &Path) -> rust_swe_agent::Config {
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    rust_swe_agent::Config::from_toml_str(&format!("[environment]\nworkdir = \"{workdir}\"\n"))
        .unwrap()
}

/// Responses that immediately fail with ResponsesExhausted → ModelApi.
fn fail_responses() -> Vec<String> {
    vec![]
}

fn base_args(
    dataset: std::path::PathBuf,
    output: std::path::PathBuf,
    repo: &Path,
    responses: Vec<String>,
) -> SwebenchArgs {
    SwebenchArgs {
        dataset_source: rust_swe_agent::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
        output_dir: output,
        parallel: 1,
        reruns: 1,
        config: config_with_workdir(repo),
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: rust_swe_agent::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(responses),
        deterministic_usage_per_call: None,
        config_overlay_paths: Vec::new(),
        dry_run: false,
        skip_preflight: true,
        preflight_format: "text".into(),
        skip_model_probe: true,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "test".into(),
        skip_patch_validation: true,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: 30,
        install_os_signal_handlers: false,
        cancellation_signals: None,
        github_pr: None,
        reproduced_from: None,
        abort_on_systemic_failure: true,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the CircuitBreaker decision logic
// ---------------------------------------------------------------------------

#[test]
fn circuit_breaker_does_not_trip_when_disabled() {
    let cb = CircuitBreaker::new(false, 5, 80);
    let completions = vec![
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
    ];
    assert!(cb.check(&completions).is_none());
}

#[test]
fn circuit_breaker_does_not_trip_below_min_samples() {
    let cb = CircuitBreaker::new(true, 5, 80);
    let completions = vec![
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
    ];
    // Only 4 completed, min is 5.
    assert!(cb.check(&completions).is_none());
}

#[test]
fn circuit_breaker_does_not_trip_without_dominant_actionable_category() {
    let cb = CircuitBreaker::new(true, 5, 80);
    // 3 ModelApi + 2 EnvSetup = neither reaches 80%.
    let completions = vec![
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::EnvSetup), true),
        (Some(FailureCategory::EnvSetup), true),
    ];
    assert!(cb.check(&completions).is_none());
}

#[test]
fn circuit_breaker_does_not_trip_on_non_actionable_dominant_category() {
    let cb = CircuitBreaker::new(true, 5, 80);
    // StepLimit is not actionable — should never trip even at 100%.
    let completions = vec![
        (Some(FailureCategory::StepLimit), true),
        (Some(FailureCategory::StepLimit), true),
        (Some(FailureCategory::StepLimit), true),
        (Some(FailureCategory::StepLimit), true),
        (Some(FailureCategory::StepLimit), true),
    ];
    assert!(cb.check(&completions).is_none());
}

#[test]
fn circuit_breaker_trips_on_actionable_dominant_category_at_exact_threshold() {
    let cb = CircuitBreaker::new(true, 5, 80);
    // 4 out of 5 = 80% ModelApi (exactly at threshold).
    let completions = vec![
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::StepLimit), true),
    ];
    let result = cb.check(&completions);
    assert_eq!(result, Some(FailureCategory::ModelApi));
}

#[test]
fn circuit_breaker_trips_on_env_setup_dominant() {
    let cb = CircuitBreaker::new(true, 5, 80);
    let completions = vec![
        (Some(FailureCategory::EnvSetup), true),
        (Some(FailureCategory::EnvSetup), true),
        (Some(FailureCategory::EnvSetup), true),
        (Some(FailureCategory::EnvSetup), true),
        (Some(FailureCategory::EnvSetup), true),
    ];
    let result = cb.check(&completions);
    assert_eq!(result, Some(FailureCategory::EnvSetup));
}

#[test]
fn circuit_breaker_does_not_trip_when_successes_dilute_share_below_threshold() {
    // 5 ModelApi failures + 5 successes (None) = 10 completed, 50% share.
    // 50% < 80% threshold — must NOT trip even though min_samples is met.
    let cb = CircuitBreaker::new(true, 5, 80);
    let completions = vec![
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (None, true), // success
        (None, true),
        (None, true),
        (None, true),
        (None, true),
    ];
    assert!(
        cb.check(&completions).is_none(),
        "5/10 = 50% should not trip the 80% threshold"
    );
}

#[test]
fn circuit_breaker_trips_when_failures_dominate_mixed_completions() {
    // 8 ModelApi failures + 2 successes = 10 completed, 80% share — exactly at threshold.
    let cb = CircuitBreaker::new(true, 5, 80);
    let completions = vec![
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (None, true), // success
        (None, true),
    ];
    assert_eq!(
        cb.check(&completions),
        Some(FailureCategory::ModelApi),
        "8/10 = 80% should trip the 80% threshold"
    );
}

#[test]
fn circuit_breaker_is_deterministic() {
    let cb = CircuitBreaker::new(true, 5, 80);
    let completions = vec![
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
        (Some(FailureCategory::ModelApi), true),
    ];
    let first = cb.check(&completions);
    let second = cb.check(&completions);
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// FailureCategory::is_actionable unit tests
// ---------------------------------------------------------------------------

#[test]
fn failure_category_is_actionable_env_setup() {
    assert!(FailureCategory::EnvSetup.is_actionable());
}

#[test]
fn failure_category_is_actionable_model_api() {
    assert!(FailureCategory::ModelApi.is_actionable());
}

#[test]
fn failure_category_not_actionable_step_limit() {
    assert!(!FailureCategory::StepLimit.is_actionable());
}

#[test]
fn failure_category_not_actionable_patch_invalid() {
    assert!(!FailureCategory::PatchApplyInvalid.is_actionable());
}

#[test]
fn failure_category_not_actionable_patch_empty() {
    assert!(!FailureCategory::PatchEmpty.is_actionable());
}

#[test]
fn failure_category_not_actionable_cost_limit() {
    assert!(!FailureCategory::CostLimit.is_actionable());
}

#[test]
fn failure_category_not_actionable_budget_exhausted() {
    assert!(!FailureCategory::BudgetExhausted.is_actionable());
}

#[test]
fn failure_category_not_actionable_wallclock_timeout() {
    assert!(!FailureCategory::WallclockTimeout.is_actionable());
}

// ---------------------------------------------------------------------------
// Integration tests: full sweep run
// ---------------------------------------------------------------------------

/// Happy path: all instances fail with ModelApi → breaker trips at exactly
/// N=5 samples, leaving the remaining instances unstarted.
#[tokio::test]
async fn systemic_halt_trips_at_min_samples_with_dominant_actionable_category() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");

    let ids: Vec<_> = (1..=10).map(|i| format!("task-{i:02}")).collect();
    let id_refs: Vec<_> = ids.iter().map(String::as_str).collect();
    write_dataset(&dataset, &id_refs);

    let mut args = base_args(dataset, output.clone(), &repo, fail_responses());
    args.systemic_failure_min_samples = 5;
    args.systemic_failure_share_pct = 80;

    let results = run(args).await.unwrap();

    assert_eq!(
        results.sweep_status, SWEEP_STATUS_SYSTEMIC_HALT,
        "expected systemic_halt status, got: {}",
        results.sweep_status
    );
    // At trip point: 5 errored (ModelApi), 5 not started.
    assert_eq!(results.errored, 5, "expected exactly 5 errored at trip");
    assert!(
        results.not_started >= 5,
        "expected ≥5 instances not started, got {}",
        results.not_started
    );
    // The dominant category must be recorded.
    assert_eq!(
        results.systemic_halt_category,
        Some(FailureCategory::ModelApi)
    );
    // halt-report.json must exist and parse.
    let halt_report_path = output.join("halt-report.json");
    assert!(
        halt_report_path.exists(),
        "halt-report.json must be written when breaker trips"
    );
    let raw = std::fs::read_to_string(&halt_report_path).unwrap();
    let report: SweepHaltReport = {
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Verify artifact_kind header.
        assert_eq!(
            v["artifact_kind"].as_str().unwrap(),
            ArtifactKind::SweepHaltReport.label()
        );
        serde_json::from_value(v).unwrap()
    };
    assert_eq!(report.dominant_failure_category, FailureCategory::ModelApi);
    assert_eq!(report.sample_size, 5);
    assert!(report.share_pct >= 80.0);
    assert!(!report.first_failing_instance_ids.is_empty());
    assert!(report.first_failing_instance_ids.len() <= 3);
    assert!(!report.next_step.is_empty());
    assert!(!report.trip_reason.is_empty());
}

/// Below min_samples: all instances fail but the sweep has fewer instances
/// than the minimum sample threshold. Breaker should NOT trip.
#[tokio::test]
async fn systemic_halt_does_not_trip_when_total_below_min_samples() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");

    // Only 4 instances — below the default min_samples=5.
    write_dataset(&dataset, &["a", "b", "c", "d"]);

    let mut args = base_args(dataset, output, &repo, fail_responses());
    args.systemic_failure_min_samples = 5;

    let results = run(args).await.unwrap();

    assert_eq!(
        results.sweep_status, SWEEP_STATUS_COMPLETED,
        "breaker must not trip below min_samples; got: {}",
        results.sweep_status
    );
    assert_eq!(results.errored, 4);
    assert!(results.systemic_halt_category.is_none());
}

/// Breaker disabled via flag: even with 100% ModelApi failures across 10
/// instances, the sweep must run to completion.
#[tokio::test]
async fn systemic_halt_disabled_flag_lets_all_instances_run() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");

    let ids: Vec<_> = (1..=8).map(|i| format!("task-{i:02}")).collect();
    let id_refs: Vec<_> = ids.iter().map(String::as_str).collect();
    write_dataset(&dataset, &id_refs);

    let mut args = base_args(dataset, output.clone(), &repo, fail_responses());
    args.abort_on_systemic_failure = false;

    let results = run(args).await.unwrap();

    assert_eq!(
        results.sweep_status, SWEEP_STATUS_COMPLETED,
        "breaker disabled: expected completed, got: {}",
        results.sweep_status
    );
    assert_eq!(results.errored, 8, "all 8 should run to completion");
    assert!(results.systemic_halt_category.is_none());
    // No halt-report.json should be written.
    assert!(
        !output.join("halt-report.json").exists(),
        "halt-report.json must not be written when breaker is disabled"
    );
}

/// results.json is valid and complete even when the breaker trips mid-sweep.
#[tokio::test]
async fn systemic_halt_writes_valid_results_json_on_trip() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");

    let ids: Vec<_> = (1..=10).map(|i| format!("task-{i:02}")).collect();
    let id_refs: Vec<_> = ids.iter().map(String::as_str).collect();
    write_dataset(&dataset, &id_refs);

    let mut args = base_args(dataset, output.clone(), &repo, fail_responses());
    args.systemic_failure_min_samples = 5;

    let _ = run(args).await.unwrap();

    let results_json = output.join("results.json");
    assert!(results_json.exists());
    let raw = std::fs::read_to_string(&results_json).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // Must have valid artifact header.
    assert_eq!(v["artifact_kind"].as_str().unwrap(), "sweep_results");
    // sweep_status must be systemic_halt.
    assert_eq!(
        v["sweep_status"].as_str().unwrap(),
        SWEEP_STATUS_SYSTEMIC_HALT
    );
    // total is accurate.
    assert_eq!(v["total"].as_u64().unwrap(), 10);
}

/// Breaker decision is deterministic: two identical sweeps with the same
/// responses produce the same trip point.
#[tokio::test]
async fn systemic_halt_is_deterministic_across_runs() {
    async fn do_run(
        dataset: std::path::PathBuf,
        output: std::path::PathBuf,
        repo: &Path,
    ) -> (String, usize, usize) {
        let mut args = base_args(dataset, output, repo, fail_responses());
        args.systemic_failure_min_samples = 5;
        args.systemic_failure_share_pct = 80;
        let r = run(args).await.unwrap();
        (r.sweep_status, r.errored, r.not_started)
    }

    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let ids: Vec<_> = (1..=10).map(|i| format!("task-{i:02}")).collect();
    let id_refs: Vec<_> = ids.iter().map(String::as_str).collect();

    let d1 = work.path().join("dataset1.jsonl");
    let o1 = work.path().join("runs1");
    write_dataset(&d1, &id_refs);

    let d2 = work.path().join("dataset2.jsonl");
    let o2 = work.path().join("runs2");
    write_dataset(&d2, &id_refs);

    let r1 = do_run(d1, o1, &repo).await;
    let r2 = do_run(d2, o2, &repo).await;

    assert_eq!(
        r1, r2,
        "circuit breaker must produce identical results for identical input"
    );
}

/// The `bench tail` summary table surfaces a "circuit breaker" line when the
/// sweep status is systemic_halt.
#[test]
fn sweep_results_summary_table_shows_circuit_breaker_status() {
    let mut results = SweepResults::default();
    results.total = 10;
    results.sweep_status = SWEEP_STATUS_SYSTEMIC_HALT.into();
    results.errored = 5;
    results.not_started = 5;
    results.systemic_halt_category = Some(FailureCategory::ModelApi);

    let table = results.summary_table();
    assert!(
        table.contains("systemic") || table.contains("circuit breaker") || table.contains("halt"),
        "summary table must surface systemic halt status; got:\n{table}"
    );
}
