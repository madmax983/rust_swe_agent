//! `bench cache-stats`: surface prompt-cache hit rate per sweep.
//!
//! Reads only on-disk artifacts (zero model calls, zero network).

#![allow(clippy::cast_precision_loss)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactKind;
use crate::cost::{
    ANTHROPIC_CACHE_CREATION_MULTIPLIER, ANTHROPIC_CACHE_READ_MULTIPLIER, SONNET_INPUT_USD_PER_MTOK,
};
use crate::error::Error;
use crate::run::compare::load_sweep;

// ── public calculation helpers (unit-testable) ────────────────────────────────

/// `cache_read / (input + cache_read + cache_creation)`; returns 0.0 for all-zero.
#[must_use]
pub fn compute_cache_hit_rate(input: u64, cache_read: u64, cache_creation: u64) -> f64 {
    let total = input
        .saturating_add(cache_read)
        .saturating_add(cache_creation);
    if total == 0 {
        return 0.0;
    }
    cache_read as f64 / total as f64
}

/// USD saved vs a hypothetical cold run where every cached read was fresh input.
#[must_use]
pub fn compute_estimated_savings_usd_vs_cold(cache_read: u64) -> f64 {
    cache_read as f64 / 1_000_000.0
        * SONNET_INPUT_USD_PER_MTOK
        * (1.0 - ANTHROPIC_CACHE_READ_MULTIPLIER)
}

/// Actual cost of cache operations (reads + creations).
#[must_use]
pub fn compute_realized_cache_spend_usd(cache_read: u64, cache_creation: u64) -> f64 {
    (cache_read as f64 / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK).mul_add(
        ANTHROPIC_CACHE_READ_MULTIPLIER,
        cache_creation as f64 / 1_000_000.0
            * SONNET_INPUT_USD_PER_MTOK
            * ANTHROPIC_CACHE_CREATION_MULTIPLIER,
    )
}

// ── public data types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CacheStatsArgs {
    pub sweep_dir: PathBuf,
    pub top: usize,
    pub baseline: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceCacheRow {
    pub instance_id: String,
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub cache_hit_rate: f64,
    pub estimated_savings_usd_vs_cold: f64,
    pub realized_cache_spend_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepCacheTotals {
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub cache_hit_rate: f64,
    pub estimated_savings_usd_vs_cold: f64,
    pub realized_cache_spend_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineDelta {
    pub baseline_sweep: String,
    pub delta_hit_rate: f64,
    pub delta_realized_spend_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStatsReport {
    pub sweep: String,
    pub generated_at: String,
    pub cache_disabled: bool,
    pub sweep_totals: SweepCacheTotals,
    /// Per-instance rows sorted by `cache_hit_rate` ascending (worst first),
    /// truncated to the `--top N` requested.
    pub instances: Vec<InstanceCacheRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineDelta>,
}

// ── public entry point ────────────────────────────────────────────────────────

pub fn run(args: &CacheStatsArgs) -> Result<CacheStatsReport, Error> {
    let report = build_report(args)?;
    write_artifact(&args.sweep_dir, &report)?;
    Ok(report)
}

// ── rendering ─────────────────────────────────────────────────────────────────

pub fn render_text(report: &CacheStatsReport, top: usize) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let mut out = String::new();
    let _ = writeln!(out, "\n=== bench cache-stats ===");
    let _ = writeln!(out, "Sweep: {}", report.sweep);

    if report.cache_disabled {
        let _ = writeln!(
            out,
            "cache disabled or unsupported by provider (no cache tokens recorded)"
        );
        return out;
    }

    let t = &report.sweep_totals;
    let _ = writeln!(out);
    let _ = writeln!(out, "Sweep-level cache summary:");
    let mut sweep_table = Table::new();
    sweep_table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "total_input_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
            "cache_hit_rate",
            "est_savings_usd",
            "realized_spend_usd",
        ])
        .add_row(vec![
            t.total_input_tokens.to_string(),
            t.total_cache_read_tokens.to_string(),
            t.total_cache_creation_tokens.to_string(),
            format!("{:.4}", t.cache_hit_rate),
            format!("{:.6}", t.estimated_savings_usd_vs_cold),
            format!("{:.6}", t.realized_cache_spend_usd),
        ]);
    let _ = writeln!(out, "{sweep_table}");

    if let Some(delta) = &report.baseline {
        let _ = writeln!(
            out,
            "Baseline: {}  Δ hit_rate={:+.4}  Δ realized_spend_usd={:+.6}",
            delta.baseline_sweep, delta.delta_hit_rate, delta.delta_realized_spend_usd
        );
        let _ = writeln!(out);
    }

    let display_top = top.min(report.instances.len());
    let _ = writeln!(
        out,
        "Per-instance breakdown (worst cache efficiency first, top {display_top}):"
    );
    let mut inst_table = Table::new();
    inst_table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "instance_id",
            "input_tokens",
            "cache_read",
            "cache_creation",
            "cache_hit_rate",
            "est_savings_usd",
            "realized_spend_usd",
        ]);
    for row in report.instances.iter().take(display_top) {
        inst_table.add_row(vec![
            row.instance_id.clone(),
            row.total_input_tokens.to_string(),
            row.total_cache_read_tokens.to_string(),
            row.total_cache_creation_tokens.to_string(),
            format!("{:.4}", row.cache_hit_rate),
            format!("{:.6}", row.estimated_savings_usd_vs_cold),
            format!("{:.6}", row.realized_cache_spend_usd),
        ]);
    }
    let _ = writeln!(out, "{inst_table}");
    out
}

// ── internals ─────────────────────────────────────────────────────────────────

fn build_report(args: &CacheStatsArgs) -> Result<CacheStatsReport, Error> {
    let sweep = load_sweep(&args.sweep_dir)?;

    let mut rows: Vec<InstanceCacheRow> = sweep
        .instances
        .values()
        .map(|inst| {
            let input = inst.prompt_tokens.unwrap_or(0);
            let reads = inst.cache_read_tokens.unwrap_or(0);
            let creation = inst.cache_creation_tokens.unwrap_or(0);
            InstanceCacheRow {
                instance_id: inst.instance_id.clone(),
                total_input_tokens: input,
                total_cache_read_tokens: reads,
                total_cache_creation_tokens: creation,
                cache_hit_rate: compute_cache_hit_rate(input, reads, creation),
                estimated_savings_usd_vs_cold: compute_estimated_savings_usd_vs_cold(reads),
                realized_cache_spend_usd: compute_realized_cache_spend_usd(reads, creation),
            }
        })
        .collect();

    // sort worst (lowest hit rate) first
    rows.sort_by(|a, b| {
        a.cache_hit_rate
            .partial_cmp(&b.cache_hit_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    let total_input: u64 = rows.iter().map(|r| r.total_input_tokens).sum();
    let total_reads: u64 = rows.iter().map(|r| r.total_cache_read_tokens).sum();
    let total_creation: u64 = rows.iter().map(|r| r.total_cache_creation_tokens).sum();

    let cache_disabled = total_reads == 0 && total_creation == 0;

    let sweep_totals = SweepCacheTotals {
        total_input_tokens: total_input,
        total_cache_read_tokens: total_reads,
        total_cache_creation_tokens: total_creation,
        cache_hit_rate: compute_cache_hit_rate(total_input, total_reads, total_creation),
        estimated_savings_usd_vs_cold: compute_estimated_savings_usd_vs_cold(total_reads),
        realized_cache_spend_usd: compute_realized_cache_spend_usd(total_reads, total_creation),
    };

    let baseline = args
        .baseline
        .as_deref()
        .map(|baseline_dir| build_baseline_delta(baseline_dir, &sweep_totals))
        .transpose()?;

    Ok(CacheStatsReport {
        sweep: args.sweep_dir.display().to_string(),
        generated_at: utc_now_iso8601(),
        cache_disabled,
        sweep_totals,
        instances: rows,
        baseline,
    })
}

fn build_baseline_delta(
    baseline_dir: &Path,
    current_totals: &SweepCacheTotals,
) -> Result<BaselineDelta, Error> {
    let baseline_sweep = load_sweep(baseline_dir)?;
    let b_input: u64 = baseline_sweep
        .instances
        .values()
        .map(|i| i.prompt_tokens.unwrap_or(0))
        .sum();
    let b_reads: u64 = baseline_sweep
        .instances
        .values()
        .map(|i| i.cache_read_tokens.unwrap_or(0))
        .sum();
    let b_creation: u64 = baseline_sweep
        .instances
        .values()
        .map(|i| i.cache_creation_tokens.unwrap_or(0))
        .sum();
    let b_hit_rate = compute_cache_hit_rate(b_input, b_reads, b_creation);
    let b_spend = compute_realized_cache_spend_usd(b_reads, b_creation);
    Ok(BaselineDelta {
        baseline_sweep: baseline_dir.display().to_string(),
        delta_hit_rate: current_totals.cache_hit_rate - b_hit_rate,
        delta_realized_spend_usd: current_totals.realized_cache_spend_usd - b_spend,
    })
}

fn write_artifact(sweep_dir: &Path, report: &CacheStatsReport) -> Result<(), Error> {
    let path = sweep_dir.join("cache-stats.json");
    let file = std::fs::File::create(&path)?;
    crate::artifact::to_writer_pretty(file, ArtifactKind::CacheStatsReport, report)?;
    Ok(())
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
