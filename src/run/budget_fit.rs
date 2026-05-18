//! `bench budget-fit`: right-size step, cost, and wallclock caps from sweep data.
//!
//! Read-only: never re-runs instances, never calls a model, never modifies input sweeps.
//! Writes `budget-fit.json` and prints a ranked text report.
//!
//! See `docs/spec-budget-fit.md` for the full schema and algorithm specification.

#![allow(clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::compare::load_sweep;
use crate::run::swebench::InstanceResult;
use crate::trajectory::FailureCategory;

// ── axis constants ────────────────────────────────────────────────────────────

pub const AXIS_STEPS: &str = "steps";
pub const AXIS_COST_USD: &str = "cost_usd";
pub const AXIS_WALL_CLOCK_S: &str = "wall_clock_s";

// Outcome bucket labels
const BUCKET_RESOLVED: &str = "resolved";
const BUCKET_UNRESOLVED_CAP_BOUND: &str = "unresolved_cap_bound";
const BUCKET_UNRESOLVED_OTHER: &str = "unresolved_other";
const BUCKET_ERRORED: &str = "errored";

// Action classes that indicate forward progress (raising cap likely helps)
const PROGRESS_CLASSES: &[&str] = &["write", "test", "build"];
// Action classes that indicate the agent was stuck (raising cap unlikely to help)
const STUCK_CLASSES: &[&str] = &["noop", "read", "nav"];

// Rounding units per axis
const UNIT_STEPS: f64 = 1.0;
const UNIT_COST_USD: f64 = 0.01;
const UNIT_WALL_CLOCK_S: f64 = 1.0;

// Multiplier for "raise" recommendation: configured_cap × RAISE_MULTIPLIER
const RAISE_MULTIPLIER: f64 = 1.5;

// ── public args / types ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BudgetFitArgs {
    pub sweep_dir: PathBuf,
    /// Fraction of configured cap within which an instance is considered "at cap".
    /// Default: 0.05 (5%).
    pub at_cap_tolerance: f64,
    /// Which percentile of the resolved distribution to use as `recommended_cap`.
    /// Default: 95.
    pub target_percentile: u8,
    /// When `Some`, restrict output to a single axis ("steps", "cost_usd", "wall_clock_s").
    pub axis: Option<String>,
    /// Key=value filters applied before analysis (same syntax as `bench inspect --filter`).
    pub filter: Vec<String>,
}

/// Distribution statistics for one outcome bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributionStats {
    pub count: usize,
    pub p10: Option<f64>,
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    pub max: Option<f64>,
    pub mean: Option<f64>,
}

/// Conservative projected impact of changing a cap value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectedImpact {
    /// Positive = more resolved instances expected; negative = fewer.
    pub estimated_resolved_delta: i64,
    /// Positive = more spend expected; negative = savings.
    pub estimated_cost_delta_usd: f64,
    pub derivation: String,
}

/// Per-axis right-sizing report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxisReport {
    pub axis_name: String,
    /// Configured cap value, in axis units. `None` when no cap was set for this axis.
    pub configured_cap: Option<f64>,
    /// The manifest field path that sourced `configured_cap`.
    pub configured_cap_source: Option<String>,
    /// Display unit (e.g. "steps", "USD", "seconds").
    pub unit: String,
    /// Per-outcome-bucket distribution statistics.
    pub distribution_by_outcome: BTreeMap<String, DistributionStats>,
    /// Number of instances within `at_cap_tolerance` of the configured cap.
    pub at_cap_count: usize,
    /// Fraction of total instances that are at-cap.
    pub at_cap_share: f64,
    /// Primary recommended cap. P{target_percentile} of the resolved distribution
    /// (rounded up to unit), OR configured_cap × 1.5 when behavior enrichment shows
    /// progress-class actions on cap-bound instances. `None` when insufficient data.
    pub recommended_cap: Option<f64>,
    /// Human-readable rationale for the recommendation.
    pub recommended_cap_rationale: String,
    /// Projected impact of implementing `recommended_cap`.
    pub projected_impact_if_recommended: Option<ProjectedImpact>,
    /// Projected impact of tightening to P95 of the resolved distribution
    /// (differs from `projected_impact_if_recommended` when target_percentile ≠ 95).
    pub projected_impact_if_tightened_to_p95: Option<ProjectedImpact>,
    /// Total cost_usd of cap-bound instances for this axis (excluded from JSON schema;
    /// used internally to compute cross-axis waste_estimate_usd without double-counting).
    #[serde(skip)]
    pub cap_bound_cost_usd: f64,
}

/// Cross-axis summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossAxisSummary {
    /// The axis with the most cap-bound unresolved instances. `None` when no axis dominates.
    pub dominant_axis: Option<String>,
    pub dominant_axis_reason: String,
    /// Conservative estimate of USD spent on instances that hit a cap without making progress.
    pub waste_estimate_usd: f64,
    /// One-sentence recommendation suitable for a sweep-config-change PR.
    pub headline_recommendation: String,
}

/// Full budget-fit report, written to `budget-fit.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetFitReport {
    pub sweep: String,
    pub generated_at: String,
    pub axes: Vec<AxisReport>,
    pub summary: CrossAxisSummary,
}

// ── public entry point ────────────────────────────────────────────────────────

/// Compute the budget-fit report for a completed sweep directory.
///
/// Reads `results.json` (required) and `behavior.json` (optional, enables enrichment).
/// Never re-runs instances, never calls a model.
pub fn compute_budget_fit(args: &BudgetFitArgs) -> Result<BudgetFitReport, Error> {
    let loaded = load_sweep(&args.sweep_dir)?;
    let instances: Vec<&InstanceResult> = loaded.instances.values().collect();

    // Optional behavior enrichment
    let behavior_map = load_behavior_map(&args.sweep_dir);

    // Extract configured caps from manifest
    let argv = loaded
        .manifest
        .as_ref()
        .map(|m| m.cli.argv.clone())
        .unwrap_or_default();
    let cost_limit_from_results = loaded
        .manifest
        .as_ref()
        .and_then(|_| None::<f64>); // populated from SweepResults.cost_limit_usd below
    let _ = cost_limit_from_results;

    // Re-read raw results to get cost_limit_usd
    let cost_limit_usd = read_cost_limit(&args.sweep_dir);
    let step_limit = extract_argv_value(&argv, "--step-limit")
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v as f64);
    let task_timeout_secs = extract_argv_value(&argv, "--task-timeout-secs")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v as f64);
    let per_task_budget_usd = extract_argv_value(&argv, "--per-task-budget-usd")
        .and_then(|v| v.parse::<f64>().ok());

    // Apply instance filters (same key=value syntax as bench inspect)
    let instances = apply_filter(instances, &args.filter);

    let n_total = instances.len();

    // Build per-axis reports
    let all_axes = [
        build_axis_report(
            AXIS_STEPS,
            "steps",
            step_limit,
            step_limit.map(|_| "manifest.cli.argv[--step-limit]".to_owned()),
            &instances,
            &behavior_map,
            args.at_cap_tolerance,
            args.target_percentile,
            UNIT_STEPS,
            n_total,
        ),
        build_axis_report(
            AXIS_COST_USD,
            "USD",
            cost_limit_usd.or(per_task_budget_usd),
            cost_limit_usd
                .map(|_| "results.json[cost_limit_usd]".to_owned())
                .or_else(|| {
                    per_task_budget_usd
                        .map(|_| "manifest.cli.argv[--per-task-budget-usd]".to_owned())
                }),
            &instances,
            &behavior_map,
            args.at_cap_tolerance,
            args.target_percentile,
            UNIT_COST_USD,
            n_total,
        ),
        build_axis_report(
            AXIS_WALL_CLOCK_S,
            "seconds",
            task_timeout_secs,
            task_timeout_secs.map(|_| "manifest.cli.argv[--task-timeout-secs]".to_owned()),
            &instances,
            &behavior_map,
            args.at_cap_tolerance,
            args.target_percentile,
            UNIT_WALL_CLOCK_S,
            n_total,
        ),
    ];

    let axes: Vec<AxisReport> = match &args.axis {
        None => all_axes.into_iter().collect(),
        Some(name) => all_axes
            .into_iter()
            .filter(|a| a.axis_name == *name)
            .collect(),
    };

    let summary = build_summary(&axes);
    let sweep_label = args
        .sweep_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.sweep_dir.display().to_string());

    Ok(BudgetFitReport {
        sweep: sweep_label,
        generated_at: utc_now_iso8601(),
        axes,
        summary,
    })
}

/// Entry point called from the CLI. Writes `budget-fit.json` to the sweep dir.
pub fn run(args: &BudgetFitArgs) -> Result<BudgetFitReport, Error> {
    let report = compute_budget_fit(args)?;
    let output_path = args.sweep_dir.join("budget-fit.json");
    let file = std::fs::File::create(&output_path)?;
    serde_json::to_writer_pretty(file, &report)?;
    Ok(report)
}

// ── text rendering ────────────────────────────────────────────────────────────

pub fn render_text(report: &BudgetFitReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Budget Fit Report: {}", report.sweep);
    let _ = writeln!(s, "Generated: {}", report.generated_at);
    let _ = writeln!(s);

    // Summary
    let _ = writeln!(s, "## Summary");
    let _ = writeln!(s, "  {}", report.summary.headline_recommendation);
    if let Some(ax) = &report.summary.dominant_axis {
        let _ = writeln!(s, "  Dominant axis: {ax}");
    }
    let _ = writeln!(
        s,
        "  Waste estimate: ${:.4}",
        report.summary.waste_estimate_usd
    );
    let _ = writeln!(s);

    // Per-axis sections
    for axis in &report.axes {
        let _ = writeln!(s, "## Axis: {} ({})", axis.axis_name, axis.unit);
        if let Some(cap) = axis.configured_cap {
            let _ = writeln!(s, "  Configured cap: {cap:.4}");
        } else {
            let _ = writeln!(s, "  Configured cap: (none)");
        }
        if let Some(rec) = axis.recommended_cap {
            let _ = writeln!(s, "  Recommended cap: {rec:.4}");
        } else {
            let _ = writeln!(s, "  Recommended cap: (none — {}", axis.recommended_cap_rationale);
            let _ = writeln!(s, "  )");
        }
        let _ = writeln!(s, "  At-cap count: {} ({:.1}%)", axis.at_cap_count, axis.at_cap_share * 100.0);
        let _ = writeln!(s, "  Rationale: {}", axis.recommended_cap_rationale);

        for (bucket, stats) in &axis.distribution_by_outcome {
            let _ = writeln!(s, "  [{bucket}] count={}", stats.count);
            if let (Some(p50), Some(p95), Some(max)) = (stats.p50, stats.p95, stats.max) {
                let _ = writeln!(s, "    p50={p50:.2}  p95={p95:.2}  max={max:.2}");
            }
        }
        if let Some(impact) = &axis.projected_impact_if_recommended {
            let _ = writeln!(
                s,
                "  Projected impact (recommended): Δresolved={}, Δcost=${:.4}",
                impact.estimated_resolved_delta, impact.estimated_cost_delta_usd
            );
        }
        let _ = writeln!(s);
    }
    s
}

// ── internal helpers ──────────────────────────────────────────────────────────

/// Extract a value following a named flag from an argv slice.
fn extract_argv_value(argv: &[String], flag: &str) -> Option<String> {
    for i in 0..argv.len() {
        if argv[i] == flag {
            return argv.get(i + 1).cloned();
        }
        // Support --flag=value form
        let prefix = format!("{flag}=");
        if let Some(val) = argv[i].strip_prefix(&prefix) {
            return Some(val.to_owned());
        }
    }
    None
}

/// Read `cost_limit_usd` from `results.json` (top-level field).
fn read_cost_limit(sweep_dir: &Path) -> Option<f64> {
    let path = sweep_dir.join("results.json");
    let text = std::fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&text).ok()?;
    val.get("cost_limit_usd")?.as_f64()
}

/// Map of instance_id → dominant action class, loaded from `behavior.json`.
/// Returns empty map when file is absent or unreadable.
fn load_behavior_map(sweep_dir: &Path) -> BTreeMap<String, String> {
    let path = sweep_dir.join("behavior.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return BTreeMap::new(),
    };
    let val: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return BTreeMap::new(),
    };
    let per_instance = match val.get("per_instance").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return BTreeMap::new(),
    };
    let mut map = BTreeMap::new();
    for inst in per_instance {
        let id = match inst.get("instance_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let class_counts = match inst.get("class_counts").and_then(|v| v.as_object()) {
            Some(m) => m,
            None => continue,
        };
        // Dominant class = the one with the highest count
        let dominant = class_counts
            .iter()
            .max_by_key(|(_, v)| v.as_u64().unwrap_or(0))
            .map(|(k, _)| k.clone());
        if let Some(cls) = dominant {
            map.insert(id, cls);
        }
    }
    map
}

/// Apply key=value filters to the instance list.
fn apply_filter<'a>(
    instances: Vec<&'a InstanceResult>,
    filters: &[String],
) -> Vec<&'a InstanceResult> {
    if filters.is_empty() {
        return instances;
    }
    instances
        .into_iter()
        .filter(|inst| {
            filters.iter().all(|f| {
                let mut it = f.splitn(2, '=');
                let key = it.next().unwrap_or_default().trim();
                let val = it.next().unwrap_or_default().trim();
                match key {
                    "failure_category" => inst
                        .failure_category
                        .is_some_and(|c| failure_category_label(c) == val),
                    "resolved" => {
                        let expected = val == "true";
                        (inst.resolved_count > 0) == expected
                    }
                    _ => true, // unknown filter keys are ignored (not an error)
                }
            })
        })
        .collect()
}

fn failure_category_label(c: FailureCategory) -> &'static str {
    match c {
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::BudgetExhausted => "budget_exhausted",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::AgentStagnation => "agent_stagnation",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::HistoryCompactionFailed => "history_compaction_failed",
        FailureCategory::Unknown => "unknown",
    }
}

/// Which `FailureCategory` marks "at-cap" for a given axis.
fn cap_failure_category(axis: &str) -> Option<FailureCategory> {
    match axis {
        AXIS_STEPS => Some(FailureCategory::StepLimit),
        AXIS_COST_USD => Some(FailureCategory::CostLimit),
        AXIS_WALL_CLOCK_S => Some(FailureCategory::WallclockTimeout),
        _ => None,
    }
}

/// Determine which outcome bucket an instance belongs to for the given axis.
fn outcome_bucket(inst: &InstanceResult, cap_cat: Option<FailureCategory>) -> &'static str {
    if inst.resolved_count > 0 {
        return BUCKET_RESOLVED;
    }
    if let (Some(c), Some(expected)) = (inst.failure_category, cap_cat) {
        if c == expected {
            return BUCKET_UNRESOLVED_CAP_BOUND;
        }
    }
    // Errored instances: any non-submitted outcome with a failure category that isn't
    // env_setup / model_api / agent_internal treated as errors (these are not
    // "cap-bound" and are not "resolved")
    match inst.outcome.as_deref() {
        Some("submitted") => BUCKET_UNRESOLVED_OTHER, // submitted but not resolved
        _ => {
            // If it has any failure category at all, call it errored (broad; operator can filter)
            if inst.failure_category.is_some() {
                // Cap-bound already handled above; this is non-cap failure
                BUCKET_ERRORED
            } else {
                BUCKET_UNRESOLVED_OTHER
            }
        }
    }
}

/// Extract the axis value for an instance.
fn axis_value(inst: &InstanceResult, axis: &str) -> Option<f64> {
    match axis {
        AXIS_STEPS => inst.steps.map(|s| s as f64),
        AXIS_COST_USD => inst.cost_usd,
        AXIS_WALL_CLOCK_S => inst.duration_secs,
        _ => None,
    }
}

/// Build an `AxisReport` for one axis.
#[allow(clippy::too_many_arguments)]
fn build_axis_report(
    axis: &str,
    unit: &str,
    configured_cap: Option<f64>,
    configured_cap_source: Option<String>,
    instances: &[&InstanceResult],
    behavior_map: &BTreeMap<String, String>,
    at_cap_tolerance: f64,
    target_percentile: u8,
    round_unit: f64,
    n_total: usize,
) -> AxisReport {
    let cap_cat = cap_failure_category(axis);

    // Bucket instances and collect values
    let mut resolved_values: Vec<f64> = Vec::new();
    let mut cap_bound_values: Vec<f64> = Vec::new();
    let mut other_values: Vec<f64> = Vec::new();
    let mut errored_values: Vec<f64> = Vec::new();

    // Track cap-bound instances by class (for behavior enrichment)
    let mut cap_bound_progress_count: i64 = 0;
    let mut cap_bound_stuck_count: i64 = 0;
    let mut cap_bound_total_cost: f64 = 0.0;

    for inst in instances {
        let bucket = outcome_bucket(inst, cap_cat);
        if let Some(v) = axis_value(inst, axis) {
            match bucket {
                BUCKET_RESOLVED => resolved_values.push(v),
                BUCKET_UNRESOLVED_CAP_BOUND => {
                    cap_bound_values.push(v);
                    // Behavior enrichment: classify this instance's final action class
                    if let Some(cls) = behavior_map.get(&inst.instance_id) {
                        if PROGRESS_CLASSES.contains(&cls.as_str()) {
                            cap_bound_progress_count += 1;
                        } else if STUCK_CLASSES.contains(&cls.as_str()) {
                            cap_bound_stuck_count += 1;
                        }
                    }
                    cap_bound_total_cost += inst.cost_usd.unwrap_or(0.0);
                }
                BUCKET_UNRESOLVED_OTHER => other_values.push(v),
                _ => errored_values.push(v),
            }
        } else if outcome_bucket(inst, cap_cat) == BUCKET_UNRESOLVED_CAP_BOUND {
            // Count cost even without axis value
            cap_bound_total_cost += inst.cost_usd.unwrap_or(0.0);
        }
    }

    // Distribution stats per bucket
    let mut distribution_by_outcome: BTreeMap<String, DistributionStats> = BTreeMap::new();
    distribution_by_outcome.insert(BUCKET_RESOLVED.into(), compute_distribution(&resolved_values));
    distribution_by_outcome.insert(
        BUCKET_UNRESOLVED_CAP_BOUND.into(),
        compute_distribution(&cap_bound_values),
    );
    distribution_by_outcome.insert(
        BUCKET_UNRESOLVED_OTHER.into(),
        compute_distribution(&other_values),
    );
    distribution_by_outcome.insert(BUCKET_ERRORED.into(), compute_distribution(&errored_values));

    // At-cap computation
    let (at_cap_count, at_cap_share) =
        compute_at_cap(instances, axis, configured_cap, at_cap_tolerance, n_total);

    // Recommendation logic
    let (recommended_cap, recommended_cap_rationale, projected_impact_if_recommended) =
        make_recommendation(
            axis,
            configured_cap,
            &resolved_values,
            &cap_bound_values,
            cap_bound_progress_count,
            cap_bound_stuck_count,
            cap_bound_total_cost,
            behavior_map.is_empty(),
            target_percentile,
            round_unit,
            instances,
        );

    // projected_impact_if_tightened_to_p95 (always uses p95, not target_percentile)
    let projected_impact_if_tightened_to_p95 =
        configured_cap.and_then(|cap| {
            if resolved_values.is_empty() {
                return None;
            }
            let p95 = percentile_of_sorted(&{
                let mut v = resolved_values.clone();
                v.sort_by(f64_cmp);
                v
            }, 95.0)?;
            let tighten_cap = round_up(p95, round_unit);
            if tighten_cap >= cap {
                return None; // no tightening needed
            }
            let lost_resolved = resolved_values.iter().filter(|&&v| v > tighten_cap).count() as i64;
            let savings = estimated_cost_savings(instances, axis, cap, tighten_cap);
            Some(ProjectedImpact {
                estimated_resolved_delta: -lost_resolved,
                estimated_cost_delta_usd: -savings,
                derivation: format!(
                    "Tightening from {cap:.4} to {tighten_cap:.4}: \
                     {lost_resolved} resolved instances use more than {tighten_cap:.4} {axis}; \
                     estimated savings ${savings:.4}."
                ),
            })
        });

    AxisReport {
        axis_name: axis.to_owned(),
        configured_cap,
        configured_cap_source,
        unit: unit.to_owned(),
        distribution_by_outcome,
        at_cap_count,
        at_cap_share,
        recommended_cap,
        recommended_cap_rationale,
        projected_impact_if_recommended,
        projected_impact_if_tightened_to_p95,
        cap_bound_cost_usd: cap_bound_total_cost,
    }
}

/// Compute recommended cap, rationale, and projected impact.
#[allow(clippy::too_many_arguments)]
fn make_recommendation(
    axis: &str,
    configured_cap: Option<f64>,
    resolved_values: &[f64],
    cap_bound_values: &[f64],
    cap_bound_progress_count: i64,
    cap_bound_stuck_count: i64,
    _cap_bound_total_cost: f64,
    behavior_absent: bool,
    target_percentile: u8,
    round_unit: f64,
    instances: &[&InstanceResult],
) -> (Option<f64>, String, Option<ProjectedImpact>) {
    // No cap configured for this axis
    if configured_cap.is_none() {
        return (
            None,
            "no cap configured for this axis".to_owned(),
            None,
        );
    }
    let cap = configured_cap.unwrap();

    // P{target} of resolved
    let resolved_p_target = {
        let mut sorted = resolved_values.to_vec();
        sorted.sort_by(f64_cmp);
        percentile_of_sorted(&sorted, target_percentile as f64).map(|v| round_up(v, round_unit))
    };

    let cap_bound_count = cap_bound_values.len();

    // --- Behavior-enriched recommendation ---
    // When behavior.json is present and most cap-bound instances had progress-class actions,
    // recommend raising the cap; otherwise recommend tightening to p_target of resolved.
    let behavior_present = !behavior_absent && cap_bound_count > 0;
    let has_progress_class = cap_bound_progress_count > 0
        && cap_bound_progress_count >= cap_bound_stuck_count;
    let has_stuck_class = cap_bound_stuck_count > 0
        && cap_bound_stuck_count > cap_bound_progress_count;

    if behavior_present && has_progress_class {
        // RAISE recommendation
        let new_cap = round_up(cap * RAISE_MULTIPLIER, round_unit);
        let rationale = format!(
            "{cap_bound_progress_count} cap-bound instance(s) had progress-class actions \
             (write/test/build); raising the cap from {cap:.4} to {new_cap:.4} is recommended. \
             ({cap_bound_stuck_count} were stuck.)"
        );
        let cost_per_unit = mean_cost_per_unit(instances, axis);
        let extra_units = new_cap - cap;
        let extra_cost = cost_per_unit * extra_units * cap_bound_progress_count as f64;
        let impact = ProjectedImpact {
            estimated_resolved_delta: cap_bound_progress_count,
            estimated_cost_delta_usd: extra_cost,
            derivation: format!(
                "Raising from {cap:.4} to {new_cap:.4}: \
                 {cap_bound_progress_count} progress-class cap-bound instance(s) may now resolve \
                 (conservative: excludes stuck instances). \
                 Extra cost estimate: ${extra_cost:.4} \
                 ({cost_per_unit:.6} USD/{axis}/instance × {extra_units:.4} extra {axis} \
                 × {cap_bound_progress_count} instance(s))."
            ),
        };
        return (Some(new_cap), rationale, Some(impact));
    }

    if behavior_present && has_stuck_class {
        // Do NOT raise — instances are stuck
        let recommended = resolved_p_target.unwrap_or(cap);
        let rationale = format!(
            "{cap_bound_stuck_count} cap-bound instance(s) had stuck-class actions \
             (noop/read/nav); raising the cap is unlikely to help. \
             Tightening to {recommended:.4} (P{target_percentile} of resolved) is recommended."
        );
        let impact = make_tighten_impact(
            instances,
            axis,
            cap,
            recommended,
            resolved_values,
            target_percentile,
        );
        return (Some(recommended), rationale, impact);
    }

    // --- Default: tighten to P{target} of resolved ---
    match resolved_p_target {
        None => {
            // No resolved instances → cannot compute P{target}
            let rationale = format!(
                "no resolved instances to base recommendation on; \
                 {} cap-bound failure(s) recorded.",
                cap_bound_count
            );
            (None, rationale, None)
        }
        Some(p_target) if p_target >= cap => {
            // P{target} is at or above cap — no tightening useful
            let rationale = format!(
                "P{target_percentile} of resolved ({p_target:.4}) ≥ configured cap ({cap:.4}); \
                 cap is already well-sized. {} cap-bound failure(s).",
                cap_bound_count
            );
            (Some(p_target), rationale, None)
        }
        Some(p_target) => {
            let rationale = if cap_bound_count > 0 && behavior_absent {
                format!(
                    "P{target_percentile} of resolved is {p_target:.4} (current cap: {cap:.4}). \
                     {cap_bound_count} cap-bound failure(s) detected; behavior.json not present — \
                     cannot determine if raising would help. Tightening recommended."
                )
            } else {
                format!(
                    "P{target_percentile} of resolved is {p_target:.4} (current cap: {cap:.4}); \
                     tightening frees {:.4} {axis} of headroom.",
                    cap - p_target
                )
            };
            let impact = make_tighten_impact(
                instances,
                axis,
                cap,
                p_target,
                resolved_values,
                target_percentile,
            );
            (Some(p_target), rationale, impact)
        }
    }
}

/// Build a tightening impact estimate (from `cap` down to `new_cap`).
fn make_tighten_impact(
    instances: &[&InstanceResult],
    axis: &str,
    old_cap: f64,
    new_cap: f64,
    resolved_values: &[f64],
    target_percentile: u8,
) -> Option<ProjectedImpact> {
    if new_cap >= old_cap {
        return None;
    }
    let lost_resolved = resolved_values
        .iter()
        .filter(|&&v| v > new_cap)
        .count() as i64;
    let savings = estimated_cost_savings(instances, axis, old_cap, new_cap);
    Some(ProjectedImpact {
        estimated_resolved_delta: -lost_resolved,
        estimated_cost_delta_usd: -savings,
        derivation: format!(
            "Tightening from {old_cap:.4} to {new_cap:.4} (P{target_percentile} of resolved): \
             {lost_resolved} resolved instance(s) use more than {new_cap:.4} {axis}; \
             estimated cost savings: ${savings:.4}."
        ),
    })
}

/// Estimate cost savings from tightening: mean cost-per-unit × cap reduction × eligible instances.
///
/// Result is rounded to 6 decimal places for deterministic serialization.
#[inline(never)]
fn estimated_cost_savings(
    instances: &[&InstanceResult],
    axis: &str,
    old_cap: f64,
    new_cap: f64,
) -> f64 {
    let cap_reduction = old_cap - new_cap;
    if cap_reduction <= 0.0 {
        return 0.0;
    }
    let cpu = mean_cost_per_unit(instances, axis);
    // Eligible: instances whose value exceeds new_cap (they'd terminate earlier)
    let eligible = instances
        .iter()
        .filter(|inst| axis_value(inst, axis).map_or(false, |v| v > new_cap))
        .count() as f64;
    round_to_ndp(cpu * cap_reduction * eligible, 6)
}

/// Mean cost (USD) per axis unit across all instances with both cost and axis data.
fn mean_cost_per_unit(instances: &[&InstanceResult], axis: &str) -> f64 {
    let pairs: Vec<(f64, f64)> = instances
        .iter()
        .filter_map(|inst| {
            let v = axis_value(inst, axis)?;
            let c = inst.cost_usd?;
            if v > 0.0 { Some((v, c)) } else { None }
        })
        .collect();
    if pairs.is_empty() {
        return 0.0;
    }
    let total_cost: f64 = pairs.iter().map(|(_, c)| c).sum();
    let total_units: f64 = pairs.iter().map(|(v, _)| v).sum();
    if total_units > 0.0 { total_cost / total_units } else { 0.0 }
}

/// Compute at_cap_count and at_cap_share.
fn compute_at_cap(
    instances: &[&InstanceResult],
    axis: &str,
    configured_cap: Option<f64>,
    tolerance: f64,
    n_total: usize,
) -> (usize, f64) {
    let Some(cap) = configured_cap else {
        return (0, 0.0);
    };
    let threshold = cap * (1.0 - tolerance);
    let count = instances
        .iter()
        .filter(|inst| {
            axis_value(inst, axis)
                .map_or(false, |v| v >= threshold)
        })
        .count();
    let share = if n_total > 0 {
        count as f64 / n_total as f64
    } else {
        0.0
    };
    (count, share)
}

/// Build the cross-axis summary.
fn build_summary(axes: &[AxisReport]) -> CrossAxisSummary {
    // Dominant axis: axis with most cap-bound unresolved instances
    let dominant = axes
        .iter()
        .filter_map(|a| {
            let cap_bound = a
                .distribution_by_outcome
                .get(BUCKET_UNRESOLVED_CAP_BOUND)?;
            if cap_bound.count > 0 {
                Some((a.axis_name.clone(), cap_bound.count))
            } else {
                None
            }
        })
        .max_by_key(|(_, c)| *c);

    let (dominant_axis, dominant_axis_reason) = match dominant {
        Some((name, count)) => (
            Some(name.clone()),
            format!("{count} cap-bound unresolved instance(s) on axis '{name}'"),
        ),
        None => (None, "no cap-bound failures detected".to_owned()),
    };

    let waste_usd = compute_waste_usd(axes);

    let headline = build_headline(axes, &dominant_axis);

    CrossAxisSummary {
        dominant_axis,
        dominant_axis_reason,
        waste_estimate_usd: waste_usd,
        headline_recommendation: headline,
    }
}

fn compute_waste_usd(axes: &[AxisReport]) -> f64 {
    // Sum cap_bound_cost_usd across all axes. Each instance is bucketed into exactly one
    // axis's cap_bound bucket (by its failure_category), so this sum has no double-counting.
    axes.iter().map(|a| a.cap_bound_cost_usd).sum()
}

fn build_headline(axes: &[AxisReport], dominant_axis: &Option<String>) -> String {
    match dominant_axis {
        None => "No cap-bound failures detected; consider tightening caps to reduce cost.".into(),
        Some(ax) => {
            let axis_report = axes.iter().find(|a| &a.axis_name == ax);
            let rec = axis_report.and_then(|a| a.recommended_cap);
            match rec {
                None => format!("Dominant axis '{ax}': insufficient data for recommendation."),
                Some(v) => format!(
                    "Dominant axis '{ax}': set cap to {v:.4} ({}); {}",
                    axis_report
                        .map(|a| a.unit.as_str())
                        .unwrap_or("units"),
                    axis_report
                        .map(|a| a.recommended_cap_rationale.as_str())
                        .unwrap_or("")
                ),
            }
        }
    }
}

// ── distribution mathematics ──────────────────────────────────────────────────

/// Compute distribution statistics for a slice of values (unsorted).
pub fn compute_distribution(values: &[f64]) -> DistributionStats {
    if values.is_empty() {
        return DistributionStats {
            count: 0,
            p10: None,
            p50: None,
            p90: None,
            p95: None,
            p99: None,
            max: None,
            mean: None,
        };
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64_cmp);
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    let max = sorted.last().copied();

    DistributionStats {
        count: n,
        p10: percentile_of_sorted(&sorted, 10.0),
        p50: percentile_of_sorted(&sorted, 50.0),
        p90: percentile_of_sorted(&sorted, 90.0),
        p95: percentile_of_sorted(&sorted, 95.0),
        p99: percentile_of_sorted(&sorted, 99.0),
        max,
        mean: Some(mean),
    }
}

/// Compute the p-th percentile of a sorted slice using linear interpolation.
fn percentile_of_sorted(sorted: &[f64], p: f64) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    if n == 1 {
        return Some(sorted[0]);
    }
    let rank = (p / 100.0) * (n - 1) as f64;
    let lo = rank as usize;
    let hi = (lo + 1).min(n - 1);
    let frac = rank - lo as f64;
    Some(sorted[lo] + frac * (sorted[hi] - sorted[lo]))
}

/// Round `v` up to the nearest multiple of `unit`.
fn round_up(v: f64, unit: f64) -> f64 {
    if unit <= 0.0 {
        return v;
    }
    (v / unit).ceil() * unit
}

/// Round to N decimal places for deterministic serialization.
fn round_to_ndp(v: f64, n: u32) -> f64 {
    let factor = 10f64.powi(n as i32);
    (v * factor).round() / factor
}

fn f64_cmp(a: &f64, b: &f64) -> std::cmp::Ordering {
    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}
