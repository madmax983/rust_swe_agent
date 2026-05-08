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
use crate::run::evaluate::EvaluationResults;
use crate::run::patch_stats::PatchStats;
use crate::run::swebench::{InstanceResult, ProvenanceManifest};
use crate::trajectory::{FailureCategory, TokenUsage, Trajectory};

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
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub steps: Vec<InspectStep>,
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
    let report = build_instance_report(
        &args.sweep,
        &instance_id,
        args.full,
        evaluation_overrides.as_ref(),
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
    Ok(SummaryReport {
        sweep_dir: sweep.to_path_buf(),
        filter: filter.raw,
        manifest: loaded.manifest,
        rows,
    })
}

#[allow(clippy::too_many_lines)]
fn build_instance_report(
    sweep: &Path,
    instance_id: &str,
    full: bool,
    evaluation_overrides: Option<&HashMap<String, EvaluationOverride>>,
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
                resolved: evaluation_overrides
                    .and_then(|m| m.get(instance_id))
                    .map(|value| value.resolved),
                patch_stats: evaluation_overrides
                    .and_then(|m| m.get(instance_id))
                    .and_then(|value| value.patch_stats.clone()),
                test_invocations_count: 0,
                last_test_exit_code: None,
                tests_run_before_submit: false,
                last_tests_passed: None,
                warnings,
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

    let token_usage = traj.info.token_usage.as_ref();
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
        resolved: evaluation_overrides
            .and_then(|m| m.get(instance_id))
            .map(|value| value.resolved),
        patch_stats: evaluation_overrides
            .and_then(|m| m.get(instance_id))
            .and_then(|value| value.patch_stats.clone()),
        test_invocations_count: traj.info.test_invocations.len(),
        last_test_exit_code: traj
            .info
            .test_invocations
            .last()
            .map(|invocation| invocation.exit_code),
        tests_run_before_submit: traj.info.tests_run_before_submit,
        last_tests_passed: traj.info.last_tests_passed,
        warnings,
        steps,
    })
}

fn build_inspect_steps(traj: &Trajectory, full: bool) -> Vec<InspectStep> {
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
        let (stdout, stdout_note, stdout_truncated) =
            maybe_truncate(&run_result.stdout, full, current_index);
        let (stderr, stderr_note, stderr_truncated) =
            maybe_truncate(&run_result.stderr, full, current_index);
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
    for w in &report.warnings {
        let _ = writeln!(s, "warning:          {w}");
    }

    for step in &report.steps {
        let header = format!("[step {}] {}", step.index, step.role);
        if color {
            let _ = writeln!(s, "\n\x1b[1;36m{header}\x1b[0m");
        } else {
            let _ = writeln!(s, "\n{header}");
        }

        if let Some(msg) = &step.message {
            s.push_str(msg);
            if !msg.ends_with('\n') {
                s.push('\n');
            }
            continue;
        }

        if let Some(cmd) = &step.bash {
            let _ = writeln!(s, "$ {cmd}");
        }
        if let Some(code) = step.exit_code {
            let _ = writeln!(s, "exit_code: {code}");
        }
        if let Some(out) = &step.stdout {
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

fn maybe_truncate(text: &str, full: bool, step_index: usize) -> (String, Option<String>, bool) {
    if full {
        return (text.to_owned(), None, false);
    }
    let line_count = text.lines().count();
    if text.len() <= TRUNCATE_MAX_BYTES && line_count <= TRUNCATE_MAX_LINES {
        return (text.to_owned(), None, false);
    }

    let mut truncated = String::new();
    let mut consumed_bytes = 0usize;
    let mut shown_lines = 0usize;
    for line in text.split_inclusive('\n') {
        if shown_lines >= TRUNCATE_MAX_LINES || consumed_bytes >= TRUNCATE_MAX_BYTES {
            break;
        }
        let remaining = TRUNCATE_MAX_BYTES - consumed_bytes;
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

fn resolve_trajectory_path(sweep: &Path, instance_id: &str) -> Option<PathBuf> {
    let nested = sweep.join(instance_id).join("trajectory.json");
    if nested.exists() {
        return Some(nested);
    }
    let nested_run = sweep.join(instance_id).join("run-1.traj.json");
    if nested_run.exists() {
        return Some(nested_run);
    }
    let flat = sweep.join(format!("{instance_id}.traj.json"));
    flat.exists().then_some(flat)
}

#[derive(Debug, Clone)]
struct EvaluationOverride {
    resolved: bool,
    patch_stats: Option<PatchStats>,
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
                    },
                )
            })
            .collect(),
    ))
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
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::Unknown => "unknown",
    }
}

fn redact_trajectory_for_inspect(trajectory: &mut Trajectory, redactor: &Redactor) -> bool {
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
        for value in message.extra.other.values_mut() {
            redacted |= redactor.redact_json_value(value, surface::INSPECT);
        }
    }
    redacted
}
