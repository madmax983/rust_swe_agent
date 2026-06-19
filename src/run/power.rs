#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::suboptimal_flops,
    clippy::manual_midpoint,
    clippy::unreadable_literal,
    clippy::excessive_precision,
    clippy::many_single_char_names,
    clippy::while_float
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

    let h_neg = if baseline_rate - delta >= 0.0 {
        cohens_h(baseline_rate, baseline_rate - delta)
    } else {
        f64::INFINITY
    };
    let h_pos = if baseline_rate + delta <= 1.0 {
        cohens_h(baseline_rate, baseline_rate + delta)
    } else {
        f64::INFINITY
    };

    // Pick the smaller positive Cohen's h to solve for the more conservative (larger) sample size.
    let h = if h_neg <= 0.0 {
        h_pos
    } else if h_pos <= 0.0 {
        h_neg
    } else {
        h_neg.min(h_pos)
    };

    if h <= 0.0 || h.is_nan() || h.is_infinite() {
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
    if n_approx.is_nan() || !n_approx.is_finite() || n_approx > usize::MAX as f64 {
        return 0;
    }

    let mut n = (n_approx.floor() as usize).max(1);

    let check_power = |val_n: usize| -> f64 {
        let n_eff = val_n as f64 / 2.0;
        if one_sided {
            phi(h * n_eff.sqrt() - z_crit)
        } else {
            phi(h * n_eff.sqrt() - z_crit) + phi(-h * n_eff.sqrt() - z_crit)
        }
    };

    // Precise search loop:
    // If starting n already meets power, search downward using binary search to find the absolute minimum n meeting power.
    // If starting n does not meet power, search upward to find the first n meeting power.
    let mut iterations = 0;
    if check_power(n) >= power {
        let mut low = 1;
        let mut high = n;
        while low < high {
            let mid = low + (high - low) / 2;
            if check_power(mid) >= power {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        n = high;
    } else {
        while check_power(n) < power {
            if n == usize::MAX || iterations >= MAX_ITERATIONS {
                return 0;
            }
            n += 1;
            iterations += 1;
        }
    }

    n
}

/// Bisection search to find Cohen's h satisfying target power.
fn solve_h_for_power(n: usize, alpha: f64, target_power: f64, one_sided: bool) -> f64 {
    if target_power <= alpha {
        return 0.0;
    }

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
fn h_to_delta(p1: f64, h: f64) -> Option<f64> {
    if h == 0.0 {
        return Some(0.0);
    }
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
        // Report the larger (conservative/robust) delta
        Some(d_pos.max(d_neg))
    } else if !d_pos.is_nan() {
        Some(d_pos)
    } else if !d_neg.is_nan() {
        Some(d_neg)
    } else {
        None
    }
}

pub fn run(cmd: &PowerCmd) -> Result<PowerReport, Error> {
    // 1. Validation Checks
    if cmd.alpha.is_nan() || cmd.alpha <= 0.0 || cmd.alpha >= 1.0 {
        return Err(Error::Config(crate::error::ConfigError::Usage(format!(
            "Significance level alpha must be between 0.0 and 1.0, got {}",
            cmd.alpha
        ))));
    }
    if cmd.power.is_nan() || cmd.power <= 0.0 || cmd.power >= 1.0 {
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

    if let Some(c) = cmd.cost_per_instance {
        if c.is_nan() || c < 0.0 || !c.is_finite() {
            return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                "Cost per instance must be a finite, non-negative number, got {c}"
            ))));
        }
    }

    // 2. Resolve Baseline Resolved Rate
    let baseline_rate = match (cmd.baseline_rate, &cmd.from_sweep) {
        (Some(b), None) => {
            if b.is_nan() || !(0.0..=1.0).contains(&b) {
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
                    .filter(|inst| crate::run::swebench::resolved_count(inst) > 0)
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

    if let Some(delta) = cmd.delta {
        if delta.is_nan() || delta <= 0.0 || delta >= 1.0 {
            return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                "Target delta must be strictly positive and less than 1.0, got {delta}"
            ))));
        }

        let has_valid_lower = (baseline_rate - delta) >= 0.0;
        let has_valid_upper = (baseline_rate + delta) <= 1.0;

        if !has_valid_lower && !has_valid_upper {
            return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                "Impossible baseline rate ({baseline_rate}) and delta ({delta}) combination: comparison rate is outside [0, 1]"
            ))));
        }

        // Check if Cohen's h collapses to zero under floating-point precision
        let h_neg = if has_valid_lower {
            cohens_h(baseline_rate, baseline_rate - delta)
        } else {
            f64::INFINITY
        };
        let h_pos = if has_valid_upper {
            cohens_h(baseline_rate, baseline_rate + delta)
        } else {
            f64::INFINITY
        };
        let h = h_neg.min(h_pos);

        if h <= 0.0 || h.is_nan() {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "The requested delta is too small and collapses to zero effect size under floating-point precision".to_string(),
            )));
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

    // Target power must be strictly greater than significance level alpha (taking Bonferroni corrections into account).
    if cmd.power <= adjusted_alpha {
        return Err(Error::Config(crate::error::ConfigError::Usage(format!(
            "Target statistical power ({}) must be strictly greater than the (possibly corrected) significance level alpha ({})",
            cmd.power, adjusted_alpha
        ))));
    }

    // Extreme precision bounds check on adjusted alpha and power
    let z_crit = if cmd.one_sided {
        inverse_phi(1.0 - adjusted_alpha)
    } else {
        inverse_phi(1.0 - adjusted_alpha / 2.0)
    };
    let z_power = inverse_phi(cmd.power);

    if !z_crit.is_finite() || !z_power.is_finite() {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "Significance level alpha or power is too extreme to be resolved with finite float precision".to_string(),
        )));
    }

    // 4. Mode Solver
    let mut solved_n = None;
    let mut solved_mde = None;

    if let Some(delta) = cmd.delta {
        // Mode A: Solve for Sample Size
        let n = solve_sample_size(
            baseline_rate,
            delta,
            adjusted_alpha,
            cmd.power,
            cmd.one_sided,
        );
        if n == 0 {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "The required sample size is too large to be represented (infeasible precision or extremely small delta).".to_string()
            )));
        }
        solved_n = Some(n);
    } else if let Some(n) = cmd.n {
        // Mode B: Solve for MDE
        let h = solve_h_for_power(n, adjusted_alpha, cmd.power, cmd.one_sided);
        let mde = h_to_delta(baseline_rate, h).ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Usage(format!(
                "Infeasible configuration: required Cohen's h ({h:.4}) cannot be resolved from baseline rate {baseline_rate} for sample size {n}"
            )))
        })?;
        solved_mde = Some(mde);
    }

    // 5. Cost calculation
    let cost_per_instance = match (cmd.cost_per_instance, &cmd.from_forecast) {
        (Some(c), None) => {
            // Already validated at CLI boundary, but safe sanity check.
            if c.is_nan() || c < 0.0 || !c.is_finite() {
                return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                    "Cost per instance must be a finite, non-negative number, got {c}"
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

            if target_n.is_nan() || target_n <= 0.0 || target_n.fract() != 0.0 {
                return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                    "Forecast file `/forecast/target_n` must be a positive integer, got {target_n}"
                ))));
            }

            let point_cost = val
                .pointer("/forecast/total_cost_usd/point")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| {
                    Error::Config(crate::error::ConfigError::Usage(
                        "Forecast file missing `/forecast/total_cost_usd/point` field".to_string(),
                    ))
                })?;

            if point_cost.is_nan() || point_cost < 0.0 || !point_cost.is_finite() {
                return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                    "Forecast total cost must be non-negative and finite, got {point_cost}"
                ))));
            }

            Some(point_cost / target_n)
        }
        (None, None) => None,
        _ => unreachable!(),
    };

    let total_cost = if let Some(c) = cost_per_instance {
        let size = solved_n.or(cmd.n).unwrap_or(0);
        let tc = size as f64 * cmd.arms as f64 * c;
        if !tc.is_finite() {
            return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                "Total study cost calculation overflowed (result must be finite): size ({size}) * arms ({}) * cost per instance ({c}) = {tc}",
                cmd.arms
            ))));
        }
        Some(tc)
    } else {
        None
    };

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
    let mut out = String::new();
    out.push_str("\n📊 Statistical Power Analysis Report\n");

    let mut table = crate::ui::create_table();
    table.set_header(vec!["Parameter", "Value"]);

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
