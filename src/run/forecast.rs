//! `bench forecast`: run a small calibration sweep and extrapolate cost.

use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error};
use crate::run::swebench::{self, InstanceResult, SweepResults};
use crate::trajectory::{Trajectory, outcome};

/// Inputs for a forecast run.
pub struct ForecastArgs {
    /// Base sweep arguments. Forecast overrides output, subset selection, and
    /// calibration cost-limit handling before delegating to the sweep runner.
    pub sweep: swebench::SwebenchArgs,
    /// Number of instances to run in the calibration slice.
    pub calibration_n: usize,
    /// Deterministic sampler seed for calibration selection.
    pub seed: u64,
    /// Optional extrapolation target. Defaults to full post-filter dataset.
    pub target_n: Option<usize>,
    /// Confidence level percentage for interval estimates.
    pub confidence_pct: f64,
}

/// Result of running forecast setup.
#[derive(Debug, Clone)]
pub enum ForecastOutcome {
    /// A measured calibration sweep produced a forecast report.
    Report(Box<ForecastReport>),
    /// Dry-run stopped after preflight without writing forecast artifacts.
    DryRun(Box<SweepResults>),
}

/// Stable, serializable forecast report emitted by `bench forecast`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForecastReport {
    /// What calibration slice was measured.
    pub calibration: CalibrationSummary,
    /// Per-instance p10/median/p90 summaries for observed calibration data.
    pub per_instance: PerInstanceSummary,
    /// Extrapolated totals for the requested target size.
    pub forecast: ForecastTotals,
    /// Directional submit/resolution-rate signal from the calibration slice.
    pub resolution_rate: ResolutionRateSignal,
    /// Optional sweep-cost cap interpretation.
    pub threshold: ThresholdCheck,
}

/// Metadata about the calibration slice used for the forecast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationSummary {
    /// Number of calibration instances actually measured.
    pub n: usize,
    /// Seed used to select the calibration slice.
    pub seed: u64,
    /// Directory where calibration sweep artifacts were written.
    pub output_dir: String,
    /// Calibration instance ids in result order.
    pub instance_ids: Vec<String>,
}

/// Per-instance observed distributions from the calibration slice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerInstanceSummary {
    /// Input/prompt token distribution.
    pub input_tokens: QuantileSummary,
    /// Output/completion token distribution.
    pub output_tokens: QuantileSummary,
    /// Recorded or estimated USD cost distribution.
    pub usd_cost: QuantileSummary,
    /// Agent step count distribution.
    pub step_count: QuantileSummary,
    /// Per-instance wall-clock runtime distribution in seconds.
    pub wall_clock_seconds: QuantileSummary,
}

/// Three quantiles used for compact terminal and JSON summaries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QuantileSummary {
    /// 10th percentile.
    pub p10: f64,
    /// 50th percentile.
    pub median: f64,
    /// 90th percentile.
    pub p90: f64,
}

/// Extrapolated aggregate estimates for a target sweep size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForecastTotals {
    /// Number of instances being forecast.
    pub target_n: usize,
    /// Parallel worker count used for wall-clock extrapolation.
    pub parallel: usize,
    /// Confidence level percentage used for intervals.
    pub confidence_pct: f64,
    /// Total USD cost estimate.
    pub total_cost_usd: IntervalEstimate,
    /// Total input token estimate.
    pub total_input_tokens: IntervalEstimate,
    /// Total output token estimate.
    pub total_output_tokens: IntervalEstimate,
    /// Total elapsed wall-clock estimate at the requested parallelism.
    pub wall_clock_seconds: IntervalEstimate,
}

/// Point estimate with a lower/upper confidence interval.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IntervalEstimate {
    /// Mean-based point estimate.
    pub point: f64,
    /// Lower interval bound.
    pub lower: f64,
    /// Upper interval bound.
    pub upper: f64,
}

impl IntervalEstimate {
    /// Interval width (`upper - lower`).
    pub fn width(self) -> f64 {
        self.upper - self.lower
    }
}

/// Directional submit/resolution-rate estimate from calibration outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionRateSignal {
    /// Number of calibration instances that submitted.
    pub resolved: usize,
    /// Number of calibration instances measured.
    pub total: usize,
    /// `resolved / total` as a fraction.
    pub point: f64,
    /// Human-facing small-n caveat.
    pub disclaimer: String,
}

/// Interpretation of a forecast against an optional sweep cost cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdCheck {
    /// Configured sweep cap, when present.
    pub limit_usd: Option<f64>,
    /// Machine-readable threshold result.
    pub status: ThresholdStatus,
    /// Human-readable threshold explanation.
    pub message: String,
}

/// Cost-cap status for the forecast interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdStatus {
    /// No cap was configured.
    NotConfigured,
    /// The interval upper bound is below or equal to the cap.
    Under,
    /// The interval lower bound is above the cap.
    Exceeds,
    /// The interval crosses the cap.
    TooTight,
}

/// Operator override controls for `bench swebench --forecast-first`.
#[derive(Debug, Clone, Copy)]
pub struct ForecastGate {
    /// Allow the real sweep even if the forecast does not clearly pass.
    pub yes: bool,
}

/// Run a forecast calibration through the normal sweep runner, isolated under
/// `<output>/forecast`, then extrapolate from the recorded calibration usage.
pub async fn run(args: ForecastArgs) -> Result<ForecastOutcome, Error> {
    validate_args(args.calibration_n, args.confidence_pct)?;
    let dry_run = args.sweep.dry_run;
    let original_output = args.sweep.output_dir.clone();
    let calibration_dir = original_output.join("forecast");
    let target_n = match args.target_n {
        Some(n) => {
            validate_positive("target-n", n)?;
            n
        }
        None => target_count(&args.sweep)?,
    };
    let limit = args.sweep.cost_limit_usd;
    let parallel = args.sweep.parallel.max(1);
    let calibration_instance_ids =
        calibration_instance_ids(&args.sweep, args.calibration_n, args.seed)?;

    let mut sweep = args.sweep;
    sweep.output_dir.clone_from(&calibration_dir);
    sweep.resume = false;
    sweep.instance_ids = Some(calibration_instance_ids.join(","));
    sweep.limit = None;
    sweep.sample = None;
    sweep.seed = None;
    sweep.stratify_by = None;
    sweep.stratify_mode = swebench::StratifyMode::Proportional;
    sweep.cost_limit_usd = None;
    if !dry_run {
        sweep.preflight_format = "silent".into();
    }
    sweep.preflight_mode = "forecast".into();

    let mut results = swebench::run(sweep).await?;
    if dry_run {
        return Ok(ForecastOutcome::DryRun(Box::new(results)));
    }
    results
        .instances
        .sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    mark_forecast_manifest(&mut results, &calibration_dir)?;
    let mut report = forecast_from_results(
        &results,
        args.seed,
        target_n,
        parallel,
        args.confidence_pct,
        limit,
    )?;
    report.calibration.output_dir = calibration_dir.display().to_string();
    Ok(ForecastOutcome::Report(Box::new(report)))
}

/// Build a forecast report from an already-completed calibration sweep.
pub fn forecast_from_results(
    results: &SweepResults,
    seed: u64,
    target_n: usize,
    parallel: usize,
    confidence_pct: f64,
    cost_limit_usd: Option<f64>,
) -> Result<ForecastReport, Error> {
    validate_args(results.instances.len().max(1), confidence_pct)?;
    validate_positive("target-n", target_n)?;
    validate_positive("parallel", parallel)?;
    let mut instances = results.instances.clone();
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    if instances.is_empty() {
        return Err(Error::Config(ConfigError::Invalid(
            "forecast calibration produced zero instances".into(),
        )));
    }

    let input_tokens = values(&instances, |r| r.prompt_tokens.map_or(0.0, as_f64_u64));
    let output_tokens = values(&instances, |r| r.completion_tokens.map_or(0.0, as_f64_u64));
    let usd_cost = values(&instances, instance_cost_usd);
    let step_count = values(&instances, |r| r.steps.map_or(0.0, f64::from));
    let wall_clock_seconds = values(&instances, |r| r.duration_secs.unwrap_or(0.0));
    let resolved = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
        .count();

    Ok(ForecastReport {
        calibration: CalibrationSummary {
            n: instances.len(),
            seed,
            output_dir: String::new(),
            instance_ids: instances.iter().map(|r| r.instance_id.clone()).collect(),
        },
        per_instance: PerInstanceSummary {
            input_tokens: quantiles(&input_tokens),
            output_tokens: quantiles(&output_tokens),
            usd_cost: quantiles(&usd_cost),
            step_count: quantiles(&step_count),
            wall_clock_seconds: quantiles(&wall_clock_seconds),
        },
        forecast: ForecastTotals {
            target_n,
            parallel,
            confidence_pct,
            total_cost_usd: total_interval(&usd_cost, target_n, 1.0, confidence_pct),
            total_input_tokens: total_interval(&input_tokens, target_n, 1.0, confidence_pct),
            total_output_tokens: total_interval(&output_tokens, target_n, 1.0, confidence_pct),
            wall_clock_seconds: total_interval(
                &wall_clock_seconds,
                target_n,
                1.0 / as_f64_usize(parallel),
                confidence_pct,
            ),
        },
        resolution_rate: ResolutionRateSignal {
            resolved,
            total: instances.len(),
            point: as_f64_usize(resolved) / as_f64_usize(instances.len()),
            disclaimer: format!("n={}, treat as directional only", instances.len()),
        },
        threshold: threshold_check(
            cost_limit_usd,
            total_interval(&usd_cost, target_n, 1.0, confidence_pct),
        ),
    })
}

/// Enforce `--fail-over-cap` for a completed forecast report.
pub fn validate_fail_over_cap(report: &ForecastReport, fail_over_cap: bool) -> Result<(), Error> {
    if fail_over_cap && report.threshold.status == ThresholdStatus::Exceeds {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "forecast projects sweep cost will exceed cap: {}",
            report.threshold.message
        ))));
    }
    Ok(())
}

/// Decide whether `--forecast-first` should launch the real sweep.
pub fn forecast_gate_allows_sweep(
    report: &ForecastReport,
    gate: ForecastGate,
) -> Result<bool, Error> {
    if gate.yes {
        return Ok(true);
    }
    Ok(report.threshold.status == ThresholdStatus::Under)
}

/// Serialize a forecast report as stable pretty JSON.
pub fn to_json(report: &ForecastReport) -> Result<String, Error> {
    serde_json::to_string_pretty(report).map_err(Error::from)
}

/// Render a terminal-friendly forecast report.
pub fn render_text(report: &ForecastReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\n=== SWE-bench forecast ===");
    let _ = writeln!(
        out,
        "Calibration:       n={} seed={}",
        report.calibration.n, report.calibration.seed
    );
    let _ = writeln!(
        out,
        "Target:            {} instance(s) at parallel {}",
        report.forecast.target_n, report.forecast.parallel
    );
    let _ = writeln!(
        out,
        "Confidence:        {:.1}%",
        report.forecast.confidence_pct
    );
    out.push_str("Per-instance p10 / median / p90:\n");
    write_quantile_line(&mut out, "Input tokens", report.per_instance.input_tokens);
    write_quantile_line(&mut out, "Output tokens", report.per_instance.output_tokens);
    write_quantile_line(&mut out, "USD cost", report.per_instance.usd_cost);
    write_quantile_line(&mut out, "Steps", report.per_instance.step_count);
    write_quantile_line(
        &mut out,
        "Wall-clock sec",
        report.per_instance.wall_clock_seconds,
    );
    out.push_str("Forecast totals:\n");
    write_interval_line(
        &mut out,
        "Total USD",
        report.forecast.total_cost_usd,
        report.forecast.confidence_pct,
        "$",
    );
    write_interval_line(
        &mut out,
        "Input tokens",
        report.forecast.total_input_tokens,
        report.forecast.confidence_pct,
        "",
    );
    write_interval_line(
        &mut out,
        "Output tokens",
        report.forecast.total_output_tokens,
        report.forecast.confidence_pct,
        "",
    );
    write_interval_line(
        &mut out,
        "Wall-clock sec",
        report.forecast.wall_clock_seconds,
        report.forecast.confidence_pct,
        "",
    );
    let _ = writeln!(
        out,
        "Resolution signal: {:.2}% ({}/{}) - {}",
        report.resolution_rate.point * 100.0,
        report.resolution_rate.resolved,
        report.resolution_rate.total,
        report.resolution_rate.disclaimer
    );
    let _ = writeln!(out, "Threshold:         {}", report.threshold.message);
    out
}

fn target_count(args: &swebench::SwebenchArgs) -> Result<usize, Error> {
    Ok(planned_instances(args)?.len())
}

fn calibration_instance_ids(
    args: &swebench::SwebenchArgs,
    calibration_n: usize,
    seed: u64,
) -> Result<Vec<String>, Error> {
    let planned = planned_instances(args)?;
    let (calibration, _) = swebench::apply_subset(
        planned,
        &swebench::ApplySubsetParams {
            sample: Some(calibration_n),
            seed: Some(seed),
            ..Default::default()
        },
    )?;
    Ok(calibration
        .into_iter()
        .map(|inst| inst.instance_id)
        .collect())
}

fn planned_instances(
    args: &swebench::SwebenchArgs,
) -> Result<Vec<swebench::SweBenchInstance>, Error> {
    let instances = swebench::load_dataset(&args.dataset_path)?;
    let (filtered, _) = swebench::apply_subset(
        instances,
        &swebench::ApplySubsetParams {
            instance_ids_arg: args.instance_ids.as_deref(),
            limit: args.limit,
            sample: args.sample,
            seed: args.seed,
            stratify_by: args.stratify_by,
            stratify_mode: args.stratify_mode,
        },
    )?;
    Ok(filtered)
}

fn mark_forecast_manifest(results: &mut SweepResults, calibration_dir: &Path) -> Result<(), Error> {
    if let Some(manifest) = &mut results.manifest {
        manifest.purpose = Some("forecast".into());
    }
    mark_trajectory_purpose(results, calibration_dir)?;
    let path = calibration_dir.join("results.json");
    std::fs::write(path, serde_json::to_string_pretty(results)?)?;
    Ok(())
}

fn mark_trajectory_purpose(results: &SweepResults, calibration_dir: &Path) -> Result<(), Error> {
    for result in &results.instances {
        let path = swebench::trajectory_path_for(calibration_dir, &result.instance_id);
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        let mut trajectory: Trajectory = serde_json::from_str(&text)?;
        trajectory.info.other.insert(
            "purpose".into(),
            serde_json::Value::String("forecast".into()),
        );
        trajectory.save_pretty(&path)?;
    }
    Ok(())
}

fn validate_args(calibration_n: usize, confidence_pct: f64) -> Result<(), Error> {
    validate_positive("calibration-n", calibration_n)?;
    if !(0.0..100.0).contains(&confidence_pct) {
        return Err(Error::Config(ConfigError::Invalid(
            "--confidence must be greater than 0 and less than 100".into(),
        )));
    }
    Ok(())
}

fn validate_positive(name: &str, n: usize) -> Result<(), Error> {
    if n == 0 {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "--{name} must be greater than 0"
        ))));
    }
    Ok(())
}

fn values(instances: &[InstanceResult], f: impl Fn(&InstanceResult) -> f64) -> Vec<f64> {
    instances.iter().map(f).collect()
}

fn instance_cost_usd(r: &InstanceResult) -> f64 {
    r.cost_usd.unwrap_or_else(|| {
        swebench::estimate_cost_usd(
            r.prompt_tokens.unwrap_or(0),
            r.completion_tokens.unwrap_or(0),
        )
    })
}

fn quantiles(values: &[f64]) -> QuantileSummary {
    QuantileSummary {
        p10: quantile(values, 0.10),
        median: quantile(values, 0.50),
        p90: quantile(values, 0.90),
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn quantile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.len() == 1 {
        return sorted[0];
    }
    let span = as_f64_usize(sorted.len() - 1);
    let pos = q.clamp(0.0, 1.0) * span;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let weight = pos - as_f64_usize(lo);
        sorted[lo].mul_add(1.0 - weight, sorted[hi] * weight)
    }
}

fn total_interval(
    values: &[f64],
    target_n: usize,
    scale: f64,
    confidence_pct: f64,
) -> IntervalEstimate {
    let target = as_f64_usize(target_n);
    let mean = mean(values);
    let point = mean * target * scale;
    if values.len() < 2 {
        return IntervalEstimate {
            point,
            lower: point,
            upper: point,
        };
    }

    let mut bootstrap = bootstrap_totals(values, target, scale);
    bootstrap.sort_by(f64::total_cmp);
    let alpha = (1.0 - confidence_pct / 100.0) / 2.0;
    let lower = quantile_sorted(&bootstrap, alpha).max(0.0);
    let upper = quantile_sorted(&bootstrap, 1.0 - alpha).max(lower);
    IntervalEstimate {
        point,
        lower,
        upper,
    }
}

fn bootstrap_totals(values: &[f64], target: f64, scale: f64) -> Vec<f64> {
    let mut rng = ForecastRng::new(0x05EE_DF0E_CA57_u64);
    let mut estimates = Vec::with_capacity(2000);
    for _ in 0..2000 {
        let mut sum = 0.0;
        for _ in values {
            let idx = rng.next_usize() % values.len();
            sum += values[idx];
        }
        estimates.push((sum / as_f64_usize(values.len())) * target * scale);
    }
    estimates
}

fn quantile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
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

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / as_f64_usize(values.len())
    }
}

fn threshold_check(limit_usd: Option<f64>, cost: IntervalEstimate) -> ThresholdCheck {
    let Some(limit) = limit_usd else {
        return ThresholdCheck {
            limit_usd: None,
            status: ThresholdStatus::NotConfigured,
            message: "no sweep cost limit configured".into(),
        };
    };
    if cost.upper <= limit {
        ThresholdCheck {
            limit_usd: Some(limit),
            status: ThresholdStatus::Under,
            message: format!(
                "projects under cap: upper ${:.4} <= limit ${limit:.4}",
                cost.upper
            ),
        }
    } else if cost.lower > limit {
        ThresholdCheck {
            limit_usd: Some(limit),
            status: ThresholdStatus::Exceeds,
            message: format!(
                "projects over cap: lower ${:.4} > limit ${limit:.4}",
                cost.lower
            ),
        }
    } else {
        ThresholdCheck {
            limit_usd: Some(limit),
            status: ThresholdStatus::TooTight,
            message: format!(
                "too tight to call: interval ${:.4}-${:.4} crosses limit ${limit:.4}",
                cost.lower, cost.upper
            ),
        }
    }
}

fn write_quantile_line(out: &mut String, label: &str, q: QuantileSummary) {
    let _ = writeln!(
        out,
        "  {label:<15} {:>10.4} / {:>10.4} / {:>10.4}",
        q.p10, q.median, q.p90
    );
}

fn write_interval_line(
    out: &mut String,
    label: &str,
    interval: IntervalEstimate,
    confidence_pct: f64,
    prefix: &str,
) {
    let _ = writeln!(
        out,
        "  {label:<15} {prefix}{:.4} ({:.1}% CI {prefix}{:.4}-{prefix}{:.4})",
        interval.point, confidence_pct, interval.lower, interval.upper
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

#[derive(Clone, Copy)]
struct ForecastRng {
    state: u64,
}

impl ForecastRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_usize(&mut self) -> usize {
        #[cfg(target_pointer_width = "64")]
        {
            usize::from_le_bytes(self.next_u64().to_le_bytes())
        }
        #[cfg(target_pointer_width = "32")]
        {
            let bytes = self.next_u64().to_le_bytes();
            usize::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }
    }
}
