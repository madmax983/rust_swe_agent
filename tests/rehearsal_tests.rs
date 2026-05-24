//! Integration tests for bench rehearsal subcommand and TDD workflow (issue #282).

#![recursion_limit = "256"]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::uninlined_format_args,
    clippy::needless_borrows_for_generic_args,
    clippy::manual_string_new,
    clippy::large_futures
)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use maxwells_daemon::Config;
use maxwells_daemon::run::dataset::DatasetSource;
use maxwells_daemon::run::swebench::{SwebenchArgs, run};

// ── helpers ────────────────────────────────────────────────────────────────

fn write_jsonl(path: &Path, ids: &[&str], patches: &[&str]) {
    let mut s = String::new();
    for (id, patch) in ids.iter().zip(patches.iter()) {
        let escaped_patch = patch.replace('\n', "\\n").replace('"', "\\\"");
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\",\"patch\":\"{escaped_patch}\"}}"
        );
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, s).unwrap();
}

fn base_rehearsal_args(dataset_source: DatasetSource, output_dir: PathBuf) -> SwebenchArgs {
    SwebenchArgs {
        dataset_source,
        dataset_cache_dir: PathBuf::from("/nonexistent-cache"),
        output_dir,
        parallel: 2,
        config: Config::defaults().unwrap(),
        reruns: 1,
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: maxwells_daemon::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        config_overlay_paths: vec![],
        dry_run: false,
        skip_preflight: true,
        preflight_format: "text".into(),
        skip_model_probe: true,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "sweep".into(),
        skip_patch_validation: true,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: 5,
        install_os_signal_handlers: false,
        cancellation_signals: None,
        github_pr: None,
        reproduced_from: None,
        abort_on_systemic_failure: true,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
        otlp_endpoint: None,
        rehearse: true,
        skip_evaluator: false,
        eval_backend: "rehearsal".to_string(),
        sb_subset: None,
        sb_split: None,
        eval_timeout_secs: None,
        event_log: None,
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_gold_shadow_verbatim_patch() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_jsonl(
        &dataset,
        &["inst-1", "inst-2"],
        &["patch content 1\n", "patch content 2\n"],
    );

    let output = work.path().join("out");
    let args = base_rehearsal_args(DatasetSource::LocalPath(dataset), output.clone());
    let results = run(args).await.unwrap();
    println!("DEBUG RESULTS: {:?}", results);
    assert_eq!(results.total, 2);
    assert_eq!(results.submitted, 2);

    // Assert traj.json exists and has rehearsal mode/zero cost
    let traj_path = output.join("inst-1").join("run-1.traj.json");
    assert!(traj_path.exists());
    let traj_content = std::fs::read_to_string(&traj_path).unwrap();
    println!("DEBUG TRAJ CONTENT: {}", traj_content);
    let traj: serde_json::Value = serde_json::from_str(&traj_content).unwrap();

    assert_eq!(traj["info"]["mode"], "rehearsal");
    assert_eq!(traj["info"]["total_cost_usd"], 0.0);

    let traj_res = serde_json::from_str::<maxwells_daemon::Trajectory>(&traj_content);
    println!("TRAJ DE SERIALIZE ERROR: {:?}", traj_res.err());

    // Assert patch is written verbatim
    let patch_path1 = output.join("inst-1").join("run-1.patch");
    assert_eq!(
        std::fs::read_to_string(patch_path1).unwrap(),
        "patch content 1\n"
    );

    let patch_path2 = output.join("inst-2").join("run-1.patch");
    assert_eq!(
        std::fs::read_to_string(patch_path2).unwrap(),
        "patch content 2\n"
    );
}

#[tokio::test]
async fn test_rehearsal_byte_stable_reproducibility() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_jsonl(&dataset, &["inst-1"], &["patch content 1\n"]);

    let output_a = work.path().join("out-a");
    let args_a = base_rehearsal_args(DatasetSource::LocalPath(dataset.clone()), output_a.clone());
    run(args_a).await.unwrap();

    let output_b = work.path().join("out-b");
    let args_b = base_rehearsal_args(DatasetSource::LocalPath(dataset.clone()), output_b.clone());
    run(args_b).await.unwrap();

    // Verify byte-stability of trajectories
    let traj_a = std::fs::read(&output_a.join("inst-1").join("run-1.traj.json")).unwrap();
    let traj_b = std::fs::read(&output_b.join("inst-1").join("run-1.traj.json")).unwrap();
    assert_eq!(traj_a, traj_b);

    // Verify byte-stability of results
    let res_a = std::fs::read(&output_a.join("results.json")).unwrap();
    let res_b = std::fs::read(&output_b.join("results.json")).unwrap();
    assert_eq!(res_a, res_b);
}

#[tokio::test]
async fn test_rehearsal_evaluator_scores_one() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_jsonl(
        &dataset,
        &["inst-1", "inst-2"],
        &["patch content 1\n", "patch content 2\n"],
    );

    let output = work.path().join("out");
    let args = base_rehearsal_args(DatasetSource::LocalPath(dataset), output.clone());
    run(args).await.unwrap();

    // Call evaluate manually
    let eval_args = maxwells_daemon::run::evaluate::EvaluateArgs {
        sweep_dir: output.clone(),
        dataset_path: None,
        backend: maxwells_daemon::run::evaluate::EvaluateBackend::Rehearsal,
        timeout_per_instance_secs: 60,
        parallel: 2,
        sb_subset: "".to_owned(),
        sb_split: "test".to_owned(),
        run_id: None,
        breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::default_axes(),
        cost_attribution: true,
    };

    let eval_results = maxwells_daemon::run::evaluate::run(&eval_args).unwrap();
    assert_eq!(eval_results.instances.len(), 2);
    for inst in eval_results.instances {
        assert!(inst.resolved);
        assert_eq!(inst.resolved_count, 1);
    }
}

#[tokio::test]
async fn test_skip_evaluator_short_circuit() {
    use clap::Parser as _;
    use maxwells_daemon::cli::{Cli, Command};

    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_jsonl(&dataset, &["inst-1"], &["patch content 1\n"]);

    let output = work.path().join("out");

    let args_vec = vec![
        "max".to_string(),
        "bench".to_string(),
        "rehearsal".to_string(),
        "--dataset-path".to_string(),
        dataset.to_str().unwrap().to_string(),
        "--output".to_string(),
        output.to_str().unwrap().to_string(),
        "--skip-evaluator".to_string(),
        "--skip-preflight".to_string(),
        "--skip-model-probe".to_string(),
        "--skip-patch-validation".to_string(),
    ];

    let cli = Cli::try_parse_from(args_vec).unwrap();
    match cli.command {
        Command::Bench { cmd } => match *cmd {
            maxwells_daemon::cli::args::BenchCmd::Rehearsal(mut s) => {
                s.rehearse = true;
                maxwells_daemon::cli::bench_swebench(*s).await.unwrap();
            }
            _ => panic!("Expected bench rehearsal subcommand"),
        },
        _ => panic!("Expected Command::Bench"),
    }

    let output_rehearsal = output.with_extension("rehearsal");

    let results_path = output_rehearsal.join("results.json");
    assert!(results_path.exists());

    let evaluation_path = output_rehearsal.join("evaluation.json");
    assert!(!evaluation_path.exists());

    let report_path = output_rehearsal.join("report.md");
    assert!(report_path.exists());
}

#[tokio::test]
async fn test_diff_surfaces_drift() {
    use clap::Parser as _;
    use maxwells_daemon::cli::{Cli, Command};

    let work = tempfile::tempdir().unwrap();
    let dataset_baseline = work.path().join("dataset_baseline.jsonl");
    write_jsonl(
        &dataset_baseline,
        &["inst-1", "inst-2"],
        &["patch 1\n", "patch 2\n"],
    );

    let output_baseline = work.path().join("out_baseline");

    let args_baseline = vec![
        "max".to_string(),
        "bench".to_string(),
        "rehearsal".to_string(),
        "--dataset-path".to_string(),
        dataset_baseline.to_str().unwrap().to_string(),
        "--output".to_string(),
        output_baseline.to_str().unwrap().to_string(),
        "--skip-preflight".to_string(),
        "--skip-model-probe".to_string(),
        "--skip-patch-validation".to_string(),
    ];
    let cli_baseline = Cli::try_parse_from(args_baseline).unwrap();
    if let Command::Bench { cmd } = cli_baseline.command {
        if let maxwells_daemon::cli::args::BenchCmd::Rehearsal(mut s) = *cmd {
            s.rehearse = true;
            maxwells_daemon::cli::bench_swebench(*s).await.unwrap();
        }
    }

    let output_baseline_rehearsal = output_baseline.with_extension("rehearsal");

    let output_candidate_match = work.path().join("out_candidate_match");
    let args_candidate_match = vec![
        "max".to_string(),
        "bench".to_string(),
        "rehearsal".to_string(),
        "--dataset-path".to_string(),
        dataset_baseline.to_str().unwrap().to_string(),
        "--output".to_string(),
        output_candidate_match.to_str().unwrap().to_string(),
        "--skip-preflight".to_string(),
        "--skip-model-probe".to_string(),
        "--skip-patch-validation".to_string(),
    ];
    let cli_candidate_match = Cli::try_parse_from(args_candidate_match).unwrap();
    if let Command::Bench { cmd } = cli_candidate_match.command {
        if let maxwells_daemon::cli::args::BenchCmd::Rehearsal(mut s) = *cmd {
            s.rehearse = true;
            maxwells_daemon::cli::bench_swebench(*s).await.unwrap();
        }
    }

    let output_candidate_match_rehearsal = output_candidate_match.with_extension("rehearsal");

    assert!(
        maxwells_daemon::cli::compare_rehearsals(
            &output_baseline_rehearsal,
            &output_candidate_match_rehearsal
        )
        .is_ok()
    );

    let dataset_drift = work.path().join("dataset_drift.jsonl");
    write_jsonl(&dataset_drift, &["inst-1"], &["patch 1\n"]);

    let output_candidate_drift = work.path().join("out_candidate_drift");
    let args_candidate_drift = vec![
        "max".to_string(),
        "bench".to_string(),
        "rehearsal".to_string(),
        "--dataset-path".to_string(),
        dataset_drift.to_str().unwrap().to_string(),
        "--output".to_string(),
        output_candidate_drift.to_str().unwrap().to_string(),
        "--skip-preflight".to_string(),
        "--skip-model-probe".to_string(),
        "--skip-patch-validation".to_string(),
    ];
    let cli_candidate_drift = Cli::try_parse_from(args_candidate_drift).unwrap();
    if let Command::Bench { cmd } = cli_candidate_drift.command {
        if let maxwells_daemon::cli::args::BenchCmd::Rehearsal(mut s) = *cmd {
            s.rehearse = true;
            maxwells_daemon::cli::bench_swebench(*s).await.unwrap();
        }
    }

    let output_candidate_drift_rehearsal = output_candidate_drift.with_extension("rehearsal");

    let diff_result = maxwells_daemon::cli::compare_rehearsals(
        &output_baseline_rehearsal,
        &output_candidate_drift_rehearsal,
    );
    assert!(diff_result.is_err());
    let err_msg = diff_result.err().unwrap().to_string();
    assert!(err_msg.contains("regressions detected"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_rehearsal_empty_patch_and_missing_eval_comparisons() {
    let work = tempfile::tempdir().unwrap();

    // 1. Test empty patch is not resolved
    let dataset = work.path().join("dataset.jsonl");
    write_jsonl(&dataset, &["inst-empty"], &[""]);

    let output = work.path().join("out_empty");
    let args = base_rehearsal_args(DatasetSource::LocalPath(dataset), output.clone());
    let results = run(args).await.unwrap();
    assert_eq!(results.total, 1);

    // Call evaluate manually
    let eval_args = maxwells_daemon::run::evaluate::EvaluateArgs {
        sweep_dir: output.clone(),
        dataset_path: None,
        backend: maxwells_daemon::run::evaluate::EvaluateBackend::Rehearsal,
        timeout_per_instance_secs: 60,
        parallel: 1,
        sb_subset: "".to_owned(),
        sb_split: "test".to_owned(),
        run_id: None,
        breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::default_axes(),
        cost_attribution: true,
    };

    let eval_results = maxwells_daemon::run::evaluate::run(&eval_args).unwrap();
    assert_eq!(eval_results.instances.len(), 1);
    assert!(!eval_results.instances[0].resolved);
    assert_eq!(
        eval_results.instances[0].eval_exit_reason,
        maxwells_daemon::run::evaluate::EvalExitReason::SkippedNoPatch
    );

    // 2. Test missing candidate evaluation comparison
    let base_dir = work.path().join("base_cmp");
    let cand_dir = work.path().join("cand_cmp");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&cand_dir).unwrap();

    let sweep_results = maxwells_daemon::run::swebench::SweepResults {
        total: 1,
        sweep_status: "complete".to_owned(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 1,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 1,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: std::collections::BTreeMap::new(),
        budget_halted: 0,
        with_patch: 1,
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
        filter_spec: maxwells_daemon::run::swebench::FilterSpec::default(),
        manifest: None,
        cost_limit_usd: None,
        instances: vec![maxwells_daemon::run::swebench::InstanceResult {
            instance_id: "inst-1".to_owned(),
            exit_reason: "submitted".to_owned(),
            outcome: Some("submitted".to_owned()),
            failure_category: None,
            steps: Some(1),
            cost_usd: Some(0.0),
            prompt_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(0),
            duration_secs: Some(0.0),
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: vec![],
            runs: 1,
            resolved_count: 1,
            pass_at_1: true,
            tests_run_before_submit: false,
            last_tests_passed: None,
            fallback_count: None,
            final_model: None,
            retry_id: None,
            previous_failure_category: None,
            trace_id: None,
        }],
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: std::collections::BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: vec![],
        partial: 0,
        span_export_dropped: 0,
    };
    let results_json = serde_json::to_string(&sweep_results).unwrap();

    let base_eval_res = maxwells_daemon::run::evaluate::EvaluationResults {
        instances: vec![maxwells_daemon::run::evaluate::InstanceEvaluation {
            instance_id: "inst-1".to_owned(),
            resolved: true,
            runs: 1,
            resolved_count: 1,
            pass_at_1: true,
            tests_passed: vec![],
            tests_failed: vec![],
            eval_exit_reason: maxwells_daemon::run::evaluate::EvalExitReason::Resolved,
            eval_log_path: None,
            patch_stats: None,
            patch_error_log: None,
        }],
        behavioral: maxwells_daemon::run::evaluate::BehavioralMetrics::default(),
        breakdown: vec![],
        cost_attribution: vec![],
        model_mix_summary: vec![],
        latency_summary: None,
        provenance: None,
    };
    let base_eval_json = serde_json::to_string(&base_eval_res).unwrap();

    std::fs::write(base_dir.join("results.json"), &results_json).unwrap();
    std::fs::write(base_dir.join("evaluation.json"), &base_eval_json).unwrap();

    let cand_eval_res = maxwells_daemon::run::evaluate::EvaluationResults {
        instances: vec![],
        behavioral: maxwells_daemon::run::evaluate::BehavioralMetrics::default(),
        breakdown: vec![],
        cost_attribution: vec![],
        model_mix_summary: vec![],
        latency_summary: None,
        provenance: None,
    };
    let cand_eval_json = serde_json::to_string(&cand_eval_res).unwrap();

    std::fs::write(cand_dir.join("results.json"), &results_json).unwrap();
    std::fs::write(cand_dir.join("evaluation.json"), &cand_eval_json).unwrap();

    let cmp_res = maxwells_daemon::cli::compare_rehearsals(&base_dir, &cand_dir);
    if let Err(ref e) = cmp_res {
        println!("DEBUG MOCK CMP ERROR: {}", e);
    }
    assert!(cmp_res.is_err());
    let err_msg = cmp_res.err().unwrap().to_string();
    assert!(
        err_msg.contains("Drift/regression comparison failed: regressions detected."),
        "Unexpected error message: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_forecast_rejects_rehearsal() {
    use clap::Parser as _;
    use maxwells_daemon::cli::{Cli, Command};

    let args = vec![
        "max".to_string(),
        "bench".to_string(),
        "swebench".to_string(),
        "--rehearse".to_string(),
        "--forecast-first".to_string(),
        "--dataset-path".to_string(),
        "dummy.jsonl".to_string(),
        "--output".to_string(),
        "dummy".to_string(),
    ];
    let cli = Cli::try_parse_from(args).unwrap();
    if let Command::Bench { cmd } = cli.command {
        if let maxwells_daemon::cli::args::BenchCmd::Swebench(s) = *cmd {
            let res = maxwells_daemon::cli::bench_swebench(*s).await;
            assert!(res.is_err());
            let err_msg = res.unwrap_err().to_string();
            assert!(err_msg.contains("rehearsal mode cannot be used with forecast-first"));
        }
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_rehearsal_drift_resolved_count_and_pass_at_1() {
    let work = tempfile::tempdir().unwrap();
    let base_dir = work.path().join("base_cmp");
    let cand_dir = work.path().join("cand_cmp");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&cand_dir).unwrap();

    let sweep_results = maxwells_daemon::run::swebench::SweepResults {
        total: 1,
        sweep_status: "complete".to_owned(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 1,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 1,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: std::collections::BTreeMap::new(),
        budget_halted: 0,
        with_patch: 1,
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
        filter_spec: maxwells_daemon::run::swebench::FilterSpec::default(),
        manifest: None,
        cost_limit_usd: None,
        instances: vec![maxwells_daemon::run::swebench::InstanceResult {
            instance_id: "inst-1".to_owned(),
            exit_reason: "submitted".to_owned(),
            outcome: Some("submitted".to_owned()),
            failure_category: None,
            steps: Some(1),
            cost_usd: Some(0.0),
            prompt_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(0),
            duration_secs: Some(0.0),
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: vec![],
            runs: 3,
            resolved_count: 3,
            pass_at_1: true,
            tests_run_before_submit: false,
            last_tests_passed: None,
            fallback_count: None,
            final_model: None,
            retry_id: None,
            previous_failure_category: None,
            trace_id: None,
        }],
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: std::collections::BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: vec![],
        partial: 0,
        span_export_dropped: 0,
    };
    let results_json = serde_json::to_string(&sweep_results).unwrap();

    let base_eval_res = maxwells_daemon::run::evaluate::EvaluationResults {
        instances: vec![maxwells_daemon::run::evaluate::InstanceEvaluation {
            instance_id: "inst-1".to_owned(),
            resolved: true,
            runs: 3,
            resolved_count: 3,
            pass_at_1: true,
            tests_passed: vec![],
            tests_failed: vec![],
            eval_exit_reason: maxwells_daemon::run::evaluate::EvalExitReason::Resolved,
            eval_log_path: None,
            patch_stats: None,
            patch_error_log: None,
        }],
        behavioral: maxwells_daemon::run::evaluate::BehavioralMetrics::default(),
        breakdown: vec![],
        cost_attribution: vec![],
        model_mix_summary: vec![],
        latency_summary: None,
        provenance: None,
    };
    let base_eval_json = serde_json::to_string(&base_eval_res).unwrap();

    // Candidate has resolved count regressed from 3 to 1, and pass_at_1 regressed to false
    let cand_eval_res = maxwells_daemon::run::evaluate::EvaluationResults {
        instances: vec![maxwells_daemon::run::evaluate::InstanceEvaluation {
            instance_id: "inst-1".to_owned(),
            resolved: true,
            runs: 3,
            resolved_count: 1,
            pass_at_1: false,
            tests_passed: vec![],
            tests_failed: vec![],
            eval_exit_reason: maxwells_daemon::run::evaluate::EvalExitReason::Resolved,
            eval_log_path: None,
            patch_stats: None,
            patch_error_log: None,
        }],
        behavioral: maxwells_daemon::run::evaluate::BehavioralMetrics::default(),
        breakdown: vec![],
        cost_attribution: vec![],
        model_mix_summary: vec![],
        latency_summary: None,
        provenance: None,
    };
    let cand_eval_json = serde_json::to_string(&cand_eval_res).unwrap();

    std::fs::write(base_dir.join("results.json"), &results_json).unwrap();
    std::fs::write(base_dir.join("evaluation.json"), &base_eval_json).unwrap();
    std::fs::write(cand_dir.join("results.json"), &results_json).unwrap();
    std::fs::write(cand_dir.join("evaluation.json"), &cand_eval_json).unwrap();

    let cmp_res = maxwells_daemon::cli::compare_rehearsals(&base_dir, &cand_dir);
    assert!(cmp_res.is_err());
    let err_msg = cmp_res.err().unwrap().to_string();
    assert!(err_msg.contains("Drift/regression comparison failed: regressions detected."));
}

#[tokio::test]
async fn test_rehearsal_fails_on_corrupted_trajectory() {
    let work = tempfile::tempdir().unwrap();

    // Create a dummy sweep output dir with a corrupted trajectory JSON
    let sweep_dir = work.path().join("sweep");
    let inst_dir = sweep_dir.join("inst-1");
    std::fs::create_dir_all(&inst_dir).unwrap();
    std::fs::write(inst_dir.join("run-1.traj.json"), "{ corrupted json...").unwrap();
    std::fs::write(inst_dir.join("run-1.patch"), "some diff\n").unwrap();

    let sweep_results = serde_json::json!({
        "total": 1,
        "sweep_status": "complete",
        "completed": 1,
        "submitted": 1,
        "submitted_with_tests": 0,
        "skipped": 0,
        "errored": 0,
        "failures_by_category": {},
        "budget_halted": 0,
        "with_patch": 1,
        "patch_empty": 0,
        "patch_apply_invalid": 0,
        "github_pr_failures": 0,
        "total_prompt_tokens": 0,
        "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "cache_hit_rate": 0.0,
        "retries": 0,
        "retried_instances": 0,
        "pass_at_k": 0.0,
        "filter_spec": {},
        "total_fallbacks": 0,
        "model_mix": {},
        "partial": 0,
        "span_export_dropped": 0,
        "instances": [{
            "instance_id": "inst-1",
            "exit_reason": "submitted",
            "outcome": "submitted",
            "steps": 1,
            "cost_usd": 0.0,
            "prompt_tokens": 0,
            "cache_read_tokens": 0,
            "cache_creation_tokens": 0,
            "completion_tokens": 0,
            "duration_secs": 0.0,
            "patch_present": true,
            "non_empty_patch": true,
            "attempts": 1,
            "retry_reasons": [],
            "runs": 1,
            "resolved_count": 1,
            "pass_at_1": true,
            "tests_run_before_submit": false
        }]
    });
    std::fs::write(
        sweep_dir.join("results.json"),
        serde_json::to_string(&sweep_results).unwrap(),
    )
    .unwrap();

    let eval_args = maxwells_daemon::run::evaluate::EvaluateArgs {
        sweep_dir,
        dataset_path: None,
        backend: maxwells_daemon::run::evaluate::EvaluateBackend::Rehearsal,
        timeout_per_instance_secs: 60,
        parallel: 1,
        sb_subset: "".to_owned(),
        sb_split: "test".to_owned(),
        run_id: None,
        breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::default_axes(),
        cost_attribution: true,
    };

    let eval_res = maxwells_daemon::run::evaluate::run(&eval_args);
    assert!(eval_res.is_err());
    let err_msg = eval_res.err().unwrap().to_string();
    assert!(err_msg.contains("Failed to parse trajectory file"));
}

#[tokio::test]
async fn test_rehearsal_rejects_diff_combined_with_dry_run() {
    use clap::Parser as _;
    use maxwells_daemon::cli::{Cli, Command};

    let args = vec![
        "max".to_string(),
        "bench".to_string(),
        "swebench".to_string(),
        "--rehearse".to_string(),
        "--dry-run".to_string(),
        "--diff".to_string(),
        "dummy_dir".to_string(),
        "--dataset-path".to_string(),
        "dummy.jsonl".to_string(),
        "--output".to_string(),
        "dummy".to_string(),
    ];
    let cli = Cli::try_parse_from(args).unwrap();
    if let Command::Bench { cmd } = cli.command {
        if let maxwells_daemon::cli::args::BenchCmd::Swebench(s) = *cmd {
            let res = maxwells_daemon::cli::bench_swebench(*s).await;
            assert!(res.is_err());
            let err_msg = res.unwrap_err().to_string();
            assert!(err_msg.contains("--diff cannot be used with --dry-run"));
        }
    }
}

#[tokio::test]
async fn test_rehearsal_fails_on_missing_trajectory() {
    let work = tempfile::tempdir().unwrap();

    // Create a dummy sweep output dir with a missing trajectory JSON
    let sweep_dir = work.path().join("sweep");
    let inst_dir = sweep_dir.join("inst-1");
    std::fs::create_dir_all(&inst_dir).unwrap();
    // Do NOT write the traj.json file
    std::fs::write(inst_dir.join("run-1.patch"), "some diff\n").unwrap();

    let sweep_results = serde_json::json!({
        "total": 1,
        "sweep_status": "complete",
        "completed": 1,
        "submitted": 1,
        "submitted_with_tests": 0,
        "skipped": 0,
        "errored": 0,
        "failures_by_category": {},
        "budget_halted": 0,
        "with_patch": 1,
        "patch_empty": 0,
        "patch_apply_invalid": 0,
        "github_pr_failures": 0,
        "total_prompt_tokens": 0,
        "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "cache_hit_rate": 0.0,
        "retries": 0,
        "retried_instances": 0,
        "pass_at_k": 0.0,
        "filter_spec": {},
        "total_fallbacks": 0,
        "model_mix": {},
        "partial": 0,
        "span_export_dropped": 0,
        "instances": [{
            "instance_id": "inst-1",
            "exit_reason": "submitted",
            "outcome": "submitted",
            "steps": 1,
            "cost_usd": 0.0,
            "prompt_tokens": 0,
            "cache_read_tokens": 0,
            "cache_creation_tokens": 0,
            "completion_tokens": 0,
            "duration_secs": 0.0,
            "patch_present": true,
            "non_empty_patch": true,
            "attempts": 1,
            "retry_reasons": [],
            "runs": 1,
            "resolved_count": 1,
            "pass_at_1": true,
            "tests_run_before_submit": false
        }]
    });
    std::fs::write(
        sweep_dir.join("results.json"),
        serde_json::to_string(&sweep_results).unwrap(),
    )
    .unwrap();

    let eval_args = maxwells_daemon::run::evaluate::EvaluateArgs {
        sweep_dir,
        dataset_path: None,
        backend: maxwells_daemon::run::evaluate::EvaluateBackend::Rehearsal,
        timeout_per_instance_secs: 60,
        parallel: 1,
        sb_subset: "".to_owned(),
        sb_split: "test".to_owned(),
        run_id: None,
        breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::default_axes(),
        cost_attribution: true,
    };

    let eval_res = maxwells_daemon::run::evaluate::run(&eval_args);
    assert!(eval_res.is_err());
    let err_msg = eval_res.err().unwrap().to_string();
    assert!(err_msg.contains("does not exist during rehearsal evaluation"));
}
