//! Sweep-level cost ceiling. A small deterministic sweep with a tiny
//! budget must:
//!   * terminate cleanly (no panics, no aborted in-flight work),
//!   * record at least one task as `budget_halt`,
//!   * write a valid `results.json` whose `cost_limit_usd` and
//!     `budget_halted` fields are populated,
//!   * keep total recorded cost in `[limit, limit + (parallel - 1) * P]`,
//!     where `P` is the per-task cost ceiling. Bounded overshoot is the
//!     contract — we let the in-flight tasks finish so trajectories and
//!     `.patch` artifacts are not corrupted mid-write.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::Config;
use rust_swe_agent::ModelUsage;
use rust_swe_agent::run::swebench::{
    EXIT_REASON_BUDGET_HALT, InstanceResult, SwebenchArgs, SweepResults, estimate_cost_usd,
    patch_path_for_run, run, trajectory_path_for_run,
};
use rust_swe_agent::trajectory::{
    FORMAT_VERSION, FailureCategory, Trajectory, TrajectoryInfo, outcome,
};

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

fn submit_only_responses_for(n: usize) -> Vec<String> {
    // One scripted response per task. The agent submits on its first turn,
    // so each task makes exactly one model call before terminating.
    (0..n)
        .map(|_| "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfresh-run\n```".to_owned())
        .collect()
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
            "git {:?} failed: {}",
            args,
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

fn config_with_workdir(dir: &Path) -> Config {
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let toml = format!("[environment]\nworkdir = \"{workdir}\"\n");
    Config::from_toml_str(&toml).unwrap()
}

fn config_with_workdir_and_model(dir: &Path, model: &str) -> Config {
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let model = model.replace('"', "\\\"");
    let toml = format!("[environment]\nworkdir = \"{workdir}\"\n[model]\nname = \"{model}\"\n");
    Config::from_toml_str(&toml).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
async fn sweep_halts_when_cumulative_cost_reaches_limit() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(
        &dataset,
        &["task-a", "task-b", "task-c", "task-d", "task-e"],
    );

    // Per-task spend: 4_000 completion tokens at the sonnet output rate
    // of $15/MTok = exactly $0.06. Limit chosen so the trigger lands
    // *exactly* on the cap (cumulative at halt = $0.12 = limit), keeping
    // the post-trigger overshoot within the documented
    // `(parallel - 1) * per_task` envelope.
    let per_task_completion_tokens = 4_000u64;
    let per_task_cost = estimate_cost_usd(0, 0, 0, per_task_completion_tokens, "claude-3-5-sonnet");
    assert!((per_task_cost - 0.06).abs() < 1e-9, "got {per_task_cost}");
    let limit = 0.12;
    let parallel = 2;

    let usage = ModelUsage {
        input_tokens: 0,
        output_tokens: per_task_completion_tokens,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(per_task_cost),
    };

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel,
        reruns: 1,
        config: cfg,
        resume: false,
        cost_limit_usd: Some(limit),
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
        deterministic_responses: Some(submit_only_responses_for(5)),
        deterministic_usage_per_call: Some(usage),
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
    })
    .await
    .unwrap();

    // (a) Sweep terminated without panicking — we are here.
    // (b) At least one task is recorded as `budget_halt`.
    assert!(
        results.budget_halted >= 1,
        "expected at least one budget_halt task, got {} (results: {:?})",
        results.budget_halted,
        results
    );
    assert_eq!(results.total, 5);
    assert_eq!(
        results.submitted + results.budget_halted + results.errored,
        5,
        "every task must be accounted for once"
    );

    // budget_halt tasks are excluded from `submitted` and `errored`.
    let halted_in_instances = results
        .instances
        .iter()
        .filter(|r| r.exit_reason == EXIT_REASON_BUDGET_HALT)
        .count();
    assert_eq!(halted_in_instances, results.budget_halted);
    for r in results
        .instances
        .iter()
        .filter(|r| r.exit_reason == EXIT_REASON_BUDGET_HALT)
    {
        assert!(
            r.outcome.is_none(),
            "budget_halt instance must have outcome=None: {r:?}"
        );
        assert!(r.cost_usd.is_none());
        assert!(r.steps.is_none());
        assert!(!r.patch_present);
    }

    // (c) results.json is written and parses.
    let summary_path = output.join("results.json");
    let summary_text = std::fs::read_to_string(&summary_path).unwrap();
    let summary: serde_json::Value = serde_json::from_str(&summary_text).unwrap();
    assert_eq!(
        summary
            .get("budget_halted")
            .and_then(serde_json::Value::as_u64),
        Some(results.budget_halted as u64)
    );
    assert!(
        (summary
            .get("cost_limit_usd")
            .and_then(serde_json::Value::as_f64)
            .unwrap()
            - limit)
            .abs()
            < 1e-9
    );

    // (d) Total recorded cost is ≥ limit but bounded by
    //     limit + (parallel - 1) * per_task_cost.
    let recorded_cost: f64 = results.instances.iter().filter_map(|r| r.cost_usd).sum();
    let bound_overshoot = (parallel as f64 - 1.0) * per_task_cost;
    assert!(
        recorded_cost >= limit,
        "recorded {recorded_cost} should be ≥ limit {limit}"
    );
    assert!(
        recorded_cost <= limit + bound_overshoot + 1e-9,
        "recorded {recorded_cost} exceeded budget bound \
         (limit {limit} + (parallel-1)*per_task {bound_overshoot})"
    );

    // The summary table mentions the BUDGET HALT line and the limit.
    let table = results.summary_table();
    assert!(
        table.contains(&format!("Sweep cost limit:   ${limit:.4}")),
        "missing limit row in summary: {table}"
    );
    assert!(
        table.contains("BUDGET HALT at"),
        "missing BUDGET HALT line in summary: {table}"
    );

    // Trajectories of the in-flight tasks that completed after halt
    // remain valid, fully-formed records (same schema as a normal run).
    for r in &results.instances {
        if r.outcome.as_deref() == Some(outcome::SUBMITTED) {
            let traj_path = trajectory_path_for_run(&output, &r.instance_id, 1);
            let traj: Trajectory =
                serde_json::from_str(&std::fs::read_to_string(&traj_path).unwrap()).unwrap();
            assert_eq!(traj.trajectory_format, FORMAT_VERSION);
            assert_eq!(traj.info.outcome.as_deref(), Some(outcome::SUBMITTED));
            assert!(patch_path_for_run(&output, &r.instance_id, 1).exists());
        }
    }

    // No trajectory or patch was written for budget-halt tasks: they were
    // short-circuited before any agent code ran.
    for r in &results.instances {
        if r.exit_reason == EXIT_REASON_BUDGET_HALT {
            assert!(
                !trajectory_path_for_run(&output, &r.instance_id, 1).exists(),
                "budget_halt task must not write a trajectory"
            );
            assert!(
                !patch_path_for_run(&output, &r.instance_id, 1).exists(),
                "budget_halt task must not write a patch"
            );
        }
    }
}

#[tokio::test]
async fn zero_stored_cost_still_trips_budget_from_tokens() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["priced-a", "priced-b"]);

    let per_task_cost = estimate_cost_usd(100_000, 0, 0, 100_000, "openai/gpt-4o-mini");
    let usage = ModelUsage {
        input_tokens: 100_000,
        output_tokens: 100_000,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(0.0),
    };

    let cfg = config_with_workdir_and_model(&repo, "openai/gpt-4o-mini");
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output,
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: false,
        cost_limit_usd: Some(per_task_cost),
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
        deterministic_responses: Some(submit_only_responses_for(2)),
        deterministic_usage_per_call: Some(usage),
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
    })
    .await
    .unwrap();

    assert_eq!(results.submitted, 1, "results: {results:?}");
    assert_eq!(results.budget_halted, 1, "results: {results:?}");
    assert!(
        (results.estimated_cost_usd - per_task_cost).abs() < 1e-9,
        "got {} expected {}",
        results.estimated_cost_usd,
        per_task_cost
    );
    assert!(
        results.instances[0]
            .effective_cost_usd(Some("openai/gpt-4o-mini"))
            .unwrap_or_default()
            > 0.0,
        "instance cost should fall back from zero stored cost"
    );
}

#[tokio::test]
async fn sweep_without_limit_runs_all_tasks() {
    // Sanity: when `cost_limit_usd` is `None`, behavior is unchanged —
    // every task runs even when per-call usage would have crossed any
    // small budget.
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["a", "b", "c"]);

    let usage = ModelUsage {
        input_tokens: 0,
        output_tokens: 4_000,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(0.06),
    };

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 2,
        reruns: 1,
        config: cfg,
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
        deterministic_responses: Some(submit_only_responses_for(3)),
        deterministic_usage_per_call: Some(usage),
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
    })
    .await
    .unwrap();

    assert_eq!(results.total, 3);
    assert_eq!(results.budget_halted, 0);
    assert_eq!(results.submitted, 3, "results: {results:?}");
    assert!(results.cost_limit_usd.is_none());
}

#[tokio::test]
#[allow(clippy::cast_precision_loss)]
async fn cached_sweep_cost_stays_within_ten_percent_of_anthropic_oracle() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["cache-a", "cache-b"]);

    let usage = ModelUsage {
        input_tokens: 100_000,
        output_tokens: 25_000,
        cache_read_tokens: 900_000,
        cache_creation_tokens: 50_000,
        cost_usd: None,
    };

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
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
        deterministic_responses: Some(submit_only_responses_for(2)),
        deterministic_usage_per_call: Some(usage),
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
    })
    .await
    .unwrap();

    let expected_total =
        2.0 * estimate_cost_usd(100_000, 900_000, 50_000, 25_000, "claude-3-5-sonnet");
    let relative_error = ((results.estimated_cost_usd - expected_total) / expected_total).abs();
    assert!(
        relative_error <= 0.10,
        "reported total_cost_usd={} diverged from oracle {} by {:.2}%",
        results.estimated_cost_usd,
        expected_total,
        relative_error * 100.0
    );
    assert!(
        results.cache_hit_rate >= 0.8,
        "expected cache-heavy fixture, got hit rate {}",
        results.cache_hit_rate
    );
    assert_eq!(results.total_prompt_tokens, 200_000);
    assert_eq!(results.total_cache_read_tokens, 1_800_000);
    assert_eq!(results.total_cache_creation_tokens, 100_000);
    assert_eq!(results.total_completion_tokens, 50_000);

    let summary_path = output.join("results.json");
    let summary: SweepResults =
        serde_json::from_str(&std::fs::read_to_string(summary_path).unwrap()).unwrap();
    let relative_error = ((summary.estimated_cost_usd - expected_total) / expected_total).abs();
    assert!(
        relative_error <= 0.10,
        "serialized total_cost_usd={} diverged from oracle {} by {:.2}%",
        summary.estimated_cost_usd,
        expected_total,
        relative_error * 100.0
    );
    assert!(summary.cache_hit_rate >= 0.8);
}

#[tokio::test]
async fn resume_skipped_costs_count_against_budget() {
    // A resumed sweep already on disk should be billed at its stored
    // cost: re-summing only freshly-run tasks would let an operator
    // unwittingly overshoot the cap by re-running into a tight budget.
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["already-on-disk", "fresh-1", "fresh-2"]);

    // Pre-populate `already-on-disk` with a trajectory whose stored
    // token counts imply a $0.06 prior cost. Patch file present so the
    // resume short-circuit takes the skip path.
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info: TrajectoryInfo {
            outcome: Some(outcome::SUBMITTED.into()),
            exit_reason: Some("submitted".into()),
            steps: Some(1),
            token_usage: Some(rust_swe_agent::trajectory::TokenUsage {
                prompt_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                completion_tokens: 4_000,
            }),
            total_cost_usd: Some(0.06),
            ..Default::default()
        },
        messages: vec![],
    };
    std::fs::write(
        output.join("already-on-disk.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
    std::fs::write(output.join("already-on-disk.patch"), b"").unwrap();

    let usage = ModelUsage {
        input_tokens: 0,
        output_tokens: 4_000,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(0.06),
    };

    // Limit = $0.10; the on-disk task already costs $0.06. After one
    // fresh task finishes ($0.12 cumulative), halt fires; the second
    // fresh task either also runs (in-flight) or short-circuits to
    // budget_halt depending on permit timing. Either way, *something*
    // must halt — otherwise the resume case is broken.
    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
        cost_limit_usd: Some(0.10),
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
        deterministic_responses: Some(submit_only_responses_for(2)),
        deterministic_usage_per_call: Some(usage),
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
    })
    .await
    .unwrap();

    assert_eq!(results.total, 3);
    assert_eq!(results.skipped, 1);
    assert!(
        results.budget_halted >= 1,
        "resume-skipped cost should have driven cumulative past the limit, \
         halting at least one fresh task; got: {results:?}"
    );
}

#[tokio::test]
async fn resume_uses_prior_results_token_totals_for_budget_accounting() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["already-on-disk", "fresh"]);

    // Trajectory only reflects a terminal attempt with small usage.
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info: TrajectoryInfo {
            outcome: Some(outcome::SUBMITTED.into()),
            exit_reason: Some("submitted".into()),
            steps: Some(1),
            token_usage: Some(rust_swe_agent::trajectory::TokenUsage {
                prompt_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                completion_tokens: 1_000,
            }),
            total_cost_usd: Some(0.015),
            ..Default::default()
        },
        messages: vec![],
    };
    std::fs::write(
        output.join("already-on-disk.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
    std::fs::write(output.join("already-on-disk.patch"), b"").unwrap();

    // Prior results.json preserves cumulative retry usage/costs.
    let prior = SweepResults {
        total: 1,
        sweep_status: rust_swe_agent::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 1,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: std::collections::BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 8_000,
        estimated_cost_usd: estimate_cost_usd(0, 0, 0, 8_000, "claude-3-5-sonnet"),
        cache_hit_rate: 0.0,
        retries: 1,
        retried_instances: 1,
        pass_at_k: 0.0,
        filter_spec: rust_swe_agent::run::swebench::FilterSpec::default(),
        manifest: None,
        cost_limit_usd: Some(0.10),
        instances: vec![InstanceResult {
            instance_id: "already-on-disk".into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: Some(1),
            cost_usd: Some(0.12),
            prompt_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(8_000),
            duration_secs: Some(1.0),
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: false,
            attempts: 2,
            retry_reasons: vec![FailureCategory::ModelApi],
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,
        }],
        rate_limit_events: None,
    };
    std::fs::write(
        output.join("results.json"),
        serde_json::to_string_pretty(&prior).unwrap(),
    )
    .unwrap();

    let usage = ModelUsage {
        input_tokens: 0,
        output_tokens: 4_000,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(0.06),
    };

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
        cost_limit_usd: Some(0.10),
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
        deterministic_responses: Some(submit_only_responses_for(1)),
        deterministic_usage_per_call: Some(usage),
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
    })
    .await
    .unwrap();

    assert_eq!(results.skipped, 1);
    assert_eq!(results.budget_halted, 1);
    assert!(
        results
            .instances
            .iter()
            .any(|r| r.instance_id == "fresh" && r.exit_reason == EXIT_REASON_BUDGET_HALT)
    );
}

#[tokio::test]
async fn retry_on_resume_instances_are_precharged_before_rerun() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["retryable", "fresh"]);

    let retryable_traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info: TrajectoryInfo {
            outcome: Some(outcome::ERROR.into()),
            exit_reason: Some("error".into()),
            failure_category: Some(FailureCategory::ModelApi),
            token_usage: Some(rust_swe_agent::trajectory::TokenUsage {
                prompt_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                completion_tokens: 100,
            }),
            ..Default::default()
        },
        messages: vec![],
    };
    std::fs::write(
        output.join("retryable.traj.json"),
        serde_json::to_string_pretty(&retryable_traj).unwrap(),
    )
    .unwrap();

    let prior = SweepResults {
        total: 1,
        sweep_status: rust_swe_agent::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 0,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 1,
        failures_by_category: std::collections::BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 10_000,
        estimated_cost_usd: estimate_cost_usd(0, 0, 0, 10_000, "claude-3-5-sonnet"),
        cache_hit_rate: 0.0,
        retries: 2,
        retried_instances: 1,
        pass_at_k: 0.0,
        filter_spec: rust_swe_agent::run::swebench::FilterSpec::default(),
        manifest: None,
        cost_limit_usd: Some(0.10),
        instances: vec![InstanceResult {
            instance_id: "retryable".into(),
            exit_reason: "error".into(),
            outcome: Some(outcome::ERROR.into()),
            failure_category: Some(FailureCategory::ModelApi),
            steps: None,
            cost_usd: Some(0.15),
            prompt_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(10_000),
            duration_secs: Some(1.0),
            error: None,
            github_pr_error: None,
            patch_present: false,
            non_empty_patch: false,
            attempts: 3,
            retry_reasons: vec![FailureCategory::ModelApi, FailureCategory::ModelApi],
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,
        }],
        rate_limit_events: None,
    };
    std::fs::write(
        output.join("results.json"),
        serde_json::to_string_pretty(&prior).unwrap(),
    )
    .unwrap();

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output,
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
        cost_limit_usd: Some(0.10),
        task_timeout_secs: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: rust_swe_agent::run::swebench::StratifyMode::Proportional,
        max_retries: 1,
        retry_on: Some("model_api".into()),
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: true,
        deterministic_responses: Some(submit_only_responses_for(1)),
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
    })
    .await
    .unwrap();

    assert_eq!(results.budget_halted, 2);
    assert_eq!(results.submitted, 0);
    assert!(
        results
            .instances
            .iter()
            .any(|r| r.instance_id == "retryable" && r.exit_reason == EXIT_REASON_BUDGET_HALT)
    );
}

#[tokio::test]
async fn stale_results_json_is_not_trusted_over_newer_trajectory() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["already-on-disk", "fresh"]);

    // Write stale summary first (older mtime) with inflated usage.
    let stale_summary = SweepResults {
        total: 1,
        sweep_status: rust_swe_agent::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 1,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: std::collections::BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 10_000,
        estimated_cost_usd: estimate_cost_usd(0, 0, 0, 10_000, "claude-3-5-sonnet"),
        cache_hit_rate: 0.0,
        retries: 2,
        retried_instances: 1,
        pass_at_k: 0.0,
        filter_spec: rust_swe_agent::run::swebench::FilterSpec::default(),
        manifest: None,
        cost_limit_usd: Some(0.10),
        instances: vec![InstanceResult {
            instance_id: "already-on-disk".into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: Some(2),
            cost_usd: Some(0.15),
            prompt_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(10_000),
            duration_secs: Some(1.0),
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: false,
            attempts: 3,
            retry_reasons: vec![FailureCategory::ModelApi, FailureCategory::ModelApi],
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,
        }],
        rate_limit_events: None,
    };
    std::fs::write(
        output.join("results.json"),
        serde_json::to_string_pretty(&stale_summary).unwrap(),
    )
    .unwrap();

    // Newer trajectory is source-of-truth and has small token usage.
    std::thread::sleep(std::time::Duration::from_millis(15));
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info: TrajectoryInfo {
            outcome: Some(outcome::SUBMITTED.into()),
            exit_reason: Some("submitted".into()),
            steps: Some(1),
            token_usage: Some(rust_swe_agent::trajectory::TokenUsage {
                prompt_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                completion_tokens: 1_000,
            }),
            total_cost_usd: Some(0.015),
            ..Default::default()
        },
        messages: vec![],
    };
    std::fs::write(
        output.join("already-on-disk.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
    std::fs::write(output.join("already-on-disk.patch"), b"").unwrap();

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output,
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
        cost_limit_usd: Some(0.10),
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
        deterministic_responses: Some(submit_only_responses_for(1)),
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
    })
    .await
    .unwrap();

    assert_eq!(results.budget_halted, 0);
    assert_eq!(results.submitted, 1, "results: {results:?}");
}

// ── Per-task budget integration tests (Issue #50) ─────────────────────────

fn config_with_workdir_and_per_task_budget(dir: &Path, budget_usd: f64) -> Config {
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let toml = format!(
        "[environment]\nworkdir = \"{workdir}\"\n\
         [agent]\nper_task_budget_usd = {budget_usd}\n"
    );
    Config::from_toml_str(&toml).unwrap()
}

#[tokio::test]
async fn per_task_budget_terminates_task_with_budget_exhausted_category() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["task-a", "task-b"]);

    // Each task costs $0.10 per model call; per-task budget is $0.05
    // so the second step should see $0.10 >= $0.05 and terminate.
    let per_call_cost = 0.10f64;
    let per_task_budget = 0.05f64;

    let usage = ModelUsage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(per_call_cost),
    };

    let cfg = config_with_workdir_and_per_task_budget(&repo, per_task_budget);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
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
        deterministic_responses: Some(vec![
            "```bash\necho step1\n```".to_owned(),
            "```bash\necho step2\n```".to_owned(),
            "```bash\necho step3\n```".to_owned(),
            "```bash\necho step4\n```".to_owned(),
        ]),
        deterministic_usage_per_call: Some(usage),
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
    })
    .await
    .unwrap();

    assert_eq!(results.total, 2);
    // Both tasks should be terminated by per-task budget (not submitted)
    assert_eq!(
        results.submitted, 0,
        "no task should have submitted: {results:?}"
    );
    // Budget-exhausted is a resource-limit termination, not counted in errored
    // (consistent with step_limit_reached). Tasks are visible in failures_by_category.
    assert_eq!(
        results.errored, 0,
        "budget_exhausted should not appear in errored: {results:?}"
    );

    // At least one trajectory should have failure_category=budget_exhausted
    let budget_exhausted_count = results
        .instances
        .iter()
        .filter(|r| r.failure_category == Some(FailureCategory::BudgetExhausted))
        .count();
    assert!(
        budget_exhausted_count >= 1,
        "expected at least one budget_exhausted task, got: {results:?}"
    );

    // Summary table should surface per-task budget kills
    let table = results.summary_table();
    assert!(
        table.contains("budget_exhausted") || table.contains("Budget-exhausted"),
        "summary table should mention budget_exhausted; got:\n{table}"
    );
}

#[tokio::test]
async fn per_task_budget_absent_means_no_enforcement() {
    // Sanity check: when per_task_budget_usd is not set, tasks run normally
    // even if each call would exceed a hypothetical limit.
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["task-a"]);

    let usage = ModelUsage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(999.0), // enormous per-call cost, but no cap
    };

    let cfg = config_with_workdir(&repo); // no per_task_budget_usd
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output,
        parallel: 1,
        reruns: 1,
        config: cfg,
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
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".to_owned(),
        ]),
        deterministic_usage_per_call: Some(usage),
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
    })
    .await
    .unwrap();

    assert_eq!(
        results.submitted, 1,
        "task should submit without a per-task cap: {results:?}"
    );
}
