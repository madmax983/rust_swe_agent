//! `bench stagnation-report`: post-hoc cross-sweep stagnation halt aggregation.
//!
//! Zero-cost: reads only on-disk trajectory and results artifacts.
//! No model calls, no network.
//!
//! # USD-saved Estimate Formula
//!
//! `usd_saved_estimate = (step_limit − halt_step) × mean_per_step_usd`
//!
//! where `mean_per_step_usd = budget_burned_usd / halt_step`.
//! Conservative: assumes constant per-step cost equal to the observed mean
//! across the completed steps. All inputs appear in the JSON output.

#![allow(clippy::cast_precision_loss)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactKind;
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::compare::load_sweep;
use crate::stagnation::{action_hash, canonicalize_action};
use crate::trajectory::{FailureCategory, Trajectory};

// ── format ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagnationReportFormat {
    Text,
    Json,
}

impl std::str::FromStr for StagnationReportFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!("unknown format `{other}` (expected text|json)")),
        }
    }
}

// ── args ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StagnationReportArgs {
    pub sweep: PathBuf,
    pub format: StagnationReportFormat,
}

// ── data model ────────────────────────────────────────────────────────────────

const ACTION_DISPLAY_LIMIT: usize = 80;

/// Per-instance row in the stagnation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagnationInstanceRow {
    pub instance_id: String,
    /// First 8 hex chars of the SHA-256 canonical fingerprint.
    pub fingerprint: String,
    /// Canonical action text, truncated to 80 chars with "…" when longer.
    pub canonical_action: String,
    /// Number of occurrences (K) that triggered the halt.
    pub hit_count: u32,
    /// Step number at which the halt fired.
    pub halt_step: u32,
    /// USD cost burned before the halt.
    pub budget_burned_usd: f64,
    /// Estimated USD saved by the early halt. `null` when step_limit unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_saved_estimate: Option<f64>,
}

/// Cluster of instances sharing a canonical fingerprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagnationCluster {
    pub fingerprint: String,
    pub exemplar_action: String,
    pub instance_count: usize,
    pub total_usd_burned: f64,
    /// Up to 5 exemplar instance IDs from the cluster.
    pub exemplar_instance_ids: Vec<String>,
}

/// Sweep-level aggregate totals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagnationTotals {
    pub halted_count: usize,
    pub total_usd_burned_before_halt: f64,
    pub total_usd_saved_estimate: f64,
}

/// Full stagnation report returned by [`run`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagnationReport {
    pub sweep_path: String,
    /// Per-instance rows, ranked by `budget_burned_usd` descending.
    pub instances: Vec<StagnationInstanceRow>,
    /// Cluster view, ranked by `instance_count` descending.
    pub clusters: Vec<StagnationCluster>,
    pub totals: StagnationTotals,
}

// ── public API ────────────────────────────────────────────────────────────────

pub fn run(args: &StagnationReportArgs) -> Result<StagnationReport, Error> {
    let sweep = load_sweep(&args.sweep).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "stagnation-report: failed to load sweep `{}`: {e}",
            args.sweep.display()
        )))
    })?;

    // Try to determine step_limit from sweep manifest's resolved config TOML.
    let step_limit: Option<u32> = sweep
        .manifest
        .as_ref()
        .and_then(|m| parse_step_limit_from_config(&m.config.resolved));

    let redactor = Redactor::default_enabled();

    // Walk instances alphabetically; silently skip non-stagnation ones.
    let mut sorted_ids: Vec<String> = sweep.instances.keys().cloned().collect();
    sorted_ids.sort();

    let mut rows: Vec<StagnationInstanceRow> = Vec::new();
    for instance_id in &sorted_ids {
        let ir = &sweep.instances[instance_id];
        if !matches!(ir.failure_category, Some(FailureCategory::AgentStagnation)) {
            continue;
        }
        if let Some(row) = try_build_instance_row(instance_id, &args.sweep, step_limit, &redactor) {
            rows.push(row);
        }
    }

    // Rank by budget burned descending.
    rows.sort_by(|a, b| {
        b.budget_burned_usd
            .partial_cmp(&a.budget_burned_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let clusters = build_clusters(&rows);

    let halted_count = rows.len();
    let total_usd_burned_before_halt: f64 = rows.iter().map(|r| r.budget_burned_usd).sum();
    let total_usd_saved_estimate: f64 = rows.iter().filter_map(|r| r.usd_saved_estimate).sum();

    Ok(StagnationReport {
        sweep_path: args.sweep.display().to_string(),
        instances: rows,
        clusters,
        totals: StagnationTotals {
            halted_count,
            total_usd_burned_before_halt,
            total_usd_saved_estimate,
        },
    })
}

/// Render the report as a human-readable table (default output).
pub fn render_text(report: &StagnationReport) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let mut out = String::new();
    let _ = writeln!(out, "\n=== bench stagnation-report ===");
    let _ = writeln!(out, "Sweep: {}", report.sweep_path);
    let _ = writeln!(out, "Stagnation halts: {}", report.totals.halted_count);

    if !report.instances.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "── Per-instance table (ranked by USD burned) ──");

        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "instance_id",
                "fingerprint",
                "canonical_action",
                "K",
                "halt_step",
                "burned_usd",
                "saved_est_usd",
            ]);

        for row in &report.instances {
            table.add_row(vec![
                row.instance_id.clone(),
                row.fingerprint.clone(),
                row.canonical_action.clone(),
                row.hit_count.to_string(),
                row.halt_step.to_string(),
                format!("${:.4}", row.budget_burned_usd),
                row.usd_saved_estimate
                    .map_or_else(|| "—".into(), |v| format!("${v:.4}")),
            ]);
        }
        let _ = writeln!(out, "{table}");

        let _ = writeln!(out, "── Cluster view (ranked by instance count) ──");

        let mut ctable = Table::new();
        ctable
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "fingerprint",
                "exemplar_action",
                "instances",
                "total_burned_usd",
                "exemplar_ids",
            ]);
        for c in &report.clusters {
            ctable.add_row(vec![
                c.fingerprint.clone(),
                c.exemplar_action.clone(),
                c.instance_count.to_string(),
                format!("${:.4}", c.total_usd_burned),
                c.exemplar_instance_ids.join(", "),
            ]);
        }
        let _ = writeln!(out, "{ctable}");
    }

    let _ = writeln!(
        out,
        "Totals: {} halted  ${:.4} burned  ${:.4} estimated saved",
        report.totals.halted_count,
        report.totals.total_usd_burned_before_halt,
        report.totals.total_usd_saved_estimate,
    );
    out
}

/// Render the report as a schema-versioned JSON artifact.
pub fn render_json(report: &StagnationReport) -> Result<String, serde_json::Error> {
    crate::artifact::to_string_pretty(ArtifactKind::StagnationReport, report)
}

// ── internals ─────────────────────────────────────────────────────────────────

fn try_build_instance_row(
    instance_id: &str,
    sweep_dir: &Path,
    step_limit: Option<u32>,
    redactor: &Redactor,
) -> Option<StagnationInstanceRow> {
    let paths = resolve_trajectory_paths(sweep_dir, instance_id);
    let path = paths.last()?;

    let traj = load_trajectory(path)?;

    // Confirm the trajectory agrees on the failure category.
    if !matches!(
        traj.info.failure_category,
        Some(FailureCategory::AgentStagnation)
    ) {
        return None;
    }

    let stag_val = traj.info.other.get("stagnation")?;
    let stag: StagnationRecord = serde_json::from_value(stag_val.clone()).ok()?;

    let full_hash = &stag.action_hash; // 32-char hex
    let fingerprint: String = full_hash.chars().take(8).collect();

    let canonical_raw = find_canonical_action(&traj, full_hash)?;
    let canonical_redacted = redactor.redact_text(&canonical_raw, surface::EXPORT).text;
    let canonical_action = truncate_action(&canonical_redacted);

    let halt_step = traj
        .info
        .steps
        .unwrap_or_else(|| stag.step_indices.iter().copied().max().unwrap_or(0));

    let budget_burned_usd = traj
        .info
        .total_cost_usd
        .or(traj.info.actual_cost_usd)
        .unwrap_or(0.0);

    let usd_saved_estimate = compute_saved_estimate(budget_burned_usd, halt_step, step_limit);

    Some(StagnationInstanceRow {
        instance_id: instance_id.to_owned(),
        fingerprint,
        canonical_action,
        hit_count: stag.count,
        halt_step,
        budget_burned_usd,
        usd_saved_estimate,
    })
}

fn find_canonical_action(traj: &Trajectory, target_hash: &str) -> Option<String> {
    for msg in &traj.messages {
        if let Some(actions) = &msg.extra.actions {
            for action in actions {
                let canonical = canonicalize_action(action);
                if action_hash(&canonical) == target_hash {
                    return Some(canonical);
                }
            }
        }
    }
    None
}

fn compute_saved_estimate(
    budget_burned_usd: f64,
    halt_step: u32,
    step_limit: Option<u32>,
) -> Option<f64> {
    let limit = step_limit?;
    if halt_step == 0 || halt_step >= limit {
        return Some(0.0);
    }
    let mean_per_step = budget_burned_usd / f64::from(halt_step);
    let steps_saved = f64::from(limit - halt_step);
    Some(mean_per_step * steps_saved)
}

fn truncate_action(s: &str) -> String {
    let char_count = s.chars().count();
    if char_count <= ACTION_DISPLAY_LIMIT {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(ACTION_DISPLAY_LIMIT).collect();
        format!("{truncated}\u{2026}") // U+2026 HORIZONTAL ELLIPSIS
    }
}

fn build_clusters(rows: &[StagnationInstanceRow]) -> Vec<StagnationCluster> {
    // Map fingerprint → (exemplar_action, total_usd, exemplar_ids up to 5).
    // Rows are already ranked by budget_burned descending, so the first instance
    // encountered per fingerprint is the most expensive (good exemplar).
    let mut by_fp: HashMap<&str, (String, f64, Vec<String>)> = HashMap::new();
    for row in rows {
        let entry = by_fp
            .entry(row.fingerprint.as_str())
            .or_insert_with(|| (row.canonical_action.clone(), 0.0, Vec::new()));
        entry.1 += row.budget_burned_usd;
        if entry.2.len() < 5 {
            entry.2.push(row.instance_id.clone());
        }
    }

    let mut clusters: Vec<StagnationCluster> = by_fp
        .into_iter()
        .map(|(fp, (exemplar_action, total_usd, exemplar_ids))| {
            let instance_count = rows.iter().filter(|r| r.fingerprint == fp).count();
            StagnationCluster {
                fingerprint: fp.to_owned(),
                exemplar_action,
                instance_count,
                total_usd_burned: total_usd,
                exemplar_instance_ids: exemplar_ids,
            }
        })
        .collect();

    // Rank by instance_count descending, then fingerprint for determinism.
    clusters.sort_by(|a, b| {
        b.instance_count
            .cmp(&a.instance_count)
            .then_with(|| a.fingerprint.cmp(&b.fingerprint))
    });

    clusters
}

fn parse_step_limit_from_config(config_resolved: &str) -> Option<u32> {
    let re = regex::Regex::new(r"step_limit\s*=\s*(\d+)").ok()?;
    // Take the last match in case of multiple TOML sections overriding the value.
    re.captures_iter(config_resolved)
        .last()
        .and_then(|cap| cap[1].parse::<u32>().ok())
}

fn resolve_trajectory_paths(sweep: &Path, instance_id: &str) -> Vec<PathBuf> {
    // Legacy nested format.
    let nested = sweep.join(instance_id).join("trajectory.json");
    if nested.exists() {
        return vec![nested];
    }

    // New run-N.traj.json format in per-instance subdirectory.
    let instance_dir = sweep.join(instance_id);
    if instance_dir.is_dir() {
        let mut run_paths: Vec<PathBuf> = std::fs::read_dir(&instance_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("run-") && n.ends_with(".traj.json"))
            })
            .collect();
        run_paths.sort_by_key(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("run-"))
                .and_then(|n| n.strip_suffix(".traj.json"))
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        if !run_paths.is_empty() {
            return run_paths;
        }
    }

    // Flat format: <sweep>/<instance_id>.traj.json
    let flat = sweep.join(format!("{instance_id}.traj.json"));
    if flat.exists() {
        return vec![flat];
    }

    // Bundled/exported format.
    let bundled = sweep
        .join("trajectories")
        .join(format!("{instance_id}.traj.json"));
    if bundled.exists() {
        return vec![bundled];
    }

    vec![]
}

fn load_trajectory(path: &Path) -> Option<Trajectory> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Classify; silently skip on schema mismatch or unsupported future version.
    crate::artifact::classify_json_value(
        &value,
        ArtifactKind::Trajectory,
        path.display().to_string(),
    )
    .ok()?;
    serde_json::from_value(value).ok()
}

/// Deserialized stagnation record from `trajectory.info.other["stagnation"]`.
#[derive(Debug, Deserialize)]
struct StagnationRecord {
    action_hash: String,
    count: u32,
    #[serde(default)]
    step_indices: Vec<u32>,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn hash_of(raw: &str) -> String {
        action_hash(&canonicalize_action(raw))
    }

    fn results_json(instances: &[serde_json::Value]) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "total": instances.len(),
            "submitted": 0,
            "skipped": 0,
            "errored": instances.len(),
            "instances": instances,
        }))
        .unwrap()
    }

    fn stagnation_traj_json(
        action: &str,
        count: u32,
        step_indices: &[u32],
        halt_step: u32,
        cost_usd: f64,
    ) -> String {
        let full_hash = hash_of(action);
        serde_json::to_string_pretty(&serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.3",
            "info": {
                "failure_category": "agent_stagnation",
                "steps": halt_step,
                "total_cost_usd": cost_usd,
                "stagnation": {
                    "action_hash": full_hash,
                    "count": count,
                    "window": 8,
                    "step_indices": step_indices,
                }
            },
            "messages": [
                {
                    "role": "assistant",
                    "content": "running command",
                    "extra": {"actions": [action]}
                }
            ]
        }))
        .unwrap()
    }

    fn run_report(sweep: &Path) -> StagnationReport {
        run(&StagnationReportArgs {
            sweep: sweep.to_path_buf(),
            format: StagnationReportFormat::Text,
        })
        .expect("run should succeed")
    }

    // ── unit tests (pure functions) ───────────────────────────────────────────

    #[test]
    fn format_parses_text_and_json() {
        assert_eq!(
            "text".parse::<StagnationReportFormat>().unwrap(),
            StagnationReportFormat::Text
        );
        assert_eq!(
            "json".parse::<StagnationReportFormat>().unwrap(),
            StagnationReportFormat::Json
        );
        assert!("xml".parse::<StagnationReportFormat>().is_err());
    }

    #[test]
    fn truncate_short_action_unchanged() {
        assert_eq!(truncate_action("ls -la"), "ls -la");
    }

    #[test]
    fn truncate_exactly_80_chars_unchanged() {
        let s: String = "x".repeat(80);
        let result = truncate_action(&s);
        assert_eq!(result.chars().count(), 80);
        assert!(!result.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_81_chars_gets_ellipsis() {
        let s: String = "x".repeat(81);
        let result = truncate_action(&s);
        // 80 'x' chars + U+2026 = 81 display chars
        assert_eq!(result.chars().count(), 81);
        assert!(result.ends_with('\u{2026}'));
    }

    #[test]
    fn parse_step_limit_from_toml_config() {
        assert_eq!(
            parse_step_limit_from_config("[agent]\nstep_limit = 30\n"),
            Some(30)
        );
        assert_eq!(parse_step_limit_from_config("step_limit = 50"), Some(50));
        assert_eq!(parse_step_limit_from_config("no_limit_here"), None);
    }

    #[test]
    fn compute_saved_estimate_correct_formula() {
        // cost=0.016, halt_step=8, step_limit=50
        // mean_per_step = 0.016/8 = 0.002
        // saved = (50-8) * 0.002 = 42 * 0.002 = 0.084
        let estimate = compute_saved_estimate(0.016, 8, Some(50)).unwrap();
        assert!((estimate - 0.084).abs() < 1e-9);
    }

    #[test]
    fn compute_saved_estimate_none_when_no_step_limit() {
        assert!(compute_saved_estimate(0.016, 8, None).is_none());
    }

    #[test]
    fn compute_saved_estimate_zero_when_halt_equals_limit() {
        let estimate = compute_saved_estimate(0.016, 50, Some(50)).unwrap();
        assert!(estimate.abs() < 1e-9);
    }

    // ── integration tests (file I/O) ──────────────────────────────────────────

    #[test]
    fn empty_sweep_produces_well_formed_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(sweep.join("results.json"), results_json(&[])).unwrap();

        let report = run_report(sweep);

        assert_eq!(report.totals.halted_count, 0);
        assert!(report.instances.is_empty());
        assert!(report.clusters.is_empty());
        assert!(report.totals.total_usd_burned_before_halt.abs() < f64::EPSILON);
        assert!(report.totals.total_usd_saved_estimate.abs() < f64::EPSILON);
    }

    #[test]
    fn non_stagnation_instances_silently_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[
                serde_json::json!({
                    "instance_id": "step-limit-inst",
                    "exit_reason": "step_limit",
                    "failure_category": "step_limit",
                }),
                serde_json::json!({
                    "instance_id": "stag-inst",
                    "exit_reason": "error",
                    "failure_category": "agent_stagnation",
                }),
            ]),
        )
        .unwrap();
        std::fs::write(
            sweep.join("stag-inst.traj.json"),
            stagnation_traj_json("ls", 4, &[1, 2, 3, 4], 4, 0.010),
        )
        .unwrap();

        let report = run_report(sweep);

        assert_eq!(report.totals.halted_count, 1);
        assert_eq!(report.instances[0].instance_id, "stag-inst");
    }

    #[test]
    fn stagnation_instance_has_correct_fields() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[serde_json::json!({
                "instance_id": "inst-1",
                "exit_reason": "error",
                "failure_category": "agent_stagnation",
            })]),
        )
        .unwrap();
        std::fs::write(
            sweep.join("inst-1.traj.json"),
            stagnation_traj_json("find . -name '*.py'", 4, &[2, 4, 6, 8], 8, 0.016),
        )
        .unwrap();

        let report = run_report(sweep);

        assert_eq!(report.instances.len(), 1);
        let row = &report.instances[0];
        assert_eq!(row.instance_id, "inst-1");
        let expected_fp: String = hash_of("find . -name '*.py'").chars().take(8).collect();
        assert_eq!(row.fingerprint, expected_fp);
        assert_eq!(row.canonical_action, "find . -name '*.py'");
        assert_eq!(row.hit_count, 4);
        assert_eq!(row.halt_step, 8);
        assert!((row.budget_burned_usd - 0.016).abs() < 1e-9);
    }

    #[test]
    fn fingerprint_is_first_8_hex_chars_of_full_hash() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[serde_json::json!({
                "instance_id": "i1",
                "exit_reason": "error",
                "failure_category": "agent_stagnation",
            })]),
        )
        .unwrap();
        std::fs::write(
            sweep.join("i1.traj.json"),
            stagnation_traj_json("find . -name '*.py'", 4, &[2, 4, 6, 8], 8, 0.016),
        )
        .unwrap();

        let report = run_report(sweep);
        let expected_fp: String = hash_of("find . -name '*.py'").chars().take(8).collect();
        assert_eq!(report.instances[0].fingerprint, expected_fp);
    }

    #[test]
    fn instances_ranked_by_budget_burned_descending() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[
                serde_json::json!({"instance_id": "cheap", "exit_reason": "error", "failure_category": "agent_stagnation"}),
                serde_json::json!({"instance_id": "expensive", "exit_reason": "error", "failure_category": "agent_stagnation"}),
            ]),
        )
        .unwrap();
        std::fs::write(
            sweep.join("cheap.traj.json"),
            stagnation_traj_json("ls", 4, &[1, 2, 3, 4], 4, 0.001),
        )
        .unwrap();
        std::fs::write(
            sweep.join("expensive.traj.json"),
            stagnation_traj_json("find . -type f", 4, &[1, 2, 3, 4], 4, 0.050),
        )
        .unwrap();

        let report = run_report(sweep);

        assert_eq!(report.instances[0].instance_id, "expensive");
        assert_eq!(report.instances[1].instance_id, "cheap");
    }

    #[test]
    fn clusters_group_by_same_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[
                serde_json::json!({"instance_id": "a", "exit_reason": "error", "failure_category": "agent_stagnation"}),
                serde_json::json!({"instance_id": "b", "exit_reason": "error", "failure_category": "agent_stagnation"}),
                serde_json::json!({"instance_id": "c", "exit_reason": "error", "failure_category": "agent_stagnation"}),
            ]),
        )
        .unwrap();
        // a and b share the "ls" fingerprint; c uses a different action.
        std::fs::write(
            sweep.join("a.traj.json"),
            stagnation_traj_json("ls", 4, &[1, 2, 3, 4], 4, 0.010),
        )
        .unwrap();
        std::fs::write(
            sweep.join("b.traj.json"),
            stagnation_traj_json("ls", 4, &[1, 2, 3, 4], 4, 0.012),
        )
        .unwrap();
        std::fs::write(
            sweep.join("c.traj.json"),
            stagnation_traj_json("find . -type f", 4, &[1, 2, 3, 4], 4, 0.005),
        )
        .unwrap();

        let report = run_report(sweep);

        assert_eq!(report.clusters.len(), 2);
        // Top cluster has 2 instances (both "ls" loops).
        assert_eq!(report.clusters[0].instance_count, 2);
        let expected_fp: String = hash_of("ls").chars().take(8).collect();
        assert_eq!(report.clusters[0].fingerprint, expected_fp);
    }

    #[test]
    fn cluster_shows_up_to_5_exemplar_ids() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        let ids: Vec<String> = (1..=7).map(|i| format!("inst-{i}")).collect();
        let instances: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({"instance_id": id, "exit_reason": "error", "failure_category": "agent_stagnation"})
            })
            .collect();
        std::fs::write(sweep.join("results.json"), results_json(&instances)).unwrap();
        for id in &ids {
            std::fs::write(
                sweep.join(format!("{id}.traj.json")),
                stagnation_traj_json("ls", 4, &[1, 2, 3, 4], 4, 0.001),
            )
            .unwrap();
        }

        let report = run_report(sweep);

        assert_eq!(report.clusters[0].instance_count, 7);
        assert_eq!(report.clusters[0].exemplar_instance_ids.len(), 5);
    }

    #[test]
    fn totals_sum_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[
                serde_json::json!({"instance_id": "x", "exit_reason": "error", "failure_category": "agent_stagnation"}),
                serde_json::json!({"instance_id": "y", "exit_reason": "error", "failure_category": "agent_stagnation"}),
            ]),
        )
        .unwrap();
        std::fs::write(
            sweep.join("x.traj.json"),
            stagnation_traj_json("ls", 4, &[1, 2, 3, 4], 4, 0.020),
        )
        .unwrap();
        std::fs::write(
            sweep.join("y.traj.json"),
            stagnation_traj_json("find .", 4, &[1, 2, 3, 4], 4, 0.030),
        )
        .unwrap();

        let report = run_report(sweep);

        assert_eq!(report.totals.halted_count, 2);
        assert!((report.totals.total_usd_burned_before_halt - 0.050).abs() < 1e-9);
    }

    #[test]
    fn json_output_has_correct_schema_shape() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(sweep.join("results.json"), results_json(&[])).unwrap();

        let report = run_report(sweep);
        let json = render_json(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["artifact_kind"], "stagnation_report");
        assert!(value["schema_version"].is_object());
        assert_eq!(value["sweep_path"], sweep.display().to_string());
        assert!(value["instances"].is_array());
        assert!(value["clusters"].is_array());
        assert!(value["totals"].is_object());
        assert!(value["totals"]["halted_count"].is_number());
        assert!(value["totals"]["total_usd_burned_before_halt"].is_number());
        assert!(value["totals"]["total_usd_saved_estimate"].is_number());
    }

    #[test]
    fn canonical_action_text_is_truncated_at_80_chars() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        let long_action: String = "x".repeat(100);
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[serde_json::json!({
                "instance_id": "i1",
                "exit_reason": "error",
                "failure_category": "agent_stagnation",
            })]),
        )
        .unwrap();
        std::fs::write(
            sweep.join("i1.traj.json"),
            stagnation_traj_json(&long_action, 4, &[1, 2, 3, 4], 4, 0.010),
        )
        .unwrap();

        let report = run_report(sweep);
        let action = &report.instances[0].canonical_action;
        // 80 chars + "…" = 81 display chars
        assert_eq!(action.chars().count(), 81);
        assert!(action.ends_with('\u{2026}'));
    }
}
