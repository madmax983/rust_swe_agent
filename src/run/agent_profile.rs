//! `agent profile` — profile a single trajectory file for cost, tokens, time, and action mix.
//!
//! Reads a standalone `.traj.json` without requiring a sweep directory or `results.json`.
//! Reports outcome, cost, token splits, per-stage wall-clock, and action-class breakdown.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::error::Error;
use crate::run::behavior::classify_turn;
use crate::trajectory::{FailureCategory, Trajectory};

const ALL_CLASS_NAMES: &[&str] = &[
    "test", "write", "build", "search", "read", "nav", "git", "other", "noop",
];

// ── public opts ───────────────────────────────────────────────────────────────

pub struct AgentProfileOpts {
    pub trajectory_path: PathBuf,
    pub format: ProfileFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileFormat {
    Text,
    Json,
}

// ── report types ──────────────────────────────────────────────────────────────

/// Per-stage wall-clock breakdown for one run.
/// `None` means the stage has no measurements in the trajectory (renders as "unknown").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageBreakdown {
    /// Total model-provider wall-clock (ms). `null` when no turns measured it.
    pub model_ms: Option<u64>,
    /// Total tool/bash execution wall-clock (ms). `null` when no turns measured it.
    pub tool_ms: Option<u64>,
    /// Total harness overhead wall-clock (ms). `null` when no turns measured it.
    pub harness_ms: Option<u64>,
    /// Model share of total `duration_secs`, 0–100 (whole-percent). `null` when unmeasured.
    pub model_pct: Option<u8>,
    /// Tool share of total `duration_secs`, 0–100 (whole-percent). `null` when unmeasured.
    pub tool_pct: Option<u8>,
    /// Harness share of total `duration_secs`, 0–100 (whole-percent). `null` when unmeasured.
    pub harness_pct: Option<u8>,
}

/// Per-class action-mix entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionMixEntry {
    pub count: usize,
    pub share_pct: f64,
}

/// Token usage with all four splits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileTokenUsage {
    pub prompt_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub completion_tokens: u64,
}

/// The full profile report — also the JSON artifact shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfileReport {
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
    /// Source trajectory file path (as supplied by the operator).
    pub trajectory_path: String,
    /// Final outcome string from `info.outcome` (e.g. `"submitted"`, `"error"`).
    pub outcome: Option<String>,
    /// `failure_category` from `info`, if present.
    pub failure_category: Option<FailureCategory>,
    /// Total steps from `info.steps`.
    pub steps: Option<u32>,
    /// Total cost USD from `info.total_cost_usd`.
    pub total_cost_usd: f64,
    /// Token usage with all four splits.
    pub token_usage: ProfileTokenUsage,
    /// Total wall-clock from `info.duration_secs`.
    pub duration_secs: Option<f64>,
    /// Per-stage wall-clock breakdown aggregated from `MessageExtra` latency fields.
    pub stage_breakdown: StageBreakdown,
    /// Per-class action mix over all tool/bash turns.
    pub action_mix: BTreeMap<String, ActionMixEntry>,
}

// ── core logic ────────────────────────────────────────────────────────────────

pub fn run_agent_profile(opts: &AgentProfileOpts) -> Result<AgentProfileReport, Error> {
    let raw = std::fs::read_to_string(&opts.trajectory_path).map_err(|e| {
        Error::Trajectory(format!(
            "cannot read {}: {e}",
            opts.trajectory_path.display()
        ))
    })?;

    let traj: Trajectory = serde_json::from_str(&raw).map_err(|e| {
        Error::Trajectory(format!(
            "cannot parse {}: {e}",
            opts.trajectory_path.display()
        ))
    })?;

    let info = &traj.info;

    // ── token usage ──────────────────────────────────────────────────────────
    let token_usage = info.token_usage.as_ref().map_or(
        ProfileTokenUsage {
            prompt_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            completion_tokens: 0,
        },
        |tu| ProfileTokenUsage {
            prompt_tokens: tu.prompt_tokens,
            cache_read_tokens: tu.cache_read_tokens,
            cache_creation_tokens: tu.cache_creation_tokens,
            completion_tokens: tu.completion_tokens,
        },
    );

    // ── stage latency aggregation ────────────────────────────────────────────
    let mut model_ms_total: Option<u64> = None;
    let mut tool_ms_total: Option<u64> = None;
    let mut harness_ms_total: Option<u64> = None;

    // ── action-class counting ────────────────────────────────────────────────
    let mut class_counts: BTreeMap<String, usize> = BTreeMap::new();
    for name in ALL_CLASS_NAMES {
        class_counts.insert((*name).to_owned(), 0);
    }
    let mut total_classified: usize = 0;

    for msg in &traj.messages {
        let extra = &msg.extra;

        // Accumulate latency (only from fields that are present)
        if let Some(ms) = extra.model_latency_ms {
            *model_ms_total.get_or_insert(0) += ms;
        }
        if let Some(ms) = extra.tool_latency_ms {
            *tool_ms_total.get_or_insert(0) += ms;
        }
        if let Some(ms) = extra.harness_overhead_ms {
            *harness_ms_total.get_or_insert(0) += ms;
        }

        // Count action classes from assistant turns
        if msg.role == "assistant" {
            if let Some(actions) = &extra.actions {
                let action_refs: Vec<&str> = actions.iter().map(String::as_str).collect();
                let class = classify_turn(&action_refs);
                *class_counts.entry(class.as_str().to_owned()).or_insert(0) += 1;
                total_classified += 1;
            }
        }
    }

    // ── per-stage share of duration_secs ────────────────────────────────────
    let duration_ms = info.duration_secs.map(|s| (s * 1000.0) as u64);
    let model_pct = compute_pct(model_ms_total, duration_ms);
    let tool_pct = compute_pct(tool_ms_total, duration_ms);
    let harness_pct = compute_pct(harness_ms_total, duration_ms);

    let stage_breakdown = StageBreakdown {
        model_ms: model_ms_total,
        tool_ms: tool_ms_total,
        harness_ms: harness_ms_total,
        model_pct,
        tool_pct,
        harness_pct,
    };

    // ── action-mix shares ────────────────────────────────────────────────────
    let action_mix: BTreeMap<String, ActionMixEntry> = class_counts
        .into_iter()
        .map(|(name, count)| {
            let share_pct = if total_classified > 0 {
                (count as f64 / total_classified as f64) * 100.0
            } else {
                0.0
            };
            (name, ActionMixEntry { count, share_pct })
        })
        .collect();

    Ok(AgentProfileReport {
        artifact_kind: ArtifactKind::AgentProfileReport,
        schema_version: ArtifactSchemaVersion::CURRENT,
        trajectory_path: opts.trajectory_path.display().to_string(),
        outcome: info.outcome.clone(),
        failure_category: info.failure_category.clone(),
        steps: info.steps,
        total_cost_usd: info.total_cost_usd.unwrap_or(0.0),
        token_usage,
        duration_secs: info.duration_secs,
        stage_breakdown,
        action_mix,
    })
}

fn compute_pct(stage_ms: Option<u64>, total_ms: Option<u64>) -> Option<u8> {
    match (stage_ms, total_ms) {
        (Some(s), Some(t)) if t > 0 => Some(((s as f64 / t as f64) * 100.0).round() as u8),
        (Some(_), Some(0)) => Some(0),
        _ => None,
    }
}

// ── text formatting ───────────────────────────────────────────────────────────

pub fn format_text(report: &AgentProfileReport) -> String {
    let mut out = String::new();

    // Header
    let path = Path::new(&report.trajectory_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&report.trajectory_path);
    let _ = writeln!(out, "Agent Profile: {path}");
    let _ = writeln!(out, "{}", "─".repeat(60));

    // Run summary
    let _ = writeln!(
        out,
        "\n── Run Summary ──────────────────────────────────────────"
    );
    let outcome = report.outcome.as_deref().unwrap_or("unknown");
    let failure = report
        .failure_category
        .as_ref()
        .map(|c| format!(" ({})", failure_category_label(c)))
        .unwrap_or_default();
    let _ = writeln!(out, "  outcome:        {outcome}{failure}");
    let _ = writeln!(out, "  steps:          {}", report.steps.unwrap_or(0));
    let _ = writeln!(out, "  total_cost_usd: ${:.4}", report.total_cost_usd);
    let duration = report
        .duration_secs
        .map(|d| format!("{d:.1}s"))
        .unwrap_or_else(|| "unknown".to_owned());
    let _ = writeln!(out, "  duration:       {duration}");

    // Token usage
    let _ = writeln!(
        out,
        "\n── Token Usage ──────────────────────────────────────────"
    );
    let tu = &report.token_usage;
    let _ = writeln!(out, "  prompt:          {:>8}", tu.prompt_tokens);
    let _ = writeln!(out, "  cache-read:      {:>8}", tu.cache_read_tokens);
    let _ = writeln!(out, "  cache-creation:  {:>8}", tu.cache_creation_tokens);
    let _ = writeln!(out, "  completion:      {:>8}", tu.completion_tokens);
    let total =
        tu.prompt_tokens + tu.cache_read_tokens + tu.cache_creation_tokens + tu.completion_tokens;
    let _ = writeln!(out, "  ─────────────────────────");
    let _ = writeln!(out, "  total:           {:>8}", total);

    // Stage breakdown
    let _ = writeln!(
        out,
        "\n── Stage Breakdown ──────────────────────────────────────"
    );
    let sb = &report.stage_breakdown;
    let _ = writeln!(out, "  {:<10} {:>10}   {:>5}", "stage", "ms", "share");
    let _ = writeln!(out, "  {}", "─".repeat(30));
    let _ = writeln!(
        out,
        "  {:<10} {:>10}   {:>5}",
        "model",
        fmt_ms(sb.model_ms),
        fmt_pct(sb.model_pct)
    );
    let _ = writeln!(
        out,
        "  {:<10} {:>10}   {:>5}",
        "tool",
        fmt_ms(sb.tool_ms),
        fmt_pct(sb.tool_pct)
    );
    let _ = writeln!(
        out,
        "  {:<10} {:>10}   {:>5}",
        "harness",
        fmt_ms(sb.harness_ms),
        fmt_pct(sb.harness_pct)
    );

    // Action mix
    let _ = writeln!(
        out,
        "\n── Action Mix ───────────────────────────────────────────"
    );
    let _ = writeln!(out, "  {:<10} {:>6}   {:>6}", "class", "count", "share");
    let _ = writeln!(out, "  {}", "─".repeat(28));
    for (name, entry) in &report.action_mix {
        let _ = writeln!(
            out,
            "  {:<10} {:>6}   {:>5.1}%",
            name, entry.count, entry.share_pct
        );
    }

    out
}

fn fmt_ms(v: Option<u64>) -> String {
    v.map(|ms| format!("{ms}ms"))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn fmt_pct(v: Option<u8>) -> String {
    v.map(|p| format!("{p}%"))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn failure_category_label(fc: &FailureCategory) -> &'static str {
    match fc {
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
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::AgentStagnation => "agent_stagnation",
        FailureCategory::HistoryCompactionFailed => "history_compaction_failed",
        FailureCategory::ReadOnlyViolation => "read_only_violation",
        FailureCategory::Unknown => "unknown",
    }
}
