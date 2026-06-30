//! `bench forecast`: calibration-driven sweep cost forecasts.

#![allow(clippy::unwrap_used, clippy::too_many_lines, clippy::large_futures)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use maxwells_daemon::run::evaluate::{BreakdownSelection, EvaluateArgs, EvaluateBackend};
use maxwells_daemon::run::forecast::{
    ForecastArgs, ForecastGate, ForecastOutcome, ForecastReport, ThresholdStatus,
    forecast_from_results, forecast_gate_allows_sweep, run, validate_fail_over_cap,
};
use maxwells_daemon::run::swebench::{
    InstanceResult, SwebenchArgs, SweepResults, SweepSignal, trajectory_path_for_run,
};
use maxwells_daemon::trajectory::{FailureCategory, outcome};
use maxwells_daemon::{Config, ModelUsage};
use tokio::sync::mpsc;

mod support;
use support::binary_path;

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

fn write_step_limit_zero_config(path: &Path) {
    std::fs::write(path, "[agent]\nstep_limit = 0\n").unwrap();
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

fn submit_response() -> String {
    "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".to_owned()
}

fn cancellation_blocking_response(started_marker: &Path) -> Vec<String> {
    let marker = started_marker.display().to_string();
    let command = if cfg!(windows) {
        let blocker = started_marker.with_file_name("forecast-blocker.cmd");
        std::fs::write(&blocker, "@echo off\r\n:loop\r\ngoto loop\r\n").unwrap();
        let blocker = blocker.display().to_string();
        format!("echo forecast-partial & echo started>\"{marker}\" & call \"{blocker}\"")
    } else {
        format!("echo forecast-partial; echo started > '{marker}'; sleep 30")
    };
    vec![format!("```bash\n{command}\n```")]
}

async fn wait_for_path(path: PathBuf) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {}", path.display());
}

fn instance(
    id: &str,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    steps: u32,
    duration_secs: f64,
    submitted: bool,
) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: if submitted { "submitted" } else { "error" }.into(),
        outcome: Some(if submitted {
            outcome::SUBMITTED.into()
        } else {
            outcome::ERROR.into()
        }),
        failure_category: (!submitted).then_some(FailureCategory::Unknown),
        steps: Some(steps),
        cost_usd: Some(cost_usd),
        prompt_tokens: Some(input_tokens),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(output_tokens),
        duration_secs: Some(duration_secs),
        error: None,
        github_pr_error: None,
        patch_present: submitted,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 0,
        resolved_count: 0,
        pass_at_1: false,
        tests_run_before_submit: false,
        last_tests_passed: None,

        fallback_count: None,

        final_model: None,
        retry_id: None,
        previous_failure_category: None,
        trace_id: None,
        context_pressure: Default::default(),
        peak_memory_bytes: None,
        cpu_seconds: None,
    }
}

fn fixture_results() -> SweepResults {
    fixture_results_with_model(None)
}

fn fixture_results_with_model(model_name: Option<&str>) -> SweepResults {
    let instances = vec![
        instance("a", 100, 10, 0.01, 1, 2.0, true),
        instance("b", 200, 20, 0.02, 2, 4.0, false),
        instance("c", 300, 30, 0.03, 3, 6.0, true),
    ];
    SweepResults {
        total: instances.len(),
        sweep_status: maxwells_daemon::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 2,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 1,
        failures_by_category: Default::default(),
        budget_halted: 0,
        with_patch: 0,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 600,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 60,
        estimated_cost_usd: 0.06,
        actual_cost_usd: None,
        actual_cost_source: None,
        baseline_cost_usd: None,
        baseline_cost_model: None,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: 0.0,
        filter_spec: Default::default(),
        manifest: model_name.map(|name| maxwells_daemon::run::swebench::ProvenanceManifest {
            purpose: None,
            harness: maxwells_daemon::run::swebench::HarnessManifest {
                name: "maxwells-daemon".into(),
                version: "test".into(),
                git_sha: None,
                git_dirty: None,
                git_resolution: "test".into(),
            },
            dataset: maxwells_daemon::run::swebench::DatasetManifest {
                path: "test.jsonl".into(),
                sha256: "test".into(),
                instance_count: instances.len(),
                filter_spec: Some(Default::default()),
                ..Default::default()
            },
            prompt_template: maxwells_daemon::run::swebench::PromptTemplateManifest {
                source: "inline".into(),
                path: None,
                sha256: "test".into(),
            },
            config: maxwells_daemon::run::swebench::ConfigManifest {
                resolved: "test".into(),
                overlay_paths: Vec::new(),
            },
            model: maxwells_daemon::run::swebench::ModelManifest {
                name: name.into(),
                backend: "litellm".into(),
                backend_version: None,
                base_url: None,
            },
            runtime: maxwells_daemon::run::swebench::RuntimeManifest {
                started_at_utc: "2026-05-01T00:00:00Z".into(),
                finished_at_utc: Some("2026-05-01T00:01:00Z".into()),
                host_os: "linux".into(),
                resume_mode: false,
                rust_version: None,
            },
            cli: maxwells_daemon::run::swebench::CliManifest { argv: Vec::new() },
            chaos_fail_every: 0,
            circuit_breaker: None,
            source: None,
            import_predictions_path: None,
            import_predictions_sha256: None,
            reproduced_from: None,
            merged_from: None,
        }),
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,

        total_fallbacks: 0,

        model_mix: std::collections::BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: vec![],
        partial: 0,
        span_export_dropped: 0,
        max_peak_memory_bytes: None,
        median_peak_memory_bytes: None,
        total_cpu_seconds: None,
    }
}

fn expect_forecast_report(outcome: ForecastOutcome) -> ForecastReport {
    match outcome {
        ForecastOutcome::Report(report) => *report,
        ForecastOutcome::DryRun(_) => panic!("expected measured forecast report, got dry-run"),
        ForecastOutcome::Cancelled(_) => panic!("expected measured forecast report, got cancelled"),
    }
}

#[test]
fn target_n_extrapolation_math_is_correct_against_fixture() {
    let report = forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, None).unwrap();

    assert_eq!(report.calibration.n, 3);
    assert_eq!(report.calibration.seed, 42);
    assert_eq!(report.forecast.target_n, 6);
    assert_eq!(report.forecast.parallel, 2);

    assert!((report.forecast.total_input_tokens.point - 1200.0).abs() < 1e-9);
    assert!((report.forecast.total_output_tokens.point - 120.0).abs() < 1e-9);
    assert!((report.forecast.total_cost_usd.point - 0.12).abs() < 1e-9);
    assert!((report.forecast.wall_clock_seconds.point - 12.0).abs() < 1e-9);

    assert!((report.per_instance.input_tokens.median - 200.0).abs() < 1e-9);
    assert!((report.per_instance.output_tokens.median - 20.0).abs() < 1e-9);
    assert!((report.per_instance.usd_cost.median - 0.02).abs() < 1e-9);
    assert_eq!(report.resolution_rate.resolved, 2);
    assert_eq!(report.resolution_rate.total, 3);
}

#[test]
fn confidence_interval_widens_monotonically_as_confidence_increases() {
    let report_80 = forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, None).unwrap();
    let report_95 = forecast_from_results(&fixture_results(), 42, 6, 2, 95.0, None).unwrap();

    assert!(
        report_95.forecast.total_cost_usd.width() > report_80.forecast.total_cost_usd.width(),
        "95% interval should be wider than 80%"
    );
}

#[test]
fn forecast_json_is_byte_deterministic_for_same_calibration_slice() {
    let mut reversed = fixture_results();
    reversed.instances.reverse();

    let expected = maxwells_daemon::run::forecast::to_json(
        &forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, None).unwrap(),
    )
    .unwrap();
    let actual = maxwells_daemon::run::forecast::to_json(
        &forecast_from_results(&reversed, 42, 6, 2, 80.0, None).unwrap(),
    )
    .unwrap();

    assert_eq!(actual, expected);
}

#[test]
fn fail_over_cap_errors_only_when_forecast_exceeds_cap() {
    let under = forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, Some(0.50)).unwrap();
    assert_eq!(under.threshold.status, ThresholdStatus::Under);
    validate_fail_over_cap(&under, true).unwrap();

    let over = forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, Some(0.01)).unwrap();
    assert_eq!(over.threshold.status, ThresholdStatus::Exceeds);
    assert!(validate_fail_over_cap(&over, true).is_err());
    validate_fail_over_cap(&over, false).unwrap();
}

#[test]
fn forecast_first_gate_requires_clear_cap_or_yes() {
    let clear = forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, Some(1.0)).unwrap();
    assert!(forecast_gate_allows_sweep(&clear, ForecastGate { yes: false }).unwrap());

    let tripped = forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, Some(0.01)).unwrap();
    assert!(!forecast_gate_allows_sweep(&tripped, ForecastGate { yes: false }).unwrap());
    assert!(forecast_gate_allows_sweep(&tripped, ForecastGate { yes: true }).unwrap());

    let no_cap = forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, None).unwrap();
    assert!(!forecast_gate_allows_sweep(&no_cap, ForecastGate { yes: false }).unwrap());
    assert!(forecast_gate_allows_sweep(&no_cap, ForecastGate { yes: true }).unwrap());
}

#[test]
fn forecast_uses_manifest_model_for_fallback_cost_repricing() {
    let mut results = fixture_results_with_model(Some("anthropic/claude-sonnet-4-6"));
    results.instances = vec![InstanceResult {
        instance_id: "cached".into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(1),
        cost_usd: Some(0.0),
        prompt_tokens: Some(0),
        cache_read_tokens: Some(1_000_000),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(0),
        duration_secs: Some(1.0),
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: true,
        attempts: 1,
        retry_reasons: Vec::new(),
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
        context_pressure: Default::default(),
        peak_memory_bytes: None,
        cpu_seconds: None,
    }];
    results.total = 1;
    results.submitted = 1;
    results.errored = 0;
    results.total_prompt_tokens = 0;
    results.total_cache_read_tokens = 1_000_000;
    results.total_cache_creation_tokens = 0;
    results.total_completion_tokens = 0;
    results.estimated_cost_usd = 0.0;

    let report = forecast_from_results(&results, 42, 1, 1, 80.0, None).unwrap();
    assert!(
        (report.forecast.total_cost_usd.point - 0.3).abs() < 1e-9,
        "{report:#?}"
    );
    assert!(
        (report.per_instance.usd_cost.median - 0.3).abs() < 1e-9,
        "{report:#?}"
    );
}

#[test]
fn cli_exposes_forecast_command_and_forecast_first_flag() {
    let bin = binary_path();

    let bench_help = Command::new(&bin)
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(bench_help.status.success());
    let bench_stdout = String::from_utf8(bench_help.stdout).unwrap();
    assert!(
        bench_stdout.contains("forecast"),
        "expected `forecast` in bench help, got:\n{bench_stdout}"
    );

    let forecast_help = Command::new(&bin)
        .args(["bench", "forecast", "--help"])
        .output()
        .unwrap();
    assert!(forecast_help.status.success());
    let forecast_stdout = String::from_utf8(forecast_help.stdout).unwrap();
    for flag in [
        "--calibration-n",
        "--target-n",
        "--confidence",
        "--fail-over-cap",
    ] {
        assert!(
            forecast_stdout.contains(flag),
            "expected {flag} in forecast help, got:\n{forecast_stdout}"
        );
    }

    let swebench_help = Command::new(&bin)
        .args(["bench", "swebench", "--help"])
        .output()
        .unwrap();
    assert!(swebench_help.status.success());
    let swebench_stdout = String::from_utf8(swebench_help.stdout).unwrap();
    assert!(
        swebench_stdout.contains("--forecast-first"),
        "expected --forecast-first in swebench help, got:\n{swebench_stdout}"
    );
    assert!(
        swebench_stdout.contains("--yes"),
        "expected --yes in swebench help, got:\n{swebench_stdout}"
    );
}

#[test]
fn cli_forecast_json_stdout_is_one_forecast_document() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let config = work.path().join("config.toml");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["a", "b"]);
    write_step_limit_zero_config(&config);

    let run = Command::new(binary_path())
        .args([
            "bench",
            "forecast",
            "--dataset-path",
            dataset.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--calibration-n",
            "1",
            "--seed",
            "7",
            "--target-n",
            "2",
            "--step-limit",
            "0",
            "--format",
            "json",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();

    assert!(
        run.status.success(),
        "forecast failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8(run.stdout).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|err| panic!("{err}: {stdout}"));
    assert_eq!(
        parsed
            .pointer("/calibration/seed")
            .and_then(serde_json::Value::as_u64),
        Some(7)
    );
    assert!(
        parsed.get("checks").is_none(),
        "stdout should contain only the forecast document, got: {stdout}"
    );
}

#[test]
fn cli_forecast_dry_run_returns_preflight_without_artifacts() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let config = work.path().join("config.toml");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["a"]);
    write_step_limit_zero_config(&config);

    let run = Command::new(binary_path())
        .args([
            "bench",
            "forecast",
            "--dataset-path",
            dataset.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--calibration-n",
            "1",
            "--seed",
            "7",
            "--target-n",
            "1",
            "--dry-run",
            "--step-limit",
            "0",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();

    assert!(
        run.status.success(),
        "forecast dry-run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("preflight checks passed"),
        "dry-run should report preflight success, got: {}",
        String::from_utf8_lossy(&run.stdout)
    );
    assert!(
        !output.join("forecast/results.json").exists(),
        "dry-run must not write forecast results"
    );
}

#[test]
fn cli_forecast_first_dry_run_returns_preflight_without_artifacts() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let config = work.path().join("config.toml");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["a"]);
    write_step_limit_zero_config(&config);

    let run = Command::new(binary_path())
        .args([
            "bench",
            "swebench",
            "--dataset-path",
            dataset.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--forecast-first",
            "--calibration-n",
            "1",
            "--seed",
            "7",
            "--target-n",
            "1",
            "--dry-run",
            "--step-limit",
            "0",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();

    assert!(
        run.status.success(),
        "forecast-first dry-run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("preflight checks passed"),
        "dry-run should report preflight success, got: {}",
        String::from_utf8_lossy(&run.stdout)
    );
    assert!(
        !output.join("forecast/results.json").exists(),
        "dry-run must not write forecast results"
    );
    assert!(
        !output.join("results.json").exists(),
        "dry-run must not launch or write the real sweep"
    );
}

#[test]
fn cli_fail_over_cap_returns_nonzero_when_forecast_exceeds_cap() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let config = work.path().join("config.toml");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["a"]);
    write_step_limit_zero_config(&config);

    let run = Command::new(binary_path())
        .args([
            "bench",
            "forecast",
            "--dataset-path",
            dataset.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--calibration-n",
            "1",
            "--seed",
            "7",
            "--target-n",
            "1",
            "--step-limit",
            "0",
            "--sweep-cost-limit-usd=-0.01",
            "--fail-over-cap",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();

    assert!(!run.status.success(), "forecast unexpectedly passed");
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("exceed cap"),
        "stderr should mention cap exceedance: {}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn cli_forecast_first_blocks_or_launches_real_sweep() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let config = work.path().join("config.toml");
    write_dataset(&dataset, &["a", "b"]);
    write_step_limit_zero_config(&config);

    let blocked_output = work.path().join("blocked");
    let blocked = Command::new(binary_path())
        .args([
            "bench",
            "swebench",
            "--dataset-path",
            dataset.to_str().unwrap(),
            "--output",
            blocked_output.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--forecast-first",
            "--calibration-n",
            "1",
            "--seed",
            "7",
            "--step-limit",
            "0",
            "--sweep-cost-limit-usd=-0.01",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();
    assert!(!blocked.status.success(), "forecast-first should block");
    assert!(blocked_output.join("forecast/results.json").exists());
    assert!(
        !blocked_output.join("results.json").exists(),
        "blocked real sweep must not write parent results.json"
    );

    let clear_output = work.path().join("clear");
    let clear = Command::new(binary_path())
        .args([
            "bench",
            "swebench",
            "--dataset-path",
            dataset.to_str().unwrap(),
            "--output",
            clear_output.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--forecast-first",
            "--calibration-n",
            "1",
            "--seed",
            "7",
            "--step-limit",
            "0",
            "--sweep-cost-limit-usd",
            "1.0",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();
    assert!(
        clear.status.success(),
        "forecast-first clear case failed: {}",
        String::from_utf8_lossy(&clear.stderr)
    );
    assert!(clear_output.join("forecast/results.json").exists());
    assert!(
        clear_output.join("results.json").exists(),
        "clear forecast should launch real sweep"
    );
}

#[tokio::test]
async fn calibration_writes_only_inside_forecast_subdirectory_and_marks_manifest() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["a", "b", "c"]);
    let output = work.path().join("runs");

    let usage = ModelUsage {
        input_tokens: 100,
        output_tokens: 10,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(0.001),
    };
    let outcome = run(ForecastArgs {
        sweep: SwebenchArgs {
            dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
            dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
            output_dir: output.clone(),
            parallel: 1,
            reruns: 1,
            config: config_with_workdir(&repo),
            resume: false,
            cost_limit_usd: Some(0.50),
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
            deterministic_responses: Some(vec![submit_response()]),
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
            event_log: None,
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
            otlp_endpoint: None,
            otlp_metrics_interval_secs: None,
            rehearse: false,
            skip_evaluator: false,
            eval_backend: "rehearsal".to_string(),
            sb_subset: None,
            sb_split: None,
            eval_timeout_secs: None,
            notify_webhook_url: None,
            notify_webhook_headers: vec![],
        },
        calibration_n: 2,
        seed: 7,
        target_n: Some(4),
        confidence_pct: 80.0,
    })
    .await
    .unwrap();
    let report = expect_forecast_report(outcome);

    assert_eq!(report.calibration.n, 2);
    assert_eq!(report.forecast.target_n, 4);
    assert!(output.join("forecast").join("results.json").exists());
    assert!(
        !output.join("results.json").exists(),
        "forecast must not write the parent sweep results.json"
    );

    let forecast_results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output.join("forecast/results.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        forecast_results
            .pointer("/manifest/purpose")
            .and_then(serde_json::Value::as_str),
        Some("forecast")
    );
    for id in &report.calibration.instance_ids {
        let traj: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(trajectory_path_for_run(&output.join("forecast"), id, 1))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            traj.pointer("/info/purpose")
                .and_then(serde_json::Value::as_str),
            Some("forecast")
        );
    }

    let eval_err = maxwells_daemon::run::evaluate::run(&EvaluateArgs {
        sweep_dir: output.join("forecast"),
        dataset_path: None,
        backend: EvaluateBackend::None,
        timeout_per_instance_secs: 1,
        parallel: 1,
        sb_subset: "swe-bench-m".into(),
        sb_split: "dev".into(),
        run_id: None,
        breakdown: BreakdownSelection::none(),
        cost_attribution: true,
        force: false,
    })
    .unwrap_err();
    assert!(
        eval_err.to_string().contains("forecast calibration"),
        "evaluate should refuse forecast output, got: {eval_err}"
    );

    assert!(
        output
            .join("forecast")
            .read_dir()
            .unwrap()
            .all(|entry| entry.unwrap().path().starts_with(output.join("forecast")))
    );
}

#[tokio::test]
async fn cancelled_calibration_returns_cancelled_outcome_instead_of_forecast_report() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["cancelled-calibration"]);
    let output = work.path().join("runs");
    let started_marker = work.path().join("forecast-command-started");
    let (signal_tx, signal_rx) = mpsc::unbounded_channel();
    tokio::spawn({
        let started_marker = started_marker.clone();
        async move {
            wait_for_path(started_marker).await;
            signal_tx.send(SweepSignal::Interrupt).unwrap();
        }
    });

    let outcome = run(ForecastArgs {
        sweep: SwebenchArgs {
            dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
            dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
            output_dir: output,
            parallel: 1,
            reruns: 1,
            config: config_with_workdir(&repo),
            resume: false,
            cost_limit_usd: Some(0.50),
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
            deterministic_responses: Some(cancellation_blocking_response(&started_marker)),
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
            event_log: None,
            max_rpm: None,
            max_input_tpm: None,
            cancel_deadline_secs: 0,
            install_os_signal_handlers: false,
            cancellation_signals: Some(signal_rx),
            github_pr: None,
            reproduced_from: None,
            abort_on_systemic_failure: true,
            systemic_failure_min_samples: 5,
            systemic_failure_share_pct: 80,
            otlp_endpoint: None,
            otlp_metrics_interval_secs: None,
            rehearse: false,
            skip_evaluator: false,
            eval_backend: "rehearsal".to_string(),
            sb_subset: None,
            sb_split: None,
            eval_timeout_secs: None,
            notify_webhook_url: None,
            notify_webhook_headers: vec![],
        },
        calibration_n: 1,
        seed: 7,
        target_n: Some(1),
        confidence_pct: 80.0,
    })
    .await
    .unwrap();

    match outcome {
        ForecastOutcome::Cancelled(results) => {
            assert_eq!(
                results.sweep_status,
                maxwells_daemon::run::swebench::SWEEP_STATUS_CANCELLED
            );
            assert_eq!(
                results.cancel_exit_code,
                Some(maxwells_daemon::run::swebench::CANCEL_EXIT_CODE_GRACEFUL)
            );
            assert_eq!(results.instances.len(), 1);
            assert_eq!(results.instances[0].exit_reason, "cancelled");
        }
        ForecastOutcome::Report(_) => panic!("cancelled calibration must not produce a forecast"),
        ForecastOutcome::DryRun(_) => panic!("cancelled calibration must not be a dry run"),
    }
}

#[tokio::test]
async fn default_target_n_honors_planned_sample_and_seed() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["a", "b", "c", "d", "e"]);
    let output = work.path().join("runs");
    let mut cfg = config_with_workdir(&repo);
    cfg.root.agent.step_limit = 0;

    let outcome = run(ForecastArgs {
        sweep: SwebenchArgs {
            dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
            dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
            output_dir: output,
            parallel: 1,
            reruns: 1,
            config: cfg,
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: Some(2),
            seed: Some(99),
            stratify_by: None,
            stratify_mode: maxwells_daemon::run::swebench::StratifyMode::Proportional,
            max_retries: 0,
            retry_on: None,
            retry_backoff_base_ms: 0,
            retry_backoff_cap_s: 0,
            retry_on_resume: false,
            deterministic_responses: None,
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
            event_log: None,
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
            otlp_endpoint: None,
            otlp_metrics_interval_secs: None,
            rehearse: false,
            skip_evaluator: false,
            eval_backend: "rehearsal".to_string(),
            sb_subset: None,
            sb_split: None,
            eval_timeout_secs: None,
            notify_webhook_url: None,
            notify_webhook_headers: vec![],
        },
        calibration_n: 1,
        seed: 7,
        target_n: None,
        confidence_pct: 80.0,
    })
    .await
    .unwrap();
    let report = expect_forecast_report(outcome);

    assert_eq!(report.forecast.target_n, 2);
}

#[tokio::test]
async fn calibration_sampling_stays_within_planned_limit() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["a", "b", "c", "d", "e"]);
    let output = work.path().join("runs");
    let mut cfg = config_with_workdir(&repo);
    cfg.root.agent.step_limit = 0;

    let outcome = run(ForecastArgs {
        sweep: SwebenchArgs {
            dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
            dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
            output_dir: output,
            parallel: 1,
            reruns: 1,
            config: cfg,
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: Some(1),
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
            config_overlay_paths: Vec::new(),
            dry_run: false,
            skip_preflight: true,
            preflight_format: "text".into(),
            skip_model_probe: true,
            preflight_check_timeout_s: 10,
            preflight_total_timeout_s: 60,
            preflight_mode: "test".into(),
            skip_patch_validation: true,
            event_log: None,
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
            otlp_endpoint: None,
            otlp_metrics_interval_secs: None,
            rehearse: false,
            skip_evaluator: false,
            eval_backend: "rehearsal".to_string(),
            sb_subset: None,
            sb_split: None,
            eval_timeout_secs: None,
            notify_webhook_url: None,
            notify_webhook_headers: vec![],
        },
        calibration_n: 1,
        seed: 7,
        target_n: None,
        confidence_pct: 80.0,
    })
    .await
    .unwrap();
    let report = expect_forecast_report(outcome);

    assert_eq!(report.forecast.target_n, 1);
    assert_eq!(report.forecast.target_instance_ids, ["a"]);
    assert_eq!(report.calibration.instance_ids, ["a"]);
}

#[tokio::test]
async fn missing_planned_sample_seed_fails_before_calibration_writes() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["a", "b", "c"]);
    let output = work.path().join("runs");
    let mut cfg = config_with_workdir(&repo);
    cfg.root.agent.step_limit = 0;

    let err = run(ForecastArgs {
        sweep: SwebenchArgs {
            dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
            dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
            output_dir: output.clone(),
            parallel: 1,
            reruns: 1,
            config: cfg,
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: Some(2),
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
            config_overlay_paths: Vec::new(),
            dry_run: false,
            skip_preflight: true,
            preflight_format: "text".into(),
            skip_model_probe: true,
            preflight_check_timeout_s: 10,
            preflight_total_timeout_s: 60,
            preflight_mode: "test".into(),
            skip_patch_validation: true,
            event_log: None,
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
            otlp_endpoint: None,
            otlp_metrics_interval_secs: None,
            rehearse: false,
            skip_evaluator: false,
            eval_backend: "rehearsal".to_string(),
            sb_subset: None,
            sb_split: None,
            eval_timeout_secs: None,
            notify_webhook_url: None,
            notify_webhook_headers: vec![],
        },
        calibration_n: 1,
        seed: 7,
        target_n: None,
        confidence_pct: 80.0,
    })
    .await
    .unwrap_err();

    assert!(
        err.to_string().contains("`--sample` requires `--seed`"),
        "unexpected error: {err}"
    );
    assert!(
        !output.join("forecast/results.json").exists(),
        "forecast must fail validation before spending calibration budget"
    );
}

#[tokio::test]
async fn forecast_with_stratified_planning_runs_calibration_subset() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["a", "b", "c", "d", "e"]);
    let output = work.path().join("runs");
    let mut cfg = config_with_workdir(&repo);
    cfg.root.agent.step_limit = 0;

    let outcome = run(ForecastArgs {
        sweep: SwebenchArgs {
            dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
            dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
            output_dir: output,
            parallel: 1,
            reruns: 1,
            config: cfg,
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: Some(3),
            seed: Some(99),
            stratify_by: Some(maxwells_daemon::run::swebench::StratifyBy::Repo),
            stratify_mode: maxwells_daemon::run::swebench::StratifyMode::Balanced,
            max_retries: 0,
            retry_on: None,
            retry_backoff_base_ms: 0,
            retry_backoff_cap_s: 0,
            retry_on_resume: false,
            deterministic_responses: None,
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
            event_log: None,
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
            otlp_endpoint: None,
            otlp_metrics_interval_secs: None,
            rehearse: false,
            skip_evaluator: false,
            eval_backend: "rehearsal".to_string(),
            sb_subset: None,
            sb_split: None,
            eval_timeout_secs: None,
            notify_webhook_url: None,
            notify_webhook_headers: vec![],
        },
        calibration_n: 2,
        seed: 7,
        target_n: None,
        confidence_pct: 80.0,
    })
    .await
    .unwrap();

    let report = expect_forecast_report(outcome);
    assert_eq!(report.calibration.instance_ids.len(), 2);
}
