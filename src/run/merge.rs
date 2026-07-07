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
    check_uniform_rerun_count(&loaded)?;

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

    // Combine rate-limit telemetry across shards (recompute_aggregates only cloned
    // shard 0's), or a throttled later worker would be invisible in the merged run.
    merged.rate_limit_events = merge_rate_limit_events(&loaded);

    // A merged multi-shard sweep ran under no single global cost cap — shard 0's
    // `--sweep-cost-limit-usd` does not bound the union's ~K× spend, so clear it
    // rather than presenting one shard's cap over all shards' cost.
    merged.cost_limit_usd = None;

    // recompute_aggregates hardcodes the cost source to RateCardEstimate; preserve
    // the shards' real provenance (provider-reported / free-tier) when they agree,
    // and report a mix as Unknown rather than mislabeling it as an estimate.
    merged.actual_cost_source = merged_cost_source(&loaded, merged.actual_cost_usd.is_some());

    // `recompute_aggregates` counts outcomes per-instance, but a pass@k sweep has
    // one collapsed row per task while results.json records per-run *slots* (and
    // `bench audit` recomputes the same way from the copied trajectories). Recount
    // the per-slot aggregates so the merged sweep audits and its report fields
    // (with_patch / submitted_with_tests / failures_by_category) match a real sweep.
    let slots = recount_slot_outcomes(&args.output, &union_instances)?;
    merged.submitted = slots.submitted;
    merged.errored = slots.errored;
    merged.skipped = slots.skipped;
    merged.budget_halted = slots.budget_halted;
    merged.submitted_with_tests = slots.submitted_with_tests;
    merged.with_patch = slots.with_patch;
    // patch_empty / patch_apply_invalid are derived from the failure histogram,
    // exactly as a canonical sweep's finalization does.
    merged.patch_empty = slots
        .failures_by_category
        .get(&crate::trajectory::FailureCategory::PatchEmpty)
        .copied()
        .unwrap_or(0);
    merged.patch_apply_invalid = slots
        .failures_by_category
        .get(&crate::trajectory::FailureCategory::PatchApplyInvalid)
        .copied()
        .unwrap_or(0);
    merged.failures_by_category = slots.failures_by_category;

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
        // `resolved_count` handles legacy rows (pre-`runs`/`resolved_count`), where
        // the raw field is 0; a raw `> 0` check would report all-legacy merges as 0.
        union_instances
            .iter()
            .filter(|r| crate::run::swebench::resolved_count(r) > 0)
            .count()
    });

    // Keep the report's pass@k consistent with `resolved`: when the merged
    // evaluation supplies an authoritative resolved count, derive pass@k from it
    // rather than from `merged.pass_at_k` (the submission proxy over instance rows),
    // so `bench merge --format json` doesn't mix evaluated and proxy rates.
    #[allow(clippy::cast_precision_loss)]
    let pass_at_k = if eval_resolved.is_some() && !union_instances.is_empty() {
        resolved as f64 / union_instances.len() as f64
    } else {
        merged.pass_at_k
    };

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
        pass_at_k,
    })
}

// ── rendering ─────────────────────────────────────────────────────────────────

impl MergeReport {
    pub fn render_text(&self) {
        use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};

        println!("\n=== bench merge ===");
        println!("Shards merged: {}", self.shards.len());
        println!(
            "Total instances: {} ({} duplicates resolved via {})",
            self.total_instances, self.duplicates, self.collision_policy
        );
        println!("Output: {}\n", self.output_dir);

        if !self.shards.is_empty() {
            let mut shard_table = Table::new();
            shard_table
                .load_preset(UTF8_FULL)
                .apply_modifier(UTF8_ROUND_CORNERS)
                .set_header(vec!["Label", "Instances", "Directory"]);

            for s in &self.shards {
                shard_table.add_row(vec![
                    s.label.clone(),
                    s.instance_count.to_string(),
                    s.dir.clone(),
                ]);
            }
            println!("── Shards ──");
            println!("{shard_table}\n");
        }

        let mut metrics = Table::new();
        metrics
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec!["Metric", "Value"]);

        metrics.add_row(vec![
            "total_cost_usd".to_string(),
            format!("${:.6}", self.total_cost_usd),
        ]);
        metrics.add_row(vec!["submitted".to_string(), self.submitted.to_string()]);
        metrics.add_row(vec!["errored".to_string(), self.errored.to_string()]);
        metrics.add_row(vec!["resolved".to_string(), self.resolved.to_string()]);
        metrics.add_row(vec!["pass_at_k".to_string(), format!("{:.4}", self.pass_at_k)]);

        println!("── Top-line metrics ──");
        println!("{metrics}");
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

        // Model endpoint identity lives outside config.resolved: two shards can
        // share a model name + config yet hit a different backend/base_url.
        if manifest.model.backend != manifest0.model.backend
            || manifest.model.base_url != manifest0.model.base_url
        {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' model endpoint (backend '{}', base_url {:?}) differs from \
                 shard '{label0}' (backend '{}', base_url {:?}); \
                 bench merge requires an identical model endpoint across shards",
                manifest.model.backend,
                manifest.model.base_url,
                manifest0.model.backend,
                manifest0.model.base_url
            ))));
        }

        // Reject shards produced by a different harness commit — the merged
        // manifest carries only shard 0's git_sha, so mixing revisions would
        // misrepresent provenance. Only enforced when both shards recorded a sha.
        if let (Some(sha), Some(sha0)) = (&manifest.harness.git_sha, &manifest0.harness.git_sha) {
            if sha != sha0 {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "merge: shard '{label}' harness git_sha '{sha}' differs from shard \
                     '{label0}' '{sha0}'; bench merge requires shards built from the same \
                     harness revision"
                ))));
            }
        }
    }

    Ok(())
}

/// Reject shards sampled with different `--rerun` (pass@k) counts. `runs` is a
/// per-row sweep-shaping value, not part of `config.resolved`, so a `--rerun 1`
/// shard and a `--rerun 3` shard would otherwise pass the config check and yield
/// a single pass@k summary over tasks sampled with different k.
fn check_uniform_rerun_count(loaded: &[(String, PathBuf, SweepResults)]) -> Result<(), Error> {
    let mut reference: Option<(&str, u32)> = None;
    for (label, _, results) in loaded {
        // Normalize legacy rows (written before `runs` existed, serde-default 0) the
        // same way the rest of the code does, so a 1-run legacy shard isn't rejected
        // against a 1-run modern shard.
        let Some(runs) = results
            .instances
            .iter()
            .map(crate::run::swebench::effective_runs)
            .max()
        else {
            continue; // empty shard contributes no rerun signal
        };
        match reference {
            None => reference = Some((label, runs)),
            Some((ref_label, ref_runs)) if runs != ref_runs => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "merge: shard '{label}' rerun count ({runs}) differs from shard \
                     '{ref_label}' ({ref_runs}); bench merge requires the same --rerun \
                     (pass@k) count across shards"
                ))));
            }
            _ => {}
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
    }
    // Always also copy the flat and bundled artifacts: a shard can store the
    // trajectory under a nested `<id>/` dir while keeping the submitted patch at
    // the root (`<id>.patch`) or under `patches/<id>.patch`. copy_file no-ops on
    // missing sources, so this is safe regardless of the source layout.
    for ext in [".traj.json", ".patch"] {
        let name = format!("{instance_id}{ext}");
        copy_file(src_sweep.join(&name), dst_sweep.join(&name))?;
    }
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
#[allow(clippy::too_many_lines)]
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
    let mut merged_provenance: Option<(String, Value)> = None; // (shard label, provenance)
    let mut shards_with_provenance = 0usize;

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

        // Evaluator provenance must agree: merging verdicts produced by different
        // backends / versions / dataset settings into one authoritative file would
        // hide the mismatch from downstream compare/report.
        if let Some(prov) = eval.get("provenance").filter(|p| !p.is_null()) {
            shards_with_provenance += 1;
            match &merged_provenance {
                None => merged_provenance = Some((label.clone(), prov.clone())),
                Some((ref0, prov0)) => {
                    if evaluator_identity(prov) != evaluator_identity(prov0) {
                        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                            "merge: shard '{label}' evaluator provenance (backend/version/dataset) \
                             differs from shard '{ref0}'; bench merge requires shards evaluated \
                             with the same evaluator"
                        ))));
                    }
                }
            }
        }

        // Index whatever verdicts the shard's evaluation.json carries, in either
        // the modern (`instances[]`) or legacy (`resolved_ids`) shape.
        let modern: HashMap<&str, &Value> = eval
            .get("instances")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        e.get("instance_id")
                            .and_then(Value::as_str)
                            .map(|id| (id, e))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let resolved_ids: HashSet<&str> = eval
            .get("resolved_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        // Emit exactly one entry for every owned instance (each has a copied
        // trajectory), reusing the shard's rich modern entry when present and
        // synthesizing an unresolved one otherwise. This guarantees exact coverage
        // and never emits unknown/extra IDs, so a stale or partial shard eval still
        // yields a merged modern evaluation.json that passes audit/budget-fit.
        for inst in &shard_results.instances {
            let id = inst.instance_id.as_str();
            if !owns(id) {
                continue;
            }
            if let Some(entry) = modern.get(id) {
                // Normalize: a pre-`eval_exit_reason` modern entry would otherwise be
                // copied verbatim and fail `InstanceEvaluation` deserialization
                // downstream, even though synthesized rows below carry the field.
                eval_entries.push(normalize_eval_entry(entry, inst.non_empty_patch));
            } else {
                // `eval_exit_reason` is required by `InstanceEvaluation`; downstream
                // commands (report/triage/inspect) reject rows that omit it. A row
                // with no submitted patch was never scored, so label it
                // `skipped_no_patch` (evaluator-unavailable) rather than `unresolved`
                // (a real failed verdict), matching canonical `bench evaluate`.
                let resolved = resolved_ids.contains(id);
                let eval_exit_reason = if resolved {
                    "resolved"
                } else if inst.non_empty_patch {
                    "unresolved"
                } else {
                    "skipped_no_patch"
                };
                eval_entries.push(serde_json::json!({
                    "instance_id": id,
                    "resolved": resolved,
                    "eval_exit_reason": eval_exit_reason
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

    let mut merged_eval = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 3},
        "instances": eval_entries,
    });

    // Recompute the top-level summary fields `bench report` (submission_class_rollup)
    // and `bench compare` (test_only_resolved_rate) read, over the merged union.
    let insts: Vec<crate::run::evaluate::InstanceEvaluation> = merged_eval["instances"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| serde_json::from_value(e.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    if insts.len() == merged_eval["instances"].as_array().map_or(0, Vec::len) {
        let (rollup, test_only_resolved_rate) =
            crate::run::evaluate::build_submission_class_rollup(&insts);
        if let Ok(v) = serde_json::to_value(rollup) {
            merged_eval["submission_class_rollup"] = v;
        }
        merged_eval["test_only_resolved_rate"] = serde_json::json!(test_only_resolved_rate);
    }
    // Only stamp the merged file with evaluator provenance when *every* evaluated
    // shard recorded one — otherwise we'd label a legacy/unprovenanced shard's
    // verdicts as if they shared the first shard's backend/dataset settings.
    if shards_with_provenance == loaded.len() {
        if let Some((_, prov)) = merged_provenance {
            merged_eval["provenance"] = prov;
        }
    }

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

/// Ensure a copied modern evaluation entry carries the `eval_exit_reason` field
/// that `InstanceEvaluation` requires (legacy artifacts predate it). A row with
/// no submitted patch was never scored, so classify it `skipped_no_patch` rather
/// than `unresolved` — the same patch-aware logic used for synthesized rows.
fn normalize_eval_entry(entry: &Value, has_patch: bool) -> Value {
    let mut e = entry.clone();
    if let Some(obj) = e.as_object_mut() {
        if !obj.contains_key("eval_exit_reason") {
            let resolved = obj
                .get("resolved")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let reason = if resolved {
                "resolved"
            } else if has_patch {
                "unresolved"
            } else {
                "skipped_no_patch"
            };
            obj.insert(
                "eval_exit_reason".to_owned(),
                Value::String(reason.to_owned()),
            );
        }
    }
    e
}

/// The scoring-relevant evaluator identity that must match across shards before
/// their verdicts can be merged. We compare the whole provenance object with the
/// per-shard *volatile* fields (run ids, file paths/hashes, timestamps, command
/// shapes, per-slot source reports) recursively stripped, so scoring-relevant
/// settings — dataset subset/split, Docker image names, timeouts, parallelism —
/// are all included.
fn evaluator_identity(prov: &Value) -> Value {
    let mut v = prov.clone();
    strip_volatile_provenance(&mut v);
    v
}

fn strip_volatile_provenance(v: &mut Value) {
    const VOLATILE: &[&str] = &[
        "run_id",
        "prediction_path",
        "prediction_sha256",
        "eval_started_at",
        "eval_ended_at",
        "report_source",
        "report_paths",
        "report_hashes",
        "submit_command",
        "report_command",
        "report_path",
        "report_sha256",
        "source_reports",
        // `image_names` is the set of Docker images used by *this shard's* instances;
        // disjoint shards of the same dataset legitimately differ, so it is not a
        // scoring-identity field (timeout/parallel/backend/dataset still are).
        "image_names",
    ];
    match v {
        Value::Object(obj) => {
            for k in VOLATILE {
                obj.remove(*k);
            }
            for child in obj.values_mut() {
                strip_volatile_provenance(child);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                strip_volatile_provenance(child);
            }
        }
        _ => {}
    }
}

/// Best-effort absolute, symlink-resolved path. When the target does not exist
/// yet (e.g. the not-yet-created output dir), canonicalize the nearest existing
/// ancestor — so symlinks in the parent chain are resolved — and re-append the
/// non-existent tail. A purely lexical `absolute()` would miss a parent symlink
/// that points into a shard, letting an overlapping output slip past isolation.
fn abs_path(p: &Path) -> PathBuf {
    if let Ok(canon) = p.canonicalize() {
        return canon;
    }
    let mut ancestor = p;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(base) = ancestor.canonicalize() {
            let mut result = base;
            for comp in tail.iter().rev() {
                result.push(comp);
            }
            return result;
        }
        match (ancestor.file_name(), ancestor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                ancestor = parent;
            }
            _ => break,
        }
    }
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
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
    submitted_with_tests: usize,
    with_patch: usize,
    failures_by_category: BTreeMap<crate::trajectory::FailureCategory, usize>,
}

/// Report whether the run slot has a non-empty patch, using the canonical patch
/// resolver so every layout (nested `run-k.patch`, legacy `<id>.patch`, bundled
/// `patches/<id>.patch`) is found exactly as the sweep/evaluator would.
fn slot_has_nonempty_patch(sweep_dir: &Path, instance_id: &str, run_index: u32) -> bool {
    let patch_path =
        crate::run::swebench::existing_patch_path_for_run(sweep_dir, instance_id, run_index);
    fs::read_to_string(&patch_path).is_ok_and(|s| !s.trim().is_empty())
}

/// Recount per-run-slot aggregates from the copied trajectories, mirroring exactly
/// what `bench audit` recomputes and what a canonical sweep records, so the merged
/// results.json reconciles for pass@k/rerun sweeps where one task spans several
/// run slots (the collapsed per-task rows only carry run 1's representative).
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
        for (run_index, path) in runs {
            let Ok(content) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<Value>(&content) else {
                continue;
            };
            let info = val.get("info");
            let outcome = info
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

            // Per-slot report fields (match a canonical sweep's finalization).
            if outcome == "submitted" && !is_skipped_resume {
                let tests_run = info
                    .and_then(|i| i.get("tests_run_before_submit"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if tests_run {
                    counts.submitted_with_tests += 1;
                }
                if slot_has_nonempty_patch(output, inst_id, *run_index) {
                    counts.with_patch += 1;
                }
            }
            if let Some(cat) = info
                .and_then(|i| i.get("failure_category"))
                .cloned()
                .and_then(|v| serde_json::from_value::<crate::trajectory::FailureCategory>(v).ok())
            {
                *counts.failures_by_category.entry(cat).or_insert(0) += 1;
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

    // If any shard has predictions, every shard must — otherwise we would silently
    // drop a shard's submissions while still keeping its rows/patches, and
    // `bench evaluate` would treat those submissions as missing. Fail loudly with
    // the incomplete shard instead of writing a partial predictions artifact.
    for (label, dir, _) in loaded {
        let path = predictions_path(dir);
        if !path.exists() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' is missing all_preds.jsonl while other shards have \
                 predictions; cannot produce a complete merged predictions set \
                 (re-run the shard's sweep or evaluate the merged sweep instead)"
            ))));
        }
        if fs::read_to_string(&path).is_err() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' all_preds.jsonl is unreadable; cannot produce a \
                 complete merged predictions set"
            ))));
        }
    }

    // Owning shard for a prediction row: the aggregate file may carry unique
    // `<id>::run-k` IDs plus an `original_instance_id`; per-run files keep the
    // original SWE-bench ID. Resolve both to the underlying instance id. A
    // malformed/idless row is corruption — fail rather than silently dropping a
    // submission that `bench evaluate` would then see as missing.
    let owned_line = |shard_idx: usize, line: &str, label: &str| -> Result<bool, Error> {
        let val: Value = serde_json::from_str(line).map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' has a malformed prediction row: {e}"
            )))
        })?;
        let orig = val
            .get("original_instance_id")
            .and_then(Value::as_str)
            .or_else(|| val.get("instance_id").and_then(Value::as_str));
        let Some(orig) = orig else {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "merge: shard '{label}' has a prediction row without an instance_id"
            ))));
        };
        Ok(owner_shard.get(orig).copied() == Some(shard_idx))
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
    for (shard_idx, (label, dir, _)) in loaded.iter().enumerate() {
        let path = predictions_path(dir);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if owned_line(shard_idx, line, label)? {
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
        for (shard_idx, (label, dir, _)) in loaded.iter().enumerate() {
            let path = predictions_path_for_run(dir, k);
            // A missing per-run file is legitimate: rerun sweeps only write a slot
            // file when that slot produced a submission. But a file that exists yet
            // cannot be read is corruption — fail rather than silently drop its rows
            // (bench evaluate reads per-run files when max_runs > 1).
            if !path.exists() {
                continue;
            }
            let text = fs::read_to_string(&path).map_err(|e| {
                Error::Config(crate::error::ConfigError::Invalid(format!(
                    "merge: shard '{label}' all_preds.run-{k}.jsonl is unreadable: {e}"
                )))
            })?;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if owned_line(shard_idx, line, label)? {
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

/// Combine per-shard rate-limit telemetry: sum throttled calls/seconds, take the
/// peak concurrency. The configured caps are kept only when every shard that
/// recorded telemetry used the same value; a mix is reported as `None` rather
/// than presenting one shard's cap as if it applied to all.
fn merge_rate_limit_events(
    loaded: &[(String, PathBuf, SweepResults)],
) -> Option<crate::run::rate_limit::RateLimitEvents> {
    let mut any = false;
    let mut throttled_calls = 0u64;
    let mut total_throttled_seconds = 0.0;
    let mut peak_concurrent = 0u32;
    let mut rpm_caps: HashSet<u32> = HashSet::new();
    let mut tpm_caps: HashSet<u64> = HashSet::new();
    for (_, _, results) in loaded {
        if let Some(ev) = &results.rate_limit_events {
            any = true;
            throttled_calls += ev.throttled_calls;
            total_throttled_seconds += ev.total_throttled_seconds;
            peak_concurrent = peak_concurrent.max(ev.peak_concurrent);
            if let Some(rpm) = ev.configured_max_rpm {
                rpm_caps.insert(rpm);
            }
            if let Some(tpm) = ev.configured_max_input_tpm {
                tpm_caps.insert(tpm);
            }
        }
    }
    if !any {
        return None;
    }
    // Only a single agreed value survives; mixed (or absent) caps become None.
    let agreed_rpm = if rpm_caps.len() == 1 {
        rpm_caps.iter().next().copied()
    } else {
        None
    };
    let agreed_input_tpm = if tpm_caps.len() == 1 {
        tpm_caps.iter().next().copied()
    } else {
        None
    };
    Some(crate::run::rate_limit::RateLimitEvents {
        throttled_calls,
        total_throttled_seconds,
        peak_concurrent,
        configured_max_rpm: agreed_rpm,
        configured_max_input_tpm: agreed_input_tpm,
    })
}

/// Combine per-shard actual-cost provenance. When every shard that recorded a
/// source agrees, keep it; a mix is reported as `Unknown` so a merged
/// provider-reported + free-tier sweep is not mislabeled. Falls back to
/// `RateCardEstimate` (recompute_aggregates' default) only when no shard recorded
/// a source. Returns `None` when the merged sweep has no actual cost at all.
fn merged_cost_source(
    loaded: &[(String, PathBuf, SweepResults)],
    has_actual_cost: bool,
) -> Option<crate::cost::CostSource> {
    if !has_actual_cost {
        return None;
    }
    let mut found: Option<crate::cost::CostSource> = None;
    for (_, _, results) in loaded {
        if let Some(src) = results.actual_cost_source {
            match found {
                None => found = Some(src),
                Some(f) if f != src => return Some(crate::cost::CostSource::Unknown),
                _ => {}
            }
        }
    }
    found.or(Some(crate::cost::CostSource::RateCardEstimate))
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
