//! `bench evaluator-selftest`: verify the evaluator pipeline against gold patches.
//!
//! This command loads a dataset, takes each instance's gold `patch` field as
//! the "agent output", and checks whether the evaluator marks it resolved.
//! It costs $0 in model inference and serves as a preflight check before
//! a paid sweep.
//!
//! The `none` backend resolves any non-empty gold patch (trivially verifying
//! the patch is present). The `sb-cli` backend creates a synthetic predictions
//! file and routes it through the same evaluator pipeline that `bench evaluate`
//! uses on real sweeps, giving a real confirmed signal.
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
use crate::run::evaluate::{BreakdownSelection, EvalExitReason, EvaluateArgs, EvaluateBackend};
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
    /// Evaluation backend: `"none"` (presence check) or `"sb-cli"` (real
    /// evaluator pipeline, same code path as `bench evaluate`).
    pub backend: String,
    /// SWE-bench subset for the `sb-cli` backend (e.g. `"swe-bench-m"`).
    pub sb_subset: String,
    /// SWE-bench split for the `sb-cli` backend (e.g. `"dev"`).
    pub sb_split: String,
    /// Per-instance evaluation timeout in seconds for the `sb-cli` backend.
    pub timeout_per_instance: u64,
    /// Parallel worker count for the `sb-cli` backend.
    pub parallel: usize,
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
    assert!(
        args.backend == "none" || args.backend == "sb-cli" || args.backend == "docker-tests",
        "unknown --backend {:?}: accepted values are `none`, `sb-cli`, and `docker-tests`",
        args.backend
    );
    assert!(
        args.sample.is_none() || args.seed.is_some(),
        "--sample requires --seed"
    );

    let dataset_bytes =
        std::fs::read(&args.dataset_path).unwrap_or_else(|e| panic!("failed to read dataset: {e}"));
    let dataset_sha = sha256_hex(&dataset_bytes);

    let all_instances =
        swebench::load_dataset(&args.dataset_path).unwrap_or_else(|e| panic!("dataset parse: {e}"));

    let selected = select_instances(all_instances, &args);

    let mut instance_results: Vec<SelftestInstanceResult> = if args.backend == "sb-cli" {
        evaluate_via_sb_cli(&selected, &args)
    } else if args.backend == "docker-tests" {
        evaluate_via_docker_tests(&selected, &args)
    } else {
        selected.iter().map(evaluate_gold_patch_none).collect()
    };

    // Sort by instance_id for determinism.
    instance_results.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let totals = compute_totals(&instance_results);

    let output = SelftestOutput {
        schema_version: SCHEMA_VERSION,
        dataset_path: args.dataset_path.display().to_string(),
        dataset_sha256: dataset_sha,
        harness_git_sha: current_git_sha(),
        timestamp_utc: utc_now_iso8601(),
        evaluator_backend: args.backend.clone(),
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

fn select_instances(
    mut instances: Vec<SweBenchInstance>,
    args: &SelftestArgs,
) -> Vec<SweBenchInstance> {
    if let Some(ids_raw) = args.instance_ids.as_deref() {
        let text = if let Some(path) = ids_raw.trim().strip_prefix('@') {
            std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("failed to read --instance-ids file `{path}`: {e}"))
        } else {
            ids_raw.to_owned()
        };
        let ids: HashSet<String> = text
            .split([',', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        instances.retain(|i| ids.contains(&i.instance_id));
        assert!(
            !instances.is_empty(),
            "--instance-ids filter matched 0 instances; check for typos"
        );
    }

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

    if let Some(n) = args.limit {
        instances.truncate(n);
    }

    assert!(
        !instances.is_empty(),
        "slicing flags produced an empty selection; use --limit / --sample > 0"
    );

    instances
}

// ── None-backend evaluation ───────────────────────────────────────────────────

fn evaluate_gold_patch_none(instance: &SweBenchInstance) -> SelftestInstanceResult {
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

// ── Sb-cli backend evaluation ─────────────────────────────────────────────────

/// Route gold patches through the same `evaluate::run()` pipeline that real
/// sweeps use, by creating a synthetic sweep directory.
fn evaluate_via_sb_cli(
    selected: &[SweBenchInstance],
    args: &SelftestArgs,
) -> Vec<SelftestInstanceResult> {
    // Scratch dir for the synthetic sweep artifacts (results.json, all_preds.jsonl).
    let scratch = args.output_dir.join("_eval_scratch");
    std::fs::create_dir_all(&scratch)
        .unwrap_or_else(|e| panic!("cannot create eval scratch dir: {e}"));

    // Split into instances that have a gold patch and those that don't.
    let mut missing: Vec<SelftestInstanceResult> = Vec::new();
    let mut eval_pairs: Vec<(&SweBenchInstance, String)> = Vec::new();

    for inst in selected {
        let patch = inst
            .other
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if patch.trim().is_empty() {
            missing.push(SelftestInstanceResult {
                instance_id: inst.instance_id.clone(),
                resolved: false,
                evaluator_exit_reason: EXIT_REASON_GOLD_PATCH_MISSING.to_owned(),
                evaluator_duration_ms: 0,
            });
        } else {
            eval_pairs.push((inst, patch.to_owned()));
        }
    }

    if eval_pairs.is_empty() {
        return missing;
    }

    write_synthetic_results_json(&scratch, &eval_pairs);
    write_synthetic_predictions(&scratch, &eval_pairs);

    let eval_args = EvaluateArgs {
        sweep_dir: scratch,
        dataset_path: Some(args.dataset_path.clone()),
        backend: EvaluateBackend::SbCli,
        timeout_per_instance_secs: args.timeout_per_instance,
        parallel: args.parallel,
        sb_subset: args.sb_subset.clone(),
        sb_split: args.sb_split.clone(),
        run_id: None,
        breakdown: BreakdownSelection::none(),
        cost_attribution: false,
    };

    let start = Instant::now();
    let eval_result = crate::run::evaluate::run(&eval_args);
    let total_ms = start.elapsed().as_millis() as u64;

    let mut sb_cli_results: Vec<SelftestInstanceResult> = match eval_result {
        Ok(eval) => {
            let n = eval.instances.len().max(1) as u64;
            let per_inst_ms = total_ms / n;
            eval.instances
                .into_iter()
                .map(|e| SelftestInstanceResult {
                    instance_id: e.instance_id,
                    resolved: e.resolved,
                    evaluator_exit_reason: map_eval_exit_reason(&e.eval_exit_reason),
                    evaluator_duration_ms: per_inst_ms,
                })
                .collect()
        }
        Err(e) => {
            // Entire evaluator invocation failed — mark all submitted instances.
            eval_pairs
                .iter()
                .map(|(inst, _)| SelftestInstanceResult {
                    instance_id: inst.instance_id.clone(),
                    resolved: false,
                    evaluator_exit_reason: format!("{EXIT_REASON_EVALUATOR_FAILED}: {e}"),
                    evaluator_duration_ms: total_ms,
                })
                .collect()
        }
    };

    sb_cli_results.extend(missing);
    sb_cli_results
}

/// Write a minimal synthetic `results.json` that `evaluate::run()` / `load_sweep()`
/// can parse. Each instance is marked as `outcome: submitted, patch_present: true`.
fn write_synthetic_results_json(dir: &Path, pairs: &[(&SweBenchInstance, String)]) {
    let n = pairs.len();
    let instances: Vec<serde_json::Value> = pairs
        .iter()
        .map(|(inst, _)| {
            serde_json::json!({
                "instance_id": inst.instance_id,
                "exit_reason": "submitted",
                "outcome": "submitted",
                "patch_present": true,
                "non_empty_patch": true,
            })
        })
        .collect();

    let results = serde_json::json!({
        "total": n,
        "submitted": n,
        "skipped": 0,
        "errored": 0,
        "instances": instances,
    });

    let path = dir.join("results.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&results).unwrap_or_default(),
    )
    .unwrap_or_else(|e| panic!("failed to write synthetic results.json: {e}"));
}

/// Write `all_preds.jsonl` with the gold patch for each instance.
fn write_synthetic_predictions(dir: &Path, pairs: &[(&SweBenchInstance, String)]) {
    let mut lines = String::new();
    for (inst, patch) in pairs {
        let row = serde_json::json!({
            "instance_id": inst.instance_id,
            "model_patch": patch,
            "model_name_or_path": "evaluator_selftest",
        });
        lines.push_str(&serde_json::to_string(&row).unwrap_or_default());
        lines.push('\n');
    }
    let path = swebench::predictions_path(dir);
    std::fs::write(&path, lines)
        .unwrap_or_else(|e| panic!("failed to write synthetic predictions: {e}"));
}

fn map_eval_exit_reason(reason: &EvalExitReason) -> String {
    match reason {
        EvalExitReason::Resolved => EXIT_REASON_RESOLVED.to_owned(),
        EvalExitReason::Unresolved => "unresolved".to_owned(),
        EvalExitReason::PatchApplyFailed => "patch_apply_failed".to_owned(),
        EvalExitReason::EvalError => EXIT_REASON_EVALUATOR_FAILED.to_owned(),
        EvalExitReason::SkippedNoPatch => EXIT_REASON_GOLD_PATCH_MISSING.to_owned(),
        EvalExitReason::SkippedNoImage => "skipped_no_image".to_owned(),
    }
}

// ── Docker-tests backend evaluation ──────────────────────────────────────────

/// Route gold patches through the `docker-tests` offline evaluator backend.
///
/// For each instance, writes gold patch + synthetic sweep directory, then calls
/// `evaluate::run()` with `EvaluateBackend::DockerTests`. Instances missing an
/// `image` field in the dataset are returned as `skipped_no_image`.
fn evaluate_via_docker_tests(
    selected: &[SweBenchInstance],
    args: &SelftestArgs,
) -> Vec<SelftestInstanceResult> {
    let scratch = args.output_dir.join("_docker_tests_scratch");
    std::fs::create_dir_all(&scratch)
        .unwrap_or_else(|e| panic!("cannot create docker-tests scratch dir: {e}"));

    let mut missing: Vec<SelftestInstanceResult> = Vec::new();
    let mut eval_pairs: Vec<(&SweBenchInstance, String)> = Vec::new();

    for inst in selected {
        let patch = inst
            .other
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if patch.trim().is_empty() {
            missing.push(SelftestInstanceResult {
                instance_id: inst.instance_id.clone(),
                resolved: false,
                evaluator_exit_reason: EXIT_REASON_GOLD_PATCH_MISSING.to_owned(),
                evaluator_duration_ms: 0,
            });
        } else {
            eval_pairs.push((inst, patch.to_owned()));
        }
    }

    if eval_pairs.is_empty() {
        return missing;
    }

    write_synthetic_results_json(&scratch, &eval_pairs);
    write_synthetic_predictions(&scratch, &eval_pairs);

    // Write per-instance run-1.patch files so docker-tests can read them via
    // swebench::existing_patch_path_for_run(), which expects <sweep>/<id>/run-1.patch.
    for (inst, patch) in &eval_pairs {
        let inst_dir = scratch.join(&inst.instance_id);
        std::fs::create_dir_all(&inst_dir)
            .unwrap_or_else(|e| panic!("cannot create instance dir for selftest: {e}"));
        let patch_path = swebench::patch_path_for_run(&scratch, &inst.instance_id, 1);
        std::fs::write(&patch_path, patch)
            .unwrap_or_else(|e| panic!("failed to write selftest patch file: {e}"));
    }

    // Write a minimal dataset JSONL containing the instances so docker-tests can
    // find their image field and test lists.
    let dataset_path = scratch.join("selftest_dataset.jsonl");
    {
        let mut lines = String::new();
        for (inst, _) in &eval_pairs {
            let row = serde_json::json!({
                "instance_id": inst.instance_id,
                "image": inst.image,
                "FAIL_TO_PASS": inst.other.get("FAIL_TO_PASS").cloned().unwrap_or(serde_json::Value::Array(vec![])),
                "PASS_TO_PASS": inst.other.get("PASS_TO_PASS").cloned().unwrap_or(serde_json::Value::Array(vec![])),
            });
            lines.push_str(&serde_json::to_string(&row).unwrap_or_default());
            lines.push('\n');
        }
        std::fs::write(&dataset_path, lines)
            .unwrap_or_else(|e| panic!("failed to write selftest dataset: {e}"));
    }

    let eval_args = EvaluateArgs {
        sweep_dir: scratch,
        dataset_path: Some(dataset_path),
        backend: EvaluateBackend::DockerTests,
        timeout_per_instance_secs: args.timeout_per_instance,
        parallel: args.parallel,
        sb_subset: args.sb_subset.clone(),
        sb_split: args.sb_split.clone(),
        run_id: None,
        breakdown: BreakdownSelection::none(),
        cost_attribution: false,
    };

    let start = std::time::Instant::now();
    let eval_result = crate::run::evaluate::run(&eval_args);
    let total_ms = start.elapsed().as_millis() as u64;

    let mut results: Vec<SelftestInstanceResult> = match eval_result {
        Ok(eval) => {
            let n = eval.instances.len().max(1) as u64;
            let per_inst_ms = total_ms / n;
            eval.instances
                .into_iter()
                .map(|e| SelftestInstanceResult {
                    instance_id: e.instance_id,
                    resolved: e.resolved,
                    evaluator_exit_reason: map_eval_exit_reason(&e.eval_exit_reason),
                    evaluator_duration_ms: per_inst_ms,
                })
                .collect()
        }
        Err(e) => eval_pairs
            .iter()
            .map(|(inst, _)| SelftestInstanceResult {
                instance_id: inst.instance_id.clone(),
                resolved: false,
                evaluator_exit_reason: format!("{EXIT_REASON_EVALUATOR_FAILED}: {e}"),
                evaluator_duration_ms: total_ms,
            })
            .collect(),
    };

    results.extend(missing);
    results
}

// ── Aggregate computations ────────────────────────────────────────────────────

/// Returns `true` when an exit reason indicates an infrastructure/setup error
/// rather than a clean "evaluator ran and said no" verdict.
///
/// `gold_patch_missing` — dataset row had no patch to evaluate.
/// `evaluator_failed[: ...]` — evaluator crashed or couldn't grade the patch.
///
/// Both warrant exit code 3 (`HasErrored`); a plain unresolved verdict
/// (gold patch submitted but not resolved) warrants exit code 4 (`HasUnresolved`).
fn is_errored_reason(reason: &str) -> bool {
    reason == EXIT_REASON_GOLD_PATCH_MISSING || reason.starts_with(EXIT_REASON_EVALUATOR_FAILED)
}

fn compute_totals(results: &[SelftestInstanceResult]) -> SelftestTotals {
    let instances_total = results.len();
    let instances_resolved = results.iter().filter(|r| r.resolved).count();
    let instances_errored = results
        .iter()
        .filter(|r| !r.resolved && is_errored_reason(&r.evaluator_exit_reason))
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

    let non_resolved: Vec<&SelftestInstanceResult> =
        output.instances.iter().filter(|r| !r.resolved).collect();

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
        .map_or(0, |d| d.as_secs());
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
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
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
    #![allow(clippy::unwrap_used)]
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
        // both gold_patch_missing and evaluator_failed count as errored
        assert_eq!(t.instances_errored, 2);
        assert_eq!(t.instances_unresolved, 0);
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

    #[test]
    fn is_errored_reason_recognises_exact_and_prefixed_forms() {
        assert!(is_errored_reason(EXIT_REASON_GOLD_PATCH_MISSING));
        assert!(is_errored_reason(EXIT_REASON_EVALUATOR_FAILED));
        // sb-cli path appends ": <detail>" — still errored
        assert!(is_errored_reason(
            "evaluator_failed: sb-cli exited with status 1"
        ));
        // unresolved verdict — not an infrastructure error
        assert!(!is_errored_reason("unresolved"));
        assert!(!is_errored_reason("patch_apply_failed"));
        assert!(!is_errored_reason(EXIT_REASON_RESOLVED));
    }

    #[test]
    fn map_eval_exit_reason_covers_all_variants() {
        use crate::run::evaluate::EvalExitReason;
        assert_eq!(
            map_eval_exit_reason(&EvalExitReason::Resolved),
            EXIT_REASON_RESOLVED
        );
        assert_eq!(
            map_eval_exit_reason(&EvalExitReason::Unresolved),
            "unresolved"
        );
        assert_eq!(
            map_eval_exit_reason(&EvalExitReason::PatchApplyFailed),
            "patch_apply_failed"
        );
        assert_eq!(
            map_eval_exit_reason(&EvalExitReason::EvalError),
            EXIT_REASON_EVALUATOR_FAILED
        );
        assert_eq!(
            map_eval_exit_reason(&EvalExitReason::SkippedNoPatch),
            EXIT_REASON_GOLD_PATCH_MISSING
        );
    }

    #[test]
    fn write_synthetic_predictions_produces_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let inst = crate::run::swebench::SweBenchInstance {
            instance_id: "test__repo-1".into(),
            repo: Some("test/repo".into()),
            base_commit: Some("abc123".into()),
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        };
        let pairs = vec![(&inst, "diff --git a/f.py b/f.py".to_owned())];
        write_synthetic_predictions(dir.path(), &pairs);

        let preds_path = crate::run::swebench::predictions_path(dir.path());
        assert!(preds_path.exists(), "all_preds.jsonl must be written");
        let text = std::fs::read_to_string(preds_path).unwrap();
        let row: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(row["instance_id"].as_str().unwrap(), "test__repo-1");
        assert_eq!(
            row["model_name_or_path"].as_str().unwrap(),
            "evaluator_selftest"
        );
        assert!(row["model_patch"].as_str().unwrap().contains("diff"));
    }

    #[test]
    fn write_synthetic_results_json_is_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let inst = crate::run::swebench::SweBenchInstance {
            instance_id: "test__repo-1".into(),
            repo: Some("test/repo".into()),
            base_commit: Some("abc123".into()),
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        };
        let pairs = vec![(&inst, "diff --git a/f.py b/f.py".to_owned())];
        write_synthetic_results_json(dir.path(), &pairs);

        let path = dir.path().join("results.json");
        assert!(path.exists());
        let text = std::fs::read_to_string(path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["total"].as_u64().unwrap(), 1);
        assert_eq!(v["submitted"].as_u64().unwrap(), 1);
        let instances = v["instances"].as_array().unwrap();
        assert_eq!(
            instances[0]["instance_id"].as_str().unwrap(),
            "test__repo-1"
        );
        assert_eq!(instances[0]["outcome"].as_str().unwrap(), "submitted");
    }
}
