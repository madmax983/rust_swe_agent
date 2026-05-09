//! `bench evaluate`: score an existing sweep by real resolved-rate.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::compare::{load_run_slots, load_sweep};
use crate::run::patch_stats::{PatchClassifiers, PatchStats, score_patch};
use crate::run::swebench::{self, InstanceResult, TokenBreakdown, effective_runs};
use crate::trajectory::{FailureCategory, outcome};

/// Evaluator provenance recorded in every `bench evaluate` artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvaluatorProvenance {
    /// Evaluator backend name: `"sb-cli"` or `"none"`.
    pub backend: String,
    /// Backend/tool version when determinable (e.g. `sb-cli --version` output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_version: Option<String>,
    /// SWE-bench dataset subset (e.g. `"swe-bench-m"`, `"swe-bench_lite"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_subset: Option<String>,
    /// SWE-bench dataset split (e.g. `"dev"`, `"test"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_split: Option<String>,
    /// Run ID supplied to the evaluator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Path to the predictions file consumed by the evaluator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction_path: Option<String>,
    /// SHA-256 hex digest of the predictions file at evaluation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction_sha256: Option<String>,
    /// ISO 8601 UTC timestamp when the evaluation run started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_started_at: Option<String>,
    /// ISO 8601 UTC timestamp when the evaluation run ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_ended_at: Option<String>,
    /// Human-readable description of how the report was obtained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_source: Option<String>,
    /// `sb-cli`-specific provenance fields; present only when `backend == "sb-cli"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sb_cli: Option<SbCliProvenance>,
    /// One entry per run slot for rerun/pass@k evaluations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_reports: Vec<SourceReportEntry>,
}

/// Provenance specific to `sb-cli` submit/get-report command pairs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SbCliProvenance {
    /// Redacted `sb-cli submit` command shape used for submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit_command: Option<String>,
    /// Redacted `sb-cli get-report` command shape used for report retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_command: Option<String>,
    /// Paths to the generated report JSON files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub report_paths: Vec<String>,
    /// SHA-256 hex digests of the report files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub report_hashes: Vec<String>,
    pub verify_submission: bool,
    pub wait_for_evaluation: bool,
    pub overwrite: bool,
    pub timeout_per_instance_secs: u64,
    pub parallel: usize,
}

/// One source-report entry for a single run slot in a rerun/pass@k evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceReportEntry {
    pub run_index: u32,
    /// Path to the report file for this run slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
    /// SHA-256 hex digest of the report file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_sha256: Option<String>,
    /// Instance IDs whose rows were influenced by this run slot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instance_ids: Vec<String>,
}

pub const COST_ATTRIBUTION_RESOLVED_BUCKET: &str = "resolved";
pub const COST_ATTRIBUTION_UNCATEGORIZED_BUCKET: &str = "uncategorized";
pub const COST_ATTRIBUTION_TOTAL_BUCKET: &str = "TOTAL";
pub const ALL_FAILURE_CATEGORIES: [FailureCategory; 11] = [
    FailureCategory::EnvSetup,
    FailureCategory::ModelApi,
    FailureCategory::ModelParse,
    FailureCategory::StepLimit,
    FailureCategory::CostLimit,
    FailureCategory::WallclockTimeout,
    FailureCategory::AgentInternal,
    FailureCategory::PatchApplyInvalid,
    FailureCategory::PatchEmpty,
    FailureCategory::SecretLeakDetected,
    FailureCategory::Unknown,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluateBackend {
    SbCli,
    None,
}

#[derive(Debug, Clone)]
pub struct EvaluateArgs {
    pub sweep_dir: PathBuf,
    pub dataset_path: Option<PathBuf>,
    pub backend: EvaluateBackend,
    pub timeout_per_instance_secs: u64,
    pub parallel: usize,
    pub sb_subset: String,
    pub sb_split: String,
    pub run_id: Option<String>,
    pub breakdown: BreakdownSelection,
    pub cost_attribution: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakdownAxis {
    Repo,
    FailureCategory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakdownSelection {
    pub axes: Vec<BreakdownAxis>,
}

impl BreakdownSelection {
    #[must_use]
    pub fn none() -> Self {
        Self { axes: Vec::new() }
    }

    #[must_use]
    pub fn default_axes() -> Self {
        Self {
            axes: vec![BreakdownAxis::Repo, BreakdownAxis::FailureCategory],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalExitReason {
    Resolved,
    Unresolved,
    PatchApplyFailed,
    EvalError,
    SkippedNoPatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceEvaluation {
    pub instance_id: String,
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub runs: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub resolved_count: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pass_at_1: bool,
    #[serde(default)]
    pub tests_passed: Vec<String>,
    #[serde(default)]
    pub tests_failed: Vec<String>,
    pub eval_exit_reason: EvalExitReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_stats: Option<PatchStats>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResults {
    pub instances: Vec<InstanceEvaluation>,
    #[serde(default)]
    pub behavioral: BehavioralMetrics,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdown: Vec<BreakdownBucket>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cost_attribution: Vec<CostAttributionBucket>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_mix_summary: Vec<ModelMixBucket>,
    /// Evaluator provenance recorded at evaluation time.
    /// `None` for artifacts produced before this field was added (legacy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<EvaluatorProvenance>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct BehavioralMetrics {
    pub tests_run_before_submit_rate: f64,
    pub resolved_rate_when_tests_run: f64,
    pub resolved_rate_when_tests_skipped: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvaluationSummary {
    pub instances: usize,
    pub resolved: usize,
    pub resolved_rate: f64,
    pub pass_at_1: f64,
    pub pass_at_k: f64,
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
    pub cache_hit_rate: f64,
    /// Total cost divided by resolved count. `f64::NAN` when `resolved == 0`.
    /// Computed on budget-respecting runs only when budget-exhausted instances
    /// are present in the sweep.
    pub cost_per_resolved_usd: f64,
    /// 95% bootstrap CI lower bound. `f64::NAN` when `resolved == 0`.
    pub cost_per_resolved_ci95_lower: f64,
    /// 95% bootstrap CI upper bound. `f64::NAN` when `resolved == 0`.
    pub cost_per_resolved_ci95_upper: f64,
    /// Instances excluded from `cost_per_resolved_usd` because they were
    /// killed by the per-task budget cap. Zero when no budget is active.
    pub budget_exhausted_excluded: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BreakdownBucket {
    pub bucket_axis: BreakdownAxis,
    pub bucket_value: String,
    pub n: usize,
    pub resolved: usize,
    pub resolved_rate: f64,
    /// `total_cost_usd / resolved` for this bucket, excluding budget-exhausted
    /// instances. `None` when no instance in this bucket resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_resolved_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostAttributionBucket {
    pub bucket: String,
    pub n: usize,
    pub total_usd: f64,
    pub mean_usd: f64,
    pub share_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMixBucket {
    pub model: String,
    pub n: usize,
    pub resolved: usize,
    pub resolved_rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct CostAttributionReport {
    rows: Vec<CostAttributionBucket>,
    missing_cost_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunSlotKey {
    instance_id: String,
    run_index: u32,
}

impl RunSlotKey {
    fn new(instance_id: &str, run_index: u32) -> Self {
        Self {
            instance_id: instance_id.to_owned(),
            run_index,
        }
    }
}

#[derive(Debug, Clone)]
struct EvaluateRunOutput {
    eval: EvaluationResults,
    resolved_by_run: HashMap<RunSlotKey, bool>,
    /// The actual run id used (may be auto-generated when args.run_id is None).
    effective_run_id: Option<String>,
}

impl EvaluateRunOutput {
    fn without_run_resolution(eval: EvaluationResults) -> Self {
        Self {
            eval,
            resolved_by_run: HashMap::new(),
            effective_run_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CostAttributionSample {
    resolved: bool,
    failure_category: Option<FailureCategory>,
    cost_usd: Option<f64>,
}

#[must_use]
pub fn evaluation_path(sweep_dir: &Path) -> PathBuf {
    sweep_dir.join("evaluation.json")
}

pub fn run(args: &EvaluateArgs) -> Result<EvaluationResults, Error> {
    let loaded = load_sweep(&args.sweep_dir)?;
    let model_name = loaded.manifest.as_ref().map(|m| m.model.name.clone());
    if loaded.manifest.as_ref().and_then(|m| m.purpose.as_deref()) == Some("forecast") {
        return Err(Error::Trajectory(
            "bench evaluate: refusing to evaluate forecast calibration output".into(),
        ));
    }
    let results = loaded.instances;
    let eval_started_at = utc_now_iso8601();
    let run_output = match args.backend {
        EvaluateBackend::None => {
            EvaluateRunOutput::without_run_resolution(build_none_eval(&results))
        }
        EvaluateBackend::SbCli => run_sb_cli(args, &results)?,
    };
    let EvaluateRunOutput {
        mut eval,
        resolved_by_run,
        effective_run_id,
    } = run_output;
    let provenance = build_provenance(
        args,
        &resolved_by_run,
        effective_run_id.as_deref(),
        eval_started_at,
    );
    attach_patch_stats(&mut eval, args, &resolved_by_run)?;
    eval.behavioral = build_behavioral_metrics(&eval.instances, &results);
    eval.breakdown = build_breakdown(
        &eval.instances,
        &results,
        &args.breakdown,
        model_name.as_deref(),
    );
    let run_slots = load_run_slots(&args.sweep_dir, &results)?;
    if args.cost_attribution {
        eval.cost_attribution = build_cost_attribution_from_run_slots(
            &run_slots,
            &resolved_by_run,
            model_name.as_deref(),
        )
        .rows;
    }
    eval.model_mix_summary = build_model_mix_summary_from_slots(&run_slots, &resolved_by_run);
    eval.provenance = Some(provenance);
    let file = std::fs::File::create(evaluation_path(&args.sweep_dir))?;
    crate::artifact::to_writer_pretty(
        file,
        crate::artifact::ArtifactKind::EvaluationResults,
        &eval,
    )?;
    Ok(eval)
}

fn build_provenance(
    args: &EvaluateArgs,
    resolved_by_run: &HashMap<RunSlotKey, bool>,
    effective_run_id: Option<&str>,
    started_at: String,
) -> EvaluatorProvenance {
    let run_id_str = effective_run_id
        .or(args.run_id.as_deref())
        .unwrap_or_default();
    let (backend_str, sb_cli, source_reports) = match args.backend {
        EvaluateBackend::None => ("none", None, vec![]),
        EvaluateBackend::SbCli => {
            let preds = swebench::predictions_path(&args.sweep_dir);
            let report_dir = args.sweep_dir.join("sb_cli_reports");

            let submit_cmd = build_redacted_submit_command(args, &preds, &report_dir, run_id_str);
            let report_cmd = build_redacted_report_command(args, &report_dir, run_id_str);
            let report_paths = collect_report_paths(&report_dir, run_id_str, args);
            let report_hashes = report_paths
                .iter()
                .filter_map(|p| sha256_file(std::path::Path::new(p)).ok())
                .collect();
            let source_reports = build_source_reports(resolved_by_run, args, run_id_str);
            let sb = SbCliProvenance {
                submit_command: Some(submit_cmd),
                report_command: Some(report_cmd),
                report_paths,
                report_hashes,
                verify_submission: false,
                wait_for_evaluation: true,
                overwrite: true,
                timeout_per_instance_secs: args.timeout_per_instance_secs,
                parallel: args.parallel,
            };
            ("sb-cli", Some(sb), source_reports)
        }
    };

    let prediction_path = match args.backend {
        EvaluateBackend::SbCli => {
            let p = swebench::predictions_path(&args.sweep_dir);
            Some(p.display().to_string())
        }
        EvaluateBackend::None => None,
    };
    let prediction_sha256 = prediction_path
        .as_deref()
        .and_then(|p| sha256_file(std::path::Path::new(p)).ok());

    let recorded_run_id = effective_run_id
        .map(str::to_owned)
        .or_else(|| args.run_id.clone());
    EvaluatorProvenance {
        backend: backend_str.into(),
        backend_version: match args.backend {
            EvaluateBackend::SbCli => probe_sb_cli_version(),
            EvaluateBackend::None => None,
        },
        dataset_subset: Some(args.sb_subset.clone()),
        dataset_split: Some(args.sb_split.clone()),
        run_id: recorded_run_id,
        prediction_path,
        prediction_sha256,
        eval_started_at: Some(started_at),
        eval_ended_at: Some(utc_now_iso8601()),
        report_source: None,
        sb_cli,
        source_reports,
    }
}

fn build_redacted_submit_command(
    args: &EvaluateArgs,
    preds: &Path,
    report_dir: &Path,
    run_id: &str,
) -> String {
    let redactor = crate::redaction::Redactor::default_enabled();
    let raw = format!(
        "sb-cli submit {} {} --predictions_path {} --run_id {} --output_dir {} --wait_for_evaluation 1 --gen_report 1 --timeout-per-instance {} --parallel {}",
        args.sb_subset,
        args.sb_split,
        preds.display(),
        run_id,
        report_dir.display(),
        args.timeout_per_instance_secs,
        args.parallel,
    );
    redactor.redact_text(&raw, "evaluator_provenance").text
}

fn build_redacted_report_command(args: &EvaluateArgs, report_dir: &Path, run_id: &str) -> String {
    let redactor = crate::redaction::Redactor::default_enabled();
    let raw = format!(
        "sb-cli get-report {} {} --run_id {} --output_dir {} --overwrite 1",
        args.sb_subset,
        args.sb_split,
        run_id,
        report_dir.display(),
    );
    redactor.redact_text(&raw, "evaluator_provenance").text
}

fn collect_report_paths(report_dir: &Path, run_id: &str, args: &EvaluateArgs) -> Vec<String> {
    let path = report_dir.join(format!(
        "{}__{}__{}.json",
        args.sb_subset, args.sb_split, run_id
    ));
    if path.exists() {
        vec![path.display().to_string()]
    } else {
        vec![]
    }
}

fn build_source_reports(
    resolved_by_run: &HashMap<RunSlotKey, bool>,
    args: &EvaluateArgs,
    run_id_str: &str,
) -> Vec<SourceReportEntry> {
    if resolved_by_run.is_empty() {
        return vec![];
    }
    let max_run_index = resolved_by_run
        .keys()
        .map(|k| k.run_index)
        .max()
        .unwrap_or(1);
    if max_run_index <= 1 {
        return vec![];
    }
    let mut ids_by_run: HashMap<u32, Vec<String>> = HashMap::new();
    for key in resolved_by_run.keys() {
        ids_by_run
            .entry(key.run_index)
            .or_default()
            .push(key.instance_id.clone());
    }
    let report_dir = args.sweep_dir.join("sb_cli_reports");
    let run_id = run_id_str;
    (1..=max_run_index)
        .map(|run_index| {
            let run_report_id = format!("{run_id}-run-{run_index}");
            let report_path = report_dir.join(format!(
                "{}__{}__{}.json",
                args.sb_subset, args.sb_split, run_report_id
            ));
            let path_str = if report_path.exists() {
                Some(report_path.display().to_string())
            } else {
                None
            };
            let report_sha256 = path_str
                .as_deref()
                .and_then(|p| sha256_file(std::path::Path::new(p)).ok());
            let instance_ids = ids_by_run.remove(&run_index).unwrap_or_default();
            SourceReportEntry {
                run_index,
                report_path: path_str,
                report_sha256,
                instance_ids,
            }
        })
        .collect()
}

fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    let digest = hasher.finalize();
    Ok(format!("{digest:x}"))
}

fn probe_sb_cli_version() -> Option<String> {
    let output = Command::new("sb-cli").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.trim().to_owned();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn attach_patch_stats(
    eval: &mut EvaluationResults,
    args: &EvaluateArgs,
    resolved_by_run: &HashMap<RunSlotKey, bool>,
) -> Result<(), Error> {
    let classifiers = PatchClassifiers::from_default_toml()?;
    let gold_patches = load_gold_patches(args.dataset_path.as_deref())?;
    for row in &mut eval.instances {
        let run_index = patch_stats_run_index(&row.instance_id, resolved_by_run);
        let patch_text = read_patch_or_empty(&args.sweep_dir, &row.instance_id, run_index)?;
        row.patch_stats = Some(score_patch(
            &patch_text,
            &classifiers,
            gold_patches.get(&row.instance_id).map(String::as_str),
        ));
    }
    Ok(())
}

fn load_gold_patches(dataset_path: Option<&Path>) -> Result<HashMap<String, String>, Error> {
    let Some(path) = dataset_path else {
        return Ok(HashMap::new());
    };
    let mut out = HashMap::new();
    for row in swebench::load_dataset(path)? {
        if let Some(patch) = row.other.get("patch").and_then(serde_json::Value::as_str) {
            out.insert(row.instance_id, patch.to_owned());
        }
    }
    Ok(out)
}

fn patch_stats_run_index(instance_id: &str, resolved_by_run: &HashMap<RunSlotKey, bool>) -> u32 {
    resolved_by_run
        .iter()
        .filter(|(key, resolved)| key.instance_id == instance_id && **resolved)
        .map(|(key, _)| key.run_index)
        .min()
        .unwrap_or(1)
}

fn read_patch_or_empty(
    sweep_dir: &Path,
    instance_id: &str,
    run_index: u32,
) -> Result<String, Error> {
    let path = existing_patch_path_for_run(sweep_dir, instance_id, run_index);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(Error::Io(err)),
    }
}

fn existing_patch_path_for_run(sweep_dir: &Path, instance_id: &str, run_index: u32) -> PathBuf {
    let nested = swebench::patch_path_for_run(sweep_dir, instance_id, run_index);
    if nested.exists() || run_index != 1 {
        return nested;
    }
    sweep_dir.join(format!("{instance_id}.patch"))
}

#[must_use]
pub fn summarize<S: std::hash::BuildHasher>(
    eval: &EvaluationResults,
    results: &HashMap<String, InstanceResult, S>,
) -> EvaluationSummary {
    summarize_with_model(eval, results, None)
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn summarize_with_model<S: std::hash::BuildHasher>(
    eval: &EvaluationResults,
    results: &HashMap<String, InstanceResult, S>,
    model_name: Option<&str>,
) -> EvaluationSummary {
    let instances = eval.instances.len();
    if instances == 0 {
        return EvaluationSummary {
            instances: 0,
            resolved: 0,
            resolved_rate: 0.0,
            pass_at_1: 0.0,
            pass_at_k: 0.0,
            total_input_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            total_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            cost_per_resolved_usd: f64::NAN,
            cost_per_resolved_ci95_lower: f64::NAN,
            cost_per_resolved_ci95_upper: f64::NAN,
            budget_exhausted_excluded: 0,
        };
    }
    let tokens = results
        .values()
        .fold(TokenBreakdown::default(), |mut total, row| {
            let row_tokens = row.token_breakdown();
            total.input_tokens = total.input_tokens.saturating_add(row_tokens.input_tokens);
            total.cache_read_tokens = total
                .cache_read_tokens
                .saturating_add(row_tokens.cache_read_tokens);
            total.cache_creation_tokens = total
                .cache_creation_tokens
                .saturating_add(row_tokens.cache_creation_tokens);
            total.completion_tokens = total
                .completion_tokens
                .saturating_add(row_tokens.completion_tokens);
            total
        });
    let total_cost_usd = results
        .values()
        .filter_map(|row| row.effective_cost_usd(model_name))
        .sum();
    let resolved = eval.instances.iter().filter(|row| row.resolved).count();
    let pass_at_1 = eval
        .instances
        .iter()
        .filter(|row| {
            if row.runs > 1 || row.resolved_count > 0 || row.pass_at_1 {
                return row.pass_at_1;
            }
            row.resolved
                && results
                    .get(&row.instance_id)
                    .is_none_or(|result| effective_runs(result) == 1 || result.pass_at_1)
        })
        .count();
    let pass_at_k = eval.instances.iter().filter(|row| row.resolved).count();

    // Exclude budget-exhausted instances from cost_per_resolved_usd so we
    // never silently mix capped and uncapped runs in the efficiency metric.
    let mut budget_exhausted_excluded = 0usize;
    let mut per_instance_samples: Vec<(f64, bool)> = Vec::with_capacity(eval.instances.len());
    for row in &eval.instances {
        let result = results.get(&row.instance_id);
        if result.and_then(|r| r.failure_category) == Some(FailureCategory::BudgetExhausted) {
            budget_exhausted_excluded += 1;
        } else {
            let cost = result
                .and_then(|r| r.effective_cost_usd(model_name))
                .unwrap_or(0.0);
            per_instance_samples.push((cost, row.resolved));
        }
    }

    let br_resolved = per_instance_samples.iter().filter(|(_, r)| *r).count();
    let cost_per_resolved_usd = if br_resolved == 0 {
        f64::NAN
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            per_instance_samples.iter().map(|(c, _)| c).sum::<f64>() / br_resolved as f64
        }
    };
    let (cost_per_resolved_ci95_lower, cost_per_resolved_ci95_upper) =
        bootstrap_cost_per_resolved_ci95(&per_instance_samples);

    EvaluationSummary {
        instances,
        resolved,
        resolved_rate: pct(resolved, instances),
        pass_at_1: pct(pass_at_1, instances),
        pass_at_k: pct(pass_at_k, instances),
        total_input_tokens: tokens.input_tokens,
        total_cache_read_tokens: tokens.cache_read_tokens,
        total_cache_creation_tokens: tokens.cache_creation_tokens,
        total_completion_tokens: tokens.completion_tokens,
        total_cost_usd,
        cache_hit_rate: tokens.cache_hit_rate(),
        cost_per_resolved_usd,
        cost_per_resolved_ci95_lower,
        cost_per_resolved_ci95_upper,
        budget_exhausted_excluded,
    }
}

fn lcg_next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

/// 95% percentile bootstrap CI for `cost_per_resolved_usd`.
///
/// Returns `(f64::NAN, f64::NAN)` when no resolved instances exist in any
/// resample (i.e., the true resolved count is 0).
fn bootstrap_cost_per_resolved_ci95(samples: &[(f64, bool)]) -> (f64, f64) {
    const N_BOOTSTRAP: usize = 1000;
    const SEED: u64 = 12_345_678_901_234;
    let n = samples.len();
    if n == 0 {
        return (f64::NAN, f64::NAN);
    }
    let resolved_count = samples.iter().filter(|(_, r)| *r).count();
    if resolved_count == 0 {
        return (f64::NAN, f64::NAN);
    }
    let mut rng = SEED;
    let mut estimates: Vec<f64> = Vec::with_capacity(N_BOOTSTRAP);
    for _ in 0..N_BOOTSTRAP {
        let mut total_cost = 0.0_f64;
        let mut resolved = 0usize;
        for _ in 0..n {
            #[allow(clippy::cast_possible_truncation)]
            let idx = (lcg_next(&mut rng) as usize) % n;
            let (cost, is_resolved) = samples[idx];
            total_cost += cost;
            if is_resolved {
                resolved += 1;
            }
        }
        if resolved > 0 {
            #[allow(clippy::cast_precision_loss)]
            estimates.push(total_cost / resolved as f64);
        }
    }
    if estimates.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    estimates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let lower_idx = ((estimates.len() as f64) * 0.025) as usize;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let upper_idx = ((estimates.len() as f64) * 0.975) as usize;
    let lower = estimates[lower_idx];
    let upper = estimates[upper_idx.min(estimates.len() - 1)];
    (lower, upper)
}

fn build_none_eval(results: &HashMap<String, InstanceResult>) -> EvaluationResults {
    let mut instances: Vec<InstanceEvaluation> = results
        .iter()
        .map(|(id, r)| none_eval_for_result(id, r))
        .collect();
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    EvaluationResults {
        instances,
        behavioral: BehavioralMetrics::default(),
        breakdown: Vec::new(),
        cost_attribution: Vec::new(),
        model_mix_summary: Vec::new(),
        provenance: None,
    }
}

fn none_eval_for_result(id: &str, r: &InstanceResult) -> InstanceEvaluation {
    let skipped = r.outcome.as_deref() != Some(outcome::SUBMITTED) || !r.patch_present;
    InstanceEvaluation {
        instance_id: id.to_owned(),
        resolved: false,
        runs: effective_runs(r),
        resolved_count: 0,
        pass_at_1: false,
        tests_passed: vec![],
        tests_failed: vec![],
        eval_exit_reason: if skipped {
            EvalExitReason::SkippedNoPatch
        } else {
            EvalExitReason::EvalError
        },
        eval_log_path: None,
        patch_stats: None,
    }
}

fn run_sb_cli(
    args: &EvaluateArgs,
    results: &HashMap<String, InstanceResult>,
) -> Result<EvaluateRunOutput, Error> {
    let preds = swebench::predictions_path(&args.sweep_dir);
    if !preds.exists() {
        return Err(Error::Trajectory(format!(
            "bench evaluate: missing predictions file at {}",
            preds.display()
        )));
    }

    let report_dir = args.sweep_dir.join("sb_cli_reports");
    std::fs::create_dir_all(&report_dir)?;
    let run_id = args.run_id.clone().unwrap_or_else(generated_run_id);

    let max_runs = results.values().map(effective_runs).max().unwrap_or(1);
    let mut resolved_by_run = HashMap::new();
    if max_runs > 1 {
        let mut reports = BTreeMap::new();
        for run_index in 1..=max_runs {
            let run_preds = swebench::predictions_path_for_run(&args.sweep_dir, run_index);
            if !predictions_file_has_rows(&run_preds)? {
                reports.insert(run_index, HashMap::new());
                continue;
            }
            let run_report_id = format!("{run_id}-run-{run_index}");
            let parsed = submit_sb_cli_predictions(args, &run_preds, &report_dir, &run_report_id)?;
            resolved_by_run.extend(
                parsed.iter().map(|(instance_id, row)| {
                    (RunSlotKey::new(instance_id, run_index), row.resolved)
                }),
            );
            reports.insert(run_index, parsed);
        }
        return Ok(EvaluateRunOutput {
            eval: merge_rerun_reports_with_results(results, &reports),
            resolved_by_run,
            effective_run_id: Some(run_id),
        });
    }

    let parsed = submit_sb_cli_predictions(args, &preds, &report_dir, &run_id)?;
    resolved_by_run.extend(
        parsed
            .iter()
            .map(|(instance_id, row)| (RunSlotKey::new(instance_id, 1), row.resolved)),
    );
    Ok(EvaluateRunOutput {
        eval: merge_with_results(results, &parsed),
        resolved_by_run,
        effective_run_id: Some(run_id),
    })
}

fn predictions_file_has_rows(path: &Path) -> Result<bool, Error> {
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(text.lines().any(|line| !line.trim().is_empty()))
}

fn submit_sb_cli_predictions(
    args: &EvaluateArgs,
    preds: &Path,
    report_dir: &Path,
    run_id: &str,
) -> Result<HashMap<String, InstanceEvaluation>, Error> {
    let mut cmd = Command::new("sb-cli");
    cmd.arg("submit")
        .arg(&args.sb_subset)
        .arg(&args.sb_split)
        .arg("--predictions_path")
        .arg(preds)
        .arg("--run_id")
        .arg(run_id)
        .arg("--output_dir")
        .arg(report_dir)
        .arg("--wait_for_evaluation")
        .arg("1")
        .arg("--gen_report")
        .arg("1")
        .arg("--timeout-per-instance")
        .arg(args.timeout_per_instance_secs.to_string())
        .arg("--parallel")
        .arg(args.parallel.to_string());
    if let Some(dataset) = &args.dataset_path {
        cmd.arg("--dataset").arg(dataset);
    }

    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Trajectory(
                "bench evaluate: `sb-cli` not found on PATH; install it or run --backend none"
                    .into(),
            )
        } else {
            Error::Io(e)
        }
    })?;

    if !output.status.success() {
        return Err(Error::Trajectory(format!(
            "bench evaluate: sb-cli submit failed (status={}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let report_path = report_dir.join(format!(
        "{}__{}__{}.json",
        args.sb_subset, args.sb_split, run_id
    ));

    if !report_path.exists() {
        let output = Command::new("sb-cli")
            .arg("get-report")
            .arg(&args.sb_subset)
            .arg(&args.sb_split)
            .arg(run_id)
            .arg("--output_dir")
            .arg(report_dir)
            .arg("--overwrite")
            .arg("1")
            .output()?;
        if !output.status.success() {
            return Err(Error::Trajectory(format!(
                "bench evaluate: sb-cli get-report failed (status={}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }

    parse_sb_cli_results(&report_path)
}

fn parse_sb_cli_results(path: &Path) -> Result<HashMap<String, InstanceEvaluation>, Error> {
    let text = std::fs::read_to_string(path)?;
    if let Ok(eval) = serde_json::from_str::<EvaluationResults>(&text) {
        return Ok(eval
            .instances
            .into_iter()
            .map(|x| (x.instance_id.clone(), x))
            .collect());
    }

    let value: serde_json::Value = serde_json::from_str(&text)?;
    let mut map = HashMap::new();
    match value {
        serde_json::Value::Array(rows) => {
            for row in rows {
                if let Some(eval) = parse_generic_eval_row(&row) {
                    map.insert(eval.instance_id.clone(), eval);
                }
            }
        }
        serde_json::Value::Object(obj) => {
            if let Some(rows) = obj.get("instances").and_then(serde_json::Value::as_array) {
                for row in rows {
                    if let Some(eval) = parse_generic_eval_row(row) {
                        map.insert(eval.instance_id.clone(), eval);
                    }
                }
            }
            // sb-cli report shape: `resolved_ids` and sometimes `submitted_ids`.
            if map.is_empty() {
                let resolved_ids = obj
                    .get("resolved_ids")
                    .and_then(serde_json::Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let submitted_ids = obj
                    .get("submitted_ids")
                    .and_then(serde_json::Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                for id in &submitted_ids {
                    map.insert(
                        id.clone(),
                        InstanceEvaluation {
                            instance_id: id.clone(),
                            resolved: resolved_ids.contains(id),
                            runs: 0,
                            resolved_count: 0,
                            pass_at_1: false,
                            tests_passed: vec![],
                            tests_failed: vec![],
                            eval_exit_reason: if resolved_ids.contains(id) {
                                EvalExitReason::Resolved
                            } else {
                                EvalExitReason::Unresolved
                            },
                            eval_log_path: None,
                            patch_stats: None,
                        },
                    );
                }
                if submitted_ids.is_empty() {
                    for id in resolved_ids {
                        map.insert(
                            id.clone(),
                            InstanceEvaluation {
                                instance_id: id,
                                resolved: true,
                                runs: 0,
                                resolved_count: 0,
                                pass_at_1: false,
                                tests_passed: vec![],
                                tests_failed: vec![],
                                eval_exit_reason: EvalExitReason::Resolved,
                                eval_log_path: None,
                                patch_stats: None,
                            },
                        );
                    }
                }
            }
        }
        _ => {}
    }
    Ok(map)
}

fn generated_run_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("rust_swe_agent_{secs}")
}

fn parse_generic_eval_row(v: &serde_json::Value) -> Option<InstanceEvaluation> {
    let id = v.get("instance_id")?.as_str()?.to_owned();
    let resolved = v
        .get("resolved")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let tests_passed = as_string_vec(v.get("tests_passed"));
    let tests_failed = as_string_vec(v.get("tests_failed"));
    let eval_exit_reason = match v
        .get("eval_exit_reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
    {
        "resolved" => EvalExitReason::Resolved,
        "unresolved" => EvalExitReason::Unresolved,
        "patch_apply_failed" => EvalExitReason::PatchApplyFailed,
        "eval_error" => EvalExitReason::EvalError,
        "skipped_no_patch" => EvalExitReason::SkippedNoPatch,
        _ => {
            if resolved {
                EvalExitReason::Resolved
            } else {
                EvalExitReason::Unresolved
            }
        }
    };
    let eval_log_path = v
        .get("eval_log_path")
        .or_else(|| v.get("log_path"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    Some(InstanceEvaluation {
        instance_id: id,
        resolved,
        runs: 0,
        resolved_count: 0,
        pass_at_1: false,
        tests_passed,
        tests_failed,
        eval_exit_reason,
        eval_log_path,
        patch_stats: None,
    })
}

fn as_string_vec(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn merge_with_results(
    results: &HashMap<String, InstanceResult>,
    parsed: &HashMap<String, InstanceEvaluation>,
) -> EvaluationResults {
    let mut instances: Vec<InstanceEvaluation> = Vec::with_capacity(results.len());
    for (id, r) in results {
        if let Some(row) = parsed.get(id) {
            let mut row = row.clone();
            normalize_single_run_metrics(&mut row, effective_runs(r));
            instances.push(row);
            continue;
        }
        let skipped = r.outcome.as_deref() != Some(outcome::SUBMITTED) || !r.patch_present;
        instances.push(InstanceEvaluation {
            instance_id: id.clone(),
            resolved: false,
            runs: effective_runs(r),
            resolved_count: 0,
            pass_at_1: false,
            tests_passed: vec![],
            tests_failed: vec![],
            eval_exit_reason: if skipped {
                EvalExitReason::SkippedNoPatch
            } else {
                EvalExitReason::EvalError
            },
            eval_log_path: None,
            patch_stats: None,
        });
    }
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    EvaluationResults {
        instances,
        behavioral: BehavioralMetrics::default(),
        breakdown: Vec::new(),
        cost_attribution: Vec::new(),
        model_mix_summary: Vec::new(),
        provenance: None,
    }
}

fn build_behavioral_metrics(
    evals: &[InstanceEvaluation],
    results: &HashMap<String, InstanceResult>,
) -> BehavioralMetrics {
    let resolved_by_id: HashMap<&str, bool> = evals
        .iter()
        .map(|row| (row.instance_id.as_str(), row.resolved))
        .collect();
    let mut submitted = 0usize;
    let mut tests_run = 0usize;
    let mut resolved_with_tests = 0usize;
    let mut tests_skipped = 0usize;
    let mut resolved_tests_skipped = 0usize;

    for row in results
        .values()
        .filter(|row| row.outcome.as_deref() == Some(outcome::SUBMITTED))
    {
        submitted += 1;
        let resolved = resolved_by_id
            .get(row.instance_id.as_str())
            .copied()
            .unwrap_or(false);
        if row.tests_run_before_submit {
            tests_run += 1;
            if resolved {
                resolved_with_tests += 1;
            }
        } else {
            tests_skipped += 1;
            if resolved {
                resolved_tests_skipped += 1;
            }
        }
    }

    BehavioralMetrics {
        tests_run_before_submit_rate: pct(tests_run, submitted),
        resolved_rate_when_tests_run: pct(resolved_with_tests, tests_run),
        resolved_rate_when_tests_skipped: pct(resolved_tests_skipped, tests_skipped),
    }
}

fn normalize_single_run_metrics(row: &mut InstanceEvaluation, runs: u32) {
    if row.runs == 0 {
        row.runs = runs;
    }
    if row.resolved_count == 0 && row.resolved {
        row.resolved_count = 1.min(row.runs);
    }
    if row.runs <= 1 {
        row.pass_at_1 = row.resolved;
    }
}

fn merge_rerun_reports_with_results(
    results: &HashMap<String, InstanceResult>,
    run_reports: &BTreeMap<u32, HashMap<String, InstanceEvaluation>>,
) -> EvaluationResults {
    let mut instances = Vec::with_capacity(results.len());
    for (id, result) in results {
        let runs = effective_runs(result);
        let mut resolved_count = 0u32;
        let mut pass_at_1 = false;
        let mut representative: Option<InstanceEvaluation> = None;

        for run_index in 1..=runs {
            let Some(row) = run_reports
                .get(&run_index)
                .and_then(|report| report.get(id))
            else {
                continue;
            };
            if representative.is_none() || row.resolved {
                representative = Some(row.clone());
            }
            if row.resolved {
                resolved_count = resolved_count.saturating_add(1);
                if run_index == 1 {
                    pass_at_1 = true;
                }
            }
        }

        let resolved = resolved_count > 0;
        let mut row = representative.unwrap_or_else(|| missing_eval_for_result(id, result));
        row.instance_id.clone_from(id);
        row.resolved = resolved;
        row.runs = runs;
        row.resolved_count = resolved_count;
        row.pass_at_1 = pass_at_1;
        row.eval_exit_reason = if resolved {
            EvalExitReason::Resolved
        } else {
            row.eval_exit_reason
        };
        instances.push(row);
    }
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    EvaluationResults {
        instances,
        behavioral: BehavioralMetrics::default(),
        breakdown: Vec::new(),
        cost_attribution: Vec::new(),
        model_mix_summary: Vec::new(),
        provenance: None,
    }
}

fn missing_eval_for_result(id: &str, result: &InstanceResult) -> InstanceEvaluation {
    let skipped = result.outcome.as_deref() != Some(outcome::SUBMITTED) || !result.patch_present;
    InstanceEvaluation {
        instance_id: id.to_owned(),
        resolved: false,
        runs: effective_runs(result),
        resolved_count: 0,
        pass_at_1: false,
        tests_passed: vec![],
        tests_failed: vec![],
        eval_exit_reason: if skipped {
            EvalExitReason::SkippedNoPatch
        } else {
            EvalExitReason::EvalError
        },
        eval_log_path: None,
        patch_stats: None,
    }
}

/// Build the model-mix summary from per-run-slot data so that reruns using
/// different fallback models are all counted. Each slot's `final_model` is
/// counted independently; its resolution comes from the per-slot key in
/// `resolved_by_run` (falls back to `false` when no evaluation result exists).
fn build_model_mix_summary_from_slots(
    slots: &[crate::run::compare::LoadedRunSlot],
    resolved_by_run: &HashMap<RunSlotKey, bool>,
) -> Vec<ModelMixBucket> {
    let mut by_model: BTreeMap<String, (usize, usize, f64)> = BTreeMap::new();
    for slot in slots {
        let Some(model) = slot.result.final_model.as_deref() else {
            continue;
        };
        let resolved = resolved_by_run
            .get(&RunSlotKey::new(&slot.instance_id, slot.run_index))
            .copied()
            .unwrap_or(false);
        let (n, res, cost) = by_model.entry(model.to_owned()).or_default();
        *n += 1;
        if resolved {
            *res += 1;
        }
        *cost += slot.result.cost_usd.unwrap_or(0.0);
    }
    if by_model.is_empty() {
        return Vec::new();
    }
    by_model
        .into_iter()
        .map(|(model, (n, resolved, total_cost))| {
            #[allow(clippy::cast_precision_loss)]
            let resolved_rate = if n == 0 {
                0.0
            } else {
                resolved as f64 / n as f64
            };
            ModelMixBucket {
                model,
                n,
                resolved,
                resolved_rate,
                total_cost_usd: if total_cost > 0.0 {
                    Some(total_cost)
                } else {
                    None
                },
            }
        })
        .collect()
}

fn build_breakdown(
    evals: &[InstanceEvaluation],
    results: &HashMap<String, InstanceResult>,
    selection: &BreakdownSelection,
    model_name: Option<&str>,
) -> Vec<BreakdownBucket> {
    let mut out = Vec::with_capacity(selection.axes.len());
    for axis in &selection.axes {
        // (n, resolved, budget_respecting_cost, budget_respecting_resolved)
        let mut buckets: BTreeMap<String, (usize, usize, f64, usize)> = BTreeMap::new();
        let mut unknown_repo = 0usize;
        for row in evals {
            let bucket_value = match axis {
                BreakdownAxis::Repo => {
                    if let Some(repo) = parse_repo_from_instance_id(&row.instance_id) {
                        repo
                    } else {
                        unknown_repo += 1;
                        "unknown".to_owned()
                    }
                }
                BreakdownAxis::FailureCategory => {
                    if row.resolved {
                        "resolved".to_owned()
                    } else {
                        results
                            .get(&row.instance_id)
                            .and_then(|r| r.failure_category)
                            .map_or("none", failure_label)
                            .to_owned()
                    }
                }
            };
            let is_budget_exhausted = results
                .get(&row.instance_id)
                .and_then(|r| r.failure_category)
                == Some(FailureCategory::BudgetExhausted);
            let entry = buckets.entry(bucket_value).or_insert((0, 0, 0.0, 0));
            entry.0 += 1;
            if row.resolved {
                entry.1 += 1;
            }
            if !is_budget_exhausted {
                let cost = results
                    .get(&row.instance_id)
                    .and_then(|r| r.effective_cost_usd(model_name))
                    .unwrap_or(0.0);
                entry.2 += cost;
                if row.resolved {
                    entry.3 += 1;
                }
            }
        }
        if matches!(axis, BreakdownAxis::Repo) && unknown_repo > 0 {
            tracing::warn!(
                unknown_repo_instances = unknown_repo,
                "bench evaluate: repo parse failed for some ids; using repo=unknown"
            );
        }
        let mut rows: Vec<BreakdownBucket> = buckets
            .into_iter()
            .map(|(bucket_value, (n, resolved, br_cost, br_resolved))| {
                #[allow(clippy::cast_precision_loss)]
                let cost_per_resolved_usd = if br_resolved == 0 {
                    None
                } else {
                    Some(br_cost / br_resolved as f64)
                };
                BreakdownBucket {
                    bucket_axis: *axis,
                    bucket_value,
                    n,
                    resolved,
                    resolved_rate: pct(resolved, n),
                    cost_per_resolved_usd,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.n.cmp(&a.n)
                .then_with(|| a.bucket_value.cmp(&b.bucket_value))
        });
        out.extend(rows);
    }
    out
}

#[must_use]
pub fn parse_repo_from_instance_id(instance_id: &str) -> Option<String> {
    let (owner, rest) = instance_id.split_once("__")?;
    let (repo, _) = rest.rsplit_once('-')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

#[must_use]
pub fn failure_label(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::BudgetExhausted => "budget_exhausted",
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::Unknown => "unknown",
    }
}

#[must_use]
pub fn cost_attribution_bucket_label(
    resolved: bool,
    failure_category: Option<FailureCategory>,
) -> &'static str {
    if resolved {
        COST_ATTRIBUTION_RESOLVED_BUCKET
    } else {
        failure_category.map_or(COST_ATTRIBUTION_UNCATEGORIZED_BUCKET, failure_label)
    }
}

#[must_use]
pub fn pct(numer: usize, denom: usize) -> f64 {
    if denom == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    {
        numer as f64 / denom as f64
    }
}

#[must_use]
pub fn round_dp(value: f64, places: u32) -> f64 {
    let factor = 10_f64.powf(f64::from(places));
    (value * factor).round() / factor
}

#[must_use]
pub fn cost_missing_count<S: std::hash::BuildHasher>(
    results: &HashMap<String, InstanceResult, S>,
) -> usize {
    results
        .values()
        .filter(|row| row.cost_usd.is_none())
        .count()
}

pub(crate) fn cost_missing_count_for_run_slots<S: std::hash::BuildHasher>(
    sweep_dir: &Path,
    results: &HashMap<String, InstanceResult, S>,
) -> Result<usize, Error> {
    Ok(load_run_slots(sweep_dir, results)?
        .into_iter()
        .filter(|slot| slot.result.cost_usd.is_none())
        .count())
}

#[cfg(test)]
fn build_cost_attribution<S: std::hash::BuildHasher>(
    evals: &[InstanceEvaluation],
    results: &HashMap<String, InstanceResult, S>,
    model_name: Option<&str>,
) -> CostAttributionReport {
    build_cost_attribution_report(evals.iter().map(|row| {
        let result = results.get(&row.instance_id);
        CostAttributionSample {
            resolved: row.resolved,
            failure_category: result.and_then(|result| result.failure_category),
            cost_usd: result.and_then(|result| result.effective_cost_usd(model_name)),
        }
    }))
}

fn build_cost_attribution_from_run_slots(
    run_slots: &[crate::run::compare::LoadedRunSlot],
    resolved_by_run: &HashMap<RunSlotKey, bool>,
    model_name: Option<&str>,
) -> CostAttributionReport {
    build_cost_attribution_report(run_slots.iter().map(|slot| {
        CostAttributionSample {
            resolved: resolved_by_run
                .get(&RunSlotKey::new(&slot.instance_id, slot.run_index))
                .copied()
                .unwrap_or(false),
            failure_category: slot.result.failure_category,
            cost_usd: slot.result.effective_cost_usd(model_name),
        }
    }))
}

fn build_cost_attribution_report(
    samples: impl IntoIterator<Item = CostAttributionSample>,
) -> CostAttributionReport {
    let mut buckets: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for category in ALL_FAILURE_CATEGORIES {
        buckets.insert(failure_label(category).to_owned(), (0, 0.0));
    }
    buckets.insert(COST_ATTRIBUTION_RESOLVED_BUCKET.to_owned(), (0, 0.0));
    buckets.insert(COST_ATTRIBUTION_UNCATEGORIZED_BUCKET.to_owned(), (0, 0.0));

    let mut total_n = 0usize;
    let mut total_usd = 0.0;
    let mut missing_cost_count = 0usize;

    for sample in samples {
        total_n += 1;
        let bucket = cost_attribution_bucket_label(sample.resolved, sample.failure_category);
        let usd_cost = if let Some(cost) = sample.cost_usd {
            cost
        } else {
            missing_cost_count += 1;
            0.0
        };
        total_usd += usd_cost;
        let entry = buckets.entry(bucket.to_owned()).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += usd_cost;
    }

    let mut rows: Vec<CostAttributionBucket> = buckets
        .into_iter()
        .map(|(bucket, (n, total))| CostAttributionBucket {
            bucket,
            n,
            total_usd: round_dp(total, 4),
            mean_usd: if n == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                {
                    round_dp(total / n as f64, 4)
                }
            },
            share_pct: if total_usd == 0.0 {
                0.0
            } else {
                round_dp(total * 100.0 / total_usd, 2)
            },
        })
        .collect();
    rows.sort_by(|a, b| {
        b.total_usd
            .partial_cmp(&a.total_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.bucket.cmp(&b.bucket))
    });
    rows.push(CostAttributionBucket {
        bucket: COST_ATTRIBUTION_TOTAL_BUCKET.to_owned(),
        n: total_n,
        total_usd: round_dp(total_usd, 4),
        mean_usd: if total_n == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                round_dp(total_usd / total_n as f64, 4)
            }
        },
        share_pct: if total_n == 0 { 0.0 } else { 100.0 },
    });
    CostAttributionReport {
        rows,
        missing_cost_count,
    }
}

#[must_use]
pub fn render_breakdown_table(rows: &[BreakdownBucket]) -> String {
    let mut out = String::from("axis,bucket,n,resolved,resolved_rate,cost_per_resolved_usd\n");
    for row in rows {
        let axis = match row.bucket_axis {
            BreakdownAxis::Repo => "repo",
            BreakdownAxis::FailureCategory => "failure_category",
        };
        let cpr = match row.cost_per_resolved_usd {
            Some(v) => format!("{v:.4}"),
            None => "NaN".to_owned(),
        };
        let _ = writeln!(
            out,
            "{axis},{},{},{},{:.4},{cpr}",
            row.bucket_value, row.n, row.resolved, row.resolved_rate
        );
    }
    out
}

#[must_use]
pub fn render_cost_attribution_table(rows: &[CostAttributionBucket]) -> String {
    let mut out = String::from("bucket,n,total_usd,mean_usd,share_pct\n");
    for row in rows {
        let _ = writeln!(
            out,
            "{},{},{:.4},{:.4},{:.2}",
            row.bucket, row.n, row.total_usd, row.mean_usd, row.share_pct
        );
    }
    out
}

#[must_use]
pub fn render_summary_table(summary: &EvaluationSummary) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "resolved: {}", summary.resolved);
    let _ = writeln!(out, "resolved_rate: {:.4}", summary.resolved_rate);
    let _ = writeln!(out, "pass@1: {:.4}", summary.pass_at_1);
    let _ = writeln!(out, "pass@k: {:.4}", summary.pass_at_k);
    let _ = writeln!(out, "input_tokens: {}", summary.total_input_tokens);
    let _ = writeln!(
        out,
        "cache_read_tokens: {}",
        summary.total_cache_read_tokens
    );
    let _ = writeln!(
        out,
        "cache_creation_tokens: {}",
        summary.total_cache_creation_tokens
    );
    let _ = writeln!(
        out,
        "completion_tokens: {}",
        summary.total_completion_tokens
    );
    let _ = writeln!(out, "cache_hit_rate: {:.4}", summary.cache_hit_rate);
    let _ = writeln!(out, "total_cost_usd: {:.4}", summary.total_cost_usd);
    let _ = writeln!(
        out,
        "cost_per_resolved_usd: {:.4}",
        summary.cost_per_resolved_usd
    );
    if !summary.cost_per_resolved_ci95_lower.is_nan() {
        let _ = writeln!(
            out,
            "cost_per_resolved_ci95: [{:.4}, {:.4}]",
            summary.cost_per_resolved_ci95_lower, summary.cost_per_resolved_ci95_upper
        );
    }
    if summary.budget_exhausted_excluded > 0 {
        let _ = writeln!(
            out,
            "budget_exhausted_excluded: {} (cost_per_resolved computed on budget-respecting runs only)",
            summary.budget_exhausted_excluded
        );
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::trajectory::FailureCategory;

    fn assert_f64_eq(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn submitted(id: &str) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: None,
            cost_usd: None,
            prompt_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            duration_secs: None,
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,

            fallback_count: None,

            final_model: None,
        }
    }

    fn errored(id: &str) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "error".into(),
            outcome: Some(outcome::ERROR.into()),
            failure_category: Some(FailureCategory::Unknown),
            steps: None,
            cost_usd: None,
            prompt_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            duration_secs: None,
            error: Some("boom".into()),
            github_pr_error: None,
            patch_present: false,
            non_empty_patch: false,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,

            fallback_count: None,

            final_model: None,
        }
    }

    fn eval_row(id: &str, resolved: bool) -> InstanceEvaluation {
        InstanceEvaluation {
            instance_id: id.into(),
            resolved,
            runs: 1,
            resolved_count: u32::from(resolved),
            pass_at_1: resolved,
            tests_passed: vec![],
            tests_failed: vec![],
            eval_exit_reason: if resolved {
                EvalExitReason::Resolved
            } else {
                EvalExitReason::Unresolved
            },
            eval_log_path: None,
            patch_stats: None,
        }
    }

    fn submitted_with_tests(id: &str, tests_run: bool) -> InstanceResult {
        let mut r = submitted(id);
        r.tests_run_before_submit = tests_run;
        r.last_tests_passed = tests_run.then_some(true);
        r
    }

    #[test]
    fn none_backend_marks_non_submitted_as_skipped_no_patch() {
        let map = HashMap::from([
            ("a".to_string(), submitted("a")),
            ("b".to_string(), errored("b")),
        ]);
        let eval = build_none_eval(&map);
        assert_eq!(eval.instances.len(), 2);
        assert_eq!(eval.instances[0].instance_id, "a");
        assert!(matches!(
            eval.instances[0].eval_exit_reason,
            EvalExitReason::EvalError
        ));
        assert_eq!(eval.instances[1].instance_id, "b");
        assert!(matches!(
            eval.instances[1].eval_exit_reason,
            EvalExitReason::SkippedNoPatch
        ));
    }

    #[test]
    fn behavioral_metrics_split_resolved_rate_by_test_behavior() {
        let eval = EvaluationResults {
            instances: vec![
                eval_row("tested-pass", true),
                eval_row("tested-fail", false),
                eval_row("skipped-fail", false),
            ],
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
            model_mix_summary: Vec::new(),
            provenance: None,
            behavioral: BehavioralMetrics::default(),
        };
        let results = HashMap::from([
            (
                "tested-pass".to_string(),
                submitted_with_tests("tested-pass", true),
            ),
            (
                "tested-fail".to_string(),
                submitted_with_tests("tested-fail", true),
            ),
            (
                "skipped-fail".to_string(),
                submitted_with_tests("skipped-fail", false),
            ),
        ]);

        let behavioral = build_behavioral_metrics(&eval.instances, &results);
        assert_f64_eq(behavioral.tests_run_before_submit_rate, 2.0 / 3.0);
        assert_f64_eq(behavioral.resolved_rate_when_tests_run, 0.5);
        assert_f64_eq(behavioral.resolved_rate_when_tests_skipped, 0.0);
    }

    #[test]
    fn parses_sb_cli_report_with_resolved_and_submitted_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        let report = serde_json::json!({
            "resolved_ids": ["inst-a"],
            "submitted_ids": ["inst-a", "inst-b"]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        let parsed = parse_sb_cli_results(&path).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.get("inst-a").map(|r| r.resolved), Some(true));
        assert_eq!(parsed.get("inst-b").map(|r| r.resolved), Some(false));
    }

    #[test]
    fn aggregate_rerun_evaluations_preserves_pass_at_1_and_pass_at_k() {
        let mut result = submitted("inst-a");
        result.runs = 2;
        result.resolved_count = 0;
        result.pass_at_1 = false;
        let results = HashMap::from([("inst-a".to_string(), result)]);
        let run_reports = BTreeMap::from([
            (
                1,
                HashMap::from([(
                    "inst-a".to_string(),
                    InstanceEvaluation {
                        instance_id: "inst-a".into(),
                        resolved: false,
                        runs: 0,
                        resolved_count: 0,
                        pass_at_1: false,
                        tests_passed: vec![],
                        tests_failed: vec![],
                        eval_exit_reason: EvalExitReason::Unresolved,
                        eval_log_path: None,
                        patch_stats: None,
                    },
                )]),
            ),
            (
                2,
                HashMap::from([(
                    "inst-a".to_string(),
                    InstanceEvaluation {
                        instance_id: "inst-a".into(),
                        resolved: true,
                        runs: 0,
                        resolved_count: 0,
                        pass_at_1: false,
                        tests_passed: vec![],
                        tests_failed: vec![],
                        eval_exit_reason: EvalExitReason::Resolved,
                        eval_log_path: None,
                        patch_stats: None,
                    },
                )]),
            ),
        ]);

        let eval = merge_rerun_reports_with_results(&results, &run_reports);
        let row = &eval.instances[0];
        assert_eq!(row.instance_id, "inst-a");
        assert!(row.resolved);
        assert_eq!(row.runs, 2);
        assert_eq!(row.resolved_count, 1);
        assert!(!row.pass_at_1);

        let summary = summarize(&eval, &results);
        assert!((summary.pass_at_1 - 0.0).abs() < f64::EPSILON);
        assert!((summary.pass_at_k - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_repo_from_instance_id() {
        assert_eq!(
            parse_repo_from_instance_id("django__django-10087"),
            Some("django/django".into())
        );
        assert_eq!(parse_repo_from_instance_id("invalid"), None);
    }

    #[test]
    fn failure_category_breakdown_uses_none_for_missing_category() {
        let results = HashMap::from([("a".to_string(), submitted("a"))]);
        let evals = vec![InstanceEvaluation {
            instance_id: "a".into(),
            resolved: false,
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_passed: vec![],
            tests_failed: vec![],
            eval_exit_reason: EvalExitReason::Unresolved,
            eval_log_path: None,
            patch_stats: None,
        }];
        let rows = build_breakdown(
            &evals,
            &results,
            &BreakdownSelection {
                axes: vec![BreakdownAxis::FailureCategory],
            },
            None,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bucket_value, "none");
    }

    #[test]
    fn breakdown_bucket_cost_per_resolved_usd_per_slice() {
        let mut django_res = submitted("django__django-1");
        django_res.cost_usd = Some(3.0);
        let mut requests_unres = submitted("psf__requests-2");
        requests_unres.cost_usd = Some(2.0);
        let results = HashMap::from([
            ("django__django-1".to_string(), django_res),
            ("psf__requests-2".to_string(), requests_unres),
        ]);
        let evals = vec![
            eval_row("django__django-1", true),
            eval_row("psf__requests-2", false),
        ];
        let rows = build_breakdown(
            &evals,
            &results,
            &BreakdownSelection {
                axes: vec![BreakdownAxis::Repo],
            },
            None,
        );
        let django = rows
            .iter()
            .find(|r| r.bucket_value == "django/django")
            .unwrap();
        assert_eq!(django.cost_per_resolved_usd, Some(3.0));
        let requests = rows
            .iter()
            .find(|r| r.bucket_value == "psf/requests")
            .unwrap();
        assert_eq!(requests.cost_per_resolved_usd, None);
    }

    #[test]
    fn budget_exhausted_instances_excluded_from_cost_per_resolved() {
        let mut normal = submitted("a");
        normal.cost_usd = Some(2.0);
        let mut budgeted = submitted("b");
        budgeted.cost_usd = Some(5.0);
        budgeted.failure_category = Some(FailureCategory::BudgetExhausted);
        let results = HashMap::from([("a".to_string(), normal), ("b".to_string(), budgeted)]);
        let eval = EvaluationResults {
            instances: vec![eval_row("a", true), eval_row("b", false)],
            behavioral: BehavioralMetrics::default(),
            breakdown: vec![],
            cost_attribution: vec![],
            model_mix_summary: vec![],
            provenance: None,
        };
        let summary = summarize_with_model(&eval, &results, None);
        assert_eq!(summary.budget_exhausted_excluded, 1);
        // only "a" is budget-respecting and resolved: cost_per_resolved = $2.0 / 1
        assert!(
            (summary.cost_per_resolved_usd - 2.0).abs() < 1e-9,
            "expected cost_per_resolved_usd=2.0, got {}",
            summary.cost_per_resolved_usd
        );
    }

    #[test]
    fn cost_attribution_tracks_missing_costs_and_resolved_precedence() {
        let mut resolved = submitted("resolved");
        resolved.failure_category = Some(FailureCategory::StepLimit);
        resolved.cost_usd = None;

        let mut uncategorized = errored("uncategorized");
        uncategorized.failure_category = None;
        uncategorized.cost_usd = Some(1.0);

        let mut model_api = errored("model-api");
        model_api.failure_category = Some(FailureCategory::ModelApi);
        model_api.cost_usd = Some(3.0);

        let evals = vec![
            eval_row("resolved", true),
            eval_row("uncategorized", false),
            eval_row("model-api", false),
        ];
        let results = HashMap::from([
            ("resolved".to_string(), resolved),
            ("uncategorized".to_string(), uncategorized),
            ("model-api".to_string(), model_api),
        ]);

        let report = build_cost_attribution(&evals, &results, None);
        assert_eq!(report.missing_cost_count, 1);

        let rows = report
            .rows
            .iter()
            .map(|row| (row.bucket.as_str(), row))
            .collect::<HashMap<_, _>>();

        let resolved_row = rows.get("resolved").copied().unwrap();
        assert_eq!(resolved_row.n, 1);
        assert_f64_eq(resolved_row.total_usd, 0.0);
        assert_f64_eq(resolved_row.mean_usd, 0.0);
        assert_f64_eq(resolved_row.share_pct, 0.0);

        let uncategorized_row = rows.get("uncategorized").copied().unwrap();
        assert_eq!(uncategorized_row.n, 1);
        assert_f64_eq(uncategorized_row.total_usd, 1.0);
        assert_f64_eq(uncategorized_row.mean_usd, 1.0);
        assert_f64_eq(uncategorized_row.share_pct, 25.0);

        let model_api_row = rows.get("model_api").copied().unwrap();
        assert_eq!(model_api_row.n, 1);
        assert_f64_eq(model_api_row.total_usd, 3.0);
        assert_f64_eq(model_api_row.mean_usd, 3.0);
        assert_f64_eq(model_api_row.share_pct, 75.0);

        let total_row = report.rows.last().unwrap();
        assert_eq!(total_row.bucket, "TOTAL");
        assert_eq!(total_row.n, 3);
        assert_f64_eq(total_row.total_usd, 4.0);
        assert_f64_eq(total_row.mean_usd, 1.3333);
        assert_f64_eq(total_row.share_pct, 100.0);
    }

    #[test]
    fn cost_attribution_rows_sort_by_total_usd_then_bucket() {
        let mut env_setup = errored("env");
        env_setup.failure_category = Some(FailureCategory::EnvSetup);
        env_setup.cost_usd = Some(2.0);

        let mut model_api = errored("api");
        model_api.failure_category = Some(FailureCategory::ModelApi);
        model_api.cost_usd = Some(2.0);

        let mut uncategorized = errored("uncategorized");
        uncategorized.failure_category = None;
        uncategorized.cost_usd = Some(5.0);

        let evals = vec![
            eval_row("env", false),
            eval_row("api", false),
            eval_row("uncategorized", false),
        ];
        let results = HashMap::from([
            ("env".to_string(), env_setup),
            ("api".to_string(), model_api),
            ("uncategorized".to_string(), uncategorized),
        ]);

        let report = build_cost_attribution(&evals, &results, None);
        let top_buckets: Vec<&str> = report
            .rows
            .iter()
            .take(3)
            .map(|row| row.bucket.as_str())
            .collect();
        assert_eq!(top_buckets, vec!["uncategorized", "env_setup", "model_api"]);
    }

    #[test]
    fn summary_reports_cache_breakdown_and_rendered_table() {
        let mut cached = submitted("cached");
        cached.cost_usd = Some(0.42);
        cached.prompt_tokens = Some(100);
        cached.cache_read_tokens = Some(800);
        cached.cache_creation_tokens = Some(100);
        cached.completion_tokens = Some(50);

        let results = HashMap::from([("cached".to_string(), cached)]);
        let eval = EvaluationResults {
            instances: vec![eval_row("cached", true)],
            behavioral: BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
            model_mix_summary: Vec::new(),
            provenance: None,
        };
        let summary = summarize(&eval, &results);
        assert_eq!(summary.instances, 1);
        assert_eq!(summary.resolved, 1);
        assert_eq!(summary.total_input_tokens, 100);
        assert_eq!(summary.total_cache_read_tokens, 800);
        assert_eq!(summary.total_cache_creation_tokens, 100);
        assert_eq!(summary.total_completion_tokens, 50);
        assert!((summary.total_cost_usd - 0.42).abs() < f64::EPSILON);
        assert!((summary.cache_hit_rate - 0.8).abs() < f64::EPSILON);

        let rendered = render_summary_table(&summary);
        assert!(rendered.contains("input_tokens: 100"), "got:\n{rendered}");
        assert!(
            rendered.contains("cache_read_tokens: 800"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("cache_creation_tokens: 100"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("completion_tokens: 50"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("cache_hit_rate: 0.8000"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("total_cost_usd: 0.4200"),
            "got:\n{rendered}"
        );
    }

    // --- RED phase: cost_per_resolved_usd tests ---

    #[test]
    fn cost_per_resolved_usd_is_nan_when_resolved_count_is_zero() {
        let results: HashMap<String, InstanceResult> = HashMap::from([("a".to_string(), {
            let mut r = errored("a");
            r.cost_usd = Some(5.0);
            r
        })]);
        let eval = EvaluationResults {
            instances: vec![eval_row("a", false)],
            behavioral: BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
            model_mix_summary: Vec::new(),
            provenance: None,
        };
        let summary = summarize(&eval, &results);
        assert!(
            summary.cost_per_resolved_usd.is_nan(),
            "expected NaN when resolved=0, got {}",
            summary.cost_per_resolved_usd
        );
    }

    #[test]
    fn cost_per_resolved_usd_divides_total_cost_by_resolved_count() {
        let mut r1 = submitted("r1");
        r1.cost_usd = Some(3.0);
        let mut r2 = submitted("r2");
        r2.cost_usd = Some(1.0);
        let mut r3 = errored("r3");
        r3.cost_usd = Some(2.0);
        let results = HashMap::from([
            ("r1".to_string(), r1),
            ("r2".to_string(), r2),
            ("r3".to_string(), r3),
        ]);
        let eval = EvaluationResults {
            instances: vec![
                eval_row("r1", true),
                eval_row("r2", true),
                eval_row("r3", false),
            ],
            behavioral: BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
            model_mix_summary: Vec::new(),
            provenance: None,
        };
        let summary = summarize(&eval, &results);
        assert_eq!(summary.resolved, 2);
        assert_f64_eq(summary.total_cost_usd, 6.0);
        // cost_per_resolved_usd = 6.0 / 2 = 3.0
        assert_f64_eq(summary.cost_per_resolved_usd, 3.0);
    }

    #[test]
    fn cost_per_resolved_ci95_is_nan_when_nothing_resolved() {
        let results: HashMap<String, InstanceResult> = HashMap::from([("a".to_string(), {
            let mut r = errored("a");
            r.cost_usd = Some(5.0);
            r
        })]);
        let eval = EvaluationResults {
            instances: vec![eval_row("a", false)],
            behavioral: BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
            model_mix_summary: Vec::new(),
            provenance: None,
        };
        let summary = summarize(&eval, &results);
        assert!(
            summary.cost_per_resolved_ci95_lower.is_nan(),
            "expected NaN CI lower when resolved=0"
        );
        assert!(
            summary.cost_per_resolved_ci95_upper.is_nan(),
            "expected NaN CI upper when resolved=0"
        );
    }

    #[test]
    fn cost_per_resolved_ci95_brackets_true_value() {
        // 10 resolved at $1.00 each, 0 unresolved -> cost_per_resolved = 1.0
        let results: HashMap<String, InstanceResult> = (0..10)
            .map(|i| {
                let mut r = submitted(&format!("r{i}"));
                r.cost_usd = Some(1.0);
                (format!("r{i}"), r)
            })
            .collect();
        let eval = EvaluationResults {
            instances: (0..10).map(|i| eval_row(&format!("r{i}"), true)).collect(),
            behavioral: BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
            model_mix_summary: Vec::new(),
            provenance: None,
        };
        let summary = summarize(&eval, &results);
        assert_f64_eq(summary.cost_per_resolved_usd, 1.0);
        assert!(
            summary.cost_per_resolved_ci95_lower <= 1.0,
            "CI lower={} should be <= true value 1.0",
            summary.cost_per_resolved_ci95_lower
        );
        assert!(
            summary.cost_per_resolved_ci95_upper >= 1.0,
            "CI upper={} should be >= true value 1.0",
            summary.cost_per_resolved_ci95_upper
        );
    }

    #[test]
    fn render_summary_table_includes_cost_per_resolved_usd() {
        let mut r = submitted("a");
        r.cost_usd = Some(4.0);
        let results = HashMap::from([("a".to_string(), r)]);
        let eval = EvaluationResults {
            instances: vec![eval_row("a", true)],
            behavioral: BehavioralMetrics::default(),
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
            model_mix_summary: Vec::new(),
            provenance: None,
        };
        let summary = summarize(&eval, &results);
        let rendered = render_summary_table(&summary);
        assert!(
            rendered.contains("cost_per_resolved_usd:"),
            "render_summary_table should include cost_per_resolved_usd; got:\n{rendered}"
        );
        assert!(
            rendered.contains("4.0000"),
            "cost_per_resolved_usd should be 4.0000 (1 resolved at $4); got:\n{rendered}"
        );
    }

    fn make_slot(
        instance_id: &str,
        run_index: u32,
        final_model: Option<&str>,
        cost_usd: Option<f64>,
    ) -> crate::run::compare::LoadedRunSlot {
        let mut r = submitted(instance_id);
        r.final_model = final_model.map(str::to_owned);
        r.cost_usd = cost_usd;
        crate::run::compare::LoadedRunSlot {
            instance_id: instance_id.into(),
            run_index,
            result: r,
        }
    }

    #[test]
    fn model_mix_from_slots_empty_returns_empty() {
        let buckets = build_model_mix_summary_from_slots(&[], &HashMap::new());
        assert!(buckets.is_empty());
    }

    #[test]
    fn model_mix_from_slots_skips_slots_with_no_final_model() {
        let slots = vec![make_slot("a", 0, None, None)];
        let buckets = build_model_mix_summary_from_slots(&slots, &HashMap::new());
        assert!(buckets.is_empty());
    }

    #[test]
    fn model_mix_from_slots_counts_all_rerun_slots() {
        // Two slots for same instance using different models — both must appear.
        let slots = vec![
            make_slot("a", 0, Some("primary"), Some(1.0)),
            make_slot("a", 1, Some("secondary"), Some(0.5)),
        ];
        let buckets = build_model_mix_summary_from_slots(&slots, &HashMap::new());
        assert_eq!(buckets.len(), 2);
        let models: Vec<&str> = buckets.iter().map(|b| b.model.as_str()).collect();
        assert!(models.contains(&"primary"), "primary missing: {models:?}");
        assert!(
            models.contains(&"secondary"),
            "secondary missing: {models:?}"
        );
    }

    #[test]
    fn model_mix_from_slots_uses_per_slot_resolution() {
        let slots = vec![
            make_slot("a", 0, Some("model-x"), None),
            make_slot("b", 0, Some("model-x"), None),
        ];
        let mut resolved_by_run = HashMap::new();
        resolved_by_run.insert(RunSlotKey::new("a", 0), true);
        // b/0 not in map → false
        let buckets = build_model_mix_summary_from_slots(&slots, &resolved_by_run);
        assert_eq!(buckets.len(), 1);
        let bucket = &buckets[0];
        assert_eq!(bucket.model, "model-x");
        assert_eq!(bucket.n, 2);
        assert_eq!(bucket.resolved, 1);
    }

    // --- Evaluator provenance helpers ---

    #[test]
    fn sha256_file_produces_valid_hex_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let digest = sha256_file(&path).unwrap();
        assert_eq!(digest.len(), 64, "SHA-256 hex digest should be 64 chars");
        assert!(
            digest.chars().all(|c| c.is_ascii_hexdigit()),
            "digest should be lowercase hex: {digest}"
        );
        // Same content → same digest (deterministic)
        let digest2 = sha256_file(&path).unwrap();
        assert_eq!(digest, digest2);
    }

    #[test]
    fn sha256_file_returns_error_for_missing_file() {
        let result = sha256_file(std::path::Path::new("/nonexistent/path/file.txt"));
        assert!(result.is_err());
    }

    #[test]
    fn utc_now_iso8601_returns_valid_format() {
        let ts = utc_now_iso8601();
        // Expected: "YYYY-MM-DDTHH:MM:SSZ"
        assert_eq!(ts.len(), 20, "timestamp should be 20 chars: {ts}");
        assert!(ts.ends_with('Z'), "timestamp should end with Z: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    #[test]
    fn collect_report_paths_returns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let args = EvaluateArgs {
            sweep_dir: dir.path().to_path_buf(),
            dataset_path: None,
            backend: EvaluateBackend::None,
            timeout_per_instance_secs: 300,
            parallel: 1,
            sb_subset: "swe-bench-m".into(),
            sb_split: "dev".into(),
            run_id: Some("test-run".into()),
            breakdown: BreakdownSelection::none(),
            cost_attribution: false,
        };
        let report_dir = dir.path().join("sb_cli_reports");
        let paths = collect_report_paths(&report_dir, "test-run", &args);
        assert!(paths.is_empty(), "no report file should mean empty paths");
    }

    #[test]
    fn collect_report_paths_returns_path_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let report_dir = dir.path().join("sb_cli_reports");
        std::fs::create_dir_all(&report_dir).unwrap();
        let report_file = report_dir.join("swe-bench-m__dev__test-run.json");
        std::fs::write(&report_file, b"{}").unwrap();
        let args = EvaluateArgs {
            sweep_dir: dir.path().to_path_buf(),
            dataset_path: None,
            backend: EvaluateBackend::None,
            timeout_per_instance_secs: 300,
            parallel: 1,
            sb_subset: "swe-bench-m".into(),
            sb_split: "dev".into(),
            run_id: Some("test-run".into()),
            breakdown: BreakdownSelection::none(),
            cost_attribution: false,
        };
        let paths = collect_report_paths(&report_dir, "test-run", &args);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].contains("swe-bench-m__dev__test-run.json"));
    }

    #[test]
    fn build_source_reports_returns_empty_when_resolved_by_run_is_empty() {
        let args = EvaluateArgs {
            sweep_dir: "/tmp".into(),
            dataset_path: None,
            backend: EvaluateBackend::None,
            timeout_per_instance_secs: 300,
            parallel: 1,
            sb_subset: "swe-bench-m".into(),
            sb_split: "dev".into(),
            run_id: Some("r".into()),
            breakdown: BreakdownSelection::none(),
            cost_attribution: false,
        };
        let entries = build_source_reports(&HashMap::new(), &args, "r");
        assert!(entries.is_empty());
    }

    #[test]
    fn build_source_reports_returns_empty_when_max_run_index_is_one() {
        let mut resolved_by_run = HashMap::new();
        resolved_by_run.insert(RunSlotKey::new("task-a", 1), true);
        let args = EvaluateArgs {
            sweep_dir: "/tmp".into(),
            dataset_path: None,
            backend: EvaluateBackend::None,
            timeout_per_instance_secs: 300,
            parallel: 1,
            sb_subset: "swe-bench-m".into(),
            sb_split: "dev".into(),
            run_id: Some("r".into()),
            breakdown: BreakdownSelection::none(),
            cost_attribution: false,
        };
        let entries = build_source_reports(&resolved_by_run, &args, "r");
        assert!(
            entries.is_empty(),
            "single-run sweeps have no source_reports"
        );
    }

    #[test]
    fn build_provenance_none_backend_has_no_prediction_path_or_sb_cli() {
        let dir = tempfile::tempdir().unwrap();
        let args = EvaluateArgs {
            sweep_dir: dir.path().to_path_buf(),
            dataset_path: None,
            backend: EvaluateBackend::None,
            timeout_per_instance_secs: 300,
            parallel: 1,
            sb_subset: "swe-bench-m".into(),
            sb_split: "dev".into(),
            run_id: Some("my-run".into()),
            breakdown: BreakdownSelection::none(),
            cost_attribution: false,
        };
        let prov = build_provenance(&args, &HashMap::new(), None, "2026-01-01T00:00:00Z".into());
        assert_eq!(prov.backend, "none");
        assert!(prov.backend_version.is_none());
        assert!(prov.prediction_path.is_none());
        assert!(prov.sb_cli.is_none());
        assert_eq!(prov.run_id.as_deref(), Some("my-run"));
        assert_eq!(prov.dataset_subset.as_deref(), Some("swe-bench-m"));
        assert_eq!(prov.dataset_split.as_deref(), Some("dev"));
    }

    #[test]
    fn build_provenance_effective_run_id_overrides_args_run_id() {
        let dir = tempfile::tempdir().unwrap();
        let args = EvaluateArgs {
            sweep_dir: dir.path().to_path_buf(),
            dataset_path: None,
            backend: EvaluateBackend::None,
            timeout_per_instance_secs: 300,
            parallel: 1,
            sb_subset: "swe-bench-m".into(),
            sb_split: "dev".into(),
            run_id: None,
            breakdown: BreakdownSelection::none(),
            cost_attribution: false,
        };
        let prov = build_provenance(
            &args,
            &HashMap::new(),
            Some("generated-123"),
            "2026-01-01T00:00:00Z".into(),
        );
        assert_eq!(prov.run_id.as_deref(), Some("generated-123"));
    }
}
