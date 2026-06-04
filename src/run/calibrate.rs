//! `bench calibrate`: compare a forecast artifact against a completed sweep.
//!
//! This is deliberately read-only over existing artifacts. The forecast is the
//! promise; the sweep results are the bill. No model calls, no runner state,
//! no fresh astrology.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::artifact::{ArtifactCompatibility, ArtifactKind, classify_json_value};
use crate::error::{ConfigError, Error};
use crate::run::forecast::{ForecastReport, IntervalEstimate, QuantileSummary};
use crate::run::swebench::{self, ProvenanceManifest, SweepResults};

/// Inputs for a calibration comparison.
#[derive(Debug, Clone)]
pub struct CalibrationArgs {
    pub forecast_path: PathBuf,
    pub results_path: PathBuf,
}

/// Durable forecast-vs-actual report.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationReport {
    pub forecast_path: String,
    pub results_path: String,
    pub forecast_artifact: String,
    pub results_artifact: String,
    pub verdict: CalibrationVerdict,
    pub comparability: CalibrationComparability,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mismatches: Vec<CalibrationMismatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub forecast_target_n: usize,
    pub actual_instance_count: usize,
    pub metrics: CalibrationMetrics,
    pub per_instance: PerInstanceCalibration,
}

/// Overall sweep calibration result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationVerdict {
    WellCalibrated,
    Optimistic,
    Pessimistic,
    NotComparable,
}

impl CalibrationVerdict {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WellCalibrated => "well_calibrated",
            Self::Optimistic => "optimistic",
            Self::Pessimistic => "pessimistic",
            Self::NotComparable => "not_comparable",
        }
    }
}

/// Whether forecast and actual artifacts appear to describe the same sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationComparability {
    Matched,
    Mismatched,
}

/// Forecast/result provenance difference that makes the comparison suspect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CalibrationMismatch {
    pub field: String,
    pub forecast: String,
    pub actual: String,
}

/// Scalar aggregate metrics whose forecast uses a confidence interval.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationMetrics {
    pub total_usd: ScalarCalibration,
    pub total_input_tokens: ScalarCalibration,
    pub total_output_tokens: ScalarCalibration,
    pub wall_clock_seconds: ScalarCalibration,
    pub resolution_rate: ScalarCalibration,
}

impl CalibrationMetrics {
    fn statuses(&self) -> impl Iterator<Item = CalibrationMetricStatus> + '_ {
        [
            self.total_usd.status,
            self.total_input_tokens.status,
            self.total_output_tokens.status,
            self.wall_clock_seconds.status,
            self.resolution_rate.status,
        ]
        .into_iter()
    }
}

/// Per-instance distribution diagnostics from the forecast's p10/median/p90
/// summaries compared with the actual sweep's observed p10/median/p90.
#[derive(Debug, Clone, Serialize)]
pub struct PerInstanceCalibration {
    pub input_tokens: DistributionCalibration,
    pub output_tokens: DistributionCalibration,
    pub usd_cost: DistributionCalibration,
    pub step_count: DistributionCalibration,
    pub wall_clock_seconds: DistributionCalibration,
}

impl PerInstanceCalibration {
    fn statuses(&self) -> impl Iterator<Item = CalibrationMetricStatus> + '_ {
        [
            self.input_tokens.status,
            self.output_tokens.status,
            self.usd_cost.status,
            self.step_count.status,
            self.wall_clock_seconds.status,
        ]
        .into_iter()
    }
}

/// Classification against the forecast interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationMetricStatus {
    WithinInterval,
    OverUpper,
    UnderLower,
}

impl CalibrationMetricStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::WithinInterval => "within_interval",
            Self::OverUpper => "over_upper",
            Self::UnderLower => "under_lower",
        }
    }
}

/// Scalar forecast-vs-actual comparison.
#[derive(Debug, Clone, Serialize)]
pub struct ScalarCalibration {
    pub forecast: IntervalEstimate,
    pub actual: f64,
    pub status: CalibrationMetricStatus,
    pub absolute_error: f64,
    pub relative_error: Option<f64>,
}

impl ScalarCalibration {
    fn new(forecast: IntervalEstimate, actual: f64) -> Self {
        Self {
            forecast,
            actual,
            status: classify_interval(forecast, actual),
            absolute_error: (actual - forecast.point).abs(),
            relative_error: relative_error(forecast.point, actual),
        }
    }
}

/// Distribution forecast-vs-actual comparison.
#[derive(Debug, Clone, Serialize)]
pub struct DistributionCalibration {
    pub forecast: QuantileSummary,
    pub actual: QuantileSummary,
    pub status: CalibrationMetricStatus,
    pub absolute_error: f64,
    pub relative_error: Option<f64>,
}

impl DistributionCalibration {
    fn new(forecast: QuantileSummary, actual: QuantileSummary) -> Self {
        let interval = IntervalEstimate {
            point: forecast.median,
            lower: forecast.p10,
            upper: forecast.p90,
        };
        Self {
            forecast,
            actual,
            status: classify_interval(interval, actual.median),
            absolute_error: (actual.median - forecast.median).abs(),
            relative_error: relative_error(forecast.median, actual.median),
        }
    }
}

#[derive(Debug, Clone)]
struct LoadedForecast {
    report: ForecastReport,
    artifact: ArtifactCompatibility,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct LoadedResults {
    results: SweepResults,
    artifact: ArtifactCompatibility,
    warnings: Vec<String>,
}

/// Compute a calibration report from an existing forecast JSON artifact and a
/// completed sweep `results.json` artifact.
pub fn compute(args: &CalibrationArgs) -> Result<CalibrationReport, Error> {
    let forecast = load_forecast(&args.forecast_path)?;
    let actual = load_results(&args.results_path, "sweep results")?;
    if actual.results.sweep_status != swebench::SWEEP_STATUS_COMPLETED {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "calibration requires completed sweep results, got sweep_status={}",
            actual.results.sweep_status
        ))));
    }

    let mut warnings = Vec::new();
    warnings.extend(forecast.warnings.clone());
    warnings.extend(actual.warnings.clone());

    let calibration = load_forecast_calibration_results(&args.forecast_path, &forecast.report)?;
    let calibration_manifest = if let Some(loaded) = calibration {
        warnings.extend(loaded.warnings);
        loaded.results.manifest
    } else {
        warnings.push(format!(
            "forecast calibration results unavailable at {}; dataset/model comparability not fully checked",
            calibration_results_path(&args.forecast_path, &forecast.report).display()
        ));
        None
    };

    let mismatches = build_mismatches(
        &forecast.report,
        calibration_manifest.as_ref(),
        actual.results.manifest.as_ref(),
        &actual.results,
    );
    let metrics = build_metrics(&forecast.report, &actual.results);
    let per_instance = build_per_instance(&forecast.report, &actual.results);
    let comparability = if mismatches.is_empty() {
        CalibrationComparability::Matched
    } else {
        CalibrationComparability::Mismatched
    };
    let verdict = calibration_verdict(comparability, &metrics, &per_instance);

    Ok(CalibrationReport {
        forecast_path: args.forecast_path.display().to_string(),
        results_path: args.results_path.display().to_string(),
        forecast_artifact: forecast.artifact.identity_label(),
        results_artifact: actual.artifact.identity_label(),
        verdict,
        comparability,
        mismatches,
        warnings,
        forecast_target_n: forecast.report.forecast.target_n,
        actual_instance_count: actual.results.instances.len(),
        metrics,
        per_instance,
    })
}

/// Serialize a calibration report as a versioned JSON artifact.
pub fn to_json(report: &CalibrationReport) -> Result<String, Error> {
    crate::artifact::to_string_pretty(ArtifactKind::CalibrationReport, report).map_err(Error::from)
}

/// Render a compact operator-facing calibration summary.
#[must_use]
pub fn render_text(report: &CalibrationReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\n=== bench calibrate ===");
    let _ = writeln!(out, "Forecast:           {}", report.forecast_path);
    let _ = writeln!(out, "Results:            {}", report.results_path);
    let _ = writeln!(out, "Verdict:            {}", report.verdict.label());
    let _ = writeln!(
        out,
        "Comparability:      {}",
        match report.comparability {
            CalibrationComparability::Matched => "matched",
            CalibrationComparability::Mismatched => "mismatched",
        }
    );
    let _ = writeln!(
        out,
        "Instances:          forecast target {} / actual {}",
        report.forecast_target_n, report.actual_instance_count
    );
    write_scalar_line(&mut out, "Total USD", "$", &report.metrics.total_usd);
    write_scalar_line(
        &mut out,
        "Input tokens",
        "",
        &report.metrics.total_input_tokens,
    );
    write_scalar_line(
        &mut out,
        "Output tokens",
        "",
        &report.metrics.total_output_tokens,
    );
    write_scalar_line(
        &mut out,
        "Wall-clock sec",
        "",
        &report.metrics.wall_clock_seconds,
    );
    write_scalar_line(
        &mut out,
        "Resolution rate",
        "",
        &report.metrics.resolution_rate,
    );
    if !report.mismatches.is_empty() {
        out.push_str("Mismatches:\n");
        for mismatch in &report.mismatches {
            let _ = writeln!(
                out,
                "  {}: forecast={} actual={}",
                mismatch.field, mismatch.forecast, mismatch.actual
            );
        }
    }
    for warning in &report.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    out
}

fn load_forecast(path: &Path) -> Result<LoadedForecast, Error> {
    let value = read_json_artifact(path, "forecast")?;
    let artifact = classify_json_value(
        &value,
        ArtifactKind::ForecastReport,
        path.display().to_string(),
    )
    .map_err(|err| Error::Trajectory(err.to_string()))?;
    let warnings = artifact.warnings.clone();
    let report: ForecastReport = serde_json::from_value(value)?;
    Ok(LoadedForecast {
        report,
        artifact,
        warnings,
    })
}

fn load_results(path: &Path, label: &str) -> Result<LoadedResults, Error> {
    let value = read_json_artifact(path, label)?;
    let artifact = classify_json_value(
        &value,
        ArtifactKind::SweepResults,
        path.display().to_string(),
    )
    .map_err(|err| Error::Trajectory(err.to_string()))?;
    let warnings = artifact.warnings.clone();
    let results: SweepResults = serde_json::from_value(value)?;
    Ok(LoadedResults {
        results,
        artifact,
        warnings,
    })
}

fn read_json_artifact(path: &Path, label: &str) -> Result<serde_json::Value, Error> {
    if !path.is_file() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "{label} artifact not found: {}",
            path.display()
        ))));
    }
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn load_forecast_calibration_results(
    forecast_path: &Path,
    report: &ForecastReport,
) -> Result<Option<LoadedResults>, Error> {
    let path = calibration_results_path(forecast_path, report);
    if !path.is_file() {
        return Ok(None);
    }
    load_results(&path, "forecast calibration").map(Some)
}

fn calibration_results_path(forecast_path: &Path, report: &ForecastReport) -> PathBuf {
    let output_dir = PathBuf::from(&report.calibration.output_dir);
    let cwd_relative = output_dir.join("results.json");
    if output_dir.is_absolute() || cwd_relative.is_file() {
        return cwd_relative;
    }
    forecast_path.parent().map_or(cwd_relative, |parent| {
        parent.join(output_dir).join("results.json")
    })
}

fn build_metrics(forecast: &ForecastReport, actual: &SweepResults) -> CalibrationMetrics {
    CalibrationMetrics {
        total_usd: ScalarCalibration::new(
            forecast.forecast.total_cost_usd,
            actual
                .actual_cost_total_usd()
                .unwrap_or(actual.estimated_cost_usd),
        ),
        total_input_tokens: ScalarCalibration::new(
            forecast.forecast.total_input_tokens,
            as_f64_u64(actual.total_prompt_tokens),
        ),
        total_output_tokens: ScalarCalibration::new(
            forecast.forecast.total_output_tokens,
            as_f64_u64(actual.total_completion_tokens),
        ),
        wall_clock_seconds: ScalarCalibration::new(
            forecast.forecast.wall_clock_seconds,
            actual_wall_clock_seconds(actual, forecast.forecast.parallel),
        ),
        resolution_rate: ScalarCalibration::new(
            resolution_interval(forecast),
            actual_resolution_rate(actual),
        ),
    }
}

fn build_per_instance(forecast: &ForecastReport, actual: &SweepResults) -> PerInstanceCalibration {
    let model_name = actual.manifest.as_ref().map(|m| m.model.name.as_str());
    PerInstanceCalibration {
        input_tokens: DistributionCalibration::new(
            forecast.per_instance.input_tokens,
            quantiles(
                &actual
                    .instances
                    .iter()
                    .map(|row| row.prompt_tokens.map_or(0.0, as_f64_u64))
                    .collect::<Vec<_>>(),
            ),
        ),
        output_tokens: DistributionCalibration::new(
            forecast.per_instance.output_tokens,
            quantiles(
                &actual
                    .instances
                    .iter()
                    .map(|row| row.completion_tokens.map_or(0.0, as_f64_u64))
                    .collect::<Vec<_>>(),
            ),
        ),
        usd_cost: DistributionCalibration::new(
            forecast.per_instance.usd_cost,
            quantiles(
                &actual
                    .instances
                    .iter()
                    .map(|row| row.effective_cost_usd(model_name).unwrap_or_default())
                    .collect::<Vec<_>>(),
            ),
        ),
        step_count: DistributionCalibration::new(
            forecast.per_instance.step_count,
            quantiles(
                &actual
                    .instances
                    .iter()
                    .map(|row| row.steps.map_or(0.0, f64::from))
                    .collect::<Vec<_>>(),
            ),
        ),
        wall_clock_seconds: DistributionCalibration::new(
            forecast.per_instance.wall_clock_seconds,
            quantiles(
                &actual
                    .instances
                    .iter()
                    .map(|row| row.duration_secs.unwrap_or_default())
                    .collect::<Vec<_>>(),
            ),
        ),
    }
}

fn build_mismatches(
    forecast: &ForecastReport,
    calibration_manifest: Option<&ProvenanceManifest>,
    actual_manifest: Option<&ProvenanceManifest>,
    actual: &SweepResults,
) -> Vec<CalibrationMismatch> {
    let mut mismatches = Vec::new();
    if let (Some(forecast_manifest), Some(actual_manifest)) =
        (calibration_manifest, actual_manifest)
    {
        compare_field(
            &mut mismatches,
            "dataset.sha256",
            &forecast_manifest.dataset.sha256,
            &actual_manifest.dataset.sha256,
        );
        compare_optional_field(
            &mut mismatches,
            "dataset.alias",
            forecast_manifest.dataset.alias.as_deref(),
            actual_manifest.dataset.alias.as_deref(),
        );
        compare_optional_field(
            &mut mismatches,
            "dataset.split",
            forecast_manifest.dataset.split.as_deref(),
            actual_manifest.dataset.split.as_deref(),
        );
        compare_field(
            &mut mismatches,
            "model.name",
            &forecast_manifest.model.name,
            &actual_manifest.model.name,
        );
    }

    if let Some(actual_manifest) = actual_manifest {
        let forecast_parallel = forecast.forecast.parallel.to_string();
        let actual_parallel = parallel_from_manifest(actual_manifest).to_string();
        compare_field(
            &mut mismatches,
            "parallel",
            &forecast_parallel,
            &actual_parallel,
        );
    }

    let forecast_count = forecast.forecast.target_n.to_string();
    let actual_count = actual.instances.len().to_string();
    compare_field(
        &mut mismatches,
        "instance_count",
        &forecast_count,
        &actual_count,
    );

    let actual_ids: BTreeSet<_> = actual
        .instances
        .iter()
        .map(|row| row.instance_id.as_str())
        .collect();
    if forecast.forecast.target_instance_ids.is_empty() {
        let missing_calibration_ids: Vec<_> = forecast
            .calibration
            .instance_ids
            .iter()
            .filter(|id| !actual_ids.contains(id.as_str()))
            .cloned()
            .collect();
        if !missing_calibration_ids.is_empty() {
            mismatches.push(CalibrationMismatch {
                field: "instance_set".into(),
                forecast: format!(
                    "calibration subset included {}",
                    forecast.calibration.instance_ids.join(",")
                ),
                actual: format!("missing {}", missing_calibration_ids.join(",")),
            });
        }
    } else {
        let forecast_ids: BTreeSet<_> = forecast
            .forecast
            .target_instance_ids
            .iter()
            .map(String::as_str)
            .collect();
        if forecast_ids != actual_ids {
            mismatches.push(CalibrationMismatch {
                field: "instance_set".into(),
                forecast: sorted_join(forecast_ids),
                actual: sorted_join(actual_ids),
            });
        }
    }

    mismatches
}

fn sorted_join(values: BTreeSet<&str>) -> String {
    values.iter().enumerate().fold(String::with_capacity(values.iter().map(|s| s.len() + 1).sum()), |mut acc, (i, s)| { if i > 0 { acc.push(','); } acc.push_str(s); acc })
}

fn compare_field(
    mismatches: &mut Vec<CalibrationMismatch>,
    field: &str,
    forecast: &str,
    actual: &str,
) {
    if forecast != actual {
        mismatches.push(CalibrationMismatch {
            field: field.into(),
            forecast: forecast.into(),
            actual: actual.into(),
        });
    }
}

fn compare_optional_field(
    mismatches: &mut Vec<CalibrationMismatch>,
    field: &str,
    forecast: Option<&str>,
    actual: Option<&str>,
) {
    if forecast != actual {
        mismatches.push(CalibrationMismatch {
            field: field.into(),
            forecast: forecast.unwrap_or("<none>").into(),
            actual: actual.unwrap_or("<none>").into(),
        });
    }
}

fn parallel_from_manifest(manifest: &ProvenanceManifest) -> usize {
    let argv = &manifest.cli.argv;
    for (idx, arg) in argv.iter().enumerate() {
        if let Some(value) = arg.strip_prefix("--parallel=") {
            if let Ok(parallel) = value.parse() {
                return parallel;
            }
        }
        if (arg == "--parallel" || arg == "-p") && idx + 1 < argv.len() {
            if let Ok(parallel) = argv[idx + 1].parse() {
                return parallel;
            }
        }
    }
    swebench::DEFAULT_PARALLEL
}

fn calibration_verdict(
    comparability: CalibrationComparability,
    metrics: &CalibrationMetrics,
    per_instance: &PerInstanceCalibration,
) -> CalibrationVerdict {
    if comparability == CalibrationComparability::Mismatched {
        return CalibrationVerdict::NotComparable;
    }
    let statuses: Vec<_> = metrics.statuses().chain(per_instance.statuses()).collect();
    if statuses.contains(&CalibrationMetricStatus::OverUpper) {
        CalibrationVerdict::Optimistic
    } else if statuses.contains(&CalibrationMetricStatus::UnderLower) {
        CalibrationVerdict::Pessimistic
    } else {
        CalibrationVerdict::WellCalibrated
    }
}

fn classify_interval(interval: IntervalEstimate, actual: f64) -> CalibrationMetricStatus {
    const EPSILON: f64 = 1e-9;
    if actual > interval.upper + EPSILON {
        CalibrationMetricStatus::OverUpper
    } else if actual < interval.lower - EPSILON {
        CalibrationMetricStatus::UnderLower
    } else {
        CalibrationMetricStatus::WithinInterval
    }
}

fn relative_error(point: f64, actual: f64) -> Option<f64> {
    if point.abs() <= f64::EPSILON {
        if actual.abs() <= f64::EPSILON {
            Some(0.0)
        } else {
            None
        }
    } else {
        Some((actual - point) / point)
    }
}

fn resolution_interval(forecast: &ForecastReport) -> IntervalEstimate {
    let point = forecast.resolution_rate.point.clamp(0.0, 1.0);
    let total = forecast.resolution_rate.total;
    if total == 0 {
        return IntervalEstimate {
            point,
            lower: point,
            upper: point,
        };
    }
    let successes = forecast.resolution_rate.resolved.min(total);
    let n = as_f64_usize(total);
    let p_hat = as_f64_usize(successes) / n;
    let z = z_for_confidence(forecast.forecast.confidence_pct);
    let z2 = z * z;
    let denominator = 1.0 + z2 / n;
    let center = (p_hat + z2 / (2.0 * n)) / denominator;
    let margin = z * (p_hat.mul_add(1.0 - p_hat, z2 / (4.0 * n)) / n).sqrt() / denominator;
    IntervalEstimate {
        point,
        lower: (center - margin).max(0.0),
        upper: (center + margin).min(1.0),
    }
}

fn z_for_confidence(confidence_pct: f64) -> f64 {
    if confidence_pct <= 80.0 {
        1.281_551_565_544_600_4
    } else if confidence_pct <= 90.0 {
        1.644_853_626_951_472_2
    } else if confidence_pct <= 95.0 {
        1.959_963_984_540_054
    } else if confidence_pct <= 99.0 {
        2.575_829_303_548_900_4
    } else {
        3.0
    }
}

fn actual_resolution_rate(results: &SweepResults) -> f64 {
    if !results.instances.is_empty() {
        let resolved = results
            .instances
            .iter()
            .filter(|row| swebench::pass_at_1(row))
            .count();
        return as_f64_usize(resolved) / as_f64_usize(results.instances.len());
    }
    results.pass_at_k
}

fn actual_wall_clock_seconds(results: &SweepResults, forecast_parallel: usize) -> f64 {
    if let Some(seconds) = manifest_wall_clock_seconds(results.manifest.as_ref()) {
        return seconds;
    }
    let total_duration = results
        .instances
        .iter()
        .filter_map(|row| row.duration_secs)
        .sum::<f64>();
    total_duration / as_f64_usize(forecast_parallel.max(1))
}

fn manifest_wall_clock_seconds(manifest: Option<&ProvenanceManifest>) -> Option<f64> {
    let manifest = manifest?;
    let started = chrono::DateTime::parse_from_rfc3339(&manifest.runtime.started_at_utc).ok()?;
    let finished =
        chrono::DateTime::parse_from_rfc3339(manifest.runtime.finished_at_utc.as_ref()?).ok()?;
    let millis = finished.signed_duration_since(started).num_milliseconds();
    #[allow(clippy::cast_precision_loss)]
    {
        Some((millis.max(0) as f64) / 1000.0)
    }
}

fn quantiles(values: &[f64]) -> QuantileSummary {
    if values.is_empty() {
        return QuantileSummary {
            p10: 0.0,
            median: 0.0,
            p90: 0.0,
        };
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    QuantileSummary {
        p10: quantile_sorted(&sorted, 0.10),
        median: quantile_sorted(&sorted, 0.50),
        p90: quantile_sorted(&sorted, 0.90),
    }
}

fn quantile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let span = as_f64_usize(sorted.len() - 1);
    let pos = q.clamp(0.0, 1.0) * span;
    let lo = floor_to_usize(pos);
    let hi = ceil_to_usize(pos);
    if lo == hi {
        sorted[lo]
    } else {
        let weight = pos - as_f64_usize(lo);
        sorted[lo].mul_add(1.0 - weight, sorted[hi] * weight)
    }
}

fn write_scalar_line(out: &mut String, label: &str, prefix: &str, metric: &ScalarCalibration) {
    let rel = metric.relative_error.map_or_else(
        || "n/a".to_owned(),
        |value| format!("{:+.2}%", value * 100.0),
    );
    let _ = writeln!(
        out,
        "  {label:<16} actual {prefix}{:.4} vs forecast {prefix}{:.4} [{prefix}{:.4}, {prefix}{:.4}] => {} (abs {:.4}, rel {rel})",
        metric.actual,
        metric.forecast.point,
        metric.forecast.lower,
        metric.forecast.upper,
        metric.status.label(),
        metric.absolute_error
    );
}

fn as_f64_u64(value: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        value as f64
    }
}

fn as_f64_usize(value: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        value as f64
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn floor_to_usize(value: f64) -> usize {
    value.floor() as usize
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ceil_to_usize(value: f64) -> usize {
    value.ceil() as usize
}
