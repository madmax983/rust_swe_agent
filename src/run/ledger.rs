//! `bench ledger`: roll up cumulative actual spend across runs/sweeps.
//!
//! Reads only on-disk artifacts (zero model calls, zero network).

#![allow(clippy::cast_precision_loss)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactKind;
use crate::error::Error;
use crate::run::agent_runs::collect_traj_paths;
use crate::trajectory::{ResumeRecord, Trajectory};

// ── type aliases ──────────────────────────────────────────────────────────────

type GroupKey = (String, String);
type GroupEntry = (Option<f64>, String, String, String);

// ── public data types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LedgerArgs {
    pub dirs: Vec<PathBuf>,
    pub budget_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupSubtotal {
    pub key: String,
    pub total_cost_usd: f64,
    pub trajectory_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerReport {
    pub generated_at: String,
    pub roots: Vec<String>,
    pub grand_total_usd: f64,
    /// Number of logical runs (resume chains collapsed to one) that had a recorded cost.
    pub counted_trajectories: usize,
    /// Extra trajectory files absorbed into resume chains (chain members beyond the first).
    /// Invariant: discovered_trajectories == counted_trajectories + chained_trajectories + uncosted.
    pub chained_trajectories: usize,
    pub discovered_trajectories: usize,
    pub uncosted: usize,
    pub by_model: Vec<GroupSubtotal>,
    pub by_dataset: Vec<GroupSubtotal>,
    pub by_day: Vec<GroupSubtotal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over_budget: Option<bool>,
}

// ── public entry point ────────────────────────────────────────────────────────

pub fn run(args: &LedgerArgs) -> Result<LedgerReport, Error> {
    let report = build_report(args)?;
    if let Some(first_dir) = args.dirs.first() {
        if let Err(e) = write_artifact(first_dir, &report) {
            eprintln!(
                "warning: ledger: could not write ledger.json to {}: {e}",
                first_dir.display()
            );
        }
    }
    Ok(report)
}

// ── rendering ─────────────────────────────────────────────────────────────────

pub fn render_text(report: &LedgerReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\n=== bench ledger ===");
    let _ = writeln!(out, "Roots: {}", report.roots.join(", "));
    let _ = writeln!(out, "Generated: {}", report.generated_at);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Discovered: {}  Counted: {}  Chained: {}  Uncosted: {}",
        report.discovered_trajectories,
        report.counted_trajectories,
        report.chained_trajectories,
        report.uncosted
    );
    let _ = writeln!(out, "Grand total: ${:.6}", report.grand_total_usd);

    if let Some(budget) = report.budget_usd {
        let remaining = report.remaining_usd.unwrap_or(0.0);
        let over = report.over_budget.unwrap_or(false);
        let _ = writeln!(out, "Budget: ${budget:.6}");
        if over {
            let _ = writeln!(
                out,
                "OVER BUDGET by ${:.6}",
                report.grand_total_usd - budget
            );
        } else {
            let _ = writeln!(out, "Remaining: ${remaining:.6}");
        }
    }

    render_group_table(&mut out, "By model", &report.by_model);
    render_group_table(&mut out, "By dataset", &report.by_dataset);
    render_group_table(&mut out, "By day (UTC)", &report.by_day);

    let _ = writeln!(
        out,
        "\nNote: each grouping independently sums to the grand total."
    );

    out
}

fn render_group_table(out: &mut String, title: &str, groups: &[GroupSubtotal]) {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let _ = writeln!(out, "\n{title}:");
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["key", "total_cost_usd", "trajectory_count"]);
    for g in groups {
        table.add_row(vec![
            g.key.clone(),
            format!("{:.6}", g.total_cost_usd),
            g.trajectory_count.to_string(),
        ]);
    }
    let _ = writeln!(out, "{table}");
}

// ── pure helpers (unit-testable) ──────────────────────────────────────────────

/// Logical-run key: `(parent_dir_of_file, anchor_started_at)`.
///
/// `anchor_started_at` is the original start time of the logical run:
/// - for resumed trajectories: `resume_history[0].original_started_at`
/// - otherwise: `info.started_at`
///
/// When `started_at` is absent (legacy files) the key degrades to the file
/// path itself so each file is counted exactly once.
#[must_use]
pub fn logical_run_key(
    file_path: &Path,
    started_at: Option<&str>,
    resume_history: &[ResumeRecord],
) -> (String, String) {
    let parent = file_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let anchor = resume_history
        .first()
        .and_then(|r| r.original_started_at.as_deref())
        .or(started_at);

    let anchor_str = anchor.map_or_else(|| file_path.display().to_string(), ToOwned::to_owned);

    (parent, anchor_str)
}

/// Given a group of trajectory costs for the same logical run, select the
/// maximum (the deepest resumed trajectory already includes all prior spend).
/// Returns `None` if all entries are `None`.
#[must_use]
pub fn select_chain_cost(costs: &[Option<f64>]) -> Option<f64> {
    costs.iter().copied().flatten().reduce(f64::max)
}

/// Bucket an RFC3339 timestamp to a `YYYY-MM-DD` UTC date string.
/// Returns `"unknown"` for `None` or unparseable input.
#[must_use]
pub fn day_bucket(started_at: Option<&str>) -> String {
    started_at
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map_or_else(
            || "unknown".to_owned(),
            |dt| {
                dt.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d")
                    .to_string()
            },
        )
}

/// Extract a human-readable dataset label from the first ancestor directory
/// that contains a readable `results.json` with a `manifest.dataset`.
///
/// Uses `alias` when set, otherwise the basename of `path`.
/// Falls back to `"unknown"` when no suitable `results.json` is found.
#[must_use]
pub fn dataset_label_for<S: ::std::hash::BuildHasher>(
    traj_path: &Path,
    cache: &mut HashMap<PathBuf, String, S>,
) -> String {
    dataset_label_for_inner(traj_path, cache)
}

fn dataset_label_for_inner<S: ::std::hash::BuildHasher>(
    traj_path: &Path,
    cache: &mut HashMap<PathBuf, String, S>,
) -> String {
    // Walk from the trajectory's directory up to root, stopping at the first
    // ancestor that has a results.json we can parse.
    let start = traj_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    // Collect visited dirs so we can back-fill the cache once the label is known,
    // avoiding re-walks for siblings under the same sweep directory.
    let mut trail: Vec<PathBuf> = Vec::new();
    let mut current = start;

    let result = loop {
        if let Some(cached) = cache.get(&current) {
            let label = cached.clone();
            for dir in trail {
                cache.insert(dir, label.clone());
            }
            return label;
        }

        let results_path = current.join("results.json");
        if results_path.exists() {
            if let Some(label) = try_parse_dataset_label(&results_path) {
                trail.push(current);
                break label;
            }
        }

        match current.parent().map(Path::to_path_buf) {
            Some(parent) => {
                trail.push(current);
                current = parent;
            }
            None => break "unknown".to_owned(),
        }
    };

    for dir in trail {
        cache.insert(dir, result.clone());
    }
    result
}

fn try_parse_dataset_label(results_path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(results_path).ok()?;
    // Parse as a generic JSON Value to avoid failing on missing optional manifest sub-fields.
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let dataset = v.get("manifest")?.get("dataset")?;
    let alias = dataset
        .get("alias")
        .and_then(|a| a.as_str())
        .map(ToOwned::to_owned);
    if let Some(a) = alias {
        return Some(a);
    }
    // Fallback: basename of dataset.path
    let path_str = dataset.get("path")?.as_str()?;
    let label = Path::new(path_str)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_owned();
    Some(label)
}

/// Summarise a map of `key → (total_cost, count)` into a sorted `Vec<GroupSubtotal>`.
/// Sorted by `total_cost_usd` descending, then `key` ascending.
#[must_use]
pub fn group_subtotals<S: ::std::hash::BuildHasher>(
    map: &HashMap<String, (f64, usize), S>,
) -> Vec<GroupSubtotal> {
    let mut v: Vec<GroupSubtotal> = map
        .iter()
        .map(|(key, (cost, count))| GroupSubtotal {
            key: key.clone(),
            total_cost_usd: *cost,
            trajectory_count: *count,
        })
        .collect();
    v.sort_by(|a, b| {
        b.total_cost_usd
            .partial_cmp(&a.total_cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });
    v
}

// ── internals ─────────────────────────────────────────────────────────────────

struct TrajRecord {
    path: PathBuf,
    cost: Option<f64>,
    model: String,
    started_at: Option<String>,
    resume_history: Vec<ResumeRecord>,
}

fn build_report(args: &LedgerArgs) -> Result<LedgerReport, Error> {
    // Collect all trajectory paths from all root dirs (recursive).
    let mut all_paths: Vec<PathBuf> = Vec::new();
    for dir in &args.dirs {
        let paths = collect_traj_paths(dir, true)?;
        all_paths.extend(paths);
    }
    // Canonicalize before dedup so overlapping roots with different spellings
    // (relative vs absolute, symlinked) don't inflate counts.
    all_paths = all_paths
        .into_iter()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect();
    all_paths.sort();
    all_paths.dedup();

    let discovered = all_paths.len();

    // Load each trajectory.
    let mut records: Vec<TrajRecord> = Vec::with_capacity(all_paths.len());
    for path in &all_paths {
        match load_record(path) {
            Ok(r) => records.push(r),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "ledger: skipping unreadable trajectory");
                records.push(TrajRecord {
                    path: path.clone(),
                    cost: None,
                    model: "unknown".to_owned(),
                    started_at: None,
                    resume_history: Vec::new(),
                });
            }
        }
    }

    // Dataset label cache (memoized per ancestor dir).
    let mut dataset_cache: HashMap<PathBuf, String> = HashMap::new();

    // Group by logical-run key; collect costs per group.
    // Key → list of (cost, model, day, dataset).
    let mut groups: HashMap<GroupKey, Vec<GroupEntry>> = HashMap::new();

    for rec in &records {
        let key = logical_run_key(&rec.path, rec.started_at.as_deref(), &rec.resume_history);
        let model = rec.model.clone();
        let day = day_bucket(rec.started_at.as_deref());
        let dataset = dataset_label_for(&rec.path, &mut dataset_cache);
        groups
            .entry(key)
            .or_default()
            .push((rec.cost, model, day, dataset));
    }

    // For each logical run, select the max cost and record attribution.
    let mut grand_total: f64 = 0.0;
    let mut counted: usize = 0;
    let mut chained: usize = 0;
    let mut uncosted: usize = 0;
    let mut by_model: HashMap<String, (f64, usize)> = HashMap::new();
    let mut by_dataset: HashMap<String, (f64, usize)> = HashMap::new();
    let mut by_day: HashMap<String, (f64, usize)> = HashMap::new();

    for entries in groups.values() {
        // Find the entry with the highest cost. For a resume chain the deepest
        // resumed trajectory has the cumulative total, so using the max-cost
        // entry also gives us the correct attribution (model/day/dataset of the
        // continuation, not the original checkpoint).
        let best = entries
            .iter()
            .filter_map(|(c, m, d, ds)| c.map(|v| (v, m, d, ds)))
            .reduce(|acc, cur| if cur.0 >= acc.0 { cur } else { acc });

        match best {
            Some((cost, best_model, best_day, best_dataset)) => {
                grand_total += cost;
                counted += 1;
                // Files beyond the one "counted" run are chain members.
                chained += entries.len() - 1;
                let e = by_model.entry(best_model.clone()).or_insert((0.0, 0));
                e.0 += cost;
                e.1 += 1;
                let e = by_dataset.entry(best_dataset.clone()).or_insert((0.0, 0));
                e.0 += cost;
                e.1 += 1;
                let e = by_day.entry(best_day.clone()).or_insert((0.0, 0));
                e.0 += cost;
                e.1 += 1;
            }
            None => {
                uncosted += entries.len();
            }
        }
    }

    let (budget_usd, remaining_usd, over_budget) = if let Some(budget) = args.budget_usd {
        let remaining = (budget - grand_total).max(0.0);
        let over = grand_total >= budget;
        (Some(budget), Some(remaining), Some(over))
    } else {
        (None, None, None)
    };

    Ok(LedgerReport {
        generated_at: utc_now_iso8601(),
        roots: args.dirs.iter().map(|d| d.display().to_string()).collect(),
        grand_total_usd: grand_total,
        counted_trajectories: counted,
        chained_trajectories: chained,
        discovered_trajectories: discovered,
        uncosted,
        by_model: group_subtotals(&by_model),
        by_dataset: group_subtotals(&by_dataset),
        by_day: group_subtotals(&by_day),
        budget_usd,
        remaining_usd,
        over_budget,
    })
}

fn load_record(path: &Path) -> Result<TrajRecord, Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Trajectory(format!("cannot read {}: {e}", path.display())))?;
    let traj: Trajectory = serde_json::from_str(&raw)
        .map_err(|e| Error::Trajectory(format!("cannot parse {}: {e}", path.display())))?;
    let info = &traj.info;
    // Fall back to actual_cost_usd for interrupted checkpoints that were never
    // cleanly finalised (their total_cost_usd may be None while actual_cost_usd
    // already reflects the spend up to the interruption point).
    let cost = info.total_cost_usd.or(info.actual_cost_usd);
    Ok(TrajRecord {
        path: path.to_path_buf(),
        cost,
        model: info.model_name.as_deref().unwrap_or("unknown").to_owned(),
        started_at: info.started_at.clone(),
        resume_history: info.resume_history.clone(),
    })
}

fn write_artifact(dir: &Path, report: &LedgerReport) -> Result<(), Error> {
    let path = dir.join("ledger.json");
    let file = std::fs::File::create(&path)?;
    crate::artifact::to_writer_pretty(file, ArtifactKind::LedgerReport, report)?;
    Ok(())
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
