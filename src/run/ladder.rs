//! `bench ladder`: resolved-rate and cost trend across sweeps.
//!
//! Read-only walk of a local sweep root directory. Discovers every direct
//! subdirectory that contains a valid `results.json`, sorts by sweep
//! `start_timestamp`, and renders a chronological trend table in text,
//! JSON, or markdown format.
//!
//! Zero model calls, zero network; deterministic over fixed inputs (no
//! wallclock-of-now in any output field).

#![allow(clippy::cast_precision_loss)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactKind;
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::swebench::SweepResults;

// ── output format ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderFormat {
    Text,
    Json,
    Markdown,
}

impl std::str::FromStr for LadderFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "markdown" => Ok(Self::Markdown),
            other => Err(format!(
                "unknown format `{other}` (expected text|json|markdown)"
            )),
        }
    }
}

// ── public data types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LadderArgs {
    pub root: PathBuf,
    pub dataset: Option<String>,
    pub last: Option<usize>,
    pub baseline: Option<String>,
    pub format: LadderFormat,
}

/// One row in the ladder table, one per sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadderRow {
    /// Short directory name used as the sweep identifier.
    pub sweep_id: String,
    /// UTC start timestamp formatted as `YYYY-MM-DD HH:MM`.
    pub date: String,
    /// Model name from provenance manifest (redacted).
    pub model: String,
    /// First 8 hex chars of the prompt-template SHA256.
    pub prompt_sha: String,
    /// Total instance count for the sweep.
    pub n: usize,
    /// Resolved rate as a percentage (pass@k × 100).
    pub resolved_pct: f64,
    /// Cost per resolved instance in USD. `null` when resolved_count == 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_per_resolved: Option<f64>,
    /// Mean step count across all instances with recorded steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_steps: Option<f64>,
    /// Δ resolved_pct vs the previous row. `null` for the first row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_resolved_pct: Option<f64>,
    /// Δ resolved_pct vs the `--baseline` row. `null` when `--baseline` is not set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_vs_baseline: Option<f64>,
}

/// A sweep directory that could not be included in the ladder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedSweep {
    /// Directory name (basename only).
    pub dir: String,
    /// Human-readable reason for exclusion.
    pub reason: String,
}

/// Full ladder report: rows + skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadderReport {
    /// Absolute path of the scanned root directory.
    pub root: String,
    /// Validated, sorted, filtered sweep rows (chronological, oldest first).
    pub rows: Vec<LadderRow>,
    /// Sweeps that could not be included, with reasons.
    pub skipped: Vec<SkippedSweep>,
}

// ── public entry point ────────────────────────────────────────────────────────

pub fn run(args: &LadderArgs) -> Result<LadderReport, Error> {
    // Walk immediate children of root (I/O failure here exits non-zero per AC).
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&args.root)?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();

    // Sort directory list for deterministic processing order.
    entries.sort();

    let mut raw_sweeps: Vec<SweepData> = Vec::new();
    let mut skipped: Vec<SkippedSweep> = Vec::new();

    for dir in &entries {
        match try_load_sweep(dir) {
            Ok(data) => raw_sweeps.push(data),
            Err(reason) => skipped.push(SkippedSweep {
                dir: dir_name(dir),
                reason,
            }),
        }
    }

    // Skipped list is also deterministic: sort by dir name.
    skipped.sort_by(|a, b| a.dir.cmp(&b.dir));

    // Sort valid sweeps by start_timestamp ascending (ISO8601 lexicographic order).
    raw_sweeps.sort_by(|a, b| a.start_timestamp.cmp(&b.start_timestamp));

    // Apply --dataset filter before --last.
    if let Some(filter) = &args.dataset {
        raw_sweeps.retain(|s| dataset_matches(s, filter));
    }

    // Resolve baseline before --last truncation so a baseline outside the
    // last-N window still produces the delta_vs_baseline column.
    let baseline_pct: Option<f64> = args.baseline.as_deref().and_then(|baseline_id| {
        raw_sweeps
            .iter()
            .find(|s| s.dir_name == baseline_id)
            .map(SweepData::resolved_pct)
    });

    // Apply --last N: keep the N most recent (tail of the sorted list).
    if let Some(n) = args.last {
        if raw_sweeps.len() > n {
            raw_sweeps.drain(..raw_sweeps.len() - n);
        }
    }

    // Apply redaction to free-text provenance fields before building rows.
    let redactor = Redactor::default_enabled();

    let rows: Vec<LadderRow> = raw_sweeps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let resolved_pct = s.resolved_pct();
            let delta_resolved_pct = if i == 0 {
                None
            } else {
                Some(resolved_pct - raw_sweeps[i - 1].resolved_pct())
            };
            let delta_vs_baseline = baseline_pct.map(|b| resolved_pct - b);

            LadderRow {
                sweep_id: s.dir_name.clone(),
                date: format_date(&s.start_timestamp),
                model: redactor.redact_text(&s.model, surface::EXPORT).text,
                prompt_sha: redactor.redact_text(&s.prompt_sha, surface::EXPORT).text,
                n: s.n,
                resolved_pct,
                usd_per_resolved: s.usd_per_resolved(),
                mean_steps: s.mean_steps,
                delta_resolved_pct,
                delta_vs_baseline,
            }
        })
        .collect();

    Ok(LadderReport {
        root: args.root.display().to_string(),
        rows,
        skipped,
    })
}

// ── rendering ─────────────────────────────────────────────────────────────────

pub fn render_text(report: &LadderReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\n=== bench ladder ===");
    let _ = writeln!(out, "Root: {}", report.root);
    let _ = writeln!(
        out,
        "Sweeps: {}  Skipped: {}",
        report.rows.len(),
        report.skipped.len()
    );

    if !report.rows.is_empty() {
        let _ = writeln!(out);
        let has_baseline = report.rows.iter().any(|r| r.delta_vs_baseline.is_some());

        let mut headers: Vec<&str> = vec![
            "sweep_id",
            "date",
            "model",
            "prompt_sha",
            "n",
            "resolved%",
            "$/resolved",
            "mean_steps",
            "Δ resolved%",
        ];
        if has_baseline {
            headers.push("Δ vs baseline");
        }

        let mut table = crate::ui::create_table();
        table.set_header(headers);

        for row in &report.rows {
            let mut cells: Vec<String> = vec![
                row.sweep_id.clone(),
                row.date.clone(),
                row.model.clone(),
                row.prompt_sha.clone(),
                row.n.to_string(),
                format!("{:.2}%", row.resolved_pct),
                row.usd_per_resolved
                    .map_or_else(|| "—".into(), |v| format!("${v:.4}")),
                row.mean_steps
                    .map_or_else(|| "—".into(), |v| format!("{v:.1}")),
                row.delta_resolved_pct
                    .map_or_else(|| "—".into(), |d| format!("{d:+.2}pp")),
            ];
            if has_baseline {
                cells.push(
                    row.delta_vs_baseline
                        .map_or_else(|| "—".into(), |d| format!("{d:+.2}pp")),
                );
            }
            table.add_row(cells);
        }
        let _ = writeln!(out, "{table}");
    }

    if !report.skipped.is_empty() {
        let _ = writeln!(out, "Skipped ({}):", report.skipped.len());
        for s in &report.skipped {
            let _ = writeln!(out, "  {}: {}", s.dir, s.reason);
        }
    }

    out
}

pub fn render_markdown(report: &LadderReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## bench ladder");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Root: {}  Sweeps: {}  Skipped: {}",
        report.root,
        report.rows.len(),
        report.skipped.len()
    );
    let _ = writeln!(out);

    let has_baseline = report.rows.iter().any(|r| r.delta_vs_baseline.is_some());

    let mut headers: Vec<&str> = vec![
        "sweep_id",
        "date",
        "model",
        "prompt_sha",
        "n",
        "resolved%",
        "$/resolved",
        "mean_steps",
        "Δ resolved%",
    ];
    if has_baseline {
        headers.push("Δ vs baseline");
    }

    let _ = writeln!(out, "| {} |", headers.join(" | "));
    let _ = writeln!(out, "|{}|", vec!["---"; headers.len()].join("|"));

    for row in &report.rows {
        let mut cells: Vec<String> = vec![
            row.sweep_id.clone(),
            row.date.clone(),
            row.model.clone(),
            row.prompt_sha.clone(),
            row.n.to_string(),
            format!("{:.2}%", row.resolved_pct),
            row.usd_per_resolved
                .map_or_else(|| "—".into(), |v| format!("${v:.4}")),
            row.mean_steps
                .map_or_else(|| "—".into(), |v| format!("{v:.1}")),
            row.delta_resolved_pct
                .map_or_else(|| "—".into(), |d| format!("{d:+.2}pp")),
        ];
        if has_baseline {
            cells.push(
                row.delta_vs_baseline
                    .map_or_else(|| "—".into(), |d| format!("{d:+.2}pp")),
            );
        }
        let _ = writeln!(out, "| {} |", cells.join(" | "));
    }

    if !report.skipped.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "### Skipped ({})", report.skipped.len());
        let _ = writeln!(out);
        let _ = writeln!(out, "| dir | reason |");
        let _ = writeln!(out, "|---|---|");
        for s in &report.skipped {
            let _ = writeln!(out, "| {} | {} |", s.dir, s.reason);
        }
    }

    out
}

pub fn render_json(report: &LadderReport) -> Result<String, serde_json::Error> {
    crate::artifact::to_string_pretty(ArtifactKind::LadderReport, report)
}

// ── internals ─────────────────────────────────────────────────────────────────

struct SweepData {
    dir_name: String,
    start_timestamp: String,
    model: String,
    prompt_sha: String,
    dataset_path: String,
    dataset_alias: Option<String>,
    n: usize,
    total_resolved: usize,
    estimated_cost_usd: f64,
    mean_steps: Option<f64>,
}

impl SweepData {
    fn resolved_pct(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        self.total_resolved as f64 / self.n as f64 * 100.0
    }

    fn usd_per_resolved(&self) -> Option<f64> {
        if self.total_resolved == 0 {
            return None;
        }
        Some(self.estimated_cost_usd / self.total_resolved as f64)
    }
}

fn try_load_sweep(dir: &Path) -> Result<SweepData, String> {
    let results_path = dir.join("results.json");

    if !results_path.exists() {
        return Err("no results.json found".into());
    }

    let file = std::fs::File::open(&results_path).map_err(|e| format!("I/O error: {e}"))?;
    let reader = std::io::BufReader::new(file);
    let value: serde_json::Value =
        serde_json::from_reader(reader).map_err(|e| format!("JSON parse error: {e}"))?;

    // Validate artifact schema — kind mismatch, unknown future version, etc.
    // Use a short label (no full path) because the dir is already shown in `dir`.
    let _artifact = crate::artifact::classify_json_value(
        &value,
        crate::artifact::ArtifactKind::SweepResults,
        "results.json",
    )
    .map_err(|e| e.to_string())?;

    let sweep: SweepResults =
        serde_json::from_value(value).map_err(|e| format!("deserialization error: {e}"))?;

    let manifest = sweep
        .manifest
        .as_ref()
        .ok_or_else(|| "missing required provenance fields (manifest absent)".to_owned())?;

    let start_timestamp = manifest.runtime.started_at_utc.clone();
    let model = manifest.model.name.clone();
    let prompt_sha: String = manifest.prompt_template.sha256.chars().take(8).collect();
    let dataset_path = manifest.dataset.path.clone();
    let dataset_alias = manifest.dataset.alias.clone();

    let total_resolved: usize = sweep
        .instances
        .iter()
        .map(|i| i.resolved_count as usize)
        .sum();

    let step_values: Vec<f64> = sweep
        .instances
        .iter()
        .filter_map(|i| i.steps.map(f64::from))
        .collect();

    let mean_steps = if step_values.is_empty() {
        None
    } else {
        Some(step_values.iter().sum::<f64>() / step_values.len() as f64)
    };

    Ok(SweepData {
        dir_name: dir_name(dir),
        start_timestamp,
        model,
        prompt_sha,
        dataset_path,
        dataset_alias,
        n: sweep.total,
        total_resolved,
        estimated_cost_usd: sweep.estimated_cost_usd,
        mean_steps,
    })
}

fn dir_name(dir: &Path) -> String {
    dir.file_name().map_or_else(
        || dir.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn format_date(timestamp: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        let utc: chrono::DateTime<chrono::Utc> = dt.into();
        utc.format("%Y-%m-%d %H:%M").to_string()
    } else {
        timestamp.to_owned()
    }
}

fn dataset_matches(s: &SweepData, filter: &str) -> bool {
    // 1. Exact alias match
    if s.dataset_alias.as_deref() == Some(filter) {
        return true;
    }
    // 2. Full path match
    if s.dataset_path == filter {
        return true;
    }
    // 3. Basename with extension match (e.g., "swebench-lite.jsonl")
    let basename = Path::new(&s.dataset_path)
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    if basename == filter {
        return true;
    }
    // 4. Stem match without extension (e.g., "swebench-lite")
    let stem = Path::new(&s.dataset_path)
        .file_stem()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    if stem == filter {
        return true;
    }
    false
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_date_parses_rfc3339() {
        assert_eq!(format_date("2026-04-28T00:00:00Z"), "2026-04-28 00:00");
    }

    #[test]
    fn format_date_passthrough_on_invalid() {
        assert_eq!(format_date("not-a-date"), "not-a-date");
    }

    #[test]
    fn sweep_data_resolved_pct_zero_n() {
        let s = SweepData {
            dir_name: "x".into(),
            start_timestamp: "2026-01-01T00:00:00Z".into(),
            model: "m".into(),
            prompt_sha: "sha".into(),
            dataset_path: "d".into(),
            dataset_alias: None,
            n: 0,
            total_resolved: 0,
            estimated_cost_usd: 0.0,
            mean_steps: None,
        };
        assert!(s.resolved_pct() < f64::EPSILON);
    }

    #[test]
    fn sweep_data_usd_per_resolved_none_when_zero_resolved() {
        let s = SweepData {
            dir_name: "x".into(),
            start_timestamp: "2026-01-01T00:00:00Z".into(),
            model: "m".into(),
            prompt_sha: "sha".into(),
            dataset_path: "d".into(),
            dataset_alias: None,
            n: 4,
            total_resolved: 0,
            estimated_cost_usd: 0.10,
            mean_steps: None,
        };
        assert!(s.usd_per_resolved().is_none());
    }
}
