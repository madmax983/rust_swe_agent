//! Artifact schema compatibility and producer/reader contract tests.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use maxwells_daemon::Config;
use maxwells_daemon::artifact::{
    ArtifactKind, ArtifactSchemaVersion, CompatibilityClass, classify_json_value,
};
use maxwells_daemon::run::evaluate::{EvaluateArgs, EvaluateBackend};
use maxwells_daemon::run::forecast::{ForecastArgs, ForecastOutcome, forecast_from_results};
use maxwells_daemon::run::swebench::{
    InstanceResult, SwebenchArgs, SweepResults, run, trajectory_path_for_run,
};
use maxwells_daemon::trajectory::{Trajectory, outcome};

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

mod support;
use support::binary_path;

#[test]
fn artifact_schema_current_minor_bumped_for_replay_fingerprinting() {
    // Additive minor bumps: 1.4 added replay fingerprinting, 1.5 added
    // per-turn wall-clock attribution (model/tool/harness latency),
    // 1.6 added sweep-halt artifacts, 1.7 added render-only preview,
    // 1.8 added per-call sampling parameters (issue #177),
    // 1.9 added patch_error_log to InstanceEvaluation (issue #273).
    assert_eq!(
        ArtifactSchemaVersion::CURRENT,
        ArtifactSchemaVersion::new(1, 9)
    );
}

#[test]
fn artifact_classifier_marks_exact_current_version_supported_current() {
    let payload = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 9},
        "total": 0,
        "instances": []
    });

    let compat = classify_json_value(&payload, ArtifactKind::SweepResults, "results.json")
        .expect("current artifact should classify");

    assert_eq!(compat.kind, ArtifactKind::SweepResults);
    assert_eq!(compat.version, Some(ArtifactSchemaVersion::CURRENT));
    assert_eq!(compat.class, CompatibilityClass::SupportedCurrent);
    assert!(compat.warnings.is_empty());
}

#[test]
fn artifact_classifier_warns_for_pre_versioning_legacy_artifacts() {
    let payload = serde_json::json!({
        "total": 0,
        "instances": []
    });

    let compat = classify_json_value(&payload, ArtifactKind::SweepResults, "results.json")
        .expect("legacy artifact should remain supported");

    assert_eq!(compat.class, CompatibilityClass::SupportedLegacy);
    assert_eq!(compat.version, None);
    assert!(
        compat
            .warnings
            .iter()
            .any(|warning| warning.contains("pre-versioning legacy sweep_results")),
        "{compat:#?}"
    );
}

#[test]
fn artifact_classifier_rejects_unsupported_future_major_versions() {
    let payload = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 2, "minor": 0},
        "total": 0,
        "instances": []
    });

    let err = classify_json_value(&payload, ArtifactKind::SweepResults, "results.json")
        .expect_err("future major versions must fail fast");

    let message = err.to_string();
    assert!(
        message.contains("unsupported future artifact schema"),
        "{message}"
    );
    assert!(message.contains("sweep_results"), "{message}");
    assert!(message.contains("2.0"), "{message}");
}

#[test]
fn artifact_classifier_rejects_kind_mismatch() {
    let payload = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 3},
        "instances": []
    });

    let err = classify_json_value(&payload, ArtifactKind::SweepResults, "results.json")
        .expect_err("wrong artifact kind must be rejected");

    let message = err.to_string();
    assert!(message.contains("artifact kind mismatch"), "{message}");
    assert!(message.contains("expected sweep_results"), "{message}");
    assert!(message.contains("found evaluation_results"), "{message}");
}

#[test]
fn artifact_writer_pretty_matches_string_serializer() {
    let payload = serde_json::json!({
        "total": 1,
        "instances": [{"instance_id": "task-a"}]
    });
    let expected =
        maxwells_daemon::artifact::to_string_pretty(ArtifactKind::SweepResults, &payload).unwrap();
    let mut actual = Vec::new();

    maxwells_daemon::artifact::to_writer_pretty(&mut actual, ArtifactKind::SweepResults, &payload)
        .unwrap();

    assert_eq!(String::from_utf8(actual).unwrap(), expected);
}

#[test]
fn sweep_results_serialization_includes_dual_cost_metadata() {
    let json =
        maxwells_daemon::artifact::to_string_pretty(ArtifactKind::SweepResults, &fixture_results())
            .unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(value["actual_cost_usd"], 0.03);
    assert_eq!(value["actual_cost_source"], "rate_card_estimate");
    assert_eq!(value["baseline_cost_usd"], 0.00135);
    assert_eq!(value["baseline_cost_model"], "claude-3-5-sonnet");
}

#[test]
fn trajectory_serialization_includes_artifact_header() {
    let json = Trajectory::new().to_json_pretty().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(value["artifact_kind"], "trajectory");
    assert_eq!(
        value["schema_version"],
        serde_json::json!({"major": 1, "minor": 9})
    );
}

#[tokio::test]
async fn swebench_run_writes_versioned_results_and_prediction_metadata() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["task-a"]);
    let output = work.path().join("runs");

    run(base_args(
        dataset,
        output.clone(),
        config_with_workdir(&repo),
    ))
    .await
    .unwrap();

    let results: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("results.json")).unwrap())
            .unwrap();
    assert_eq!(results["artifact_kind"], "sweep_results");
    assert_eq!(
        results["schema_version"],
        serde_json::json!({"major": 1, "minor": 9})
    );

    let predictions = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    let first_prediction: serde_json::Value = serde_json::from_str(
        predictions
            .lines()
            .next()
            .expect("expected at least one SWE-bench prediction row"),
    )
    .unwrap();
    assert!(
        first_prediction.get("artifact_kind").is_none(),
        "SWE-bench prediction rows must stay evaluator-compatible: {first_prediction:#}"
    );
    assert!(first_prediction.get("schema_version").is_none());

    let metadata: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output.join("all_preds.metadata.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["artifact_kind"], "swebench_predictions_metadata");
    assert_eq!(
        metadata["schema_version"],
        serde_json::json!({"major": 1, "minor": 9})
    );
    assert_eq!(metadata["predictions_file"], "all_preds.jsonl");
    assert_eq!(metadata["row_count"], 1);
}

#[tokio::test]
async fn evaluate_run_writes_versioned_evaluation_json() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["task-a"]);
    let output = work.path().join("runs");

    run(base_args(
        dataset,
        output.clone(),
        config_with_workdir(&repo),
    ))
    .await
    .unwrap();
    maxwells_daemon::run::evaluate::run(&EvaluateArgs {
        sweep_dir: output.clone(),
        dataset_path: None,
        backend: EvaluateBackend::None,
        timeout_per_instance_secs: 1,
        parallel: 1,
        sb_subset: "verified".into(),
        sb_split: "test".into(),
        run_id: None,
        breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::none(),
        cost_attribution: false,
    })
    .unwrap();

    let eval: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("evaluation.json")).unwrap())
            .unwrap();
    assert_eq!(eval["artifact_kind"], "evaluation_results");
    assert_eq!(
        eval["schema_version"],
        serde_json::json!({"major": 1, "minor": 9})
    );
}

#[test]
fn forecast_json_includes_artifact_header() {
    let results = fixture_results();
    let report = forecast_from_results(&results, 42, 2, 1, 80.0, None).unwrap();

    let json = maxwells_daemon::run::forecast::to_json(&report).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(value["artifact_kind"], "forecast_report");
    assert_eq!(
        value["schema_version"],
        serde_json::json!({"major": 1, "minor": 9})
    );
}

#[tokio::test]
async fn forecast_run_keeps_calibration_results_versioned_after_manifest_mark() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["task-a"]);
    let output = work.path().join("runs");

    let outcome = maxwells_daemon::run::forecast::run(ForecastArgs {
        sweep: base_args(dataset, output.clone(), config_with_workdir(&repo)),
        calibration_n: 1,
        seed: 42,
        target_n: Some(1),
        confidence_pct: 80.0,
    })
    .await
    .unwrap();
    match outcome {
        ForecastOutcome::Report(_) => {}
        other => panic!("expected forecast report, got {other:?}"),
    }

    let calibration_results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output.join("forecast").join("results.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(calibration_results["artifact_kind"], "sweep_results");
    assert_eq!(
        calibration_results["schema_version"],
        serde_json::json!({"major": 1, "minor": 9})
    );
}

#[test]
fn tail_rejects_future_results_schema_before_printing_metrics() {
    let sweep = tempfile::tempdir().unwrap();
    write_results_value(sweep.path(), &versioned_results_value(2, 0));

    let out = Command::new(binary_path())
        .args([
            "bench",
            "tail",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--once",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsupported future artifact schema"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("Cost:"), "{stdout}");
    assert!(!stdout.contains("Progress:"), "{stdout}");
}

#[test]
fn inspect_warns_for_legacy_trajectory_schema() {
    let sweep = tempfile::tempdir().unwrap();
    write_legacy_trajectory(sweep.path(), "legacy");

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "legacy",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("pre-versioning legacy trajectory"),
        "{stdout}"
    );
    assert!(stdout.contains("warning:"), "{stdout}");
}

#[test]
fn inspect_rejects_future_trajectory_schema_before_printing_metrics() {
    let sweep = tempfile::tempdir().unwrap();
    write_future_trajectory(sweep.path(), "future");

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "future",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsupported future artifact schema"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("total_cost_usd"), "{stdout}");
}

#[test]
fn compare_surfaces_artifact_version_mismatch_before_metrics() {
    let baseline = tempfile::tempdir().unwrap();
    let candidate = tempfile::tempdir().unwrap();
    write_results_value(baseline.path(), &versioned_results_value(1, 0));
    write_results_value(candidate.path(), &legacy_results_value());

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline.path().to_str().unwrap(),
            "--candidate",
            candidate.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let artifact_pos = stdout
        .find("Artifact versions:")
        .unwrap_or_else(|| panic!("missing artifact version section:\n{stdout}"));
    let resolved_pos = stdout
        .find("Resolved:")
        .unwrap_or_else(|| panic!("missing resolved line:\n{stdout}"));
    assert!(artifact_pos < resolved_pos, "{stdout}");
    assert!(stdout.contains("sweep_results@1.0"), "{stdout}");
    assert!(stdout.contains("legacy-pre-versioning"), "{stdout}");
}

#[test]
fn evaluate_rejects_future_results_schema_before_writing_metrics() {
    let sweep = tempfile::tempdir().unwrap();
    write_results_value(sweep.path(), &versioned_results_value(2, 0));

    let out = Command::new(binary_path())
        .args([
            "bench",
            "evaluate",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--backend",
            "none",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsupported future artifact schema"),
        "{stderr}"
    );
    assert!(!sweep.path().join("evaluation.json").exists());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("resolved_rate"), "{stdout}");
}

#[test]
fn forecast_calibration_reader_rejects_future_results_schema_before_metrics() {
    let calibration = tempfile::tempdir().unwrap();
    write_results_value(calibration.path(), &versioned_results_value(2, 0));

    let err = maxwells_daemon::run::forecast::load_calibration_results(calibration.path())
        .expect_err("future calibration artifacts must fail before metrics");

    let message = err.to_string();
    assert!(
        message.contains("unsupported future artifact schema"),
        "{message}"
    );
    assert!(!message.contains("resolved_rate"), "{message}");
}

#[test]
fn forecast_calibration_reader_warns_for_legacy_results_schema() {
    let calibration = tempfile::tempdir().unwrap();
    write_results_value(calibration.path(), &legacy_results_value());

    let loaded = maxwells_daemon::run::forecast::load_calibration_results(calibration.path())
        .expect("legacy calibration artifacts should remain readable");

    assert_eq!(
        loaded.compatibility.class,
        CompatibilityClass::SupportedLegacy
    );
    assert_eq!(loaded.results.instances.len(), 1);
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("pre-versioning legacy sweep_results")),
        "{loaded:#?}"
    );
}

#[tokio::test]
async fn current_contract_fixtures_match_emitted_artifact_top_level_fields() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, &["task-a"]);
    let output = work.path().join("runs");

    run(base_args(
        dataset,
        output.clone(),
        config_with_workdir(&repo),
    ))
    .await
    .unwrap();

    assert_contract_shape_matches_fixture(
        "current/results.json",
        &read_json_file(&output.join("results.json")),
    );
    assert_contract_shape_matches_fixture(
        "current/trajectory.traj.json",
        &read_json_file(&trajectory_path_for_run(&output, "task-a", 1)),
    );
    assert_contract_shape_matches_fixture(
        "current/all_preds.metadata.json",
        &read_json_file(&output.join("all_preds.metadata.json")),
    );

    let eval = maxwells_daemon::run::evaluate::run(&EvaluateArgs {
        sweep_dir: output.clone(),
        dataset_path: None,
        backend: EvaluateBackend::None,
        timeout_per_instance_secs: 1,
        parallel: 1,
        sb_subset: "verified".into(),
        sb_split: "test".into(),
        run_id: None,
        breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::none(),
        cost_attribution: false,
    })
    .unwrap();
    let eval_json = serde_json::from_str(
        &maxwells_daemon::artifact::to_string_pretty(ArtifactKind::EvaluationResults, &eval)
            .unwrap(),
    )
    .unwrap();
    assert_contract_shape_matches_fixture("current/evaluation.json", &eval_json);

    let forecast = forecast_from_results(&fixture_results(), 42, 2, 1, 80.0, None).unwrap();
    let forecast_json =
        serde_json::from_str(&maxwells_daemon::run::forecast::to_json(&forecast).unwrap()).unwrap();
    assert_contract_shape_matches_fixture("current/forecast.json", &forecast_json);

    let preflight = serde_json::json!({
        "artifact_kind": "preflight_report",
        "schema_version": {"major": 1, "minor": 9},
        "mode": "doctor",
        "checks": [{
            "status": "ok",
            "name": "dataset.read",
            "message": "readable"
        }]
    });
    assert_contract_shape_matches_fixture("current/preflight.json", &preflight);
}

#[test]
fn checked_in_fixtures_cover_legacy_current_and_future_artifact_contracts() {
    let root = fixture_root();
    let supported = [
        (
            "legacy/results.json",
            ArtifactKind::SweepResults,
            CompatibilityClass::SupportedLegacy,
        ),
        (
            "legacy/evaluation.json",
            ArtifactKind::EvaluationResults,
            CompatibilityClass::SupportedLegacy,
        ),
        (
            "legacy/trajectory.traj.json",
            ArtifactKind::Trajectory,
            CompatibilityClass::SupportedLegacy,
        ),
        (
            "legacy/forecast.json",
            ArtifactKind::ForecastReport,
            CompatibilityClass::SupportedLegacy,
        ),
        (
            "legacy/preflight.json",
            ArtifactKind::PreflightReport,
            CompatibilityClass::SupportedLegacy,
        ),
        (
            "legacy/all_preds.metadata.json",
            ArtifactKind::SwebenchPredictionsMetadata,
            CompatibilityClass::SupportedLegacy,
        ),
        (
            "current/results.json",
            ArtifactKind::SweepResults,
            CompatibilityClass::SupportedCurrent,
        ),
        (
            "current/evaluation.json",
            ArtifactKind::EvaluationResults,
            CompatibilityClass::SupportedCurrent,
        ),
        (
            "current/trajectory.traj.json",
            ArtifactKind::Trajectory,
            CompatibilityClass::SupportedCurrent,
        ),
        (
            "current/forecast.json",
            ArtifactKind::ForecastReport,
            CompatibilityClass::SupportedCurrent,
        ),
        (
            "current/calibration.json",
            ArtifactKind::CalibrationReport,
            CompatibilityClass::SupportedCurrent,
        ),
        (
            "current/preflight.json",
            ArtifactKind::PreflightReport,
            CompatibilityClass::SupportedCurrent,
        ),
        (
            "current/all_preds.metadata.json",
            ArtifactKind::SwebenchPredictionsMetadata,
            CompatibilityClass::SupportedCurrent,
        ),
    ];

    for (relative, kind, expected_class) in supported {
        let path = root.join(relative);
        let value = read_fixture_json(&path);
        let compat = classify_json_value(&value, kind, path.display().to_string())
            .unwrap_or_else(|err| panic!("fixture {relative} should classify: {err}"));
        assert_eq!(compat.class, expected_class, "{relative}");
    }

    let future = read_fixture_json(&root.join("future/results.json"));
    let err = classify_json_value(&future, ArtifactKind::SweepResults, "future/results.json")
        .expect_err("future major fixture should be unsupported");
    assert!(
        err.to_string()
            .contains("unsupported future artifact schema"),
        "{err}"
    );
}

#[test]
fn checked_in_fixture_sweeps_load_through_public_reader_commands() {
    let root = fixture_root();
    for fixture in ["legacy", "current"] {
        let sweep = root.join(fixture).join("sweep");
        let tail = Command::new(binary_path())
            .args([
                "bench",
                "tail",
                "--sweep",
                sweep.to_str().unwrap(),
                "--once",
            ])
            .output()
            .unwrap();
        assert!(
            tail.status.success(),
            "{fixture} tail stderr: {}",
            String::from_utf8_lossy(&tail.stderr)
        );

        let inspect = Command::new(binary_path())
            .args([
                "bench",
                "inspect",
                "--sweep",
                sweep.to_str().unwrap(),
                "--instance",
                "task-a",
            ])
            .output()
            .unwrap();
        assert!(
            inspect.status.success(),
            "{fixture} inspect stderr: {}",
            String::from_utf8_lossy(&inspect.stderr)
        );
    }

    let compare = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            root.join("legacy").join("sweep").to_str().unwrap(),
            "--candidate",
            root.join("current").join("sweep").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        compare.status.success(),
        "compare stderr: {}",
        String::from_utf8_lossy(&compare.stderr)
    );

    let eval_tmp = tempfile::tempdir().unwrap();
    copy_dir(&root.join("current").join("sweep"), eval_tmp.path());
    let evaluate = Command::new(binary_path())
        .args([
            "bench",
            "evaluate",
            "--sweep",
            eval_tmp.path().to_str().unwrap(),
            "--backend",
            "none",
        ])
        .output()
        .unwrap();
    assert!(
        evaluate.status.success(),
        "evaluate stderr: {}",
        String::from_utf8_lossy(&evaluate.stderr)
    );
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
    let mut cfg = Config::defaults().unwrap();
    cfg.root.environment.workdir = dir.display().to_string();
    cfg
}

fn base_args(dataset: std::path::PathBuf, output: std::path::PathBuf, cfg: Config) -> SwebenchArgs {
    SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
        output_dir: output,
        parallel: 1,
        config: cfg,
        resume: false,
        reruns: 1,
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
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ]),
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

fn instance(id: &str, input_tokens: u64, output_tokens: u64, cost_usd: f64) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(1),
        cost_usd: Some(cost_usd),
        prompt_tokens: Some(input_tokens),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(output_tokens),
        duration_secs: Some(1.0),
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: false,
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
    }
}

fn fixture_results() -> SweepResults {
    let instances = vec![instance("a", 100, 10, 0.01), instance("b", 200, 20, 0.02)];
    SweepResults {
        total: instances.len(),
        sweep_status: maxwells_daemon::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: instances.len(),
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: Default::default(),
        budget_halted: 0,
        with_patch: instances.len(),
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 300,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 30,
        estimated_cost_usd: 0.03,
        actual_cost_usd: Some(0.03),
        actual_cost_source: Some(maxwells_daemon::cost::CostSource::RateCardEstimate),
        baseline_cost_usd: Some(0.00135),
        baseline_cost_model: Some("claude-3-5-sonnet".into()),
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: 1.0,
        filter_spec: Default::default(),
        manifest: None,
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,

        total_fallbacks: 0,

        model_mix: std::collections::BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: vec![],
    }
}

fn legacy_results_value() -> serde_json::Value {
    serde_json::json!({
        "total": 1,
        "sweep_status": "completed",
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "failures_by_category": {},
        "budget_halted": 0,
        "with_patch": 1,
        "total_prompt_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "instances": [{
            "instance_id": "task-a",
            "exit_reason": "submitted",
            "outcome": "submitted",
            "cost_usd": 0.0,
            "patch_present": true,
            "non_empty_patch": false
        }]
    })
}

fn versioned_results_value(major: u16, minor: u16) -> serde_json::Value {
    let mut value = legacy_results_value();
    let object = value.as_object_mut().unwrap();
    object.insert("artifact_kind".into(), serde_json::json!("sweep_results"));
    object.insert(
        "schema_version".into(),
        serde_json::json!({"major": major, "minor": minor}),
    );
    value
}

fn write_results_value(dir: &Path, value: &serde_json::Value) {
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn write_legacy_trajectory(dir: &Path, instance_id: &str) {
    let value = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.1",
        "info": {
            "model_name": "deterministic-test",
            "outcome": "submitted",
            "total_cost_usd": 0.0
        },
        "messages": []
    });
    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn write_future_trajectory(dir: &Path, instance_id: &str) {
    let value = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.1",
        "artifact_kind": "trajectory",
        "schema_version": {"major": 2, "minor": 0},
        "info": {
            "model_name": "deterministic-test",
            "outcome": "submitted",
            "total_cost_usd": 0.0
        },
        "messages": []
    });
    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("artifact_schema")
}

fn read_fixture_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn read_json_file(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn assert_contract_shape_matches_fixture(relative: &str, actual: &serde_json::Value) {
    let fixture = read_fixture_json(&fixture_root().join(relative));
    let expected_shape = json_shape(&fixture);
    let actual_shape = json_shape(actual);
    assert_eq!(
        actual_shape, expected_shape,
        "{relative} serialized artifact contract drifted; update the current fixture and docs/artifact-contract.md, or bump schema_version"
    );
}

fn json_shape(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Null => serde_json::json!("null"),
        serde_json::Value::Bool(_) => serde_json::json!("bool"),
        serde_json::Value::Number(_) => serde_json::json!("number"),
        serde_json::Value::String(_) => serde_json::json!("string"),
        serde_json::Value::Array(items) => items.first().map_or_else(
            || serde_json::json!([]),
            |first| serde_json::json!([json_shape(first)]),
        ),
        serde_json::Value::Object(object) => {
            let mut shape = serde_json::Map::new();
            for (key, nested) in object {
                shape.insert(key.clone(), json_shape(nested));
            }
            serde_json::Value::Object(shape)
        }
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let source = entry.path();
        let target = dst.join(entry.file_name());
        if source.is_dir() {
            copy_dir(&source, &target);
        } else {
            std::fs::copy(&source, &target).unwrap();
        }
    }
}
