//! `bench evaluate`: score an existing sweep by real resolved-rate.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::compare::{load_run_slots, load_sweep};
use crate::run::swebench::{self, InstanceResult, TokenBreakdown, effective_runs};
use crate::trajectory::{FailureCategory, outcome};

pub const COST_ATTRIBUTION_RESOLVED_BUCKET: &str = "resolved";
pub const COST_ATTRIBUTION_UNCATEGORIZED_BUCKET: &str = "uncategorized";
pub const COST_ATTRIBUTION_TOTAL_BUCKET: &str = "TOTAL";
pub const ALL_FAILURE_CATEGORIES: [FailureCategory; 10] = [
    FailureCategory::EnvSetup,
    FailureCategory::ModelApi,
    FailureCategory::ModelParse,
    FailureCategory::StepLimit,
    FailureCategory::CostLimit,
    FailureCategory::WallclockTimeout,
    FailureCategory::PatchApplyInvalid,
    FailureCategory::PatchEmpty,
    FailureCategory::AgentInternal,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdown: Vec<BreakdownBucket>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cost_attribution: Vec<CostAttributionBucket>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BreakdownBucket {
    pub bucket_axis: BreakdownAxis,
    pub bucket_value: String,
    pub n: usize,
    pub resolved: usize,
    pub resolved_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostAttributionBucket {
    pub bucket: String,
    pub n: usize,
    pub total_usd: f64,
    pub mean_usd: f64,
    pub share_pct: f64,
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
}

impl EvaluateRunOutput {
    fn without_run_resolution(eval: EvaluationResults) -> Self {
        Self {
            eval,
            resolved_by_run: HashMap::new(),
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
    let run_output = match args.backend {
        EvaluateBackend::None => {
            EvaluateRunOutput::without_run_resolution(build_none_eval(&results))
        }
        EvaluateBackend::SbCli => run_sb_cli(args, &results)?,
    };
    let mut eval = run_output.eval;
    eval.breakdown = build_breakdown(&eval.instances, &results, &args.breakdown);
    if args.cost_attribution {
        let run_slots = load_run_slots(&args.sweep_dir, &results)?;
        eval.cost_attribution = build_cost_attribution_from_run_slots(
            &run_slots,
            &run_output.resolved_by_run,
            model_name.as_deref(),
        )
        .rows;
    }
    std::fs::write(
        evaluation_path(&args.sweep_dir),
        serde_json::to_string_pretty(&eval)?,
    )?;
    Ok(eval)
}

#[must_use]
pub fn summarize<S: std::hash::BuildHasher>(
    eval: &EvaluationResults,
    results: &HashMap<String, InstanceResult, S>,
) -> EvaluationSummary {
    summarize_with_model(eval, results, None)
}

#[must_use]
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
    }
}

fn build_none_eval(results: &HashMap<String, InstanceResult>) -> EvaluationResults {
    let mut instances: Vec<InstanceEvaluation> = results
        .iter()
        .map(|(id, r)| none_eval_for_result(id, r))
        .collect();
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    EvaluationResults {
        instances,
        breakdown: Vec::new(),
        cost_attribution: Vec::new(),
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
        });
    }
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    EvaluationResults {
        instances,
        breakdown: Vec::new(),
        cost_attribution: Vec::new(),
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
        breakdown: Vec::new(),
        cost_attribution: Vec::new(),
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
    }
}

fn build_breakdown(
    evals: &[InstanceEvaluation],
    results: &HashMap<String, InstanceResult>,
    selection: &BreakdownSelection,
) -> Vec<BreakdownBucket> {
    let mut out = Vec::with_capacity(selection.axes.len());
    for axis in &selection.axes {
        let mut buckets: BTreeMap<String, (usize, usize)> = BTreeMap::new();
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
            let entry = buckets.entry(bucket_value).or_insert((0, 0));
            entry.0 += 1;
            if row.resolved {
                entry.1 += 1;
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
            .map(|(bucket_value, (n, resolved))| BreakdownBucket {
                bucket_axis: *axis,
                bucket_value,
                n,
                resolved,
                resolved_rate: pct(resolved, n),
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
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::AgentInternal => "agent_internal",
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
    let mut out = String::from("axis,bucket,n,resolved,resolved_rate\n");
    for row in rows {
        let axis = match row.bucket_axis {
            BreakdownAxis::Repo => "repo",
            BreakdownAxis::FailureCategory => "failure_category",
        };
        let _ = writeln!(
            out,
            "{axis},{},{},{},{:.4}",
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
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
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
            patch_present: false,
            non_empty_patch: false,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
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
        }
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
        }];
        let rows = build_breakdown(
            &evals,
            &results,
            &BreakdownSelection {
                axes: vec![BreakdownAxis::FailureCategory],
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bucket_value, "none");
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
            breakdown: Vec::new(),
            cost_attribution: Vec::new(),
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
}
