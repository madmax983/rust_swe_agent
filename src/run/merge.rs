//! Offline merge of independent sharded sweep result directories.
//!
//! `bench merge --shard <dir> --shard <dir> ... --output <dir>` recombines K
//! completed sweep directories into one canonical aggregate — arithmetically
//! correct, provenance-preserving, and a drop-in for `bench evaluate`, `bench
//! report`, `bench triage`, and `bench audit`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cli::args::{MergeCmd, MergeCollisionPolicy};
use crate::error::Error;
use crate::run::swebench::{
    MergeShardProvenance, SWEEP_STATUS_COMPLETED, SweepResults, write_sweep_results_atomic,
};

// ── public types ──────────────────────────────────────────────────────────────

/// Summary of a `bench merge` run, emitted via `--format text|json`.
#[derive(Debug, Serialize)]
pub struct MergeReport {
    /// Per-shard summary: label, dir, instance count.
    pub shards: Vec<MergeShardSummary>,
    /// Total instances in the merged output (after collision resolution).
    pub total_instances: usize,
    /// Number of instance IDs that appeared in more than one shard.
    pub duplicates: usize,
    /// The collision policy that was applied.
    pub collision_policy: String,
    /// Path to the output directory.
    pub output_dir: String,
    // Combined top-line metrics:
    pub total_cost_usd: f64,
    pub submitted: usize,
    pub errored: usize,
    pub resolved: usize,
    pub pass_at_k: f64,
}

#[derive(Debug, Serialize)]
pub struct MergeShardSummary {
    pub label: String,
    pub dir: String,
    pub instance_count: usize,
}

// ── entry point ───────────────────────────────────────────────────────────────

pub fn run(args: &MergeCmd) -> Result<MergeReport, Error> {
    // Require ≥2 shards
    if args.shards.len() < 2 {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "bench merge requires at least 2 --shard arguments (merging one shard is just a copy)"
                .to_owned(),
        )));
    }

    // Validate label count if provided
    if !args.labels.is_empty() && args.labels.len() != args.shards.len() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "number of --label values ({}) must match number of --shard values ({})",
            args.labels.len(),
            args.shards.len(),
        ))));
    }

    // Build shard labels
    let labels: Vec<String> = args
        .shards
        .iter()
        .enumerate()
        .map(|(i, path)| {
            args.labels.get(i).cloned().unwrap_or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("shard")
                    .to_owned()
            })
        })
        .collect();

    // Load and validate each shard
    let mut loaded: Vec<(String, PathBuf, SweepResults)> = Vec::with_capacity(args.shards.len());
    for (label, dir) in labels.iter().zip(args.shards.iter()) {
        let results = load_shard(label, dir)?;
        loaded.push((label.clone(), dir.clone(), results));
    }

    // Check provenance compatibility
    check_provenance_compatibility(&loaded)?;

    // Detect collisions and build union instance list
    let policy = args.on_collision;
    let (union_instances, duplicates, owner_shard) = build_union_instances(&loaded, policy)?;

    // Prepare output directory
    prepare_output_dir(&args.output, args.force)?;

    // Copy artifacts for each instance from its owning shard
    copy_artifacts(&loaded, &union_instances, &owner_shard, &args.output)?;

    // Merge evaluation.json if any shard has one
    merge_evaluation_json(&loaded, &owner_shard, &args.output)?;

    // Recompute aggregates over the union
    let base = &loaded[0].2;
    let mut merged = crate::run::swebench::recompute_aggregates(base, union_instances.clone());

    // Build merged manifest (clone shard0's, add merged_from)
    let merged_from: Vec<MergeShardProvenance> = loaded
        .iter()
        .map(|(label, dir, results)| {
            let manifest = results.manifest.as_ref();
            MergeShardProvenance {
                label: label.clone(),
                dir: dir.to_string_lossy().to_string(),
                model: manifest.map(|m| m.model.name.clone()),
                config_sha256: manifest.map(|m| config_hash(&m.config.resolved)),
                dataset_sha256: manifest.map(|m| m.dataset.sha256.clone()),
                git_sha: manifest.and_then(|m| m.harness.git_sha.clone()),
                instance_count: results.instances.len(),
            }
        })
        .collect();

    if let Some(ref mut manifest) = merged.manifest {
        manifest.merged_from = Some(merged_from);
        manifest.source = Some("merge".to_owned());
        manifest.dataset.instance_count = union_instances.len();
        manifest.dataset.selected_row_count = union_instances.len();
        manifest.dataset.post_filter_row_count = union_instances.len();
    }

    // Update sweep_status to completed
    merged.sweep_status = SWEEP_STATUS_COMPLETED.into();

    // Write results.json
    let results_path = args.output.join("results.json");
    write_sweep_results_atomic(&results_path, &merged)?;

    // Build report
    let shard_summaries: Vec<MergeShardSummary> = loaded
        .iter()
        .map(|(label, dir, results)| MergeShardSummary {
            label: label.clone(),
            dir: dir.to_string_lossy().to_string(),
            instance_count: results.instances.len(),
        })
        .collect();

    let resolved = union_instances
        .iter()
        .filter(|r| r.resolved_count > 0)
        .count();

    Ok(MergeReport {
        shards: shard_summaries,
        total_instances: union_instances.len(),
        duplicates,
        collision_policy: policy_label(policy),
        output_dir: args.output.to_string_lossy().to_string(),
        total_cost_usd: merged.estimated_cost_usd,
        submitted: merged.submitted,
        errored: merged.errored,
        resolved,
        pass_at_k: merged.pass_at_k,
    })
}

// ── rendering ─────────────────────────────────────────────────────────────────

impl MergeReport {
    pub fn render_text(&self) {
        println!("Shards merged: {}", self.shards.len());
        for s in &self.shards {
            println!(
                "  {:20}  {:6} instances  {}",
                s.label, s.instance_count, s.dir
            );
        }
        println!();
        println!(
            "Total instances: {} ({} duplicates resolved via {})",
            self.total_instances, self.duplicates, self.collision_policy
        );
        println!("Output: {}", self.output_dir);
        println!();
        println!("Top-line metrics:");
        println!("  total_cost_usd : {:.6}", self.total_cost_usd);
        println!("  submitted      : {}", self.submitted);
        println!("  errored        : {}", self.errored);
        println!("  resolved       : {}", self.resolved);
        println!("  pass_at_k      : {:.4}", self.pass_at_k);
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn load_shard(label: &str, dir: &Path) -> Result<SweepResults, Error> {
    let results_path = dir.join("results.json");
    if !results_path.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: shard '{label}' is missing results.json (path: {})",
            results_path.display()
        ))));
    }

    let text = fs::read_to_string(&results_path).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: failed to read shard '{label}' results.json: {e}"
        )))
    })?;

    let results: SweepResults = serde_json::from_str(&text).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: failed to parse shard '{label}' results.json: {e}"
        )))
    })?;

    if results.sweep_status != SWEEP_STATUS_COMPLETED {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: shard '{label}' is not completed (sweep_status={}); \
             only completed sweeps can be merged",
            results.sweep_status
        ))));
    }

    if results.manifest.is_none() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: shard '{label}' has no manifest in results.json; \
             manifest is required for provenance-preserving merge"
        ))));
    }

    Ok(results)
}

fn check_provenance_compatibility(loaded: &[(String, PathBuf, SweepResults)]) -> Result<(), Error> {
    let (label0, _, ref0) = &loaded[0];
    let manifest0 = ref0.manifest.as_ref().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: shard '{label0}' has no manifest (should have been caught in load_shard)"
        )))
    })?;
    let dataset_sha0 = &manifest0.dataset.sha256;
    let model0 = &manifest0.model.name;
    let config0 = config_hash(&manifest0.config.resolved);

    for (label, _, results) in loaded.iter().skip(1) {
        let manifest = results.manifest.as_ref().ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' has no manifest (should have been caught in load_shard)"
            )))
        })?;

        if &manifest.dataset.sha256 != dataset_sha0 {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' dataset_sha256 '{}' differs from shard '{label0}' '{}'; \
                 bench merge requires identical dataset across shards \
                 (use bench matrix for cross-dataset comparison)",
                manifest.dataset.sha256, dataset_sha0
            ))));
        }

        if &manifest.model.name != model0 {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' model '{}' differs from shard '{label0}' '{}'; \
                 bench merge requires identical model across shards \
                 (use bench matrix for cross-model comparison)",
                manifest.model.name, model0
            ))));
        }

        if config_hash(&manifest.config.resolved) != config0 {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' resolved config differs from shard '{label0}'; \
                 bench merge requires identical config across shards \
                 (use bench matrix for cross-config comparison)"
            ))));
        }
    }

    Ok(())
}

type UnionResult = Result<
    (
        Vec<crate::run::swebench::InstanceResult>,
        usize,
        HashMap<String, usize>,
    ),
    Error,
>;

/// Build the union instance list, detecting/resolving collisions.
/// Returns (union_instances, duplicates_count, owner_shard_index per instance_id).
fn build_union_instances(
    loaded: &[(String, PathBuf, SweepResults)],
    policy: MergeCollisionPolicy,
) -> UnionResult {
    let mut seen: HashMap<String, usize> = HashMap::new(); // id → shard index
    let mut collisions: Vec<(String, usize, usize)> = Vec::new(); // (id, first_shard, second_shard)
    let mut duplicates = 0;

    // First pass: detect all collisions
    for (shard_idx, (_, _, results)) in loaded.iter().enumerate() {
        for inst in &results.instances {
            match seen.get(&inst.instance_id) {
                None => {
                    seen.insert(inst.instance_id.clone(), shard_idx);
                }
                Some(&first_shard) => {
                    collisions.push((inst.instance_id.clone(), first_shard, shard_idx));
                }
            }
        }
    }

    if !collisions.is_empty() {
        match policy {
            MergeCollisionPolicy::Error => {
                let ids: Vec<String> = collisions
                    .iter()
                    .map(|(id, first, second)| {
                        format!(
                            "  '{}' in shards '{}' and '{}'",
                            id, loaded[*first].0, loaded[*second].0
                        )
                    })
                    .collect();
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "merge: {} instance ID collision(s) detected (use --on-collision \
                     first-wins or last-wins to override):\n{}",
                    collisions.len(),
                    ids.join("\n")
                ))));
            }
            MergeCollisionPolicy::FirstWins => {
                // owner already set to first occurrence; just count
                duplicates = collisions.len();
            }
            MergeCollisionPolicy::LastWins => {
                // Override owner to last occurrence
                for (id, _, last_shard) in &collisions {
                    seen.insert(id.clone(), *last_shard);
                }
                duplicates = collisions.len();
            }
        }
    }

    // Build ordered union (shard order, then instance order within shard)
    let mut included: HashSet<String> = HashSet::new();
    let mut union: Vec<crate::run::swebench::InstanceResult> = Vec::new();

    for (shard_idx, (_, _, results)) in loaded.iter().enumerate() {
        for inst in &results.instances {
            let owner = seen.get(&inst.instance_id).copied().unwrap_or(shard_idx);
            if owner == shard_idx && !included.contains(&inst.instance_id) {
                included.insert(inst.instance_id.clone());
                union.push(inst.clone());
            }
        }
    }

    Ok((union, duplicates, seen))
}

fn prepare_output_dir(output: &Path, force: bool) -> Result<(), Error> {
    if output.exists() {
        let is_empty = output.read_dir().is_ok_and(|mut d| d.next().is_none());
        if !is_empty {
            if !force {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "merge: output directory '{}' already exists and is non-empty; \
                     use --force to overwrite",
                    output.display()
                ))));
            }
            // --force: clear stale artifacts so the merged results.json↔trajectory
            // bijection that `bench audit` enforces stays intact.
            fs::remove_dir_all(output).map_err(|e| {
                Error::Config(crate::error::ConfigError::Invalid(format!(
                    "merge: failed to clear output directory '{}': {e}",
                    output.display()
                )))
            })?;
        }
    }
    fs::create_dir_all(output).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: failed to create output directory '{}': {e}",
            output.display()
        )))
    })?;
    Ok(())
}

/// Copy trajectory and patch artifacts for each included instance from its owning shard.
fn copy_artifacts(
    loaded: &[(String, PathBuf, SweepResults)],
    union_instances: &[crate::run::swebench::InstanceResult],
    owner_shard: &HashMap<String, usize>,
    output: &Path,
) -> Result<(), Error> {
    for inst in union_instances {
        let shard_idx = owner_shard.get(&inst.instance_id).copied().unwrap_or(0);
        let shard_dir = &loaded[shard_idx].1;
        copy_instance_artifacts(shard_dir, output, &inst.instance_id)?;
    }
    Ok(())
}

/// Copy all artifact files for a single instance from `src_sweep` to `dst_sweep`.
/// Supports both the nested layout (`<id>/run-N.traj.json`) and legacy flat layout
/// (`<id>.traj.json`). Copies the entire per-instance subdirectory if it exists.
fn copy_instance_artifacts(
    src_sweep: &Path,
    dst_sweep: &Path,
    instance_id: &str,
) -> Result<(), Error> {
    let inst_dir = src_sweep.join(instance_id);
    if inst_dir.is_dir() {
        // Nested layout: copy the whole per-instance directory
        let dst_inst_dir = dst_sweep.join(instance_id);
        copy_dir_recursive(&inst_dir, &dst_inst_dir).map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: failed to copy instance artifacts for '{instance_id}': {e}"
            )))
        })?;
    } else {
        // Legacy flat layout: copy <id>.traj.json and <id>.patch if they exist
        for ext in [".traj.json", ".patch"] {
            let src = src_sweep.join(format!("{instance_id}{ext}"));
            if src.exists() {
                let dst = dst_sweep.join(format!("{instance_id}{ext}"));
                fs::copy(&src, &dst).map_err(|e| {
                    Error::Config(crate::error::ConfigError::Invalid(format!(
                        "merge: failed to copy '{}': {e}",
                        src.display()
                    )))
                })?;
            }
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Merge evaluation.json from all shards. Normalizes legacy format to modern.
/// Writes merged output if any shard had eval data.
fn merge_evaluation_json(
    loaded: &[(String, PathBuf, SweepResults)],
    owner_shard: &HashMap<String, usize>,
    output: &Path,
) -> Result<(), Error> {
    let mut eval_entries: Vec<Value> = Vec::new();

    for (shard_idx, (label, shard_dir, _)) in loaded.iter().enumerate() {
        let eval_path = shard_dir.join("evaluation.json");
        if !eval_path.exists() {
            continue;
        }

        let text = fs::read_to_string(&eval_path).map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: failed to read shard '{label}' evaluation.json: {e}"
            )))
        })?;

        let eval: Value = serde_json::from_str(&text).map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: failed to parse shard '{label}' evaluation.json: {e}"
            )))
        })?;

        // Parse modern format: instances[].{instance_id, resolved}
        if let Some(instances) = eval.get("instances").and_then(Value::as_array) {
            for inst in instances {
                let id = inst
                    .get("instance_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                // Only include if this shard owns the instance
                if owner_shard.get(id).copied().unwrap_or(shard_idx) == shard_idx {
                    eval_entries.push(inst.clone());
                }
            }
        } else {
            // Legacy format: resolved_ids / submitted_ids arrays
            let mut resolved_ids: HashSet<&str> = HashSet::new();
            let mut submitted_ids: HashSet<&str> = HashSet::new();

            if let Some(ids) = eval.get("resolved_ids").and_then(Value::as_array) {
                for v in ids {
                    if let Some(id) = v.as_str() {
                        resolved_ids.insert(id);
                    }
                }
            }
            if let Some(ids) = eval.get("submitted_ids").and_then(Value::as_array) {
                for v in ids {
                    if let Some(id) = v.as_str() {
                        submitted_ids.insert(id);
                    }
                }
            }
            for id in resolved_ids.union(&submitted_ids) {
                if owner_shard.get(*id).copied().unwrap_or(shard_idx) == shard_idx {
                    let resolved = resolved_ids.contains(id);
                    eval_entries.push(serde_json::json!({
                        "instance_id": id,
                        "resolved": resolved
                    }));
                }
            }
        }
    }

    if eval_entries.is_empty() {
        return Ok(());
    }

    let merged_eval = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 3},
        "instances": eval_entries
    });

    let eval_json = serde_json::to_string_pretty(&merged_eval).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: failed to serialize evaluation.json: {e}"
        )))
    })?;
    fs::write(output.join("evaluation.json"), eval_json).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: failed to write evaluation.json: {e}"
        )))
    })?;

    Ok(())
}

fn config_hash(resolved_toml: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(resolved_toml.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn policy_label(policy: MergeCollisionPolicy) -> String {
    match policy {
        MergeCollisionPolicy::Error => "error".to_owned(),
        MergeCollisionPolicy::FirstWins => "first-wins".to_owned(),
        MergeCollisionPolicy::LastWins => "last-wins".to_owned(),
    }
}
