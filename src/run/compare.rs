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

use crate::artifact::{ArtifactCompatibility, ArtifactKind, classify_json_value};
use crate::error::Error;
use crate::run::evaluate::{
    BreakdownAxis, CostAttributionBucket, EvaluationResults, cost_attribution_bucket_label, pct,
    round_dp,
};
use crate::run::patch_stats::PatchStats;
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
    pub max_patch_size_regression_pct: Option<f64>,
    pub breakdown: crate::run::evaluate::BreakdownSelection,
    pub min_delta_pp: f64,
    pub cost_attribution: bool,
    pub cost_attribution_min_delta_usd: f64,
    /// Exit non-zero when the resolved-rate delta is positive but p > alpha
    /// (suspected-noise wins). `None` disables this gate.
    pub min_significance: Option<f64>,
    /// Exit non-zero when the resolved-rate delta is negative and p <= alpha
    /// (significant regressions). `None` disables this gate.
    pub regression_significance: Option<f64>,
    /// When true, significance-based gating proceeds even when the paired
    /// sample is underpowered. Without this flag, gating exits non-zero
    /// whenever the test is underpowered.
    pub allow_underpowered: bool,
    /// Optional path to an `eval-flake.json` artifact. When set, instances
    /// flagged `is_flaky=true` are excluded from both the resolved-rate delta
    /// and the McNemar paired significance test.
    pub flake_report: Option<PathBuf>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_mean_lines_changed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_mean_lines_changed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_lines_changed_delta: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_p90_lines_changed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_p90_lines_changed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90_lines_changed_delta: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_mean_files_changed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_mean_files_changed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_files_changed_delta: Option<f64>,
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
    pub artifact_version_mismatches: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_warnings: Vec<String>,
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
    /// Warnings when the baseline and candidate used different model mixes
    /// (fallback chains differ). Empty when both sides used the same model(s).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_mix_warnings: Vec<String>,
    /// Comparability classification based on evaluator provenance.
    /// Always present; classifies as matching, mismatched, or unavailable.
    pub evaluator_provenance_status: EvaluatorProvenanceStatus,
    /// Warnings describing specific evaluator provenance mismatches or missing provenance.
    /// Empty when `evaluator_provenance_status == Matching`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluator_provenance_warnings: Vec<String>,
    /// Paired McNemar significance test on the resolved-rate delta.
    pub resolved_rate_significance: ResolvedRateSignificance,
    /// Sampling drift between the two sweeps. `None` when no trajectory files
    /// were loadable for either sweep. Present (possibly with `steps_drifted=0`)
    /// when at least one instance pair had loadable trajectories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_drift: Option<SamplingDriftSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_test_only_resolved_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_test_only_resolved_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_only_resolved_rate_delta: Option<f64>,
    /// Number of flaky instances excluded from the significance test and delta
    /// computation when `--flake-report` is provided. Zero when no flake report
    /// was loaded or when no flaky instances were found in the paired overlap.
    #[serde(default)]
    pub flaky_instances_excluded: usize,
    /// Comparability metadata block
    pub comparability: ComparabilityBlock,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComparabilityBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_sha256_a: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_sha256_b: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_instance_count_a: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_instance_count_b: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_content_matches: Option<bool>,
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

/// Classification of evaluator provenance comparability between baseline and candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorProvenanceStatus {
    /// Both evaluations have provenance and all scoring-affecting fields match.
    Matching,
    /// Both evaluations have provenance but differ in backend, version, subset, or split.
    Mismatched,
    /// One or both evaluations lack provenance.
    Unavailable,
}

/// Summary of per-step sampling drift between a baseline and candidate sweep.
#[derive(Debug, Clone, Serialize)]
pub struct SamplingDriftSummary {
    /// Total steps across all instance pairs where sampling params differed.
    pub steps_drifted: usize,
    /// Representative example of a drifted step, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<SamplingDriftExample>,
}

/// One example of a sampling mismatch between baseline and candidate.
#[derive(Debug, Clone, Serialize)]
pub struct SamplingDriftExample {
    pub instance_id: String,
    pub baseline_sampling: Option<crate::model::SamplingParams>,
    pub candidate_sampling: Option<crate::model::SamplingParams>,
}

/// Minimum number of discordant pairs required for the paired significance test
/// to be considered powered. Below this threshold `underpowered` is `true`.
pub const UNDERPOWERED_DISCORDANT_THRESHOLD: usize = 10;

/// Paired McNemar significance test result for the resolved-rate delta.
///
/// Computed on the overlap subset (instances present in both sweeps). Instances
/// only in one sweep are counted in `only_in_baseline` / `only_in_candidate`
/// and excluded from the test.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedRateSignificance {
    /// Statistical test applied: always `"mcnemar_exact"`.
    pub test_name: String,
    /// Two-sided exact McNemar p-value. `null` when `paired_n == 0`.
    pub p_value: Option<f64>,
    /// Lower bound of the 95% Wilson-score CI on the rate delta (percentage points).
    pub ci95_lower_pp: f64,
    /// Upper bound of the 95% Wilson-score CI on the rate delta (percentage points).
    pub ci95_upper_pp: f64,
    /// Number of instances present in both sweeps (the paired overlap).
    pub paired_n: usize,
    /// Discordant pairs where baseline passed and candidate failed.
    pub pass_to_fail: usize,
    /// Discordant pairs where baseline failed and candidate passed.
    pub fail_to_pass: usize,
    /// Rate delta within the paired overlap subset: candidate_rate − baseline_rate.
    /// Equals `(fail_to_pass − pass_to_fail) / paired_n`. Zero when `paired_n == 0`.
    /// Use this — not the population `resolved_delta_rate` — for interpreting the
    /// CI and p-value, which are also computed on the paired subset.
    pub paired_delta_rate: f64,
    /// True when the test lacks statistical power (fewer than
    /// `UNDERPOWERED_DISCORDANT_THRESHOLD` discordant pairs, or `paired_n == 0`).
    pub underpowered: bool,
    /// Human-readable reason why the test is underpowered. `null` when powered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underpowered_reason: Option<String>,
    /// Instance IDs present only in the baseline sweep (excluded from the test).
    pub only_in_baseline: usize,
    /// Instance IDs present only in the candidate sweep (excluded from the test).
    pub only_in_candidate: usize,
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

    #[must_use]
    pub fn patch_size_regression_exceeds(&self, max_pct: f64) -> bool {
        let (Some(baseline), Some(candidate)) = (
            self.baseline_mean_lines_changed,
            self.candidate_mean_lines_changed,
        ) else {
            return false;
        };
        if candidate <= baseline {
            return false;
        }
        if baseline <= f64::EPSILON {
            return candidate > 0.0;
        }
        ((candidate - baseline) / baseline) * 100.0 > max_pct
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
        write_patch_stats_delta_lines(&mut s, self);
        write_compare_cost_and_token_section(&mut s, self);
        write_rate_limit_events_section(&mut s, self);
        for w in &self.model_mix_warnings {
            let _ = writeln!(s, "WARNING: {w}");
        }
        write_evaluator_provenance_section(
            &mut s,
            self.evaluator_provenance_status,
            &self.evaluator_provenance_warnings,
        );
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
        let _ = writeln!(
            s,
            "Run 'bench near-miss --sweep {}' to rank failed instances by gold-patch proximity.",
            self.candidate_dir.display()
        );
        write_sampling_drift_section(&mut s, self.sampling_drift.as_ref());
        if self.flaky_instances_excluded > 0 {
            let n = self.flaky_instances_excluded;
            let paired = self.resolved_rate_significance.paired_n;
            let _ = writeln!(
                s,
                "\n{n} flaky instance{s} excluded; {paired} paired instance{ps} used in significance test",
                s = if n == 1 { "" } else { "s" },
                ps = if paired == 1 { "" } else { "s" },
            );
        }
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
    write_artifact_version_section(
        s,
        &report.artifact_version_mismatches,
        &report.artifact_warnings,
    );
    if report.evaluator_provenance_status == EvaluatorProvenanceStatus::Mismatched {
        if let (Some(sha_a), Some(sha_b)) = (
            &report.comparability.dataset_sha256_a,
            &report.comparability.dataset_sha256_b,
        ) {
            if sha_a != sha_b {
                let prefix_a = &sha_a[..12.min(sha_a.len())];
                let prefix_b = &sha_b[..12.min(sha_b.len())];
                let count_a = report
                    .comparability
                    .dataset_instance_count_a
                    .map_or_else(|| "?".to_string(), |c| format!("n={c}"));
                let count_b = report
                    .comparability
                    .dataset_instance_count_b
                    .map_or_else(|| "?".to_string(), |c| format!("n={c}"));
                let _ = writeln!(s, "Dataset mismatch:");
                let _ = writeln!(s, "  Baseline:  {prefix_a} ({count_a})");
                let _ = writeln!(s, "  Candidate: {prefix_b} ({count_b})");
            }
        }
    }
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
    if let (Some(b), Some(c)) = (
        report.baseline_test_only_resolved_rate,
        report.candidate_test_only_resolved_rate,
    ) {
        let delta = c - b;
        let _ = writeln!(
            s,
            "Test-only resolved: {:.1}% -> {:.1}% ({:+.1}pp)",
            b * 100.0,
            c * 100.0,
            delta * 100.0
        );
    }
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
    write_significance_line(s, &report.resolved_rate_significance);
}

fn write_significance_line(s: &mut String, sig: &ResolvedRateSignificance) {
    let underpowered_tag = if sig.underpowered {
        " (underpowered)"
    } else {
        ""
    };
    let sig_str = match sig.p_value {
        Some(p) => format!(
            "resolved-rate \u{394} {:+.2}pp [95% CI: {:+.2}\u{2013}{:+.2} pp], p={:.4} (paired N={}){underpowered_tag}",
            sig.paired_delta_rate * 100.0,
            sig.ci95_lower_pp,
            sig.ci95_upper_pp,
            p,
            sig.paired_n,
        ),
        None => "(underpowered)".to_string(),
    };
    let _ = writeln!(s, "Significance:       {sig_str}");
}

fn write_patch_stats_delta_lines(s: &mut String, report: &CompareReport) {
    write_optional_delta_line(
        s,
        "Mean lines changed",
        report.baseline_mean_lines_changed,
        report.candidate_mean_lines_changed,
        report.mean_lines_changed_delta,
    );
    write_optional_delta_line(
        s,
        "P90 lines changed",
        report.baseline_p90_lines_changed,
        report.candidate_p90_lines_changed,
        report.p90_lines_changed_delta,
    );
    write_optional_delta_line(
        s,
        "Mean files changed",
        report.baseline_mean_files_changed,
        report.candidate_mean_files_changed,
        report.mean_files_changed_delta,
    );
}

fn write_optional_delta_line(
    s: &mut String,
    label: &str,
    baseline: Option<f64>,
    candidate: Option<f64>,
    delta: Option<f64>,
) {
    match (baseline, candidate, delta) {
        (Some(b), Some(c), Some(d)) => {
            let _ = writeln!(s, "{label}: {b:.2} -> {c:.2} ({d:+.2})");
        }
        _ => {
            let _ = writeln!(s, "{label}: n/a");
        }
    }
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

fn write_evaluator_provenance_section(
    s: &mut String,
    status: EvaluatorProvenanceStatus,
    warnings: &[String],
) {
    let label = match status {
        EvaluatorProvenanceStatus::Matching => "matching",
        EvaluatorProvenanceStatus::Mismatched => "mismatched",
        EvaluatorProvenanceStatus::Unavailable => "unavailable",
    };
    let _ = writeln!(s, "Evaluator provenance: {label}");
    for w in warnings {
        let _ = writeln!(s, "  ! {w}");
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
    let both_have_manifests = !manifest_deltas.iter().any(|d| d == "manifest unavailable");
    if manifest_deltas.is_empty() {
        if both_have_manifests {
            s.push_str("Manifest delta:     none (run `bench diff-config` for details)\n");
        } else {
            s.push_str("Manifest delta:     none\n");
        }
    } else {
        if both_have_manifests {
            s.push_str("Manifest delta: (run `bench diff-config` for details)\n");
        } else {
            s.push_str("Manifest delta:\n");
        }
        for d in manifest_deltas {
            let _ = writeln!(s, "  - {d}");
        }
    }
}

fn write_artifact_version_section(s: &mut String, mismatches: &[String], warnings: &[String]) {
    if mismatches.is_empty() && warnings.is_empty() {
        return;
    }
    s.push_str("Artifact versions:\n");
    for mismatch in mismatches {
        let _ = writeln!(s, "  - {mismatch}");
    }
    for warning in warnings {
        let _ = writeln!(s, "  ! {warning}");
    }
}

fn write_transition_matrix(s: &mut String, transitions: &BTreeMap<TransitionKind, usize>) {
    use comfy_table::CellAlignment;
    use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};
    s.push_str(
        "
Transition matrix:
",
    );

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(["Transition", "Count"]);

    for column in table.column_iter_mut() {
        column.set_cell_alignment(CellAlignment::Right);
    }
    if let Some(col) = table.column_mut(0) {
        col.set_cell_alignment(CellAlignment::Left);
    }

    for kind in [
        TransitionKind::PassPass,
        TransitionKind::PassFail,
        TransitionKind::FailPass,
        TransitionKind::FailFail,
        TransitionKind::MissingPresent,
        TransitionKind::PresentMissing,
    ] {
        let n = transitions.get(&kind).copied().unwrap_or(0);
        table.add_row([kind.label().to_string(), n.to_string()]);
    }
    let _ = writeln!(s, "{table}");
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

fn write_sampling_drift_section(s: &mut String, drift: Option<&SamplingDriftSummary>) {
    match drift {
        None
        | Some(SamplingDriftSummary {
            steps_drifted: 0, ..
        }) => {}
        Some(sd) => {
            let _ = writeln!(
                s,
                "\nSampling drift:     {steps} step(s) had different per-call sampling params",
                steps = sd.steps_drifted
            );
            if let Some(ex) = &sd.example {
                let b = ex
                    .baseline_sampling
                    .as_ref()
                    .map_or_else(|| "null".into(), crate::model::SamplingParams::summary_line);
                let c = ex
                    .candidate_sampling
                    .as_ref()
                    .map_or_else(|| "null".into(), crate::model::SamplingParams::summary_line);
                let _ = writeln!(
                    s,
                    "  example ({id}): baseline=[{b}] candidate=[{c}]",
                    id = ex.instance_id
                );
            }
        }
    }
}

// ── Model-mix warning helpers (issue #91) ────────────────────────────────────

/// Snapshot of model-mix data extracted from a `SweepResults` for comparison.
#[derive(Debug, Clone)]
pub struct ModelMixSnapshot {
    /// Count of instances by final responding model name.
    pub model_mix: std::collections::BTreeMap<String, usize>,
    /// Total fallback attempts in the sweep.
    pub total_fallbacks: u64,
}

/// Build human-readable warnings when a `bench compare` pair has mismatched
/// model mixes or different fallback rates. Called before resolved-rate deltas
/// are reported so operators can see the contamination signal first.
///
/// Returns an empty `Vec` when both sides are identical (no fallbacks, same
/// model distribution) — no noise for normal same-model comparisons.
#[must_use]
pub fn build_model_mix_warnings(
    baseline: &ModelMixSnapshot,
    candidate: &ModelMixSnapshot,
) -> Vec<String> {
    let mut warnings = Vec::new();

    let baseline_has_fallback = baseline.total_fallbacks > 0 || baseline.model_mix.len() > 1;
    let candidate_has_fallback = candidate.total_fallbacks > 0 || candidate.model_mix.len() > 1;

    match (baseline_has_fallback, candidate_has_fallback) {
        (false, true) => warnings.push(
            "Model-mix warning: candidate sweep used model fallback but baseline did not;              resolved-rate delta may reflect model differences, not prompt/harness changes."
                .to_owned(),
        ),
        (true, false) => warnings.push(
            "Model-mix warning: baseline sweep used model fallback but candidate did not;              resolved-rate delta may reflect model differences, not prompt/harness changes."
                .to_owned(),
        ),
        (true, true) => {
            if baseline.model_mix != candidate.model_mix {
                let b_summary: Vec<String> = baseline
                    .model_mix
                    .iter()
                    .map(|(m, n)| format!("{m}:{n}"))
                    .collect();
                let c_summary: Vec<String> = candidate
                    .model_mix
                    .iter()
                    .map(|(m, n)| format!("{m}:{n}"))
                    .collect();
                warnings.push(format!(
                    "Model-mix warning: baseline and candidate have different final model                      distributions (baseline=[{}], candidate=[{}]); compare deltas may be                      confounded by model differences.",
                    b_summary.join(", "),
                    c_summary.join(", "),
                ));
            }
            // Same model distribution but different fallback *rates*: one sweep
            // hit many more transient primary failures than the other, which can
            // still confound resolved-rate deltas even when both ended up on the
            // same final model.
            if baseline.total_fallbacks != candidate.total_fallbacks {
                warnings.push(format!(
                    "Model-mix warning: baseline and candidate have different fallback attempt \
                     counts (baseline={}, candidate={}); a large rate difference may reflect \
                     different primary-model reliability rather than harness or prompt changes.",
                    baseline.total_fallbacks, candidate.total_fallbacks,
                ));
            }
        }
        (false, false) => {}
    }

    warnings
}

// ─────────────────────────────────────────────────────────────────────────────

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
    pub artifact: Option<ArtifactCompatibility>,
    pub artifact_warnings: Vec<String>,
    pub total_fallbacks: u64,
    pub model_mix: std::collections::BTreeMap<String, usize>,
}

struct DiffContext<'a> {
    manifest_deltas: Vec<String>,
    artifact_version_mismatches: Vec<String>,
    artifact_warnings: Vec<String>,
    baseline_model_name: Option<&'a str>,
    candidate_model_name: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedRunSlot {
    pub instance_id: String,
    pub run_index: u32,
    pub result: InstanceResult,
}

#[allow(clippy::too_many_lines)]
pub fn load_sweep(dir: &Path) -> Result<LoadedSweep, Error> {
    let results_path = dir.join("results.json");
    if results_path.exists() {
        let text = std::fs::read_to_string(&results_path)?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        let artifact = classify_json_value(
            &value,
            ArtifactKind::SweepResults,
            results_path.display().to_string(),
        )
        .map_err(|err| Error::Trajectory(err.to_string()))?;
        let artifact_warnings = artifact.warnings.clone();
        let filter_spec_present = value.get("filter_spec").is_some();
        let sweep: SweepResults = serde_json::from_value(value)?;
        let rate_limit_events = sweep.rate_limit_events.clone();
        let total_fallbacks = sweep.total_fallbacks;
        let model_mix = sweep.model_mix.clone();
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
            let scanned_slots = scan_trajectory_run_slots(dir, min_mtime)?;
            let (slot_fallbacks, slot_mix) = fallback_totals_from_slots(&scanned_slots);
            let scanned = aggregate_scanned_results(scanned_slots);
            let manifest = sweep.manifest;
            // When trajectory files are fresher than results.json, use scanned
            // instances and re-derive fallback totals from per-run-slot data so
            // model-mix warnings count all reruns, not just the winning slot.
            let (effective_fallbacks, effective_mix) = if scanned.is_empty() {
                (total_fallbacks, model_mix)
            } else {
                (slot_fallbacks, slot_mix)
            };
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
                artifact: Some(artifact),
                artifact_warnings,
                total_fallbacks: effective_fallbacks,
                model_mix: effective_mix,
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
            artifact: Some(artifact),
            artifact_warnings,
            total_fallbacks,
            model_mix,
        });
    }
    if !dir.exists() {
        return Err(Error::Trajectory(format!(
            "compare: directory does not exist: {}",
            dir.display()
        )));
    }

    let slots = scan_trajectory_run_slots(dir, None)?;
    let (total_fallbacks, model_mix) = fallback_totals_from_slots(&slots);
    let out = aggregate_scanned_results(slots);
    Ok(LoadedSweep {
        instances: out,
        manifest: None,
        filter_spec: None,
        rate_limit_events: None,
        artifact: None,
        artifact_warnings: Vec::new(),
        total_fallbacks,
        model_mix,
    })
}

fn manifest_indicates_resume(manifest: &ProvenanceManifest) -> bool {
    manifest.runtime.resume_mode || manifest.cli.argv.iter().any(|arg| arg == "--resume")
}

/// Compute `total_fallbacks` and `model_mix` from per-run-slot data before
/// aggregation, so that reruns with different responding models are all counted.
fn fallback_totals_from_slots(
    slots: &[LoadedRunSlot],
) -> (u64, std::collections::BTreeMap<String, usize>) {
    let total_fallbacks: u64 = slots
        .iter()
        .filter_map(|s| s.result.fallback_count)
        .map(u64::from)
        .sum();
    let mut model_mix: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for s in slots {
        if let Some(model) = s.result.final_model.as_deref() {
            *model_mix.entry(model.to_owned()).or_insert(0) += 1;
        }
    }
    (total_fallbacks, model_mix)
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
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string())
        .map_err(|err| Error::Trajectory(err.to_string()))?;
    let traj: Trajectory = match serde_json::from_value(value) {
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
        cost_usd: info.actual_cost_usd.or(info.total_cost_usd),
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
        fallback_count: info.fallback_summary.as_ref().map(|s| s.fallback_count),
        // Exclude all-failed runs from model_mix — final_model is only the
        // last attempted model when all_failed=true, not a responding model.
        final_model: info.fallback_summary.as_ref().and_then(|s| {
            if s.all_failed {
                None
            } else {
                Some(s.final_model.clone())
            }
        }),
        retry_id: None,
        previous_failure_category: None,
        trace_id: info.trace_id,
        context_pressure: Default::default(),
        peak_memory_bytes: info.peak_memory_bytes,
        cpu_seconds: info.cpu_seconds,
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
        // Sum fallback counts across all run slots; keep final_model from the
        // first (pass@1 representative) run.
        aggregate.fallback_count = Some(
            rows.iter()
                .filter_map(|(_, result)| result.fallback_count)
                .fold(0u32, u32::saturating_add),
        );
        aggregate.final_model.clone_from(&first.final_model);
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
#[allow(clippy::too_many_lines)]
pub fn compute(args: &CompareArgs) -> Result<CompareReport, Error> {
    let baseline = load_sweep(&args.baseline)?;
    let candidate = load_sweep(&args.candidate)?;
    let baseline_eval = load_evaluation_results_checked(&args.baseline)?;
    let candidate_eval = load_evaluation_results_checked(&args.candidate)?;
    let baseline_model_name = baseline.manifest.as_ref().map(|m| m.model.name.as_str());
    let candidate_model_name = candidate.manifest.as_ref().map(|m| m.model.name.as_str());
    let baseline_eval_results = baseline_eval.as_ref().map(|loaded| &loaded.results);
    let candidate_eval_results = candidate_eval.as_ref().map(|loaded| &loaded.results);
    let baseline_resolved_override = baseline_eval_results.map(resolved_overrides_from_eval);
    let candidate_resolved_override = candidate_eval_results.map(resolved_overrides_from_eval);

    // Load flake report and compute the excluded instance set.
    let (flaky_ids, flaky_instances_excluded) = if let Some(ref flake_path) = args.flake_report {
        let flake_report = crate::run::eval_flake::EvalFlakeReport::load(flake_path)?;
        let flagged_ids = flake_report.flaky_ids();
        // Count paired instances that are flaky (present in both sweeps and flaky).
        let paired_flaky = flagged_ids
            .iter()
            .filter(|id| {
                baseline.instances.contains_key(*id) && candidate.instances.contains_key(*id)
            })
            .count();
        // Degenerate: all paired instances are flaky — caller gets UsageError.
        let paired_n = baseline
            .instances
            .keys()
            .filter(|id| candidate.instances.contains_key(*id))
            .count();
        let non_flaky_paired = paired_n.saturating_sub(paired_flaky);
        if non_flaky_paired == 0 && paired_n > 0 {
            return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                "compare: all {paired_flaky} paired instance(s) are flagged flaky in the flake report; \
                 no instances remain for the significance test. \
                 Provide a sweep with non-flaky instances or omit --flake-report.",
            ))));
        }
        (flagged_ids, paired_flaky)
    } else {
        (std::collections::HashSet::new(), 0)
    };

    // Filter instance maps to exclude flaky instances from delta + significance.
    let filtered_baseline: HashMap<String, InstanceResult> = if flaky_ids.is_empty() {
        baseline.instances.clone()
    } else {
        baseline
            .instances
            .iter()
            .filter(|(id, _)| !flaky_ids.contains(*id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    let filtered_candidate: HashMap<String, InstanceResult> = if flaky_ids.is_empty() {
        candidate.instances.clone()
    } else {
        candidate
            .instances
            .iter()
            .filter(|(id, _)| !flaky_ids.contains(*id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let artifact_warnings = artifact_warning_lines(
        &baseline,
        &candidate,
        baseline_eval.as_ref(),
        candidate_eval.as_ref(),
    );
    let mut report = diff_with_overrides(
        &args.baseline,
        &args.candidate,
        &filtered_baseline,
        &filtered_candidate,
        baseline_resolved_override.as_ref(),
        candidate_resolved_override.as_ref(),
        DiffContext {
            manifest_deltas: manifest_delta_lines(
                baseline.manifest.as_ref(),
                candidate.manifest.as_ref(),
            ),
            artifact_version_mismatches: artifact_version_mismatch_lines(
                &baseline,
                &candidate,
                baseline_eval.as_ref(),
                candidate_eval.as_ref(),
            ),
            artifact_warnings,
            baseline_model_name,
            candidate_model_name,
        },
    );
    report.flaky_instances_excluded = flaky_instances_excluded;
    report.subset_warnings = subset_warnings(
        baseline.filter_spec.as_ref(),
        candidate.filter_spec.as_ref(),
    );
    report
        .baseline_rate_limit_events
        .clone_from(&baseline.rate_limit_events);
    report
        .candidate_rate_limit_events
        .clone_from(&candidate.rate_limit_events);
    report.breakdown_delta = build_breakdown_delta(
        &baseline.instances,
        &candidate.instances,
        baseline_resolved_override.as_ref(),
        candidate_resolved_override.as_ref(),
        &args.breakdown.axes,
        args.min_delta_pp,
    );
    let baseline_patch_agg = baseline_eval
        .as_ref()
        .map_or_else(PatchStatsAggregates::default, |eval| {
            patch_stats_aggregates(&eval.results, &baseline.instances)
        });
    let candidate_patch_agg = candidate_eval
        .as_ref()
        .map_or_else(PatchStatsAggregates::default, |eval| {
            patch_stats_aggregates(&eval.results, &candidate.instances)
        });
    apply_patch_stats_aggregates(&mut report, baseline_patch_agg, candidate_patch_agg);
    let baseline_test_only = baseline_eval
        .as_ref()
        .and_then(|loaded| loaded.results.test_only_resolved_rate);
    let candidate_test_only = candidate_eval
        .as_ref()
        .and_then(|loaded| loaded.results.test_only_resolved_rate);
    report.baseline_test_only_resolved_rate = baseline_test_only;
    report.candidate_test_only_resolved_rate = candidate_test_only;
    report.test_only_resolved_rate_delta = match (baseline_test_only, candidate_test_only) {
        (Some(b), Some(c)) => Some(c - b),
        _ => None,
    };
    apply_cost_attribution_delta(
        &mut report,
        CostAttributionContext {
            args,
            baseline: &baseline,
            candidate: &candidate,
            baseline_eval: baseline_eval.as_ref(),
            candidate_eval: candidate_eval.as_ref(),
            baseline_model_name,
            candidate_model_name,
        },
    )?;
    report.model_mix_warnings = build_model_mix_warnings(
        &ModelMixSnapshot {
            model_mix: baseline.model_mix.clone(),
            total_fallbacks: baseline.total_fallbacks,
        },
        &ModelMixSnapshot {
            model_mix: candidate.model_mix.clone(),
            total_fallbacks: candidate.total_fallbacks,
        },
    );
    apply_evaluator_provenance(&mut report, baseline_eval.as_ref(), candidate_eval.as_ref());
    let dataset_sha256_a = baseline_eval
        .as_ref()
        .and_then(|e| e.results.provenance.as_ref())
        .and_then(|p| p.dataset_sha256.clone());
    let dataset_sha256_b = candidate_eval
        .as_ref()
        .and_then(|e| e.results.provenance.as_ref())
        .and_then(|p| p.dataset_sha256.clone());
    let dataset_instance_count_a = baseline_eval
        .as_ref()
        .and_then(|e| e.results.provenance.as_ref())
        .and_then(|p| p.dataset_instance_count);
    let dataset_instance_count_b = candidate_eval
        .as_ref()
        .and_then(|e| e.results.provenance.as_ref())
        .and_then(|p| p.dataset_instance_count);
    let dataset_content_matches = match (&dataset_sha256_a, &dataset_sha256_b) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };
    report.comparability = ComparabilityBlock {
        dataset_sha256_a,
        dataset_sha256_b,
        dataset_instance_count_a,
        dataset_instance_count_b,
        dataset_content_matches,
    };
    // Sampling drift: scan trajectory files for both sweeps.
    let common_ids: Vec<String> = baseline
        .instances
        .keys()
        .filter(|id| candidate.instances.contains_key(*id))
        .cloned()
        .collect();
    report.sampling_drift = detect_sampling_drift(&args.baseline, &args.candidate, &common_ids);
    Ok(report)
}

fn apply_evaluator_provenance(
    report: &mut CompareReport,
    baseline_eval: Option<&LoadedEvaluationResults>,
    candidate_eval: Option<&LoadedEvaluationResults>,
) {
    let (status, warnings) = compare_evaluator_provenance(
        baseline_eval.and_then(|e| e.results.provenance.as_ref()),
        candidate_eval.and_then(|e| e.results.provenance.as_ref()),
    );
    report.evaluator_provenance_status = status;
    report.evaluator_provenance_warnings = warnings;
}

#[derive(Clone, Copy)]
struct CostAttributionContext<'a> {
    args: &'a CompareArgs,
    baseline: &'a LoadedSweep,
    candidate: &'a LoadedSweep,
    baseline_eval: Option<&'a LoadedEvaluationResults>,
    candidate_eval: Option<&'a LoadedEvaluationResults>,
    baseline_model_name: Option<&'a str>,
    candidate_model_name: Option<&'a str>,
}

fn apply_cost_attribution_delta(
    report: &mut CompareReport,
    ctx: CostAttributionContext<'_>,
) -> Result<(), Error> {
    if !ctx.args.cost_attribution {
        return Ok(());
    }
    let baseline_cost_rows = ctx
        .baseline_eval
        .and_then(|eval| non_empty_cost_attribution_rows(&eval.results));
    let candidate_cost_rows = ctx
        .candidate_eval
        .and_then(|eval| non_empty_cost_attribution_rows(&eval.results));
    let baseline_fallback_rows = if baseline_cost_rows.is_none() {
        Some(build_cost_attribution_rows_from_run_slots(
            &load_run_slots(&ctx.args.baseline, &ctx.baseline.instances)?,
            ctx.baseline_model_name,
        ))
    } else {
        None
    };
    let candidate_fallback_rows = if candidate_cost_rows.is_none() {
        Some(build_cost_attribution_rows_from_run_slots(
            &load_run_slots(&ctx.args.candidate, &ctx.candidate.instances)?,
            ctx.candidate_model_name,
        ))
    } else {
        None
    };
    let baseline_rows =
        baseline_cost_rows.unwrap_or_else(|| baseline_fallback_rows.as_deref().unwrap_or(&[]));
    let candidate_rows =
        candidate_cost_rows.unwrap_or_else(|| candidate_fallback_rows.as_deref().unwrap_or(&[]));
    report.cost_attribution_delta = build_cost_attribution_delta_from_rows(
        baseline_rows,
        candidate_rows,
        ctx.args.cost_attribution_min_delta_usd,
    );
    report.cost_attribution_warnings = build_cost_attribution_warnings(&report.subset_warnings);
    Ok(())
}

pub fn write_diff_script(report: &CompareReport, out_path: &Path) -> Result<(), Error> {
    let exe = std::env::current_exe()
        .ok()
        .map_or_else(|| "max".into(), |path| path.display().to_string());
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
            artifact_version_mismatches: Vec::new(),
            artifact_warnings: Vec::new(),
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

    let mut resolved_rate_significance =
        compute_paired_significance(&transition_summary.transitions);
    // For rerun sweeps the transition matrix uses resolved_count > 0 (pass@k),
    // which does not reflect the multi-run resolved rate used by the population
    // metrics.  Mark the significance block underpowered so gating flags require
    // --allow-underpowered and operators are not silently misled.
    let either_is_rerun =
        baseline.values().any(|r| r.runs > 1) || candidate.values().any(|r| r.runs > 1);
    if either_is_rerun {
        resolved_rate_significance.underpowered = true;
        resolved_rate_significance.underpowered_reason = Some(
            "rerun sweep detected: paired test uses pass@k (resolved_count > 0), \
             not the multi-run resolved rate; significance gating is unreliable"
                .to_owned(),
        );
    }

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
        baseline_mean_lines_changed: None,
        candidate_mean_lines_changed: None,
        mean_lines_changed_delta: None,
        baseline_p90_lines_changed: None,
        candidate_p90_lines_changed: None,
        p90_lines_changed_delta: None,
        baseline_mean_files_changed: None,
        candidate_mean_files_changed: None,
        mean_files_changed_delta: None,
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
        artifact_version_mismatches: diff_context.artifact_version_mismatches,
        artifact_warnings: diff_context.artifact_warnings,
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
        model_mix_warnings: Vec::new(),
        evaluator_provenance_status: EvaluatorProvenanceStatus::Unavailable,
        evaluator_provenance_warnings: Vec::new(),
        resolved_rate_significance,
        sampling_drift: None,
        baseline_test_only_resolved_rate: None,
        candidate_test_only_resolved_rate: None,
        test_only_resolved_rate_delta: None,
        flaky_instances_excluded: 0,
        comparability: ComparabilityBlock {
            dataset_sha256_a: None,
            dataset_sha256_b: None,
            dataset_instance_count_a: None,
            dataset_instance_count_b: None,
            dataset_content_matches: None,
        },
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

#[derive(Debug, Clone, Copy, Default)]
struct PatchStatsAggregates {
    mean_lines_changed: Option<f64>,
    p90_lines_changed: Option<f64>,
    mean_files_changed: Option<f64>,
}

fn apply_patch_stats_aggregates(
    report: &mut CompareReport,
    baseline: PatchStatsAggregates,
    candidate: PatchStatsAggregates,
) {
    report.baseline_mean_lines_changed = baseline.mean_lines_changed;
    report.candidate_mean_lines_changed = candidate.mean_lines_changed;
    report.mean_lines_changed_delta =
        optional_delta(baseline.mean_lines_changed, candidate.mean_lines_changed);
    report.baseline_p90_lines_changed = baseline.p90_lines_changed;
    report.candidate_p90_lines_changed = candidate.p90_lines_changed;
    report.p90_lines_changed_delta =
        optional_delta(baseline.p90_lines_changed, candidate.p90_lines_changed);
    report.baseline_mean_files_changed = baseline.mean_files_changed;
    report.candidate_mean_files_changed = candidate.mean_files_changed;
    report.mean_files_changed_delta =
        optional_delta(baseline.mean_files_changed, candidate.mean_files_changed);
}

fn optional_delta(baseline: Option<f64>, candidate: Option<f64>) -> Option<f64> {
    match (baseline, candidate) {
        (Some(b), Some(c)) => Some(c - b),
        _ => None,
    }
}

fn patch_stats_aggregates<S: std::hash::BuildHasher>(
    eval: &EvaluationResults,
    loaded_instances: &HashMap<String, InstanceResult, S>,
) -> PatchStatsAggregates {
    let stats: Vec<&PatchStats> = eval
        .instances
        .iter()
        .filter(|row| row.resolved && loaded_instances.contains_key(&row.instance_id))
        .filter_map(|row| row.patch_stats.as_ref())
        .collect();
    if stats.is_empty() {
        return PatchStatsAggregates::default();
    }
    let lines_changed: Vec<u32> = stats.iter().map(|s| s.lines_changed()).collect();
    let files_changed: Vec<u32> = stats.iter().map(|s| s.files_changed).collect();
    PatchStatsAggregates {
        mean_lines_changed: Some(mean_u32(&lines_changed)),
        p90_lines_changed: Some(f64::from(p90_u32(&lines_changed))),
        mean_files_changed: Some(mean_u32(&files_changed)),
    }
}

#[allow(clippy::cast_precision_loss)]
fn mean_u32(values: &[u32]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let total: u64 = values.iter().map(|value| u64::from(*value)).sum();
    total as f64 / values.len() as f64
}

fn p90_u32(values: &[u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = (sorted.len() * 9).div_ceil(10).saturating_sub(1);
    sorted[index]
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
    Ok(load_evaluation_results_checked(dir)?.map(|loaded| loaded.results))
}

#[derive(Debug, Clone)]
pub struct LoadedEvaluationResults {
    pub results: EvaluationResults,
    pub artifact: ArtifactCompatibility,
    pub artifact_warnings: Vec<String>,
}

pub fn load_evaluation_results_checked(
    dir: &Path,
) -> Result<Option<LoadedEvaluationResults>, Error> {
    let path = crate::run::evaluate::evaluation_path(dir);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    let artifact = classify_json_value(
        &value,
        ArtifactKind::EvaluationResults,
        path.display().to_string(),
    )
    .map_err(|err| Error::Trajectory(err.to_string()))?;
    let artifact_warnings = artifact.warnings.clone();
    Ok(Some(LoadedEvaluationResults {
        results: serde_json::from_value(value)?,
        artifact,
        artifact_warnings,
    }))
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

fn artifact_version_mismatch_lines(
    baseline: &LoadedSweep,
    candidate: &LoadedSweep,
    baseline_eval: Option<&LoadedEvaluationResults>,
    candidate_eval: Option<&LoadedEvaluationResults>,
) -> Vec<String> {
    let mut out = Vec::new();
    let baseline_results =
        artifact_identity(baseline.artifact.as_ref(), ArtifactKind::SweepResults);
    let candidate_results =
        artifact_identity(candidate.artifact.as_ref(), ArtifactKind::SweepResults);
    push_artifact_mismatch(
        &mut out,
        "results.json",
        &baseline_results,
        &candidate_results,
    );
    if baseline_eval.is_some() || candidate_eval.is_some() {
        let baseline_evaluation = artifact_identity(
            baseline_eval.map(|loaded| &loaded.artifact),
            ArtifactKind::EvaluationResults,
        );
        let candidate_evaluation = artifact_identity(
            candidate_eval.map(|loaded| &loaded.artifact),
            ArtifactKind::EvaluationResults,
        );
        push_artifact_mismatch(
            &mut out,
            "evaluation.json",
            &baseline_evaluation,
            &candidate_evaluation,
        );
    }
    out
}

fn artifact_warning_lines(
    baseline: &LoadedSweep,
    candidate: &LoadedSweep,
    baseline_eval: Option<&LoadedEvaluationResults>,
    candidate_eval: Option<&LoadedEvaluationResults>,
) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(
        baseline
            .artifact_warnings
            .iter()
            .map(|warning| format!("baseline {warning}")),
    );
    out.extend(
        candidate
            .artifact_warnings
            .iter()
            .map(|warning| format!("candidate {warning}")),
    );
    if let Some(eval) = baseline_eval {
        out.extend(
            eval.artifact_warnings
                .iter()
                .map(|warning| format!("baseline {warning}")),
        );
    }
    if let Some(eval) = candidate_eval {
        out.extend(
            eval.artifact_warnings
                .iter()
                .map(|warning| format!("candidate {warning}")),
        );
    }
    out
}

fn push_artifact_mismatch(out: &mut Vec<String>, label: &str, baseline: &str, candidate: &str) {
    if baseline != candidate {
        out.push(format!(
            "{label}: baseline {baseline} candidate {candidate}"
        ));
    }
}

fn artifact_identity(artifact: Option<&ArtifactCompatibility>, expected: ArtifactKind) -> String {
    artifact.map_or_else(
        || format!("{expected}@unavailable"),
        ArtifactCompatibility::identity_label,
    )
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
        FailureCategory::AgentStagnation => "agent_stagnation",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::HistoryCompactionFailed => "history_compaction_failed",
        FailureCategory::Unknown => "unknown",
        FailureCategory::ReadOnlyViolation => "read_only_violation",
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

/// Compare evaluator provenance from two `evaluation.json` artifacts and return
/// the classification status plus human-readable warning strings.
///
/// Fields that do NOT affect scoring comparability (run_id, prediction_path,
/// prediction_sha256, timestamps) are intentionally ignored so that comparing
/// two different candidate sweeps scored by the same evaluator setup reports
/// `Matching` rather than `Mismatched`.
#[allow(clippy::too_many_lines)]
fn compare_evaluator_provenance(
    baseline: Option<&crate::run::evaluate::EvaluatorProvenance>,
    candidate: Option<&crate::run::evaluate::EvaluatorProvenance>,
) -> (EvaluatorProvenanceStatus, Vec<String>) {
    let (Some(b), Some(c)) = (baseline, candidate) else {
        let msg = match (baseline.is_some(), candidate.is_some()) {
            (true, false) => "evaluator provenance: candidate evaluation.json has no provenance (legacy artifact)".into(),
            (false, true) => "evaluator provenance: baseline evaluation.json has no provenance (legacy artifact)".into(),
            _ => "evaluator provenance: neither evaluation.json has provenance (legacy artifacts or evaluation not yet run)".into(),
        };
        return (EvaluatorProvenanceStatus::Unavailable, vec![msg]);
    };

    let mut warnings = Vec::new();

    let mut has_legacy_missing_hash = false;
    if let (Some(b_sha), Some(c_sha)) = (&b.dataset_sha256, &c.dataset_sha256) {
        if b_sha != c_sha {
            warnings.push("evaluator provenance: dataset content (sha256) differs".to_owned());
        }
    } else {
        has_legacy_missing_hash = true;
        warnings.push("evaluator provenance: legacy artifact missing dataset_sha256".to_owned());
    }

    if b.backend != c.backend {
        warnings.push(format!(
            "evaluator provenance: backend differs (baseline={:?}, candidate={:?})",
            b.backend, c.backend
        ));
    }

    match (&b.backend_version, &c.backend_version) {
        (Some(bv), Some(cv)) if bv != cv => {
            warnings.push(format!(
                "evaluator provenance: backend version differs (baseline={bv:?}, candidate={cv:?})"
            ));
        }
        _ => {}
    }

    if b.dataset_subset != c.dataset_subset {
        warnings.push(format!(
            "evaluator provenance: dataset subset differs (baseline={:?}, candidate={:?})",
            b.dataset_subset, c.dataset_subset
        ));
    }

    if b.dataset_split != c.dataset_split {
        warnings.push(format!(
            "evaluator provenance: dataset split differs (baseline={:?}, candidate={:?})",
            b.dataset_split, c.dataset_split
        ));
    }

    if b.backend == "sb-cli" && c.backend == "sb-cli" {
        match (&b.sb_cli, &c.sb_cli) {
            (Some(b_sb), Some(c_sb)) => {
                if b_sb.timeout_per_instance_secs != c_sb.timeout_per_instance_secs {
                    warnings.push(format!(
                        "evaluator provenance: timeout_per_instance_secs differs (baseline={}, candidate={})",
                        b_sb.timeout_per_instance_secs, c_sb.timeout_per_instance_secs
                    ));
                }
                if b_sb.parallel != c_sb.parallel {
                    warnings.push(format!(
                        "evaluator provenance: parallel differs (baseline={}, candidate={})",
                        b_sb.parallel, c_sb.parallel
                    ));
                }
            }
            (None, Some(_)) => warnings.push(
                "evaluator provenance: baseline sb-cli details unavailable (legacy artifact)"
                    .into(),
            ),
            (Some(_), None) => warnings.push(
                "evaluator provenance: candidate sb-cli details unavailable (legacy artifact)"
                    .into(),
            ),
            (None, None) => {}
        }
    }

    if b.backend == "docker-tests" && c.backend == "docker-tests" {
        match (&b.docker_tests, &c.docker_tests) {
            (Some(b_dt), Some(c_dt)) => {
                if b_dt.timeout_per_instance_secs != c_dt.timeout_per_instance_secs {
                    warnings.push(format!(
                        "evaluator provenance: docker_tests timeout_per_instance_secs differs (baseline={}, candidate={})",
                        b_dt.timeout_per_instance_secs, c_dt.timeout_per_instance_secs
                    ));
                }
                if b_dt.parallel != c_dt.parallel {
                    warnings.push(format!(
                        "evaluator provenance: docker_tests parallel differs (baseline={}, candidate={})",
                        b_dt.parallel, c_dt.parallel
                    ));
                }
                if b_dt.image_names != c_dt.image_names {
                    warnings.push(format!(
                        "evaluator provenance: docker_tests image names differ (baseline={:?}, candidate={:?})",
                        b_dt.image_names, c_dt.image_names
                    ));
                }
            }
            (None, Some(_)) => warnings.push(
                "evaluator provenance: baseline docker-tests details unavailable (legacy artifact)"
                    .into(),
            ),
            (Some(_), None) => warnings.push(
                "evaluator provenance: candidate docker-tests details unavailable (legacy artifact)"
                    .into(),
            ),
            (None, None) => {}
        }
    }

    if warnings.is_empty() {
        (EvaluatorProvenanceStatus::Matching, Vec::new())
    } else if has_legacy_missing_hash && warnings.len() == 1 {
        (EvaluatorProvenanceStatus::Unavailable, warnings)
    } else {
        (EvaluatorProvenanceStatus::Mismatched, warnings)
    }
}

// ── Paired significance (McNemar exact test) ──────────────────────────────

/// Two-sided exact McNemar p-value.
///
/// `pass_to_fail` = n01 (baseline pass, candidate fail)
/// `fail_to_pass` = n10 (baseline fail, candidate pass)
///
/// Under H0 each discordant pair is equally likely to go either way, so
/// the smaller count follows Binomial(n_discordant, 0.5). The two-sided
/// p-value is 2 * Σ_{k=0}^{min(n01,n10)} C(n,k) * 0.5^n, capped at 1.
///
/// log C(n,k) is accumulated incrementally via the recurrence
/// log C(n,k) = log C(n,k-1) + log(n-k+1) - log(k), giving O(m) time.
#[allow(clippy::cast_precision_loss)]
fn mcnemar_exact_p_value(pass_to_fail: usize, fail_to_pass: usize) -> f64 {
    let n = pass_to_fail + fail_to_pass;
    if n == 0 {
        return 1.0;
    }
    let m = pass_to_fail.min(fail_to_pass);
    let log_half_n = -(n as f64) * std::f64::consts::LN_2;
    let mut log_binom = 0.0_f64; // log C(n, 0) = 0
    let mut tail = 0.0_f64;
    for k in 0..=m {
        tail += (log_binom + log_half_n).exp();
        if k < m {
            // recurrence: log C(n,k+1) = log C(n,k) + log(n-k) - log(k+1)
            log_binom += ((n - k) as f64).ln() - ((k + 1) as f64).ln();
        }
    }
    (2.0 * tail).min(1.0)
}

/// Build the `ResolvedRateSignificance` block from the transition counts.
fn compute_paired_significance(
    transitions: &BTreeMap<TransitionKind, usize>,
) -> ResolvedRateSignificance {
    let pass_pass = *transitions.get(&TransitionKind::PassPass).unwrap_or(&0);
    let pass_fail = *transitions.get(&TransitionKind::PassFail).unwrap_or(&0);
    let fail_pass = *transitions.get(&TransitionKind::FailPass).unwrap_or(&0);
    let fail_fail = *transitions.get(&TransitionKind::FailFail).unwrap_or(&0);
    let missing_present = *transitions
        .get(&TransitionKind::MissingPresent)
        .unwrap_or(&0);
    let present_missing = *transitions
        .get(&TransitionKind::PresentMissing)
        .unwrap_or(&0);

    let paired_n = pass_pass + pass_fail + fail_pass + fail_fail;
    let pass_to_fail = pass_fail;
    let fail_to_pass = fail_pass;
    let discordant = pass_to_fail + fail_to_pass;

    // Wilson-score CI on the rate delta within the paired subset.
    let baseline_resolved_paired = pass_pass + pass_fail;
    let candidate_resolved_paired = pass_pass + fail_pass;
    let ci = wilson_delta_ci95(
        usize_to_u64(candidate_resolved_paired),
        usize_to_u64(paired_n),
        usize_to_u64(baseline_resolved_paired),
        usize_to_u64(paired_n),
    );

    // Paired delta rate: candidate rate - baseline rate, restricted to the overlap.
    // = (fail_to_pass - pass_to_fail) / paired_n.  Zero when paired_n==0.
    #[allow(clippy::cast_precision_loss)]
    let paired_delta_rate = if paired_n == 0 {
        0.0
    } else {
        (fail_to_pass as f64 - pass_to_fail as f64) / paired_n as f64
    };

    let p_value = if paired_n == 0 {
        None
    } else if discordant == 0 {
        Some(1.0)
    } else {
        Some(mcnemar_exact_p_value(pass_to_fail, fail_to_pass))
    };

    let (underpowered, underpowered_reason) = if paired_n == 0 {
        (true, Some("no shared instance_ids (paired_n=0)".to_owned()))
    } else if discordant < UNDERPOWERED_DISCORDANT_THRESHOLD {
        (
            true,
            Some(format!(
                "fewer than {UNDERPOWERED_DISCORDANT_THRESHOLD} discordant pairs (got {discordant})"
            )),
        )
    } else {
        (false, None)
    };

    ResolvedRateSignificance {
        test_name: "mcnemar_exact".to_owned(),
        p_value,
        ci95_lower_pp: ci.lower * 100.0,
        ci95_upper_pp: ci.upper * 100.0,
        paired_n,
        pass_to_fail,
        fail_to_pass,
        paired_delta_rate,
        underpowered,
        underpowered_reason,
        only_in_baseline: present_missing,
        only_in_candidate: missing_present,
    }
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

// ── sampling drift ───────────────────────────────────────────────────────────

fn sampling_differs(a: &crate::model::SamplingParams, b: &crate::model::SamplingParams) -> bool {
    a != b
}

/// Scan trajectory files for both sweeps and count per-step sampling drift.
/// Returns `None` when no trajectory files were found in either directory.
fn detect_sampling_drift(
    baseline_dir: &Path,
    candidate_dir: &Path,
    instance_ids: &[String],
) -> Option<SamplingDriftSummary> {
    let mut steps_drifted = 0usize;
    let mut example: Option<SamplingDriftExample> = None;
    let mut any_loaded = false;

    for id in instance_ids {
        let b_trajs = crate::trajectory::load_all_trajectories_for_instance(baseline_dir, id);
        let c_trajs = crate::trajectory::load_all_trajectories_for_instance(candidate_dir, id);
        if b_trajs.is_empty() || c_trajs.is_empty() {
            continue;
        }
        any_loaded = true;

        for (b_traj, c_traj) in b_trajs.iter().zip(c_trajs.iter()) {
            // Collect Option<&SamplingParams> for every assistant turn so that
            // Some(params) vs None (legacy trajectory) is counted as drift.
            let b_sampling: Vec<Option<&crate::model::SamplingParams>> = b_traj
                .messages
                .iter()
                .filter(|m| m.role == "assistant")
                .map(|m| m.extra.sampling.as_ref())
                .collect();
            let c_sampling: Vec<Option<&crate::model::SamplingParams>> = c_traj
                .messages
                .iter()
                .filter(|m| m.role == "assistant")
                .map(|m| m.extra.sampling.as_ref())
                .collect();

            // zip-longest: extra turns on either side count as drift.
            let len = b_sampling.len().max(c_sampling.len());
            for i in 0..len {
                let bs = b_sampling.get(i).copied().flatten();
                let cs = c_sampling.get(i).copied().flatten();
                let drifted = match (bs, cs) {
                    (None, None) => false,
                    (Some(a), Some(b)) => sampling_differs(a, b),
                    _ => true,
                };
                if drifted {
                    steps_drifted += 1;
                    if example.is_none() {
                        example = Some(SamplingDriftExample {
                            instance_id: id.clone(),
                            baseline_sampling: bs.cloned(),
                            candidate_sampling: cs.cloned(),
                        });
                    }
                }
            }
        }
    }

    if !any_loaded {
        return None;
    }
    Some(SamplingDriftSummary {
        steps_drifted,
        example,
    })
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

            fallback_count: None,

            final_model: None,
            retry_id: None,
            previous_failure_category: None,
            trace_id: None,
            context_pressure: Default::default(),

            peak_memory_bytes: None,

            cpu_seconds: None,
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

            fallback_count: None,

            final_model: None,
            retry_id: None,
            previous_failure_category: None,

            trace_id: None,
            context_pressure: Default::default(),

            peak_memory_bytes: None,

            cpu_seconds: None,
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

            fallback_count: None,

            final_model: None,
            retry_id: None,
            previous_failure_category: None,

            trace_id: None,
            context_pressure: Default::default(),

            peak_memory_bytes: None,

            cpu_seconds: None,
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
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
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
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances,
            rate_limit_events: None,

            total_fallbacks: 0,

            model_mix: BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,
            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
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
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
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
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![submitted("a"), errored("b", FailureCategory::ModelApi)],
            rate_limit_events: None,

            total_fallbacks: 0,

            model_mix: BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,

            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
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
            max_patch_size_regression_pct: None,
            breakdown: crate::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: true,
            cost_attribution_min_delta_usd: 1.0,
            min_significance: None,
            regression_significance: None,
            allow_underpowered: false,
            flake_report: None,
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

            fallback_count: None,

            final_model: None,
            retry_id: None,
            previous_failure_category: None,

            trace_id: None,
            context_pressure: Default::default(),

            peak_memory_bytes: None,

            cpu_seconds: None,
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

            fallback_count: None,

            final_model: None,
            retry_id: None,
            previous_failure_category: None,

            trace_id: None,
            context_pressure: Default::default(),

            peak_memory_bytes: None,

            cpu_seconds: None,
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
                ..Default::default()
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
            chaos_fail_every: 0,
            circuit_breaker: None,
            source: None,
            import_predictions_path: None,
            import_predictions_sha256: None,
            reproduced_from: None,
            merged_from: None,
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
    #[allow(clippy::too_many_lines)]
    fn prefers_evaluation_json_resolved_over_submission_proxy() {
        let dir_b = tempfile::tempdir().unwrap();
        let dir_c = tempfile::tempdir().unwrap();
        let baseline_sweep = SweepResults {
            total: 1,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
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
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![submitted("a")],
            rate_limit_events: None,

            total_fallbacks: 0,

            model_mix: BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,

            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
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
                patch_stats: None,
                patch_error_log: None,
                submission_fingerprint: None,
            }],
            ..Default::default()
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
                patch_stats: None,
                patch_error_log: None,
                submission_fingerprint: None,
            }],
            ..Default::default()
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
            max_patch_size_regression_pct: None,
            breakdown: crate::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: true,
            cost_attribution_min_delta_usd: 1.0,
            min_significance: None,
            regression_significance: None,
            allow_underpowered: false,
            flake_report: None,
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
                patch_stats: None,
                patch_error_log: None,
                submission_fingerprint: None,
            }],
            ..Default::default()
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
            max_patch_size_regression_pct: None,
            breakdown: crate::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: true,
            cost_attribution_min_delta_usd: 1.0,
            min_significance: None,
            regression_significance: None,
            allow_underpowered: false,
            flake_report: None,
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
            fork_lineage: None,
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

    #[allow(clippy::too_many_lines)]
    #[test]
    fn incomplete_results_json_falls_back_to_trajectory_scan() {
        use crate::trajectory::{FORMAT_VERSION, TrajectoryInfo};
        let dir = tempfile::tempdir().unwrap();
        let sweep = SweepResults {
            total: 1,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
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
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
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
                    ..Default::default()
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
                reproduced_from: None,
                merged_from: None,
                chaos_fail_every: 0,
                circuit_breaker: None,
                source: None,
                import_predictions_path: None,
                import_predictions_sha256: None,
            }),
            cost_limit_usd: None,
            instances: Vec::new(),
            rate_limit_events: None,

            total_fallbacks: 0,

            model_mix: BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,

            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
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
            fork_lineage: None,
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
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
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
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: crate::run::swebench::FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![submitted("a")],
            rate_limit_events: None,

            total_fallbacks: 0,

            model_mix: BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,

            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
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

    #[allow(clippy::too_many_lines)]
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
            fork_lineage: None,
        };
        std::fs::write(
            dir.path().join("old.traj.json"),
            serde_json::to_string_pretty(&traj).unwrap(),
        )
        .unwrap();
        let started = chrono::Utc::now() + chrono::Duration::seconds(10);
        let sweep = SweepResults {
            total: 1,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
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
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
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
                    ..Default::default()
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
                reproduced_from: None,
                merged_from: None,
                chaos_fail_every: 0,
                circuit_breaker: None,
                source: None,
                import_predictions_path: None,
                import_predictions_sha256: None,
            }),
            cost_limit_usd: None,
            instances: Vec::new(),
            rate_limit_events: None,

            total_fallbacks: 0,

            model_mix: BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,

            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
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
    #[allow(clippy::too_many_lines)]
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
            fork_lineage: None,
        };
        std::fs::write(
            dir.path().join("resume-old.traj.json"),
            serde_json::to_string_pretty(&traj).unwrap(),
        )
        .unwrap();
        let started = chrono::Utc::now() + chrono::Duration::seconds(10);
        let sweep = SweepResults {
            total: 1,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
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
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
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
                    ..Default::default()
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
                    argv: vec!["max".into()],
                },
                reproduced_from: None,
                merged_from: None,
                chaos_fail_every: 0,
                circuit_breaker: None,
                source: None,
                import_predictions_path: None,
                import_predictions_sha256: None,
            }),
            cost_limit_usd: None,
            instances: Vec::new(),
            rate_limit_events: None,

            total_fallbacks: 0,

            model_mix: BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,

            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
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
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
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
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![],
            rate_limit_events: Some(events),
            total_fallbacks: 0,
            model_mix: BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,

            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
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

    fn slot(
        instance_id: &str,
        run_index: u32,
        final_model: Option<&str>,
        fallback_count: Option<u32>,
    ) -> LoadedRunSlot {
        let mut r = submitted(instance_id);
        r.final_model = final_model.map(str::to_owned);
        r.fallback_count = fallback_count;
        LoadedRunSlot {
            instance_id: instance_id.into(),
            run_index,
            result: r,
        }
    }

    #[test]
    fn fallback_totals_from_slots_empty_returns_zeros() {
        let (total, mix) = fallback_totals_from_slots(&[]);
        assert_eq!(total, 0);
        assert!(mix.is_empty());
    }

    #[test]
    fn fallback_totals_from_slots_sums_fallback_counts_across_reruns() {
        let slots = vec![
            slot("a", 0, Some("model-x"), Some(1)),
            slot("a", 1, Some("model-y"), Some(2)),
            slot("b", 0, Some("model-x"), None),
        ];
        let (total, mix) = fallback_totals_from_slots(&slots);
        assert_eq!(total, 3, "should sum fallback counts from all slots");
        assert_eq!(mix["model-x"], 2, "model-x appears in slot a/0 and b/0");
        assert_eq!(mix["model-y"], 1, "model-y appears in slot a/1 only");
    }

    #[test]
    fn fallback_totals_from_slots_counts_each_rerun_slot_model_independently() {
        // When reruns use different models, model_mix must include all of them,
        // not just the first (winning) slot per instance.
        let slots = vec![
            slot("task-1", 0, Some("primary"), Some(0)),
            slot("task-1", 1, Some("secondary"), Some(1)),
        ];
        let (_, mix) = fallback_totals_from_slots(&slots);
        assert!(mix.contains_key("primary"), "primary should be counted");
        assert!(mix.contains_key("secondary"), "secondary should be counted");
        assert_eq!(mix["primary"] + mix["secondary"], 2);
    }

    #[test]
    fn fallback_totals_from_slots_slot_with_no_final_model_is_skipped_in_mix() {
        let slots = vec![
            slot("a", 0, None, Some(1)),
            slot("b", 0, Some("model-z"), Some(0)),
        ];
        let (total, mix) = fallback_totals_from_slots(&slots);
        assert_eq!(total, 1);
        assert_eq!(mix.len(), 1);
        assert_eq!(mix["model-z"], 1);
    }

    fn snapshot_one_model(model: &str, n: usize, total_fallbacks: u64) -> ModelMixSnapshot {
        let mut mix = std::collections::BTreeMap::new();
        mix.insert(model.to_owned(), n);
        ModelMixSnapshot {
            model_mix: mix,
            total_fallbacks,
        }
    }

    #[test]
    fn build_model_mix_warnings_no_fallback_on_either_side_is_silent() {
        let b = ModelMixSnapshot {
            model_mix: std::collections::BTreeMap::new(),
            total_fallbacks: 0,
        };
        assert!(build_model_mix_warnings(&b, &b).is_empty());
    }

    #[test]
    fn build_model_mix_warnings_candidate_only_fallback_warns() {
        let base = ModelMixSnapshot {
            model_mix: std::collections::BTreeMap::new(),
            total_fallbacks: 0,
        };
        let cand = snapshot_one_model("secondary", 5, 5);
        let w = build_model_mix_warnings(&base, &cand);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("candidate sweep used model fallback"),
            "{}",
            w[0]
        );
    }

    #[test]
    fn build_model_mix_warnings_both_fallback_same_mix_same_rate_is_silent() {
        let snap = snapshot_one_model("secondary", 5, 10);
        assert!(build_model_mix_warnings(&snap, &snap).is_empty());
    }

    #[test]
    fn build_model_mix_warnings_both_fallback_different_rate_warns() {
        let b = snapshot_one_model("secondary", 5, 1);
        let c = snapshot_one_model("secondary", 5, 100);
        let w = build_model_mix_warnings(&b, &c);
        assert_eq!(
            w.len(),
            1,
            "expected exactly one warning for rate diff: {w:?}"
        );
        assert!(
            w[0].contains("different fallback attempt counts"),
            "warning should mention count diff: {}",
            w[0]
        );
        assert!(w[0].contains("baseline=1"), "{}", w[0]);
        assert!(w[0].contains("candidate=100"), "{}", w[0]);
    }

    #[test]
    fn build_model_mix_warnings_both_fallback_different_mix_and_rate_warns_twice() {
        let b = snapshot_one_model("primary", 8, 2);
        let c = snapshot_one_model("secondary", 8, 50);
        let w = build_model_mix_warnings(&b, &c);
        assert_eq!(
            w.len(),
            2,
            "expected warnings for both mix diff and rate diff: {w:?}"
        );
    }

    // --- Evaluator provenance comparison ---

    fn sb_prov(timeout: u64, parallel: usize) -> crate::run::evaluate::SbCliProvenance {
        crate::run::evaluate::SbCliProvenance {
            submit_command: None,
            report_command: None,
            report_paths: vec![],
            report_hashes: vec![],
            verify_submission: false,
            wait_for_evaluation: true,
            overwrite: true,
            timeout_per_instance_secs: timeout,
            parallel,
        }
    }

    fn eval_prov(
        backend: &str,
        sb_cli: Option<crate::run::evaluate::SbCliProvenance>,
    ) -> crate::run::evaluate::EvaluatorProvenance {
        crate::run::evaluate::EvaluatorProvenance {
            backend: backend.into(),
            backend_version: None,
            dataset_subset: Some("swe-bench-m".into()),
            dataset_split: Some("dev".into()),
            dataset_sha256: Some("default_sha256_value".into()),
            dataset_instance_count: Some(10),
            run_id: None,
            prediction_path: None,
            prediction_sha256: None,
            eval_started_at: None,
            eval_ended_at: None,
            report_source: None,
            sb_cli,
            docker_tests: None,
            source_reports: vec![],
        }
    }

    #[test]
    fn compare_provenance_sb_cli_both_none_sb_details_gives_matching() {
        // Both sides have sb-cli backend but neither has sb_cli details — (None, None) arm
        let b = eval_prov("sb-cli", None);
        let c = eval_prov("sb-cli", None);
        let (status, warnings) = compare_evaluator_provenance(Some(&b), Some(&c));
        assert_eq!(status, EvaluatorProvenanceStatus::Matching);
        assert!(warnings.is_empty());
    }

    #[test]
    fn compare_provenance_sb_cli_baseline_missing_sb_details_gives_mismatched() {
        // Baseline has sb_cli: None, candidate has Some — (None, Some) arm
        let b = eval_prov("sb-cli", None);
        let c = eval_prov("sb-cli", Some(sb_prov(300, 4)));
        let (status, warnings) = compare_evaluator_provenance(Some(&b), Some(&c));
        assert_eq!(status, EvaluatorProvenanceStatus::Mismatched);
        assert!(
            warnings.iter().any(|w| w.contains("baseline")),
            "expected baseline warning: {warnings:?}"
        );
    }

    #[test]
    fn compare_provenance_sb_cli_candidate_missing_sb_details_gives_mismatched() {
        // Baseline has Some, candidate has sb_cli: None — (Some, None) arm
        let b = eval_prov("sb-cli", Some(sb_prov(300, 4)));
        let c = eval_prov("sb-cli", None);
        let (status, warnings) = compare_evaluator_provenance(Some(&b), Some(&c));
        assert_eq!(status, EvaluatorProvenanceStatus::Mismatched);
        assert!(
            warnings.iter().any(|w| w.contains("candidate")),
            "expected candidate warning: {warnings:?}"
        );
    }

    #[test]
    fn write_evaluator_provenance_section_matching_label() {
        let mut s = String::new();
        write_evaluator_provenance_section(&mut s, EvaluatorProvenanceStatus::Matching, &[]);
        assert!(s.contains("matching"), "got: {s}");
    }

    #[test]
    fn write_evaluator_provenance_section_mismatched_with_warnings() {
        let mut s = String::new();
        write_evaluator_provenance_section(
            &mut s,
            EvaluatorProvenanceStatus::Mismatched,
            &["backend differs".into()],
        );
        assert!(s.contains("mismatched"));
        assert!(s.contains("backend differs"));
    }
}
