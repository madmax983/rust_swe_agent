//! `bench inspect`: inspect one trajectory or list filtered instances.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::cost::CostSource;
use crate::env::RunResult;
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::evaluate::{EvalExitReason, EvaluationResults};
use crate::run::patch_stats::PatchStats;
use crate::run::swebench::{InstanceResult, ProvenanceManifest, SweBenchInstance};
use crate::trajectory::{
    FailureCategory, FallbackSummary, TokenUsage, Trajectory, VerificationResult,
};

const TRUNCATE_MAX_LINES: usize = 40;
const TRUNCATE_MAX_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectFormat {
    Text,
    Json,
}

#[derive(Debug, Clone)]
pub struct InspectArgs {
    pub sweep: PathBuf,
    pub instance: Option<String>,
    pub filter: Option<String>,
    pub full: bool,
    /// When true, load PASS_TO_PASS / FAIL_TO_PASS from the sweep's dataset.jsonl
    /// and add them to the report.
    pub show_expected: bool,
}

/// Failing tests from the evaluator, or an explanation of why names are unavailable.
#[derive(Debug, Clone, Serialize)]
pub struct FailingTests {
    pub tests: Vec<String>,
    /// `"evaluator"` when names come from evaluator output; `"unavailable"` otherwise.
    pub source: String,
    /// Human-readable reason when `source == "unavailable"`. Empty otherwise.
    pub reason: String,
}

/// Expected test groupings read from the SWE-bench instance record.
#[derive(Debug, Clone, Serialize)]
pub struct ExpectedTests {
    pub pass_to_pass: Vec<String>,
    pub fail_to_pass: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectStep {
    pub index: usize,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation_note: Option<String>,
    /// True when this observation was elided from the model-visible prompt.
    #[serde(default)]
    pub history_elided: bool,
    /// The marker text that was sent to the model in place of the full content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_sent_marker: Option<String>,
    /// Sampling parameters used for the model call that produced this
    /// assistant turn. `None` on non-assistant turns and on legacy
    /// trajectories written before schema 1.8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<crate::model::SamplingParams>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectReport {
    pub sweep_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_cost_source: Option<CostSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_cost_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_stats: Option<PatchStats>,
    pub test_invocations_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_exit_code: Option<i32>,
    pub tests_run_before_submit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tests_passed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_summary: Option<FallbackSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_status: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification_results: Vec<VerificationResult>,
    /// Sum of `model_latency_ms` over all messages. `None` when no turn
    /// recorded model latency (legacy trajectory or deterministic fixture).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_latency_ms_total: Option<u64>,
    /// Sum of `tool_latency_ms` over all messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_latency_ms_total: Option<u64>,
    /// Sum of `harness_overhead_ms` over all messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_overhead_ms_total: Option<u64>,
    /// Each stage's share of `duration_secs`, rounded to whole percent.
    /// Empty when `duration_secs` is missing or zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_share_pct: Option<LatencySharePct>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Failing tests from the evaluator, or a reason why names are unavailable.
    /// `None` for resolved instances (silent on the happy path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_tests: Option<FailingTests>,
    /// PASS_TO_PASS / FAIL_TO_PASS groupings from the SWE-bench instance record.
    /// Populated only when `--show-expected` is set and the dataset is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_tests: Option<ExpectedTests>,
    #[serde(default)]
    pub steps: Vec<InspectStep>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct LatencySharePct {
    pub model_pct: u32,
    pub tool_pct: u32,
    pub harness_pct: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryRow {
    pub instance_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryReport {
    pub sweep_dir: PathBuf,
    pub filter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<ProvenanceManifest>,
    /// Evaluator provenance from `evaluation.json`, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluator_provenance: Option<crate::run::evaluate::EvaluatorProvenance>,
    pub rows: Vec<SummaryRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InspectOutput {
    Instance(Box<InspectReport>),
    Summary(Box<SummaryReport>),
}

pub fn run(args: &InspectArgs) -> Result<InspectOutput, Error> {
    if !args.sweep.exists() {
        return Err(Error::Trajectory(format!(
            "inspect: sweep directory does not exist: {}",
            args.sweep.display()
        )));
    }
    match (&args.instance, &args.filter) {
        (Some(_), Some(_)) => {
            return Err(Error::Trajectory(
                "inspect: pass exactly one of --instance or --filter".into(),
            ));
        }
        (None, None) => {
            return Err(Error::Trajectory(
                "inspect: one of --instance or --filter is required".into(),
            ));
        }
        _ => {}
    }

    if let Some(filter) = &args.filter {
        return build_summary(&args.sweep, filter).map(|r| InspectOutput::Summary(Box::new(r)));
    }

    let instance_id = args.instance.clone().unwrap_or_default();
    let evaluation_overrides = load_evaluation_overrides(&args.sweep)?;
    let dataset_instance = if args.show_expected {
        load_dataset_instance(&args.sweep, &instance_id)
    } else {
        None
    };
    let report = build_instance_report(
        &args.sweep,
        &instance_id,
        args.full,
        evaluation_overrides.as_ref(),
        dataset_instance.as_ref(),
    )?;
    Ok(InspectOutput::Instance(Box::new(report)))
}

fn build_summary(sweep: &Path, filter: &str) -> Result<SummaryReport, Error> {
    let filter = parse_filter(filter)?;
    let mut rows: Vec<SummaryRow> = Vec::new();
    let loaded = crate::run::compare::load_sweep(sweep)?;
    let resolved = load_evaluation_overrides(sweep)?.unwrap_or_default();
    for r in loaded.instances.values() {
        let res = resolved.get(&r.instance_id).map(|value| value.resolved);
        if !filter.matches(r, res) {
            continue;
        }
        rows.push(SummaryRow {
            instance_id: r.instance_id.clone(),
            outcome: r.outcome.clone(),
            failure_category: r.failure_category,
            cost_usd: r.cost_usd,
            resolved: res,
        });
    }
    rows.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    let evaluator_provenance =
        crate::run::compare::load_evaluation_results(sweep)?.and_then(|eval| eval.provenance);
    Ok(SummaryReport {
        sweep_dir: sweep.to_path_buf(),
        filter: filter.raw,
        manifest: loaded.manifest,
        evaluator_provenance,
        rows,
    })
}

#[allow(clippy::too_many_lines)]
fn build_instance_report(
    sweep: &Path,
    instance_id: &str,
    full: bool,
    evaluation_overrides: Option<&HashMap<String, EvaluationOverride>>,
    dataset_instance: Option<&SweBenchInstance>,
) -> Result<InspectReport, Error> {
    let traj_path = resolve_trajectory_path(sweep, instance_id).ok_or_else(|| {
        Error::Trajectory(format!(
            "inspect: trajectory not found for instance `{instance_id}` in {}",
            sweep.display()
        ))
    })?;
    let text = std::fs::read_to_string(&traj_path)?;
    let mut warnings = Vec::new();
    if let Ok(traj_value) = serde_json::from_str::<serde_json::Value>(&text) {
        let compat = classify_json_value(
            &traj_value,
            ArtifactKind::Trajectory,
            traj_path.display().to_string(),
        )
        .map_err(|err| Error::Trajectory(err.to_string()))?;
        warnings.extend(compat.warnings);
    }
    let mut traj: Trajectory = match serde_json::from_str(&text) {
        Ok(t) => t,
        Err(err) => {
            warnings.push(format!(
                "failed to parse trajectory as canonical schema: {err}; rendering minimal report"
            ));
            let resolved = evaluation_overrides
                .and_then(|m| m.get(instance_id))
                .map(|value| value.resolved);
            let eval_override = evaluation_overrides.and_then(|m| m.get(instance_id));
            let minimal_redactor = Redactor::default_enabled();
            let failing_tests = redact_failing_tests(
                build_failing_tests(resolved, eval_override, None),
                &minimal_redactor,
                &mut warnings,
            );
            return Ok(InspectReport {
                sweep_dir: sweep.to_path_buf(),
                instance_id: Some(instance_id.to_owned()),
                model: None,
                outcome: None,
                failure_category: None,
                total_cost_usd: None,
                actual_cost_usd: None,
                actual_cost_source: None,
                baseline_cost_usd: None,
                baseline_cost_model: None,
                prompt_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                completion_tokens: None,
                resolved,
                patch_stats: eval_override.and_then(|value| value.patch_stats.clone()),
                test_invocations_count: 0,
                last_test_exit_code: None,
                tests_run_before_submit: false,
                last_tests_passed: None,
                fallback_summary: None,
                verification_status: None,
                verification_results: vec![],
                model_latency_ms_total: None,
                tool_latency_ms_total: None,
                harness_overhead_ms_total: None,
                latency_share_pct: None,
                warnings,
                failing_tests,
                expected_tests: dataset_instance
                    .map(|inst| extract_expected_tests(inst, &minimal_redactor)),
                steps: vec![],
            });
        }
    };

    let inspect_redactor = Redactor::default_enabled();
    let redacted_at_view = redact_trajectory_for_inspect(&mut traj, &inspect_redactor);
    if traj
        .info
        .redaction
        .as_ref()
        .is_some_and(|summary| summary.redacted)
    {
        warnings.push("content was redacted at run time".into());
    }
    if redacted_at_view {
        warnings.push("bench inspect redacted secret-shaped content at view time".into());
    }

    let steps = build_inspect_steps(&traj, full);
    let (model_latency_ms_total, tool_latency_ms_total, harness_overhead_ms_total) =
        sum_stage_latencies(&traj);
    let latency_share_pct = compute_latency_share(
        traj.info.duration_secs,
        model_latency_ms_total,
        tool_latency_ms_total,
        harness_overhead_ms_total,
    );

    let token_usage = traj.info.token_usage.as_ref();
    let eval_override = evaluation_overrides.and_then(|m| m.get(instance_id));
    let resolved = eval_override.map(|value| value.resolved);
    let failing_tests = redact_failing_tests(
        build_failing_tests(resolved, eval_override, traj.info.failure_category),
        &inspect_redactor,
        &mut warnings,
    );
    let expected_tests =
        dataset_instance.map(|inst| extract_expected_tests(inst, &inspect_redactor));
    Ok(InspectReport {
        sweep_dir: sweep.to_path_buf(),
        instance_id: Some(instance_id.to_owned()),
        model: traj.info.model_name,
        outcome: traj.info.outcome,
        failure_category: traj.info.failure_category,
        total_cost_usd: traj.info.total_cost_usd,
        actual_cost_usd: traj.info.actual_cost_usd,
        actual_cost_source: traj.info.actual_cost_source,
        baseline_cost_usd: traj.info.baseline_cost_usd,
        baseline_cost_model: traj.info.baseline_cost_model,
        prompt_tokens: token_usage.map(TokenUsage::total_prompt_tokens),
        input_tokens: token_usage.map(|t| t.prompt_tokens),
        cache_read_tokens: token_usage.map(|t| t.cache_read_tokens),
        cache_creation_tokens: token_usage.map(|t| t.cache_creation_tokens),
        completion_tokens: token_usage.map(|t| t.completion_tokens),
        resolved,
        patch_stats: eval_override.and_then(|value| value.patch_stats.clone()),
        test_invocations_count: traj.info.test_invocations.len(),
        last_test_exit_code: traj
            .info
            .test_invocations
            .last()
            .map(|invocation| invocation.exit_code),
        tests_run_before_submit: traj.info.tests_run_before_submit,
        last_tests_passed: traj.info.last_tests_passed,
        fallback_summary: traj.info.fallback_summary,
        verification_status: traj.info.verification_status,
        verification_results: traj.info.verification_results,
        model_latency_ms_total,
        tool_latency_ms_total,
        harness_overhead_ms_total,
        latency_share_pct,
        warnings,
        failing_tests,
        expected_tests,
        steps,
    })
}

/// Returns `(model_total, tool_total, harness_total)` summed across messages.
/// Each component is `None` when no message recorded that stage. Recording
/// `Some(0)` is preserved as a measured zero (e.g., a fast tool turn).
fn sum_stage_latencies(traj: &Trajectory) -> (Option<u64>, Option<u64>, Option<u64>) {
    let mut model = None::<u64>;
    let mut tool = None::<u64>;
    let mut harness = None::<u64>;
    for m in &traj.messages {
        if let Some(v) = m.extra.model_latency_ms {
            model = Some(model.unwrap_or(0).saturating_add(v));
        }
        if let Some(v) = m.extra.tool_latency_ms {
            tool = Some(tool.unwrap_or(0).saturating_add(v));
        }
        if let Some(v) = m.extra.harness_overhead_ms {
            harness = Some(harness.unwrap_or(0).saturating_add(v));
        }
    }
    (model, tool, harness)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn compute_latency_share(
    duration_secs: Option<f64>,
    model_ms: Option<u64>,
    tool_ms: Option<u64>,
    harness_ms: Option<u64>,
) -> Option<LatencySharePct> {
    let dur_ms = duration_secs? * 1000.0;
    if dur_ms <= 0.0 {
        return None;
    }
    if model_ms.is_none() && tool_ms.is_none() && harness_ms.is_none() {
        return None;
    }
    let pct = |ms: Option<u64>| -> u32 {
        let v = ms.unwrap_or(0) as f64;
        ((v / dur_ms) * 100.0).round() as u32
    };
    Some(LatencySharePct {
        model_pct: pct(model_ms),
        tool_pct: pct(tool_ms),
        harness_pct: pct(harness_ms),
    })
}

fn build_inspect_steps(traj: &Trajectory, full: bool) -> Vec<InspectStep> {
    build_inspect_steps_with_max(traj, full, TRUNCATE_MAX_BYTES)
}

pub(crate) fn build_inspect_steps_with_max(
    traj: &Trajectory,
    full: bool,
    max_bytes: usize,
) -> Vec<InspectStep> {
    let mut steps = Vec::new();
    for (msg_idx, msg) in traj.messages.iter().enumerate() {
        let current_index = steps.len();
        let role = msg.role.clone();
        if role == "assistant" {
            steps.push(InspectStep {
                index: current_index,
                role,
                message: Some(msg.content.clone()),
                bash: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                truncated: false,
                truncation_note: None,
                history_elided: false,
                as_sent_marker: None,
                sampling: msg.extra.sampling.clone(),
            });
            continue;
        }

        if role != "user" {
            continue;
        }
        let Some(run_result) = msg
            .extra
            .other
            .get("run_result")
            .and_then(|v| serde_json::from_value::<RunResult>(v.clone()).ok())
        else {
            continue;
        };
        let bash = infer_bash_from_previous_assistant(traj, msg_idx);
        let history_elided = msg
            .extra
            .other
            .get("history_elided")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let as_sent_marker = if history_elided {
            msg.extra
                .other
                .get("history_elision_marker")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        } else {
            None
        };
        let (stdout, stdout_note, stdout_truncated) =
            maybe_truncate(&run_result.stdout, full, max_bytes, current_index);
        let (stderr, stderr_note, stderr_truncated) =
            maybe_truncate(&run_result.stderr, full, max_bytes, current_index);
        let mut note_parts = Vec::new();
        if let Some(n) = stdout_note {
            note_parts.push(format!("stdout: {n}"));
        }
        if let Some(n) = stderr_note {
            note_parts.push(format!("stderr: {n}"));
        }
        steps.push(InspectStep {
            index: current_index,
            role: "bash".into(),
            message: None,
            bash,
            exit_code: Some(run_result.exit_code),
            stdout: Some(stdout),
            stderr: Some(stderr),
            truncated: stdout_truncated || stderr_truncated,
            truncation_note: (!note_parts.is_empty()).then(|| note_parts.join("; ")),
            history_elided,
            as_sent_marker,
            sampling: None,
        });
    }
    steps
}

pub fn render_text(output: &InspectOutput) -> String {
    match output {
        InspectOutput::Instance(r) => render_instance_text(r),
        InspectOutput::Summary(s) => render_summary_text(s),
    }
}

use comfy_table::Table;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;

fn render_summary_text(report: &SummaryReport) -> String {
    let mut s = String::new();
    s.push_str("\n=== bench inspect summary ===\n");
    let _ = writeln!(s, "Sweep:   {}", report.sweep_dir.display());
    let _ = writeln!(s, "Filter:  {}", report.filter);
    if let Some(m) = &report.manifest {
        let _ = writeln!(
            s,
            "Manifest: harness_sha={} prompt_sha={} dataset_sha={} model={}",
            m.harness.git_sha.as_deref().unwrap_or("unavailable"),
            m.prompt_template.sha256,
            m.dataset.sha256,
            m.model.name
        );
    } else {
        s.push_str("Manifest: unavailable\n");
    }
    if let Some(prov) = &report.evaluator_provenance {
        let version = prov.backend_version.as_deref().unwrap_or("?");
        let subset = prov.dataset_subset.as_deref().unwrap_or("?");
        let split = prov.dataset_split.as_deref().unwrap_or("?");
        let run_id = prov.run_id.as_deref().unwrap_or("?");
        let started = prov.eval_started_at.as_deref().unwrap_or("?");
        let _ = writeln!(
            s,
            "evaluator_provenance: backend={} version={} subset={} split={} run_id={} started={}",
            prov.backend, version, subset, split, run_id, started
        );
    } else {
        s.push_str("evaluator_provenance: unavailable\n");
    }
    s.push('\n');

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "instance_id",
            "outcome",
            "failure_category",
            "cost_usd",
            "resolved",
        ]);

    for row in &report.rows {
        table.add_row(vec![
            row.instance_id.clone(),
            row.outcome.as_deref().unwrap_or("?").to_string(),
            row.failure_category
                .map_or("none", failure_label)
                .to_string(),
            row.cost_usd
                .map_or_else(|| "?".into(), |c| format!("{c:.4}")),
            row.resolved
                .map_or("?", |v| if v { "true" } else { "false" })
                .to_string(),
        ]);
    }

    s.push_str(&table.to_string());
    s.push('\n');
    s
}

#[allow(clippy::too_many_lines)]
fn render_instance_text(report: &InspectReport) -> String {
    let mut s = String::new();
    let color = std::io::stdout().is_terminal();
    s.push_str("\n=== bench inspect ===\n");
    let _ = writeln!(
        s,
        "instance_id:      {}",
        report.instance_id.as_deref().unwrap_or("?")
    );
    let _ = writeln!(
        s,
        "model:            {}",
        report.model.as_deref().unwrap_or("?")
    );
    let _ = writeln!(
        s,
        "outcome:          {}",
        report.outcome.as_deref().unwrap_or("?")
    );
    let _ = writeln!(
        s,
        "failure_category: {}",
        report.failure_category.map_or("none", failure_label)
    );
    let _ = writeln!(
        s,
        "total_cost_usd:   {}",
        report
            .total_cost_usd
            .map_or_else(|| "?".into(), |v| format!("{v:.6}"))
    );
    if report.actual_cost_usd.is_some() || report.actual_cost_source.is_some() {
        let source = report
            .actual_cost_source
            .map_or_else(|| "unknown".to_owned(), |source| source.to_string());
        let _ = writeln!(
            s,
            "actual_cost_usd:  {} ({source})",
            report
                .actual_cost_usd
                .map_or_else(|| "?".into(), |v| format!("{v:.6}"))
        );
    }
    if report.baseline_cost_usd.is_some() || report.baseline_cost_model.is_some() {
        let model = report
            .baseline_cost_model
            .as_deref()
            .unwrap_or("claude-3-5-sonnet");
        let _ = writeln!(
            s,
            "baseline_cost_usd: {} ({model})",
            report
                .baseline_cost_usd
                .map_or_else(|| "?".into(), |v| format!("{v:.6}"))
        );
    }
    let _ = writeln!(
        s,
        "tokens:           {}",
        render_token_summary(
            report.prompt_tokens,
            report.input_tokens,
            report.cache_read_tokens,
            report.cache_creation_tokens,
            report.completion_tokens,
        ),
    );
    if let Some(r) = report.resolved {
        let _ = writeln!(s, "resolved:         {r}");
    }
    if let Some(ft) = &report.failing_tests {
        if ft.source == "evaluator" {
            let _ = writeln!(s, "Failing tests ({}):", ft.tests.len());
            for name in &ft.tests {
                let _ = writeln!(s, "  {name}");
            }
        } else {
            let _ = writeln!(s, "Failing tests: <{}>", ft.reason);
        }
    }
    if let Some(et) = &report.expected_tests {
        let _ = writeln!(s, "PASS_TO_PASS ({}):", et.pass_to_pass.len());
        for name in &et.pass_to_pass {
            let _ = writeln!(s, "  {name}");
        }
        let _ = writeln!(s, "FAIL_TO_PASS ({}):", et.fail_to_pass.len());
        for name in &et.fail_to_pass {
            let _ = writeln!(s, "  {name}");
        }
    }
    write_patch_stats_lines(&mut s, report.patch_stats.as_ref());
    let submitted_without_tests = report.outcome.as_deref()
        == Some(crate::trajectory::outcome::SUBMITTED)
        && !report.tests_run_before_submit;
    let _ = writeln!(
        s,
        "tests:            count={} last_exit_code={} last_passed={} submitted_without_tests={submitted_without_tests}",
        report.test_invocations_count,
        report
            .last_test_exit_code
            .map_or_else(|| "?".into(), |code| code.to_string()),
        report
            .last_tests_passed
            .map_or_else(|| "?".into(), |passed| passed.to_string()),
    );
    if let Some(fb) = &report.fallback_summary {
        let _ = writeln!(
            s,
            "fallback:         happened={} count={} primary={} final={}",
            fb.fallback_happened, fb.fallback_count, fb.primary_model, fb.final_model,
        );
        if !fb.attempted_models.is_empty() {
            let _ = writeln!(s, "fallback_chain:   {}", fb.attempted_models.join(" → "));
        }
    }
    if report.verification_status.is_some() || !report.verification_results.is_empty() {
        let _ = writeln!(
            s,
            "verification:     status={} checks={}",
            report
                .verification_status
                .as_deref()
                .unwrap_or("unverified"),
            report.verification_results.len(),
        );
        for r in &report.verification_results {
            let timeout_note = if r.timed_out { " (timed_out)" } else { "" };
            let _ = writeln!(
                s,
                "  [{}] passed={} exit_code={} duration={}ms{}",
                r.name, r.passed, r.exit_code, r.duration_ms, timeout_note,
            );
        }
    }
    if report.model_latency_ms_total.is_some()
        || report.tool_latency_ms_total.is_some()
        || report.harness_overhead_ms_total.is_some()
    {
        let model_ms = report
            .model_latency_ms_total
            .map_or_else(|| "unknown".to_owned(), |v| v.to_string());
        let tool_ms = report
            .tool_latency_ms_total
            .map_or_else(|| "unknown".to_owned(), |v| v.to_string());
        let harness_ms = report
            .harness_overhead_ms_total
            .map_or_else(|| "unknown".to_owned(), |v| v.to_string());
        let share = report.latency_share_pct.map_or_else(String::new, |s| {
            format!(" ({}% / {}% / {}%)", s.model_pct, s.tool_pct, s.harness_pct)
        });
        let _ = writeln!(
            s,
            "latency:          model_ms={model_ms} tool_ms={tool_ms} harness_ms={harness_ms}{share}",
        );
    } else {
        // Legacy trajectory pre-1.5: no stage attribution recorded.
        // Surface explicitly so operators don't misread silence as zero.
        let _ = writeln!(s, "latency:          unknown (pre-1.5 trajectory)");
    }
    for w in &report.warnings {
        let _ = writeln!(s, "warning:          {w}");
    }

    for step in &report.steps {
        s.push_str(&render_step_text(step, color));
    }
    s
}

/// Render a single inspect step to a human-readable string.
///
/// `color` enables ANSI colour codes for the step header.
pub(crate) fn render_step_text(step: &InspectStep, color: bool) -> String {
    let mut s = String::new();
    let header = format!("[step {}] {}", step.index, step.role);
    if color {
        let _ = writeln!(s, "\n\x1b[1;36m{header}\x1b[0m");
    } else {
        let _ = writeln!(s, "\n{header}");
    }
    if let Some(sampling) = &step.sampling {
        let _ = writeln!(s, "sampling: {}", sampling.summary_line());
    }

    if let Some(msg) = &step.message {
        s.push_str(msg);
        if !msg.ends_with('\n') {
            s.push('\n');
        }
        return s;
    }

    if let Some(cmd) = &step.bash {
        let _ = writeln!(s, "$ {cmd}");
    }
    if let Some(code) = step.exit_code {
        let _ = writeln!(s, "exit_code: {code}");
    }
    if step.history_elided {
        let marker = step
            .as_sent_marker
            .as_deref()
            .unwrap_or("[elision marker unavailable]");
        let _ = writeln!(s, "[as-sent to model] {marker}");
        if step.stdout.as_ref().is_some_and(|o| !o.is_empty()) {
            let _ = writeln!(s, "[as-recorded stdout]");
            if let Some(out) = &step.stdout {
                let _ = writeln!(s, "{out}");
            }
        }
    } else if let Some(out) = &step.stdout {
        let _ = writeln!(s, "stdout:\n{out}");
    }
    if let Some(err) = &step.stderr {
        if !err.is_empty() {
            let _ = writeln!(s, "stderr:\n{err}");
        }
    }
    if let Some(note) = &step.truncation_note {
        let _ = writeln!(s, "… {note}");
    }
    s
}

fn write_patch_stats_lines(s: &mut String, stats: Option<&PatchStats>) {
    let Some(stats) = stats else {
        return;
    };
    let _ = writeln!(
        s,
        "patch_stats:      files={} hunks={} +{} -{} empty={} tests={} lock_or_generated={}",
        stats.files_changed,
        stats.hunks,
        stats.lines_added,
        stats.lines_removed,
        stats.is_empty,
        stats.touches_test_files,
        stats.touches_lock_or_generated
    );
    if let (Some(files_iou), Some(lines_overlap), Some(size_ratio)) = (
        stats.gold_files_iou,
        stats.gold_lines_overlap,
        stats.gold_size_ratio,
    ) {
        let _ = writeln!(
            s,
            "gold_distance:    files_iou={files_iou:.3} lines_overlap={lines_overlap:.3} size_ratio={size_ratio:.3}"
        );
    }
}

fn render_token_summary(
    prompt_tokens: Option<u64>,
    input_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    completion_tokens: Option<u64>,
) -> String {
    let prompt = format_optional_u64(prompt_tokens);
    let completion = format_optional_u64(completion_tokens);
    let cache_read = cache_read_tokens.unwrap_or(0);
    let cache_creation = cache_creation_tokens.unwrap_or(0);
    if cache_read == 0 && cache_creation == 0 {
        return format!("prompt={prompt} completion={completion}");
    }
    format!(
        "prompt={prompt} (input={} cache_read={} cache_creation={}) completion={completion}",
        format_optional_u64(input_tokens),
        cache_read,
        cache_creation
    )
}

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "?".into(), |v| v.to_string())
}

fn infer_bash_from_previous_assistant(traj: &Trajectory, msg_idx: usize) -> Option<String> {
    traj.messages
        .iter()
        .take(msg_idx)
        .rev()
        .find(|m| m.role == "assistant")
        .and_then(|m| m.extra.actions.as_ref())
        .and_then(|actions| actions.first())
        .and_then(|a| (a != "__SUBMIT__").then(|| a.clone()))
}

fn maybe_truncate(
    text: &str,
    full: bool,
    max_bytes: usize,
    step_index: usize,
) -> (String, Option<String>, bool) {
    if full {
        return (text.to_owned(), None, false);
    }
    let line_count = text.lines().count();
    if text.len() <= max_bytes && line_count <= TRUNCATE_MAX_LINES {
        return (text.to_owned(), None, false);
    }

    let mut truncated = String::new();
    let mut consumed_bytes = 0usize;
    let mut shown_lines = 0usize;
    for line in text.split_inclusive('\n') {
        if shown_lines >= TRUNCATE_MAX_LINES || consumed_bytes >= max_bytes {
            break;
        }
        let remaining = max_bytes - consumed_bytes;
        let head = utf8_prefix_within_bytes(line, remaining);
        if head.is_empty() {
            break;
        }
        truncated.push_str(head);
        consumed_bytes += head.len();
        shown_lines += 1;
        if head.len() < line.len() {
            break;
        }
    }
    while truncated.ends_with('\n') {
        truncated.pop();
    }
    if shown_lines == 0 && !truncated.is_empty() {
        shown_lines = truncated.lines().count();
    }
    let more_lines = line_count.saturating_sub(shown_lines);
    let note =
        format!("[{more_lines} more lines, full output at trajectory.json#/steps/{step_index}]");
    (truncated, Some(note), true)
}

fn utf8_prefix_within_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    &s[..end]
}

pub(crate) fn resolve_trajectory_path(sweep: &Path, instance_id: &str) -> Option<PathBuf> {
    let nested = sweep.join(instance_id).join("trajectory.json");
    if nested.exists() {
        return Some(nested);
    }
    let nested_run = sweep.join(instance_id).join("run-1.traj.json");
    if nested_run.exists() {
        return Some(nested_run);
    }
    let flat = sweep.join(format!("{instance_id}.traj.json"));
    if flat.exists() {
        return Some(flat);
    }
    let bundled = sweep
        .join("trajectories")
        .join(format!("{instance_id}.traj.json"));
    bundled.exists().then_some(bundled)
}

#[derive(Debug, Clone)]
struct EvaluationOverride {
    resolved: bool,
    patch_stats: Option<PatchStats>,
    tests_failed: Vec<String>,
    eval_exit_reason: Option<EvalExitReason>,
}

fn load_evaluation_overrides(
    dir: &Path,
) -> Result<Option<HashMap<String, EvaluationOverride>>, Error> {
    let eval_path = crate::run::evaluate::evaluation_path(dir);
    if !eval_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&eval_path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(
        &value,
        ArtifactKind::EvaluationResults,
        eval_path.display().to_string(),
    )
    .map_err(|err| Error::Trajectory(err.to_string()))?;
    let eval: EvaluationResults = serde_json::from_value(value)?;
    Ok(Some(
        eval.instances
            .into_iter()
            .map(|x| {
                (
                    x.instance_id,
                    EvaluationOverride {
                        resolved: x.resolved,
                        patch_stats: x.patch_stats,
                        tests_failed: x.tests_failed,
                        eval_exit_reason: Some(x.eval_exit_reason),
                    },
                )
            })
            .collect(),
    ))
}

fn build_failing_tests(
    resolved: Option<bool>,
    eval_override: Option<&EvaluationOverride>,
    failure_category: Option<FailureCategory>,
) -> Option<FailingTests> {
    if resolved == Some(true) {
        return None;
    }
    let eval = eval_override?;
    if !eval.tests_failed.is_empty() {
        return Some(FailingTests {
            tests: eval.tests_failed.clone(),
            source: "evaluator".into(),
            reason: String::new(),
        });
    }
    let reason = eval.eval_exit_reason.as_ref().map_or_else(
        || failure_category.map_or_else(|| "unknown".into(), |c| failure_label(c).to_owned()),
        eval_exit_reason_label,
    );
    Some(FailingTests {
        tests: vec![],
        source: "unavailable".into(),
        reason,
    })
}

fn redact_failing_tests(
    failing_tests: Option<FailingTests>,
    redactor: &Redactor,
    warnings: &mut Vec<String>,
) -> Option<FailingTests> {
    let mut ft = failing_tests?;
    let mut any_redacted = false;
    for name in &mut ft.tests {
        let outcome = redactor.redact_text(name, surface::INSPECT);
        if outcome.redacted {
            any_redacted = true;
        }
        *name = outcome.text;
    }
    if any_redacted {
        warnings.push("bench inspect redacted secret-shaped content at view time".into());
    }
    Some(ft)
}

fn eval_exit_reason_label(reason: &EvalExitReason) -> String {
    match reason {
        EvalExitReason::Resolved => "resolved".into(),
        EvalExitReason::Unresolved => "unresolved".into(),
        EvalExitReason::PatchApplyFailed => "patch_apply_failed".into(),
        EvalExitReason::EvalError => "eval_error".into(),
        EvalExitReason::SkippedNoPatch => "skipped_no_patch".into(),
    }
}

fn extract_expected_tests(inst: &SweBenchInstance, redactor: &Redactor) -> ExpectedTests {
    let get_string_list = |key: &str| -> Vec<String> {
        let Some(val) = inst.other.get(key) else {
            return vec![];
        };
        // SWE-bench Hugging Face exports store these as a JSON-encoded string
        // (e.g. "[\"test_a\", \"test_b\"]"). Accept both that form and a native
        // JSON array so synthetic fixtures and real datasets both work.
        let items: Vec<serde_json::Value> = if let Some(arr) = val.as_array() {
            arr.clone()
        } else if let Some(s) = val.as_str() {
            serde_json::from_str(s).unwrap_or_default()
        } else {
            vec![]
        };
        items
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| redactor.redact_text(s, surface::INSPECT).text)
            .collect()
    };
    ExpectedTests {
        pass_to_pass: get_string_list("PASS_TO_PASS"),
        fail_to_pass: get_string_list("FAIL_TO_PASS"),
    }
}

fn load_dataset_instance(sweep: &Path, instance_id: &str) -> Option<SweBenchInstance> {
    let dataset_path = sweep.join("dataset.jsonl");
    if !dataset_path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&dataset_path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(inst) = serde_json::from_str::<SweBenchInstance>(line) {
            if inst.instance_id == instance_id {
                return Some(inst);
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
struct FilterSpec {
    raw: String,
    key: String,
    value: String,
}

impl FilterSpec {
    fn matches(&self, row: &InstanceResult, resolved: Option<bool>) -> bool {
        match self.key.as_str() {
            "resolved" => {
                let expected = self.value == "true";
                resolved == Some(expected)
            }
            "failure_category" => row
                .failure_category
                .is_some_and(|c| failure_label(c) == self.value),
            _ => false,
        }
    }
}

fn parse_filter(s: &str) -> Result<FilterSpec, Error> {
    let mut it = s.splitn(2, '=');
    let key = it.next().unwrap_or_default().trim().to_owned();
    let value = it.next().unwrap_or_default().trim().to_owned();
    if key.is_empty() || value.is_empty() {
        return Err(Error::Trajectory(
            "inspect: --filter expects key=value (e.g. resolved=false)".into(),
        ));
    }
    if key != "resolved" && key != "failure_category" {
        return Err(Error::Trajectory(
            "inspect: supported filters are `resolved` and `failure_category`".into(),
        ));
    }
    if key == "resolved" && value != "true" && value != "false" {
        return Err(Error::Trajectory(
            "inspect: resolved filter must be true or false".into(),
        ));
    }
    Ok(FilterSpec {
        raw: s.to_owned(),
        key,
        value,
    })
}

fn failure_label(c: FailureCategory) -> &'static str {
    match c {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::BudgetExhausted => "budget_exhausted",
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::AgentStagnation => "agent_stagnation",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::HistoryCompactionFailed => "history_compaction_failed",
        FailureCategory::Unknown => "unknown",
    }
}

pub fn redact_trajectory_for_inspect(trajectory: &mut Trajectory, redactor: &Redactor) -> bool {
    let mut redacted = false;
    if let Some(task) = &mut trajectory.info.task {
        let outcome = redactor.redact_text(task, surface::INSPECT);
        redacted |= outcome.redacted;
        *task = outcome.text;
    }
    if let Some(final_output) = &mut trajectory.info.final_output {
        let outcome = redactor.redact_text(final_output, surface::INSPECT);
        redacted |= outcome.redacted;
        *final_output = outcome.text;
    }
    for value in trajectory.info.other.values_mut() {
        redacted |= redactor.redact_json_value(value, surface::INSPECT);
    }
    for vr in &mut trajectory.info.verification_results {
        let command = redactor.redact_text(&vr.command, surface::INSPECT);
        redacted |= command.redacted;
        vr.command = command.text;
        let stdout = redactor.redact_text(&vr.stdout_preview, surface::INSPECT);
        redacted |= stdout.redacted;
        vr.stdout_preview = stdout.text;
        let stderr = redactor.redact_text(&vr.stderr_preview, surface::INSPECT);
        redacted |= stderr.redacted;
        vr.stderr_preview = stderr.text;
    }
    for message in &mut trajectory.messages {
        let outcome = redactor.redact_text(&message.content, surface::INSPECT);
        redacted |= outcome.redacted;
        message.content = outcome.text;
        if let Some(actions) = &mut message.extra.actions {
            for action in actions {
                let outcome = redactor.redact_text(action, surface::INSPECT);
                redacted |= outcome.redacted;
                *action = outcome.text;
            }
        }
        if let Some(response) = &mut message.extra.response {
            redacted |= redactor.redact_json_value(response, surface::INSPECT);
        }
        if let Some(sampling) = &mut message.extra.sampling {
            let mut extra_val = serde_json::Value::Object(sampling.extra.clone());
            let changed = redactor.redact_json_value(&mut extra_val, surface::INSPECT);
            redacted |= changed;
            if changed {
                sampling.extra = match extra_val {
                    serde_json::Value::Object(m) => m,
                    _ => serde_json::Map::new(),
                };
            }
        }
        for value in message.extra.other.values_mut() {
            redacted |= redactor.redact_json_value(value, surface::INSPECT);
        }
    }
    redacted
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::trajectory::{VerificationResult, verification_status};

    fn write_trajectory_fixture(
        sweep: &std::path::Path,
        instance_id: &str,
        results: &[VerificationResult],
        status: &str,
    ) {
        let instance_dir = sweep.join(instance_id);
        std::fs::create_dir_all(&instance_dir).unwrap();
        let traj = serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 1},
            "info": {
                "verification_status": status,
                "verification_results": serde_json::to_value(results).unwrap(),
            },
            "messages": [{"role": "assistant", "content": "done"}]
        });
        std::fs::write(
            instance_dir.join("trajectory.json"),
            serde_json::to_string(&traj).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn timed_out_check_renders_timeout_note() {
        let dir = tempfile::tempdir().unwrap();
        write_trajectory_fixture(
            dir.path(),
            "task-a",
            &[VerificationResult {
                name: "slow-check".into(),
                command: "sleep 60".into(),
                exit_code: -1,
                duration_ms: 5000,
                passed: false,
                stdout_preview: String::new(),
                stderr_preview: String::new(),
                timed_out: true,
            }],
            verification_status::VERIFICATION_FAILED,
        );
        let args = InspectArgs {
            sweep: dir.path().to_path_buf(),
            instance: Some("task-a".into()),
            filter: None,
            full: false,
            show_expected: false,
        };
        let output = run(&args).unwrap();
        let text = render_text(&output);
        assert!(
            text.contains("(timed_out)"),
            "expected timed_out note in:\n{text}"
        );
        assert!(
            text.contains("verification:"),
            "expected verification line in:\n{text}"
        );
        if let InspectOutput::Instance(report) = &output {
            assert_eq!(report.verification_results.len(), 1);
            assert!(report.verification_results[0].timed_out);
        } else {
            panic!("expected Instance output");
        }
    }

    #[test]
    fn verification_command_and_output_redacted_at_view_time() {
        let secret = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
        let dir = tempfile::tempdir().unwrap();
        write_trajectory_fixture(
            dir.path(),
            "task-b",
            &[VerificationResult {
                name: "secret-check".into(),
                command: format!("curl -H 'Authorization: Bearer {secret}'"),
                exit_code: 0,
                duration_ms: 50,
                passed: true,
                stdout_preview: format!("token={secret}"),
                stderr_preview: String::new(),
                timed_out: false,
            }],
            verification_status::VERIFIED,
        );
        let args = InspectArgs {
            sweep: dir.path().to_path_buf(),
            instance: Some("task-b".into()),
            filter: None,
            full: false,
            show_expected: false,
        };
        let output = run(&args).unwrap();
        if let InspectOutput::Instance(report) = &output {
            let vr = &report.verification_results[0];
            assert!(
                !vr.command.contains(secret),
                "command should be redacted at view time"
            );
            assert!(
                !vr.stdout_preview.contains(secret),
                "stdout_preview should be redacted at view time"
            );
        } else {
            panic!("expected Instance output");
        }
    }

    #[test]
    fn elided_step_renders_two_view() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("task-elided");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let run_result = serde_json::json!({
            "stdout": "full_observation_content",
            "stderr": "",
            "exit_code": 0,
            "timed_out": false
        });
        let traj = serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 1},
            "info": {},
            "messages": [
                {"role": "assistant", "content": "```bash\necho x\n```"},
                {
                    "role": "user",
                    "content": "full_observation_content",
                    "extra": {
                        "run_result": run_result,
                        "history_elided": true,
                        "history_elision_marker": "[history-elided: step 0 observation, 23 bytes]",
                        "history_bytes_elided": 23
                    }
                }
            ]
        });
        std::fs::write(
            instance_dir.join("trajectory.json"),
            serde_json::to_string(&traj).unwrap(),
        )
        .unwrap();

        let args = InspectArgs {
            sweep: dir.path().to_path_buf(),
            instance: Some("task-elided".into()),
            filter: None,
            full: false,
            show_expected: false,
        };
        let output = run(&args).unwrap();
        let text = render_text(&output);

        assert!(
            text.contains("[as-sent to model]"),
            "elided step should show as-sent view:\n{text}"
        );
        assert!(
            text.contains("[history-elided: step 0 observation"),
            "elided step should include marker text:\n{text}"
        );
        assert!(
            text.contains("[as-recorded stdout]"),
            "elided step should show as-recorded label:\n{text}"
        );
        assert!(
            text.contains("full_observation_content"),
            "elided step should show full recorded content:\n{text}"
        );

        if let InspectOutput::Instance(report) = &output {
            let elided_step = report.steps.iter().find(|s| s.history_elided);
            assert!(elided_step.is_some(), "report must contain an elided step");
            let step = elided_step.unwrap();
            assert!(
                step.as_sent_marker
                    .as_deref()
                    .unwrap_or("")
                    .contains("[history-elided:"),
                "as_sent_marker must contain the elision marker"
            );
        } else {
            panic!("expected Instance output");
        }
    }

    #[test]
    fn elided_step_without_marker_shows_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("task-no-marker");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let run_result = serde_json::json!({
            "stdout": "content",
            "stderr": "",
            "exit_code": 0,
            "timed_out": false
        });
        let traj = serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 1},
            "info": {},
            "messages": [
                {"role": "assistant", "content": "```bash\necho x\n```"},
                {
                    "role": "user",
                    "content": "content",
                    "extra": {
                        "run_result": run_result,
                        "history_elided": true
                    }
                }
            ]
        });
        std::fs::write(
            instance_dir.join("trajectory.json"),
            serde_json::to_string(&traj).unwrap(),
        )
        .unwrap();

        let args = InspectArgs {
            sweep: dir.path().to_path_buf(),
            instance: Some("task-no-marker".into()),
            filter: None,
            full: false,
            show_expected: false,
        };
        let output = run(&args).unwrap();
        let text = render_text(&output);
        assert!(
            text.contains("[elision marker unavailable]"),
            "should show fallback when marker is absent:\n{text}"
        );
    }

    #[test]
    fn build_failing_tests_falls_back_to_failure_category_when_eval_exit_reason_absent() {
        let eval_override = EvaluationOverride {
            resolved: false,
            patch_stats: None,
            tests_failed: vec![],
            eval_exit_reason: None,
        };
        let ft = build_failing_tests(
            Some(false),
            Some(&eval_override),
            Some(FailureCategory::WallclockTimeout),
        );
        let ft = ft.unwrap();
        assert_eq!(ft.source, "unavailable");
        assert_eq!(ft.reason, "wallclock_timeout");
    }

    #[test]
    fn build_failing_tests_returns_none_for_resolved() {
        let eval_override = EvaluationOverride {
            resolved: true,
            patch_stats: None,
            tests_failed: vec!["tests/test.py::test_foo".into()],
            eval_exit_reason: Some(crate::run::evaluate::EvalExitReason::Resolved),
        };
        let ft = build_failing_tests(Some(true), Some(&eval_override), None);
        assert!(ft.is_none(), "resolved instances must return None");
    }
}
