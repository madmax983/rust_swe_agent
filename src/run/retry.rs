//! Operator-driven post-completion retry path for completed sweep directories.
//!
//! `bench retry --sweep <dir> --failure-category step_limit --yes` re-runs only
//! the selected failed instances and merges their outcomes back into the same
//! `results.json` with full provenance in `retry_history`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Config;
use crate::error::{ConfigError, Error};
use crate::run::swebench::{
    InstanceResult, SweepResults, existing_patch_path_for_run, patch_path_for_run,
};
use crate::trajectory::FailureCategory;

// Re-export so callers can import from `run::retry`.
pub use crate::run::swebench::{OverrideDelta, RetryHistoryEntry, RetrySelection};

/// Arguments for a `bench retry` invocation.
pub struct RetryArgs {
    /// Path to the sweep directory containing `results.json`.
    pub sweep_dir: PathBuf,

    // ── selection ─────────────────────────────────────────────────────────────
    /// Retry only instances whose `failure_category` is in this list.
    pub failure_categories: Option<Vec<FailureCategory>>,
    /// Retry only instances whose `outcome` is in this list.
    pub outcomes: Option<Vec<String>>,
    /// Retry only these specific instance ids (intersected with other filters).
    pub instance_ids: Option<Vec<String>>,
    /// Cap the number of selected instances after all other filters.
    pub limit: Option<usize>,
    /// Allow retrying instances that were `submitted` (incurs cost).
    pub allow_resolved_retry: bool,

    // ── gates ─────────────────────────────────────────────────────────────────
    /// Skip the harness git-SHA check even when the binary and manifest differ.
    pub allow_harness_mismatch: bool,
    /// Proceed without interactive confirmation (dry-run disabled).
    pub yes: bool,

    // ── override flags ────────────────────────────────────────────────────────
    pub model: Option<String>,
    pub config: Option<Config>,
    pub step_limit: Option<u32>,
    pub task_timeout_secs: Option<u64>,
    pub per_task_budget_usd: Option<f64>,
    pub sweep_cost_limit_usd: Option<f64>,
    pub env: Option<String>,
    pub docker_image: Option<String>,
    pub parallel: Option<usize>,
    pub dataset_path: Option<PathBuf>,
    pub dataset: Option<String>,

    // ── testing hooks ─────────────────────────────────────────────────────────
    /// Injected deterministic model responses (test-only).
    pub deterministic_responses: Option<Vec<String>>,
}

// ─── selection ────────────────────────────────────────────────────────────────

/// Resolve the set of instance results to retry, applying the four composable
/// filters in stable order: `failure_categories` → `outcomes` → `instance_ids`
/// → `limit`.
///
/// Returns `Err` when no selector flag was provided or when the selection
/// includes `submitted` instances without `allow_resolved_retry`.
pub fn resolve_selection<'a>(
    instances: &'a [InstanceResult],
    failure_categories: Option<&[FailureCategory]>,
    outcomes: Option<&[String]>,
    instance_ids: Option<&[String]>,
    limit: Option<usize>,
    allow_resolved_retry: bool,
) -> Result<Vec<&'a InstanceResult>, Error> {
    if failure_categories.is_none() && outcomes.is_none() && instance_ids.is_none() {
        return Err(Error::Config(ConfigError::Invalid(
            "bench retry: at least one of --failure-category, --outcome, \
             or --instance-ids is required; running without a selector would \
             silently re-run all instances"
                .into(),
        )));
    }

    let mut selected: Vec<&InstanceResult> = instances.iter().collect();

    // 1. Filter by failure category.
    if let Some(cats) = failure_categories {
        selected.retain(|r| {
            r.failure_category
                .as_ref()
                .is_some_and(|c| cats.contains(c))
        });
    }

    // 2. Filter by outcome.
    if let Some(outs) = outcomes {
        selected.retain(|r| {
            r.outcome
                .as_ref()
                .is_some_and(|o| outs.iter().any(|f| f == o))
        });
    }

    // 3. Intersect with explicit instance_ids.
    if let Some(ids) = instance_ids {
        let id_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        selected.retain(|r| id_set.contains(r.instance_id.as_str()));
    }

    // 4. Guard against re-running already-resolved instances.
    //    In pass@k sweeps, resolved_count > 0 means at least one run succeeded
    //    even if outcome is not "submitted", so we treat those as resolved too.
    if !allow_resolved_retry {
        if let Some(resolved) = selected
            .iter()
            .find(|r| r.outcome.as_deref() == Some("submitted") || r.resolved_count > 0)
        {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "bench retry: instance '{}' is already resolved; pass \
                 --allow-resolved-retry to acknowledge the extra cost",
                resolved.instance_id
            ))));
        }
    }

    // 5. Cap.
    if let Some(n) = limit {
        selected.truncate(n);
    }

    if selected.is_empty() {
        return Err(Error::Config(ConfigError::Invalid(
            "bench retry: no instances matched the given filters — nothing to retry".into(),
        )));
    }

    Ok(selected)
}

// ─── merge ────────────────────────────────────────────────────────────────────

/// Merge retry run results back into the original `SweepResults`.
///
/// For each instance id in `selected_ids`:
/// - Replace the `InstanceResult` row with the one from `retry_results`.
/// - Set `retry_id` to `entry.retry_id`.
/// - Set `previous_failure_category` to the original instance's
///   `failure_category`.
///
/// All other rows are preserved byte-for-byte. Aggregate counters are
/// recomputed. The `entry` is appended to `retry_history`.
#[allow(clippy::too_many_lines)]
pub fn merge_retry_results<S: std::hash::BuildHasher>(
    original: &SweepResults,
    retry_results: &SweepResults,
    entry: RetryHistoryEntry,
    selected_ids: &HashSet<String, S>,
) -> SweepResults {
    let retry_by_id: HashMap<&str, &InstanceResult> = retry_results
        .instances
        .iter()
        .map(|r| (r.instance_id.as_str(), r))
        .collect();

    let merged_instances: Vec<InstanceResult> = original
        .instances
        .iter()
        .map(|orig| {
            if selected_ids.contains(&orig.instance_id) {
                if let Some(new) = retry_by_id.get(orig.instance_id.as_str()) {
                    // Don't replace with a cancelled row; the trajectory restore
                    // has already put the old trajectory back on disk but
                    // results.json should keep the original summary row.
                    if new.exit_reason != "cancelled" {
                        let mut updated = (*new).clone();
                        updated.retry_id = Some(entry.retry_id.clone());
                        updated.previous_failure_category = orig.failure_category;
                        return updated;
                    }
                }
            }
            orig.clone()
        })
        .collect();

    // Recompute all aggregates from the merged instance list so reports and
    // comparisons don't see stale counters from the pre-retry sweep.
    let total = merged_instances.len();
    let submitted = merged_instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some("submitted") && r.exit_reason != "skipped_resume")
        .count();
    let submitted_with_tests = merged_instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some("submitted") && r.tests_run_before_submit)
        .count();
    let errored = merged_instances
        .iter()
        .filter(|r| {
            let out = r.outcome.as_deref().unwrap_or("");
            out != "submitted" && !r.exit_reason.starts_with("budget_halt")
        })
        .count();
    let budget_halted = merged_instances
        .iter()
        .filter(|r| r.exit_reason.starts_with("budget_halt"))
        .count();
    let with_patch = merged_instances.iter().filter(|r| r.with_patch()).count();
    let patch_empty = merged_instances.iter().filter(|r| r.patch_empty()).count();
    let patch_apply_invalid = merged_instances
        .iter()
        .filter(|r| r.failure_category == Some(FailureCategory::PatchApplyInvalid))
        .count();
    let github_pr_failures = merged_instances
        .iter()
        .filter(|r| r.github_pr_error.is_some())
        .count();
    let failures_by_category: BTreeMap<FailureCategory, usize> = {
        let mut map = BTreeMap::new();
        for r in &merged_instances {
            if let Some(cat) = r.failure_category {
                *map.entry(cat).or_insert(0) += 1;
            }
        }
        map
    };
    // Count rows with any resolved run (pass@k semantics: resolved_count > 0 means
    // at least one of the k runs submitted successfully, regardless of value).
    let resolved_any = merged_instances
        .iter()
        .filter(|r| r.resolved_count > 0)
        .count();
    #[allow(clippy::cast_precision_loss)]
    let pass_at_k = if merged_instances.is_empty() {
        0.0
    } else {
        resolved_any as f64 / merged_instances.len() as f64
    };
    let total_prompt_tokens: u64 = merged_instances
        .iter()
        .filter_map(|r| r.prompt_tokens)
        .sum();
    let total_cache_read_tokens: u64 = merged_instances
        .iter()
        .filter_map(|r| r.cache_read_tokens)
        .sum();
    let total_cache_creation_tokens: u64 = merged_instances
        .iter()
        .filter_map(|r| r.cache_creation_tokens)
        .sum();
    let total_completion_tokens: u64 = merged_instances
        .iter()
        .filter_map(|r| r.completion_tokens)
        .sum();
    let estimated_cost_usd: f64 = merged_instances.iter().filter_map(|r| r.cost_usd).sum();
    let retried_instances = merged_instances
        .iter()
        .filter(|r| !r.retry_reasons.is_empty())
        .count();
    let retries: u64 = merged_instances
        .iter()
        .map(|r| r.retry_reasons.len() as u64)
        .sum();
    let total_fallbacks: u64 = merged_instances
        .iter()
        .filter_map(|r| r.fallback_count)
        .map(u64::from)
        .sum();
    let model_mix: BTreeMap<String, usize> = {
        let mut map = BTreeMap::new();
        for r in &merged_instances {
            if let Some(m) = &r.final_model {
                *map.entry(m.clone()).or_insert(0) += 1;
            }
        }
        map
    };

    let mut result = original.clone();
    result.total = total;
    result.submitted = submitted;
    result.submitted_with_tests = submitted_with_tests;
    result.errored = errored;
    result.budget_halted = budget_halted;
    result.with_patch = with_patch;
    result.patch_empty = patch_empty;
    result.patch_apply_invalid = patch_apply_invalid;
    result.github_pr_failures = github_pr_failures;
    result.failures_by_category = failures_by_category;
    result.pass_at_k = pass_at_k;
    result.total_prompt_tokens = total_prompt_tokens;
    result.total_cache_read_tokens = total_cache_read_tokens;
    result.total_cache_creation_tokens = total_cache_creation_tokens;
    result.total_completion_tokens = total_completion_tokens;
    result.estimated_cost_usd = estimated_cost_usd;
    result.retried_instances = retried_instances;
    result.retries = retries;
    result.total_fallbacks = total_fallbacks;
    result.model_mix = model_mix;
    result.instances = merged_instances;
    result.retry_history.push(entry);
    result
}

// ─── archive ──────────────────────────────────────────────────────────────────

/// Archive trajectory files for the selected instances to
/// `{sweep_dir}/.retry/{retry_id}/{instance_id}.traj.json` before the
/// agent loop overwrites them.
pub fn archive_trajectories(
    sweep_dir: &Path,
    selected: &[&InstanceResult],
    retry_id: &str,
) -> Result<(), Error> {
    let archive_root = sweep_dir.join(".retry").join(retry_id);
    std::fs::create_dir_all(&archive_root)?;

    for inst in selected {
        let id = &inst.instance_id;
        // Try nested layout first, then legacy flat layout.
        let src = sweep_dir.join(id).join("run-1.traj.json");
        let src = if src.exists() {
            src
        } else {
            sweep_dir.join(format!("{id}.traj.json"))
        };

        if !src.exists() {
            continue;
        }

        let dst = archive_root.join(format!("{id}.traj.json"));
        std::fs::copy(&src, &dst)?;

        // Also archive the patch file so it can be restored if the retry is cancelled.
        let patch_src = existing_patch_path_for_run(sweep_dir, id, 1);
        if patch_src.exists() {
            let patch_dst = archive_root.join(format!("{id}.patch"));
            std::fs::copy(&patch_src, &patch_dst)?;
        }
    }

    Ok(())
}

// ─── pre-retry backup ─────────────────────────────────────────────────────────

/// Save `results.json` to `{sweep_dir}/.retry/{retry_id}/pre-retry.json` so
/// the original sweep state can be manually restored if the process is killed
/// unexpectedly (SIGKILL, OOM). For graceful cancellation (SIGINT/SIGTERM)
/// `restore_missing_trajectories` handles the revert automatically.
pub fn save_pre_retry_backup(
    sweep_dir: &Path,
    results: &SweepResults,
    retry_id: &str,
) -> Result<(), Error> {
    let archive_root = sweep_dir.join(".retry").join(retry_id);
    std::fs::create_dir_all(&archive_root)?;
    let backup_path = archive_root.join("pre-retry.json");
    // Use the artifact serializer so the backup includes artifact_kind/schema_version
    // headers; load_sweep_results calls classify_json_value which requires them.
    let json =
        crate::artifact::to_string_pretty(crate::artifact::ArtifactKind::SweepResults, results)?;
    std::fs::write(&backup_path, json.as_bytes())?;
    Ok(())
}

/// Copy `pre-retry.json` back to `results.json` to undo a failed retry attempt.
/// Called when `swebench::run()` returns a hard error before any instances ran.
pub fn restore_pre_retry_backup(sweep_dir: &Path, retry_id: &str) -> Result<(), Error> {
    let backup_path = sweep_dir
        .join(".retry")
        .join(retry_id)
        .join("pre-retry.json");
    if backup_path.exists() {
        let results_path = sweep_dir.join("results.json");
        std::fs::copy(&backup_path, &results_path)?;
    }
    Ok(())
}

// ─── partial-results restore ──────────────────────────────────────────────────

/// Restore archived trajectory files for any selected instances whose new
/// trajectory is absent, empty, invalid JSON, or carries `exit_reason =
/// "cancelled"` (written by swebench when an in-flight task is cut short by
/// SIGINT/SIGTERM before it could complete).
///
/// This gives `bench retry` the same partial-results guarantee as the sweep
/// itself: completed instances keep their new data, in-flight instances revert
/// to the last known-good archived trajectory.
pub fn restore_missing_trajectories(
    sweep_dir: &Path,
    selected: &[&InstanceResult],
    retry_id: &str,
) -> Result<(), Error> {
    for inst in selected {
        let id = &inst.instance_id;
        let live_path = sweep_dir.join(id).join("run-1.traj.json");

        if needs_trajectory_restore(&live_path) {
            let archive_dir = sweep_dir.join(".retry").join(retry_id);
            let archived_traj = archive_dir.join(format!("{id}.traj.json"));
            if archived_traj.exists() {
                std::fs::create_dir_all(sweep_dir.join(id))?;
                std::fs::copy(&archived_traj, &live_path)?;
                tracing::debug!(
                    instance_id = %id,
                    "restored archived trajectory for in-flight/incomplete instance"
                );
            }
            // Restore the patch file too so a cancelled retry doesn't leave the
            // original summary row pointing at a patch from the abandoned run.
            let archived_patch = archive_dir.join(format!("{id}.patch"));
            if archived_patch.exists() {
                let dest_patch = patch_path_for_run(sweep_dir, id, 1);
                if let Some(parent) = dest_patch.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&archived_patch, &dest_patch)?;
            }
        }
    }
    Ok(())
}

/// Returns `true` when the trajectory at `path` should be replaced with the
/// archived copy because it was never written (missing), is empty, is not
/// valid JSON, or records `exit_reason = "cancelled"`.
fn needs_trajectory_restore(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return true;
    };
    if content.trim().is_empty() {
        return true;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
        return true;
    };
    // In-flight instances cancelled by SIGINT get exit_reason = "cancelled".
    let exit_reason = v
        .get("info")
        .and_then(|i| i.get("exit_reason"))
        .and_then(|e| e.as_str())
        .unwrap_or("");
    exit_reason == "cancelled" || v.get("info").is_none()
}

// ─── harness mismatch check ────────────────────────────────────────────────────

/// Returns `true` when the manifest's recorded harness git SHA differs from
/// the current binary's SHA. Returns `false` when either SHA is unavailable
/// (treated as "can't compare, proceed safely").
pub fn detect_harness_mismatch(results: &SweepResults) -> bool {
    detect_harness_mismatch_with_sha(results, current_git_sha().as_deref())
}

/// Testable inner form: accepts an explicit `current_sha` rather than running
/// `git rev-parse HEAD`. Pass `None` to simulate "git not available".
pub fn detect_harness_mismatch_with_sha(results: &SweepResults, current_sha: Option<&str>) -> bool {
    let Some(manifest) = &results.manifest else {
        return false;
    };
    let Some(manifest_sha) = &manifest.harness.git_sha else {
        return false;
    };
    let Some(sha) = current_sha else {
        return false;
    };
    manifest_sha.as_str() != sha
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

// ─── ID generation ────────────────────────────────────────────────────────────

/// Generate a UUID v4-style identifier from the current timestamp and process
/// id without requiring the `uuid` crate.
pub fn generate_retry_id() -> String {
    use sha2::{Digest, Sha256};
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let mut hasher = Sha256::new();
    hasher.update(ts.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    let h = hasher.finalize();
    let b = &h[..16];
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6] & 0x0F,
        b[7],
        (b[8] & 0x3F) | 0x80,
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

// ─── load results ─────────────────────────────────────────────────────────────

/// Load and classify `results.json` from a sweep directory.
pub fn load_sweep_results(sweep_dir: &Path) -> Result<SweepResults, Error> {
    use crate::artifact::ArtifactKind;
    use crate::artifact::classify_json_value;

    let results_path = sweep_dir.join("results.json");
    let text = std::fs::read_to_string(&results_path).map_err(|e| {
        Error::Trajectory(format!(
            "bench retry: cannot read {}: {e}",
            results_path.display()
        ))
    })?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Trajectory(format!("bench retry: malformed results.json: {e}")))?;

    classify_json_value(
        &value,
        ArtifactKind::SweepResults,
        results_path.display().to_string(),
    )
    .map_err(|e| Error::Trajectory(e.to_string()))?;

    let results: SweepResults = serde_json::from_value(value)
        .map_err(|e| Error::Trajectory(format!("bench retry: cannot deserialize results: {e}")))?;
    Ok(results)
}

// ─── build retry history entry ────────────────────────────────────────────────

/// Build the `RetryHistoryEntry` from pre/post sweep stats.
pub fn build_history_entry(
    retry_id: &str,
    selected: &[&InstanceResult],
    selection: RetrySelection,
    override_delta: OverrideDelta,
    harness_mismatch: bool,
    pre: &SweepResults,
    post: &SweepResults,
) -> RetryHistoryEntry {
    let timestamp_utc = chrono::Utc::now().to_rfc3339();
    let pre_resolved: u32 = pre.instances.iter().map(|r| r.resolved_count).sum();
    let post_resolved: u32 = post.instances.iter().map(|r| r.resolved_count).sum();

    RetryHistoryEntry {
        retry_id: retry_id.to_owned(),
        timestamp_utc,
        selection,
        override_delta,
        count: selected.len(),
        harness_mismatch,
        pre_submitted: pre.submitted,
        pre_errored: pre.errored,
        pre_resolved_count: pre_resolved as usize,
        post_submitted: post.submitted,
        post_errored: post.errored,
        post_resolved_count: post_resolved as usize,
    }
}

// ─── trait helpers used in merge ──────────────────────────────────────────────

trait InstanceResultExt {
    fn with_patch(&self) -> bool;
    fn patch_empty(&self) -> bool;
}

impl InstanceResultExt for InstanceResult {
    fn with_patch(&self) -> bool {
        self.patch_present && self.non_empty_patch
    }
    fn patch_empty(&self) -> bool {
        self.patch_present && !self.non_empty_patch
    }
}
