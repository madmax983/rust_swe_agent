//! `bench frontier`: Pareto-frontier view across multiple sweep runs.
//!
//! For each sweep directory computes `(resolved_rate, cost_per_resolved_usd)`
//! and marks which runs lie on the efficient frontier — i.e. no other run is
//! simultaneously cheaper and higher-resolved.  Emits an ASCII chart (text
//! mode) or a JSON dataset (json mode) suitable for downstream plotting.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Error;
use crate::run::compare::{LoadedSweep, load_evaluation_results, load_sweep};
use crate::run::evaluate::{EvaluationResults, pct};
use crate::run::swebench::InstanceResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FrontierFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrontierPoint {
    /// Path to the sweep directory.
    pub dir: PathBuf,
    /// Total instances in the sweep.
    pub instances: usize,
    /// Number resolved (from `evaluation.json` when present, else sweep row).
    pub resolved: usize,
    /// `resolved / instances`.
    pub resolved_rate: f64,
    /// Total USD cost across all instances.
    pub total_cost_usd: f64,
    /// `total_cost_usd / resolved`. `None` when `resolved == 0`.
    pub cost_per_resolved_usd: Option<f64>,
    /// Whether this point lies on the Pareto-efficient frontier.
    pub on_frontier: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrontierReport {
    pub points: Vec<FrontierPoint>,
}

pub struct FrontierArgs {
    pub dirs: Vec<PathBuf>,
}

pub fn compute(args: &FrontierArgs) -> Result<FrontierReport, Error> {
    let mut points: Vec<FrontierPoint> = Vec::with_capacity(args.dirs.len());
    for dir in &args.dirs {
        let point = load_point(dir)?;
        points.push(point);
    }
    mark_pareto_frontier(&mut points);
    Ok(FrontierReport { points })
}

fn load_point(dir: &Path) -> Result<FrontierPoint, Error> {
    let loaded: LoadedSweep = load_sweep(dir)?;
    let model_name = loaded.manifest.as_ref().map(|m| m.model.name.as_str());
    let eval: Option<EvaluationResults> = load_evaluation_results(dir)?;
    let instances = loaded.instances.len();

    let resolved = if let Some(ref ev) = eval {
        ev.instances.iter().filter(|r| r.resolved).count()
    } else {
        count_sweep_resolved(&loaded.instances)
    };

    let total_cost_usd: f64 = loaded
        .instances
        .values()
        .filter_map(|r| r.effective_cost_usd(model_name))
        .sum();

    #[allow(clippy::cast_precision_loss)]
    let cost_per_resolved_usd = if resolved == 0 {
        None
    } else {
        Some(total_cost_usd / resolved as f64)
    };

    Ok(FrontierPoint {
        dir: dir.to_path_buf(),
        instances,
        resolved,
        resolved_rate: pct(resolved, instances),
        total_cost_usd,
        cost_per_resolved_usd,
        on_frontier: false,
    })
}

fn count_sweep_resolved(instances: &std::collections::HashMap<String, InstanceResult>) -> usize {
    instances.values().filter(|r| r.resolved_count > 0).count()
}

/// Mark each point as on-frontier if no other point dominates it.
///
/// Point A dominates point B if A has a higher (or equal) resolved_rate AND
/// a lower (or equal) cost_per_resolved_usd, with at least one strict
/// inequality. Points with `cost_per_resolved_usd == NaN` (resolved=0) are
/// never on the frontier unless all points have resolved=0.
fn mark_pareto_frontier(points: &mut [FrontierPoint]) {
    let n = points.len();
    let all_none = points.iter().all(|p| p.cost_per_resolved_usd.is_none());
    for i in 0..n {
        let dominated = if all_none {
            false
        } else {
            points[i].cost_per_resolved_usd.is_none()
                || (0..n)
                    .filter(|&j| j != i)
                    .any(|j| dominates(&points[j], &points[i]))
        };
        points[i].on_frontier = !dominated;
    }
}

/// Returns true when `a` weakly dominates `b` with at least one strict
/// improvement on the (resolved_rate, cost_per_resolved_usd) axes.
fn dominates(a: &FrontierPoint, b: &FrontierPoint) -> bool {
    let (Some(a_cost), Some(b_cost)) = (a.cost_per_resolved_usd, b.cost_per_resolved_usd) else {
        // a has no resolved instances → cannot dominate
        // b has no resolved instances → a (with any resolved) dominates
        return a.cost_per_resolved_usd.is_some();
    };
    let better_rate = a.resolved_rate > b.resolved_rate + f64::EPSILON;
    let equal_rate = (a.resolved_rate - b.resolved_rate).abs() <= f64::EPSILON;
    let better_cost = a_cost < b_cost - f64::EPSILON;
    let equal_cost = (a_cost - b_cost).abs() <= f64::EPSILON;
    (better_rate || equal_rate) && (better_cost || equal_cost) && (better_rate || better_cost)
}

#[must_use]
pub fn render_json(report: &FrontierReport) -> String {
    serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_owned())
}

#[must_use]
pub fn render_text(report: &FrontierReport) -> String {
    let mut out = String::new();
    out.push_str("\n=== bench frontier ===\n");
    if report.points.is_empty() {
        out.push_str("No sweep directories provided.\n");
        return out;
    }

    let col_width = report
        .points
        .iter()
        .map(|p| p.dir.display().to_string().len())
        .max()
        .unwrap_or(4)
        .max(4);

    let header = format!(
        "  {:<width$}  {:>10}  {:>14}  {:>8}  {}",
        "dir",
        "resolved%",
        "$/resolved",
        "frontier",
        "total_cost_usd",
        width = col_width
    );
    out.push_str(&header);
    out.push('\n');
    out.push_str(&"-".repeat(header.len()));
    out.push('\n');

    let mut sorted: Vec<&FrontierPoint> = report.points.iter().collect();
    sorted.sort_by(|a, b| {
        b.resolved_rate
            .partial_cmp(&a.resolved_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.cost_per_resolved_usd
                    .partial_cmp(&b.cost_per_resolved_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    for p in &sorted {
        let cpr = match p.cost_per_resolved_usd {
            Some(v) => format!("${v:.4}"),
            None => "NaN".to_owned(),
        };
        let frontier_mark = if p.on_frontier { "*" } else { " " };
        let _ = writeln!(
            out,
            "{} {:<width$}  {:>9.2}%  {:>14}  {:>8}  ${:.4}",
            frontier_mark,
            p.dir.display(),
            p.resolved_rate * 100.0,
            cpr,
            if p.on_frontier { "yes" } else { "no" },
            p.total_cost_usd,
            width = col_width
        );
    }

    out.push('\n');
    let frontier_count = report.points.iter().filter(|p| p.on_frontier).count();
    let _ = writeln!(
        out,
        "{frontier_count}/{} runs on the efficient frontier",
        report.points.len()
    );

    // ASCII scatter chart: resolved_rate on Y, cost_per_resolved on X
    write_ascii_chart(&mut out, &sorted);

    out
}

fn write_ascii_chart(out: &mut String, points: &[&FrontierPoint]) {
    const CHART_WIDTH: usize = 40;
    const CHART_HEIGHT: usize = 10;

    // Only plot points that have a finite cost (resolved > 0).
    let finite_points: Vec<(&FrontierPoint, f64)> = points
        .iter()
        .filter_map(|p| p.cost_per_resolved_usd.map(|c| (*p, c)))
        .collect();
    if finite_points.is_empty() {
        return;
    }

    let (min_cost, max_cost, min_rate, max_rate) = finite_points.iter().fold(
        (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ),
        |(min_c, max_c, min_r, max_r), (p, cost)| {
            (
                min_c.min(*cost),
                max_c.max(*cost),
                min_r.min(p.resolved_rate),
                max_r.max(p.resolved_rate),
            )
        },
    );

    let cost_range = (max_cost - min_cost).max(f64::EPSILON);
    let rate_range = (max_rate - min_rate).max(f64::EPSILON);

    let mut grid = vec![vec![' '; CHART_WIDTH]; CHART_HEIGHT];
    for (p, cost) in &finite_points {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let x = ((cost - min_cost) / cost_range * (CHART_WIDTH - 1) as f64).round() as usize;
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let y = ((p.resolved_rate - min_rate) / rate_range * (CHART_HEIGHT - 1) as f64).round()
            as usize;
        let y = CHART_HEIGHT - 1 - y.min(CHART_HEIGHT - 1);
        let x = x.min(CHART_WIDTH - 1);
        grid[y][x] = if p.on_frontier { '*' } else { 'o' };
    }

    out.push('\n');
    out.push_str("  resolved_rate\n");
    out.push_str("  ^\n");
    for row in &grid {
        out.push_str("  |");
        for &ch in row {
            out.push(ch);
        }
        out.push('\n');
    }
    out.push_str("  +");
    out.push_str(&"-".repeat(CHART_WIDTH));
    out.push_str("> cost_per_resolved_usd\n");
    out.push_str("  (* = on frontier, o = dominated)\n");
}
