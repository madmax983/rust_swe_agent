//! `bench inspect`: inspect one trajectory or list filtered instances.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::env::RunResult;
use crate::error::Error;
use crate::run::evaluate::EvaluationResults;
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
    let resolved_map = load_resolved_overrides(&args.sweep)?;
    let report =
        build_instance_report(&args.sweep, &instance_id, args.full, resolved_map.as_ref())?;
    Ok(InspectOutput::Instance(Box::new(report)))
}

fn build_summary(sweep: &Path, filter: &str) -> Result<SummaryReport, Error> {
    let filter = parse_filter(filter)?;
    let mut rows: Vec<SummaryRow> = Vec::new();
    let loaded = crate::run::compare::load_sweep(sweep)?;
    let resolved = load_resolved_overrides(sweep)?.unwrap_or_default();
    for r in loaded.instances.values() {
        let res = resolved.get(&r.instance_id).copied();
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

fn build_instance_report(
    sweep: &Path,
    instance_id: &str,
    full: bool,
    resolved_map: Option<&HashMap<String, bool>>,
) -> Result<InspectReport, Error> {
    let traj_path = resolve_trajectory_path(sweep, instance_id).ok_or_else(|| {
        Error::Trajectory(format!(
            "inspect: trajectory not found for instance `{instance_id}` in {}",
            sweep.display()
        ))
    })?;
    let text = std::fs::read_to_string(&traj_path)?;
    let mut warnings = Vec::new();
    let traj: Trajectory = match serde_json::from_str(&text) {
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
                prompt_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                completion_tokens: None,
                resolved: resolved_map.and_then(|m| m.get(instance_id).copied()),
                warnings,
                steps: vec![],
            });
        }
    };

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

        if role == "user" {
            if let Some(run_result) = msg
                .extra
                .other
                .get("run_result")
                .and_then(|v| serde_json::from_value::<RunResult>(v.clone()).ok())
            {
                let bash = infer_bash_from_previous_assistant(&traj, msg_idx);
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
        }
    }

    let token_usage = traj.info.token_usage.as_ref();
    Ok(InspectReport {
        sweep_dir: sweep.to_path_buf(),
        instance_id: Some(instance_id.to_owned()),
        model: traj.info.model_name,
        outcome: traj.info.outcome,
        failure_category: traj.info.failure_category,
        total_cost_usd: traj.info.total_cost_usd,
        prompt_tokens: token_usage.map(TokenUsage::total_prompt_tokens),
        input_tokens: token_usage.map(|t| t.prompt_tokens),
        cache_read_tokens: token_usage.map(|t| t.cache_read_tokens),
        cache_creation_tokens: token_usage.map(|t| t.cache_creation_tokens),
        completion_tokens: token_usage.map(|t| t.completion_tokens),
        resolved: resolved_map.and_then(|m| m.get(instance_id).copied()),
        warnings,
        steps,
    })
}

pub fn render_text(output: &InspectOutput) -> String {
    match output {
        InspectOutput::Instance(r) => render_instance_text(r),
        InspectOutput::Summary(s) => render_summary_text(s),
    }
}

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
    s.push_str("\ninstance_id | outcome | failure_category | cost_usd | resolved\n");
    s.push_str("----------------------------------------------------------------\n");
    for row in &report.rows {
        let _ = writeln!(
            s,
            "{} | {} | {} | {} | {}",
            row.instance_id,
            row.outcome.as_deref().unwrap_or("?"),
            row.failure_category.map_or("none", failure_label),
            row.cost_usd
                .map_or_else(|| "?".into(), |c| format!("{c:.4}")),
            row.resolved
                .map_or("?", |v| if v { "true" } else { "false" })
        );
    }
    s
}

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

fn load_resolved_overrides(dir: &Path) -> Result<Option<HashMap<String, bool>>, Error> {
    let eval_path = crate::run::evaluate::evaluation_path(dir);
    if !eval_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(eval_path)?;
    let eval: EvaluationResults = serde_json::from_str(&text)?;
    Ok(Some(
        eval.instances
            .into_iter()
            .map(|x| (x.instance_id, x.resolved))
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
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::Unknown => "unknown",
    }
}
