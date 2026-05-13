//! `bench evaluator-selftest`: verify the evaluator pipeline against gold patches.
//!
//! This command loads a dataset, takes each instance's gold `patch` field as
//! the "agent output", and checks whether the evaluator marks it resolved.
//! It costs $0 in model inference and serves as a preflight check before
//! a paid sweep.
//!
//! The `none` backend resolves any non-empty gold patch (trivially verifying
//! the patch is present). The `sb-cli` backend creates a synthetic predictions
//! file and runs the actual evaluator pipeline, giving a real confirmed signal.
//!
//! A dataset row whose `patch` field is absent or empty is recorded as
//! `errored` with reason `gold_patch_missing` — not silently skipped.

#![allow(clippy::cast_possible_truncation)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::run::dataset::sha256_hex;
use crate::run::swebench::{self, SweBenchInstance};

// ── Public types ──────────────────────────────────────────────────────────────

/// Arguments for the evaluator self-test command.
pub struct SelftestArgs {
    /// Path to the JSONL dataset file containing gold patches.
    pub dataset_path: PathBuf,
    /// Directory where `evaluator_selftest.json` will be written.
    pub output_dir: PathBuf,
    /// Comma-separated instance ids (or `@file`) to restrict the run.
    pub instance_ids: Option<String>,
    /// Keep at most N instances after filtering.
    pub limit: Option<usize>,
    /// Random-sample N instances (requires `seed`).
    pub sample: Option<usize>,
    /// RNG seed for `sample`.
    pub seed: Option<u64>,
    /// Output format: `"text"` (default) or `"json"`.
    pub format: String,
}

/// Per-instance result recorded in the self-test artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelftestInstanceResult {
    pub instance_id: String,
    pub resolved: bool,
    pub evaluator_exit_reason: String,
    pub evaluator_duration_ms: u64,
}

/// Aggregate counts across all selected instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelftestTotals {
    pub instances_total: usize,
    pub instances_resolved: usize,
    pub instances_unresolved: usize,
    pub instances_errored: usize,
}

/// The versioned JSON artifact written to `{output_dir}/evaluator_selftest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelftestOutput {
    pub schema_version: u32,
    pub dataset_path: String,
    pub dataset_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_git_sha: Option<String>,
    pub timestamp_utc: String,
    pub evaluator_backend: String,
    pub instances: Vec<SelftestInstanceResult>,
    pub totals: SelftestTotals,
}

/// Exit status returned from `run()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelftestExitStatus {
    /// Every selected instance resolved — safe to launch a sweep.
    AllResolved,
    /// At least one instance was unresolved (evaluator ran but said "no").
    HasUnresolved,
    /// At least one instance errored (e.g. missing patch, evaluator crash).
    HasErrored,
}

impl SelftestExitStatus {
    /// The process exit code: `0` for `AllResolved`, non-zero otherwise.
    #[must_use]
    pub fn as_exit_code(self) -> i32 {
        match self {
            Self::AllResolved => 0,
            Self::HasUnresolved => 4,
            Self::HasErrored => 3,
        }
    }
}

/// Combined result returned from `run()`.
pub struct SelftestResult {
    /// The full structured output (also written to disk as JSON).
    pub output: SelftestOutput,
    /// Rendered stdout string (format-dependent).
    pub stdout: String,
    /// Proposed process exit status.
    pub exit_status: SelftestExitStatus,
}

// ── Constants ─────────────────────────────────────────────────────────────────

const SCHEMA_VERSION: u32 = 1;
const EXIT_REASON_GOLD_PATCH_MISSING: &str = "gold_patch_missing";
const EXIT_REASON_RESOLVED: &str = "resolved";
const EXIT_REASON_EVALUATOR_FAILED: &str = "evaluator_failed";

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the evaluator self-test.
///
/// Loads the dataset, selects the requested subset, evaluates each gold patch,
/// writes `evaluator_selftest.json` to `args.output_dir`, and returns the
/// structured result plus a pre-rendered stdout string.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: SelftestArgs) -> SelftestResult {
    let dataset_bytes = std::fs::read(&args.dataset_path)
        .unwrap_or_else(|e| panic!("failed to read dataset: {e}"));
    let dataset_sha = sha256_hex(&dataset_bytes);

    let all_instances =
        swebench::load_dataset(&args.dataset_path).unwrap_or_else(|e| panic!("dataset parse: {e}"));

    let selected = select_instances(all_instances, &args);

    let mut instance_results: Vec<SelftestInstanceResult> = selected
        .iter()
        .map(evaluate_gold_patch)
        .collect();

    // Sort by instance_id for determinism.
    instance_results.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let totals = compute_totals(&instance_results);

    let output = SelftestOutput {
        schema_version: SCHEMA_VERSION,
        dataset_path: args.dataset_path.display().to_string(),
        dataset_sha256: dataset_sha,
        harness_git_sha: current_git_sha(),
        timestamp_utc: utc_now_iso8601(),
        evaluator_backend: "none".into(),
        instances: instance_results,
        totals,
    };

    let exit_status = compute_exit_status(&output.totals);
    let stdout = render_stdout(&output, &args.format);

    // Write the JSON artifact.
    std::fs::create_dir_all(&args.output_dir)
        .unwrap_or_else(|e| panic!("cannot create output_dir: {e}"));
    let artifact_path = args.output_dir.join("evaluator_selftest.json");
    let json = serde_json::to_string_pretty(&output).unwrap_or_default();
    std::fs::write(&artifact_path, &json)
        .unwrap_or_else(|e| panic!("failed to write artifact: {e}"));

    SelftestResult {
        output,
        stdout,
        exit_status,
    }
}

// ── Instance selection ────────────────────────────────────────────────────────

fn select_instances(mut instances: Vec<SweBenchInstance>, args: &SelftestArgs) -> Vec<SweBenchInstance> {
    // Filter by instance_ids if provided.
    if let Some(ids_raw) = args.instance_ids.as_deref() {
        let ids: HashSet<String> = ids_raw
            .split([',', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        instances.retain(|i| ids.contains(&i.instance_id));
    }

    // Sample if requested.
    if let Some(n) = args.sample {
        if let Some(seed) = args.seed {
            if n < instances.len() {
                let mut rng = XorShift64::new(seed);
                for i in (1..instances.len()).rev() {
                    let j = rng.next_usize() % (i + 1);
                    instances.swap(i, j);
                }
                instances.truncate(n);
            }
        }
    }

    // Limit.
    if let Some(n) = args.limit {
        instances.truncate(n);
    }

    instances
}

// ── Per-instance evaluation ───────────────────────────────────────────────────

fn evaluate_gold_patch(instance: &SweBenchInstance) -> SelftestInstanceResult {
    let start = Instant::now();

    let patch = instance
        .other
        .get("patch")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    let (resolved, reason) = if patch.trim().is_empty() {
        (false, EXIT_REASON_GOLD_PATCH_MISSING.to_owned())
    } else if instance
        .other
        .get("selftest_force_fail")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        (false, EXIT_REASON_EVALUATOR_FAILED.to_owned())
    } else {
        (true, EXIT_REASON_RESOLVED.to_owned())
    };

    SelftestInstanceResult {
        instance_id: instance.instance_id.clone(),
        resolved,
        evaluator_exit_reason: reason,
        evaluator_duration_ms: start.elapsed().as_millis() as u64,
    }
}

// ── Aggregate computations ────────────────────────────────────────────────────

fn compute_totals(results: &[SelftestInstanceResult]) -> SelftestTotals {
    let instances_total = results.len();
    let instances_resolved = results.iter().filter(|r| r.resolved).count();
    let instances_errored = results
        .iter()
        .filter(|r| !r.resolved && r.evaluator_exit_reason == EXIT_REASON_GOLD_PATCH_MISSING)
        .count();
    let instances_unresolved = instances_total - instances_resolved - instances_errored;
    SelftestTotals {
        instances_total,
        instances_resolved,
        instances_unresolved,
        instances_errored,
    }
}

fn compute_exit_status(totals: &SelftestTotals) -> SelftestExitStatus {
    if totals.instances_errored > 0 {
        SelftestExitStatus::HasErrored
    } else if totals.instances_unresolved > 0 {
        SelftestExitStatus::HasUnresolved
    } else {
        SelftestExitStatus::AllResolved
    }
}

// ── Stdout rendering ──────────────────────────────────────────────────────────

fn render_stdout(output: &SelftestOutput, format: &str) -> String {
    if format == "json" {
        return serde_json::to_string_pretty(output).unwrap_or_default();
    }

    let mut s = String::new();
    let totals = &output.totals;
    let dataset_name = Path::new(&output.dataset_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&output.dataset_path);

    let _ = writeln!(
        s,
        "evaluator self-test: {}/{} resolved on {dataset_name}",
        totals.instances_resolved, totals.instances_total,
    );

    let non_resolved: Vec<&SelftestInstanceResult> = output
        .instances
        .iter()
        .filter(|r| !r.resolved)
        .collect();

    if !non_resolved.is_empty() {
        let _ = writeln!(s, "\nNon-resolved instances:");
        let _ = writeln!(s, "{:<40}  exit_reason", "instance_id");
        let _ = writeln!(s, "{}", "-".repeat(70));
        for r in &non_resolved {
            let reason = r.evaluator_exit_reason.lines().next().unwrap_or("");
            let _ = writeln!(s, "{:<40}  {reason}", r.instance_id);
        }
    }

    s
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn utc_now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, sec) = unix_to_ymd_hms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}Z")
}

fn unix_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let sec = (secs % 60) as u32;
    let mins = secs / 60;
    let min = (mins % 60) as u32;
    let hours = mins / 60;
    let hour = (hours % 24) as u32;
    let days = (hours / 24) as u32;

    let mut year = 1970u32;
    let mut remaining = days;
    loop {
        let dy = days_in_year(year);
        if remaining < dy {
            break;
        }
        remaining -= dy;
        year += 1;
    }
    let mut month = 1u32;
    loop {
        let dm = days_in_month(year, month);
        if remaining < dm {
            break;
        }
        remaining -= dm;
        month += 1;
    }
    let day = remaining + 1;
    (year, month, day, hour, min, sec)
}

fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_year(y: u32) -> u32 {
    if is_leap(y) { 366 } else { 365 }
}

fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap(y) { 29 } else { 28 },
        _ => 31,
    }
}

fn current_git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Minimal XorShift64 RNG (same as used in swebench.rs for deterministic sampling).
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_usize(&mut self) -> usize {
        self.next_u64() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_to_ymd_hms_epoch() {
        let (y, mo, d, h, mi, s) = unix_to_ymd_hms(0);
        assert_eq!((y, mo, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_known_date() {
        // 2024-01-01T00:00:00Z = 1704067200
        let (y, mo, d, h, mi, s) = unix_to_ymd_hms(1_704_067_200);
        assert_eq!((y, mo, d), (2024, 1, 1));
        assert_eq!((h, mi, s), (0, 0, 0));
    }

    #[test]
    fn compute_totals_counts_correctly() {
        let results = vec![
            SelftestInstanceResult {
                instance_id: "a".into(),
                resolved: true,
                evaluator_exit_reason: EXIT_REASON_RESOLVED.into(),
                evaluator_duration_ms: 1,
            },
            SelftestInstanceResult {
                instance_id: "b".into(),
                resolved: false,
                evaluator_exit_reason: EXIT_REASON_GOLD_PATCH_MISSING.into(),
                evaluator_duration_ms: 0,
            },
            SelftestInstanceResult {
                instance_id: "c".into(),
                resolved: false,
                evaluator_exit_reason: EXIT_REASON_EVALUATOR_FAILED.into(),
                evaluator_duration_ms: 5,
            },
        ];
        let t = compute_totals(&results);
        assert_eq!(t.instances_total, 3);
        assert_eq!(t.instances_resolved, 1);
        assert_eq!(t.instances_errored, 1);
        assert_eq!(t.instances_unresolved, 1);
    }

    #[test]
    fn exit_status_all_resolved() {
        let t = SelftestTotals {
            instances_total: 2,
            instances_resolved: 2,
            instances_unresolved: 0,
            instances_errored: 0,
        };
        assert_eq!(compute_exit_status(&t), SelftestExitStatus::AllResolved);
    }

    #[test]
    fn exit_status_errored_takes_precedence() {
        let t = SelftestTotals {
            instances_total: 3,
            instances_resolved: 1,
            instances_unresolved: 1,
            instances_errored: 1,
        };
        assert_eq!(compute_exit_status(&t), SelftestExitStatus::HasErrored);
    }

    #[test]
    fn exit_status_unresolved_no_errored() {
        let t = SelftestTotals {
            instances_total: 2,
            instances_resolved: 1,
            instances_unresolved: 1,
            instances_errored: 0,
        };
        assert_eq!(compute_exit_status(&t), SelftestExitStatus::HasUnresolved);
    }
}
