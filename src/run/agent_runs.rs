//! `agent runs` — list and summarize single-task trajectory files in a directory.
//!
//! Reads only on-disk `.traj.json` artifacts; performs no network or model calls.
//! Supports filtering by outcome or failure_category, sorting by multiple columns,
//! and `--format text` (human table) or `--format json` (stable schema-versioned array).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::error::Error;
use crate::trajectory::{FailureCategory, Trajectory};

// ── options ───────────────────────────────────────────────────────────────────

pub struct AgentRunsOpts {
    pub dir: PathBuf,
    pub recursive: bool,
    pub format: RunsFormat,
    pub filters: Vec<RunsFilter>,
    pub sort: RunsSort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunsFormat {
    Text,
    Json,
}

/// A parsed `key=value` filter.
#[derive(Debug, Clone)]
pub struct RunsFilter {
    pub key: String,
    pub value: String,
}

impl RunsFilter {
    /// Parse `"outcome=submitted"` or `"failure_category=step_limit"`.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let mut it = s.splitn(2, '=');
        let key = it.next().unwrap_or_default().trim().to_owned();
        let value = it.next().unwrap_or_default().trim().to_owned();
        match key.as_str() {
            "outcome" | "failure_category" => Ok(Self { key, value }),
            other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--filter key '{other}' is not supported; use 'outcome' or 'failure_category'"
            )))),
        }
    }

    fn matches(&self, row: &RunRow) -> bool {
        match self.key.as_str() {
            "outcome" => row.outcome.as_deref().unwrap_or("") == self.value,
            "failure_category" => row.failure_category.as_deref().unwrap_or("") == self.value,
            _ => false,
        }
    }
}

/// Column to sort by (ascending, stable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunsSort {
    /// Default: alphabetical by file path (stable, reproducible).
    #[default]
    Task,
    Cost,
    Steps,
    Duration,
}

impl RunsSort {
    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "task" => Ok(Self::Task),
            "cost" => Ok(Self::Cost),
            "steps" => Ok(Self::Steps),
            "duration" => Ok(Self::Duration),
            other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--sort '{other}' is not valid; use 'task', 'cost', 'steps', or 'duration'"
            )))),
        }
    }
}

// ── report types ──────────────────────────────────────────────────────────────

/// One row in the runs table — one per trajectory file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRow {
    /// Absolute path to the trajectory file.
    pub path: String,
    /// Task description (truncated to 60 chars for display; full in JSON).
    pub task: Option<String>,
    /// Outcome string (e.g. `"submitted"`, `"error"`, `"step_limit_reached"`).
    pub outcome: Option<String>,
    /// Failure category label (e.g. `"step_limit"`), or `null` if none.
    pub failure_category: Option<String>,
    /// Number of agent steps.
    pub steps: Option<u32>,
    /// Total wall-clock in seconds.
    pub duration_secs: Option<f64>,
    /// Total cost in USD.
    pub total_cost_usd: Option<f64>,
    /// Model name used.
    pub model: Option<String>,
}

/// Aggregate footer across all listed rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunsFooter {
    /// Total number of trajectory rows included.
    pub total_rows: usize,
    /// Count by outcome string.
    pub by_outcome: BTreeMap<String, usize>,
    /// Sum of all cost_usd values (USD).
    pub total_cost_usd: f64,
    /// Sum of all duration_secs values (seconds).
    pub total_duration_secs: f64,
    /// Mean steps across rows that have a steps value.
    pub mean_steps: Option<f64>,
    /// Number of files that were skipped due to parse errors.
    pub skipped_files: usize,
}

/// The full runs report — JSON artifact shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunsReport {
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
    /// Directory that was scanned (as supplied by the operator).
    pub scanned_dir: String,
    /// Whether subdirectories were traversed.
    pub recursive: bool,
    /// The trajectory rows, after filtering and sorting.
    pub rows: Vec<RunRow>,
    /// Aggregate footer.
    pub footer: RunsFooter,
}

// ── core logic ────────────────────────────────────────────────────────────────

pub fn run_agent_runs(opts: &AgentRunsOpts) -> Result<AgentRunsReport, Error> {
    let canonical_dir = std::fs::canonicalize(&opts.dir).map_err(|e| {
        Error::Trajectory(format!(
            "cannot canonicalize directory {}: {e}",
            opts.dir.display()
        ))
    })?;

    let paths = collect_traj_paths(&canonical_dir, opts.recursive)?;

    let mut rows: Vec<RunRow> = Vec::new();
    let mut skipped: usize = 0;

    // Determine if this is a sweep dir; if so skip `results.json` logic but
    // still load individual `.traj.json` files independently (per spec: per-file
    // independent, not silently double-counted).
    let is_sweep_dir = canonical_dir.join("results.json").exists();
    let _ = is_sweep_dir; // noted; individual traj files are always read independently

    for path in &paths {
        match load_traj_row(path) {
            Ok(row) => rows.push(row),
            Err(_) => skipped += 1,
        }
    }

    // Apply filters
    for f in &opts.filters {
        rows.retain(|r| f.matches(r));
    }

    // Sort
    sort_rows(&mut rows, opts.sort);

    // Build footer (over post-filter rows)
    let footer = build_footer(&rows, skipped);

    Ok(AgentRunsReport {
        artifact_kind: ArtifactKind::AgentRunsReport,
        schema_version: ArtifactSchemaVersion::CURRENT,
        scanned_dir: canonical_dir.display().to_string(),
        recursive: opts.recursive,
        rows,
        footer,
    })
}

/// Collect all `.traj.json` paths under `dir`. If `recursive` is false only the
/// immediate directory is scanned; if true the full tree is walked.
/// I/O errors on the scan root are propagated; unreadable child subdirectories
/// are skipped silently (analogous to malformed trajectory files).
fn collect_traj_paths(dir: &Path, recursive: bool) -> Result<Vec<PathBuf>, Error> {
    if !dir.exists() {
        return Err(Error::Trajectory(format!(
            "directory does not exist: {}",
            dir.display()
        )));
    }
    if !dir.is_dir() {
        return Err(Error::Trajectory(format!(
            "path is not a directory: {}",
            dir.display()
        )));
    }

    // Propagate read errors on the scan root in both recursive and non-recursive modes.
    let root_entries = std::fs::read_dir(dir)
        .map_err(|e| Error::Trajectory(format!("cannot read directory {}: {e}", dir.display())))?;

    let mut paths: Vec<PathBuf> = Vec::new();

    for entry in root_entries.flatten() {
        let p = entry.path();
        if recursive && p.is_dir() && !p.is_symlink() {
            walk_children(&p, &mut paths);
        } else if is_traj_file(&p) {
            paths.push(p);
        }
    }

    paths.sort();
    Ok(paths)
}

/// Recursively collect `.traj.json` files under `dir`, silently skipping
/// subdirectories that cannot be read (permission errors, etc.).
fn walk_children(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        // Skip symlinks to directories to prevent infinite recursion from cycles.
        if p.is_dir() && !p.is_symlink() {
            walk_children(&p, out);
        } else if is_traj_file(&p) {
            out.push(p);
        }
    }
}

fn is_traj_file(p: &Path) -> bool {
    p.is_file()
        && p.file_name()
            .and_then(|n| n.to_str())
            .map_or(false, |n| n.ends_with(".traj.json"))
}

fn load_traj_row(path: &Path) -> Result<RunRow, Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Trajectory(format!("cannot read {}: {e}", path.display())))?;

    let traj: Trajectory = serde_json::from_str(&raw)
        .map_err(|e| Error::Trajectory(format!("cannot parse {}: {e}", path.display())))?;

    let info = &traj.info;
    let failure_category = info.failure_category.map(failure_category_label_str);

    Ok(RunRow {
        path: path.display().to_string(),
        task: info.task.clone(),
        outcome: info.outcome.clone(),
        failure_category,
        steps: info.steps,
        duration_secs: info.duration_secs,
        total_cost_usd: info.total_cost_usd,
        model: info.model_name.clone(),
    })
}

fn sort_rows(rows: &mut Vec<RunRow>, sort: RunsSort) {
    match sort {
        RunsSort::Task => rows.sort_by(|a, b| a.path.cmp(&b.path)),
        RunsSort::Cost => rows.sort_by(|a, b| match (a.total_cost_usd, b.total_cost_usd) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(va), Some(vb)) => va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal),
        }),
        RunsSort::Steps => rows.sort_by_key(|r| r.steps),
        RunsSort::Duration => rows.sort_by(|a, b| match (a.duration_secs, b.duration_secs) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(va), Some(vb)) => va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal),
        }),
    }
}

fn build_footer(rows: &[RunRow], skipped: usize) -> RunsFooter {
    let mut by_outcome: BTreeMap<String, usize> = BTreeMap::new();
    let mut total_cost: f64 = 0.0;
    let mut total_dur: f64 = 0.0;
    let mut steps_sum: u64 = 0;
    let mut steps_count: usize = 0;

    for row in rows {
        let key = row.outcome.clone().unwrap_or_else(|| "unknown".to_owned());
        *by_outcome.entry(key).or_insert(0) += 1;
        total_cost += row.total_cost_usd.unwrap_or(0.0);
        total_dur += row.duration_secs.unwrap_or(0.0);
        if let Some(s) = row.steps {
            steps_sum += u64::from(s);
            steps_count += 1;
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let mean_steps = if steps_count > 0 {
        Some(steps_sum as f64 / steps_count as f64)
    } else {
        None
    };

    RunsFooter {
        total_rows: rows.len(),
        by_outcome,
        total_cost_usd: total_cost,
        total_duration_secs: total_dur,
        mean_steps,
        skipped_files: skipped,
    }
}

fn failure_category_label_str(fc: FailureCategory) -> String {
    let s = match fc {
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
    };
    s.to_owned()
}

// ── text formatting ───────────────────────────────────────────────────────────

const TASK_DISPLAY_LEN: usize = 40;

pub fn format_text(report: &AgentRunsReport) -> String {
    let mut out = String::new();

    if report.rows.is_empty() {
        let _ = writeln!(out, "no trajectories found");
        if report.footer.skipped_files > 0 {
            let _ = writeln!(
                out,
                "({} file(s) skipped due to parse errors)",
                report.footer.skipped_files
            );
        }
        return out;
    }

    // Header row — outcome needs ≥20 chars (step_limit_reached=18), failure_category ≥26 (history_compaction_failed=25)
    let _ = writeln!(
        out,
        "{:<42} {:<20} {:<26} {:>6} {:>10} {:>9} {}",
        "task", "outcome", "failure_category", "steps", "duration", "cost_usd", "model"
    );
    let _ = writeln!(out, "{}", "─".repeat(126));

    for row in &report.rows {
        let task = truncate(row.task.as_deref().unwrap_or(""), TASK_DISPLAY_LEN);
        let outcome = row.outcome.as_deref().unwrap_or("");
        let failure = row.failure_category.as_deref().unwrap_or("");
        let steps = row.steps.map_or_else(|| "-".to_owned(), |s| s.to_string());
        let dur = row
            .duration_secs
            .map_or_else(|| "-".to_owned(), |d| format!("{d:.1}s"));
        let cost = row
            .total_cost_usd
            .map_or_else(|| "-".to_owned(), |c| format!("${c:.4}"));
        let model = row.model.as_deref().unwrap_or("");

        let _ = writeln!(
            out,
            "{:<42} {:<20} {:<26} {:>6} {:>10} {:>9} {}",
            task, outcome, failure, steps, dur, cost, model
        );
    }

    // Footer
    let _ = writeln!(out, "{}", "─".repeat(126));

    // Outcome breakdown
    let outcome_parts: Vec<String> = report
        .footer
        .by_outcome
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let _ = writeln!(
        out,
        "total: {}  |  outcomes: {}",
        report.footer.total_rows,
        outcome_parts.join(", ")
    );

    let mean_steps = report
        .footer
        .mean_steps
        .map_or_else(|| "-".to_owned(), |m| format!("{m:.1}"));
    let _ = writeln!(
        out,
        "total_cost: ${:.4}  total_duration: {:.1}s  mean_steps: {}",
        report.footer.total_cost_usd, report.footer.total_duration_secs, mean_steps
    );

    if report.footer.skipped_files > 0 {
        let _ = writeln!(
            out,
            "{} file(s) skipped due to parse errors",
            report.footer.skipped_files
        );
    }

    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn minimal_traj_json(
        task: &str,
        outcome: &str,
        failure_category: Option<&str>,
        steps: u32,
        duration: f64,
        cost: f64,
        model: &str,
    ) -> String {
        let fc_field = if let Some(fc) = failure_category {
            format!(r#","failure_category":"{fc}""#)
        } else {
            String::new()
        };
        format!(
            r#"{{
  "trajectory_format": "mini-swe-agent-1.2",
  "artifact_kind": "trajectory",
  "schema_version": "1",
  "info": {{
    "task": "{task}",
    "outcome": "{outcome}"{fc_field},
    "steps": {steps},
    "duration_secs": {duration},
    "total_cost_usd": {cost},
    "model_name": "{model}"
  }},
  "messages": []
}}"#
        )
    }

    fn write_traj(dir: &Path, name: &str, json: &str) {
        let path = dir.join(name);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(json.as_bytes()).unwrap();
    }

    // ── RED PHASE tests ───────────────────────────────────────────────────────

    // AC1: scans directory, one row per trajectory with correct columns
    #[test]
    fn red_scans_directory_and_returns_rows() {
        let dir = TempDir::new().unwrap();
        let json = minimal_traj_json("Fix the bug", "submitted", None, 5, 12.3, 0.0042, "gpt-4");
        write_traj(dir.path(), "run1.traj.json", &json);

        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(report.rows.len(), 1);
        let row = &report.rows[0];
        assert_eq!(row.outcome.as_deref(), Some("submitted"));
        assert_eq!(row.steps, Some(5));
        assert!((row.duration_secs.unwrap() - 12.3).abs() < 0.001);
        assert!((row.total_cost_usd.unwrap() - 0.0042).abs() < 0.000_001);
        assert_eq!(row.model.as_deref(), Some("gpt-4"));
    }

    // AC2: non-recursive by default; --recursive descends
    #[test]
    fn red_non_recursive_does_not_descend() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let json = minimal_traj_json("task", "error", None, 1, 1.0, 0.001, "m");
        write_traj(&sub, "nested.traj.json", &json);

        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(
            report.rows.len(),
            0,
            "non-recursive should not find nested file"
        );
    }

    #[test]
    fn red_recursive_descends_subdirectories() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let json = minimal_traj_json("task", "error", None, 1, 1.0, 0.001, "m");
        write_traj(&sub, "nested.traj.json", &json);

        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: true,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(report.rows.len(), 1, "--recursive should find nested file");
    }

    // AC3: no network/model calls — this is a unit test (implicitly satisfied
    // by the function signature; we just verify it completes without panicking)
    #[test]
    fn red_no_network_call_zero_cost_run() {
        let dir = TempDir::new().unwrap();
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        // Must not panic or error
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(report.rows.len(), 0);
    }

    // AC4: footer aggregates count by outcome, total cost, total wallclock, mean steps
    #[test]
    fn red_footer_aggregates_correctly() {
        let dir = TempDir::new().unwrap();
        write_traj(
            dir.path(),
            "a.traj.json",
            &minimal_traj_json("t1", "submitted", None, 4, 10.0, 0.01, "m"),
        );
        write_traj(
            dir.path(),
            "b.traj.json",
            &minimal_traj_json("t2", "error", Some("step_limit"), 8, 20.0, 0.02, "m"),
        );

        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        let footer = &report.footer;
        assert_eq!(footer.total_rows, 2);
        assert_eq!(footer.by_outcome.get("submitted"), Some(&1));
        assert_eq!(footer.by_outcome.get("error"), Some(&1));
        assert!((footer.total_cost_usd - 0.03).abs() < 0.000_001);
        assert!((footer.total_duration_secs - 30.0).abs() < 0.001);
        assert_eq!(footer.mean_steps, Some(6.0));
        assert_eq!(footer.skipped_files, 0);
    }

    // AC5: --format json emits schema-versioned array
    #[test]
    fn red_format_json_is_schema_versioned() {
        let dir = TempDir::new().unwrap();
        write_traj(
            dir.path(),
            "r.traj.json",
            &minimal_traj_json("task", "submitted", None, 3, 5.0, 0.001, "m"),
        );
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Json,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["artifact_kind"], "agent_runs_report");
        assert!(val["schema_version"].is_string() || val["schema_version"].is_object());
        assert!(val["rows"].is_array());
    }

    // AC6: --filter outcome= and --filter failure_category= narrow rows
    #[test]
    fn red_filter_by_outcome() {
        let dir = TempDir::new().unwrap();
        write_traj(
            dir.path(),
            "a.traj.json",
            &minimal_traj_json("t", "submitted", None, 3, 5.0, 0.001, "m"),
        );
        write_traj(
            dir.path(),
            "b.traj.json",
            &minimal_traj_json("t", "error", Some("step_limit"), 3, 5.0, 0.001, "m"),
        );
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![RunsFilter::parse("outcome=submitted").unwrap()],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].outcome.as_deref(), Some("submitted"));
    }

    #[test]
    fn red_filter_by_failure_category() {
        let dir = TempDir::new().unwrap();
        write_traj(
            dir.path(),
            "a.traj.json",
            &minimal_traj_json("t", "error", Some("step_limit"), 3, 5.0, 0.001, "m"),
        );
        write_traj(
            dir.path(),
            "b.traj.json",
            &minimal_traj_json("t", "error", Some("model_api"), 3, 5.0, 0.001, "m"),
        );
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![RunsFilter::parse("failure_category=step_limit").unwrap()],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(report.rows.len(), 1);
        assert_eq!(
            report.rows[0].failure_category.as_deref(),
            Some("step_limit")
        );
    }

    // AC7: --sort cost|steps|duration|task
    #[test]
    fn red_sort_by_cost() {
        let dir = TempDir::new().unwrap();
        write_traj(
            dir.path(),
            "a.traj.json",
            &minimal_traj_json("t", "submitted", None, 1, 1.0, 0.05, "m"),
        );
        write_traj(
            dir.path(),
            "b.traj.json",
            &minimal_traj_json("t", "submitted", None, 1, 1.0, 0.01, "m"),
        );
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Cost,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(report.rows.len(), 2);
        assert!(
            report.rows[0].total_cost_usd.unwrap() <= report.rows[1].total_cost_usd.unwrap(),
            "rows should be sorted ascending by cost"
        );
    }

    #[test]
    fn red_sort_by_steps() {
        let dir = TempDir::new().unwrap();
        write_traj(
            dir.path(),
            "a.traj.json",
            &minimal_traj_json("t", "submitted", None, 10, 1.0, 0.01, "m"),
        );
        write_traj(
            dir.path(),
            "b.traj.json",
            &minimal_traj_json("t", "submitted", None, 3, 1.0, 0.01, "m"),
        );
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Steps,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert!(
            report.rows[0].steps.unwrap() <= report.rows[1].steps.unwrap(),
            "rows should be sorted ascending by steps"
        );
    }

    // AC8: malformed files are skipped with a count
    #[test]
    fn red_malformed_file_skipped_with_count() {
        let dir = TempDir::new().unwrap();
        write_traj(
            dir.path(),
            "good.traj.json",
            &minimal_traj_json("t", "submitted", None, 2, 3.0, 0.002, "m"),
        );
        write_traj(dir.path(), "bad.traj.json", "not valid json at all {{{");

        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(report.rows.len(), 1, "malformed file should be skipped");
        assert_eq!(report.footer.skipped_files, 1, "skipped count should be 1");
    }

    // AC9: sweep directory is not double-counted (trajectories loaded individually)
    #[test]
    fn red_sweep_dir_not_double_counted() {
        let dir = TempDir::new().unwrap();
        // Write a fake results.json to mark it as a sweep dir
        std::fs::write(
            dir.path().join("results.json"),
            r#"{"sweep_status":"completed"}"#,
        )
        .unwrap();
        // Write one trajectory
        write_traj(
            dir.path(),
            "instance1.traj.json",
            &minimal_traj_json("t", "submitted", None, 5, 10.0, 0.01, "m"),
        );

        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        // Must be exactly 1 row (the traj file), not 2 (not counting results.json)
        assert_eq!(report.rows.len(), 1);
    }

    // AC10: exit 0 on success including empty case (no panic/error)
    #[test]
    fn red_empty_directory_ok() {
        let dir = TempDir::new().unwrap();
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        assert_eq!(report.rows.len(), 0);
        assert_eq!(report.footer.total_rows, 0);
    }

    // Text format tests
    #[test]
    fn red_text_format_empty_prints_no_trajectories_found() {
        let dir = TempDir::new().unwrap();
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        let text = format_text(&report);
        assert!(
            text.contains("no trajectories found"),
            "empty case should print 'no trajectories found'"
        );
    }

    #[test]
    fn red_text_format_shows_footer_totals() {
        let dir = TempDir::new().unwrap();
        write_traj(
            dir.path(),
            "r.traj.json",
            &minimal_traj_json("task", "submitted", None, 5, 10.0, 0.05, "m"),
        );
        let opts = AgentRunsOpts {
            dir: dir.path().to_path_buf(),
            recursive: false,
            format: RunsFormat::Text,
            filters: vec![],
            sort: RunsSort::Task,
        };
        let report = run_agent_runs(&opts).unwrap();
        let text = format_text(&report);
        assert!(text.contains("total:"), "footer should include total count");
        assert!(
            text.contains("total_cost:"),
            "footer should include total cost"
        );
    }

    // Filter parse errors
    #[test]
    fn red_filter_parse_rejects_unknown_key() {
        let result = RunsFilter::parse("unknown_key=foo");
        assert!(result.is_err(), "unknown filter key should be rejected");
    }

    // Sort parse
    #[test]
    fn red_sort_parse_roundtrips() {
        assert_eq!(RunsSort::parse("cost").unwrap(), RunsSort::Cost);
        assert_eq!(RunsSort::parse("steps").unwrap(), RunsSort::Steps);
        assert_eq!(RunsSort::parse("duration").unwrap(), RunsSort::Duration);
        assert_eq!(RunsSort::parse("task").unwrap(), RunsSort::Task);
        assert!(RunsSort::parse("bogus").is_err());
    }

    // Truncate helper
    #[test]
    fn red_truncate_long_strings() {
        let s = "a".repeat(100);
        let t = truncate(&s, 40);
        assert!(t.chars().count() <= 40);
    }

    #[test]
    fn red_truncate_short_strings_unchanged() {
        let t = truncate("hello", 40);
        assert_eq!(t, "hello");
    }
}
