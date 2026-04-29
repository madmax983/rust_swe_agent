//! `bench evaluate`: score an existing sweep by real resolved-rate.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::compare::load_run;
use crate::run::swebench::{self, InstanceResult};
use crate::trajectory::{FailureCategory, outcome};

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
    #[serde(default)]
    pub tests_passed: Vec<String>,
    #[serde(default)]
    pub tests_failed: Vec<String>,
    pub eval_exit_reason: EvalExitReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_log_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResults {
    pub instances: Vec<InstanceEvaluation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdown: Vec<BreakdownBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BreakdownBucket {
    pub bucket_axis: BreakdownAxis,
    pub bucket_value: String,
    pub n: usize,
    pub resolved: usize,
    pub resolved_rate: f64,
}

#[must_use]
pub fn evaluation_path(sweep_dir: &Path) -> PathBuf {
    sweep_dir.join("evaluation.json")
}

pub fn run(args: &EvaluateArgs) -> Result<EvaluationResults, Error> {
    let results = load_run(&args.sweep_dir)?;
    let mut eval = match args.backend {
        EvaluateBackend::None => build_none_eval(&results),
        EvaluateBackend::SbCli => run_sb_cli(args, &results)?,
    };
    eval.breakdown = build_breakdown(&eval.instances, &results, &args.breakdown);
    std::fs::write(
        evaluation_path(&args.sweep_dir),
        serde_json::to_string_pretty(&eval)?,
    )?;
    Ok(eval)
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
    }
}

fn none_eval_for_result(id: &str, r: &InstanceResult) -> InstanceEvaluation {
    let skipped = r.outcome.as_deref() != Some(outcome::SUBMITTED) || !r.patch_present;
    InstanceEvaluation {
        instance_id: id.to_owned(),
        resolved: false,
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
) -> Result<EvaluationResults, Error> {
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

    let mut cmd = Command::new("sb-cli");
    cmd.arg("submit")
        .arg(&args.sb_subset)
        .arg(&args.sb_split)
        .arg("--predictions_path")
        .arg(&preds)
        .arg("--run_id")
        .arg(&run_id)
        .arg("--output_dir")
        .arg(&report_dir)
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
            .arg(&run_id)
            .arg("--output_dir")
            .arg(&report_dir)
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

    let parsed = parse_sb_cli_results(&report_path)?;
    Ok(merge_with_results(results, &parsed))
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
    let mut instances: Vec<InstanceEvaluation> = Vec::new();
    for (id, r) in results {
        if let Some(row) = parsed.get(id) {
            instances.push(row.clone());
            continue;
        }
        let skipped = r.outcome.as_deref() != Some(outcome::SUBMITTED) || !r.patch_present;
        instances.push(InstanceEvaluation {
            instance_id: id.clone(),
            resolved: false,
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
    }
}

fn build_breakdown(
    evals: &[InstanceEvaluation],
    results: &HashMap<String, InstanceResult>,
    selection: &BreakdownSelection,
) -> Vec<BreakdownBucket> {
    let mut out = Vec::new();
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
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::Unknown => "unknown",
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::trajectory::FailureCategory;

    fn submitted(id: &str) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: None,
            cost_usd: None,
            prompt_tokens: None,
            completion_tokens: None,
            duration_secs: None,
            error: None,
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: Vec::new(),
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
            completion_tokens: None,
            duration_secs: None,
            error: Some("boom".into()),
            patch_present: false,
            non_empty_patch: false,
            attempts: 1,
            retry_reasons: Vec::new(),
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
}
