//! `bench import`: ingest an external SWE-bench predictions file and materialise
//! it as a normalised sweep directory that `bench compare`, `bench triage`, and
//! `bench report` can consume without any model spend.
//!
//! # Input format
//!
//! The predictions file may be:
//! * **JSONL** — one JSON object per line, each carrying at minimum
//!   `instance_id` and `model_patch`. `model_name_or_path` is optional.
//! * **JSON array** — a top-level array of those same objects.
//!
//! This matches the standard SWE-bench submission format used by the public
//! [`experiments/`](https://github.com/swe-bench/experiments) repository.
//!
//! # Output
//!
//! A normalised sweep directory is written to `--output`.  It contains:
//! * `results.json` — a `SweepResults` artifact with `artifact_kind =
//!   "sweep_results"`, `total_cost_usd = 0.0`, `steps = null` per instance,
//!   and `manifest.source = "external_import"` so downstream tooling can
//!   distinguish it from harness-native runs.
//! * `<instance_id>/run-1.patch` — the model patch, written for every instance
//!   whose `model_patch` field is non-empty.
//! * `all_preds.jsonl` — evaluator-compatible predictions file (only submitted
//!   records with non-empty patches), suitable for `bench evaluate --backend sb-cli`.
//!
//! Without `--evaluate`, `resolved` flags are absent (all `pass_at_1 = false`,
//! `resolved_count = 0`). A separate `bench evaluate` pass fills them in.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest, FilterSpec, HarnessManifest, InstanceResult,
    ModelManifest, PromptTemplateManifest, ProvenanceManifest, RuntimeManifest,
    SWEEP_STATUS_COMPLETED, SweepResults, write_sweep_results_atomic,
};
use crate::trajectory::outcome;

// ── public surface ────────────────────────────────────────────────────────────

/// Arguments for `bench import`.
#[derive(Debug, Clone)]
pub struct ImportArgs {
    /// Path to the SWE-bench predictions file (JSONL or JSON array).
    pub predictions: PathBuf,
    /// Path to the matching SWE-bench dataset JSONL (used for instance-id validation).
    pub dataset_path: PathBuf,
    /// Output directory for the normalised sweep.
    pub output: PathBuf,
    /// When `true`, run the evaluator pipeline after import (not yet implemented;
    /// presence of the flag is validated so callers get a clear error).
    pub evaluate: bool,
    /// Output format for the summary printed to stdout.
    pub format: ImportFormat,
}

/// Output format for the import summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Text,
    Json,
}

/// Summary object emitted to stdout (especially in `--format json` mode).
///
/// Stable CI-snapshotting artifact: all fields must remain present across
/// schema-compatible versions.
#[derive(Debug, Clone, Serialize)]
pub struct ImportSummary {
    /// Total number of prediction records that were imported (including those
    /// with errors such as an unrecognised `instance_id`).
    pub records_imported: usize,
    /// Number of records that could not be attributed to any instance (e.g.
    /// missing `instance_id` field or duplicate). These are excluded from
    /// `results.json`.
    pub records_skipped: usize,
    /// Per-record reasons for the `records_skipped` entries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skip_reasons: Vec<SkipReason>,
    /// Canonical path of the output sweep directory.
    pub output_path: String,
    /// `sha256:<lowercase-hex>` content digest of the source predictions file.
    pub source_hash: String,
}

/// One skip reason entry.
#[derive(Debug, Clone, Serialize)]
pub struct SkipReason {
    /// Zero-based index of the record in the predictions file.
    pub record_index: usize,
    /// Human-readable reason the record was skipped.
    pub reason: String,
}

// ── one raw prediction record ─────────────────────────────────────────────────

/// Minimal shape of one SWE-bench predictions record.
#[derive(Debug, Clone, Deserialize)]
struct PredictionRecord {
    /// Required: SWE-bench instance identifier.
    #[serde(default)]
    pub instance_id: Option<String>,
    /// The model-generated patch (unified diff). May be empty.
    #[serde(default)]
    pub model_patch: Option<String>,
    /// Model identifier. Used for the manifest `model.name` field.
    #[serde(default)]
    pub model_name_or_path: Option<String>,
}

// ── internal helpers ──────────────────────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn parse_predictions(bytes: &[u8]) -> Result<Vec<(usize, Option<PredictionRecord>)>, Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Error::Trajectory(format!("predictions file utf-8 decode: {e}")))?;
    let text = text.trim();

    // Try JSON array first.
    if text.starts_with('[') {
        let records: Vec<serde_json::Value> = serde_json::from_str(text)
            .map_err(|e| Error::Trajectory(format!("predictions JSON array parse: {e}")))?;
        return Ok(records
            .into_iter()
            .enumerate()
            .map(|(i, val)| {
                let rec: Option<PredictionRecord> = serde_json::from_value(val).ok();
                (i, rec)
            })
            .collect());
    }

    // Otherwise treat as JSONL.
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: Option<PredictionRecord> = serde_json::from_str(line).ok();
        out.push((i, rec));
    }
    Ok(out)
}

/// Parse dataset JSONL bytes and return the set of known instance IDs.
fn load_dataset_instance_ids(bytes: &[u8]) -> Result<std::collections::HashSet<String>, Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Error::Trajectory(format!("dataset utf-8 decode: {e}")))?;
    let mut ids = std::collections::HashSet::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| Error::Trajectory(format!("dataset line {}: {e}", i + 1)))?;
        match val.get("instance_id").and_then(|v| v.as_str()) {
            Some(id) => {
                ids.insert(id.to_owned());
            }
            None => {
                return Err(Error::Trajectory(format!(
                    "dataset line {} is missing required `instance_id` field; \
                     verify --dataset-path points to a SWE-bench dataset JSONL",
                    i + 1
                )));
            }
        }
    }
    Ok(ids)
}

fn resolve_harness_manifest() -> HarnessManifest {
    HarnessManifest {
        name: env!("CARGO_PKG_NAME").to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        git_sha: None,
        git_dirty: None,
        git_resolution: "import".to_owned(),
    }
}

fn write_patch_file(output_dir: &Path, instance_id: &str, patch: &str) -> Result<(), Error> {
    let instance_dir = output_dir.join(instance_id);
    std::fs::create_dir_all(&instance_dir)?;
    let patch_path = instance_dir.join("run-1.patch");
    let mut f = tempfile::NamedTempFile::new_in(&instance_dir)?;
    f.write_all(patch.as_bytes())?;
    f.as_file_mut().sync_all()?;
    f.persist(&patch_path).map_err(|e| e.error)?;
    Ok(())
}

/// Write `all_preds.jsonl` — the evaluator-compatible predictions file that
/// `bench evaluate --backend sb-cli` expects in the sweep directory.
///
/// Only SUBMITTED records with a non-empty patch are included (same filter
/// that `write_predictions_file` applies for native sweeps).
fn write_all_preds_jsonl(
    output_dir: &Path,
    instances: &[InstanceResult],
    model_name: &str,
) -> Result<(), Error> {
    let path = crate::run::swebench::predictions_path(output_dir);
    let mut content = String::new();
    for inst in instances {
        if inst.outcome.as_deref() != Some(outcome::SUBMITTED) || !inst.non_empty_patch {
            continue;
        }
        let patch_path = output_dir.join(&inst.instance_id).join("run-1.patch");
        let model_patch = std::fs::read_to_string(&patch_path).unwrap_or_default();
        let line = serde_json::json!({
            "instance_id": inst.instance_id,
            "model_patch": model_patch,
            "model_name_or_path": model_name,
        });
        let _ = writeln!(
            content,
            "{}",
            serde_json::to_string(&line).map_err(Error::Json)?
        );
    }
    std::fs::write(&path, &content)?;
    Ok(())
}

// ── core run function ─────────────────────────────────────────────────────────

/// Run `bench import`.
#[allow(clippy::too_many_lines)]
pub fn run(args: &ImportArgs) -> Result<ImportSummary, Error> {
    // 0. Fail fast on unsupported flags before touching the filesystem.
    if args.evaluate {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "bench import --evaluate is not yet implemented; run `bench evaluate` \
             separately after import to populate resolved flags"
                .to_owned(),
        )));
    }

    // 1. Read and hash the predictions file.
    let predictions_bytes = std::fs::read(&args.predictions).map_err(|e| {
        Error::Trajectory(format!(
            "cannot read predictions file {}: {e}",
            args.predictions.display()
        ))
    })?;
    let predictions_sha256 = format!("sha256:{}", sha256_hex(&predictions_bytes));

    // 2. Parse prediction records.
    let raw_records = parse_predictions(&predictions_bytes)?;

    // 3. Read dataset file once: used for both instance-ID validation and the manifest hash.
    let dataset_bytes = std::fs::read(&args.dataset_path).map_err(|e| {
        Error::Trajectory(format!(
            "cannot read dataset file {}: {e}",
            args.dataset_path.display()
        ))
    })?;
    let dataset_sha256 = sha256_hex(&dataset_bytes);
    let dataset_ids = load_dataset_instance_ids(&dataset_bytes)?;

    // 4. Create output directory — reject if a prior sweep already exists there.
    if args.output.join("results.json").exists() {
        return Err(Error::Config(crate::error::ConfigError::Usage(format!(
            "bench import: output directory '{}' already contains results.json from a \
             previous sweep; specify a new directory or remove the existing one first",
            args.output.display()
        ))));
    }
    std::fs::create_dir_all(&args.output)?;

    // 5. Process records, deduplicating on instance_id.
    let mut instances: Vec<InstanceResult> = Vec::with_capacity(raw_records.len());
    let mut skip_reasons: Vec<SkipReason> = Vec::new();
    let mut seen_instance_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut model_name: Option<String> = None;

    for (idx, maybe_rec) in raw_records {
        let Some(rec) = maybe_rec else {
            skip_reasons.push(SkipReason {
                record_index: idx,
                reason: "record could not be parsed as JSON object".to_owned(),
            });
            continue;
        };

        let instance_id = match rec.instance_id {
            Some(ref id) if !id.is_empty() => id.clone(),
            _ => {
                skip_reasons.push(SkipReason {
                    record_index: idx,
                    reason: "missing or empty instance_id".to_owned(),
                });
                continue;
            }
        };

        // Skip duplicates — downstream tools assume unique instance IDs.
        if !seen_instance_ids.insert(instance_id.clone()) {
            skip_reasons.push(SkipReason {
                record_index: idx,
                reason: format!("duplicate instance_id `{instance_id}`"),
            });
            continue;
        }

        // Capture model name from first record that supplies it.
        if model_name.is_none() {
            if let Some(ref m) = rec.model_name_or_path {
                if !m.is_empty() {
                    model_name = Some(m.clone());
                }
            }
        }

        let patch = rec.model_patch.unwrap_or_default();
        let non_empty_patch = !patch.trim().is_empty();
        let in_dataset = dataset_ids.contains(&instance_id);

        // Validate instance_id against dataset — report but never drop.
        let error_msg: Option<String> = (!in_dataset).then(|| {
            format!(
                "instance_id `{instance_id}` not found in dataset {}",
                args.dataset_path.display()
            )
        });

        // Write patch file for all non-empty patches (even unknown IDs — the
        // patch is valid data regardless of dataset membership).
        if non_empty_patch {
            write_patch_file(&args.output, &instance_id, &patch)?;
        }

        let (exit_reason, result_outcome, patch_present) = if non_empty_patch {
            ("submitted", Some(outcome::SUBMITTED.to_owned()), true)
        } else {
            ("error", Some(outcome::ERROR.to_owned()), false)
        };

        instances.push(InstanceResult {
            instance_id: instance_id.clone(),
            exit_reason: exit_reason.to_owned(),
            outcome: result_outcome,
            failure_category: None,
            steps: None,         // zero-cost: steps not available
            cost_usd: Some(0.0), // zero-cost guarantee
            prompt_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            duration_secs: None,
            error: error_msg,
            github_pr_error: None,
            patch_present,
            non_empty_patch,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 1,
            resolved_count: 0, // populated by bench evaluate
            pass_at_1: false,  // populated by bench evaluate
            tests_run_before_submit: false,
            last_tests_passed: None,
            fallback_count: None,
            final_model: None,
            retry_id: None,
            previous_failure_category: None,
            trace_id: None,
        });
    }

    // 6. Build aggregate counts.
    let total = instances.len();
    let submitted_count = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
        .count();
    let errored_count = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
        .count();
    let with_patch_count = instances.iter().filter(|r| r.non_empty_patch).count();

    // 7. Build manifest.
    let started_at_utc = chrono::Utc::now().to_rfc3339();
    let predictions_path_str = args
        .predictions
        .canonicalize()
        .unwrap_or_else(|_| args.predictions.clone())
        .display()
        .to_string();
    let used_model_name = model_name.unwrap_or_else(|| "external".to_owned());

    let manifest = ProvenanceManifest {
        purpose: Some("external_import".to_owned()),
        harness: resolve_harness_manifest(),
        dataset: DatasetManifest {
            path: args
                .dataset_path
                .canonicalize()
                .unwrap_or_else(|_| args.dataset_path.clone())
                .display()
                .to_string(),
            sha256: dataset_sha256,
            instance_count: dataset_ids.len(),
            filter_spec: None,
            source_kind: "local".to_owned(),
            alias: None,
            split: None,
            source_revision: None,
            cache_path: None,
            selected_row_count: dataset_ids.len(),
            post_filter_row_count: total,
        },
        prompt_template: PromptTemplateManifest {
            source: "external_import".to_owned(),
            path: None,
            sha256: "n/a".to_owned(),
        },
        config: ConfigManifest {
            resolved: String::new(),
            overlay_paths: Vec::new(),
        },
        model: ModelManifest {
            name: used_model_name.clone(),
            backend: "external".to_owned(),
            backend_version: None,
            base_url: None,
        },
        runtime: RuntimeManifest {
            started_at_utc,
            finished_at_utc: Some(chrono::Utc::now().to_rfc3339()),
            host_os: std::env::consts::OS.to_owned(),
            resume_mode: false,
            rust_version: None,
        },
        cli: CliManifest {
            argv: std::env::args().collect(),
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: Some("external_import".to_owned()),
        import_predictions_path: Some(predictions_path_str),
        import_predictions_sha256: Some(predictions_sha256.clone()),
        reproduced_from: None,
    };

    // 8. Build SweepResults.
    let sweep = SweepResults {
        total,
        sweep_status: SWEEP_STATUS_COMPLETED.to_owned(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: total,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: submitted_count,
        submitted_with_tests: 0,
        skipped: 0,
        errored: errored_count,
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: with_patch_count,
        patch_empty: submitted_count.saturating_sub(with_patch_count),
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 0,
        estimated_cost_usd: 0.0,
        actual_cost_usd: Some(0.0),
        actual_cost_source: None,
        baseline_cost_usd: None,
        baseline_cost_model: None,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: 0.0,
        filter_spec: FilterSpec {
            original_count: dataset_ids.len(),
            selected_count: total,
            ..FilterSpec::default()
        },
        manifest: Some(manifest),
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: Vec::new(),
        partial: 0,
        span_export_dropped: 0,
    };

    // 9. Write results.json atomically.
    let results_path = args.output.join("results.json");
    write_sweep_results_atomic(&results_path, &sweep)?;

    // 10. Write all_preds.jsonl so `bench evaluate --backend sb-cli` can submit without
    //     needing to reconstruct the predictions from the per-instance patch files.
    write_all_preds_jsonl(&args.output, &sweep.instances, &used_model_name)?;

    let canonical_output = args
        .output
        .canonicalize()
        .unwrap_or_else(|_| args.output.clone());

    let summary = ImportSummary {
        records_imported: total,
        records_skipped: skip_reasons.len(),
        skip_reasons,
        output_path: canonical_output.display().to_string(),
        source_hash: predictions_sha256,
    };

    Ok(summary)
}

/// Format the import summary as a human-readable text block.
pub fn format_summary_text(summary: &ImportSummary) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "=== bench import ===");
    let _ = writeln!(s, "Records imported:   {}", summary.records_imported);
    let _ = writeln!(s, "Records skipped:    {}", summary.records_skipped);
    let _ = writeln!(s, "Output:             {}", summary.output_path);
    let _ = writeln!(s, "Source hash:        {}", summary.source_hash);
    if !summary.skip_reasons.is_empty() {
        let _ = writeln!(s, "\nSkipped records:");
        for sr in &summary.skip_reasons {
            let _ = writeln!(s, "  [{}] {}", sr.record_index, sr.reason);
        }
    }
    s
}
