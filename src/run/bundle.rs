//! `bench bundle`: deterministic, redaction-strict sweep archive export.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read as _};
use std::path::{Component, Path, PathBuf};

use flate2::{Compression, GzBuilder, read::GzDecoder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion, classify_json_value};
use crate::config::RedactionCfg;
use crate::redaction::{Redactor, surface};

pub const BUNDLE_MANIFEST_PATH: &str = "BUNDLE.json";

#[derive(Debug, Clone)]
pub struct BundleCreateArgs {
    pub sweep_dir: PathBuf,
    pub output_path: PathBuf,
    pub instance: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BundleCreateReport {
    pub output_path: PathBuf,
    pub files: Vec<BundleFileEntry>,
    /// Instance IDs excluded from the bundle because their trajectories had
    /// `partial: true`. Normally empty; non-empty signals an incomplete sweep.
    #[allow(dead_code)]
    pub partial_excluded: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BundleVerifyReport {
    pub problems: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("{0}")]
    MissingSource(String),
    #[error("redaction:retrigger:{path}")]
    RedactionRetrigger { path: String },
    #[error("{0}")]
    InvalidArchive(String),
    #[error("{0}")]
    Schema(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleFileEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
    pub source_sweep_dir: String,
    pub source_manifest_hash: String,
    pub harness_git_sha: Option<String>,
    pub bundle_generated_at: String,
    pub instance_scope: String,
    pub files: Vec<BundleFileEntry>,
}

#[derive(Debug, Clone)]
struct PreparedFile {
    archive_path: String,
    source_path: PathBuf,
    sha256: String,
    bytes: u64,
}

struct BundleWorkspace {
    dir: tempfile::TempDir,
    next_id: usize,
}

impl BundleWorkspace {
    fn new() -> Result<Self, BundleError> {
        Ok(Self {
            dir: tempfile::tempdir()?,
            next_id: 0,
        })
    }

    fn prepare_bytes(
        &mut self,
        archive_path: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<PreparedFile, BundleError> {
        let archive_path = archive_path.into();
        let source_path = self
            .dir
            .path()
            .join(format!("bundle-entry-{}", self.next_id));
        self.next_id = self.next_id.saturating_add(1);
        let sha256 = sha256_hex(&bytes);
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        std::fs::write(&source_path, bytes)?;
        Ok(PreparedFile {
            archive_path,
            source_path,
            sha256,
            bytes: len,
        })
    }
}

struct PathNormalizer {
    needles: Vec<String>,
    windows_abs: Option<Regex>,
    unix_abs: Option<Regex>,
}

impl PathNormalizer {
    fn new(sweep_dir: &Path) -> Self {
        let mut needles = BTreeSet::new();
        push_path_needles(&mut needles, &sweep_dir.display().to_string());
        if let Ok(canonical) = std::fs::canonicalize(sweep_dir) {
            push_path_needles(&mut needles, &canonical.display().to_string());
        }
        Self {
            needles: sorted_path_needles(needles),
            windows_abs: Regex::new(r#"[A-Za-z]:(?:\\\\|\\|/)[^"'\r\n\s,}\]]+"#).ok(),
            unix_abs: Regex::new(r#"(^|[\s"'\[({:=,])/(?:[^\s/"'\]\[{}(),]+/)+[^\s/"'\]\[{}(),]+"#)
                .ok(),
        }
    }

    fn normalize_text(&self, input: &str) -> String {
        let normalized = self
            .needles
            .iter()
            .fold(input.to_owned(), |acc, needle| acc.replace(needle, "."));
        let normalized = self.windows_abs.as_ref().map_or_else(
            || normalized.clone(),
            |regex| regex.replace_all(&normalized, ".").into_owned(),
        );
        self.unix_abs.as_ref().map_or_else(
            || normalized.clone(),
            |regex| regex.replace_all(&normalized, "${1}.").into_owned(),
        )
    }
}

#[derive(Debug, Clone)]
struct ArchiveEntryInfo {
    sha256: Option<String>,
    bytes: u64,
    regular_file: bool,
}

#[derive(Debug, Clone)]
struct ArchiveInventory {
    bundle_bytes: Option<Vec<u8>>,
    entries: BTreeMap<String, ArchiveEntryInfo>,
}

#[derive(Debug, Clone, Default)]
struct ScopedResultAggregates {
    total: usize,
    submitted: usize,
    submitted_with_tests: usize,
    skipped: usize,
    errored: usize,
    with_patch: usize,
    patch_empty: usize,
    patch_apply_invalid: usize,
    github_pr_failures: usize,
    budget_halted: usize,
    total_input_tokens: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    total_completion_tokens: u64,
    total_cost_usd: Option<f64>,
    actual_cost_usd: Option<f64>,
    retries: u64,
    retried_instances: usize,
    drop_retried_instances: bool,
    resolved_count: usize,
    total_fallbacks: u64,
    model_mix: BTreeMap<String, usize>,
    failures: serde_json::Map<String, serde_json::Value>,
    drop_github_pr_failures: bool,
}

#[derive(Debug, Deserialize)]
struct ResolvedConfigForBundle {
    #[serde(default)]
    redaction: Option<RedactionCfg>,
}

#[allow(clippy::too_many_lines)]
pub fn create_bundle(args: &BundleCreateArgs) -> Result<BundleCreateReport, BundleError> {
    if !args.sweep_dir.is_dir() {
        return Err(BundleError::MissingSource(format!(
            "bundle: sweep directory does not exist: {}",
            args.sweep_dir.display()
        )));
    }
    validate_instance_scope(args.instance.as_deref())?;

    let normalizer = PathNormalizer::new(&args.sweep_dir);
    let results_path = args.sweep_dir.join("results.json");
    let results_text = read_required_text(&results_path, "results.json")?;
    let mut results_value: serde_json::Value = serde_json::from_str(&results_text)?;
    classify_json_value(
        &results_value,
        ArtifactKind::SweepResults,
        results_path.display().to_string(),
    )
    .map_err(|err| BundleError::Schema(err.to_string()))?;

    let all_instance_ids = instance_ids_from_results(&results_value)?;
    let included_ids = included_instance_ids(&all_instance_ids, args.instance.as_deref())?;
    let included_set = included_ids.iter().cloned().collect::<BTreeSet<_>>();

    let manifest_bytes = load_manifest_bytes(&args.sweep_dir, &results_value, &normalizer)?;
    let manifest_value: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let redactor = source_redactor(&results_value, &manifest_value);
    let source_manifest_hash = format!("sha256:{}", sha256_hex(&manifest_bytes));
    strict_redaction_check("manifest.json", &manifest_bytes, &redactor)?;

    let mut workspace = BundleWorkspace::new()?;
    let mut files = vec![workspace.prepare_bytes("manifest.json", manifest_bytes)?];

    if args.instance.is_some() {
        filter_results_for_scope(&mut results_value, &included_set, &args.sweep_dir)?;
    }
    let results_bytes = normalized_json_bytes(&results_value, &normalizer)?;
    strict_redaction_check("results.json", &results_bytes, &redactor)?;
    files.push(workspace.prepare_bytes("results.json", results_bytes)?);

    if let Some(evaluation) = load_optional_evaluation(&args.sweep_dir, &included_set, &normalizer)?
    {
        strict_redaction_check("evaluation.json", &evaluation, &redactor)?;
        files.push(workspace.prepare_bytes("evaluation.json", evaluation)?);
    }

    let mut patch_files = Vec::new();
    let mut partial_excluded: Vec<String> = Vec::new();
    for instance_id in &included_ids {
        let row = result_row_for_instance(&results_value, instance_id).ok_or_else(|| {
            BundleError::MissingSource(format!(
                "bundle: instance `{instance_id}` not found in results.json"
            ))
        })?;
        let trajectory_sources =
            find_trajectory_paths_for_bundle(&args.sweep_dir, instance_id, row)?;
        if trajectory_sources.is_empty() {
            continue;
        }
        // Check if any trajectory for this instance is partial; if so, skip the
        // entire instance with a warning — bundles must be completed-sweep artifacts.
        let mut instance_has_partial = false;
        for (trajectory_src, _) in &trajectory_sources {
            if let Ok(text) = std::fs::read_to_string(trajectory_src) {
                if let Ok(traj) = serde_json::from_str::<crate::trajectory::Trajectory>(&text) {
                    if traj.info.partial {
                        instance_has_partial = true;
                        break;
                    }
                }
            }
        }
        if instance_has_partial {
            partial_excluded.push(instance_id.clone());
            continue;
        }
        for (trajectory_src, trajectory_dest) in trajectory_sources {
            let trajectory = normalized_text_file(&trajectory_src, &normalizer)?;
            strict_redaction_check(&trajectory_dest, &trajectory, &redactor)?;
            files.push(workspace.prepare_bytes(trajectory_dest, trajectory)?);
        }

        for (patch_src, patch_dest) in
            find_patch_paths_for_bundle(&args.sweep_dir, instance_id, row)
        {
            let patch = normalized_text_file(&patch_src, &normalizer)?;
            strict_redaction_check(&patch_dest, &patch, &redactor)?;
            patch_files.push(workspace.prepare_bytes(patch_dest, patch)?);
        }
    }
    if !partial_excluded.is_empty() {
        eprintln!(
            "bundle: WARNING — {} partial (mid-run) trajectories excluded from bundle: {}",
            partial_excluded.len(),
            partial_excluded.join(", ")
        );
    }
    files.extend(patch_files);

    let file_entries = files
        .iter()
        .map(|file| BundleFileEntry {
            path: file.archive_path.clone(),
            sha256: file.sha256.clone(),
            bytes: file.bytes,
        })
        .collect::<Vec<_>>();

    let bundle_manifest = BundleManifest {
        artifact_kind: ArtifactKind::BundleManifest,
        schema_version: ArtifactSchemaVersion::CURRENT,
        source_sweep_dir: ".".into(),
        source_manifest_hash,
        harness_git_sha: current_git_sha(),
        bundle_generated_at: bundle_generated_at()?,
        instance_scope: args.instance.clone().unwrap_or_else(|| "full".into()),
        files: file_entries.clone(),
    };
    let bundle_bytes = serde_json::to_vec_pretty(&bundle_manifest)?;

    write_archive_atomically(&args.output_path, &files, &bundle_bytes)?;

    Ok(BundleCreateReport {
        output_path: args.output_path.clone(),
        files: file_entries,
        partial_excluded,
    })
}

pub fn verify_bundle(archive_path: &Path) -> Result<BundleVerifyReport, BundleError> {
    let inventory = read_archive_inventory(archive_path)?;
    let actual = inventory.entries;
    let mut problems = Vec::new();
    let Some(bundle_bytes) = inventory.bundle_bytes else {
        return Ok(BundleVerifyReport {
            problems: vec![format!("missing:{BUNDLE_MANIFEST_PATH}")],
        });
    };
    let bundle: BundleManifest = serde_json::from_slice(&bundle_bytes)?;
    if bundle.artifact_kind != ArtifactKind::BundleManifest {
        return Err(BundleError::Schema(format!(
            "bundle: artifact kind mismatch: expected bundle_manifest, found {}",
            bundle.artifact_kind
        )));
    }
    if bundle.schema_version.major > ArtifactSchemaVersion::CURRENT.major {
        return Err(BundleError::Schema(format!(
            "bundle: unsupported future bundle schema {}",
            bundle.schema_version
        )));
    }

    let mut expected = BTreeMap::new();
    for entry in &bundle.files {
        if expected.insert(entry.path.clone(), entry).is_some() {
            problems.push(format!("duplicate:{}", entry.path));
        }
    }

    for (path, entry) in &expected {
        match actual.get(path) {
            Some(actual_entry) => {
                if !actual_entry.regular_file
                    || actual_entry.bytes != entry.bytes
                    || actual_entry.sha256.as_deref() != Some(entry.sha256.as_str())
                {
                    problems.push(format!("hash_mismatch:{path}"));
                }
            }
            None => problems.push(format!("missing:{path}")),
        }
    }

    for path in actual.keys() {
        if !expected.contains_key(path) {
            problems.push(format!("extra:{path}"));
        }
    }
    problems.sort();
    Ok(BundleVerifyReport { problems })
}

fn load_manifest_bytes(
    sweep_dir: &Path,
    results_value: &serde_json::Value,
    normalizer: &PathNormalizer,
) -> Result<Vec<u8>, BundleError> {
    let manifest_path = sweep_dir.join("manifest.json");
    if manifest_path.exists() {
        return normalized_text_file(&manifest_path, normalizer);
    }
    let manifest = results_value.get("manifest").ok_or_else(|| {
        BundleError::MissingSource("bundle: results.json has no manifest block".into())
    })?;
    normalized_json_bytes(manifest, normalizer)
}

fn source_redactor(
    results_value: &serde_json::Value,
    manifest_value: &serde_json::Value,
) -> Redactor {
    let mut cfg = RedactionCfg::default();
    for source in [results_value, manifest_value] {
        for pointer in ["/manifest/config/resolved", "/config/resolved"] {
            if let Some(resolved) = source.pointer(pointer).and_then(serde_json::Value::as_str) {
                merge_resolved_redaction(&mut cfg, resolved);
            }
        }
    }
    Redactor::from_config_lossy(&cfg)
}

fn merge_resolved_redaction(cfg: &mut RedactionCfg, resolved: &str) {
    let Ok(parsed) = toml::from_str::<ResolvedConfigForBundle>(resolved) else {
        return;
    };
    let Some(redaction) = parsed.redaction else {
        return;
    };
    cfg.secret_literals.extend(
        redaction
            .secret_literals
            .into_iter()
            .filter(|value| !looks_like_redaction_marker(value)),
    );
    cfg.custom_patterns.extend(
        redaction
            .custom_patterns
            .into_iter()
            .filter(|value| !looks_like_redaction_marker(value))
            .filter(|value| Regex::new(value).is_ok()),
    );
}

fn looks_like_redaction_marker(value: &str) -> bool {
    value.starts_with("[REDACTED:")
}

fn load_optional_evaluation(
    sweep_dir: &Path,
    included: &BTreeSet<String>,
    normalizer: &PathNormalizer,
) -> Result<Option<Vec<u8>>, BundleError> {
    let path = sweep_dir.join("evaluation.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let mut value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(
        &value,
        ArtifactKind::EvaluationResults,
        path.display().to_string(),
    )
    .map_err(|err| BundleError::Schema(err.to_string()))?;
    if let Some(instances) = value
        .get_mut("instances")
        .and_then(serde_json::Value::as_array_mut)
    {
        let original_len = instances.len();
        instances.retain(|row| {
            row.get("instance_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| included.contains(id))
        });
        if instances.len() != original_len {
            drop_filtered_evaluation_summaries(&mut value);
        }
    }
    normalized_json_bytes(&value, normalizer).map(Some)
}

fn drop_filtered_evaluation_summaries(value: &mut serde_json::Value) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    for key in [
        "behavioral",
        "breakdown",
        "cost_attribution",
        "model_mix_summary",
    ] {
        map.remove(key);
    }
}

fn filter_results_for_scope(
    value: &mut serde_json::Value,
    included: &BTreeSet<String>,
    sweep_dir: &Path,
) -> Result<(), BundleError> {
    let Some(aggregates) = value
        .get_mut("instances")
        .and_then(serde_json::Value::as_array_mut)
        .map(|instances| {
            instances.retain(|row| {
                row.get("instance_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| included.contains(id))
            });

            let row_aggregates = scoped_result_aggregates(instances);
            Ok::<ScopedResultAggregates, BundleError>(
                scoped_slot_aggregates_from_run_artifacts(sweep_dir, instances, &row_aggregates)?
                    .unwrap_or(row_aggregates),
            )
        })
    else {
        return Ok(());
    };
    let aggregates = aggregates?;

    if let serde_json::Value::Object(map) = value {
        apply_scoped_result_aggregates(map, &aggregates, included);
    }
    Ok(())
}

fn apply_scoped_result_aggregates(
    map: &mut serde_json::Map<String, serde_json::Value>,
    aggregates: &ScopedResultAggregates,
    included: &BTreeSet<String>,
) {
    map.insert("total".into(), serde_json::json!(aggregates.total));
    map.insert("completed".into(), serde_json::json!(aggregates.total));
    map.insert("submitted".into(), serde_json::json!(aggregates.submitted));
    map.insert(
        "submitted_with_tests".into(),
        serde_json::json!(aggregates.submitted_with_tests),
    );
    map.insert("skipped".into(), serde_json::json!(aggregates.skipped));
    map.insert("errored".into(), serde_json::json!(aggregates.errored));
    map.insert(
        "with_patch".into(),
        serde_json::json!(aggregates.with_patch),
    );
    map.insert(
        "patch_empty".into(),
        serde_json::json!(aggregates.patch_empty),
    );
    map.insert(
        "patch_apply_invalid".into(),
        serde_json::json!(aggregates.patch_apply_invalid),
    );
    if aggregates.drop_github_pr_failures {
        map.remove("github_pr_failures");
    } else {
        map.insert(
            "github_pr_failures".into(),
            serde_json::json!(aggregates.github_pr_failures),
        );
    }
    map.insert(
        "budget_halted".into(),
        serde_json::json!(aggregates.budget_halted),
    );
    map.insert(
        "failures_by_category".into(),
        serde_json::Value::Object(aggregates.failures.clone()),
    );
    apply_scoped_cost_and_token_aggregates(map, aggregates);
    if let Some(filter_spec) = map
        .get_mut("filter_spec")
        .and_then(serde_json::Value::as_object_mut)
    {
        filter_spec.insert("selected_count".into(), serde_json::json!(aggregates.total));
        filter_spec.insert(
            "instance_ids".into(),
            serde_json::Value::Array(
                included
                    .iter()
                    .map(|id| serde_json::Value::String(id.clone()))
                    .collect(),
            ),
        );
    }
}

fn apply_scoped_cost_and_token_aggregates(
    map: &mut serde_json::Map<String, serde_json::Value>,
    aggregates: &ScopedResultAggregates,
) {
    map.insert(
        "total_input_tokens".into(),
        serde_json::json!(aggregates.total_input_tokens),
    );
    map.insert(
        "total_cache_read_tokens".into(),
        serde_json::json!(aggregates.total_cache_read_tokens),
    );
    map.insert(
        "total_cache_creation_tokens".into(),
        serde_json::json!(aggregates.total_cache_creation_tokens),
    );
    map.insert(
        "total_completion_tokens".into(),
        serde_json::json!(aggregates.total_completion_tokens),
    );
    if aggregates.total_cost_usd.is_some() || map.contains_key("total_cost_usd") {
        map.insert(
            "total_cost_usd".into(),
            serde_json::json!(aggregates.total_cost_usd.unwrap_or(0.0)),
        );
    }
    if map.contains_key("estimated_cost_usd") {
        map.insert(
            "estimated_cost_usd".into(),
            serde_json::json!(aggregates.total_cost_usd.unwrap_or(0.0)),
        );
    }
    if map.contains_key("actual_cost_usd") {
        map.insert(
            "actual_cost_usd".into(),
            serde_json::json!(aggregates.actual_cost_usd.unwrap_or(0.0)),
        );
    }
    map.insert(
        "cache_hit_rate".into(),
        serde_json::json!(cache_hit_rate(aggregates)),
    );
    map.insert("retries".into(), serde_json::json!(aggregates.retries));
    if aggregates.drop_retried_instances {
        map.remove("retried_instances");
    } else {
        map.insert(
            "retried_instances".into(),
            serde_json::json!(aggregates.retried_instances),
        );
    }
    map.insert("pass_at_k".into(), serde_json::json!(pass_at_k(aggregates)));
    map.insert(
        "total_fallbacks".into(),
        serde_json::json!(aggregates.total_fallbacks),
    );
    if !aggregates.model_mix.is_empty() || map.contains_key("model_mix") {
        map.insert("model_mix".into(), serde_json::json!(aggregates.model_mix));
    }
}

fn scoped_result_aggregates(instances: &[serde_json::Value]) -> ScopedResultAggregates {
    let mut aggregates = ScopedResultAggregates {
        total: instances.len(),
        ..ScopedResultAggregates::default()
    };
    for row in instances {
        add_result_row_to_aggregates(&mut aggregates, row);
    }
    aggregates
}

fn scoped_slot_aggregates_from_run_artifacts(
    sweep_dir: &Path,
    instances: &[serde_json::Value],
    row_aggregates: &ScopedResultAggregates,
) -> Result<Option<ScopedResultAggregates>, BundleError> {
    if !instances.iter().any(|row| effective_runs_value(row) > 1) {
        return Ok(None);
    }

    let mut aggregates = ScopedResultAggregates {
        total: row_aggregates.total,
        ..ScopedResultAggregates::default()
    };
    for row in instances {
        let runs = effective_runs_value(row);
        if runs <= 1 || is_never_started_budget_halt(row) {
            add_result_row_to_aggregates(&mut aggregates, row);
            continue;
        }
        let instance_id = string_field(row, "instance_id").ok_or_else(|| {
            BundleError::MissingSource("bundle: results.json instance missing instance_id".into())
        })?;
        for run_index in 1..=runs {
            let slot = run_slot_result_value_from_artifact(sweep_dir, instance_id, run_index)?;
            add_result_row_to_aggregates(&mut aggregates, &slot);
        }
    }

    aggregates.total = row_aggregates.total;
    aggregates.resolved_count = row_aggregates.resolved_count;
    aggregates.drop_github_pr_failures = true;
    aggregates.total_input_tokens = row_aggregates.total_input_tokens;
    aggregates.total_cache_read_tokens = row_aggregates.total_cache_read_tokens;
    aggregates.total_cache_creation_tokens = row_aggregates.total_cache_creation_tokens;
    aggregates.total_completion_tokens = row_aggregates.total_completion_tokens;
    aggregates.total_cost_usd = row_aggregates.total_cost_usd;
    aggregates.actual_cost_usd = row_aggregates.actual_cost_usd;
    aggregates.retries = row_aggregates.retries;
    aggregates.drop_retried_instances = true;
    aggregates.total_fallbacks = row_aggregates.total_fallbacks;
    if aggregates.model_mix.is_empty() {
        aggregates.model_mix.clone_from(&row_aggregates.model_mix);
    }
    Ok(Some(aggregates))
}

fn add_result_row_to_aggregates(aggregates: &mut ScopedResultAggregates, row: &serde_json::Value) {
    add_outcome_counts_to_aggregates(aggregates, row);
    add_patch_and_failure_counts_to_aggregates(aggregates, row);
    add_usage_and_model_counts_to_aggregates(aggregates, row);
}

fn add_outcome_counts_to_aggregates(
    aggregates: &mut ScopedResultAggregates,
    row: &serde_json::Value,
) {
    if is_never_started_budget_halt(row) {
        aggregates.budget_halted += 1;
        return;
    }
    let outcome = string_field(row, "outcome");
    let exit_reason = string_field(row, "exit_reason");
    let runs = effective_runs_value(row);
    if runs > 1 {
        let submitted_slots = u64_field(row, "resolved_count")
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or_else(|| usize::from(outcome == Some("submitted")))
            .min(usize::try_from(runs).unwrap_or(usize::MAX));
        aggregates.submitted = aggregates.submitted.saturating_add(submitted_slots);
        if submitted_slots > 0 && bool_field(row, "tests_run_before_submit") {
            aggregates.submitted_with_tests += 1;
        }
        let skipped_slots =
            usize::from(outcome == Some("skipped") || exit_reason == Some("skipped"));
        aggregates.skipped = aggregates.skipped.saturating_add(skipped_slots);
        let budget_slots = usize::from(exit_reason == Some("budget_halt"));
        aggregates.budget_halted = aggregates.budget_halted.saturating_add(budget_slots);
        aggregates.errored = aggregates.errored.saturating_add(
            usize::try_from(runs)
                .unwrap_or(usize::MAX)
                .saturating_sub(submitted_slots)
                .saturating_sub(skipped_slots)
                .saturating_sub(budget_slots),
        );
    } else {
        let submitted = outcome == Some("submitted");
        let skipped = outcome == Some("skipped") || exit_reason == Some("skipped");
        let budget_halted = exit_reason == Some("budget_halt");
        if submitted {
            aggregates.submitted += 1;
            if bool_field(row, "tests_run_before_submit") {
                aggregates.submitted_with_tests += 1;
            }
        }
        if skipped {
            aggregates.skipped += 1;
        }
        if budget_halted {
            aggregates.budget_halted += 1;
        }
        if !submitted && !skipped && !budget_halted {
            aggregates.errored += 1;
        }
    }
}

fn add_patch_and_failure_counts_to_aggregates(
    aggregates: &mut ScopedResultAggregates,
    row: &serde_json::Value,
) {
    let outcome = string_field(row, "outcome");
    if bool_field(row, "patch_present") {
        if bool_field(row, "non_empty_patch") && outcome == Some("submitted") {
            aggregates.with_patch += 1;
        } else if !bool_field(row, "non_empty_patch") {
            aggregates.patch_empty += 1;
        }
    }
    if bool_field(row, "patch_apply_invalid")
        || string_field(row, "failure_category") == Some("patch_apply_invalid")
    {
        aggregates.patch_apply_invalid += 1;
    }
    if row
        .get("github_pr_error")
        .is_some_and(|value| !value.is_null())
    {
        aggregates.github_pr_failures += 1;
    }
    if let Some(category) = string_field(row, "failure_category") {
        let next = aggregates
            .failures
            .get(category)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
            .saturating_add(1);
        aggregates
            .failures
            .insert(category.to_owned(), serde_json::json!(next));
    }
}

fn add_usage_and_model_counts_to_aggregates(
    aggregates: &mut ScopedResultAggregates,
    row: &serde_json::Value,
) {
    aggregates.total_input_tokens = aggregates
        .total_input_tokens
        .saturating_add(u64_field(row, "total_input_tokens").unwrap_or_default());
    aggregates.total_cache_read_tokens = aggregates
        .total_cache_read_tokens
        .saturating_add(u64_field(row, "total_cache_read_tokens").unwrap_or_default());
    aggregates.total_cache_creation_tokens = aggregates
        .total_cache_creation_tokens
        .saturating_add(u64_field(row, "total_cache_creation_tokens").unwrap_or_default());
    aggregates.total_completion_tokens = aggregates
        .total_completion_tokens
        .saturating_add(u64_field(row, "total_completion_tokens").unwrap_or_default());
    aggregates.total_cost_usd = sum_optional_f64(aggregates.total_cost_usd, instance_cost_usd(row));
    aggregates.actual_cost_usd =
        sum_optional_f64(aggregates.actual_cost_usd, instance_actual_cost_usd(row));

    let retries = retry_count(row);
    aggregates.retries = aggregates.retries.saturating_add(retries);
    if retries > 0 {
        aggregates.retried_instances += 1;
    }
    if instance_resolved(row) {
        aggregates.resolved_count += 1;
    }
    if let Some(fallback_count) = u64_field(row, "fallback_count") {
        aggregates.total_fallbacks = aggregates.total_fallbacks.saturating_add(fallback_count);
    }
    if let Some(model) = string_field(row, "final_model") {
        *aggregates.model_mix.entry(model.to_owned()).or_default() += 1;
    }
}

fn run_slot_result_value_from_artifact(
    sweep_dir: &Path,
    instance_id: &str,
    run_index: u32,
) -> Result<serde_json::Value, BundleError> {
    let path = required_rerun_trajectory_path(sweep_dir, instance_id, run_index)?;
    let text = std::fs::read_to_string(path)?;
    let trajectory: serde_json::Value = serde_json::from_str(&text)?;
    let info = trajectory
        .get("info")
        .ok_or_else(|| BundleError::Schema("trajectory missing info block".into()))?;

    let mut row = serde_json::Map::new();
    row.insert(
        "instance_id".into(),
        serde_json::Value::String(instance_id.to_owned()),
    );
    for key in [
        "outcome",
        "exit_reason",
        "failure_category",
        "steps",
        "duration_secs",
        "tests_run_before_submit",
        "last_tests_passed",
    ] {
        if let Some(value) = info.get(key) {
            row.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(value) = info.get("total_cost_usd") {
        row.insert("cost_usd".into(), value.clone());
    }
    if let Some(value) = info.get("actual_cost_usd") {
        row.insert("actual_cost_usd".into(), value.clone());
    }
    if let Some(tokens) = info.get("token_usage") {
        copy_token_field(tokens, &mut row, "prompt_tokens", "total_input_tokens");
        copy_token_field(
            tokens,
            &mut row,
            "cache_read_tokens",
            "total_cache_read_tokens",
        );
        copy_token_field(
            tokens,
            &mut row,
            "cache_creation_tokens",
            "total_cache_creation_tokens",
        );
        copy_token_field(
            tokens,
            &mut row,
            "completion_tokens",
            "total_completion_tokens",
        );
    }
    if let Some(summary) = info.get("fallback_summary") {
        let all_failed = summary
            .get("all_failed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !all_failed {
            if let Some(value) = summary.get("final_model") {
                row.insert("final_model".into(), value.clone());
            }
        }
        if let Some(value) = summary.get("fallback_count") {
            row.insert("fallback_count".into(), value.clone());
        }
    }
    if let Some(non_empty) =
        find_patch_path_for_run(sweep_dir, instance_id, run_index).and_then(|path| {
            std::fs::metadata(path)
                .ok()
                .map(|metadata| metadata.len() > 0)
        })
    {
        row.insert("patch_present".into(), serde_json::Value::Bool(true));
        row.insert("non_empty_patch".into(), serde_json::Value::Bool(non_empty));
    } else {
        row.insert("patch_present".into(), serde_json::Value::Bool(false));
        row.insert("non_empty_patch".into(), serde_json::Value::Bool(false));
    }
    Ok(serde_json::Value::Object(row))
}

fn copy_token_field(
    tokens: &serde_json::Value,
    row: &mut serde_json::Map<String, serde_json::Value>,
    source: &str,
    dest: &str,
) {
    if let Some(value) = tokens.get(source) {
        row.insert(dest.to_owned(), value.clone());
    }
}

fn effective_runs_value(value: &serde_json::Value) -> u32 {
    u64_field(value, "runs")
        .and_then(|runs| u32::try_from(runs).ok())
        .filter(|runs| *runs > 0)
        .unwrap_or(1)
}

fn u64_field(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn f64_field(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(serde_json::Value::as_f64)
}

fn instance_cost_usd(value: &serde_json::Value) -> Option<f64> {
    f64_field(value, "cost_usd").or_else(|| f64_field(value, "total_cost_usd"))
}

fn instance_actual_cost_usd(value: &serde_json::Value) -> Option<f64> {
    f64_field(value, "actual_cost_usd").or_else(|| f64_field(value, "cost_usd"))
}

fn sum_optional_f64(current: Option<f64>, next: Option<f64>) -> Option<f64> {
    match (current, next) {
        (Some(current), Some(next)) => Some(current + next),
        (Some(current), None) => Some(current),
        (None, Some(next)) => Some(next),
        (None, None) => None,
    }
}

fn retry_count(value: &serde_json::Value) -> u64 {
    u64_field(value, "retries")
        .unwrap_or_else(|| u64_field(value, "attempts").unwrap_or(1).saturating_sub(1))
}

fn instance_resolved(value: &serde_json::Value) -> bool {
    bool_field(value, "pass_at_1")
        || u64_field(value, "resolved_count").unwrap_or_default() > 0
        || string_field(value, "outcome") == Some("resolved")
}

fn cache_hit_rate(aggregates: &ScopedResultAggregates) -> f64 {
    let prompt = aggregates
        .total_input_tokens
        .saturating_add(aggregates.total_cache_read_tokens)
        .saturating_add(aggregates.total_cache_creation_tokens);
    if prompt == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            aggregates.total_cache_read_tokens as f64 / prompt as f64
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn pass_at_k(aggregates: &ScopedResultAggregates) -> f64 {
    if aggregates.total == 0 {
        0.0
    } else {
        aggregates.resolved_count as f64 / aggregates.total as f64
    }
}

fn bool_field(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn string_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

fn instance_ids_from_results(value: &serde_json::Value) -> Result<Vec<String>, BundleError> {
    let instances = value
        .get("instances")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            BundleError::MissingSource("bundle: results.json has no instances".into())
        })?;
    let mut ids = Vec::new();
    for row in instances {
        let id = row
            .get("instance_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                BundleError::MissingSource(
                    "bundle: results.json instance missing instance_id".into(),
                )
            })?;
        validate_instance_scope(Some(id))?;
        ids.push(id.to_owned());
    }
    Ok(ids)
}

fn included_instance_ids(
    all: &[String],
    requested: Option<&str>,
) -> Result<Vec<String>, BundleError> {
    match requested {
        Some(id) => {
            if all.iter().any(|candidate| candidate == id) {
                Ok(vec![id.to_owned()])
            } else {
                Err(BundleError::MissingSource(format!(
                    "bundle: instance `{id}` not found in results.json"
                )))
            }
        }
        None => Ok(all.to_vec()),
    }
}

fn result_row_for_instance<'a>(
    results: &'a serde_json::Value,
    instance_id: &str,
) -> Option<&'a serde_json::Value> {
    results
        .get("instances")
        .and_then(serde_json::Value::as_array)
        .and_then(|instances| {
            instances
                .iter()
                .find(|row| string_field(row, "instance_id") == Some(instance_id))
        })
}

fn is_never_started_budget_halt(row: &serde_json::Value) -> bool {
    string_field(row, "exit_reason") == Some("budget_halt")
        && row.get("outcome").is_none_or(serde_json::Value::is_null)
        && !bool_field(row, "patch_present")
}

fn validate_instance_scope(instance: Option<&str>) -> Result<(), BundleError> {
    let Some(instance) = instance else {
        return Ok(());
    };
    let invalid = instance.is_empty()
        || instance.contains('/')
        || instance.contains('\\')
        || instance.contains('\0')
        || instance == "."
        || instance == "..";
    if invalid {
        return Err(BundleError::MissingSource(format!(
            "bundle: invalid instance id for bundle layout: `{instance}`"
        )));
    }
    Ok(())
}

fn find_trajectory_path(sweep_dir: &Path, instance_id: &str) -> Option<PathBuf> {
    find_trajectory_path_for_run(sweep_dir, instance_id, 1)
}

fn find_single_run_trajectory_path(sweep_dir: &Path, instance_id: &str) -> Option<PathBuf> {
    let nested = sweep_dir.join(instance_id).join("run-1.traj.json");
    nested
        .exists()
        .then_some(nested)
        .or_else(|| find_trajectory_path(sweep_dir, instance_id))
}

fn required_rerun_trajectory_path(
    sweep_dir: &Path,
    instance_id: &str,
    run_index: u32,
) -> Result<PathBuf, BundleError> {
    let path = sweep_dir
        .join(instance_id)
        .join(format!("run-{run_index}.traj.json"));
    if path.exists() {
        return Ok(path);
    }
    Err(BundleError::MissingSource(format!(
        "bundle: missing trajectory for rerun slot `{instance_id}/run-{run_index}.traj.json` in {}",
        sweep_dir.display()
    )))
}

fn find_trajectory_path_for_run(
    sweep_dir: &Path,
    instance_id: &str,
    run_index: u32,
) -> Option<PathBuf> {
    let run_file = format!("run-{run_index}.traj.json");
    if run_index > 1 {
        let path = sweep_dir.join(instance_id).join(run_file);
        return path.exists().then_some(path);
    }
    [
        sweep_dir.join(instance_id).join("trajectory.json"),
        sweep_dir.join(instance_id).join(run_file),
        sweep_dir.join(format!("{instance_id}.traj.json")),
        sweep_dir
            .join("trajectories")
            .join(format!("{instance_id}.traj.json")),
    ]
    .into_iter()
    .find(|path| path.exists())
}

fn find_patch_path_for_run(sweep_dir: &Path, instance_id: &str, run_index: u32) -> Option<PathBuf> {
    let run_file = format!("run-{run_index}.patch");
    if run_index > 1 {
        let path = sweep_dir.join(instance_id).join(run_file);
        return path.exists().then_some(path);
    }
    [
        sweep_dir.join(instance_id).join(run_file),
        sweep_dir.join(format!("{instance_id}.patch")),
        sweep_dir
            .join("patches")
            .join(format!("{instance_id}.patch")),
    ]
    .into_iter()
    .find(|path| path.exists())
}

fn find_trajectory_paths_for_bundle(
    sweep_dir: &Path,
    instance_id: &str,
    row: &serde_json::Value,
) -> Result<Vec<(PathBuf, String)>, BundleError> {
    if is_never_started_budget_halt(row) {
        return Ok(Vec::new());
    }
    let runs = effective_runs_value(row);
    if runs > 1 {
        let mut out = Vec::new();
        for run_index in 1..=runs {
            let path = required_rerun_trajectory_path(sweep_dir, instance_id, run_index)?;
            out.push((path, format!("{instance_id}/run-{run_index}.traj.json")));
        }
        return Ok(out);
    }

    find_single_run_trajectory_path(sweep_dir, instance_id)
        .map(|path| {
            let archive_path = trajectory_archive_path_for_run(sweep_dir, instance_id, 1, &path);
            (path, archive_path)
        })
        .into_iter()
        .next()
        .map(|entry| vec![entry])
        .ok_or_else(|| {
            BundleError::MissingSource(format!(
                "bundle: missing trajectory for instance `{instance_id}` in {}",
                sweep_dir.display()
            ))
        })
}

fn find_patch_paths_for_bundle(
    sweep_dir: &Path,
    instance_id: &str,
    row: &serde_json::Value,
) -> Vec<(PathBuf, String)> {
    if is_never_started_budget_halt(row) {
        return Vec::new();
    }

    (1..=effective_runs_value(row))
        .filter_map(|run_index| {
            find_patch_path_for_run(sweep_dir, instance_id, run_index).map(|path| {
                let archive_path =
                    patch_archive_path_for_run(sweep_dir, instance_id, run_index, &path);
                (path, archive_path)
            })
        })
        .collect()
}

fn trajectory_archive_path_for_run(
    sweep_dir: &Path,
    instance_id: &str,
    run_index: u32,
    path: &Path,
) -> String {
    let file_name = format!("run-{run_index}.traj.json");
    if path == sweep_dir.join(instance_id).join(&file_name) {
        format!("{instance_id}/{file_name}")
    } else {
        format!("trajectories/{instance_id}.traj.json")
    }
}

fn patch_archive_path_for_run(
    sweep_dir: &Path,
    instance_id: &str,
    run_index: u32,
    path: &Path,
) -> String {
    let file_name = format!("run-{run_index}.patch");
    if path == sweep_dir.join(instance_id).join(&file_name) {
        format!("{instance_id}/{file_name}")
    } else {
        format!("patches/{instance_id}.patch")
    }
}

fn read_required_text(path: &Path, label: &str) -> Result<String, BundleError> {
    std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            BundleError::MissingSource(format!("bundle: missing {label}: {}", path.display()))
        } else {
            BundleError::Io(err)
        }
    })
}

fn normalized_text_file(path: &Path, normalizer: &PathNormalizer) -> Result<Vec<u8>, BundleError> {
    let text = std::fs::read_to_string(path)?;
    Ok(normalizer.normalize_text(&text).into_bytes())
}

fn normalized_json_bytes(
    value: &serde_json::Value,
    normalizer: &PathNormalizer,
) -> Result<Vec<u8>, BundleError> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(normalizer.normalize_text(&text).into_bytes())
}

fn strict_redaction_check(
    archive_path: &str,
    bytes: &[u8],
    redactor: &Redactor,
) -> Result<(), BundleError> {
    let text = std::str::from_utf8(bytes).map_err(|err| {
        BundleError::InvalidArchive(format!("bundle: {archive_path} is not UTF-8: {err}"))
    })?;
    let outcome = redactor.redact_text(text, surface::EXPORT);
    if outcome.redacted {
        return Err(BundleError::RedactionRetrigger {
            path: archive_path.to_owned(),
        });
    }
    Ok(())
}

fn write_archive_atomically(
    output_path: &Path,
    files: &[PreparedFile],
    bundle_bytes: &[u8],
) -> Result<(), BundleError> {
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = temporary_output_path(output_path);
    let result = write_archive(&tmp, files, bundle_bytes);
    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    std::fs::rename(&tmp, output_path)?;
    Ok(())
}

fn write_archive(
    output_path: &Path,
    files: &[PreparedFile],
    bundle_bytes: &[u8],
) -> Result<(), BundleError> {
    let file = std::fs::File::create(output_path)?;
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for file in files {
        append_tar_file_from_path(
            &mut builder,
            &file.archive_path,
            &file.source_path,
            file.bytes,
        )?;
    }
    append_tar_file(&mut builder, BUNDLE_MANIFEST_PATH, bundle_bytes)?;
    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(())
}

fn append_tar_file_from_path<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    archive_path: &str,
    source_path: &Path,
    bytes: u64,
) -> Result<(), BundleError> {
    validate_archive_path(archive_path)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    let mut file = std::fs::File::open(source_path)?;
    builder.append_data(&mut header, archive_path, &mut file)?;
    Ok(())
}

fn append_tar_file<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    archive_path: &str,
    bytes: &[u8],
) -> Result<(), BundleError> {
    validate_archive_path(archive_path)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder.append_data(&mut header, archive_path, &mut Cursor::new(bytes))?;
    Ok(())
}

fn temporary_output_path(output_path: &Path) -> PathBuf {
    let pid = std::process::id();
    let file_name = output_path
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "bundle.tar.gz".to_owned(), ToOwned::to_owned);
    output_path.with_file_name(format!(".{file_name}.{pid}.tmp"))
}

fn read_archive_inventory(archive_path: &Path) -> Result<ArchiveInventory, BundleError> {
    let file = std::fs::File::open(archive_path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut inventory = ArchiveInventory {
        bundle_bytes: None,
        entries: BTreeMap::new(),
    };
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?;
        let path = normalize_archive_path(&raw_path)?;
        if entry.header().entry_type().is_file() {
            if path == BUNDLE_MANIFEST_PATH {
                if inventory.bundle_bytes.is_some() {
                    return Err(BundleError::InvalidArchive(format!(
                        "bundle: duplicate archive entry {path}"
                    )));
                }
                let mut bundle_bytes = Vec::new();
                entry.read_to_end(&mut bundle_bytes)?;
                inventory.bundle_bytes = Some(bundle_bytes);
                continue;
            }
            let (digest, bytes) = hash_reader(&mut entry)?;
            let info = ArchiveEntryInfo {
                sha256: Some(digest),
                bytes,
                regular_file: true,
            };
            if inventory.entries.insert(path.clone(), info).is_some() {
                return Err(BundleError::InvalidArchive(format!(
                    "bundle: duplicate archive entry {path}"
                )));
            }
        } else {
            let info = ArchiveEntryInfo {
                sha256: None,
                bytes: 0,
                regular_file: false,
            };
            if inventory.entries.insert(path.clone(), info).is_some() {
                return Err(BundleError::InvalidArchive(format!(
                    "bundle: duplicate archive entry {path}"
                )));
            }
        }
    }
    Ok(inventory)
}

fn hash_reader<R: std::io::Read>(reader: &mut R) -> Result<(String, u64), std::io::Error> {
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 8192];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

fn validate_archive_path(path: &str) -> Result<(), BundleError> {
    let normalized = normalize_archive_path(Path::new(path))?;
    if normalized != path {
        return Err(BundleError::InvalidArchive(format!(
            "bundle: non-normalized archive path `{path}`"
        )));
    }
    Ok(())
}

fn normalize_archive_path(path: &Path) -> Result<String, BundleError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| {
                    BundleError::InvalidArchive("bundle: archive path is not UTF-8".into())
                })?;
                parts.push(text.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(BundleError::InvalidArchive(format!(
                    "bundle: unsafe archive path `{}`",
                    path.display()
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(BundleError::InvalidArchive(
            "bundle: empty archive path".into(),
        ));
    }
    Ok(parts.join("/"))
}

fn bundle_generated_at() -> Result<String, BundleError> {
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
        let seconds = epoch.parse::<i64>().map_err(|err| {
            BundleError::MissingSource(format!("bundle: invalid SOURCE_DATE_EPOCH: {err}"))
        })?;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0).ok_or_else(|| {
            BundleError::MissingSource("bundle: SOURCE_DATE_EPOCH is out of range".into())
        })?;
        return Ok(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    Ok(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn current_git_sha() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let sha = text.trim();
    (!sha.is_empty()).then(|| sha.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn push_path_needles(out: &mut BTreeSet<String>, raw: &str) {
    if raw.is_empty() {
        return;
    }
    let slash = raw.replace('\\', "/");
    let backslash = raw.replace('/', "\\");
    for value in [raw.to_owned(), slash, backslash] {
        out.insert(value.clone());
        out.insert(value.replace('\\', "\\\\"));
    }
}

fn sorted_path_needles<I>(needles: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut needles = needles
        .into_iter()
        .filter(|value| !value.is_empty() && value != ".")
        .collect::<Vec<_>>();
    needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    needles
}

#[cfg(test)]
mod tests {
    use super::{PathNormalizer, sorted_path_needles};

    #[test]
    fn path_normalizer_prefers_longest_needle_first() {
        let normalizer = PathNormalizer {
            needles: sorted_path_needles(vec!["/tmp/a".into(), "/tmp/a/b".into()]),
            windows_abs: None,
            unix_abs: None,
        };

        assert_eq!(
            normalizer.normalize_text("path=/tmp/a/b/file"),
            "path=./file"
        );
    }
}
