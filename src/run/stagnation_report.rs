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
    /// Always present in JSON (as `null`) so the schema is stable for consumers.
    pub usd_saved_estimate: Option<f64>,
    /// Full 32-char SHA-256 fingerprint used internally for collision-safe
    /// clustering. Not exposed in JSON output.
    #[serde(skip)]
    pub(crate) full_hash: String,
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
    /// `null` when no step_limit was available for the sweep (i.e. all
    /// per-instance estimates were unknown). Distinguishes "unknown savings"
    /// from `0.0` which would mean the limit was known and nothing was saved.
    pub total_usd_saved_estimate: Option<f64>,
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
        // Guard path traversal before any filesystem access.
        if !instance_id_is_safe(instance_id) {
            tracing::warn!("stagnation-report: skipping unsafe instance id {instance_id:?}");
            continue;
        }
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
    // Key on step_limit directly: if the limit is known we can always produce a
    // meaningful total (0.0 for an empty or fully-capped sweep), whereas None
    // means "limit was unavailable so savings are unknown".  Using rows.any()
    // as a proxy would return None for an empty sweep with a known limit, which
    // contradicts the stated semantics.
    let total_usd_saved_estimate: Option<f64> =
        step_limit.map(|_| rows.iter().filter_map(|r| r.usd_saved_estimate).sum());

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
    let mut out = String::new();
    let _ = writeln!(out, "\n=== bench stagnation-report ===");
    let _ = writeln!(out, "Sweep: {}", report.sweep_path);
    let _ = writeln!(out, "Stagnation halts: {}", report.totals.halted_count);

    if !report.instances.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "── Per-instance table (ranked by USD burned) ──");

        let mut table = crate::ui::create_table();
        table.set_header(vec![
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

        let mut ctable = crate::ui::create_table();
        ctable.set_header(vec![
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

    let saved_str = report.totals.total_usd_saved_estimate.map_or_else(
        || "— estimated saved (step_limit unknown)".into(),
        |v| format!("${v:.4} estimated saved"),
    );
    let _ = writeln!(
        out,
        "Totals: {} halted  ${:.4} burned  {}",
        report.totals.halted_count, report.totals.total_usd_burned_before_halt, saved_str,
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
    // Use the first (run-1) slot: results.json metadata comes from pass@1.
    let path = paths.first()?;

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

    let full_hash = stag.action_hash.clone(); // 32-char hex
    let fingerprint: String = full_hash.chars().take(8).collect();

    // find_canonical_action may return None when the action was redacted before
    // being written to the trajectory (action_hash is computed pre-redaction).
    // Fall back to "[redacted]" so the instance is still counted and clustered.
    let canonical_raw =
        find_canonical_action(&traj, &full_hash).unwrap_or_else(|| "[redacted]".to_owned());
    let canonical_redacted = redactor.redact_text(&canonical_raw, surface::EXPORT).text;
    let canonical_action = truncate_action(&canonical_redacted);

    // step_indices are 0-based (observe(self.steps - 1, ...)); step count = max + 1.
    let halt_step = traj
        .info
        .steps
        .unwrap_or_else(|| stag.step_indices.iter().copied().max().map_or(0, |m| m + 1));

    // Prefer actual (measured) cost; fall back to total which may include
    // baseline components in some artifact versions.
    let budget_burned_usd = traj
        .info
        .actual_cost_usd
        .or(traj.info.total_cost_usd)
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
        full_hash,
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
    // Key on the full 32-char hash to avoid 8-char prefix collisions on large sweeps.
    // Map full_hash → (display_fingerprint, exemplar_action, total_usd, exemplar_ids, count).
    // Rows are already ranked by budget_burned descending, so the first instance
    // encountered per fingerprint is the most expensive (good exemplar).
    // Count is accumulated in the same pass to avoid O(N×M) redundant iteration.
    let mut by_hash: HashMap<&str, (String, String, f64, Vec<String>, usize)> = HashMap::new();
    for row in rows {
        let entry = by_hash.entry(row.full_hash.as_str()).or_insert_with(|| {
            (
                row.fingerprint.clone(),
                row.canonical_action.clone(),
                0.0,
                Vec::new(),
                0,
            )
        });
        entry.2 += row.budget_burned_usd;
        if entry.3.len() < 5 {
            entry.3.push(row.instance_id.clone());
        }
        entry.4 += 1;
    }

    let mut clusters: Vec<StagnationCluster> = by_hash
        .into_iter()
        .map(
            |(
                _full_hash,
                (fingerprint, exemplar_action, total_usd, exemplar_ids, instance_count),
            )| {
                StagnationCluster {
                    fingerprint,
                    exemplar_action,
                    instance_count,
                    total_usd_burned: total_usd,
                    exemplar_instance_ids: exemplar_ids,
                }
            },
        )
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
    // Use a proper TOML parse so we get correct integer semantics (underscore
    // separators, hex literals, sign prefix, …) and look up the canonical path
    // `agent.step_limit` rather than scanning raw text. This avoids matching
    // keys in unrelated tables or inside multi-line string values.
    let root: toml::Value = toml::from_str(config_resolved).ok()?;
    root.get("agent")
        .and_then(|a| a.get("step_limit"))
        .and_then(toml::Value::as_integer)
        .and_then(|n| u32::try_from(n).ok())
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

/// Returns true when `id` is a single safe path component (no separators, no `..`).
/// Mirrors the same guard used in `bench grep`.
fn instance_id_is_safe(id: &str) -> bool {
    use std::path::{Component, Path};
    let mut components = Path::new(id).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
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
        // Canonical path: [agent] table.
        assert_eq!(
            parse_step_limit_from_config("[agent]\nstep_limit = 30\n"),
            Some(30)
        );
        // Top-level step_limit (no [agent] table) must NOT match — wrong path.
        assert_eq!(parse_step_limit_from_config("step_limit = 50"), None);
        assert_eq!(parse_step_limit_from_config("no_limit_here"), None);
        // Commented-out line must not produce a value.
        assert_eq!(
            parse_step_limit_from_config("# step_limit = 99\n[agent]\nstep_limit = 20"),
            Some(20)
        );
        // A different table (e.g. [sweep]) must not be picked up.
        assert_eq!(
            parse_step_limit_from_config("[sweep]\nstep_limit = 100"),
            None
        );
        // TOML underscore separators must be handled.
        assert_eq!(
            parse_step_limit_from_config("[agent]\nstep_limit = 1_000"),
            Some(1000)
        );
        // Values inside a TOML string literal must not be matched.
        assert_eq!(
            parse_step_limit_from_config(
                "[agent]\nstep_limit = 40\n[prompts]\nsystem = \"step_limit = 99\""
            ),
            Some(40)
        );
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
        // No instances → no step_limit info → savings unknown (None, not 0.0).
        assert!(report.totals.total_usd_saved_estimate.is_none());
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
        // Empty sweep has no manifest → total_usd_saved_estimate serialises as null
        // (field is always present; null means "unknown", not absent).
        assert!(value["totals"]["total_usd_saved_estimate"].is_null());
    }

    #[test]
    fn empty_sweep_with_known_step_limit_has_zero_saved_estimate() {
        // A sweep that has a manifest (step_limit known) but no stagnation rows
        // must emit total_usd_saved_estimate = Some(0.0), not None.
        // This distinguishes "zero savings (genuine result)" from "unknown".
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(
            sweep.join("results.json"),
            results_json_with_step_limit(&[], 50),
        )
        .unwrap();

        let report = run_report(sweep);
        assert_eq!(report.totals.halted_count, 0);
        let saved = report
            .totals
            .total_usd_saved_estimate
            .expect("step_limit known → savings should be Some");
        assert!(saved.abs() < f64::EPSILON, "expected 0.0, got {saved}");
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

    #[test]
    fn stagnation_instance_preserved_when_action_not_in_messages() {
        // Simulates a trajectory where the repeated action was redacted before
        // being written to messages. The instance must still appear in the
        // report with canonical_action = "[redacted]".
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        let full_hash = hash_of("secret-action");
        // Write a trajectory with the correct hash but no matching action text.
        let traj_json = serde_json::to_string_pretty(&serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.3",
            "info": {
                "failure_category": "agent_stagnation",
                "steps": 4,
                "total_cost_usd": 0.010,
                "stagnation": {
                    "action_hash": full_hash,
                    "count": 4,
                    "window": 8,
                    "step_indices": [0, 1, 2, 3],
                }
            },
            // Messages present but action text has been replaced/redacted.
            "messages": [
                {"role": "assistant", "content": "doing stuff",
                 "extra": {"actions": ["[REDACTED]"]}}
            ]
        }))
        .unwrap();
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[serde_json::json!({
                "instance_id": "r1",
                "exit_reason": "error",
                "failure_category": "agent_stagnation",
            })]),
        )
        .unwrap();
        std::fs::write(sweep.join("r1.traj.json"), traj_json).unwrap();

        let report = run_report(sweep);
        assert_eq!(report.totals.halted_count, 1);
        assert_eq!(report.instances[0].canonical_action, "[redacted]");
    }

    #[test]
    fn unsafe_instance_ids_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        // A crafted results.json with a path-traversal instance_id.
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[serde_json::json!({
                "instance_id": "../evil",
                "exit_reason": "error",
                "failure_category": "agent_stagnation",
            })]),
        )
        .unwrap();

        let report = run_report(sweep);
        // The unsafe instance must be skipped, not cause a panic or path escape.
        assert_eq!(report.totals.halted_count, 0);
    }

    #[test]
    fn parse_step_limit_handles_underscore_integers() {
        assert_eq!(
            parse_step_limit_from_config("[agent]\nstep_limit = 1_000"),
            Some(1000)
        );
        assert_eq!(
            parse_step_limit_from_config("[agent]\nstep_limit = 1_0_0"),
            Some(100)
        );
    }

    #[test]
    fn total_saved_estimate_is_none_when_step_limit_unknown() {
        // When the sweep manifest does not provide a step_limit, per-instance
        // usd_saved_estimate is None and the aggregate total must also be None —
        // not 0.0 which would be a misleading "zero savings" reading.
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        std::fs::write(
            sweep.join("results.json"),
            results_json(&[serde_json::json!({
                "instance_id": "no-limit",
                "exit_reason": "error",
                "failure_category": "agent_stagnation",
            })]),
        )
        .unwrap();
        std::fs::write(
            sweep.join("no-limit.traj.json"),
            stagnation_traj_json("ls", 4, &[1, 2, 3, 4], 4, 0.010),
        )
        .unwrap();
        // No manifest → step_limit unknown.
        let report = run_report(sweep);
        assert_eq!(report.totals.halted_count, 1);
        assert!(report.instances[0].usd_saved_estimate.is_none());
        assert!(report.totals.total_usd_saved_estimate.is_none());
    }

    /// Builds a `results.json` string with a minimal embedded manifest whose
    /// `config.resolved` contains `[agent]\nstep_limit = <limit>` (the
    /// canonical TOML path used by the harness at runtime).
    fn results_json_with_step_limit(instances: &[serde_json::Value], step_limit: u32) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "total": instances.len(),
            "submitted": 0,
            "skipped": 0,
            "errored": instances.len(),
            "instances": instances,
            "manifest": {
                "harness":  { "name": "test", "version": "0.0.0", "git_resolution": "none" },
                "dataset":  { "path": "test.parquet", "sha256": "abc123", "instance_count": instances.len() },
                "prompt_template": { "source": "inline", "sha256": "def456" },
                "config":   { "resolved": format!("[agent]\nstep_limit = {step_limit}\n") },
                "model":    { "name": "test-model", "backend": "test" },
                "runtime":  { "started_at_utc": "2024-01-01T00:00:00Z", "host_os": "linux" },
                "cli":      { "argv": ["max", "bench"] },
            },
        }))
        .unwrap()
    }

    #[test]
    fn total_saved_estimate_is_some_when_step_limit_known() {
        // When step_limit can be determined from the manifest's resolved config,
        // per-instance estimates are Some and the aggregate total must be Some(sum).
        // halt_step = 10, step_limit = 50 → 40 steps saved
        // cost = 0.020, mean_per_step = 0.002, saved = 40 * 0.002 = 0.080
        let dir = tempfile::tempdir().unwrap();
        let sweep = dir.path();
        let instances = [serde_json::json!({
            "instance_id": "with-limit",
            "exit_reason": "error",
            "failure_category": "agent_stagnation",
        })];
        std::fs::write(
            sweep.join("results.json"),
            results_json_with_step_limit(&instances, 50),
        )
        .unwrap();
        std::fs::write(
            sweep.join("with-limit.traj.json"),
            stagnation_traj_json("ls", 4, &[7, 8, 9, 10], 10, 0.020),
        )
        .unwrap();

        let report = run_report(sweep);
        assert_eq!(report.totals.halted_count, 1);
        let saved = report
            .totals
            .total_usd_saved_estimate
            .expect("should be Some");
        assert!((saved - 0.080).abs() < 1e-9, "expected ~0.080, got {saved}");
    }

    #[test]
    fn instance_id_safe_accepts_normal_ids() {
        assert!(instance_id_is_safe("django__django-1234"));
        assert!(instance_id_is_safe("instance-a"));
    }

    #[test]
    fn instance_id_safe_rejects_traversal() {
        assert!(!instance_id_is_safe("../evil"));
        assert!(!instance_id_is_safe("a/b"));
        assert!(!instance_id_is_safe(""));
    }
}
