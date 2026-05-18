//! `bench budget-fit`: right-size step, cost, and wallclock caps from sweep data.
//!
//! Read-only: never re-runs instances, never calls a model, never modifies input sweeps.
//! Writes `budget-fit.json` and prints a ranked text report.
//!
//! See `docs/spec-budget-fit.md` for the full schema and algorithm specification.

#![allow(clippy::cast_precision_loss)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::compare::{load_evaluation_results, load_sweep};
use crate::run::swebench::{
    InstanceResult, SWEEP_STATUS_COMPLETED, resolved_count as instance_resolved_count,
};
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
#[allow(clippy::too_many_lines)]
pub fn compute_budget_fit(args: &BudgetFitArgs) -> Result<BudgetFitReport, Error> {
    // Require results.json to be present. load_sweep can succeed via trajectory
    // fallback without it, but budget-fit needs the completed-sweep summary for
    // manifest data (configured caps) and accurate aggregate statistics.
    let results_path = args.sweep_dir.join("results.json");
    if !results_path.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "budget-fit: results.json not found in {}; \
             this command requires a completed sweep directory",
            args.sweep_dir.display()
        ))));
    }

    // Check if sweep_status is explicitly present in results.json. When it is
    // and != "completed", reject immediately (no need to load all instances).
    let explicit_status = read_sweep_status(&args.sweep_dir);
    if let Some(ref s) = explicit_status {
        if s != SWEEP_STATUS_COMPLETED {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: sweep status is '{s}', not 'completed'; \
                 only completed sweeps produce reliable cap recommendations"
            ))));
        }
    }

    // Reject sweeps where a retry changed cap parameters. The merged results.json
    // keeps the original manifest caps while some rows ran with different limits,
    // so at-cap counts and percentile recommendations would be against the wrong cap.
    check_retry_cap_overrides(&args.sweep_dir)?;

    let loaded = load_sweep(&args.sweep_dir)?;

    // For legacy results.json files that lack sweep_status, confirm completeness via
    // manifest.runtime.finished_at_utc. An absent finished_at_utc means the sweep was
    // interrupted before writing a proper summary; load_sweep may still succeed via
    // trajectory scan, but the population is incomplete.
    if explicit_status.is_none() {
        let finished = loaded
            .manifest
            .as_ref()
            .and_then(|m| m.runtime.finished_at_utc.as_ref());
        if finished.is_none() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "budget-fit: legacy results.json lacks both sweep_status and \
                 manifest.runtime.finished_at_utc; cannot confirm sweep completed"
                    .into(),
            )));
        }
    }

    // Sort by instance_id so float summation order is deterministic across runs
    // (HashMap::values() order is seed-dependent).
    let mut instances: Vec<&InstanceResult> = loaded.instances.values().collect();
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    // Reject sweeps that used --rerun/--samples (reruns > 1). load_sweep returns one
    // aggregated InstanceResult per instance: cost and duration are *summed* across run
    // slots while steps and failure_category come from the first slot only. Building
    // distributions from these aggregates mixes per-instance totals with per-run caps
    // (e.g. a two-run instance can exceed a per-task cost cap just from summing) and
    // ignores cap-bound outcomes that appear only in later run slots.
    if instances.iter().any(|inst| inst.runs > 1) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "budget-fit: sweep used --rerun/--samples (instances have runs > 1); \
             aggregated rows sum cost and duration across run slots, which corrupts \
             per-cap percentile analysis; budget-fit requires single-run sweeps"
                .into(),
        )));
    }

    // Reject sweeps with multi-attempt instances (attempts > 1). run_one accumulates
    // cost_usd across all API-retry attempts inside a single run while steps and
    // duration_secs come from the terminal attempt only. Comparing that aggregate
    // cost against a per-instance cap that was defined for a single attempt inflates
    // cost percentiles and misidentifies the cap-bound boundary.
    if instances.iter().any(|inst| inst.attempts > 1) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "budget-fit: one or more instances used multiple attempts (attempts > 1); \
             cost_usd is summed across all API-retry attempts while steps and \
             duration_secs reflect only the terminal attempt, which corrupts \
             cost percentile analysis; re-run without --max-retries or disable \
             within-run retries before using budget-fit"
                .into(),
        )));
    }

    // Reject sweeps halted by --sweep-cost-limit-usd. Budget-halted instances
    // have no steps/cost/duration because they never ran; including them in the
    // instance set inflates n_total and dilutes at-cap shares.
    if instances
        .iter()
        .any(|inst| inst.exit_reason == "budget_halt")
    {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "budget-fit: sweep contains budget-halted instances (exit_reason = budget_halt); \
             tasks that never ran have no steps/cost/duration and would distort per-instance \
             cap percentile analysis; re-run the sweep without a sweep-level cost limit or \
             filter budget-halted rows before running budget-fit"
                .into(),
        )));
    }

    // Behavior enrichment: present but malformed is an error (not a silent fallback).
    // Only the missing-file case proceeds as if behavior enrichment were absent.
    // A behavior.json produced by a fresh `bench behavior --per-instance` run after a
    // retry is the intended way to enrich post-retry analysis, so we accept it even when
    // retry_history is non-empty (we cannot distinguish stale from fresh without mtimes).
    let behavior_map = load_behavior_map(&args.sweep_dir)?;

    // Load evaluation.json when present. The evaluator is the authoritative source
    // for whether a submitted patch actually resolved the issue; use it to override
    // resolved_count from results.json for bucketing and --filter resolved=.
    // Only the missing-file case falls back to results.json; a malformed artifact
    // propagates as an error so bad evaluation data is not silently ignored.
    let mut eval_file_present = false;
    let eval_resolved: HashMap<String, bool> = match load_evaluation_results(&args.sweep_dir)? {
        None => HashMap::new(), // evaluation.json absent — fall back to results.json
        Some(eval) => {
            eval_file_present = true;
            eval.instances
                .iter()
                .map(|row| {
                    (
                        row.instance_id.clone(),
                        row.resolved_count > 0 || row.resolved,
                    )
                })
                .collect()
        }
    };

    // Require evaluation.json to cover every instance when present.
    // Use eval_file_present (not !eval_resolved.is_empty()) so that evaluation.json
    // with instances: [] on a non-empty sweep is also caught as a partial artifact:
    // all instances would fall back to results.json submission state, which is wrong.
    if eval_file_present {
        let missing_count = instances
            .iter()
            .filter(|inst| !eval_resolved.contains_key(&inst.instance_id))
            .count();
        if missing_count > 0 {
            let example = instances
                .iter()
                .find(|inst| !eval_resolved.contains_key(&inst.instance_id))
                .map_or("(unknown)", |inst| inst.instance_id.as_str());
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: evaluation.json is missing {missing_count} instance(s) \
                 (e.g. '{example}'); the artifact may be stale or partial — \
                 re-run bench evaluate or remove evaluation.json to use \
                 results.json submission state"
            ))));
        }
    }

    // Reject stale evaluation.json from before a retry.  The retry path preserves
    // evaluation.json while rewriting results.json, so retried instances can be bucketed
    // with their pre-retry resolved state.  Require a fresh bench evaluate.
    if eval_file_present && has_retry_history(&args.sweep_dir) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "budget-fit: evaluation.json may be stale — the sweep has retry history; \
             retried instances may have a different resolution state than recorded in \
             evaluation.json. Re-run bench evaluate to refresh, or remove evaluation.json \
             to fall back to results.json submission state."
                .into(),
        )));
    }

    // Extract configured caps from manifest
    let cli_argv = loaded
        .manifest
        .as_ref()
        .map(|m| m.cli.argv.clone())
        .unwrap_or_default();
    // When --step-limit was not passed explicitly, the runner still enforces the
    // default from config (cfg.root.agent.step_limit). build_manifest serializes the
    // effective resolved config to manifest.config.resolved as TOML, so we can
    // read agent.step_limit from there as a fallback.
    let (step_limit, step_limit_source): (Option<f64>, Option<String>) = {
        if let Some(v) = extract_argv_value(&cli_argv, "--step-limit")
            .and_then(|v| v.parse::<u32>().ok())
            .map(f64::from)
        {
            (Some(v), Some("manifest.cli.argv[--step-limit]".to_owned()))
        } else {
            let from_config = loaded.manifest.as_ref().and_then(|m| {
                let tv: toml::Value = m.config.resolved.parse().ok()?;
                tv.get("agent")?
                    .get("step_limit")?
                    .as_integer()
                    .map(|n| n as f64)
            });
            match from_config {
                Some(v) => (
                    Some(v),
                    Some("manifest.config.resolved[agent.step_limit]".to_owned()),
                ),
                None => (None, None),
            }
        }
    };
    let task_timeout_secs = extract_argv_value(&cli_argv, "--task-timeout-secs")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v as f64);
    // Only the per-task budget flag is a per-instance cost cap. The sweep-level
    // cost_limit_usd (a total sweep budget) must not be used here: its scale is
    // completely different from individual task costs, and sweep-budget halts are
    // recorded as not-started rows rather than CostLimit/BudgetExhausted instances,
    // so using it as a per-instance cap would produce nonsense percentile recommendations.
    // Two per-task cost cap config keys exist:
    //   agent.per_task_budget_usd → runner CLI --per-task-budget-usd, records BudgetExhausted
    //   agent.cost_limit_usd      → agent-level per-task cap, records CostLimit
    // Try CLI flag first, then resolved config. When the config has BOTH keys set, the
    // agent can fire either cap on any given instance, so the two failure categories are
    // mixed against a single configured_cap — reject that ambiguous configuration.
    let (per_task_budget_usd, per_task_budget_usd_source): (Option<f64>, Option<String>) = {
        if let Some(v) = extract_argv_value(&cli_argv, "--per-task-budget-usd")
            .and_then(|v| v.parse::<f64>().ok())
        {
            // CLI sets --per-task-budget-usd.  If the resolved config also has
            // agent.cost_limit_usd, DefaultAgent::step checks cost_limit_usd first
            // and can fire CostLimit before BudgetExhausted — same mixed-cap ambiguity as
            // having both in the config.  Reject before any analysis.
            let config_also_has_cost_limit = loaded.manifest.as_ref().and_then(|m| {
                let tv: toml::Value = m.config.resolved.parse().ok()?;
                tv.get("agent")?
                    .get("cost_limit_usd")
                    .and_then(|cv| cv.as_float().or_else(|| cv.as_integer().map(|n| n as f64)))
            });
            if config_also_has_cost_limit.is_some() {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "budget-fit: CLI --per-task-budget-usd is set alongside \
                     agent.cost_limit_usd in the resolved config; the agent can fire \
                     either cap on any instance (cost_limit before budget_exhausted), \
                     so the cost_usd axis cannot be analyzed against a single cap — \
                     remove one of the two cost cap sources before running budget-fit"
                        .into(),
                )));
            }
            (
                Some(v),
                Some("manifest.cli.argv[--per-task-budget-usd]".to_owned()),
            )
        } else if let Some(m) = loaded.manifest.as_ref() {
            if let Ok(tv) = m.config.resolved.parse::<toml::Value>() {
                if let Some(agent) = tv.get("agent") {
                    let ptb = agent
                        .get("per_task_budget_usd")
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)));
                    let cl = agent
                        .get("cost_limit_usd")
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)));
                    match (ptb, cl) {
                        (Some(_), Some(_)) => {
                            return Err(Error::Config(crate::error::ConfigError::Invalid(
                                "budget-fit: resolved config sets both \
                                 agent.per_task_budget_usd and agent.cost_limit_usd; \
                                 the two caps use different failure categories \
                                 (budget_exhausted vs cost_limit) so the cost_usd axis \
                                 cannot be analyzed against a single cap — remove one \
                                 of the two config keys before running budget-fit"
                                    .into(),
                            )));
                        }
                        (Some(v), None) => (
                            Some(v),
                            Some("manifest.config.resolved[agent.per_task_budget_usd]".to_owned()),
                        ),
                        (None, Some(v)) => (
                            Some(v),
                            Some("manifest.config.resolved[agent.cost_limit_usd]".to_owned()),
                        ),
                        (None, None) => (None, None),
                    }
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        }
    };

    // Apply instance filters (same key=value syntax as bench inspect)
    validate_filters(&args.filter)?;
    let instances = apply_filter(instances, &args.filter, &eval_resolved);

    let n_total = instances.len();

    // Build per-axis reports
    let all_axes = [
        build_axis_report(
            AXIS_STEPS,
            "steps",
            step_limit,
            step_limit_source,
            &instances,
            &behavior_map,
            &eval_resolved,
            args.at_cap_tolerance,
            args.target_percentile,
            UNIT_STEPS,
            n_total,
        ),
        build_axis_report(
            AXIS_COST_USD,
            "USD",
            per_task_budget_usd,
            per_task_budget_usd_source,
            &instances,
            &behavior_map,
            &eval_resolved,
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
            &eval_resolved,
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
    let sweep_label = args.sweep_dir.file_name().map_or_else(
        || args.sweep_dir.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );

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
            let _ = writeln!(
                s,
                "  Recommended cap: (none — {}",
                axis.recommended_cap_rationale
            );
            let _ = writeln!(s, "  )");
        }
        let _ = writeln!(
            s,
            "  At-cap count: {} ({:.1}%)",
            axis.at_cap_count,
            axis.at_cap_share * 100.0
        );
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

/// Return `true` when `results.json` contains a non-empty `retry_history` array.
fn has_retry_history(sweep_dir: &Path) -> bool {
    let path = sweep_dir.join("results.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(val): Result<serde_json::Value, _> = serde_json::from_str(&text) else {
        return false;
    };
    val.get("retry_history")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty())
}

/// Read `sweep_status` from `results.json`.
fn read_sweep_status(sweep_dir: &Path) -> Option<String> {
    let path = sweep_dir.join("results.json");
    let text = std::fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&text).ok()?;
    val.get("sweep_status")?.as_str().map(str::to_owned)
}

/// Return an error if any retry in `retry_history` changed cap parameters or would
/// silently reset originally-configured caps to CLI defaults.
///
/// `retry_swebench_args` rebuilds the retry command from CLI defaults and only
/// applies fields that are explicitly present in `override_delta`.  When the original
/// sweep configured a cap (step_limit / task_timeout_secs / per_task_budget_usd) via
/// argv but the retry omits that field from `override_delta`, the retried rows run at
/// the CLI default rather than the original cap — a silent mixed-cap situation.
///
/// Both directions are value-compared against the original resolved caps so that
/// retries that explicitly preserve the same cap (delta value == original) are
/// accepted, and originals that used the CLI default (step_limit = 50) are not
/// incorrectly flagged when the retry omits the flag.
#[allow(clippy::too_many_lines)]
fn check_retry_cap_overrides(sweep_dir: &Path) -> Result<(), Error> {
    let path = sweep_dir.join("results.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(()); // existence already checked; fail-open here
    };
    let Ok(val): Result<serde_json::Value, _> = serde_json::from_str(&text) else {
        return Ok(());
    };
    let Some(history) = val.get("retry_history").and_then(|v| v.as_array()) else {
        return Ok(());
    };

    let orig_argv: Vec<&str> = val
        .get("manifest")
        .and_then(|m| m.get("cli"))
        .and_then(|c| c.get("argv"))
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    // Extract a numeric value for a flag from orig_argv (both "--flag N" and "--flag=N").
    let argv_f64 = |flag: &str| -> Option<f64> {
        let prefix = format!("{flag}=");
        for i in 0..orig_argv.len() {
            if orig_argv[i] == flag {
                return orig_argv.get(i + 1).and_then(|v| v.parse::<f64>().ok());
            }
            if let Some(v) = orig_argv[i].strip_prefix(prefix.as_str()) {
                return v.parse::<f64>().ok();
            }
        }
        None
    };
    let argv_has = |flag: &str| -> bool {
        let prefix = format!("{flag}=");
        orig_argv.windows(2).any(|w| w[0] == flag)
            || orig_argv.iter().any(|a| a.starts_with(prefix.as_str()))
    };

    // Parse resolved TOML once so cap values not set via argv can still be compared.
    let resolved_toml: Option<toml::Value> = val
        .get("manifest")
        .and_then(|m| m.get("config"))
        .and_then(|c| c.get("resolved"))
        .and_then(|r| r.as_str())
        .and_then(|s| s.parse::<toml::Value>().ok());
    let toml_f64 = |key: &str| -> Option<f64> {
        resolved_toml
            .as_ref()?
            .get("agent")?
            .get(key)
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)))
    };

    // Effective original cap values (argv takes precedence over TOML; step_limit
    // falls back to the CLI default of 50 when absent from both).
    let cli_default_step_limit: f64 = 50.0;
    let orig_step_limit: f64 = argv_f64("--step-limit")
        .or_else(|| toml_f64("step_limit"))
        .unwrap_or(cli_default_step_limit);
    let orig_timeout: Option<f64> =
        argv_f64("--task-timeout-secs").or_else(|| toml_f64("task_timeout_secs"));
    // per_task_budget_usd and cost_limit_usd are mutually exclusive per-instance cost
    // caps (mixed use is already rejected earlier in compute_budget_fit).  Track both
    // here so that a retry that omits the cost cap is caught regardless of which key
    // the original manifest used.
    let orig_budget: Option<f64> =
        argv_f64("--per-task-budget-usd").or_else(|| toml_f64("per_task_budget_usd"));
    let orig_cost_limit: Option<f64> = toml_f64("cost_limit_usd");
    let orig_has_cost_cap = orig_budget.is_some() || orig_cost_limit.is_some();

    // Original model name (from manifest.model.name).
    let orig_model: Option<&str> = val
        .get("manifest")
        .and_then(|m| m.get("model"))
        .and_then(|m| m.get("name"))
        .and_then(|v| v.as_str());

    let orig_has_timeout = argv_has("--task-timeout-secs");

    // If the original sweep used config overlays, any retry may have run without them
    // (retry_swebench_args starts from Config::defaults() when --config is not passed
    // and OverrideDelta does not record config file usage).  We cannot verify the
    // overlays were preserved, so reject any retry history when overlays were present.
    let orig_overlay_paths_nonempty = val
        .get("manifest")
        .and_then(|m| m.get("config"))
        .and_then(|c| c.get("overlay_paths"))
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());
    if orig_overlay_paths_nonempty {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "budget-fit: original sweep used config overlays (manifest.config.overlay_paths \
             is non-empty) but OverrideDelta does not record whether the retry preserved \
             them; retried rows may have run with different prompts or agent settings, \
             making percentile distributions from the merged population unreliable"
                .into(),
        )));
    }

    // If the original sweep ran in a non-default environment (docker), any retry
    // may have run in a different environment because --env/--docker-image overrides
    // are applied by retry_swebench_args but not recorded in OverrideDelta.  A mixed
    // local/docker population yields incomparable wall-clock distributions.
    let orig_env_nondefault = resolved_toml
        .as_ref()
        .and_then(|tv| tv.get("environment"))
        .is_some_and(|env| {
            env.get("kind")
                .and_then(|k| k.as_str())
                .is_some_and(|k| k != "local")
                || env.get("docker_image").is_some()
        });
    if orig_env_nondefault {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "budget-fit: original sweep used a non-default environment (docker) but \
             --env/--docker-image overrides are not recorded in OverrideDelta; retried \
             rows may have run in a different environment, making wall-clock and \
             cap-bound distributions from the merged population unreliable"
                .into(),
        )));
    }
    // Note: the symmetric case (original local, retry in docker) is also undetectable
    // because OverrideDelta does not record --env/--docker-image.  Similarly, dataset
    // overrides (--dataset/--dataset-path), non-cap agent settings
    // (--history-max-input-tokens, --detect-stagnation, --hide-budget-from-agent,
    // --mcp-server), and retry-added --config overlays are not serialised in
    // OverrideDelta and cannot be detected here; these are known schema gaps.

    for entry in history {
        // Reject entries produced by a different harness version.  The
        // --allow-harness-mismatch flag lets bench retry override this check at
        // run time, but budget-fit cannot safely merge rows that may have used
        // different agent-loop accounting or failure categorization.
        if entry
            .get("harness_mismatch")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let retry_id = entry
                .get("retry_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: retry '{retry_id}' has harness_mismatch=true; rows produced \
                 by a different harness version may use different agent-loop accounting or \
                 failure categorization, making merged percentile distributions unreliable"
            ))));
        }

        let Some(delta) = entry.get("override_delta") else {
            continue;
        };
        // Explicit per-instance cap overrides: only flag when the delta value actually
        // differs from what the original instances used.  A retry that explicitly
        // re-states the same cap (e.g. step_limit=30 when original was 30) is fine.
        // sweep_cost_limit_usd is intentionally excluded: it is not a per-instance cap.
        let cap_step_changed = delta
            .get("step_limit")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|v| (v - orig_step_limit).abs() > 0.5);
        let cap_timeout_changed = delta
            .get("task_timeout_secs")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|v| orig_timeout.is_none_or(|orig| (v - orig).abs() > 0.5));
        let cap_budget_changed = delta
            .get("per_task_budget_usd")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|v| orig_budget.is_none_or(|orig| (v - orig).abs() > 1e-9));
        let has_cap_field = cap_step_changed || cap_timeout_changed || cap_budget_changed;
        // A model change affects cost behavior and per-instance success rates; the
        // mixed-model population would yield unreliable mean_cost_per_unit estimates
        // and percentile recommendations derived from a heterogeneous set.
        // Only flag when the recorded model value actually differs from the original.
        let has_model_change = delta
            .get("model")
            .and_then(|v| v.as_str())
            .is_some_and(|m| orig_model.is_none_or(|orig| m != orig));
        // Config overlay paths — bench retry does not currently serialise --config
        // usage into OverrideDelta, so this field is never populated in practice.
        // Keep the check for forward-compatibility: if the schema is extended to
        // record overlay paths, they will be caught here automatically.
        // Note: a retry that ADDS a --config overlay when the original had none is
        // a known undetectable gap (OverrideDelta schema limitation).
        let has_config_overlay = delta
            .get("config_overlay_paths")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| !arr.is_empty());

        // Silent cap reset: the original explicitly configured a cap in argv that
        // differs from the CLI/config default, but the retry omits it from
        // override_delta → retry_swebench_args rebuilds at the CLI default instead.
        //
        // step_limit has a non-None CLI default (50).  When the original used
        // --step-limit 50 (== the default), omitting it from override_delta is
        // harmless: retry_swebench_args also lands on 50 via Config::defaults().
        let silent_step = argv_has("--step-limit")
            && delta.get("step_limit").is_none()
            && (orig_step_limit - cli_default_step_limit).abs() > 0.5;
        // task_timeout_secs defaults to None; any omission when present is a real removal.
        let silent_timeout = orig_has_timeout && delta.get("task_timeout_secs").is_none();
        // per-task cost caps (per_task_budget_usd / cost_limit_usd) can come from argv
        // OR from manifest.config.resolved TOML.  A retry without --per-task-budget-usd
        // in override_delta uses Config::defaults(), which has no cost cap; detect this
        // regardless of whether the original cap was argv- or config-sourced.
        let silent_budget = orig_has_cost_cap && delta.get("per_task_budget_usd").is_none();

        if has_cap_field
            || has_model_change
            || has_config_overlay
            || silent_step
            || silent_timeout
            || silent_budget
        {
            let retry_id = entry
                .get("retry_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: retry '{retry_id}' changed or implicitly reset per-instance \
                 caps (step_limit / task_timeout_secs / per_task_budget_usd / \
                 model / config_overlay_paths); \
                 instances ran with different caps or models so at-cap counts and \
                 cost percentile recommendations would be unreliable. \
                 Re-run bench retry with the same cap flags as the original sweep \
                 to preserve them in override_delta."
            ))));
        }
    }
    Ok(())
}

/// Map of instance_id → dominant action class, loaded from `behavior.json`.
/// Returns `Ok(empty)` when the file is absent; returns `Err` when the file exists
/// but is malformed (so bad enrichment data is never silently ignored).
fn load_behavior_map(sweep_dir: &Path) -> Result<BTreeMap<String, String>, Error> {
    let path = sweep_dir.join("behavior.json");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(&path)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Trajectory(format!("budget-fit: behavior.json is malformed: {e}")))?;
    let Some(per_instance) = val.get("per_instance").and_then(|v| v.as_array()) else {
        return Ok(BTreeMap::new());
    };
    let mut map = BTreeMap::new();
    for inst in per_instance {
        let id = match inst.get("instance_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let Some(class_counts) = inst.get("class_counts").and_then(|v| v.as_object()) else {
            continue;
        };
        // Dominant class = the one with the uniquely highest count.
        // When two classes are tied, leave this instance unclassified (no entry).
        let max_count = class_counts
            .values()
            .map(|v| v.as_u64().unwrap_or(0))
            .max()
            .unwrap_or(0);
        let top: Vec<_> = class_counts
            .iter()
            .filter(|(_, v)| v.as_u64().unwrap_or(0) == max_count)
            .collect();
        if top.len() == 1 {
            map.insert(id, top[0].0.clone());
        }
    }
    Ok(map)
}

/// Keys that budget-fit actively matches; others pass through without filtering.
const KNOWN_FILTER_KEYS: &[&str] = &["resolved", "failure_category"];

/// Validate filter expressions before applying them.
///
/// Returns an error for missing separators, empty values on known keys, or
/// invalid values for well-known keys (e.g. `resolved=yes`).
fn validate_filters(filters: &[String]) -> Result<(), Error> {
    for f in filters {
        let mut it = f.splitn(2, '=');
        let key = it.next().unwrap_or_default().trim();
        let val = it.next().unwrap_or_default().trim();

        if key.is_empty() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: --filter '{f}' has an empty key; expected key=value"
            ))));
        }

        if !KNOWN_FILTER_KEYS.contains(&key) {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: --filter key '{key}' is not recognised; \
                 known keys: {}",
                KNOWN_FILTER_KEYS.join(", ")
            ))));
        }

        if !f.contains('=') {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: --filter '{f}' is missing a value; expected {key}=<value>"
            ))));
        }
        if val.is_empty() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: --filter '{key}=' has an empty value; expected {key}=<value>"
            ))));
        }

        if key == "resolved" && val != "true" && val != "false" {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: --filter resolved=<VALUE> must be `true` or `false`, got `{val}`"
            ))));
        }
    }
    Ok(())
}

/// Apply key=value filters to the instance list.
fn apply_filter<'a>(
    instances: Vec<&'a InstanceResult>,
    filters: &[String],
    eval_resolved: &HashMap<String, bool>,
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
                        // "true"/"false" validated in validate_filters; any other value
                        // is already rejected before we reach here.
                        let expected = val == "true";
                        let is_resolved = eval_resolved
                            .get(&inst.instance_id)
                            .copied()
                            .unwrap_or_else(|| instance_resolved_count(inst) > 0);
                        is_resolved == expected
                    }
                    _ => true, // unknown filter keys pass through (not an error)
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

/// Which `FailureCategory` values mark "at-cap" for a given axis.
///
/// `AXIS_COST_USD` matches both `CostLimit` (sweep-level budget halt propagated to
/// instance) and `BudgetExhausted` (per-task budget exhausted via `--per-task-budget-usd`).
/// Both represent the agent being stopped by a cost cap; treating only `CostLimit`
/// as cap-bound would silently exclude per-task budget exits from the cost axis.
fn cap_failure_categories(axis: &str) -> Vec<FailureCategory> {
    match axis {
        AXIS_STEPS => vec![FailureCategory::StepLimit],
        AXIS_COST_USD => vec![FailureCategory::CostLimit, FailureCategory::BudgetExhausted],
        AXIS_WALL_CLOCK_S => vec![FailureCategory::WallclockTimeout],
        _ => vec![],
    }
}

/// Determine which outcome bucket an instance belongs to for the given axis.
///
/// `is_resolved` should come from evaluation.json when present (authoritative),
/// falling back to `instance_resolved_count(inst) > 0` from results.json.
fn outcome_bucket(
    inst: &InstanceResult,
    cap_cats: &[FailureCategory],
    is_resolved: bool,
) -> &'static str {
    if is_resolved {
        return BUCKET_RESOLVED;
    }
    if let Some(c) = inst.failure_category {
        if cap_cats.contains(&c) {
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
        AXIS_STEPS => inst.steps.map(f64::from),
        AXIS_COST_USD => inst.cost_usd,
        AXIS_WALL_CLOCK_S => inst.duration_secs,
        _ => None,
    }
}

/// Build an `AxisReport` for one axis.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_axis_report(
    axis: &str,
    unit: &str,
    configured_cap: Option<f64>,
    configured_cap_source: Option<String>,
    instances: &[&InstanceResult],
    behavior_map: &BTreeMap<String, String>,
    eval_resolved: &HashMap<String, bool>,
    at_cap_tolerance: f64,
    target_percentile: u8,
    round_unit: f64,
    n_total: usize,
) -> AxisReport {
    let cap_cats = cap_failure_categories(axis);

    // Bucket instances and collect values
    let mut resolved_values: Vec<f64> = Vec::new();
    let mut cap_bound_values: Vec<f64> = Vec::new();
    let mut other_values: Vec<f64> = Vec::new();
    let mut errored_values: Vec<f64> = Vec::new();

    // Track cap-bound instances by class (for behavior enrichment)
    let mut cap_bound_progress_count: i64 = 0;
    let mut cap_bound_stuck_count: i64 = 0;
    let mut cap_bound_total_cost: f64 = 0.0;
    let mut cap_bound_progress_cost: f64 = 0.0;
    // Cap-bound instances whose axis value is missing (legacy rows); counted for
    // recommendations and dominance but excluded from the numeric distribution.
    let mut cap_bound_no_value_count: usize = 0;

    for inst in instances {
        let is_resolved = eval_resolved
            .get(&inst.instance_id)
            .copied()
            .unwrap_or_else(|| instance_resolved_count(inst) > 0);
        let bucket = outcome_bucket(inst, &cap_cats, is_resolved);
        if let Some(v) = axis_value(inst, axis) {
            match bucket {
                BUCKET_RESOLVED => resolved_values.push(v),
                BUCKET_UNRESOLVED_CAP_BOUND => {
                    cap_bound_values.push(v);
                    // Behavior enrichment: classify this instance's final action class
                    if let Some(cls) = behavior_map.get(&inst.instance_id) {
                        if PROGRESS_CLASSES.contains(&cls.as_str()) {
                            cap_bound_progress_count += 1;
                            cap_bound_progress_cost += inst.cost_usd.unwrap_or(0.0);
                        } else if STUCK_CLASSES.contains(&cls.as_str()) {
                            cap_bound_stuck_count += 1;
                        }
                    }
                    cap_bound_total_cost += inst.cost_usd.unwrap_or(0.0);
                }
                BUCKET_UNRESOLVED_OTHER => other_values.push(v),
                _ => errored_values.push(v),
            }
        } else if bucket == BUCKET_UNRESOLVED_CAP_BOUND {
            // Instance is cap-bound but this axis field is absent (legacy row).
            // Track it in count and cost but it has no value for percentile computation.
            cap_bound_no_value_count += 1;
            cap_bound_total_cost += inst.cost_usd.unwrap_or(0.0);
            if let Some(cls) = behavior_map.get(&inst.instance_id) {
                if PROGRESS_CLASSES.contains(&cls.as_str()) {
                    cap_bound_progress_count += 1;
                    cap_bound_progress_cost += inst.cost_usd.unwrap_or(0.0);
                } else if STUCK_CLASSES.contains(&cls.as_str()) {
                    cap_bound_stuck_count += 1;
                }
            }
        }
    }

    // Distribution stats per bucket
    let mut distribution_by_outcome: BTreeMap<String, DistributionStats> = BTreeMap::new();
    distribution_by_outcome.insert(
        BUCKET_RESOLVED.into(),
        compute_distribution(&resolved_values),
    );
    {
        let mut cap_bound_stats = compute_distribution(&cap_bound_values);
        // Include legacy rows that have the failure_category but no numeric axis value.
        cap_bound_stats.count += cap_bound_no_value_count;
        distribution_by_outcome.insert(BUCKET_UNRESOLVED_CAP_BOUND.into(), cap_bound_stats);
    }
    distribution_by_outcome.insert(
        BUCKET_UNRESOLVED_OTHER.into(),
        compute_distribution(&other_values),
    );
    distribution_by_outcome.insert(BUCKET_ERRORED.into(), compute_distribution(&errored_values));

    // At-cap computation
    let (at_cap_count, at_cap_share) =
        compute_at_cap(instances, axis, configured_cap, at_cap_tolerance, n_total);

    // Total cap-bound count includes instances without axis values (legacy rows).
    let cap_bound_total_count = cap_bound_values.len() + cap_bound_no_value_count;

    // Recommendation logic
    let (recommended_cap, recommended_cap_rationale, projected_impact_if_recommended) =
        make_recommendation(
            axis,
            configured_cap,
            &resolved_values,
            cap_bound_total_count,
            cap_bound_progress_count,
            cap_bound_stuck_count,
            cap_bound_total_cost,
            behavior_map.is_empty(),
            target_percentile,
            round_unit,
            instances,
        );

    // projected_impact_if_tightened_to_p95 (always uses p95, not target_percentile)
    let projected_impact_if_tightened_to_p95 = configured_cap.and_then(|cap| {
        if resolved_values.is_empty() {
            return None;
        }
        let p95 = percentile_of_sorted(
            &{
                let mut v = resolved_values.clone();
                v.sort_by(|a, b| f64_cmp(*a, *b));
                v
            },
            95.0,
        )?;
        let tighten_cap = round_up(p95, round_unit);
        if tighten_cap >= cap {
            return None; // no tightening needed
        }
        let lost_resolved =
            i64::try_from(resolved_values.iter().filter(|&&v| v > tighten_cap).count())
                .unwrap_or(i64::MAX);
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
        // Exclude progress-class cost from waste when behavior data is present.
        // Progress-class cap-bound instances may resolve if the cap is raised, so
        // their spend is investment rather than waste; only stuck/unclassified spend
        // is conservative waste.  When behavior.json is absent we include all costs.
        cap_bound_cost_usd: if behavior_map.is_empty() {
            cap_bound_total_cost
        } else {
            cap_bound_total_cost - cap_bound_progress_cost
        },
    }
}

/// Compute recommended cap, rationale, and projected impact.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn make_recommendation(
    axis: &str,
    configured_cap: Option<f64>,
    resolved_values: &[f64],
    cap_bound_count: usize,
    cap_bound_progress_count: i64,
    cap_bound_stuck_count: i64,
    _cap_bound_total_cost: f64,
    behavior_absent: bool,
    target_percentile: u8,
    round_unit: f64,
    instances: &[&InstanceResult],
) -> (Option<f64>, String, Option<ProjectedImpact>) {
    // No cap configured for this axis
    let Some(cap) = configured_cap else {
        return (None, "no cap configured for this axis".to_owned(), None);
    };

    // P{target} of resolved
    let resolved_p_target = {
        let mut sorted = resolved_values.to_vec();
        sorted.sort_by(|a, b| f64_cmp(*a, *b));
        percentile_of_sorted(&sorted, f64::from(target_percentile)).map(|v| round_up(v, round_unit))
    };

    // --- Behavior-enriched recommendation ---
    // When behavior.json is present and most cap-bound instances had progress-class actions,
    // recommend raising the cap; otherwise recommend tightening to p_target of resolved.
    let behavior_present = !behavior_absent && cap_bound_count > 0;
    // "Majority" means strictly more than half of all cap-bound instances, including those
    // with unclassified or unrecorded behavior classes (search, git, other, no entry).
    let cap_bound_count_i64 = i64::try_from(cap_bound_count).unwrap_or(i64::MAX);
    let has_progress_class =
        cap_bound_progress_count > 0 && cap_bound_progress_count * 2 > cap_bound_count_i64;
    let has_stuck_class =
        cap_bound_stuck_count > 0 && cap_bound_stuck_count * 2 > cap_bound_count_i64;

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
        let extra_cost = round_to_ndp(
            cost_per_unit * extra_units * cap_bound_progress_count as f64,
            6,
        );
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
        // Do NOT raise — instances are stuck. If there are no resolved instances
        // we cannot derive a percentile-based cap; return None rather than
        // fabricating a recommendation from the current cap.
        let Some(recommended) = resolved_p_target else {
            return (
                None,
                format!(
                    "{cap_bound_stuck_count} cap-bound instance(s) had stuck-class actions \
                     (noop/read/nav); raising the cap is unlikely to help. \
                     Cannot recommend a tighter cap: no resolved instances to base P{target_percentile} on."
                ),
                None,
            );
        };
        // If P{target} is already at or above cap, no useful action — same as the default path.
        // Returning Some(recommended) here would look like a raise, which contradicts the
        // stuck-class conclusion that raising is unlikely to help.
        if recommended >= cap {
            let rationale = format!(
                "P{target_percentile} of resolved ({recommended:.4}) ≥ configured cap ({cap:.4}); \
                 cap is already well-sized. {cap_bound_stuck_count} stuck-class cap-bound instance(s) detected; \
                 raising is unlikely to help."
            );
            return (None, rationale, None);
        }
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
                 {cap_bound_count} cap-bound failure(s) recorded."
            );
            (None, rationale, None)
        }
        Some(p_target) if p_target >= cap => {
            // P{target} of resolved is at or above the cap — tightening would cut off
            // resolved runs, and the cap is not obviously too generous. Emit no
            // recommendation rather than returning p_target, which would look like a raise.
            let rationale = format!(
                "P{target_percentile} of resolved ({p_target:.4}) ≥ configured cap ({cap:.4}); \
                 cap is already well-sized. {cap_bound_count} cap-bound failure(s)."
            );
            (None, rationale, None)
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
    let lost_resolved =
        i64::try_from(resolved_values.iter().filter(|&&v| v > new_cap).count()).unwrap_or(i64::MAX);
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

/// Estimate cost savings from tightening the cap from `old_cap` to `new_cap`.
///
/// Savings per eligible instance = `(min(actual_value, old_cap) - new_cap)` units × mean
/// cost-per-unit.  Capping at `old_cap` prevents overestimating savings for instances that
/// ran slightly past the cap before the check fired.  Result rounded to 6 decimal places.
fn estimated_cost_savings(
    instances: &[&InstanceResult],
    axis: &str,
    old_cap: f64,
    new_cap: f64,
) -> f64 {
    if new_cap >= old_cap {
        return 0.0;
    }
    let cpu = mean_cost_per_unit(instances, axis);
    let saved_units: f64 = instances
        .iter()
        .filter_map(|inst| {
            let v = axis_value(inst, axis)?;
            if v > new_cap {
                Some(v.min(old_cap) - new_cap)
            } else {
                None
            }
        })
        .sum();
    round_to_ndp(cpu * saved_units, 6)
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
    if total_units > 0.0 {
        total_cost / total_units
    } else {
        0.0
    }
}

/// Compute at_cap_count and at_cap_share.
///
/// Includes rows with axis values at or above the threshold AND legacy rows that
/// have the matching cap failure category but no axis value — those definitively
/// hit the cap and should not be invisible in the at-cap count.
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
    let cap_cats = cap_failure_categories(axis);
    let count = instances
        .iter()
        .filter(|inst| {
            if let Some(v) = axis_value(inst, axis) {
                v >= threshold
            } else {
                // No axis value (legacy row): count as at-cap when the failure
                // category matches this axis's cap — the cap definitely fired.
                inst.failure_category.is_some_and(|c| cap_cats.contains(&c))
            }
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
    // Dominant axis: axis with most cap-bound unresolved instances.
    // When two axes share the maximum count, no single axis dominates.
    let candidates: Vec<(String, usize)> = axes
        .iter()
        .filter_map(|a| {
            let cap_bound = a.distribution_by_outcome.get(BUCKET_UNRESOLVED_CAP_BOUND)?;
            if cap_bound.count > 0 {
                Some((a.axis_name.clone(), cap_bound.count))
            } else {
                None
            }
        })
        .collect();

    let max_count = candidates.iter().map(|(_, c)| *c).max().unwrap_or(0);
    let dominant = if max_count == 0 {
        None
    } else {
        let tied: Vec<_> = candidates.iter().filter(|(_, c)| *c == max_count).collect();
        if tied.len() == 1 {
            Some((tied[0].0.clone(), max_count))
        } else {
            None // tie — no single dominant axis
        }
    };

    let (dominant_axis, dominant_axis_reason) = match dominant {
        Some((name, count)) => (
            Some(name.clone()),
            format!("{count} cap-bound unresolved instance(s) on axis '{name}'"),
        ),
        None if max_count == 0 => (None, "no cap-bound failures detected".to_owned()),
        None => {
            let names: Vec<_> = candidates.iter().map(|(n, _)| n.as_str()).collect();
            (
                None,
                format!("tied cap-bound failures across axes: {}", names.join(", ")),
            )
        }
    };

    let waste_usd = compute_waste_usd(axes);

    let headline = build_headline(axes, dominant_axis.as_ref(), &dominant_axis_reason);

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

fn build_headline(
    axes: &[AxisReport],
    dominant_axis: Option<&String>,
    dominant_axis_reason: &str,
) -> String {
    match dominant_axis {
        None => {
            // Distinguish "truly no cap failures" from "tied cap-bound counts across axes".
            if dominant_axis_reason.starts_with("tied") {
                format!(
                    "Cap-bound failures detected on multiple axes ({dominant_axis_reason}); \
                     review each axis recommendation individually."
                )
            } else {
                "No cap-bound failures detected; consider tightening caps to reduce cost.".into()
            }
        }
        Some(ax) => {
            let axis_report = axes.iter().find(|a| &a.axis_name == ax);
            let rec = axis_report.and_then(|a| a.recommended_cap);
            match rec {
                None => {
                    // Distinguish "no data" from "cap already well-sized": use the per-axis
                    // rationale so operators see "cap is well-sized / P{n} >= cap" rather
                    // than the misleading "insufficient data" message.
                    let rationale = axis_report
                        .map_or("insufficient data for recommendation", |a| {
                            a.recommended_cap_rationale.as_str()
                        });
                    format!("Dominant axis '{ax}': {rationale}")
                }
                Some(v) => format!(
                    "Dominant axis '{ax}': set cap to {v:.4} ({}); {}",
                    axis_report.map_or("units", |a| a.unit.as_str()),
                    axis_report.map_or("", |a| a.recommended_cap_rationale.as_str())
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
    sorted.sort_by(|a, b| f64_cmp(*a, *b));
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
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
    let factor = 10f64.powi(i32::try_from(n).unwrap_or(i32::MAX));
    (v * factor).round() / factor
}

fn f64_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}
