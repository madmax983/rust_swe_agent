//! `bench reproduce`: replay a saved sweep from its `ProvenanceManifest`.
//!
//! Reads `results.json` from a source sweep directory, performs a
//! preflight manifest-diff, launches a fresh sweep using the recorded
//! settings, and writes a `reproducibility.json` comparison artifact.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error};
use crate::run::swebench::{
    InstanceResult, ProvenanceManifest, SweepResults, patch_path_for_run, resolved_count,
};

// ── public args ─────────────────────────────────────────────────────────────

/// Arguments for `bench reproduce`.
pub struct ReproduceArgs {
    /// Source sweep directory to reproduce (must contain `results.json` with a
    /// `ProvenanceManifest`).
    pub from: PathBuf,
    /// Output directory for the new sweep artifacts and `reproducibility.json`.
    pub output: PathBuf,
    /// Drift field names that are allowed to diverge without aborting.
    /// Hard-drift fields not in this list abort with a non-zero exit.
    pub allow_drift: Vec<String>,
    /// Limit replay to at most N instances (partial replay).
    pub limit: Option<usize>,
    /// Instance-id filter (comma-separated ids or `@file`).
    pub filter: Option<String>,
    /// Override per-task USD ceiling for the replay sweep.
    pub per_task_budget_usd: Option<f64>,
    /// Skip the model-endpoint probe during preflight (useful in CI/dry-run).
    pub skip_model_probe: bool,
}

// ── drift types ─────────────────────────────────────────────────────────────

/// Severity of a manifest field divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftSeverity {
    /// Abort unless whitelisted via `--allow-drift`.
    Hard,
    /// Warn but do not abort.
    Soft,
}

/// A single field divergence between the source manifest and the current
/// environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftField {
    /// Dot-separated field path, e.g. `"harness.git_sha"`.
    pub field: String,
    pub severity: DriftSeverity,
    pub source_value: Option<String>,
    pub current_value: Option<String>,
    pub message: String,
}

impl DriftField {
    /// Returns `true` when this field appears in `allow_list`.
    #[must_use]
    pub fn is_whitelisted(&self, allow_list: &[String]) -> bool {
        allow_list.iter().any(|a| a == &self.field)
    }
}

// ── report types ─────────────────────────────────────────────────────────────

/// Per-instance pair in `reproducibility.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceComparisonEntry {
    pub instance_id: String,
    pub original_resolved: bool,
    pub replay_resolved: bool,
    pub original_failure_category: Option<String>,
    pub replay_failure_category: Option<String>,
    /// `true` when both runs produced a byte-identical patch file.
    pub patch_identical: bool,
}

/// Aggregate reproducibility counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReproducibilityAggregate {
    /// Both original and replay resolved.
    pub matched: usize,
    /// Original unresolved, replay resolved.
    pub flipped_to_resolved: usize,
    /// Original resolved, replay unresolved.
    pub flipped_to_unresolved: usize,
    /// Both unresolved with the same failure category.
    pub both_unresolved_same_category: usize,
    /// Both unresolved but with different failure categories.
    pub both_unresolved_different_category: usize,
    /// Instance present in original but absent from replay (or vice versa).
    pub errored: usize,
}

/// The `reproduced_from` block embedded in the new sweep's manifest and in
/// `reproducibility.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReproducedFrom {
    /// SHA-256 hash of the source manifest JSON bytes.
    pub manifest_hash: String,
    /// Absolute path of the source sweep directory.
    pub sweep_dir: String,
}

/// The `reproducibility.json` artifact written to the output directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReproducibilityReport {
    /// Path of the source sweep directory.
    pub source_sweep: String,
    /// Hash of the source sweep's manifest (prefixed `sha256:`).
    pub source_manifest_hash: String,
    /// Back-pointer for the new sweep's manifest.
    pub reproduced_from: ReproducedFrom,
    /// Per-instance comparison entries.
    pub instances: Vec<InstanceComparisonEntry>,
    /// Aggregate counts over all instances.
    pub aggregate: ReproducibilityAggregate,
}

// ── public functions ─────────────────────────────────────────────────────────

/// Load the `ProvenanceManifest` from `<sweep_dir>/results.json`.
///
/// Returns `Err` if the file is missing, unparseable, or has no manifest block.
pub fn load_manifest_from_sweep(sweep_dir: &Path) -> Result<ProvenanceManifest, Error> {
    let results_path = sweep_dir.join("results.json");
    let file = std::fs::File::open(&results_path).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!(
                "cannot read results.json from {}: {e}",
                results_path.display()
            ),
        ))
    })?;
    let results: SweepResults = serde_json::from_reader(std::io::BufReader::new(file))?;
    results.manifest.ok_or_else(|| {
        Error::Config(ConfigError::Invalid(format!(
            "sweep directory {} has no provenance manifest (manifest block) in results.json",
            sweep_dir.display()
        )))
    })
}

/// Compare two manifests and return every field that diverges.
///
/// Hard divergences (harness SHA, dataset hash, model name, resolved config)
/// abort unless whitelisted. Soft divergences (Rust version, host OS) are
/// reported as warnings only.
#[must_use]
pub fn compare_manifests(
    original: &ProvenanceManifest,
    current: &ProvenanceManifest,
) -> Vec<DriftField> {
    let mut drifts = Vec::new();

    // Hard: harness git SHA (only when both are present)
    if let (Some(orig_sha), Some(curr_sha)) = (&original.harness.git_sha, &current.harness.git_sha)
    {
        if orig_sha != curr_sha {
            drifts.push(DriftField {
                field: "harness.git_sha".into(),
                severity: DriftSeverity::Hard,
                source_value: Some(orig_sha.clone()),
                current_value: Some(curr_sha.clone()),
                message: format!("harness git SHA changed: {orig_sha} → {curr_sha}"),
            });
        }
    }

    // Hard: dataset sha256 (when both are non-empty)
    if !original.dataset.sha256.is_empty()
        && !current.dataset.sha256.is_empty()
        && original.dataset.sha256 != current.dataset.sha256
    {
        drifts.push(DriftField {
            field: "dataset.sha256".into(),
            severity: DriftSeverity::Hard,
            source_value: Some(original.dataset.sha256.clone()),
            current_value: Some(current.dataset.sha256.clone()),
            message: format!(
                "dataset sha256 changed: {} → {}",
                original.dataset.sha256, current.dataset.sha256
            ),
        });
    }

    // Hard: model name
    if original.model.name != current.model.name {
        drifts.push(DriftField {
            field: "model.name".into(),
            severity: DriftSeverity::Hard,
            source_value: Some(original.model.name.clone()),
            current_value: Some(current.model.name.clone()),
            message: format!(
                "model name changed: {} → {}",
                original.model.name, current.model.name
            ),
        });
    }

    // Hard: resolved config (direct string comparison — both sides are TOML)
    if original.config.resolved != current.config.resolved {
        drifts.push(DriftField {
            field: "config.resolved".into(),
            severity: DriftSeverity::Hard,
            source_value: None,
            current_value: None,
            message: "resolved config changed (TOML differs from source sweep)".into(),
        });
    }

    // Soft: Rust compiler version (informational; does not affect correctness)
    if let (Some(orig_rv), Some(curr_rv)) = (
        &original.runtime.rust_version,
        &current.runtime.rust_version,
    ) {
        if orig_rv != curr_rv {
            drifts.push(DriftField {
                field: "runtime.rust_version".into(),
                severity: DriftSeverity::Soft,
                source_value: Some(orig_rv.clone()),
                current_value: Some(curr_rv.clone()),
                message: format!("Rust version changed: {orig_rv} → {curr_rv}"),
            });
        }
    }

    // Soft: host OS (cross-platform replay may behave differently)
    if original.runtime.host_os != current.runtime.host_os {
        drifts.push(DriftField {
            field: "runtime.host_os".into(),
            severity: DriftSeverity::Soft,
            source_value: Some(original.runtime.host_os.clone()),
            current_value: Some(current.runtime.host_os.clone()),
            message: format!(
                "host OS changed: {} → {}",
                original.runtime.host_os, current.runtime.host_os
            ),
        });
    }

    drifts
}

/// Return only the hard-drift fields that are **not** in `allow_list`.
#[must_use]
pub fn filter_hard_drifts<'a>(
    drifts: &'a [DriftField],
    allow_list: &[String],
) -> Vec<&'a DriftField> {
    drifts
        .iter()
        .filter(|d| d.severity == DriftSeverity::Hard && !d.is_whitelisted(allow_list))
        .collect()
}

/// Build a `ReproducibilityReport` by pairing original and replay instances.
///
/// Instances present in `original_instances` but absent from `replay_instances`
/// (by `instance_id`) are counted as `errored`.
#[must_use]
pub fn build_reproducibility_report(
    source_dir: &Path,
    source_manifest_hash: String,
    original_instances: &[InstanceResult],
    replay_instances: &[InstanceResult],
    output_dir: &Path,
) -> ReproducibilityReport {
    // Index replays by instance_id for O(1) lookup.
    let replay_map: HashMap<&str, &InstanceResult> = replay_instances
        .iter()
        .map(|r| (r.instance_id.as_str(), r))
        .collect();

    let mut instances = Vec::new();
    let mut aggregate = ReproducibilityAggregate::default();

    for orig in original_instances {
        let Some(replay) = replay_map.get(orig.instance_id.as_str()) else {
            aggregate.errored += 1;
            continue;
        };

        let original_resolved = resolved_count(orig) > 0;
        let replay_resolved = resolved_count(replay) > 0;

        let patch_identical = compare_patches(orig, replay, source_dir, output_dir);

        match (original_resolved, replay_resolved) {
            (true, true) => aggregate.matched += 1,
            (false, true) => aggregate.flipped_to_resolved += 1,
            (true, false) => aggregate.flipped_to_unresolved += 1,
            (false, false) => {
                if orig.failure_category == replay.failure_category {
                    aggregate.both_unresolved_same_category += 1;
                } else {
                    aggregate.both_unresolved_different_category += 1;
                }
            }
        }

        instances.push(InstanceComparisonEntry {
            instance_id: orig.instance_id.clone(),
            original_resolved,
            replay_resolved,
            original_failure_category: orig.failure_category.as_ref().map(|c| {
                serde_json::to_value(c)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{c:?}"))
            }),
            replay_failure_category: replay.failure_category.as_ref().map(|c| {
                serde_json::to_value(c)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{c:?}"))
            }),
            patch_identical,
        });
    }

    ReproducibilityReport {
        source_sweep: source_dir.display().to_string(),
        source_manifest_hash: source_manifest_hash.clone(),
        reproduced_from: ReproducedFrom {
            manifest_hash: source_manifest_hash,
            sweep_dir: source_dir.display().to_string(),
        },
        instances,
        aggregate,
    }
}

/// Compare patch files for an instance across two sweep directories.
///
/// Uses run index 1 (the sweep writer's convention) and compares via SHA-256
/// hashing to avoid loading large patch files fully into memory.
fn compare_patches(
    orig: &InstanceResult,
    replay: &InstanceResult,
    source_dir: &Path,
    output_dir: &Path,
) -> bool {
    if !orig.patch_present || !replay.patch_present {
        return false;
    }
    let orig_patch = patch_path_for_run(source_dir, &orig.instance_id, 1);
    let replay_patch = patch_path_for_run(output_dir, &replay.instance_id, 1);
    hash_file_sha256(&orig_patch) == hash_file_sha256(&replay_patch)
}

/// Hash a file's contents with SHA-256, reading it in 8 KiB chunks.
/// Returns `None` if the file cannot be opened or read.
fn hash_file_sha256(path: &Path) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(hasher.finalize().into())
}

/// Render a human-readable summary of a `ReproducibilityReport` to stdout.
///
/// Includes total instances, % matched on resolved status, % patch-identical,
/// aggregate counts, and the top-3 diverging failure categories.
#[must_use]
pub fn render_summary(report: &ReproducibilityReport) -> String {
    use std::fmt::Write as _;

    let total = report.instances.len() + report.aggregate.errored;
    let status_matched = report.aggregate.matched + report.aggregate.both_unresolved_same_category;

    #[allow(clippy::cast_precision_loss)]
    let pct = |n: usize| -> f64 {
        if total == 0 {
            0.0
        } else {
            n as f64 / total as f64 * 100.0
        }
    };

    let patch_identical_count = report
        .instances
        .iter()
        .filter(|e| e.patch_identical)
        .count();
    let pct_patch_identical = pct(patch_identical_count);
    let pct_status_matched = pct(status_matched);

    let mut out = format!(
        "reproduce: {total} instance(s), {pct_status_matched:.1}% matched resolved status, \
         {pct_patch_identical:.1}% patch-identical\n"
    );
    let _ = writeln!(
        out,
        "  matched={status_matched} \
         flipped_to_resolved={flipped_to_resolved} \
         flipped_to_unresolved={flipped_to_unresolved} \
         both_unresolved_diff_cat={both_unresolved_different_category} \
         errored={errored}",
        flipped_to_resolved = report.aggregate.flipped_to_resolved,
        flipped_to_unresolved = report.aggregate.flipped_to_unresolved,
        both_unresolved_different_category = report.aggregate.both_unresolved_different_category,
        errored = report.aggregate.errored,
    );

    let top3 = top_diverging_failure_categories(&report.instances, 3);
    if !top3.is_empty() {
        out.push_str("  top diverging failure categories:");
        for (cat, count) in &top3 {
            let _ = write!(out, " {cat}×{count}");
        }
        out.push('\n');
    }

    out
}

/// Collect failure categories from instances where resolved status diverged or
/// failure category changed, returning the top `n` by occurrence count.
///
/// "Diverging" means any instance where:
/// - it flipped to unresolved (replay category)
/// - it flipped to resolved (original category — the "was failing" label)
/// - both unresolved but different categories (both sides)
#[must_use]
pub fn top_diverging_failure_categories(
    instances: &[InstanceComparisonEntry],
    n: usize,
) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();

    for entry in instances {
        let diverging = match (entry.original_resolved, entry.replay_resolved) {
            (true, false) => {
                // flipped to unresolved — note the new failure category
                entry.replay_failure_category.iter().collect::<Vec<_>>()
            }
            (false, true) => {
                // flipped to resolved — note what was failing before
                entry.original_failure_category.iter().collect::<Vec<_>>()
            }
            (false, false) if entry.original_failure_category != entry.replay_failure_category => {
                // same unresolved outcome but different category — note both
                entry
                    .original_failure_category
                    .iter()
                    .chain(entry.replay_failure_category.iter())
                    .collect::<Vec<_>>()
            }
            _ => vec![],
        };
        for cat in diverging {
            *counts.entry(cat.clone()).or_insert(0) += 1;
        }
    }

    let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    sorted.truncate(n);
    sorted
}

/// Write `reproducibility.json` to the output directory.
pub fn write_report(report: &ReproducibilityReport, output_dir: &Path) -> Result<(), Error> {
    let path = output_dir.join("reproducibility.json");
    let text = serde_json::to_string_pretty(report)?;
    std::fs::write(&path, text).map_err(Error::Io)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn compare_patches_returns_false_when_neither_patch_present() {
        let dir = tempfile::tempdir().unwrap();
        let orig = make_instance("t1", false);
        let replay = make_instance("t1", false);
        assert!(!compare_patches(&orig, &replay, dir.path(), dir.path()));
    }

    fn make_instance(id: &str, patch_present: bool) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "submitted".into(),
            outcome: Some("submitted".into()),
            failure_category: None,
            steps: Some(1),
            cost_usd: None,
            prompt_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            duration_secs: None,
            error: None,
            github_pr_error: None,
            patch_present,
            non_empty_patch: patch_present,
            attempts: 1,
            retry_reasons: vec![],
            runs: 1,
            resolved_count: 1,
            pass_at_1: true,
            tests_run_before_submit: false,
            last_tests_passed: None,
            fallback_count: None,
            final_model: None,
        }
    }
}
