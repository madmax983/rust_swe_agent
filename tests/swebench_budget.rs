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
    EXIT_REASON_BUDGET_HALT, InstanceResult, SwebenchArgs, SweepResults, estimate_cost_usd, run,
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
    let yaml = format!("environment:\n  workdir: {}\n", dir.display());
    Config::from_yaml_str(&yaml).unwrap()
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
    let per_task_cost = estimate_cost_usd(0, per_task_completion_tokens);
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
        config: cfg,
        resume: false,
        cost_limit_usd: Some(limit),
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
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
            let traj_path = output.join(format!("{}.traj.json", r.instance_id));
            let traj: Trajectory =
                serde_json::from_str(&std::fs::read_to_string(&traj_path).unwrap()).unwrap();
            assert_eq!(traj.trajectory_format, FORMAT_VERSION);
            assert_eq!(traj.info.outcome.as_deref(), Some(outcome::SUBMITTED));
            assert!(output.join(format!("{}.patch", r.instance_id)).exists());
        }
    }

    // No trajectory or patch was written for budget-halt tasks: they were
    // short-circuited before any agent code ran.
    for r in &results.instances {
        if r.exit_reason == EXIT_REASON_BUDGET_HALT {
            assert!(
                !output.join(format!("{}.traj.json", r.instance_id)).exists(),
                "budget_halt task must not write a trajectory"
            );
            assert!(
                !output.join(format!("{}.patch", r.instance_id)).exists(),
                "budget_halt task must not write a patch"
            );
        }
    }
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
        config: cfg,
        resume: false,
        cost_limit_usd: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
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
    })
    .await
    .unwrap();

    assert_eq!(results.total, 3);
    assert_eq!(results.budget_halted, 0);
    assert_eq!(results.submitted, 3);
    assert!(results.cost_limit_usd.is_none());
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
        config: cfg,
        resume: true,
        cost_limit_usd: Some(0.10),
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
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
        submitted: 1,
        skipped: 0,
        errored: 0,
        failures_by_category: std::collections::BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 8_000,
        estimated_cost_usd: estimate_cost_usd(0, 8_000),
        retries: 1,
        retried_instances: 1,
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
            completion_tokens: Some(8_000),
            duration_secs: Some(1.0),
            error: None,
            patch_present: true,
            non_empty_patch: false,
            attempts: 2,
            retry_reasons: vec![FailureCategory::ModelApi],
        }],
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
        config: cfg,
        resume: true,
        cost_limit_usd: Some(0.10),
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
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
        submitted: 0,
        skipped: 0,
        errored: 1,
        failures_by_category: std::collections::BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 10_000,
        estimated_cost_usd: estimate_cost_usd(0, 10_000),
        retries: 2,
        retried_instances: 1,
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
            completion_tokens: Some(10_000),
            duration_secs: Some(1.0),
            error: None,
            patch_present: false,
            non_empty_patch: false,
            attempts: 3,
            retry_reasons: vec![FailureCategory::ModelApi, FailureCategory::ModelApi],
        }],
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
        config: cfg,
        resume: true,
        cost_limit_usd: Some(0.10),
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
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
        submitted: 1,
        skipped: 0,
        errored: 0,
        failures_by_category: std::collections::BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 10_000,
        estimated_cost_usd: estimate_cost_usd(0, 10_000),
        retries: 2,
        retried_instances: 1,
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
            completion_tokens: Some(10_000),
            duration_secs: Some(1.0),
            error: None,
            patch_present: true,
            non_empty_patch: false,
            attempts: 3,
            retry_reasons: vec![FailureCategory::ModelApi, FailureCategory::ModelApi],
        }],
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
        config: cfg,
        resume: true,
        cost_limit_usd: Some(0.10),
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
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
    })
    .await
    .unwrap();

    assert_eq!(results.budget_halted, 0);
    assert_eq!(results.submitted, 1);
}
