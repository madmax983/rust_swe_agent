//! `bench calibrate`: forecast-vs-actual calibration reports.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use maxwells_daemon::artifact::ArtifactKind;
use maxwells_daemon::run::calibrate::{
    CalibrationArgs, CalibrationMetricStatus, CalibrationVerdict, compute, render_text, to_json,
};
use maxwells_daemon::run::forecast::{
    CalibrationSummary, ForecastReport, ForecastTotals, IntervalEstimate, PerInstanceSummary,
    QuantileSummary, ResolutionRateSignal, ThresholdCheck, ThresholdStatus,
};
use maxwells_daemon::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest, FilterSpec, HarnessManifest, InstanceResult,
    ModelManifest, ProvenanceManifest, RuntimeManifest, SweepResults,
};
use maxwells_daemon::trajectory::{FailureCategory, outcome};

mod support;
use support::binary_path;

#[test]
fn actuals_inside_intervals_are_well_calibrated() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase::default(),
        ResultsCase::default(),
        ManifestCase::default(),
        ManifestCase::default(),
    );

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert_eq!(report.verdict, CalibrationVerdict::WellCalibrated);
    assert_eq!(
        report.metrics.total_usd.status,
        CalibrationMetricStatus::WithinInterval
    );
    assert_eq!(
        report.metrics.total_input_tokens.status,
        CalibrationMetricStatus::WithinInterval
    );
    assert_eq!(
        report.metrics.total_output_tokens.status,
        CalibrationMetricStatus::WithinInterval
    );
    assert_eq!(
        report.metrics.wall_clock_seconds.status,
        CalibrationMetricStatus::WithinInterval
    );
    assert_eq!(
        report.metrics.resolution_rate.status,
        CalibrationMetricStatus::WithinInterval
    );
    assert!(report.mismatches.is_empty(), "{:#?}", report.mismatches);
    assert!(render_text(&report).contains("Verdict:            well_calibrated"));
}

#[test]
fn actual_above_upper_interval_is_optimistic_with_signed_relative_error() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase::default(),
        ResultsCase {
            total_cost_usd: 15.0,
            ..ResultsCase::default()
        },
        ManifestCase::default(),
        ManifestCase::default(),
    );

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert_eq!(report.verdict, CalibrationVerdict::Optimistic);
    assert_eq!(
        report.metrics.total_usd.status,
        CalibrationMetricStatus::OverUpper
    );
    assert_eq!(report.metrics.total_usd.absolute_error, 5.0);
    assert_eq!(report.metrics.total_usd.relative_error, Some(0.5));
}

#[test]
fn actual_below_lower_interval_is_pessimistic() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase::default(),
        ResultsCase {
            total_cost_usd: 5.0,
            input_tokens: 500,
            output_tokens: 50,
            wall_clock_secs: 50.0,
            resolved: 1,
            total: 4,
            ..ResultsCase::default()
        },
        ManifestCase::default(),
        ManifestCase::default(),
    );

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert_eq!(report.verdict, CalibrationVerdict::Pessimistic);
    assert_eq!(
        report.metrics.total_usd.status,
        CalibrationMetricStatus::UnderLower
    );
    assert_eq!(
        report.metrics.total_input_tokens.status,
        CalibrationMetricStatus::UnderLower
    );
    assert_eq!(
        report.metrics.wall_clock_seconds.status,
        CalibrationMetricStatus::UnderLower
    );
}

#[test]
fn dataset_model_parallel_and_instance_mismatches_are_flagged() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase::default(),
        ResultsCase {
            total: 3,
            ..ResultsCase::default()
        },
        ManifestCase::default(),
        ManifestCase {
            dataset_sha256: "sha256:other".into(),
            model: "other-model".into(),
            parallel: 8,
            ..ManifestCase::default()
        },
    );

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert_eq!(report.verdict, CalibrationVerdict::NotComparable);
    let fields: Vec<_> = report.mismatches.iter().map(|m| m.field.as_str()).collect();
    assert!(
        fields.contains(&"dataset.sha256"),
        "{:#?}",
        report.mismatches
    );
    assert!(fields.contains(&"model.name"), "{:#?}", report.mismatches);
    assert!(fields.contains(&"parallel"), "{:#?}", report.mismatches);
    assert!(
        fields.contains(&"instance_count"),
        "{:#?}",
        report.mismatches
    );
}

#[test]
fn relative_calibration_output_dir_is_resolved_from_current_cwd_first() {
    let cwd = std::env::current_dir().unwrap();
    let work = tempfile::tempdir_in(cwd.join("target")).unwrap();
    let rel_work = work.path().strip_prefix(&cwd).unwrap();
    let runs_dir = rel_work.join("runs");
    let forecast_path = runs_dir.join("forecast.json");
    let results_path = runs_dir.join("results.json");
    let calibration_dir = runs_dir.join("forecast").join("forecast");

    std::fs::create_dir_all(cwd.join(&calibration_dir)).unwrap();

    let forecast_report = forecast_report(&ForecastCase::default(), &calibration_dir);
    std::fs::write(
        cwd.join(&forecast_path),
        maxwells_daemon::run::forecast::to_json(&forecast_report).unwrap(),
    )
    .unwrap();

    let calibration_results = sweep_results(
        ResultsCase {
            total: 2,
            resolved: 2,
            ..ResultsCase::default()
        },
        ManifestCase::default(),
        &["a".to_owned(), "b".to_owned()],
    );
    std::fs::write(
        cwd.join(&calibration_dir).join("results.json"),
        maxwells_daemon::artifact::to_string_pretty(
            ArtifactKind::SweepResults,
            &calibration_results,
        )
        .unwrap(),
    )
    .unwrap();

    let actual = sweep_results(
        ResultsCase::default(),
        ManifestCase {
            model: "other-model".into(),
            ..ManifestCase::default()
        },
        &[
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ],
    );
    std::fs::write(
        cwd.join(&results_path),
        maxwells_daemon::artifact::to_string_pretty(ArtifactKind::SweepResults, &actual).unwrap(),
    )
    .unwrap();

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert!(
        report
            .mismatches
            .iter()
            .any(|mismatch| mismatch.field == "model.name"),
        "{:#?}",
        report.mismatches
    );
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| !warning.contains("forecast calibration results unavailable")),
        "{:#?}",
        report.warnings
    );
}

#[test]
fn parallel_mismatch_is_detected_from_equals_and_short_manifest_forms() {
    for parallel_arg_style in [
        ParallelArgStyle::LongEquals,
        ParallelArgStyle::ShortSeparated,
    ] {
        let work = tempfile::tempdir().unwrap();
        let (forecast_path, results_path) = write_pair(
            work.path(),
            ForecastCase::default(),
            ResultsCase::default(),
            ManifestCase::default(),
            ManifestCase {
                parallel: 8,
                parallel_arg_style,
                ..ManifestCase::default()
            },
        );

        let report = compute(&CalibrationArgs {
            forecast_path,
            results_path,
        })
        .unwrap();

        assert_eq!(report.verdict, CalibrationVerdict::NotComparable);
        assert!(
            report
                .mismatches
                .iter()
                .any(|mismatch| mismatch.field == "parallel"),
            "{parallel_arg_style:?}: {:#?}",
            report.mismatches
        );
    }
}

#[test]
fn absent_parallel_arg_defaults_to_cli_parallel_for_comparison() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase {
            parallel: 8,
            ..ForecastCase::default()
        },
        ResultsCase::default(),
        ManifestCase::default(),
        ManifestCase {
            parallel: 4,
            parallel_arg_style: ParallelArgStyle::Absent,
            ..ManifestCase::default()
        },
    );

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert_eq!(report.verdict, CalibrationVerdict::NotComparable);
    let mismatch = report
        .mismatches
        .iter()
        .find(|mismatch| mismatch.field == "parallel")
        .unwrap_or_else(|| panic!("{:#?}", report.mismatches));
    assert_eq!(mismatch.forecast, "8");
    assert_eq!(mismatch.actual, "4");
}

#[test]
fn exact_target_instance_set_mismatch_is_flagged_even_when_counts_match() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase {
            target_instance_ids: vec!["w".into(), "x".into(), "y".into(), "z".into()],
            ..ForecastCase::default()
        },
        ResultsCase::default(),
        ManifestCase::default(),
        ManifestCase::default(),
    );

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert_eq!(report.verdict, CalibrationVerdict::NotComparable);
    assert!(
        report
            .mismatches
            .iter()
            .any(|mismatch| mismatch.field == "instance_set"),
        "{:#?}",
        report.mismatches
    );
}

#[test]
fn actual_resolution_rate_uses_pass_at_1_semantics_for_rerun_rows() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase {
            total_cost_usd: interval(5.0, 4.0, 6.0),
            input_tokens: interval(500.0, 400.0, 600.0),
            output_tokens: interval(50.0, 40.0, 60.0),
            wall_clock_secs: interval(50.0, 40.0, 60.0),
            resolution_point: 0.0,
            resolution_resolved: 0,
            resolution_total: 2,
            target_n: 2,
            target_instance_ids: vec!["a".into(), "b".into()],
            ..ForecastCase::default()
        },
        ResultsCase {
            total_cost_usd: 5.0,
            input_tokens: 500,
            output_tokens: 50,
            wall_clock_secs: 50.0,
            resolved: 0,
            total: 2,
            runs: 2,
            resolved_count: Some(1),
            pass_at_1: Some(false),
        },
        ManifestCase::default(),
        ManifestCase::default(),
    );

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert_eq!(report.metrics.resolution_rate.actual, 0.0);
    assert_eq!(
        report.metrics.resolution_rate.status,
        CalibrationMetricStatus::WithinInterval
    );
    assert_eq!(report.verdict, CalibrationVerdict::WellCalibrated);
}

#[test]
fn small_n_zero_resolution_signal_uses_nonzero_interval() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase {
            resolution_point: 0.0,
            resolution_resolved: 0,
            resolution_total: 2,
            ..ForecastCase::default()
        },
        ResultsCase {
            resolved: 1,
            total: 4,
            ..ResultsCase::default()
        },
        ManifestCase::default(),
        ManifestCase::default(),
    );

    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    assert_eq!(
        report.metrics.resolution_rate.status,
        CalibrationMetricStatus::WithinInterval
    );
    assert!(
        report.metrics.resolution_rate.forecast.upper > 0.2,
        "{:#?}",
        report.metrics.resolution_rate
    );
    assert_eq!(report.verdict, CalibrationVerdict::WellCalibrated);
}

#[test]
fn calibration_json_shape_is_stable() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase::default(),
        ResultsCase::default(),
        ManifestCase::default(),
        ManifestCase::default(),
    );
    let report = compute(&CalibrationArgs {
        forecast_path,
        results_path,
    })
    .unwrap();

    let actual: serde_json::Value = serde_json::from_str(&to_json(&report).unwrap()).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("artifact_schema")
                .join("current")
                .join("calibration.json"),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(json_shape(&actual), json_shape(&fixture));
    assert_eq!(actual["artifact_kind"], "calibration_report");
}

#[test]
fn cli_exposes_calibrate_and_writes_json_artifact() {
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("calibration.json");
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase::default(),
        ResultsCase::default(),
        ManifestCase::default(),
        ManifestCase::default(),
    );

    let help = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("calibrate"),
        "bench help should list calibrate"
    );

    let run = Command::new(binary_path())
        .args([
            "bench",
            "calibrate",
            "--forecast",
            forecast_path.to_str().unwrap(),
            "--results",
            results_path.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        run.status.success(),
        "calibrate failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8(run.stdout).unwrap();
    assert!(stdout.contains("=== bench calibrate ==="), "{stdout}");
    assert!(
        output.exists(),
        "expected JSON artifact at {}",
        output.display()
    );
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
    assert_eq!(json["verdict"], "well_calibrated");
}

#[test]
fn cli_missing_input_path_is_usage_error() {
    let work = tempfile::tempdir().unwrap();
    let run = Command::new(binary_path())
        .args([
            "bench",
            "calibrate",
            "--forecast",
            work.path().join("missing-forecast.json").to_str().unwrap(),
            "--results",
            work.path().join("missing-results.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!run.status.success());
    assert_eq!(run.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("outcome_class: usage_error"), "{stderr}");
    assert!(stderr.contains("forecast artifact not found"), "{stderr}");
}

#[test]
fn cli_can_gate_on_optimistic_verdict_with_stable_outcome_class() {
    let work = tempfile::tempdir().unwrap();
    let (forecast_path, results_path) = write_pair(
        work.path(),
        ForecastCase::default(),
        ResultsCase {
            total_cost_usd: 15.0,
            ..ResultsCase::default()
        },
        ManifestCase::default(),
        ManifestCase::default(),
    );

    let run = Command::new(binary_path())
        .args([
            "bench",
            "calibrate",
            "--forecast",
            forecast_path.to_str().unwrap(),
            "--results",
            results_path.to_str().unwrap(),
            "--fail-on-optimistic",
        ])
        .output()
        .unwrap();

    assert!(!run.status.success());
    assert_eq!(run.status.code(), Some(8));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("outcome_class: calibration_optimistic"),
        "{stderr}"
    );
}

#[derive(Debug, Clone)]
struct ForecastCase {
    total_cost_usd: IntervalEstimate,
    input_tokens: IntervalEstimate,
    output_tokens: IntervalEstimate,
    wall_clock_secs: IntervalEstimate,
    resolution_point: f64,
    resolution_resolved: usize,
    resolution_total: usize,
    calibration_ids: Vec<String>,
    target_n: usize,
    target_instance_ids: Vec<String>,
    parallel: usize,
}

impl Default for ForecastCase {
    fn default() -> Self {
        Self {
            total_cost_usd: interval(10.0, 8.0, 12.0),
            input_tokens: interval(1000.0, 800.0, 1200.0),
            output_tokens: interval(100.0, 80.0, 120.0),
            wall_clock_secs: interval(100.0, 80.0, 120.0),
            resolution_point: 0.5,
            resolution_resolved: 1,
            resolution_total: 2,
            calibration_ids: vec!["a".into(), "b".into()],
            target_n: 4,
            target_instance_ids: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            parallel: 4,
        }
    }
}

#[derive(Debug, Clone)]
struct ResultsCase {
    total_cost_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
    wall_clock_secs: f64,
    resolved: usize,
    total: usize,
    runs: u32,
    resolved_count: Option<u32>,
    pass_at_1: Option<bool>,
}

impl Default for ResultsCase {
    fn default() -> Self {
        Self {
            total_cost_usd: 10.5,
            input_tokens: 1050,
            output_tokens: 105,
            wall_clock_secs: 105.0,
            resolved: 2,
            total: 4,
            runs: 1,
            resolved_count: None,
            pass_at_1: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ManifestCase {
    dataset_sha256: String,
    model: String,
    parallel: usize,
    parallel_arg_style: ParallelArgStyle,
}

impl Default for ManifestCase {
    fn default() -> Self {
        Self {
            dataset_sha256: "sha256:dataset".into(),
            model: "fixture-model".into(),
            parallel: 4,
            parallel_arg_style: ParallelArgStyle::LongSeparated,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ParallelArgStyle {
    Absent,
    LongSeparated,
    LongEquals,
    ShortSeparated,
}

impl ParallelArgStyle {
    fn argv(self, parallel: usize) -> Vec<String> {
        match self {
            Self::Absent => vec!["max".into(), "bench".into(), "swebench".into()],
            Self::LongSeparated => vec![
                "max".into(),
                "bench".into(),
                "swebench".into(),
                "--parallel".into(),
                parallel.to_string(),
            ],
            Self::LongEquals => vec![
                "max".into(),
                "bench".into(),
                "swebench".into(),
                format!("--parallel={parallel}"),
            ],
            Self::ShortSeparated => vec![
                "max".into(),
                "bench".into(),
                "swebench".into(),
                "-p".into(),
                parallel.to_string(),
            ],
        }
    }
}

fn write_pair(
    dir: &Path,
    forecast: ForecastCase,
    results: ResultsCase,
    forecast_manifest: ManifestCase,
    results_manifest: ManifestCase,
) -> (PathBuf, PathBuf) {
    let calibration_dir = dir.join("forecast");
    std::fs::create_dir_all(&calibration_dir).unwrap();
    let forecast_path = dir.join("forecast.json");
    let results_path = dir.join("results.json");

    let forecast_report = forecast_report(&forecast, &calibration_dir);
    std::fs::write(
        &forecast_path,
        maxwells_daemon::run::forecast::to_json(&forecast_report).unwrap(),
    )
    .unwrap();

    let calibration_results = sweep_results(
        ResultsCase {
            total: forecast.calibration_ids.len(),
            resolved: forecast.calibration_ids.len(),
            ..ResultsCase::default()
        },
        forecast_manifest,
        &forecast.calibration_ids,
    );
    std::fs::write(
        calibration_dir.join("results.json"),
        maxwells_daemon::artifact::to_string_pretty(
            ArtifactKind::SweepResults,
            &calibration_results,
        )
        .unwrap(),
    )
    .unwrap();

    let actual_ids = match results.total {
        0 => Vec::new(),
        1 => vec!["a".to_owned()],
        2 => vec!["a".to_owned(), "b".to_owned()],
        3 => vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        _ => vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ],
    };
    let actual = sweep_results(results, results_manifest, &actual_ids);
    std::fs::write(
        &results_path,
        maxwells_daemon::artifact::to_string_pretty(ArtifactKind::SweepResults, &actual).unwrap(),
    )
    .unwrap();

    (forecast_path, results_path)
}

fn forecast_report(case: &ForecastCase, calibration_dir: &Path) -> ForecastReport {
    ForecastReport {
        calibration: CalibrationSummary {
            n: case.calibration_ids.len(),
            seed: 42,
            output_dir: calibration_dir.display().to_string(),
            instance_ids: case.calibration_ids.clone(),
        },
        per_instance: PerInstanceSummary {
            input_tokens: quantiles(200.0, 250.0, 300.0),
            output_tokens: quantiles(20.0, 25.0, 30.0),
            usd_cost: quantiles(2.0, 2.5, 3.0),
            step_count: quantiles(1.0, 2.0, 3.0),
            wall_clock_seconds: quantiles(20.0, 25.0, 30.0),
        },
        forecast: ForecastTotals {
            target_n: case.target_n,
            target_instance_ids: case.target_instance_ids.clone(),
            parallel: case.parallel,
            confidence_pct: 80.0,
            total_cost_usd: case.total_cost_usd,
            total_input_tokens: case.input_tokens,
            total_output_tokens: case.output_tokens,
            wall_clock_seconds: case.wall_clock_secs,
        },
        resolution_rate: ResolutionRateSignal {
            resolved: case.resolution_resolved,
            total: case.resolution_total,
            point: case.resolution_point,
            disclaimer: "fixture".into(),
        },
        threshold: ThresholdCheck {
            limit_usd: None,
            status: ThresholdStatus::NotConfigured,
            message: "no sweep cost limit configured".into(),
        },
    }
}

fn sweep_results(
    case: ResultsCase,
    manifest: ManifestCase,
    instance_ids: &[String],
) -> SweepResults {
    let instances: Vec<_> = instance_ids
        .iter()
        .enumerate()
        .map(|(idx, id)| {
            let submitted = idx < case.resolved;
            let per_instance_cost = if case.total == 0 {
                0.0
            } else {
                case.total_cost_usd / case.total as f64
            };
            let mut row = instance(
                id,
                submitted,
                per_instance_cost,
                case.wall_clock_secs / case.total.max(1) as f64,
            );
            row.runs = case.runs;
            row.resolved_count = case.resolved_count.unwrap_or_else(|| u32::from(submitted));
            row.pass_at_1 = case.pass_at_1.unwrap_or(submitted);
            row
        })
        .collect();
    SweepResults {
        total: case.total,
        sweep_status: maxwells_daemon::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: case.total,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: case.resolved,
        submitted_with_tests: 0,
        skipped: 0,
        errored: case.total.saturating_sub(case.resolved),
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: case.resolved,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: case.input_tokens,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: case.output_tokens,
        estimated_cost_usd: case.total_cost_usd,
        actual_cost_usd: Some(case.total_cost_usd),
        actual_cost_source: None,
        baseline_cost_usd: None,
        baseline_cost_model: None,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: if case.total == 0 {
            0.0
        } else {
            case.resolved as f64 / case.total as f64
        },
        filter_spec: FilterSpec {
            original_count: case.total,
            selected_count: case.total,
            instance_ids: Some(instance_ids.to_vec()),
            ..FilterSpec::default()
        },
        manifest: Some(manifest_for(manifest, case.total, case.wall_clock_secs)),
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: vec![],
        partial: 0,
        span_export_dropped: 0,
    }
}

fn instance(id: &str, submitted: bool, cost_usd: f64, duration_secs: f64) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: if submitted { "submitted" } else { "error" }.into(),
        outcome: Some(if submitted {
            outcome::SUBMITTED.into()
        } else {
            outcome::ERROR.into()
        }),
        failure_category: (!submitted).then_some(FailureCategory::Unknown),
        steps: Some(1),
        cost_usd: Some(cost_usd),
        prompt_tokens: Some(250),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(25),
        duration_secs: Some(duration_secs),
        error: None,
        github_pr_error: None,
        patch_present: submitted,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: u32::from(submitted),
        pass_at_1: submitted,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
        trace_id: None,
    }
}

fn manifest_for(case: ManifestCase, total: usize, wall_clock_secs: f64) -> ProvenanceManifest {
    ProvenanceManifest {
        purpose: None,
        harness: HarnessManifest {
            name: "maxwells-daemon".into(),
            version: "test".into(),
            git_sha: None,
            git_dirty: None,
            git_resolution: "test".into(),
        },
        dataset: DatasetManifest {
            path: "dataset.jsonl".into(),
            sha256: case.dataset_sha256,
            instance_count: total,
            filter_spec: Some(FilterSpec {
                original_count: total,
                selected_count: total,
                ..FilterSpec::default()
            }),
            source_kind: "local".into(),
            source_revision: Some("sha256:dataset".into()),
            selected_row_count: total,
            post_filter_row_count: total,
            ..DatasetManifest::default()
        },
        prompt_template: maxwells_daemon::run::swebench::PromptTemplateManifest {
            source: "inline".into(),
            path: None,
            sha256: "prompt".into(),
        },
        config: ConfigManifest {
            resolved: "test".into(),
            overlay_paths: Vec::new(),
        },
        model: ModelManifest {
            name: case.model,
            backend: "deterministic".into(),
            backend_version: None,
            base_url: None,
        },
        runtime: RuntimeManifest {
            started_at_utc: "2026-05-01T00:00:00Z".into(),
            finished_at_utc: Some(format!(
                "2026-05-01T00:{:02}:{:02}Z",
                (wall_clock_secs / 60.0).floor() as u64,
                (wall_clock_secs % 60.0).round() as u64
            )),
            host_os: "test".into(),
            resume_mode: false,
            rust_version: None,
        },
        cli: CliManifest {
            argv: case.parallel_arg_style.argv(case.parallel),
        },
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
    }
}

fn interval(point: f64, lower: f64, upper: f64) -> IntervalEstimate {
    IntervalEstimate {
        point,
        lower,
        upper,
    }
}

fn quantiles(p10: f64, median: f64, p90: f64) -> QuantileSummary {
    QuantileSummary { p10, median, p90 }
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
