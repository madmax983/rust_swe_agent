//! `bench forecast`: calibration-driven sweep cost forecasts.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::run::evaluate::{BreakdownSelection, EvaluateArgs, EvaluateBackend};
use rust_swe_agent::run::forecast::{
    ForecastArgs, ForecastGate, ThresholdStatus, forecast_from_results, forecast_gate_allows_sweep,
    run, validate_fail_over_cap,
};
use rust_swe_agent::run::swebench::{InstanceResult, SwebenchArgs, SweepResults};
use rust_swe_agent::trajectory::{FailureCategory, outcome};
use rust_swe_agent::{Config, ModelUsage};

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
    std::fs::write(path, "agent:\n  step_limit: 0\n").unwrap();
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

fn submit_response() -> String {
    "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".to_owned()
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
        completion_tokens: Some(output_tokens),
        duration_secs: Some(duration_secs),
        error: None,
        patch_present: submitted,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
    }
}

fn fixture_results() -> SweepResults {
    let instances = vec![
        instance("a", 100, 10, 0.01, 1, 2.0, true),
        instance("b", 200, 20, 0.02, 2, 4.0, false),
        instance("c", 300, 30, 0.03, 3, 6.0, true),
    ];
    SweepResults {
        total: instances.len(),
        submitted: 2,
        skipped: 0,
        errored: 1,
        failures_by_category: Default::default(),
        budget_halted: 0,
        with_patch: 0,
        total_prompt_tokens: 600,
        total_completion_tokens: 60,
        estimated_cost_usd: 0.06,
        retries: 0,
        retried_instances: 0,
        filter_spec: Default::default(),
        manifest: None,
        cost_limit_usd: None,
        instances,
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

    let expected = rust_swe_agent::run::forecast::to_json(
        &forecast_from_results(&fixture_results(), 42, 6, 2, 80.0, None).unwrap(),
    )
    .unwrap();
    let actual = rust_swe_agent::run::forecast::to_json(
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
    let config = work.path().join("config.yaml");
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
fn cli_fail_over_cap_returns_nonzero_when_forecast_exceeds_cap() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let config = work.path().join("config.yaml");
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
    let config = work.path().join("config.yaml");
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
    let report = run(ForecastArgs {
        sweep: SwebenchArgs {
            dataset_path: dataset,
            output_dir: output.clone(),
            parallel: 1,
            config: config_with_workdir(&repo),
            resume: false,
            cost_limit_usd: Some(0.50),
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
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
        },
        calibration_n: 2,
        seed: 7,
        target_n: Some(4),
        confidence_pct: 80.0,
    })
    .await
    .unwrap();

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
            &std::fs::read_to_string(output.join("forecast").join(format!("{id}.traj.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            traj.pointer("/info/purpose")
                .and_then(serde_json::Value::as_str),
            Some("forecast")
        );
    }

    let eval_err = rust_swe_agent::run::evaluate::run(&EvaluateArgs {
        sweep_dir: output.join("forecast"),
        dataset_path: None,
        backend: EvaluateBackend::None,
        timeout_per_instance_secs: 1,
        parallel: 1,
        sb_subset: "swe-bench-m".into(),
        sb_split: "dev".into(),
        run_id: None,
        breakdown: BreakdownSelection::none(),
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
