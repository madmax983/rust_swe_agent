#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::suboptimal_flops,
    clippy::manual_midpoint,
    clippy::unreadable_literal,
    clippy::excessive_precision,
    clippy::many_single_char_names
)]

//! `bench power` — statistical power, sample size, or MDE calculations.
//!
//! Run completely offline (zero network/model calls) in under 100ms.

use crate::cli::args::PowerCmd;
use crate::error::Error;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerReport {
    pub baseline_rate: f64,
    pub delta: Option<f64>,
    pub n: Option<usize>,
    pub alpha: f64,
    pub power: f64,
    pub one_sided: bool,
    pub arms: usize,
    pub bonferroni_note: Option<String>,
    pub solved_n: Option<usize>,
    pub solved_mde: Option<f64>,
    pub cost_per_instance: Option<f64>,
    pub total_cost: Option<f64>,
}

/// Standard normal cumulative distribution function (CDF) using a highly accurate rational approximation.
fn phi(x: f64) -> f64 {
    if x < 0.0 {
        return 1.0 - phi(-x);
    }
    let p = 0.2316419;
    let b1 = 0.319381530;
    let b2 = -0.356563782;
    let b3 = 1.781477937;
    let b4 = -1.821255978;
    let b5 = 1.330274429;

    let t = 1.0 / (1.0 + p * x);
    let poly = t * (b1 + t * (b2 + t * (b3 + t * (b4 + t * b5))));
    let exponent = -x * x / 2.0;
    let pdf = (1.0 / (2.0 * std::f64::consts::PI).sqrt()) * exponent.exp();
    1.0 - pdf * poly
}

/// Inverse standard normal CDF (quantile function) using Acklam's method.
fn inverse_phi(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }

    if (0.02425..=0.97575).contains(&p) {
        let q = p - 0.5;
        let r = q * q;

        let a = [
            -3.969683028665376e1,
            2.209460984245205e2,
            -2.759285104469687e2,
            1.383577518672690e2,
            -3.066479806614716e1,
            2.506628277459239e0,
        ];
        let b = [
            -5.447609879822406e1,
            1.615858368580409e2,
            -1.556989798598866e2,
            6.680131188771972e1,
            -1.328068155288572e1,
        ];

        let num = ((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5];
        let den = ((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1.0;
        return q * num / den;
    }

    let p_tail = if p < 0.02425 { p } else { 1.0 - p };
    let q = (-2.0 * p_tail.ln()).sqrt();

    let c = [
        -7.784894002430293e-3,
        -3.223964580411365e-1,
        -2.400758277161838e0,
        -2.549732539343734e0,
        4.374664141464968e0,
        2.938163982698783e0,
    ];
    let d = [
        7.784695709041462e-3,
        3.224671290700398e-1,
        2.445134137142996e0,
        3.754408661907416e0,
    ];

    let num = ((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5];
    let den = (((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0;

    let x = num / den;
    if p < 0.02425 { x } else { -x }
}

/// Calculate Cohen's h: h = 2 * |arcsin(sqrt(p1)) - arcsin(sqrt(p2))|
fn cohens_h(p1: f64, p2: f64) -> f64 {
    2.0 * (p1.sqrt().asin() - p2.sqrt().asin()).abs()
}

/// Numerical/Analytical search to find the minimum integer N per arm achieving at least the target power.
fn solve_sample_size(
    baseline_rate: f64,
    delta: f64,
    alpha: f64,
    power: f64,
    one_sided: bool,
) -> usize {
    const MAX_ITERATIONS: usize = 100_000;

    let p2 = if baseline_rate - delta >= 0.0 {
        baseline_rate - delta
    } else {
        baseline_rate + delta
    };

    let h = cohens_h(baseline_rate, p2);
    if h == 0.0 {
        return 0;
    }

    let z_crit = if one_sided {
        inverse_phi(1.0 - alpha)
    } else {
        inverse_phi(1.0 - alpha / 2.0)
    };

    let z_power = inverse_phi(power);

    // Initial analytical approximation (ceilinged)
    let n_approx = 2.0 * ((z_crit + z_power) / h).powi(2);
    let mut n = (n_approx.floor() as usize).max(1);

    // Precise search loop with a strict iteration limit to prevent infinite loops.
    let mut iterations = 0;
    loop {
        let n_eff = n as f64 / 2.0;
        let calculated_power = if one_sided {
            phi(h * n_eff.sqrt() - z_crit)
        } else {
            phi(h * n_eff.sqrt() - z_crit) + phi(-h * n_eff.sqrt() - z_crit)
        };

        if calculated_power >= power || iterations >= MAX_ITERATIONS {
            break;
        }
        n += 1;
        iterations += 1;
    }

    n
}

/// Bisection search to find Cohen's h satisfying target power.
fn solve_h_for_power(n: usize, alpha: f64, target_power: f64, one_sided: bool) -> f64 {
    let z_crit = if one_sided {
        inverse_phi(1.0 - alpha)
    } else {
        inverse_phi(1.0 - alpha / 2.0)
    };

    let n_eff = n as f64 / 2.0;

    if one_sided {
        let z_power = inverse_phi(target_power);
        return (z_crit + z_power) / n_eff.sqrt();
    }

    let mut low = 0.0;
    let mut high = 10.0;
    for _ in 0..100 {
        let mid = (low + high) / 2.0;
        let p = phi(mid * n_eff.sqrt() - z_crit) + phi(-mid * n_eff.sqrt() - z_crit);
        if p < target_power {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// Convert Cohen's h back to delta absolute percentage points.
fn h_to_delta(p1: f64, h: f64) -> f64 {
    let asin_p1 = p1.sqrt().asin();

    let term_pos = asin_p1 + h / 2.0;
    let p2_pos = if term_pos <= std::f64::consts::FRAC_PI_2 + 1e-9 {
        term_pos.sin().powi(2)
    } else {
        f64::NAN
    };

    let term_neg = asin_p1 - h / 2.0;
    let p2_neg = if term_neg >= -1e-9 {
        term_neg.sin().powi(2)
    } else {
        f64::NAN
    };

    let d_pos = if p2_pos.is_nan() {
        f64::NAN
    } else {
        (p2_pos - p1).abs()
    };
    let d_neg = if p2_neg.is_nan() {
        f64::NAN
    } else {
        (p1 - p2_neg).abs()
    };

    if !d_pos.is_nan() && !d_neg.is_nan() {
        d_pos.min(d_neg)
    } else if !d_pos.is_nan() {
        d_pos
    } else if !d_neg.is_nan() {
        d_neg
    } else {
        0.0
    }
}

pub fn run(cmd: &PowerCmd) -> Result<PowerReport, Error> {
    // 1. Validation Checks
    if cmd.alpha <= 0.0 || cmd.alpha >= 1.0 {
        return Err(Error::Config(crate::error::ConfigError::Usage(format!(
            "Significance level alpha must be between 0.0 and 1.0, got {}",
            cmd.alpha
        ))));
    }
    if cmd.power <= 0.0 || cmd.power >= 1.0 {
        return Err(Error::Config(crate::error::ConfigError::Usage(format!(
            "Statistical power must be between 0.0 and 1.0, got {}",
            cmd.power
        ))));
    }
    if cmd.arms < 2 {
        return Err(Error::Config(crate::error::ConfigError::Usage(format!(
            "Number of arms must be at least 2, got {}",
            cmd.arms
        ))));
    }

    // 2. Resolve Baseline Resolved Rate
    let baseline_rate = match (cmd.baseline_rate, &cmd.from_sweep) {
        (Some(b), None) => {
            if !(0.0..=1.0).contains(&b) {
                return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                    "Baseline rate must be in [0.0, 1.0], got {b}"
                ))));
            }
            b
        }
        (None, Some(path)) => {
            if !path.exists() {
                return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                    "Sweep directory `{}` does not exist",
                    path.display()
                ))));
            }
            // First try evaluation.json
            let evaluation = crate::run::compare::load_evaluation_results_checked(path)?;
            if let Some(ev) = evaluation {
                let total = ev.results.instances.len();
                if total == 0 {
                    return Err(Error::Config(crate::error::ConfigError::Usage(
                        "evaluation.json contains zero instances".to_string(),
                    )));
                }
                let resolved = ev.results.instances.iter().filter(|i| i.resolved).count();
                resolved as f64 / total as f64
            } else {
                // Try results.json
                let results_path = path.join("results.json");
                if !results_path.exists() {
                    return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                        "Neither evaluation.json nor results.json found in sweep directory `{}`",
                        path.display()
                    ))));
                }
                let sweep = crate::run::compare::load_sweep(path)?;
                let total = sweep.instances.len();
                if total == 0 {
                    return Err(Error::Config(crate::error::ConfigError::Usage(
                        "results.json contains zero instances".to_string(),
                    )));
                }
                let resolved = sweep
                    .instances
                    .values()
                    .filter(|inst| inst.resolved_count > 0)
                    .count();
                resolved as f64 / total as f64
            }
        }
        (None, None) => {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "Either --baseline-rate or --from-sweep must be provided".to_string(),
            )));
        }
        _ => unreachable!(),
    };

    if cmd.delta.is_none() && cmd.n.is_none() {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "Either --delta (Mode A) or --n (Mode B) must be specified".to_string(),
        )));
    }

    if let Some(d) = cmd.delta {
        if !(0.0..=1.0).contains(&d) {
            return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                "Delta must be in [0.0, 1.0], got {d}"
            ))));
        }
    }

    if let Some(n) = cmd.n {
        if n == 0 {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "Sample size --n must be at least 1".to_string(),
            )));
        }
    }

    // 3. Bonferroni correction for arms > 2
    let adjusted_alpha = if cmd.arms > 2 {
        cmd.alpha / (cmd.arms - 1) as f64
    } else {
        cmd.alpha
    };

    let bonferroni_note = if cmd.arms > 2 {
        Some(format!(
            "Bonferroni correction applied for arms={}, significance level adjusted from {:.4} to {:.5}",
            cmd.arms, cmd.alpha, adjusted_alpha
        ))
    } else {
        None
    };

    // 4. Mode Solver
    let mut solved_n = None;
    let mut solved_mde = None;

    if let Some(delta) = cmd.delta {
        // Mode A: Solve for Sample Size
        let n = if let Some(overridden) = cmd.override_solved_n {
            overridden
        } else {
            solve_sample_size(
                baseline_rate,
                delta,
                adjusted_alpha,
                cmd.power,
                cmd.one_sided,
            )
        };
        solved_n = Some(n);
    } else if let Some(n) = cmd.n {
        // Mode B: Solve for MDE
        let h = solve_h_for_power(n, adjusted_alpha, cmd.power, cmd.one_sided);
        let mde = h_to_delta(baseline_rate, h);
        solved_mde = Some(mde);
    }

    // 5. Cost calculation
    let cost_per_instance = match (cmd.cost_per_instance, &cmd.from_forecast) {
        (Some(c), None) => {
            if c < 0.0 {
                return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                    "Cost per instance cannot be negative, got {c}"
                ))));
            }
            Some(c)
        }
        (None, Some(path)) => {
            if !path.exists() {
                return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                    "Forecast file `{}` does not exist",
                    path.display()
                ))));
            }
            let text = std::fs::read_to_string(path)?;
            let val: serde_json::Value = serde_json::from_str(&text)?;

            let target_n = val
                .pointer("/forecast/target_n")
                .and_then(|v| v.as_f64().or_else(|| v.as_u64().map(|u| u as f64)))
                .ok_or_else(|| {
                    Error::Config(crate::error::ConfigError::Usage(
                        "Forecast file missing `/forecast/target_n` field".to_string(),
                    ))
                })?;

            if target_n == 0.0 {
                return Err(Error::Config(crate::error::ConfigError::Usage(
                    "Forecast file `/forecast/target_n` cannot be zero".to_string(),
                )));
            }

            let point_cost = val
                .pointer("/forecast/total_cost_usd/point")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| {
                    Error::Config(crate::error::ConfigError::Usage(
                        "Forecast file missing `/forecast/total_cost_usd/point` field".to_string(),
                    ))
                })?;

            Some(point_cost / target_n)
        }
        (None, None) => None,
        _ => unreachable!(),
    };

    let total_cost = cost_per_instance.map(|c| {
        let size = solved_n.or(cmd.n).unwrap_or(0);
        size as f64 * cmd.arms as f64 * c
    });

    Ok(PowerReport {
        baseline_rate,
        delta: cmd.delta,
        n: cmd.n,
        alpha: cmd.alpha,
        power: cmd.power,
        one_sided: cmd.one_sided,
        arms: cmd.arms,
        bonferroni_note,
        solved_n,
        solved_mde,
        cost_per_instance,
        total_cost,
    })
}

pub fn render_text(report: &PowerReport) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let mut out = String::new();
    out.push_str("\n📊 Statistical Power Analysis Report\n");

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["Parameter", "Value"]);

    table.add_row(vec![
        "Baseline Rate",
        &format!("{:.4}", report.baseline_rate),
    ]);
    table.add_row(vec![
        "Significance Level (Alpha)",
        &format!("{:.4}", report.alpha),
    ]);
    table.add_row(vec![
        "Target Statistical Power",
        &format!("{:.4}", report.power),
    ]);
    table.add_row(vec![
        "Hypothesis Type",
        if report.one_sided {
            "One-Sided"
        } else {
            "Two-Sided"
        },
    ]);
    table.add_row(vec!["Study Arms", &format!("{}", report.arms)]);

    if let Some(delta) = report.delta {
        table.add_row(vec!["Target Delta (pp)", &format!("{delta:.4}")]);
    }

    if let Some(n) = report.n {
        table.add_row(vec!["Sample Size per Arm (N)", &format!("{n}")]);
    }

    if let Some(solved_n) = report.solved_n {
        table.add_row(vec!["[SOLVED] Required N per arm", &format!("{solved_n}")]);
        table.add_row(vec![
            "Total Study Sample Size",
            &format!("{}", solved_n.saturating_mul(report.arms)),
        ]);
    }

    if let Some(solved_mde) = report.solved_mde {
        table.add_row(vec![
            "[SOLVED] Minimum Detectable Effect (MDE delta)",
            &format!("{solved_mde:.4}"),
        ]);
        if let Some(n) = report.n {
            table.add_row(vec![
                "Total Study Sample Size",
                &format!("{}", n.saturating_mul(report.arms)),
            ]);
        }
    }

    if let Some(cost) = report.cost_per_instance {
        table.add_row(vec!["Cost per Instance", &format!("${cost:.2}")]);
    }

    if let Some(tot) = report.total_cost {
        table.add_row(vec!["Total Estimated Cost", &format!("${tot:.2}")]);
    }

    out.push_str(&table.to_string());
    out.push('\n');

    if let Some(note) = &report.bonferroni_note {
        out.push_str("⚠️ Note: ");
        out.push_str(note);
        out.push_str("\n\n");
    }

    out
}
