//! `bench compare`: diff two completed sweep runs.
//!
//! Reads a baseline and candidate sweep output directory (each holding a
//! `results.json` written by `run::swebench::run`), joins per-instance
//! results by `instance_id`, and emits a transition matrix + regression
//! list. Optionally exits non-zero when regressions exceed a threshold,
//! enabling CI gating on prompt/harness changes.
//!
//! Read-only over existing artifacts: no model, env, or runtime
//! concurrency. The diff is deterministic given the same inputs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::evaluate::{
    BreakdownAxis, CostAttributionBucket, EvaluationResults, cost_attribution_bucket_label, pct,
    round_dp,
};
use crate::run::swebench::{
    FilterSpec, InstanceResult, ProvenanceManifest, SweepResults, TokenBreakdown, effective_runs,
    resolved_count,
};
use crate::trajectory::{FailureCategory, Trajectory, outcome};

/// Output format for the compare report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareFormat {
    Text,
    Json,
}

#[derive(Debug, Clone)]
pub struct CompareArgs {
    pub baseline: PathBuf,
    pub candidate: PathBuf,
    pub format: CompareFormat,
    /// When `Some(n)`, the binary exits non-zero if regressed-task count
    /// strictly exceeds `n`. `None` is informational only.
    pub max_regressions: Option<usize>,
    pub breakdown: crate::run::evaluate::BreakdownSelection,
    pub min_delta_pp: f64,
    pub cost_attribution: bool,
    pub cost_attribution_min_delta_usd: f64,
}

/// Per-task transition between baseline and candidate. `pass` prefers
/// evaluator output when present, otherwise it uses the sweep row's rerun
/// resolution count and finally the legacy submitted-without-failure proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    PassPass,
    PassFail,
    FailPass,
    FailFail,
    MissingPresent,
    PresentMissing,
}

impl TransitionKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::PassPass => "pass->pass",
            Self::PassFail => "pass->fail",
            Self::FailPass => "fail->pass",
            Self::FailFail => "fail->fail",
            Self::MissingPresent => "missing->present",
            Self::PresentMissing => "present->missing",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskTransition {
    pub instance_id: String,
    pub kind: TransitionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_failure_category: Option<FailureCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_failure_category: Option<FailureCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_exit_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_exit_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompareReport {
    pub baseline_dir: PathBuf,
    pub candidate_dir: PathBuf,
    pub baseline_total: usize,
    pub candidate_total: usize,
    /// Counts of each transition kind. Always contains every variant,
    /// with zero for absent buckets — keeps downstream JSON consumers
    /// from having to special-case missing keys.
    pub transitions: BTreeMap<TransitionKind, usize>,
    pub baseline_runs: u64,
    pub candidate_runs: u64,
    pub baseline_resolved: usize,
    pub candidate_resolved: usize,
    pub resolved_delta: i64,
    pub baseline_resolved_rate: f64,
    pub candidate_resolved_rate: f64,
    pub resolved_delta_rate: f64,
    pub baseline_tests_before_submit_rate: f64,
    pub candidate_tests_before_submit_rate: f64,
    pub tests_before_submit_delta_rate: f64,
    pub resolved_delta_ci95: ConfidenceInterval,
    pub within_noise: bool,
    pub verdict: CompareVerdict,
    pub baseline_total_cost_usd: f64,
    pub candidate_total_cost_usd: f64,
    pub cost_delta_usd: f64,
    pub baseline_total_input_tokens: u64,
    pub candidate_total_input_tokens: u64,
    pub baseline_total_cache_read_tokens: u64,
    pub candidate_total_cache_read_tokens: u64,
    pub baseline_total_cache_creation_tokens: u64,
    pub candidate_total_cache_creation_tokens: u64,
    pub baseline_total_completion_tokens: u64,
    pub candidate_total_completion_tokens: u64,
    pub baseline_cache_hit_rate: f64,
    pub candidate_cache_hit_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_mean_steps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_mean_steps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_steps_delta: Option<f64>,
    pub failure_category_baseline: BTreeMap<FailureCategory, usize>,
    pub failure_category_candidate: BTreeMap<FailureCategory, usize>,
    pub failure_category_delta: BTreeMap<FailureCategory, i64>,
    #[serde(default)]
    pub manifest_deltas: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subset_warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdown_delta: Vec<BreakdownDeltaRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cost_attribution_delta: Vec<CostAttributionDeltaRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cost_attribution_warnings: Vec<String>,
    /// Rate-limit telemetry from the baseline sweep, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_rate_limit_events: Option<crate::run::rate_limit::RateLimitEvents>,
    /// Rate-limit telemetry from the candidate sweep, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_rate_limit_events: Option<crate::run::rate_limit::RateLimitEvents>,
    /// Tasks that passed in the baseline but failed in the candidate.
    /// This is the high-signal artifact for CI gating; sorted by
    /// `instance_id` for stable output.
    pub regressions: Vec<TaskTransition>,
    /// `$/resolved-instance` for the baseline. `None` when baseline_resolved == 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_cost_per_resolved_usd: Option<f64>,
    /// `$/resolved-instance` for the candidate. `None` when candidate_resolved == 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_cost_per_resolved_usd: Option<f64>,
    /// Candidate minus baseline cost_per_resolved_usd. `None` when either is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_resolved_delta_usd: Option<f64>,
    /// Pareto-dominance verdict on the (resolved_rate, cost_per_resolved_usd) plane.
    pub pareto_verdict: ParetoVerdict,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ConfidenceInterval {
    pub lower: f64,
    pub upper: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareVerdict {
    Improvement,
    Regression,
    WithinNoise,
}

/// Whether one config dominates the other on the efficient frontier
/// (resolved_rate, cost_per_resolved_usd).
///
/// `A` dominates `B` when `A` has a higher (or equal) resolved_rate AND a
/// lower (or equal) cost_per_resolved_usd, with at least one strict
/// inequality. Otherwise the configs are non-dominated (a trade-off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParetoVerdict {
    BaselineDominates,
    CandidateDominates,
    NonDominated,
}

#[derive(Debug, Clone, Serialize)]
pub struct BreakdownDeltaRow {
    pub bucket_axis: BreakdownAxis,
    pub bucket_value: String,
    pub baseline_n: usize,
    pub baseline_resolved_rate: f64,
    pub candidate_n: usize,
    pub candidate_resolved_rate: f64,
    pub delta_resolved_rate: f64,
    pub exceeds_threshold: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostAttributionDeltaRow {
    pub bucket: String,
    pub n_baseline: usize,
    pub total_usd_baseline: f64,
    pub n_candidate: usize,
    pub total_usd_candidate: f64,
    pub delta_usd: f64,
    pub share_pp_delta: f64,
    pub exceeds_threshold: bool,
}

impl CompareReport {
    #[must_use]
    pub fn regression_count(&self) -> usize {
        self.regressions.len()
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Compact human-readable table for terminal output.
    #[must_use]
    pub fn human_table(&self) -> String {
        let mut s = String::new();
        s.push_str("\n=== bench compare ===\n");
        write_compare_overview(&mut s, self);
        write_compare_cost_and_token_section(&mut s, self);
        write_rate_limit_events_section(&mut s, self);
        write_mean_steps_line(
            &mut s,
            self.baseline_mean_steps,
            self.candidate_mean_steps,
            self.mean_steps_delta,
        );
        write_transition_matrix(&mut s, &self.transitions);
        write_failure_delta_section(
            &mut s,
            &self.failure_category_baseline,
            &self.failure_category_candidate,
            &self.failure_category_delta,
        );
        write_cost_attribution_delta_section(
            &mut s,
            &self.cost_attribution_warnings,
            &self.cost_attribution_delta,
        );
        let subset_warnings = filtered_subset_warnings(self);
        write_subset_warnings(&mut s, &subset_warnings);
        write_breakdown_delta_section(&mut s, &self.breakdown_delta);
        write_regressions(&mut s, &self.regressions);
        s
    }
}

impl CompareVerdict {
    const fn label(self) -> &'static str {
        match self {
            Self::Improvement => "improvement",
            Self::Regression => "regression",
            Self::WithinNoise => "within_noise",
        }
    }
}

fn write_compare_overview(s: &mut String, report: &CompareReport) {
    let _ = writeln!(s, "Baseline:           {}", report.baseline_dir.display());
    let _ = writeln!(s, "Candidate:          {}", report.candidate_dir.display());
    let _ = writeln!(
        s,
        "Tasks (b/c/union):  {} / {} / {}",
        report.baseline_total,
        report.candidate_total,
        report.transitions.values().sum::<usize>()
    );
    write_manifest_delta_section(s, &report.manifest_deltas);
    let _ = writeln!(
        s,
        "Resolved:           {} -> {} ({:+})",
        report.baseline_resolved, report.candidate_resolved, report.resolved_delta
    );
    let _ = writeln!(
        s,
        "Resolved rate:      {:.2}% -> {:.2}% ({:+.2}pp)",
        report.baseline_resolved_rate * 100.0,
        report.candidate_resolved_rate * 100.0,
        report.resolved_delta_rate * 100.0
    );
    let _ = writeln!(
        s,
        "Tests before submit: {:.2}% -> {:.2}% ({:+.2}pp)",
        report.baseline_tests_before_submit_rate * 100.0,
        report.candidate_tests_before_submit_rate * 100.0,
        report.tests_before_submit_delta_rate * 100.0
    );
    let _ = writeln!(
        s,
        "Delta CI 95%:       [{:+.2}pp, {:+.2}pp]",
        report.resolved_delta_ci95.lower * 100.0,
        report.resolved_delta_ci95.upper * 100.0
    );
    let _ = writeln!(
        s,
        "Within noise:       {}",
        if report.within_noise { "true" } else { "false" }
    );
    let _ = writeln!(s, "Verdict:            {}", report.verdict.label());
}

fn write_compare_cost_and_token_section(s: &mut String, report: &CompareReport) {
    let _ = writeln!(
        s,
        "Total cost USD:     ${:.4} -> ${:.4} ({:+.4})",
        report.baseline_total_cost_usd, report.candidate_total_cost_usd, report.cost_delta_usd
    );
    write_cost_per_resolved_line(
        s,
        report.baseline_cost_per_resolved_usd,
        report.candidate_cost_per_resolved_usd,
        report.cost_per_resolved_delta_usd,
    );
    let _ = writeln!(
        s,
        "Pareto verdict:     {}",
        pareto_verdict_label(report.pareto_verdict)
    );
    write_u64_delta_line(
        s,
        "Input tokens:       ",
        report.baseline_total_input_tokens,
        report.candidate_total_input_tokens,
    );
    write_u64_delta_line(
        s,
        "Cache read tokens:  ",
        report.baseline_total_cache_read_tokens,
        report.candidate_total_cache_read_tokens,
    );
    write_u64_delta_line(
        s,
        "Cache create toks:  ",
        report.baseline_total_cache_creation_tokens,
        report.candidate_total_cache_creation_tokens,
    );
    write_u64_delta_line(
        s,
        "Completion tokens:  ",
        report.baseline_total_completion_tokens,
        report.candidate_total_completion_tokens,
    );
    let _ = writeln!(
        s,
        "Cache hit rate:     {:.2}% -> {:.2}% ({:+.2}pp)",
        report.baseline_cache_hit_rate * 100.0,
        report.candidate_cache_hit_rate * 100.0,
        (report.candidate_cache_hit_rate - report.baseline_cache_hit_rate) * 100.0
    );
}

fn write_cost_per_resolved_line(
    s: &mut String,
    baseline: Option<f64>,
    candidate: Option<f64>,
    delta: Option<f64>,
) {
    let fmt_cpr = |v: Option<f64>| match v {
        Some(x) => format!("${x:.4}"),
        None => "NaN".to_owned(),
    };
    let delta_str = match delta {
        Some(d) => format!("{d:+.4}"),
        None => "NaN".to_owned(),
    };
    let _ = writeln!(
        s,
        "Cost/resolved USD:  {} -> {} ({})",
        fmt_cpr(baseline),
        fmt_cpr(candidate),
        delta_str
    );
}

fn pareto_verdict_label(verdict: ParetoVerdict) -> &'static str {
    match verdict {
        ParetoVerdict::BaselineDominates => "A dominates B",
        ParetoVerdict::CandidateDominates => "B dominates A",
        ParetoVerdict::NonDominated => "non-dominated (tradeoff)",
    }
}

fn write_u64_delta_line(s: &mut String, label: &str, baseline: u64, candidate: u64) {
    let delta = i128::from(candidate) - i128::from(baseline);
    let _ = writeln!(s, "{label}{baseline} -> {candidate} ({delta:+})");
}

fn write_rate_limit_events_section(s: &mut String, report: &CompareReport) {
    let (b, c) = match (
        report.baseline_rate_limit_events.as_ref(),
        report.candidate_rate_limit_events.as_ref(),
    ) {
        (None, None) => return,
        (b, c) => (b, c),
    };
    s.push_str("Rate-limit events:\n");
    let b_calls = b.map_or(0, |e| e.throttled_calls);
    let c_calls = c.map_or(0, |e| e.throttled_calls);
    let _ = writeln!(
        s,
        "  Throttled calls:    {} -> {} ({:+})",
        b_calls,
        c_calls,
        i128::from(c_calls) - i128::from(b_calls)
    );
    let b_secs = b.map_or(0.0, |e| e.total_throttled_seconds);
    let c_secs = c.map_or(0.0, |e| e.total_throttled_seconds);
    let _ = writeln!(
        s,
        "  Throttled secs:     {b_secs:.1} -> {c_secs:.1} ({:+.1})",
        c_secs - b_secs
    );
    let b_peak = b.map_or(0, |e| e.peak_concurrent);
    let c_peak = c.map_or(0, |e| e.peak_concurrent);
    let _ = writeln!(
        s,
        "  Peak concurrent:    {} -> {} ({:+})",
        b_peak,
        c_peak,
        i64::from(c_peak) - i64::from(b_peak)
    );
}

fn write_mean_steps_line(
    s: &mut String,
    baseline: Option<f64>,
    candidate: Option<f64>,
    delta: Option<f64>,
) {
    match (baseline, candidate, delta) {
        (Some(b), Some(c), Some(d)) => {
            let _ = writeln!(s, "Mean steps:         {b:.2} -> {c:.2} ({d:+.2})");
        }
        _ => s.push_str("Mean steps:         n/a\n"),
    }
}

fn filtered_subset_warnings(report: &CompareReport) -> Vec<String> {
    if report.cost_attribution_delta.is_empty() {
        return report.subset_warnings.clone();
    }
    report
        .subset_warnings
        .iter()
        .filter(|warning| !warning.contains("dataset subset differs"))
        .cloned()
        .collect()
}

fn write_manifest_delta_section(s: &mut String, manifest_deltas: &[String]) {
    if manifest_deltas.is_empty() {
        s.push_str("Manifest delta:     none\n");
    } else {
        s.push_str("Manifest delta:\n");
        for d in manifest_deltas {
            let _ = writeln!(s, "  - {d}");
        }
    }
}

fn write_transition_matrix(s: &mut String, transitions: &BTreeMap<TransitionKind, usize>) {
    s.push_str("\nTransition matrix:\n");
    for kind in [
        TransitionKind::PassPass,
        TransitionKind::PassFail,
        TransitionKind::FailPass,
        TransitionKind::FailFail,
        TransitionKind::MissingPresent,
        TransitionKind::PresentMissing,
    ] {
        let n = transitions.get(&kind).copied().unwrap_or(0);
        let _ = writeln!(s, "  {:<18} {n}", kind.label());
    }
}

fn write_failure_delta_section(
    s: &mut String,
    baseline: &BTreeMap<FailureCategory, usize>,
    candidate: &BTreeMap<FailureCategory, usize>,
    delta: &BTreeMap<FailureCategory, i64>,
) {
    let nonzero: Vec<(FailureCategory, i64)> = delta
        .iter()
        .filter(|(_, v)| **v != 0)
        .map(|(k, v)| (*k, *v))
        .collect();
    if nonzero.is_empty() {
        return;
    }
    s.push_str("\nFailure category delta (candidate - baseline):\n");
    for (cat, d) in nonzero {
        let b = baseline.get(&cat).copied().unwrap_or(0);
        let c = candidate.get(&cat).copied().unwrap_or(0);
        let _ = writeln!(s, "  {:<14} {b} -> {c} ({d:+})", failure_label(cat));
    }
}

fn write_subset_warnings(s: &mut String, warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    s.push_str("\nSubset warnings:\n");
    for w in warnings {
        let _ = writeln!(s, "  ! {w}");
    }
}

fn write_breakdown_delta_section(s: &mut String, rows: &[BreakdownDeltaRow]) {
    if rows.is_empty() {
        return;
    }
    s.push_str("\nBreakdown deltas:\n");
    for row in rows {
        let _ = writeln!(
            s,
            "  {} {}={}  n: {} -> {}  resolved_rate: {:.1}% -> {:.1}%  delta={:+.1}pp",
            if row.exceeds_threshold { "*" } else { "-" },
            match row.bucket_axis {
                BreakdownAxis::Repo => "repo",
                BreakdownAxis::FailureCategory => "failure_category",
            },
            row.bucket_value,
            row.baseline_n,
            row.candidate_n,
            row.baseline_resolved_rate * 100.0,
            row.candidate_resolved_rate * 100.0,
            row.delta_resolved_rate * 100.0
        );
    }
}

fn write_cost_attribution_delta_section(
    s: &mut String,
    warnings: &[String],
    rows: &[CostAttributionDeltaRow],
) {
    if rows.is_empty() {
        return;
    }
    s.push_str("\nCost attribution delta:\n");
    for warning in warnings {
        let _ = writeln!(s, "  ! {warning}");
    }
    for row in rows {
        let _ = writeln!(
            s,
            "  {} bucket={}  n: {} -> {}  total_usd: ${:.4} -> ${:.4}  delta_usd={:+.4}  share_pp_delta={:+.2}",
            if row.exceeds_threshold { "*" } else { "-" },
            row.bucket,
            row.n_baseline,
            row.n_candidate,
            row.total_usd_baseline,
            row.total_usd_candidate,
            row.delta_usd,
            row.share_pp_delta
        );
    }
}

fn write_regressions(s: &mut String, regressions: &[TaskTransition]) {
    if regressions.is_empty() {
        s.push_str("\nRegressions:        none\n");
        return;
    }
    let _ = writeln!(s, "\nRegressions ({}):", regressions.len());
    for r in regressions {
        let cat = r.candidate_failure_category.map_or("none", failure_label);
        let exit = r.candidate_exit_reason.as_deref().unwrap_or("?");
        let old = r.baseline_outcome.as_deref().unwrap_or("?");
        let new = r.candidate_outcome.as_deref().unwrap_or("?");
        let _ = writeln!(
            s,
            "  - {id}  {old} -> {new}  category={cat}  exit_reason={exit}",
            id = r.instance_id
        );
    }
}

/// Load all `InstanceResult`s from a sweep output directory.
///
/// Tries `results.json` first (the canonical end-of-sweep summary). Falls
/// back to scanning per-instance `*.traj.json` files when no `results.json`
/// exists, reconstructing minimal `InstanceResult`s. Tolerant of missing
/// newer fields: defaults flow through serde.
pub fn load_run(dir: &Path) -> Result<HashMap<String, InstanceResult>, Error> {
    Ok(load_sweep(dir)?.instances)
}

#[derive(Debug, Clone)]
pub struct LoadedSweep {
    pub instances: HashMap<String, InstanceResult>,
    pub manifest: Option<ProvenanceManifest>,
    pub filter_spec: Option<FilterSpec>,
    pub rate_limit_events: Option<crate::run::rate_limit::RateLimitEvents>,
}

struct DiffContext<'a> {
    manifest_deltas: Vec<String>,
    baseline_model_name: Option<&'a str>,
    candidate_model_name: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedRunSlot {
    pub instance_id: String,
    pub run_index: u32,
    pub result: InstanceResult,
}

pub fn load_sweep(dir: &Path) -> Result<LoadedSweep, Error> {
    let results_path = dir.join("results.json");
    if results_path.exists() {
        let text = std::fs::read_to_string(&results_path)?;
        let filter_spec_present = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("filter_spec").cloned())
            .is_some();
        let sweep: SweepResults = serde_json::from_str(&text)?;
        let rate_limit_events = sweep.rate_limit_events.clone();
        let partial_incomplete = sweep
            .manifest
            .as_ref()
            .is_some_and(|m| m.runtime.finished_at_utc.is_none());
        if partial_incomplete {
            let resume_mode = sweep
                .manifest
                .as_ref()
                .is_some_and(manifest_indicates_resume);
            let min_mtime = if resume_mode {
                None
            } else {
                sweep
                    .manifest
                    .as_ref()
                    .and_then(|m| {
                        chrono::DateTime::parse_from_rfc3339(&m.runtime.started_at_utc).ok()
                    })
                    .map(std::convert::Into::into)
            };
            let scanned = scan_trajectory_instances(dir, min_mtime)?;
            let manifest = sweep.manifest;
            return Ok(LoadedSweep {
                instances: if scanned.is_empty() {
                    sweep
                        .instances
                        .into_iter()
                        .map(|r| (r.instance_id.clone(), r))
                        .collect()
                } else {
                    scanned
                },
                manifest,
                filter_spec: if filter_spec_present {
                    Some(sweep.filter_spec)
                } else {
                    None
                },
                rate_limit_events,
            });
        }
        return Ok(LoadedSweep {
            instances: sweep
                .instances
                .into_iter()
                .map(|r| (r.instance_id.clone(), r))
                .collect(),
            manifest: sweep.manifest,
            filter_spec: if filter_spec_present {
                Some(sweep.filter_spec)
            } else {
                None
            },
            rate_limit_events,
        });
    }
    if !dir.exists() {
        return Err(Error::Trajectory(format!(
            "compare: directory does not exist: {}",
            dir.display()
        )));
    }

    let out = scan_trajectory_instances(dir, None)?;
    Ok(LoadedSweep {
        instances: out,
        manifest: None,
        filter_spec: None,
        rate_limit_events: None,
    })
}

fn manifest_indicates_resume(manifest: &ProvenanceManifest) -> bool {
    manifest.runtime.resume_mode || manifest.cli.argv.iter().any(|arg| arg == "--resume")
}

fn scan_trajectory_instances(
    dir: &Path,
    min_mtime: Option<SystemTime>,
) -> Result<HashMap<String, InstanceResult>, Error> {
    Ok(aggregate_scanned_results(scan_trajectory_run_slots(
        dir, min_mtime,
    )?))
}

fn scan_trajectory_run_slots(
    dir: &Path,
    min_mtime: Option<SystemTime>,
) -> Result<Vec<LoadedRunSlot>, Error> {
    let mut instance_dirs: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut root_trajectories: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let Some(instance_id) = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_owned)
            else {
                continue;
            };
            instance_dirs.push((instance_id, path));
            continue;
        }
        if !path_passes_mtime(&path, min_mtime) {
            continue;
        }
        let Some(name_str) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        let Some(id) = name_str.strip_suffix(".traj.json") else {
            continue;
        };
        root_trajectories.push((id.to_owned(), path));
    }
    instance_dirs.sort_by(|a, b| a.0.cmp(&b.0));
    root_trajectories.sort_by(|a, b| a.0.cmp(&b.0));

    let mut scanned = Vec::new();
    for (instance_id, path) in instance_dirs {
        scan_nested_run_trajectories(&path, &instance_id, min_mtime, &mut scanned)?;
    }
    for (id, path) in root_trajectories {
        if let Some(result) = instance_result_from_trajectory(&id, &path)? {
            scanned.push(LoadedRunSlot {
                instance_id: id,
                run_index: 1,
                result,
            });
        }
    }
    Ok(dedupe_run_slots(scanned))
}

fn scan_nested_run_trajectories(
    instance_dir: &Path,
    instance_id: &str,
    min_mtime: Option<SystemTime>,
    out: &mut Vec<LoadedRunSlot>,
) -> Result<(), Error> {
    let mut run_trajectories: Vec<(u32, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(instance_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || !path_passes_mtime(&path, min_mtime) {
            continue;
        }
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        let Some(run_index) = name
            .strip_prefix("run-")
            .and_then(|s| s.strip_suffix(".traj.json"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        run_trajectories.push((run_index, path));
    }
    run_trajectories.sort_by_key(|(run_index, _)| *run_index);
    for (run_index, path) in run_trajectories {
        if let Some(result) = instance_result_from_trajectory(instance_id, &path)? {
            out.push(LoadedRunSlot {
                instance_id: instance_id.to_owned(),
                run_index,
                result,
            });
        }
    }
    Ok(())
}

fn dedupe_run_slots(scanned: Vec<LoadedRunSlot>) -> Vec<LoadedRunSlot> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::with_capacity(scanned.len());
    for slot in scanned {
        let key = (slot.instance_id.clone(), slot.run_index);
        if seen.insert(key) {
            deduped.push(slot);
        }
    }
    deduped
}

fn path_passes_mtime(path: &Path, min_mtime: Option<SystemTime>) -> bool {
    let Some(min) = min_mtime else {
        return true;
    };
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .is_some_and(|modified| modified >= min)
}

fn instance_result_from_trajectory(
    instance_id: &str,
    path: &Path,
) -> Result<Option<InstanceResult>, Error> {
    let text = std::fs::read_to_string(path)?;
    let traj: Trajectory = match serde_json::from_str(&text) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let info = traj.info;
    let (prompt_tokens, cache_read_tokens, cache_creation_tokens, completion_tokens) = info
        .token_usage
        .as_ref()
        .map_or((None, None, None, None), |t| {
            (
                Some(t.prompt_tokens),
                Some(t.cache_read_tokens),
                Some(t.cache_creation_tokens),
                Some(t.completion_tokens),
            )
        });
    let resolved =
        info.outcome.as_deref() == Some(outcome::SUBMITTED) && info.failure_category.is_none();
    Ok(Some(InstanceResult {
        instance_id: instance_id.to_owned(),
        exit_reason: info.exit_reason.clone().unwrap_or_default(),
        outcome: info.outcome.clone(),
        failure_category: info.failure_category,
        steps: info.steps,
        cost_usd: info.total_cost_usd,
        prompt_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        completion_tokens,
        duration_secs: info.duration_secs,
        error: None,
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: u32::from(resolved),
        pass_at_1: resolved,
        tests_run_before_submit: info.tests_run_before_submit,
        last_tests_passed: info.last_tests_passed,
    }))
}

pub(crate) fn load_run_slots<S: std::hash::BuildHasher>(
    dir: &Path,
    fallback: &HashMap<String, InstanceResult, S>,
) -> Result<Vec<LoadedRunSlot>, Error> {
    let mut slots = scan_trajectory_run_slots(dir, None)?;
    let active_ids: BTreeSet<&str> = fallback.keys().map(String::as_str).collect();
    slots.retain(|slot| active_ids.contains(slot.instance_id.as_str()));
    let seen_ids: BTreeSet<String> = slots.iter().map(|slot| slot.instance_id.clone()).collect();
    if slots.is_empty() {
        slots.extend(fallback.iter().map(|(instance_id, result)| LoadedRunSlot {
            instance_id: instance_id.clone(),
            run_index: 1,
            result: result.clone(),
        }));
    } else {
        slots.extend(
            fallback
                .iter()
                .filter(|(instance_id, _)| !seen_ids.contains(*instance_id))
                .map(|(instance_id, result)| LoadedRunSlot {
                    instance_id: instance_id.clone(),
                    run_index: 1,
                    result: result.clone(),
                }),
        );
    }
    slots.sort_by(|a, b| {
        a.instance_id
            .cmp(&b.instance_id)
            .then_with(|| a.run_index.cmp(&b.run_index))
    });
    Ok(slots)
}

fn aggregate_scanned_results(scanned: Vec<LoadedRunSlot>) -> HashMap<String, InstanceResult> {
    let mut grouped: BTreeMap<String, Vec<(u32, InstanceResult)>> = BTreeMap::new();
    for slot in scanned {
        grouped
            .entry(slot.instance_id)
            .or_default()
            .push((slot.run_index, slot.result));
    }
    let mut out = HashMap::new();
    for (id, mut rows) in grouped {
        rows.sort_by_key(|(run_index, _)| *run_index);
        let Some((_, first)) = rows.first() else {
            continue;
        };
        let mut aggregate = first.clone();
        aggregate.runs = rows
            .iter()
            .map(|(run_index, _)| *run_index)
            .max()
            .unwrap_or(1);
        aggregate.resolved_count = rows
            .iter()
            .filter(|(_, result)| result.resolved_count > 0)
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        aggregate.pass_at_1 = rows
            .iter()
            .find(|(run_index, _)| *run_index == 1)
            .is_some_and(|(_, result)| result.resolved_count > 0);
        aggregate.tests_run_before_submit = rows
            .iter()
            .any(|(_, result)| result.tests_run_before_submit);
        aggregate.last_tests_passed = rows
            .iter()
            .rev()
            .find_map(|(_, result)| result.last_tests_passed);
        aggregate.cost_usd = optional_sum(rows.iter().filter_map(|(_, result)| result.cost_usd));
        aggregate.prompt_tokens = Some(
            rows.iter()
                .filter_map(|(_, result)| result.prompt_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.cache_read_tokens = Some(
            rows.iter()
                .filter_map(|(_, result)| result.cache_read_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.cache_creation_tokens = Some(
            rows.iter()
                .filter_map(|(_, result)| result.cache_creation_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.completion_tokens = Some(
            rows.iter()
                .filter_map(|(_, result)| result.completion_tokens)
                .fold(0u64, u64::saturating_add),
        );
        out.insert(id, aggregate);
    }
    out
}

fn optional_sum(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut seen = false;
    let mut total = 0.0;
    for value in values {
        seen = true;
        total += value;
    }
    seen.then_some(total)
}

/// Compute a `CompareReport` from two on-disk sweep directories.
pub fn compute(args: &CompareArgs) -> Result<CompareReport, Error> {
    let baseline = load_sweep(&args.baseline)?;
    let candidate = load_sweep(&args.candidate)?;
    let baseline_eval = load_evaluation_results(&args.baseline)?;
    let candidate_eval = load_evaluation_results(&args.candidate)?;
    let baseline_model_name = baseline.manifest.as_ref().map(|m| m.model.name.as_str());
    let candidate_model_name = candidate.manifest.as_ref().map(|m| m.model.name.as_str());
    let baseline_resolved_override = baseline_eval.as_ref().map(resolved_overrides_from_eval);
    let candidate_resolved_override = candidate_eval.as_ref().map(resolved_overrides_from_eval);
    let mut report = diff_with_overrides(
        &args.baseline,
        &args.candidate,
        &baseline.instances,
        &candidate.instances,
        baseline_resolved_override.as_ref(),
        candidate_resolved_override.as_ref(),
        DiffContext {
            manifest_deltas: manifest_delta_lines(
                baseline.manifest.as_ref(),
                candidate.manifest.as_ref(),
            ),
            baseline_model_name,
            candidate_model_name,
        },
    );
    report.subset_warnings = subset_warnings(
        baseline.filter_spec.as_ref(),
        candidate.filter_spec.as_ref(),
    );
    report.baseline_rate_limit_events = baseline.rate_limit_events;
    report.candidate_rate_limit_events = candidate.rate_limit_events;
    report.breakdown_delta = build_breakdown_delta(
        &baseline.instances,
        &candidate.instances,
        baseline_resolved_override.as_ref(),
        candidate_resolved_override.as_ref(),
        &args.breakdown.axes,
        args.min_delta_pp,
    );
    if args.cost_attribution {
        let baseline_cost_rows = baseline_eval
            .as_ref()
            .and_then(non_empty_cost_attribution_rows);
        let candidate_cost_rows = candidate_eval
            .as_ref()
            .and_then(non_empty_cost_attribution_rows);
        let baseline_fallback_rows = if baseline_cost_rows.is_none() {
            Some(build_cost_attribution_rows_from_run_slots(
                &load_run_slots(&args.baseline, &baseline.instances)?,
                baseline_model_name,
            ))
        } else {
            None
        };
        let candidate_fallback_rows = if candidate_cost_rows.is_none() {
            Some(build_cost_attribution_rows_from_run_slots(
                &load_run_slots(&args.candidate, &candidate.instances)?,
                candidate_model_name,
            ))
        } else {
            None
        };
        let baseline_rows = if let Some(rows) = baseline_cost_rows {
            rows
        } else {
            baseline_fallback_rows.as_deref().unwrap_or(&[])
        };
        let candidate_rows = if let Some(rows) = candidate_cost_rows {
            rows
        } else {
            candidate_fallback_rows.as_deref().unwrap_or(&[])
        };
        report.cost_attribution_delta = build_cost_attribution_delta_from_rows(
            baseline_rows,
            candidate_rows,
            args.cost_attribution_min_delta_usd,
        );
        report.cost_attribution_warnings = build_cost_attribution_warnings(&report.subset_warnings);
    }
    Ok(report)
}

pub fn write_diff_script(report: &CompareReport, out_path: &Path) -> Result<(), Error> {
    let exe = std::env::current_exe().ok().map_or_else(
        || "rust-swe-agent".into(),
        |path| path.display().to_string(),
    );
    let mut script = String::new();
    script.push_str("#!/usr/bin/env sh\n");
    script.push_str("set -eu\n\n");
    for regression in &report.regressions {
        let baseline = crate::run::trajectory_diff::resolve_trajectory_path(
            &report.baseline_dir,
            &regression.instance_id,
        )
        .ok_or_else(|| {
            Error::Trajectory(format!(
                "compare: baseline trajectory not found for `{}` in {}",
                regression.instance_id,
                report.baseline_dir.display()
            ))
        })?;
        let candidate = crate::run::trajectory_diff::resolve_trajectory_path(
            &report.candidate_dir,
            &regression.instance_id,
        )
        .ok_or_else(|| {
            Error::Trajectory(format!(
                "compare: candidate trajectory not found for `{}` in {}",
                regression.instance_id,
                report.candidate_dir.display()
            ))
        })?;
        let _ = writeln!(
            script,
            "{} bench inspect --diff {} {}",
            sh_quote(&exe),
            sh_quote_path(&baseline),
            sh_quote_path(&candidate)
        );
    }
    std::fs::write(out_path, script)?;
    Ok(())
}

/// Pure diff over two already-loaded id->result maps. Split out so tests
/// can drive it without touching the filesystem.
#[must_use]
pub fn diff<S: std::hash::BuildHasher>(
    baseline_dir: &Path,
    candidate_dir: &Path,
    baseline: &HashMap<String, InstanceResult, S>,
    candidate: &HashMap<String, InstanceResult, S>,
) -> CompareReport {
    diff_with_overrides(
        baseline_dir,
        candidate_dir,
        baseline,
        candidate,
        None,
        None,
        DiffContext {
            manifest_deltas: Vec::new(),
            baseline_model_name: None,
            candidate_model_name: None,
        },
    )
}

#[allow(clippy::too_many_lines)]
fn diff_with_overrides<S: std::hash::BuildHasher>(
    baseline_dir: &Path,
    candidate_dir: &Path,
    baseline: &HashMap<String, InstanceResult, S>,
    candidate: &HashMap<String, InstanceResult, S>,
    baseline_resolved_override: Option<&HashMap<String, ResolutionOverride>>,
    candidate_resolved_override: Option<&HashMap<String, ResolutionOverride>>,
    diff_context: DiffContext<'_>,
) -> CompareReport {
    let transition_summary = build_transition_summary(
        baseline,
        candidate,
        baseline_resolved_override,
        candidate_resolved_override,
    );
    let resolution = resolution_comparison(
        baseline,
        candidate,
        baseline_resolved_override,
        candidate_resolved_override,
    );

    let baseline_total_cost: f64 = baseline
        .values()
        .filter_map(|r| r.effective_cost_usd(diff_context.baseline_model_name))
        .sum();
    let candidate_total_cost: f64 = candidate
        .values()
        .filter_map(|r| r.effective_cost_usd(diff_context.candidate_model_name))
        .sum();
    let baseline_tokens = aggregate_token_breakdown(baseline);
    let candidate_tokens = aggregate_token_breakdown(candidate);

    let baseline_mean_steps = mean_steps(baseline);
    let candidate_mean_steps = mean_steps(candidate);
    let mean_steps_delta = match (baseline_mean_steps, candidate_mean_steps) {
        (Some(b), Some(c)) => Some(c - b),
        _ => None,
    };

    let failure_category_baseline = histogram(baseline);
    let failure_category_candidate = histogram(candidate);
    let failure_category_delta =
        category_delta(&failure_category_baseline, &failure_category_candidate);
    let baseline_tests_before_submit_rate = tests_before_submit_rate(baseline);
    let candidate_tests_before_submit_rate = tests_before_submit_rate(candidate);

    let baseline_cost_per_resolved_usd =
        cost_per_resolved(baseline_total_cost, resolution.baseline_resolved);
    let candidate_cost_per_resolved_usd =
        cost_per_resolved(candidate_total_cost, resolution.candidate_resolved);
    let cost_per_resolved_delta_usd = match (
        baseline_cost_per_resolved_usd,
        candidate_cost_per_resolved_usd,
    ) {
        (Some(b), Some(c)) => Some(c - b),
        _ => None,
    };
    let pareto_verdict = compute_pareto_verdict(
        resolution.baseline_resolved_rate,
        baseline_cost_per_resolved_usd.unwrap_or(f64::NAN),
        resolution.candidate_resolved_rate,
        candidate_cost_per_resolved_usd.unwrap_or(f64::NAN),
    );

    CompareReport {
        baseline_dir: baseline_dir.to_path_buf(),
        candidate_dir: candidate_dir.to_path_buf(),
        baseline_total: baseline.len(),
        candidate_total: candidate.len(),
        transitions: transition_summary.transitions,
        baseline_runs: resolution.baseline_runs,
        candidate_runs: resolution.candidate_runs,
        baseline_resolved: resolution.baseline_resolved,
        candidate_resolved: resolution.candidate_resolved,
        resolved_delta: resolution.resolved_delta,
        baseline_resolved_rate: resolution.baseline_resolved_rate,
        candidate_resolved_rate: resolution.candidate_resolved_rate,
        resolved_delta_rate: resolution.resolved_delta_rate,
        baseline_tests_before_submit_rate,
        candidate_tests_before_submit_rate,
        tests_before_submit_delta_rate: candidate_tests_before_submit_rate
            - baseline_tests_before_submit_rate,
        resolved_delta_ci95: resolution.resolved_delta_ci95,
        within_noise: resolution.within_noise,
        verdict: resolution.verdict,
        baseline_total_cost_usd: baseline_total_cost,
        candidate_total_cost_usd: candidate_total_cost,
        cost_delta_usd: candidate_total_cost - baseline_total_cost,
        baseline_total_input_tokens: baseline_tokens.input_tokens,
        candidate_total_input_tokens: candidate_tokens.input_tokens,
        baseline_total_cache_read_tokens: baseline_tokens.cache_read_tokens,
        candidate_total_cache_read_tokens: candidate_tokens.cache_read_tokens,
        baseline_total_cache_creation_tokens: baseline_tokens.cache_creation_tokens,
        candidate_total_cache_creation_tokens: candidate_tokens.cache_creation_tokens,
        baseline_total_completion_tokens: baseline_tokens.completion_tokens,
        candidate_total_completion_tokens: candidate_tokens.completion_tokens,
        baseline_cache_hit_rate: baseline_tokens.cache_hit_rate(),
        candidate_cache_hit_rate: candidate_tokens.cache_hit_rate(),
        baseline_mean_steps,
        candidate_mean_steps,
        mean_steps_delta,
        failure_category_baseline,
        failure_category_candidate,
        failure_category_delta,
        manifest_deltas: diff_context.manifest_deltas,
        subset_warnings: Vec::new(),
        breakdown_delta: Vec::new(),
        cost_attribution_delta: Vec::new(),
        cost_attribution_warnings: Vec::new(),
        baseline_rate_limit_events: None,
        candidate_rate_limit_events: None,
        regressions: transition_summary.regressions,
        baseline_cost_per_resolved_usd,
        candidate_cost_per_resolved_usd,
        cost_per_resolved_delta_usd,
        pareto_verdict,
    }
}

fn cost_per_resolved(total_cost: f64, resolved: usize) -> Option<f64> {
    if resolved == 0 {
        None
    } else {
        #[allow(clippy::cast_precision_loss)]
        Some(total_cost / resolved as f64)
    }
}

fn compute_pareto_verdict(
    baseline_resolved_rate: f64,
    baseline_cost_per_resolved: f64,
    candidate_resolved_rate: f64,
    candidate_cost_per_resolved: f64,
) -> ParetoVerdict {
    let b_rate = baseline_resolved_rate;
    let c_rate = candidate_resolved_rate;
    let b_cost = baseline_cost_per_resolved;
    let c_cost = candidate_cost_per_resolved;

    // NaN handling: if either cost is NaN, fall back to resolved-rate-only comparison
    let (b_cost_finite, c_cost_finite) = match (b_cost.is_nan(), c_cost.is_nan()) {
        (false, false) => (b_cost, c_cost),
        (true, false) => return ParetoVerdict::CandidateDominates,
        (false, true) => return ParetoVerdict::BaselineDominates,
        (true, true) => {
            if (c_rate - b_rate).abs() < f64::EPSILON {
                return ParetoVerdict::NonDominated;
            }
            if c_rate > b_rate {
                return ParetoVerdict::CandidateDominates;
            }
            return ParetoVerdict::BaselineDominates;
        }
    };

    let candidate_better_rate = c_rate > b_rate + f64::EPSILON;
    let candidate_better_cost = c_cost_finite < b_cost_finite - f64::EPSILON;
    let candidate_equal_rate = (c_rate - b_rate).abs() <= f64::EPSILON;
    let candidate_equal_cost = (c_cost_finite - b_cost_finite).abs() <= f64::EPSILON;

    let candidate_dominates = (candidate_better_rate || candidate_equal_rate)
        && (candidate_better_cost || candidate_equal_cost)
        && (candidate_better_rate || candidate_better_cost);
    let baseline_better_rate = b_rate > c_rate + f64::EPSILON;
    let baseline_better_cost = b_cost_finite < c_cost_finite - f64::EPSILON;
    let baseline_dominates = (baseline_better_rate || candidate_equal_rate)
        && (baseline_better_cost || candidate_equal_cost)
        && (baseline_better_rate || baseline_better_cost);

    if candidate_dominates {
        ParetoVerdict::CandidateDominates
    } else if baseline_dominates {
        ParetoVerdict::BaselineDominates
    } else {
        ParetoVerdict::NonDominated
    }
}

fn aggregate_token_breakdown<S: std::hash::BuildHasher>(
    rows: &HashMap<String, InstanceResult, S>,
) -> TokenBreakdown {
    rows.values()
        .fold(TokenBreakdown::default(), |mut total, row| {
            let tokens = row.token_breakdown();
            total.input_tokens = total.input_tokens.saturating_add(tokens.input_tokens);
            total.cache_read_tokens = total
                .cache_read_tokens
                .saturating_add(tokens.cache_read_tokens);
            total.cache_creation_tokens = total
                .cache_creation_tokens
                .saturating_add(tokens.cache_creation_tokens);
            total.completion_tokens = total
                .completion_tokens
                .saturating_add(tokens.completion_tokens);
            total
        })
}

fn tests_before_submit_rate<S: std::hash::BuildHasher>(
    rows: &HashMap<String, InstanceResult, S>,
) -> f64 {
    let mut submitted = 0usize;
    let mut with_tests = 0usize;
    for row in rows
        .values()
        .filter(|row| row.outcome.as_deref() == Some(outcome::SUBMITTED))
    {
        submitted += 1;
        if row.tests_run_before_submit {
            with_tests += 1;
        }
    }
    pct(with_tests, submitted)
}

struct TransitionSummary {
    transitions: BTreeMap<TransitionKind, usize>,
    regressions: Vec<TaskTransition>,
}

fn build_transition_summary<S: std::hash::BuildHasher>(
    baseline: &HashMap<String, InstanceResult, S>,
    candidate: &HashMap<String, InstanceResult, S>,
    baseline_resolved_override: Option<&HashMap<String, ResolutionOverride>>,
    candidate_resolved_override: Option<&HashMap<String, ResolutionOverride>>,
) -> TransitionSummary {
    let mut all_ids: BTreeSet<&str> = BTreeSet::new();
    all_ids.extend(baseline.keys().map(String::as_str));
    all_ids.extend(candidate.keys().map(String::as_str));

    let mut transitions = transition_counts();
    let mut regressions = Vec::new();
    for id in &all_ids {
        let b = baseline.get(*id);
        let c = candidate.get(*id);
        let kind = classify(
            id,
            b,
            c,
            baseline_resolved_override,
            candidate_resolved_override,
        );
        *transitions.entry(kind).or_insert(0) += 1;
        if matches!(kind, TransitionKind::PassFail) {
            regressions.push(TaskTransition {
                instance_id: (*id).to_owned(),
                kind,
                baseline_outcome: b.and_then(|r| r.outcome.clone()),
                candidate_outcome: c.and_then(|r| r.outcome.clone()),
                baseline_failure_category: b.and_then(|r| r.failure_category),
                candidate_failure_category: c.and_then(|r| r.failure_category),
                baseline_exit_reason: b.map(|r| r.exit_reason.clone()),
                candidate_exit_reason: c.map(|r| r.exit_reason.clone()),
            });
        }
    }
    TransitionSummary {
        transitions,
        regressions,
    }
}

fn transition_counts() -> BTreeMap<TransitionKind, usize> {
    let mut transitions = BTreeMap::new();
    for kind in [
        TransitionKind::PassPass,
        TransitionKind::PassFail,
        TransitionKind::FailPass,
        TransitionKind::FailFail,
        TransitionKind::MissingPresent,
        TransitionKind::PresentMissing,
    ] {
        transitions.insert(kind, 0);
    }
    transitions
}

#[derive(Debug, Clone, Copy)]
struct ResolutionComparison {
    baseline_runs: u64,
    candidate_runs: u64,
    baseline_resolved: usize,
    candidate_resolved: usize,
    resolved_delta: i64,
    baseline_resolved_rate: f64,
    candidate_resolved_rate: f64,
    resolved_delta_rate: f64,
    resolved_delta_ci95: ConfidenceInterval,
    within_noise: bool,
    verdict: CompareVerdict,
}

fn resolution_comparison<S: std::hash::BuildHasher>(
    baseline: &HashMap<String, InstanceResult, S>,
    candidate: &HashMap<String, InstanceResult, S>,
    baseline_resolved_override: Option<&HashMap<String, ResolutionOverride>>,
    candidate_resolved_override: Option<&HashMap<String, ResolutionOverride>>,
) -> ResolutionComparison {
    let (baseline_runs, baseline_resolved) =
        resolution_totals(baseline, baseline_resolved_override);
    let (candidate_runs, candidate_resolved) =
        resolution_totals(candidate, candidate_resolved_override);
    let baseline_resolved_rate = rate_usize_u64(baseline_resolved, baseline_runs);
    let candidate_resolved_rate = rate_usize_u64(candidate_resolved, candidate_runs);
    let resolved_delta_rate = candidate_resolved_rate - baseline_resolved_rate;
    let resolved_delta_ci95 = wilson_delta_ci95(
        usize_to_u64(candidate_resolved),
        candidate_runs,
        usize_to_u64(baseline_resolved),
        baseline_runs,
    );
    let within_noise = resolved_delta_ci95.lower <= 0.0 && resolved_delta_ci95.upper >= 0.0;
    let verdict = if resolved_delta_ci95.upper < 0.0 {
        CompareVerdict::Regression
    } else if resolved_delta_ci95.lower > 0.0 {
        CompareVerdict::Improvement
    } else {
        CompareVerdict::WithinNoise
    };
    ResolutionComparison {
        baseline_runs,
        candidate_runs,
        baseline_resolved,
        candidate_resolved,
        resolved_delta: i64::try_from(candidate_resolved).unwrap_or(i64::MAX)
            - i64::try_from(baseline_resolved).unwrap_or(i64::MAX),
        baseline_resolved_rate,
        candidate_resolved_rate,
        resolved_delta_rate,
        resolved_delta_ci95,
        within_noise,
        verdict,
    }
}

fn resolution_totals<S: std::hash::BuildHasher>(
    rows: &HashMap<String, InstanceResult, S>,
    resolved_override: Option<&HashMap<String, ResolutionOverride>>,
) -> (u64, usize) {
    let runs = rows
        .iter()
        .map(|(id, r)| u64::from(stats_for(id, r, resolved_override).runs))
        .sum();
    let resolved = rows
        .iter()
        .map(|(id, r)| {
            usize::try_from(stats_for(id, r, resolved_override).resolved_count)
                .unwrap_or(usize::MAX)
        })
        .sum();
    (runs, resolved)
}

fn category_delta(
    baseline: &BTreeMap<FailureCategory, usize>,
    candidate: &BTreeMap<FailureCategory, usize>,
) -> BTreeMap<FailureCategory, i64> {
    let mut delta = BTreeMap::new();
    for cat in baseline.keys().chain(candidate.keys()) {
        let b = i64::try_from(baseline.get(cat).copied().unwrap_or(0)).unwrap_or(i64::MAX);
        let c = i64::try_from(candidate.get(cat).copied().unwrap_or(0)).unwrap_or(i64::MAX);
        delta.insert(*cat, c - b);
    }
    delta
}

fn sh_quote_path(path: &Path) -> String {
    sh_quote(&path.display().to_string())
}

fn sh_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn classify(
    id: &str,
    b: Option<&InstanceResult>,
    c: Option<&InstanceResult>,
    baseline_resolved_override: Option<&HashMap<String, ResolutionOverride>>,
    candidate_resolved_override: Option<&HashMap<String, ResolutionOverride>>,
) -> TransitionKind {
    match (b, c) {
        (None, Some(_)) => TransitionKind::MissingPresent,
        (None | Some(_), None) => TransitionKind::PresentMissing,
        (Some(b), Some(c)) => match (
            stats_for(id, b, baseline_resolved_override).resolved_count > 0,
            stats_for(id, c, candidate_resolved_override).resolved_count > 0,
        ) {
            (true, true) => TransitionKind::PassPass,
            (true, false) => TransitionKind::PassFail,
            (false, true) => TransitionKind::FailPass,
            (false, false) => TransitionKind::FailFail,
        },
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolutionStats {
    runs: u32,
    resolved_count: u32,
}

#[derive(Debug, Clone, Copy)]
struct ResolutionOverride {
    resolved: bool,
    runs: u32,
    resolved_count: u32,
}

fn stats_for(
    id: &str,
    row: &InstanceResult,
    resolved_override: Option<&HashMap<String, ResolutionOverride>>,
) -> ResolutionStats {
    let runs = effective_runs(row);
    if let Some(override_row) = resolved_override.and_then(|m| m.get(id).copied()) {
        let override_runs = if override_row.runs == 0 {
            runs
        } else {
            override_row.runs
        };
        let override_resolved_count = if override_row.runs == 0 && override_row.resolved_count == 0
        {
            if override_row.resolved {
                override_runs
            } else {
                0
            }
        } else {
            override_row.resolved_count.min(override_runs)
        };
        return ResolutionStats {
            runs: override_runs,
            resolved_count: override_resolved_count,
        };
    }
    ResolutionStats {
        runs,
        resolved_count: resolved_count(row),
    }
}

fn rate_usize_u64(numer: usize, denom: u64) -> f64 {
    if denom == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    {
        numer as f64 / denom as f64
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn wilson_delta_ci95(
    candidate_successes: u64,
    candidate_total: u64,
    baseline_successes: u64,
    baseline_total: u64,
) -> ConfidenceInterval {
    let candidate = wilson_ci(candidate_successes, candidate_total);
    let baseline = wilson_ci(baseline_successes, baseline_total);
    ConfidenceInterval {
        lower: candidate.lower - baseline.upper,
        upper: candidate.upper - baseline.lower,
    }
}

fn wilson_ci(successes: u64, total: u64) -> ConfidenceInterval {
    const Z: f64 = 1.959_963_984_540_054;
    if total == 0 {
        return ConfidenceInterval {
            lower: 0.0,
            upper: 0.0,
        };
    }
    #[allow(clippy::cast_precision_loss)]
    let n = total as f64;
    #[allow(clippy::cast_precision_loss)]
    let phat = successes as f64 / n;
    let z2 = Z * Z;
    let denom = 1.0 + z2 / n;
    let center = (phat + z2 / (2.0 * n)) / denom;
    let margin = (Z / denom) * ((phat * (1.0 - phat) / n + z2 / (4.0 * n * n)).sqrt());
    ConfidenceInterval {
        lower: (center - margin).max(0.0),
        upper: (center + margin).min(1.0),
    }
}

pub fn load_evaluation_results(dir: &Path) -> Result<Option<EvaluationResults>, Error> {
    let path = crate::run::evaluate::evaluation_path(dir);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn resolved_overrides_from_eval(eval: &EvaluationResults) -> HashMap<String, ResolutionOverride> {
    eval.instances
        .iter()
        .map(|row| {
            (
                row.instance_id.clone(),
                ResolutionOverride {
                    resolved: row.resolved,
                    runs: row.runs,
                    resolved_count: row.resolved_count,
                },
            )
        })
        .collect()
}

fn manifest_delta_lines(
    baseline: Option<&ProvenanceManifest>,
    candidate: Option<&ProvenanceManifest>,
) -> Vec<String> {
    let (Some(b), Some(c)) = (baseline, candidate) else {
        return vec!["manifest unavailable".into()];
    };
    let mut out = Vec::new();
    if b.harness.git_sha != c.harness.git_sha {
        out.push(format!(
            "harness.git_sha: {:?} -> {:?}",
            b.harness.git_sha, c.harness.git_sha
        ));
    }
    if b.prompt_template.sha256 != c.prompt_template.sha256 {
        out.push("prompt_template.sha256 changed".into());
    }
    if b.dataset.sha256 != c.dataset.sha256 {
        out.push("dataset.sha256 changed".into());
    }
    if b.model.name != c.model.name {
        out.push(format!("model.name: {} -> {}", b.model.name, c.model.name));
    }
    let b_cfg: serde_json::Value = toml::from_str::<toml::Value>(&b.config.resolved)
        .ok()
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_default();
    let c_cfg: serde_json::Value = toml::from_str::<toml::Value>(&c.config.resolved)
        .ok()
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_default();
    let mut changed = Vec::new();
    diff_config_keys("", &b_cfg, &c_cfg, &mut changed);
    if !changed.is_empty() {
        out.push(format!(
            "config.resolved keys changed: {}",
            changed.join(", ")
        ));
    }
    out
}

fn diff_config_keys(
    prefix: &str,
    left_value: &serde_json::Value,
    right_value: &serde_json::Value,
    out: &mut Vec<String>,
) {
    match (left_value, right_value) {
        (serde_json::Value::Object(left_map), serde_json::Value::Object(right_map)) => {
            let keys: BTreeSet<&str> = left_map
                .keys()
                .map(String::as_str)
                .chain(right_map.keys().map(String::as_str))
                .collect();
            for k in keys {
                let path = if prefix.is_empty() {
                    k.to_owned()
                } else {
                    format!("{prefix}.{k}")
                };
                match (left_map.get(k), right_map.get(k)) {
                    (Some(left_child), Some(right_child)) => {
                        diff_config_keys(&path, left_child, right_child, out);
                    }
                    _ => out.push(path),
                }
            }
        }
        _ => {
            if left_value != right_value {
                out.push(prefix.to_owned());
            }
        }
    }
}
fn mean_steps<S: std::hash::BuildHasher>(map: &HashMap<String, InstanceResult, S>) -> Option<f64> {
    let xs: Vec<u32> = map.values().filter_map(|r| r.steps).collect();
    if xs.is_empty() {
        return None;
    }
    let sum: u64 = xs.iter().map(|x| u64::from(*x)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum as f64 / xs.len() as f64;
    Some(mean)
}

fn histogram<S: std::hash::BuildHasher>(
    map: &HashMap<String, InstanceResult, S>,
) -> BTreeMap<FailureCategory, usize> {
    let mut out: BTreeMap<FailureCategory, usize> = BTreeMap::new();
    for r in map.values() {
        if let Some(cat) = r.failure_category {
            *out.entry(cat).or_insert(0) += 1;
        }
    }
    out
}

fn failure_label(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::BudgetExhausted => "budget_exhausted",
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::Unknown => "unknown",
    }
}

fn subset_warnings(baseline: Option<&FilterSpec>, candidate: Option<&FilterSpec>) -> Vec<String> {
    let (Some(b), Some(c)) = (baseline, candidate) else {
        return vec![
            "subset metadata unavailable (missing filter_spec for baseline and/or candidate)"
                .into(),
        ];
    };
    if b.instance_ids.as_deref().map(normalize_instance_ids)
        == c.instance_ids.as_deref().map(normalize_instance_ids)
        && b.sample == c.sample
        && b.seed == c.seed
        && b.limit == c.limit
        && b.stratify_by == c.stratify_by
        && b.stratify_mode == c.stratify_mode
    {
        return Vec::new();
    }
    vec![format!(
        "dataset subset differs (baseline selected_count={}, candidate selected_count={})",
        b.selected_count, c.selected_count
    )]
}

fn normalize_instance_ids(ids: &[String]) -> BTreeSet<&str> {
    ids.iter().map(String::as_str).collect()
}

fn build_breakdown_delta<S: std::hash::BuildHasher>(
    baseline: &HashMap<String, InstanceResult, S>,
    candidate: &HashMap<String, InstanceResult, S>,
    baseline_override: Option<&HashMap<String, ResolutionOverride>>,
    candidate_override: Option<&HashMap<String, ResolutionOverride>>,
    axes: &[BreakdownAxis],
    min_delta_pp: f64,
) -> Vec<BreakdownDeltaRow> {
    let mut out = Vec::new();
    for axis in axes {
        let b = breakdown_map(baseline, baseline_override, *axis);
        let c = breakdown_map(candidate, candidate_override, *axis);
        let mut keys: BTreeSet<String> = BTreeSet::new();
        keys.extend(b.keys().cloned());
        keys.extend(c.keys().cloned());
        for key in keys {
            let (bn, br) = b.get(&key).copied().unwrap_or((0, 0));
            let (cn, cr) = c.get(&key).copied().unwrap_or((0, 0));
            let b_rate = pct(br, bn);
            let c_rate = pct(cr, cn);
            let delta = c_rate - b_rate;
            out.push(BreakdownDeltaRow {
                bucket_axis: *axis,
                bucket_value: key,
                baseline_n: bn,
                baseline_resolved_rate: b_rate,
                candidate_n: cn,
                candidate_resolved_rate: c_rate,
                delta_resolved_rate: delta,
                exceeds_threshold: delta.abs() >= min_delta_pp,
            });
        }
    }
    out.sort_by(|a, b| {
        b.delta_resolved_rate
            .abs()
            .partial_cmp(&a.delta_resolved_rate.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.bucket_axis.cmp(&b.bucket_axis))
            .then_with(|| a.bucket_value.cmp(&b.bucket_value))
    });
    out
}

fn build_cost_attribution_warnings(subset_warnings: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(warning) = subset_warnings
        .iter()
        .find(|warning| warning.contains("dataset subset differs"))
    {
        out.push(format!("totals are not directly comparable: {warning}"));
    }
    out
}

fn breakdown_map<S: std::hash::BuildHasher>(
    items: &HashMap<String, InstanceResult, S>,
    resolved_override: Option<&HashMap<String, ResolutionOverride>>,
    axis: BreakdownAxis,
) -> HashMap<String, (usize, usize)> {
    let mut out = HashMap::new();
    for (id, r) in items {
        let key = match axis {
            BreakdownAxis::Repo => crate::run::evaluate::parse_repo_from_instance_id(id)
                .unwrap_or_else(|| "unknown".to_owned()),
            BreakdownAxis::FailureCategory => {
                if stats_for(id, r, resolved_override).resolved_count > 0 {
                    "resolved".to_owned()
                } else {
                    r.failure_category
                        .map_or("none", crate::run::evaluate::failure_label)
                        .to_owned()
                }
            }
        };
        let entry = out.entry(key).or_insert((0, 0));
        entry.0 += 1;
        if stats_for(id, r, resolved_override).resolved_count > 0 {
            entry.1 += 1;
        }
    }
    out
}

#[cfg(test)]
fn cost_attribution_map<S: std::hash::BuildHasher>(
    items: &HashMap<String, InstanceResult, S>,
    resolved_override: Option<&HashMap<String, ResolutionOverride>>,
    model_name: Option<&str>,
) -> HashMap<String, (usize, f64)> {
    let mut out = HashMap::new();
    for (id, row) in items {
        let resolved = stats_for(id, row, resolved_override).resolved_count > 0;
        let key = cost_attribution_bucket_label(resolved, row.failure_category).to_owned();
        let entry = out.entry(key).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += row.effective_cost_usd(model_name).unwrap_or(0.0);
    }
    out
}

fn non_empty_cost_attribution_rows(eval: &EvaluationResults) -> Option<&[CostAttributionBucket]> {
    (!eval.cost_attribution.is_empty()).then_some(eval.cost_attribution.as_slice())
}

fn cost_attribution_map_from_run_slots(
    slots: &[LoadedRunSlot],
    model_name: Option<&str>,
) -> HashMap<String, (usize, f64)> {
    let mut out = HashMap::new();
    for slot in slots {
        let resolved = slot.result.resolved_count > 0;
        let key = cost_attribution_bucket_label(resolved, slot.result.failure_category).to_owned();
        let entry = out.entry(key).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += slot.result.effective_cost_usd(model_name).unwrap_or(0.0);
    }
    out
}

fn build_cost_attribution_rows_from_run_slots(
    slots: &[LoadedRunSlot],
    model_name: Option<&str>,
) -> Vec<CostAttributionBucket> {
    build_cost_attribution_rows_from_map(cost_attribution_map_from_run_slots(slots, model_name))
}

#[cfg(test)]
fn build_cost_attribution_rows_from_results<S: std::hash::BuildHasher>(
    items: &HashMap<String, InstanceResult, S>,
    resolved_override: Option<&HashMap<String, ResolutionOverride>>,
    model_name: Option<&str>,
) -> Vec<CostAttributionBucket> {
    build_cost_attribution_rows_from_map(cost_attribution_map(items, resolved_override, model_name))
}

fn build_cost_attribution_rows_from_map(
    map: HashMap<String, (usize, f64)>,
) -> Vec<CostAttributionBucket> {
    let total_usd: f64 = map.values().map(|(_, total)| *total).sum();
    let total_n: usize = map.values().map(|(n, _)| *n).sum();

    let mut rows: Vec<CostAttributionBucket> = map
        .into_iter()
        .map(|(bucket, (n, total))| CostAttributionBucket {
            bucket,
            n,
            total_usd: round_dp(total, 4),
            mean_usd: if n == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                {
                    round_dp(total / n as f64, 4)
                }
            },
            share_pct: if total_usd == 0.0 {
                0.0
            } else {
                round_dp(total * 100.0 / total_usd, 2)
            },
        })
        .collect();
    rows.sort_by(|a, b| {
        b.total_usd
            .partial_cmp(&a.total_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.bucket.cmp(&b.bucket))
    });
    rows.push(CostAttributionBucket {
        bucket: crate::run::evaluate::COST_ATTRIBUTION_TOTAL_BUCKET.to_owned(),
        n: total_n,
        total_usd: round_dp(total_usd, 4),
        mean_usd: if total_n == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                round_dp(total_usd / total_n as f64, 4)
            }
        },
        share_pct: if total_n == 0 { 0.0 } else { 100.0 },
    });
    rows
}

fn build_cost_attribution_delta_from_rows(
    baseline_rows: &[CostAttributionBucket],
    candidate_rows: &[CostAttributionBucket],
    min_delta_usd: f64,
) -> Vec<CostAttributionDeltaRow> {
    let (baseline_map, baseline_total) = cost_attribution_rows_to_map(baseline_rows);
    let (candidate_map, candidate_total) = cost_attribution_rows_to_map(candidate_rows);

    let mut keys: BTreeSet<String> = BTreeSet::new();
    keys.extend(baseline_map.keys().cloned());
    keys.extend(candidate_map.keys().cloned());

    let mut out = Vec::new();
    for key in keys {
        let (n_baseline, total_usd_baseline_raw) =
            baseline_map.get(&key).copied().unwrap_or((0, 0.0));
        let (n_candidate, total_usd_candidate_raw) =
            candidate_map.get(&key).copied().unwrap_or((0, 0.0));
        if n_baseline == 0
            && n_candidate == 0
            && total_usd_baseline_raw == 0.0
            && total_usd_candidate_raw == 0.0
        {
            continue;
        }
        let share_baseline = if baseline_total == 0.0 {
            0.0
        } else {
            total_usd_baseline_raw * 100.0 / baseline_total
        };
        let share_candidate = if candidate_total == 0.0 {
            0.0
        } else {
            total_usd_candidate_raw * 100.0 / candidate_total
        };
        let delta_usd_raw = total_usd_candidate_raw - total_usd_baseline_raw;
        out.push(CostAttributionDeltaRow {
            bucket: key,
            n_baseline,
            total_usd_baseline: round_dp(total_usd_baseline_raw, 4),
            n_candidate,
            total_usd_candidate: round_dp(total_usd_candidate_raw, 4),
            delta_usd: round_dp(delta_usd_raw, 4),
            share_pp_delta: round_dp(share_candidate - share_baseline, 2),
            exceeds_threshold: delta_usd_raw.abs() >= min_delta_usd,
        });
    }
    out.sort_by(|a, b| {
        b.delta_usd
            .abs()
            .partial_cmp(&a.delta_usd.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.bucket.cmp(&b.bucket))
    });
    out
}

fn cost_attribution_rows_to_map(
    rows: &[CostAttributionBucket],
) -> (HashMap<String, (usize, f64)>, f64) {
    let mut map = HashMap::new();
    let mut total_usd = None;
    for row in rows {
        if row.bucket == crate::run::evaluate::COST_ATTRIBUTION_TOTAL_BUCKET {
            total_usd = Some(row.total_usd);
            continue;
        }
        map.insert(row.bucket.clone(), (row.n, row.total_usd));
    }
    let total_usd = total_usd.unwrap_or_else(|| map.values().map(|(_, total)| *total).sum());
    (map, total_usd)
}

#[cfg(test)]
fn build_cost_attribution_delta<S: std::hash::BuildHasher>(
    baseline: &HashMap<String, InstanceResult, S>,
    candidate: &HashMap<String, InstanceResult, S>,
    baseline_override: Option<&HashMap<String, ResolutionOverride>>,
    candidate_override: Option<&HashMap<String, ResolutionOverride>>,
    min_delta_usd: f64,
) -> Vec<CostAttributionDeltaRow> {
    let baseline_rows = build_cost_attribution_rows_from_results(baseline, baseline_override, None);
    let candidate_rows =
        build_cost_attribution_rows_from_results(candidate, candidate_override, None);
    build_cost_attribution_delta_from_rows(&baseline_rows, &candidate_rows, min_delta_usd)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn assert_f64_eq(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn submitted(id: &str) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: Some(5),
            cost_usd: Some(0.10),
            prompt_tokens: Some(1000),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(200),
            duration_secs: Some(12.0),
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,
        }
    }

    fn errored(id: &str, cat: FailureCategory) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "error".into(),
            outcome: Some(outcome::ERROR.into()),
            failure_category: Some(cat),
            steps: Some(7),
            cost_usd: Some(0.20),
            prompt_tokens: Some(2000),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(400),
            duration_secs: Some(20.0),
            error: Some("boom".into()),
            github_pr_error: None,
            patch_present: false,
            non_empty_patch: false,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,
        }
    }

    fn legacy_unknown(id: &str) -> InstanceResult {
        // Pre-#15 baseline: ERROR with no failure_category. Per AC,
        // the diff must classify this as a non-pass without panicking.
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "error".into(),
            outcome: Some(outcome::ERROR.into()),
            failure_category: None,
            steps: None,
            cost_usd: None,
            prompt_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            duration_secs: None,
            error: None,
            github_pr_error: None,
            patch_present: false,
            non_empty_patch: false,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,
        }
    }

    fn map_of<I: IntoIterator<Item = InstanceResult>>(it: I) -> HashMap<String, InstanceResult> {
        it.into_iter().map(|r| (r.instance_id.clone(), r)).collect()
    }

    fn rerun_submitted(id: &str, runs: u32, resolved: u32) -> InstanceResult {
        let mut result = submitted(id);
        result.runs = runs;
        result.resolved_count = resolved;
        result.pass_at_1 = resolved > 0;
        result
    }

    fn write_sweep(dir: &Path, instances: Vec<InstanceResult>) {
        let sweep = SweepResults {
            total: instances.len(),
            submitted: instances.len(),
            submitted_with_tests: instances
                .iter()
                .filter(|row| row.tests_run_before_submit)
                .count(),
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: instances.len(),
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances,
            rate_limit_events: None,
        };
        std::fs::write(
            dir.join("results.json"),
            serde_json::to_string_pretty(&sweep).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn classifies_all_six_transitions() {
        // Baseline:  pp(pass), pf(pass), fp(fail), ff(fail), pm(pass)
        // Candidate: pp(pass), pf(fail), fp(pass), ff(fail), mp(pass)
        let baseline = map_of([
            submitted("pp"),
            submitted("pf"),
            errored("fp", FailureCategory::ModelApi),
            errored("ff", FailureCategory::ModelApi),
            submitted("pm"),
        ]);
        let candidate = map_of([
            submitted("pp"),
            errored("pf", FailureCategory::ModelApi),
            submitted("fp"),
            errored("ff", FailureCategory::AgentInternal),
            submitted("mp"),
        ]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.transitions[&TransitionKind::PassPass], 1);
        assert_eq!(r.transitions[&TransitionKind::PassFail], 1);
        assert_eq!(r.transitions[&TransitionKind::FailPass], 1);
        assert_eq!(r.transitions[&TransitionKind::FailFail], 1);
        assert_eq!(r.transitions[&TransitionKind::MissingPresent], 1);
        assert_eq!(r.transitions[&TransitionKind::PresentMissing], 1);
    }

    #[test]
    fn regressions_list_only_pass_fail() {
        let baseline = map_of([
            submitted("a"),
            submitted("b"),
            errored("c", FailureCategory::ModelApi),
        ]);
        let candidate = map_of([
            submitted("a"),
            errored("b", FailureCategory::StepLimit),
            errored("c", FailureCategory::ModelApi),
        ]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.regressions.len(), 1);
        assert_eq!(r.regressions[0].instance_id, "b");
        assert_eq!(
            r.regressions[0].candidate_failure_category,
            Some(FailureCategory::StepLimit)
        );
        assert_eq!(
            r.regressions[0].candidate_exit_reason.as_deref(),
            Some("error")
        );
    }

    #[test]
    fn aggregates_resolved_and_cost_deltas() {
        let baseline = map_of([
            submitted("a"),
            submitted("b"),
            errored("c", FailureCategory::ModelApi),
        ]);
        let candidate = map_of([submitted("a"), submitted("b"), submitted("c")]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.baseline_resolved, 2);
        assert_eq!(r.candidate_resolved, 3);
        assert_eq!(r.resolved_delta, 1);
        // baseline cost = 0.10+0.10+0.20 = 0.40; candidate = 0.30
        assert!((r.baseline_total_cost_usd - 0.40).abs() < 1e-9);
        assert!((r.candidate_total_cost_usd - 0.30).abs() < 1e-9);
        assert!((r.cost_delta_usd - (-0.10)).abs() < 1e-9);
    }

    #[test]
    fn legacy_baseline_without_failure_category_does_not_panic() {
        // Per AC: tolerate trajectories missing newer fields.
        let baseline = map_of([legacy_unknown("a"), submitted("b")]);
        let candidate = map_of([submitted("a"), errored("b", FailureCategory::ModelApi)]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.transitions[&TransitionKind::FailPass], 1);
        assert_eq!(r.transitions[&TransitionKind::PassFail], 1);
    }

    #[test]
    fn missing_and_extra_ids_are_bucketed_not_dropped() {
        let baseline = map_of([submitted("only_b")]);
        let candidate = map_of([submitted("only_c")]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.transitions[&TransitionKind::PresentMissing], 1);
        assert_eq!(r.transitions[&TransitionKind::MissingPresent], 1);
        assert_eq!(r.regressions.len(), 0);
    }

    #[test]
    fn json_round_trip_via_results_json() {
        // End-to-end: write a synthetic results.json on disk, load with
        // `load_run`, compute diff, ensure structure is preserved.
        let dir_b = tempfile::tempdir().unwrap();
        let dir_c = tempfile::tempdir().unwrap();
        let baseline_sweep = SweepResults {
            total: 2,
            submitted: 1,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 1,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 1,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![submitted("a"), errored("b", FailureCategory::ModelApi)],
            rate_limit_events: None,
        };
        let candidate_sweep = SweepResults {
            instances: vec![errored("a", FailureCategory::StepLimit), submitted("b")],
            ..baseline_sweep.clone()
        };
        std::fs::write(
            dir_b.path().join("results.json"),
            serde_json::to_string_pretty(&baseline_sweep).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir_c.path().join("results.json"),
            serde_json::to_string_pretty(&candidate_sweep).unwrap(),
        )
        .unwrap();

        let r = compute(&CompareArgs {
            baseline: dir_b.path().to_path_buf(),
            candidate: dir_c.path().to_path_buf(),
            format: CompareFormat::Json,
            max_regressions: None,
            breakdown: crate::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: true,
            cost_attribution_min_delta_usd: 1.0,
        })
        .unwrap();
        assert_eq!(r.regressions.len(), 1);
        assert_eq!(r.regressions[0].instance_id, "a");
        let json = r.to_json_pretty().unwrap();
        assert!(json.contains("\"pass_fail\""), "got: {json}");
        assert!(json.contains("\"regressions\""), "got: {json}");
    }

    #[test]
    fn human_table_lists_regressions_and_deltas() {
        let baseline = map_of([submitted("a"), submitted("b")]);
        let candidate = map_of([submitted("a"), errored("b", FailureCategory::StepLimit)]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        let t = r.human_table();
        assert!(t.contains("=== bench compare ==="));
        assert!(t.contains("Resolved:           2 -> 1 (-1)"));
        assert!(t.contains("pass->fail"));
        assert!(t.contains("Regressions (1):"), "got:\n{t}");
        assert!(t.contains("- b"), "got:\n{t}");
        assert!(t.contains("category=step_limit"), "got:\n{t}");
    }

    #[test]
    fn human_table_includes_cache_breakdown_and_hit_rate() {
        let baseline = map_of([InstanceResult {
            instance_id: "cached".into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: Some(3),
            cost_usd: Some(0.42),
            prompt_tokens: Some(100),
            cache_read_tokens: Some(800),
            cache_creation_tokens: Some(100),
            completion_tokens: Some(50),
            duration_secs: Some(4.0),
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 1,
            resolved_count: 1,
            pass_at_1: true,
            tests_run_before_submit: false,
            last_tests_passed: None,
        }]);
        let candidate = map_of([InstanceResult {
            instance_id: "cached".into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: Some(3),
            cost_usd: Some(0.90),
            prompt_tokens: Some(900),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(100),
            completion_tokens: Some(50),
            duration_secs: Some(4.0),
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 1,
            resolved_count: 1,
            pass_at_1: true,
            tests_run_before_submit: false,
            last_tests_passed: None,
        }]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        let t = r.human_table();
        assert!(
            t.contains("Input tokens:       100 -> 900 (+800)"),
            "got:\n{t}"
        );
        assert!(
            t.contains("Cache read tokens:  800 -> 0 (-800)"),
            "got:\n{t}"
        );
        assert!(
            t.contains("Cache create toks:  100 -> 100 (+0)"),
            "got:\n{t}"
        );
        assert!(t.contains("Completion tokens:  50 -> 50 (+0)"), "got:\n{t}");
        assert!(
            t.contains("Cache hit rate:     80.00% -> 0.00% (-80.00pp)"),
            "got:\n{t}"
        );
    }

    #[test]
    fn human_table_shows_subset_warnings_without_breakdown_rows() {
        let baseline = map_of([submitted("a")]);
        let candidate = map_of([submitted("a")]);
        let mut r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        r.subset_warnings = vec!["dataset subset differs".into()];
        r.breakdown_delta.clear();
        let t = r.human_table();
        assert!(t.contains("Subset warnings:"), "got:\n{t}");
        assert!(t.contains("dataset subset differs"), "got:\n{t}");
    }

    #[test]
    fn compare_reports_manifest_delta_when_prompt_hash_changes() {
        let mut b = map_of([submitted("a")]);
        let c = b.clone();
        let mut report = diff(Path::new("/b"), Path::new("/c"), &b, &c);
        assert!(report.manifest_deltas.is_empty());
        // sanity: direct helper detects prompt hash-only changes
        let baseline = ProvenanceManifest {
            purpose: None,
            harness: crate::run::swebench::HarnessManifest {
                name: "x".into(),
                version: "1".into(),
                git_sha: Some("a".into()),
                git_dirty: Some(false),
                git_resolution: "ok".into(),
            },
            dataset: crate::run::swebench::DatasetManifest {
                path: "d".into(),
                sha256: "d1".into(),
                instance_count: 1,
                filter_spec: None,
            },
            prompt_template: crate::run::swebench::PromptTemplateManifest {
                source: "builtin".into(),
                path: None,
                sha256: "p1".into(),
            },
            config: crate::run::swebench::ConfigManifest {
                resolved: "{}".into(),
                overlay_paths: Vec::new(),
            },
            model: crate::run::swebench::ModelManifest {
                name: "m".into(),
                backend: "litellm".into(),
                backend_version: None,
                base_url: None,
            },
            runtime: crate::run::swebench::RuntimeManifest {
                started_at_utc: "s".into(),
                finished_at_utc: None,
                host_os: "linux".into(),
                resume_mode: false,
                rust_version: None,
            },
            cli: crate::run::swebench::CliManifest { argv: Vec::new() },
        };
        let mut candidate = baseline.clone();
        candidate.prompt_template.sha256 = "p2".into();
        report.manifest_deltas = manifest_delta_lines(Some(&baseline), Some(&candidate));
        assert!(
            report
                .manifest_deltas
                .iter()
                .any(|d| d.contains("prompt_template.sha256 changed"))
        );
        b.clear();
    }

    #[test]
    fn subset_warning_ignores_instance_id_order() {
        let baseline = crate::run::swebench::FilterSpec {
            original_count: 10,
            selected_count: 2,
            instance_ids: Some(vec!["a".into(), "b".into()]),
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
        };
        let candidate = crate::run::swebench::FilterSpec {
            original_count: 10,
            selected_count: 2,
            instance_ids: Some(vec!["b".into(), "a".into()]),
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
        };
        assert!(subset_warnings(Some(&baseline), Some(&candidate)).is_empty());
    }

    #[test]
    fn subset_warning_when_stratify_settings_differ() {
        let baseline = crate::run::swebench::FilterSpec {
            original_count: 10,
            selected_count: 2,
            instance_ids: None,
            limit: None,
            sample: Some(2),
            seed: Some(7),
            stratify_by: Some(crate::run::swebench::StratifyBy::Repo),
            stratify_mode: Some(crate::run::swebench::StratifyMode::Balanced),
        };
        let candidate = crate::run::swebench::FilterSpec {
            original_count: 10,
            selected_count: 2,
            instance_ids: None,
            limit: None,
            sample: Some(2),
            seed: Some(7),
            stratify_by: None,
            stratify_mode: None,
        };
        let warnings = subset_warnings(Some(&baseline), Some(&candidate));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("dataset subset differs"));
    }

    #[test]
    fn subset_warning_when_filter_spec_missing() {
        let present = crate::run::swebench::FilterSpec {
            original_count: 10,
            selected_count: 2,
            instance_ids: Some(vec!["a".into(), "b".into()]),
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
        };
        let missing_baseline = subset_warnings(None, Some(&present));
        assert!(
            missing_baseline
                .iter()
                .any(|w| w.contains("subset metadata unavailable")),
            "{missing_baseline:?}"
        );
        let missing_candidate = subset_warnings(Some(&present), None);
        assert!(
            missing_candidate
                .iter()
                .any(|w| w.contains("subset metadata unavailable")),
            "{missing_candidate:?}"
        );
    }

    #[test]
    fn failure_category_breakdown_keeps_resolved_separate_from_none() {
        let baseline = map_of([submitted("a")]);
        let candidate = map_of([submitted("a")]);
        let baseline_override = HashMap::from([(
            "a".to_string(),
            ResolutionOverride {
                resolved: true,
                runs: 0,
                resolved_count: 0,
            },
        )]);
        let candidate_override = HashMap::from([(
            "a".to_string(),
            ResolutionOverride {
                resolved: false,
                runs: 0,
                resolved_count: 0,
            },
        )]);

        let rows = build_breakdown_delta(
            &baseline,
            &candidate,
            Some(&baseline_override),
            Some(&candidate_override),
            &[BreakdownAxis::FailureCategory],
            0.0,
        );
        assert!(
            rows.iter()
                .any(|r| r.bucket_value == "resolved" && r.baseline_n == 1 && r.candidate_n == 0)
        );
        assert!(
            rows.iter()
                .any(|r| r.bucket_value == "none" && r.baseline_n == 0 && r.candidate_n == 1)
        );
    }

    #[test]
    fn prefers_evaluation_json_resolved_over_submission_proxy() {
        let dir_b = tempfile::tempdir().unwrap();
        let dir_c = tempfile::tempdir().unwrap();
        let baseline_sweep = SweepResults {
            total: 1,
            submitted: 1,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 1,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![submitted("a")],
            rate_limit_events: None,
        };
        let candidate_sweep = baseline_sweep.clone();
        std::fs::write(
            dir_b.path().join("results.json"),
            serde_json::to_string_pretty(&baseline_sweep).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir_c.path().join("results.json"),
            serde_json::to_string_pretty(&candidate_sweep).unwrap(),
        )
        .unwrap();

        let baseline_eval = crate::run::evaluate::EvaluationResults {
            instances: vec![crate::run::evaluate::InstanceEvaluation {
                instance_id: "a".into(),
                resolved: true,
                runs: 0,
                resolved_count: 0,
                pass_at_1: false,
                tests_passed: vec![],
                tests_failed: vec![],
                eval_exit_reason: crate::run::evaluate::EvalExitReason::Resolved,
                eval_log_path: None,
            }],
            behavioral: crate::run::evaluate::BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
        };
        let candidate_eval = crate::run::evaluate::EvaluationResults {
            instances: vec![crate::run::evaluate::InstanceEvaluation {
                instance_id: "a".into(),
                resolved: false,
                runs: 0,
                resolved_count: 0,
                pass_at_1: false,
                tests_passed: vec![],
                tests_failed: vec![],
                eval_exit_reason: crate::run::evaluate::EvalExitReason::Unresolved,
                eval_log_path: None,
            }],
            behavioral: crate::run::evaluate::BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
        };

        std::fs::write(
            crate::run::evaluate::evaluation_path(dir_b.path()),
            serde_json::to_string_pretty(&baseline_eval).unwrap(),
        )
        .unwrap();
        std::fs::write(
            crate::run::evaluate::evaluation_path(dir_c.path()),
            serde_json::to_string_pretty(&candidate_eval).unwrap(),
        )
        .unwrap();

        let r = compute(&CompareArgs {
            baseline: dir_b.path().to_path_buf(),
            candidate: dir_c.path().to_path_buf(),
            format: CompareFormat::Json,
            max_regressions: None,
            breakdown: crate::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: true,
            cost_attribution_min_delta_usd: 1.0,
        })
        .unwrap();
        assert_eq!(r.baseline_resolved, 1);
        assert_eq!(r.candidate_resolved, 0);
        assert_eq!(r.regressions.len(), 1);
    }

    #[test]
    fn evaluation_json_rerun_counts_feed_compare_ci() {
        let dir_b = tempfile::tempdir().unwrap();
        let dir_c = tempfile::tempdir().unwrap();
        write_sweep(dir_b.path(), vec![rerun_submitted("a", 10, 10)]);
        write_sweep(dir_c.path(), vec![rerun_submitted("a", 10, 10)]);

        let candidate_eval = crate::run::evaluate::EvaluationResults {
            instances: vec![crate::run::evaluate::InstanceEvaluation {
                instance_id: "a".into(),
                resolved: true,
                runs: 10,
                resolved_count: 1,
                pass_at_1: false,
                tests_passed: vec![],
                tests_failed: vec![],
                eval_exit_reason: crate::run::evaluate::EvalExitReason::Resolved,
                eval_log_path: None,
            }],
            behavioral: crate::run::evaluate::BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
        };
        std::fs::write(
            crate::run::evaluate::evaluation_path(dir_c.path()),
            serde_json::to_string_pretty(&candidate_eval).unwrap(),
        )
        .unwrap();

        let r = compute(&CompareArgs {
            baseline: dir_b.path().to_path_buf(),
            candidate: dir_c.path().to_path_buf(),
            format: CompareFormat::Json,
            max_regressions: None,
            breakdown: crate::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: true,
            cost_attribution_min_delta_usd: 1.0,
        })
        .unwrap();
        assert_eq!(r.baseline_resolved, 10);
        assert_eq!(r.candidate_runs, 10);
        assert_eq!(r.candidate_resolved, 1);
    }

    #[test]
    fn cost_attribution_delta_computes_usd_and_share_point_changes() {
        let baseline = map_of([
            submitted("resolved"),
            errored("step", FailureCategory::StepLimit),
        ]);
        let candidate = map_of([
            errored("resolved", FailureCategory::StepLimit),
            errored("api", FailureCategory::ModelApi),
        ]);

        let rows = build_cost_attribution_delta(&baseline, &candidate, None, None, 1.0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].bucket, "model_api");

        let step_limit = rows.iter().find(|row| row.bucket == "step_limit").unwrap();
        assert_eq!(step_limit.n_baseline, 1);
        assert_f64_eq(step_limit.total_usd_baseline, 0.2);
        assert_eq!(step_limit.n_candidate, 1);
        assert_f64_eq(step_limit.total_usd_candidate, 0.2);
        assert_f64_eq(step_limit.delta_usd, 0.0);
        assert_f64_eq(step_limit.share_pp_delta, -16.67);
        assert!(!step_limit.exceeds_threshold);

        let resolved = rows.iter().find(|row| row.bucket == "resolved").unwrap();
        assert_eq!(resolved.n_baseline, 1);
        assert_f64_eq(resolved.total_usd_baseline, 0.1);
        assert_eq!(resolved.n_candidate, 0);
        assert_f64_eq(resolved.total_usd_candidate, 0.0);
        assert_f64_eq(resolved.delta_usd, -0.1);
        assert_f64_eq(resolved.share_pp_delta, -33.33);
        assert!(!resolved.exceeds_threshold);

        let model_api = rows.iter().find(|row| row.bucket == "model_api").unwrap();
        assert_eq!(model_api.n_baseline, 0);
        assert_f64_eq(model_api.total_usd_baseline, 0.0);
        assert_eq!(model_api.n_candidate, 1);
        assert_f64_eq(model_api.total_usd_candidate, 0.2);
        assert_f64_eq(model_api.delta_usd, 0.2);
        assert_f64_eq(model_api.share_pp_delta, 50.0);
        assert!(!model_api.exceeds_threshold);
    }

    #[test]
    fn falls_back_to_trajectory_files_when_no_results_json() {
        // Exercises the load_run fallback path used when an operator
        // points compare at a directory that only has per-task trajectories
        // (e.g. a sweep that crashed before writing results.json).
        use crate::trajectory::{FORMAT_VERSION, TrajectoryInfo};
        let dir = tempfile::tempdir().unwrap();
        let traj = Trajectory {
            trajectory_format: FORMAT_VERSION.into(),
            info: TrajectoryInfo {
                outcome: Some(outcome::SUBMITTED.into()),
                exit_reason: Some("submitted".into()),
                steps: Some(3),
                ..Default::default()
            },
            messages: vec![],
        };
        std::fs::write(
            dir.path().join("inst-1.traj.json"),
            serde_json::to_string_pretty(&traj).unwrap(),
        )
        .unwrap();
        let map = load_run(dir.path()).unwrap();
        assert_eq!(map.len(), 1);
        let r = map.get("inst-1").unwrap();
        assert_eq!(r.outcome.as_deref(), Some(outcome::SUBMITTED));
        assert_eq!(r.steps, Some(3));
    }

    #[test]
    fn incomplete_results_json_falls_back_to_trajectory_scan() {
        use crate::trajectory::{FORMAT_VERSION, TrajectoryInfo};
        let dir = tempfile::tempdir().unwrap();
        let sweep = SweepResults {
            total: 1,
            submitted: 0,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: Some(ProvenanceManifest {
                purpose: None,
                harness: crate::run::swebench::HarnessManifest {
                    name: "h".into(),
                    version: "v".into(),
                    git_sha: None,
                    git_dirty: None,
                    git_resolution: "unavailable".into(),
                },
                dataset: crate::run::swebench::DatasetManifest {
                    path: "d".into(),
                    sha256: "x".into(),
                    instance_count: 1,
                    filter_spec: None,
                },
                prompt_template: crate::run::swebench::PromptTemplateManifest {
                    source: "builtin".into(),
                    path: None,
                    sha256: "p".into(),
                },
                config: crate::run::swebench::ConfigManifest {
                    resolved: "{}".into(),
                    overlay_paths: Vec::new(),
                },
                model: crate::run::swebench::ModelManifest {
                    name: "m".into(),
                    backend: "litellm".into(),
                    backend_version: None,
                    base_url: None,
                },
                runtime: crate::run::swebench::RuntimeManifest {
                    started_at_utc: "s".into(),
                    finished_at_utc: None,
                    host_os: "linux".into(),
                    resume_mode: false,
                    rust_version: None,
                },
                cli: crate::run::swebench::CliManifest { argv: Vec::new() },
            }),
            cost_limit_usd: None,
            instances: Vec::new(),
            rate_limit_events: None,
        };
        std::fs::write(
            dir.path().join("results.json"),
            serde_json::to_string_pretty(&sweep).unwrap(),
        )
        .unwrap();
        let traj = Trajectory {
            trajectory_format: FORMAT_VERSION.into(),
            info: TrajectoryInfo {
                outcome: Some(outcome::SUBMITTED.into()),
                exit_reason: Some("submitted".into()),
                ..Default::default()
            },
            messages: vec![],
        };
        std::fs::write(
            dir.path().join("x.traj.json"),
            serde_json::to_string_pretty(&traj).unwrap(),
        )
        .unwrap();
        let loaded = load_sweep(dir.path()).unwrap();
        assert!(loaded.instances.contains_key("x"));
    }

    #[test]
    fn legacy_results_json_without_filter_spec_preserves_unavailable_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = SweepResults {
            total: 1,
            submitted: 1,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 1,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![submitted("a")],
            rate_limit_events: None,
        };
        let mut value = serde_json::to_value(&sweep).unwrap();
        value.as_object_mut().unwrap().remove("filter_spec");
        std::fs::write(
            dir.path().join("results.json"),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
        let loaded = load_sweep(dir.path()).unwrap();
        assert!(loaded.filter_spec.is_none());
    }

    #[test]
    fn incomplete_results_json_ignores_stale_trajectories_before_started_at() {
        use crate::trajectory::{FORMAT_VERSION, TrajectoryInfo};
        let dir = tempfile::tempdir().unwrap();
        let traj = Trajectory {
            trajectory_format: FORMAT_VERSION.into(),
            info: TrajectoryInfo {
                outcome: Some(outcome::SUBMITTED.into()),
                exit_reason: Some("submitted".into()),
                ..Default::default()
            },
            messages: vec![],
        };
        std::fs::write(
            dir.path().join("old.traj.json"),
            serde_json::to_string_pretty(&traj).unwrap(),
        )
        .unwrap();
        let started = chrono::Utc::now() + chrono::Duration::seconds(10);
        let sweep = SweepResults {
            total: 1,
            submitted: 0,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: Some(ProvenanceManifest {
                purpose: None,
                harness: crate::run::swebench::HarnessManifest {
                    name: "h".into(),
                    version: "v".into(),
                    git_sha: None,
                    git_dirty: None,
                    git_resolution: "unavailable".into(),
                },
                dataset: crate::run::swebench::DatasetManifest {
                    path: "d".into(),
                    sha256: "x".into(),
                    instance_count: 1,
                    filter_spec: None,
                },
                prompt_template: crate::run::swebench::PromptTemplateManifest {
                    source: "builtin".into(),
                    path: None,
                    sha256: "p".into(),
                },
                config: crate::run::swebench::ConfigManifest {
                    resolved: "{}".into(),
                    overlay_paths: Vec::new(),
                },
                model: crate::run::swebench::ModelManifest {
                    name: "m".into(),
                    backend: "litellm".into(),
                    backend_version: None,
                    base_url: None,
                },
                runtime: crate::run::swebench::RuntimeManifest {
                    started_at_utc: started.to_rfc3339(),
                    finished_at_utc: None,
                    host_os: "linux".into(),
                    resume_mode: false,
                    rust_version: None,
                },
                cli: crate::run::swebench::CliManifest { argv: Vec::new() },
            }),
            cost_limit_usd: None,
            instances: Vec::new(),
            rate_limit_events: None,
        };
        std::fs::write(
            dir.path().join("results.json"),
            serde_json::to_string_pretty(&sweep).unwrap(),
        )
        .unwrap();
        let loaded = load_sweep(dir.path()).unwrap();
        assert!(!loaded.instances.contains_key("old"));
    }

    #[test]
    fn incomplete_resume_results_include_preexisting_trajectories() {
        use crate::trajectory::{FORMAT_VERSION, TrajectoryInfo};
        let dir = tempfile::tempdir().unwrap();
        let traj = Trajectory {
            trajectory_format: FORMAT_VERSION.into(),
            info: TrajectoryInfo {
                outcome: Some(outcome::SUBMITTED.into()),
                exit_reason: Some("submitted".into()),
                ..Default::default()
            },
            messages: vec![],
        };
        std::fs::write(
            dir.path().join("resume-old.traj.json"),
            serde_json::to_string_pretty(&traj).unwrap(),
        )
        .unwrap();
        let started = chrono::Utc::now() + chrono::Duration::seconds(10);
        let sweep = SweepResults {
            total: 1,
            submitted: 0,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: Some(ProvenanceManifest {
                purpose: None,
                harness: crate::run::swebench::HarnessManifest {
                    name: "h".into(),
                    version: "v".into(),
                    git_sha: None,
                    git_dirty: None,
                    git_resolution: "unavailable".into(),
                },
                dataset: crate::run::swebench::DatasetManifest {
                    path: "d".into(),
                    sha256: "x".into(),
                    instance_count: 1,
                    filter_spec: None,
                },
                prompt_template: crate::run::swebench::PromptTemplateManifest {
                    source: "builtin".into(),
                    path: None,
                    sha256: "p".into(),
                },
                config: crate::run::swebench::ConfigManifest {
                    resolved: "{}".into(),
                    overlay_paths: Vec::new(),
                },
                model: crate::run::swebench::ModelManifest {
                    name: "m".into(),
                    backend: "litellm".into(),
                    backend_version: None,
                    base_url: None,
                },
                runtime: crate::run::swebench::RuntimeManifest {
                    started_at_utc: started.to_rfc3339(),
                    finished_at_utc: None,
                    host_os: "linux".into(),
                    resume_mode: true,
                    rust_version: None,
                },
                cli: crate::run::swebench::CliManifest {
                    argv: vec!["rust-swe-agent".into()],
                },
            }),
            cost_limit_usd: None,
            instances: Vec::new(),
            rate_limit_events: None,
        };
        std::fs::write(
            dir.path().join("results.json"),
            serde_json::to_string_pretty(&sweep).unwrap(),
        )
        .unwrap();
        let loaded = load_sweep(dir.path()).unwrap();
        assert!(loaded.instances.contains_key("resume-old"));
    }

    // ── Coverage: write_rate_limit_events_section ─────────────────────────────

    fn make_rate_limit_events(
        calls: u64,
        secs: f64,
        peak: u32,
    ) -> crate::run::rate_limit::RateLimitEvents {
        crate::run::rate_limit::RateLimitEvents {
            throttled_calls: calls,
            total_throttled_seconds: secs,
            peak_concurrent: peak,
            configured_max_rpm: Some(4000),
            configured_max_input_tpm: None,
        }
    }

    #[test]
    fn human_table_includes_rate_limit_events_when_both_sides_present() {
        let baseline = map_of([submitted("a")]);
        let candidate = map_of([submitted("a")]);
        let mut r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        r.baseline_rate_limit_events = Some(make_rate_limit_events(5, 2.0, 3));
        r.candidate_rate_limit_events = Some(make_rate_limit_events(10, 4.5, 5));
        let t = r.human_table();
        assert!(
            t.contains("Rate-limit events:"),
            "section header missing: {t}"
        );
        assert!(
            t.contains("Throttled calls:"),
            "throttled calls line missing: {t}"
        );
        assert!(t.contains("5 -> 10 (+5)"), "delta counts wrong: {t}");
        assert!(
            t.contains("Throttled secs:"),
            "throttled secs line missing: {t}"
        );
        assert!(t.contains("2.0 -> 4.5 (+2.5)"), "delta secs wrong: {t}");
        assert!(
            t.contains("Peak concurrent:"),
            "peak concurrent line missing: {t}"
        );
        assert!(t.contains("3 -> 5 (+2)"), "delta peak wrong: {t}");
    }

    #[test]
    fn human_table_rate_limit_section_absent_when_both_sides_none() {
        let baseline = map_of([submitted("a")]);
        let candidate = map_of([submitted("a")]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        let t = r.human_table();
        assert!(
            !t.contains("Rate-limit events:"),
            "section should be absent when neither side has events: {t}"
        );
    }

    #[test]
    fn human_table_rate_limit_section_handles_one_sided_events() {
        // Only candidate has events — baseline shows zero placeholders.
        let baseline = map_of([submitted("a")]);
        let candidate = map_of([submitted("a")]);
        let mut r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        r.candidate_rate_limit_events = Some(make_rate_limit_events(7, 3.0, 4));
        let t = r.human_table();
        assert!(
            t.contains("Rate-limit events:"),
            "section header missing: {t}"
        );
        // baseline placeholder is 0
        assert!(
            t.contains("0 -> 7 (+7)"),
            "one-sided throttled calls wrong: {t}"
        );
    }

    #[test]
    fn load_sweep_propagates_rate_limit_events() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let events = crate::run::rate_limit::RateLimitEvents {
            throttled_calls: 3,
            total_throttled_seconds: 1.5,
            peak_concurrent: 2,
            configured_max_rpm: Some(600),
            configured_max_input_tpm: None,
        };
        let sweep = SweepResults {
            total: 0,
            submitted: 0,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![],
            rate_limit_events: Some(events),
        };
        std::fs::write(
            dir.path().join("results.json"),
            serde_json::to_string_pretty(&sweep).unwrap(),
        )
        .unwrap();
        let loaded = load_sweep(dir.path()).unwrap();
        let ev = loaded.rate_limit_events.unwrap();
        assert_eq!(ev.throttled_calls, 3);
        assert_eq!(ev.configured_max_rpm, Some(600));
    }
}
