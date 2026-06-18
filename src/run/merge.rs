//! Offline merge of independent sharded sweep result directories.
//!
//! `bench merge --shard <dir> --shard <dir> ... --output <dir>` recombines K
//! completed sweep directories into one canonical aggregate — arithmetically
//! correct, provenance-preserving, and a drop-in for `bench evaluate`, `bench
//! report`, `bench triage`, and `bench audit`.

use std::collections::{BTreeMap, HashMap, HashSet};
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

#[allow(clippy::too_many_lines)]
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

    // Refuse to write into (or delete, under --force) a directory that overlaps an
    // input shard — that would destroy the very artifacts we are about to copy.
    validate_output_isolation(&args.output, &args.shards)?;

    // Prepare output directory
    prepare_output_dir(&args.output, args.force)?;

    // Copy artifacts for each instance from its owning shard
    copy_artifacts(&loaded, &union_instances, &owner_shard, &args.output)?;

    // Merge prediction files (all_preds*.jsonl + metadata) for `bench evaluate`.
    merge_predictions(&loaded, &owner_shard, &args.output)?;

    // Merge evaluation.json if every shard has one; returns the authoritative
    // resolved count when a merged evaluation file was actually written.
    let eval_resolved = merge_evaluation_json(&loaded, &owner_shard, &args.output)?;

    // Recompute aggregates over the union
    let base = &loaded[0].2;
    let mut merged = crate::run::swebench::recompute_aggregates(base, union_instances.clone());

    // Carry every shard's retry_history, not just shard 0's — downstream freshness
    // checks (e.g. bench budget-fit) use it to reject stale post-retry artifacts.
    merged.retry_history = loaded
        .iter()
        .flat_map(|(_, _, r)| r.retry_history.iter().cloned())
        .collect();

    // `recompute_aggregates` counts outcomes per-instance, but a pass@k sweep has
    // one collapsed row per task while results.json records per-run *slots* (and
    // `bench audit` recomputes the same way from the copied trajectories). Recount
    // submitted/errored/skipped/budget_halted per slot so the merged sweep audits.
    let slots = recount_slot_outcomes(&args.output, &union_instances)?;
    merged.submitted = slots.submitted;
    merged.errored = slots.errored;
    merged.skipped = slots.skipped;
    merged.budget_halted = slots.budget_halted;

    // Rebuild subset metadata so it describes the merged union rather than shard 0;
    // downstream `bench compare` keys "same subset?" off this filter spec.
    let union_filter_spec = merged_filter_spec(base, &union_instances);
    merged.filter_spec = union_filter_spec.clone();

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
        // Keep `instance_count` (the source dataset cardinality) from the
        // shard manifest — overwriting it with the union size would make a
        // 400-of-2294 subset look like a 400-row dataset to evaluation provenance.
        // Only the subset/post-filter counts describe the merged selection.
        manifest.dataset.selected_row_count = union_instances.len();
        manifest.dataset.post_filter_row_count = union_instances.len();
        manifest.dataset.filter_spec = Some(union_filter_spec);
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

    // Prefer the authoritative resolved count from the merged evaluation.json
    // when present; otherwise fall back to the pass@k/submission proxy.
    let resolved = eval_resolved.unwrap_or_else(|| {
        union_instances
            .iter()
            .filter(|r| r.resolved_count > 0)
            .count()
    });

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

/// Copy all artifact files for a single instance from `src_sweep` to `dst_sweep`,
/// preserving the source shard's on-disk layout so the copied paths still satisfy
/// `bench audit`. Recognizes the same three layouts audit does:
///   * nested      `<id>/run-N.traj.json`   (whole per-instance dir)
///   * legacy flat `<id>.traj.json` / `<id>.patch`
///   * bundled     `trajectories/<id>.traj.json` / `patches/<id>.patch`
fn copy_instance_artifacts(
    src_sweep: &Path,
    dst_sweep: &Path,
    instance_id: &str,
) -> Result<(), Error> {
    let copy_file = |src: PathBuf, dst: PathBuf| -> Result<(), Error> {
        if src.exists() {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    Error::Config(crate::error::ConfigError::Invalid(format!(
                        "merge: failed to create '{}': {e}",
                        parent.display()
                    )))
                })?;
            }
            fs::copy(&src, &dst).map_err(|e| {
                Error::Config(crate::error::ConfigError::Invalid(format!(
                    "merge: failed to copy '{}': {e}",
                    src.display()
                )))
            })?;
        }
        Ok(())
    };

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
        // Legacy flat layout: <id>.traj.json / <id>.patch at the sweep root.
        for ext in [".traj.json", ".patch"] {
            let name = format!("{instance_id}{ext}");
            copy_file(src_sweep.join(&name), dst_sweep.join(&name))?;
        }
        // Bundled layout: trajectories/<id>.traj.json and patches/<id>.patch.
        copy_file(
            src_sweep
                .join("trajectories")
                .join(format!("{instance_id}.traj.json")),
            dst_sweep
                .join("trajectories")
                .join(format!("{instance_id}.traj.json")),
        )?;
        copy_file(
            src_sweep
                .join("patches")
                .join(format!("{instance_id}.patch")),
            dst_sweep
                .join("patches")
                .join(format!("{instance_id}.patch")),
        )?;
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
///
/// A modern `evaluation.json` requires an entry for every on-disk trajectory, or
/// `bench audit` reports `audit:orphan:evaluation`. We therefore only emit a
/// merged file when *every* shard carries evaluation data; if evaluation is
/// present in only some shards we omit it entirely (and warn) so the merged
/// sweep still audits cleanly as an unevaluated sweep.
fn merge_evaluation_json(
    loaded: &[(String, PathBuf, SweepResults)],
    owner_shard: &HashMap<String, usize>,
    output: &Path,
) -> Result<Option<usize>, Error> {
    let shards_with_eval = loaded
        .iter()
        .filter(|(_, dir, _)| dir.join("evaluation.json").exists())
        .count();
    if shards_with_eval == 0 {
        return Ok(None);
    }
    if shards_with_eval < loaded.len() {
        eprintln!(
            "merge: warning: evaluation.json present in {shards_with_eval}/{} shards; \
             omitting merged evaluation.json so the output audits as unevaluated \
             (re-run `bench evaluate` on the merged sweep, or evaluate every shard first)",
            loaded.len()
        );
        return Ok(None);
    }

    let mut eval_entries: Vec<Value> = Vec::new();

    for (shard_idx, (label, shard_dir, shard_results)) in loaded.iter().enumerate() {
        let owns = |id: &str| owner_shard.get(id).copied().unwrap_or(shard_idx) == shard_idx;
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
                if !id.is_empty() && owns(id) {
                    eval_entries.push(inst.clone());
                }
            }
        } else {
            // Legacy sb-cli format: resolved_ids / submitted_ids arrays. These omit
            // errored/no-patch instances, but the merged file is modern, so audit
            // wants an entry for *every* owned trajectory. Synthesize an entry for
            // each owned instance, marking those absent from resolved_ids unresolved.
            let resolved_ids: HashSet<&str> = eval
                .get("resolved_ids")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for inst in &shard_results.instances {
                let id = inst.instance_id.as_str();
                if !owns(id) {
                    continue;
                }
                let resolved = resolved_ids.contains(id);
                // `eval_exit_reason` is required by `InstanceEvaluation`; downstream
                // commands (report/triage/inspect) reject rows that omit it.
                eval_entries.push(serde_json::json!({
                    "instance_id": id,
                    "resolved": resolved,
                    "eval_exit_reason": if resolved { "resolved" } else { "unresolved" }
                }));
            }
        }
    }

    if eval_entries.is_empty() {
        return Ok(None);
    }

    let resolved_count = eval_entries
        .iter()
        .filter(|e| e.get("resolved").and_then(Value::as_bool) == Some(true))
        .count();

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

    Ok(Some(resolved_count))
}

/// Best-effort absolute, symlink-resolved path. Falls back to a lexical absolute
/// path when the target does not exist yet (e.g. the not-yet-created output dir).
fn abs_path(p: &Path) -> PathBuf {
    p.canonicalize()
        .or_else(|_| std::path::absolute(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// Reject an `--output` that is equal to, contains, or is contained by any input
/// shard. Under `--force` the output dir is `remove_dir_all`'d, so an overlapping
/// path would delete the shard artifacts before they are copied.
fn validate_output_isolation(output: &Path, shards: &[PathBuf]) -> Result<(), Error> {
    let out_abs = abs_path(output);
    for shard in shards {
        let shard_abs = abs_path(shard);
        if out_abs == shard_abs
            || out_abs.starts_with(&shard_abs)
            || shard_abs.starts_with(&out_abs)
        {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: output directory '{}' overlaps input shard '{}'; \
                 choose an --output path that is outside every shard",
                output.display(),
                shard.display()
            ))));
        }
    }
    Ok(())
}

/// Rebuild the subset filter spec so it describes the merged union (sorted
/// instance IDs) rather than inheriting shard 0's per-shard filter.
fn merged_filter_spec(
    base: &SweepResults,
    union_instances: &[crate::run::swebench::InstanceResult],
) -> crate::run::swebench::FilterSpec {
    let mut ids: Vec<String> = union_instances
        .iter()
        .map(|r| r.instance_id.clone())
        .collect();
    ids.sort();
    crate::run::swebench::FilterSpec {
        // The full dataset size is identical across shards (provenance-checked).
        original_count: base.filter_spec.original_count,
        selected_count: ids.len(),
        instance_ids: Some(ids),
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: None,
    }
}

#[derive(Default)]
struct SlotOutcomeCounts {
    submitted: usize,
    errored: usize,
    skipped: usize,
    budget_halted: usize,
}

/// Recount per-run-slot outcomes from the copied trajectories, mirroring exactly
/// what `bench audit` recomputes (see `audit::run`), so the merged results.json
/// reconciles for pass@k/rerun sweeps where one task spans several run slots.
fn recount_slot_outcomes(
    output: &Path,
    union_instances: &[crate::run::swebench::InstanceResult],
) -> Result<SlotOutcomeCounts, Error> {
    let exit_reasons: HashMap<&str, &str> = union_instances
        .iter()
        .map(|r| (r.instance_id.as_str(), r.exit_reason.as_str()))
        .collect();

    // Map each instance to its run-slot trajectory files, reusing audit's
    // discovery + path parsing so merge and audit agree on what counts. The
    // de-duplication rule matches audit exactly: prefer the deeper (nested) path,
    // and at equal depth prefer `run-N.traj.json` over a sibling `trajectory.json`.
    let trajectories = crate::run::audit::collect_trajectories_on_disk(output).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: failed to scan merged trajectories: {e}"
        )))
    })?;
    let mut instances_runs: HashMap<String, BTreeMap<u32, PathBuf>> = HashMap::new();
    for path in &trajectories {
        if let Some((inst_id, run_index)) = crate::run::audit::parse_trajectory_path(output, path) {
            let slot = instances_runs.entry(inst_id).or_default();
            match slot.get(&run_index) {
                None => {
                    slot.insert(run_index, path.clone());
                }
                Some(existing) => {
                    let current_depth = path.components().count();
                    let existing_depth = existing.components().count();
                    let prefer = current_depth > existing_depth
                        || (current_depth == existing_depth
                            && file_name_contains(path, "run-1")
                            && file_name_contains(existing, "trajectory.json"));
                    if prefer {
                        slot.insert(run_index, path.clone());
                    }
                }
            }
        }
    }

    let mut counts = SlotOutcomeCounts::default();
    for (inst_id, runs) in &instances_runs {
        let is_skipped_resume =
            exit_reasons.get(inst_id.as_str()).copied() == Some("skipped_resume");
        if is_skipped_resume {
            counts.skipped += 1;
        }
        for path in runs.values() {
            let Ok(content) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<Value>(&content) else {
                continue;
            };
            let outcome = val
                .get("info")
                .and_then(|i| i.get("outcome"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match outcome {
                "submitted" if !is_skipped_resume => counts.submitted += 1,
                "errored" | "error" => counts.errored += 1,
                "skipped" if !is_skipped_resume => counts.skipped += 1,
                "budget_halted" | "budget_halt" => counts.budget_halted += 1,
                _ => {}
            }
        }
    }

    // Budget-halted / skipped tasks that never started have no trajectory files;
    // audit counts them from results.json, so we must too or the merged
    // `budget_halted`/`skipped` would drop to zero and fail audit.
    for inst in union_instances {
        if instances_runs.contains_key(&inst.instance_id) {
            continue;
        }
        let exit_reason = inst.exit_reason.as_str();
        let outcome = inst.outcome.as_deref().unwrap_or("");
        if exit_reason == "budget_halt"
            || exit_reason == "budget_halted"
            || outcome == "budget_halted"
        {
            counts.budget_halted += 1;
        } else if exit_reason == "skipped"
            || exit_reason == "skipped_resume"
            || outcome == "skipped"
        {
            counts.skipped += 1;
        }
    }

    Ok(counts)
}

fn file_name_contains(path: &Path, needle: &str) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(needle))
}

/// Merge prediction files so the merged sweep can be scored by `bench evaluate`,
/// which reads `<sweep>/all_preds.jsonl`. Keeps only rows owned by the merged
/// union, preserves per-run `all_preds.run-k.jsonl` files, and rewrites metadata.
#[allow(clippy::too_many_lines)]
fn merge_predictions(
    loaded: &[(String, PathBuf, SweepResults)],
    owner_shard: &HashMap<String, usize>,
    output: &Path,
) -> Result<(), Error> {
    use crate::run::swebench::{
        PredictionsMetadata, predictions_metadata_path, predictions_metadata_path_for_run,
        predictions_path, predictions_path_for_run, write_predictions_metadata,
    };

    // Only merge predictions if the shards actually wrote them.
    if !loaded
        .iter()
        .any(|(_, dir, _)| predictions_path(dir).exists())
    {
        return Ok(());
    }

    // Owning shard for a prediction row: the aggregate file may carry unique
    // `<id>::run-k` IDs plus an `original_instance_id`; per-run files keep the
    // original SWE-bench ID. Resolve both to the underlying instance id.
    let owned_line = |shard_idx: usize, line: &str| -> bool {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        let orig = val
            .get("original_instance_id")
            .and_then(Value::as_str)
            .or_else(|| val.get("instance_id").and_then(Value::as_str))
            .unwrap_or("");
        owner_shard.get(orig).copied() == Some(shard_idx)
    };

    // Collect every run index that has a per-run prediction file in any shard.
    // Rerun sweeps write `all_preds.run-k.jsonl` only for run slots that produced
    // a submission, so indices can be sparse (e.g. run 1 empty, run 2 present) —
    // a contiguous `while exists(k)` scan would stop at the first gap and drop
    // later runs, so scan the directory listing instead.
    let mut run_indices: Vec<u32> = Vec::new();
    for (_, dir, _) in loaded {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(k) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix("all_preds.run-"))
                .and_then(|n| n.strip_suffix(".jsonl"))
                .and_then(|n| n.parse::<u32>().ok())
            {
                if !run_indices.contains(&k) {
                    run_indices.push(k);
                }
            }
        }
    }
    run_indices.sort_unstable();

    // Merge the aggregate all_preds.jsonl.
    let mut aggregate = String::new();
    let mut aggregate_rows = 0usize;
    let mut aggregate_unique_ids = false;
    for (shard_idx, (_, dir, _)) in loaded.iter().enumerate() {
        let path = predictions_path(dir);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if owned_line(shard_idx, line) {
                if line.contains("original_instance_id") {
                    aggregate_unique_ids = true;
                }
                aggregate.push_str(line);
                aggregate.push('\n');
                aggregate_rows += 1;
            }
        }
    }
    fs::write(predictions_path(output), aggregate).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "merge: failed to write all_preds.jsonl: {e}"
        )))
    })?;
    write_predictions_metadata(
        &predictions_metadata_path(output),
        &PredictionsMetadata {
            predictions_file: "all_preds.jsonl".to_owned(),
            aggregate: true,
            run_index: None,
            row_count: aggregate_rows,
            // The aggregate is sb-cli-safe only when it carries no duplicate
            // (per-run) instance IDs — i.e. a single-run merge.
            swebench_evaluator_compatible: !aggregate_unique_ids,
        },
    )?;

    // Merge each per-run all_preds.run-k.jsonl.
    for &k in &run_indices {
        let mut run_text = String::new();
        let mut run_rows = 0usize;
        for (shard_idx, (_, dir, _)) in loaded.iter().enumerate() {
            let Ok(text) = fs::read_to_string(predictions_path_for_run(dir, k)) else {
                continue;
            };
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if owned_line(shard_idx, line) {
                    run_text.push_str(line);
                    run_text.push('\n');
                    run_rows += 1;
                }
            }
        }
        fs::write(predictions_path_for_run(output, k), run_text).map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: failed to write all_preds.run-{k}.jsonl: {e}"
            )))
        })?;
        write_predictions_metadata(
            &predictions_metadata_path_for_run(output, k),
            &PredictionsMetadata {
                predictions_file: format!("all_preds.run-{k}.jsonl"),
                aggregate: false,
                run_index: Some(k),
                row_count: run_rows,
                swebench_evaluator_compatible: true,
            },
        )?;
    }

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
