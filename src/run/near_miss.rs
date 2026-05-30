//! `bench near-miss`: rank unresolved sweep instances by gold-patch proximity.
//!
//! Zero-cost: reads only `evaluation.json` from the sweep dir. No model calls,
//! no network, no evaluator invocation.
//!
//! Unresolved instances are split into four buckets:
//! - **eligible**:    non-empty patch AND gold proximity fields present.
//! - **no_gold**:     non-empty patch but `gold_files_iou` is absent (dataset has no gold).
//! - **empty_patch**: `patch_stats.is_empty == true` (agent submitted nothing).
//! - **no_stats**:    `patch_stats` field is missing or null entirely.
//!
//! Eligible instances are ranked by:
//!   1. `gold_files_iou` desc  (primary)
//!   2. `gold_lines_overlap` desc  (secondary)
//!   3. `(gold_size_ratio - 1.0).abs()` asc  (tertiary — penalise shotgun/bloated)
//!   4. `instance_id` asc  (tiebreak for determinism)

use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactKind;
use crate::error::Error;
use crate::run::compare::load_evaluation_results;

// ── format ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NearMissFormat {
    Text,
    Json,
}

impl std::str::FromStr for NearMissFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!("unknown format `{other}` (expected text|json)")),
        }
    }
}

// ── args ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NearMissArgs {
    pub sweep: PathBuf,
    pub top: usize,
    pub format: NearMissFormat,
}

// ── data model ───────────────────────────────────────────────────────────────

/// Bucket counts for the four categories of unresolved instances.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NearMissBuckets {
    pub eligible: usize,
    pub no_gold: usize,
    pub empty_patch: usize,
    pub no_stats: usize,
}

/// One ranked row in the eligible bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearMissRow {
    pub rank: usize,
    pub instance_id: String,
    pub files_iou: f32,
    pub lines_overlap: f32,
    pub size_ratio: f32,
    pub failure_category: Option<String>,
    pub lines_changed: u32,
}

/// The complete near-miss report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearMissReport {
    pub buckets: NearMissBuckets,
    /// Top-N eligible instances ranked by gold proximity.
    pub ranked: Vec<NearMissRow>,
    pub sweep_path: String,
    pub generated_at: String,
}

// ── core logic ───────────────────────────────────────────────────────────────

fn eval_exit_reason_label(reason: &crate::run::evaluate::EvalExitReason) -> &'static str {
    use crate::run::evaluate::EvalExitReason;
    match reason {
        EvalExitReason::Resolved => "resolved",
        EvalExitReason::Unresolved => "unresolved",
        EvalExitReason::PatchApplyFailed => "patch_apply_failed",
        EvalExitReason::EvalError => "eval_error",
        EvalExitReason::SkippedNoPatch => "skipped_no_patch",
    }
}

/// Run the near-miss analysis over a completed sweep directory.
///
/// Errors:
/// - `Error::Trajectory` when `evaluation.json` is missing (caller maps to exit 2).
/// - `Error::Json` on parse failures (caller maps to exit 1).
pub fn run(args: &NearMissArgs) -> Result<NearMissReport, Error> {
    let eval = load_evaluation_results(&args.sweep)?.ok_or_else(|| {
        Error::Trajectory(format!(
            "near-miss: {} does not contain evaluation.json",
            args.sweep.display()
        ))
    })?;

    let mut eligible: Vec<NearMissRow> = Vec::new();
    let mut no_gold: usize = 0;
    let mut empty_patch: usize = 0;
    let mut no_stats: usize = 0;

    for inst in &eval.instances {
        if inst.resolved {
            continue;
        }
        let Some(stats) = &inst.patch_stats else {
            no_stats += 1;
            continue;
        };
        if stats.is_empty {
            empty_patch += 1;
            continue;
        }
        match (
            stats.gold_files_iou,
            stats.gold_lines_overlap,
            stats.gold_size_ratio,
        ) {
            (Some(iou), Some(overlap), Some(size_ratio)) => {
                eligible.push(NearMissRow {
                    rank: 0, // assigned after sort
                    instance_id: inst.instance_id.clone(),
                    files_iou: iou,
                    lines_overlap: overlap,
                    size_ratio,
                    failure_category: Some(
                        eval_exit_reason_label(&inst.eval_exit_reason).to_owned(),
                    ),
                    lines_changed: stats.lines_changed(),
                });
            }
            _ => {
                no_gold += 1;
            }
        }
    }

    // Sort: primary gold_files_iou desc, secondary gold_lines_overlap desc,
    // tertiary (size_ratio - 1.0).abs() asc, then instance_id asc.
    eligible.sort_by(|a, b| {
        b.files_iou
            .partial_cmp(&a.files_iou)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.lines_overlap
                    .partial_cmp(&a.lines_overlap)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                let da = (a.size_ratio - 1.0_f32).abs();
                let db = (b.size_ratio - 1.0_f32).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    let total_eligible = eligible.len();

    // Assign ranks and truncate to --top N.
    let mut ranked: Vec<NearMissRow> = eligible.into_iter().take(args.top).collect();
    for (i, row) in ranked.iter_mut().enumerate() {
        row.rank = i + 1;
    }

    Ok(NearMissReport {
        buckets: NearMissBuckets {
            eligible: total_eligible,
            no_gold,
            empty_patch,
            no_stats,
        },
        ranked,
        sweep_path: args.sweep.display().to_string(),
        generated_at: now_utc_iso8601(),
    })
}

fn now_utc_iso8601() -> String {
    // Integer UTC timestamp via std::time — no external crate needed.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let sec = secs % 60;
    let min = (secs / 60) % 60;
    let hour = (secs / 3600) % 24;
    let days = secs / 86400;
    // Civil calendar from days-since-epoch (Howarth 2001 / Howard Hinnant algorithm).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

// ── text rendering ────────────────────────────────────────────────────────────

/// Render the report as a human-readable text table.
pub fn render_text(report: &NearMissReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\n=== bench near-miss ===");
    let _ = writeln!(s, "Sweep:              {}", report.sweep_path);
    let b = &report.buckets;
    let _ = writeln!(
        s,
        "Buckets:            eligible: {}  no_gold: {}  empty_patch: {}  no_stats: {}",
        b.eligible, b.no_gold, b.empty_patch, b.no_stats
    );
    if report.ranked.is_empty() {
        let _ = writeln!(s, "\nNo eligible unresolved instances found.");
        return s;
    }
    let showing = report.ranked.len();
    let _ = writeln!(
        s,
        "\nTop {} of {} eligible (ranked by gold proximity):",
        showing, b.eligible
    );
    let _ = writeln!(
        s,
        "{:<6} {:<40} {:>10} {:>14} {:>12} {:>20} {:>14}",
        "rank",
        "instance_id",
        "files_iou",
        "lines_overlap",
        "size_ratio",
        "failure_category",
        "lines_changed"
    );
    let _ = writeln!(s, "{}", "-".repeat(120));
    for row in &report.ranked {
        let cat = row.failure_category.as_deref().unwrap_or("-");
        let _ = writeln!(
            s,
            "{:<6} {:<40} {:>10.4} {:>14.4} {:>12.4} {:>20} {:>14}",
            row.rank,
            row.instance_id,
            row.files_iou,
            row.lines_overlap,
            row.size_ratio,
            cat,
            row.lines_changed
        );
    }
    s
}

// ── JSON rendering ────────────────────────────────────────────────────────────

/// Render the report as a versioned JSON artifact.
pub fn render_json(report: &NearMissReport) -> Result<String, Error> {
    crate::artifact::to_string_pretty(ArtifactKind::NearMissReport, report).map_err(Error::Json)
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn format_parse_round_trips() {
        assert_eq!(
            "text".parse::<NearMissFormat>().unwrap(),
            NearMissFormat::Text
        );
        assert_eq!(
            "json".parse::<NearMissFormat>().unwrap(),
            NearMissFormat::Json
        );
        assert!("bad".parse::<NearMissFormat>().is_err());
    }

    #[test]
    fn now_utc_iso8601_looks_valid() {
        let ts = now_utc_iso8601();
        assert!(ts.ends_with('Z'), "should end with Z: {ts}");
        assert_eq!(ts.len(), 20, "should be 20 chars: {ts}");
    }
}
