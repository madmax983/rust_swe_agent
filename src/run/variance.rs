//! `bench variance` — classify per-instance flakiness from rerun sweeps.
//!
//! Read-only: never re-runs instances, never calls a model, never modifies input sweeps.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::compare::load_sweep;

// ── public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StabilityClass {
    AlwaysResolved,
    AlwaysFailed,
    Flaky,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceVariance {
    pub instance_id: String,
    pub resolved_slots: u32,
    pub total_slots: u32,
    pub stability_class: StabilityClass,
}

/// Sweep-wide noise metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoiseSummary {
    /// Number of flaky instances (resolved some but not all slots).
    pub flaky_count: u32,
    /// Fraction of instances that are flaky.
    pub flaky_share: f64,
    /// Best-case pass rate: fraction of instances with at least one resolved slot.
    pub pass_at_k: f64,
    /// Worst-case pass rate: fraction of instances where all slots resolved.
    pub all_of_k: f64,
    /// Per-slot pass rate: total_resolved / total_slots.
    pub pass_at_1: f64,
    /// Wilson 95% CI lower bound for pass_at_1.
    pub ci_lower: f64,
    /// Wilson 95% CI upper bound for pass_at_1.
    pub ci_upper: f64,
    /// Recommended reruns per instance to achieve target CI half-width. None when not requested
    /// or when current reruns already suffice.
    pub recommended_reruns: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchVarianceReport {
    pub instances: Vec<InstanceVariance>,
    pub noise: NoiseSummary,
}

#[derive(Debug, Clone)]
pub struct BenchVarianceArgs {
    pub sweep_dir: PathBuf,
    /// Target CI half-width for recommended rerun count calculation.
    pub ci_width: Option<f64>,
    /// Instance-id substring filters (currently unused, reserved for future use).
    pub filter: Vec<String>,
    /// Stability class filter: "flaky", "always_resolved", or "always_failed".
    pub class: Option<String>,
}

// ── entry point ───────────────────────────────────────────────────────────────

pub fn compute_variance(args: &BenchVarianceArgs) -> Result<BenchVarianceReport, Error> {
    let class_filter: Option<StabilityClass> = match args.class.as_deref() {
        None => None,
        Some("flaky") => Some(StabilityClass::Flaky),
        Some("always_resolved") => Some(StabilityClass::AlwaysResolved),
        Some("always_failed") => Some(StabilityClass::AlwaysFailed),
        Some(other) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "bench variance: invalid --class '{other}'; \
                 valid values: always_resolved, always_failed, flaky"
            ))));
        }
    };

    let results_path = args.sweep_dir.join("results.json");
    if !results_path.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "bench variance: results.json not found in {}; \
             this command requires a completed sweep directory",
            args.sweep_dir.display()
        ))));
    }

    let loaded = load_sweep(&args.sweep_dir)?;

    let mut instances: Vec<_> = loaded.instances.values().collect();
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let max_runs = instances.iter().map(|i| i.runs).max().unwrap_or(0);
    if max_runs <= 1 {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "bench variance: sweep contains only single-slot runs; \
             this command requires a rerun sweep (--reruns N with N > 1) \
             so that each instance has multiple runs to compare"
                .into(),
        )));
    }

    let all_variance: Vec<InstanceVariance> = instances
        .iter()
        .map(|inst| InstanceVariance {
            instance_id: inst.instance_id.clone(),
            resolved_slots: inst.resolved_count,
            total_slots: inst.runs,
            stability_class: classify(inst.resolved_count, inst.runs),
        })
        .collect();

    let noise = compute_noise_summary(&all_variance, args.ci_width);

    let filtered_instances: Vec<InstanceVariance> = match &class_filter {
        None => all_variance,
        Some(cls) => all_variance
            .into_iter()
            .filter(|i| &i.stability_class == cls)
            .collect(),
    };

    Ok(BenchVarianceReport {
        instances: filtered_instances,
        noise,
    })
}

fn classify(resolved_slots: u32, total_slots: u32) -> StabilityClass {
    if resolved_slots == 0 {
        StabilityClass::AlwaysFailed
    } else if resolved_slots == total_slots {
        StabilityClass::AlwaysResolved
    } else {
        StabilityClass::Flaky
    }
}

fn compute_noise_summary(instances: &[InstanceVariance], ci_width: Option<f64>) -> NoiseSummary {
    let n = instances.len();
    if n == 0 {
        return NoiseSummary {
            flaky_count: 0,
            flaky_share: 0.0,
            pass_at_k: 0.0,
            all_of_k: 0.0,
            pass_at_1: 0.0,
            ci_lower: 0.0,
            ci_upper: 0.0,
            recommended_reruns: None,
        };
    }

    let n_f = n as f64;
    let flaky_count = instances
        .iter()
        .filter(|i| i.stability_class == StabilityClass::Flaky)
        .count() as u32;
    let flaky_share = f64::from(flaky_count) / n_f;

    let pass_at_k = instances.iter().filter(|i| i.resolved_slots > 0).count() as f64 / n_f;
    let all_of_k =
        instances.iter().filter(|i| i.resolved_slots == i.total_slots).count() as f64 / n_f;

    let total_resolved: u64 = instances.iter().map(|i| u64::from(i.resolved_slots)).sum();
    let total_slots: u64 = instances.iter().map(|i| u64::from(i.total_slots)).sum();

    let pass_at_1 = if total_slots > 0 {
        total_resolved as f64 / total_slots as f64
    } else {
        0.0
    };

    let (ci_lower, ci_upper) = wilson_ci(pass_at_1, total_slots);

    let recommended_reruns = ci_width.and_then(|w| {
        compute_recommended_reruns(pass_at_1, n, w, total_slots)
    });

    NoiseSummary {
        flaky_count,
        flaky_share,
        pass_at_k,
        all_of_k,
        pass_at_1,
        ci_lower,
        ci_upper,
        recommended_reruns,
    }
}

fn wilson_ci(p: f64, n: u64) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    const Z: f64 = 1.96;
    let n_f = n as f64;
    let z2 = Z * Z;
    let center = (p + z2 / (2.0 * n_f)) / (1.0 + z2 / n_f);
    let half = Z * (p * (1.0 - p) / n_f + z2 / (4.0 * n_f * n_f)).sqrt()
        / (1.0 + z2 / n_f);
    let lower = (center - half).max(0.0);
    let upper = (center + half).min(1.0);
    (lower, upper)
}

fn compute_recommended_reruns(
    pass_at_1: f64,
    n_instances: usize,
    ci_width: f64,
    current_total_slots: u64,
) -> Option<u32> {
    let variance = pass_at_1 * (1.0 - pass_at_1);
    if variance < 1e-12 {
        return None;
    }
    const Z: f64 = 1.96;
    let n_required = (Z / ci_width).powi(2) * variance;
    let avg_current = current_total_slots as f64 / n_instances as f64;
    let k_required = (n_required / n_instances as f64).ceil() as u32;
    if k_required as f64 <= avg_current {
        None
    } else {
        Some(k_required)
    }
}

// ── text rendering ────────────────────────────────────────────────────────────

pub fn render_text(report: &BenchVarianceReport) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();

    let _ = writeln!(s, "# Bench Variance Report");
    let _ = writeln!(s);

    let noise = &report.noise;
    let _ = writeln!(s, "## Noise Summary");
    let _ = writeln!(s, "  flaky_count:    {}", noise.flaky_count);
    let _ = writeln!(s, "  flaky_share:    {:.3}", noise.flaky_share);
    let _ = writeln!(s, "  pass_at_k:      {:.3}  (best-case: any slot resolved)", noise.pass_at_k);
    let _ = writeln!(s, "  all_of_k:       {:.3}  (worst-case: all slots resolved)", noise.all_of_k);
    let _ = writeln!(s, "  pass_at_1:      {:.3}  (per-slot pass rate)", noise.pass_at_1);
    let _ = writeln!(s, "  CI 95%:         [{:.3}, {:.3}]", noise.ci_lower, noise.ci_upper);
    if let Some(rec) = noise.recommended_reruns {
        let _ = writeln!(s, "  recommended_reruns: {rec}");
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "## Instances ({})", report.instances.len());
    let _ = writeln!(
        s,
        "  {:<40}  {:<8}  {:<5}  {}",
        "INSTANCE_ID", "RESOLVED", "TOTAL", "CLASS"
    );
    let _ = writeln!(s, "  {}", "-".repeat(78));
    for inst in &report.instances {
        let class_label = match inst.stability_class {
            StabilityClass::AlwaysResolved => "always_resolved",
            StabilityClass::AlwaysFailed => "always_failed",
            StabilityClass::Flaky => "flaky",
        };
        let _ = writeln!(
            s,
            "  {:<40}  {:<8}  {:<5}  {}",
            inst.instance_id, inst.resolved_slots, inst.total_slots, class_label
        );
    }
    s
}
