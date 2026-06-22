//! `bench failure-digest`: self-contained failure summary for one instance in a sweep.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::env::RunResult;
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::compare::load_sweep;
use crate::run::swebench::InstanceResult;
use crate::run::triage::{
    FailureSignature, TriageReport, failure_label, load_trajectory, resolve_trajectory_path,
};
use crate::trajectory::FailureCategory;

/// Characters per section excerpt (1.5 KB).
const EXCERPT_MAX_CHARS: usize = 1536;

/// Arguments for `bench failure-digest`.
#[derive(Debug, Clone)]
pub struct FailureDigestArgs {
    pub sweep_dir: PathBuf,
    pub instance: Option<String>,
    pub format: DigestFormat,
    pub max_chars: usize,
    /// Optional baseline `signature_id` to compare against. When present the
    /// digest carries a recurrence verdict (`recurring` / `new`). The verdict is
    /// informational and never changes the process exit code.
    pub baseline_signature: Option<String>,
}

/// Output format for the digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestFormat {
    Markdown,
    Json,
}

/// Patch capture status for an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchStatus {
    Applied,
    EmptyDiff,
    InvalidDiff,
    NotAttempted,
}

impl PatchStatus {
    fn label(&self) -> &str {
        match self {
            Self::Applied => "applied",
            Self::EmptyDiff => "empty-diff",
            Self::InvalidDiff => "invalid-diff",
            Self::NotAttempted => "not-attempted",
        }
    }
}

/// The structured failure digest (stable JSON schema version 1.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureDigest {
    pub schema_version: String,
    pub instance_id: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_duration_secs: Option<f64>,
    pub last_assistant_message: String,
    pub last_tool_stderr: String,
    pub last_tool_stdout: String,
    pub patch_status: PatchStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_apply_stderr: Option<String>,
    pub triage_cluster_label: Option<String>,
    pub redacted: bool,
    /// Stable, redaction-safe failure signature for deterministic dedup.
    pub failure_signature: FailureSignatureView,
    /// Recurrence verdict against an optional baseline signature. Present only
    /// when `--baseline-signature` is supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<RecurrenceView>,
}

/// Serializable view of a [`FailureSignature`]: a stable `signature_id` plus the
/// constituent fields it is derived from and a short human `summary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureSignatureView {
    pub signature_id: String,
    pub failure_category: String,
    pub assistant_tail: String,
    pub bash_exit_code: Option<i32>,
    pub stderr_line: String,
    pub summary: String,
}

/// Recurrence verdict: does the current failure match a known baseline signature?
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurrenceView {
    pub baseline_signature_id: String,
    /// `"recurring"` when the current `signature_id` matches the baseline,
    /// otherwise `"new"`.
    pub verdict: String,
}

pub fn run(args: &FailureDigestArgs) -> Result<FailureDigest, Error> {
    let results_path = args.sweep_dir.join("results.json");
    if !results_path.exists() {
        return Err(Error::Trajectory(format!(
            "failure-digest: results.json not found in `{}`; run `bench swebench` first",
            args.sweep_dir.display()
        )));
    }

    let sweep = load_sweep(&args.sweep_dir)?;
    let instance = resolve_instance(args, &sweep.instances)?;
    let instance_id = instance.instance_id.clone();

    let traj_path = resolve_terminal_trajectory_path(&args.sweep_dir, &instance_id);
    let (last_assistant_message, last_tool_stderr, last_tool_stdout, bash_exit_code) =
        if let Some(ref path) = traj_path {
            extract_terminal_signals(path)?
        } else {
            (String::new(), String::new(), String::new(), None)
        };

    let patch_status = derive_patch_status(instance);
    let patch_apply_stderr_raw = if patch_status == PatchStatus::InvalidDiff {
        instance.error.clone()
    } else {
        None
    };

    let triage_cluster_label_raw = load_triage_cluster_label(&args.sweep_dir, &instance_id);

    let redactor = Redactor::default_enabled();
    let ast_out = redactor.redact_text(&last_assistant_message, surface::INSPECT);
    let stderr_out = redactor.redact_text(&last_tool_stderr, surface::INSPECT);
    let stdout_out = redactor.redact_text(&last_tool_stdout, surface::INSPECT);
    let patch_err_out = patch_apply_stderr_raw
        .as_deref()
        .map(|s| redactor.redact_text(s, surface::INSPECT));
    let triage_out = triage_cluster_label_raw
        .as_deref()
        .map(|s| redactor.redact_text(s, surface::INSPECT));

    let last_assistant_message = ast_out.text;
    let last_tool_stderr = stderr_out.text;
    let last_tool_stdout = stdout_out.text;
    let patch_apply_stderr = patch_err_out.as_ref().map(|o| o.text.clone());
    let triage_cluster_label = triage_out.as_ref().map(|o| o.text.clone());
    let redacted = ast_out.redacted
        || stderr_out.redacted
        || stdout_out.redacted
        || patch_err_out.as_ref().is_some_and(|o| o.redacted)
        || triage_out.as_ref().is_some_and(|o| o.redacted);

    let leak_in = |text: &str| redactor.configured_literal_leak(text).is_some();
    if leak_in(&last_assistant_message)
        || leak_in(&last_tool_stderr)
        || leak_in(&last_tool_stdout)
        || patch_apply_stderr.as_deref().is_some_and(leak_in)
        || triage_cluster_label.as_deref().is_some_and(leak_in)
    {
        return Err(Error::Trajectory(
            "failure-digest: redaction failure — configured secret literal present in digest output"
                .into(),
        ));
    }

    let failure_category = instance
        .failure_category
        .map(|c| failure_label(c).to_owned());

    // Compute the failure signature from the already-redacted terminal fields so
    // no raw secret material can leak into `signature_id` or `summary`. Marker
    // salts are normalized (consistent with `fingerprint::normalize_redaction_markers`)
    // so the signature is stable across record/replay.
    let failure_signature = build_failure_signature(
        failure_category.as_deref(),
        &last_assistant_message,
        bash_exit_code,
        &last_tool_stderr,
    );

    let recurrence = args.baseline_signature.as_ref().map(|baseline| {
        let verdict = if *baseline == failure_signature.signature_id {
            "recurring"
        } else {
            "new"
        };
        RecurrenceView {
            baseline_signature_id: baseline.clone(),
            verdict: verdict.to_owned(),
        }
    });

    Ok(FailureDigest {
        schema_version: "1.1".into(),
        instance_id,
        outcome: instance.outcome.clone().unwrap_or_else(|| "unknown".into()),
        failure_category,
        total_cost_usd: instance.actual_cost_usd(),
        step_count: instance.steps,
        task_duration_secs: instance.duration_secs,
        last_assistant_message,
        last_tool_stderr,
        last_tool_stdout,
        patch_status,
        patch_apply_stderr,
        triage_cluster_label,
        redacted,
        failure_signature,
        recurrence,
    })
}

/// Build a redaction-safe [`FailureSignatureView`] from already-redacted terminal
/// fields. Redaction markers are salt-normalized before hashing so the
/// `signature_id` is identical across record/replay of the same failure.
fn build_failure_signature(
    failure_category: Option<&str>,
    redacted_assistant: &str,
    bash_exit_code: Option<i32>,
    redacted_stderr: &str,
) -> FailureSignatureView {
    let category = failure_category.unwrap_or("none");
    let assistant = crate::fingerprint::normalize_redaction_markers(redacted_assistant);
    let stderr_norm = crate::fingerprint::normalize_redaction_markers(redacted_stderr);
    let stderr_line = stderr_norm
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();

    let signature = FailureSignature::from_parts(category, &assistant, bash_exit_code, stderr_line);

    FailureSignatureView {
        signature_id: signature.signature_id(),
        failure_category: signature.failure_category().to_owned(),
        assistant_tail: signature.assistant_tail().to_owned(),
        bash_exit_code: signature.bash_exit_code(),
        stderr_line: signature.stderr_line().to_owned(),
        summary: signature.summary(),
    }
}

/// Render the digest as markdown, truncating to `max_chars` while preserving
/// the headline and the triage cluster footer.
pub fn render_markdown(digest: &FailureDigest, max_chars: usize) -> String {
    let headline = build_headline(digest);
    let cost_line = build_cost_line(digest);
    let signature_lines = build_signature_lines(digest);
    let triage_label = digest
        .triage_cluster_label
        .as_deref()
        .unwrap_or("no triage available.");

    let is_submitted = digest.outcome == "submitted" && digest.failure_category.is_none();

    if is_submitted {
        let mut out = String::new();
        let _ = writeln!(out, "{headline}");
        let _ = write!(out, "{signature_lines}");
        let _ = writeln!(out);
        let _ = writeln!(out, "{cost_line}");
        let _ = writeln!(out);
        let _ = writeln!(out, "resolved, no failure to digest");
        let _ = writeln!(out);
        let _ = writeln!(out, "### Triage cluster");
        let _ = writeln!(out);
        out.push_str(triage_label);
        return truncate_preserving_ends(&out, max_chars, &headline, triage_label);
    }

    let ast_excerpt = excerpt_head_tail(&digest.last_assistant_message, EXCERPT_MAX_CHARS);
    let stderr_excerpt = excerpt_head_tail(&digest.last_tool_stderr, EXCERPT_MAX_CHARS);
    let stdout_excerpt = excerpt_head_tail(&digest.last_tool_stdout, EXCERPT_MAX_CHARS);
    let patch_section = build_patch_section(digest);

    let mut out = String::new();
    let _ = writeln!(out, "{headline}");
    let _ = write!(out, "{signature_lines}");
    let _ = writeln!(out);
    let _ = writeln!(out, "{cost_line}");
    let _ = writeln!(out);
    let _ = writeln!(out, "### Last assistant message");
    let _ = writeln!(out);
    if ast_excerpt.is_empty() {
        let _ = writeln!(out, "_(no assistant message recorded)_");
    } else {
        let _ = writeln!(out, "{ast_excerpt}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "### Last tool stderr");
    let _ = writeln!(out);
    if stderr_excerpt.is_empty() {
        let _ = writeln!(out, "_(no tool stderr recorded)_");
    } else {
        let _ = writeln!(out, "{stderr_excerpt}");
    }
    if !stdout_excerpt.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "### Last tool stdout");
        let _ = writeln!(out);
        let _ = writeln!(out, "{stdout_excerpt}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "### Patch status");
    let _ = writeln!(out);
    let _ = writeln!(out, "{patch_section}");
    let _ = writeln!(out);
    let _ = writeln!(out, "### Triage cluster");
    let _ = writeln!(out);
    out.push_str(triage_label);

    truncate_preserving_ends(&out, max_chars, &headline, triage_label)
}

fn build_headline(digest: &FailureDigest) -> String {
    format!(
        "**{}** | outcome: {} | failure_category: {}",
        digest.instance_id,
        digest.outcome,
        digest.failure_category.as_deref().unwrap_or("none")
    )
}

/// Stable, greppable signature line(s) emitted directly under the headline so
/// CI logs and humans can match a failure deterministically. When a baseline was
/// supplied, a `recurrence: ` line follows.
fn build_signature_lines(digest: &FailureDigest) -> String {
    let mut s = format!(
        "failure_signature: {}\n",
        digest.failure_signature.signature_id
    );
    if let Some(ref recurrence) = digest.recurrence {
        let _ = writeln!(s, "recurrence: {}", recurrence.verdict);
    }
    s
}

fn build_cost_line(digest: &FailureDigest) -> String {
    format!(
        "cost: {} | steps: {} | duration: {}s",
        digest
            .total_cost_usd
            .map_or_else(|| "n/a".into(), |c| format!("${c:.2}")),
        digest
            .step_count
            .map_or_else(|| "n/a".into(), |s| s.to_string()),
        digest
            .task_duration_secs
            .map_or_else(|| "n/a".into(), |d| format!("{d:.2}"))
    )
}

fn build_patch_section(digest: &FailureDigest) -> String {
    let mut s = digest.patch_status.label().to_owned();
    if let Some(ref stderr) = digest.patch_apply_stderr {
        if !stderr.is_empty() {
            let _ = write!(s, "\n\n```\n{stderr}\n```");
        }
    }
    s
}

fn resolve_instance<'a>(
    args: &FailureDigestArgs,
    instances: &'a HashMap<String, InstanceResult>,
) -> Result<&'a InstanceResult, Error> {
    if let Some(ref id) = args.instance {
        instances.get(id).ok_or_else(|| {
            Error::Trajectory(format!(
                "failure-digest: instance `{id}` not found in sweep `{}`",
                args.sweep_dir.display()
            ))
        })
    } else {
        match instances.len() {
            0 => Err(Error::Trajectory(
                "failure-digest: sweep contains no instances".into(),
            )),
            1 => Ok(instances.values().next().ok_or_else(|| {
                Error::Trajectory("failure-digest: sweep contains no instances".into())
            })?),
            _ => {
                let mut ids: Vec<&str> = instances.keys().map(String::as_str).collect();
                ids.sort_unstable();
                Err(Error::Trajectory(format!(
                    "failure-digest: sweep contains {} instances; use --instance to select one. Candidates: {}",
                    ids.len(),
                    ids.join(", ")
                )))
            }
        }
    }
}

/// Like `resolve_trajectory_path` but prefers the highest-numbered `run-N.traj.json`
/// so multi-retry sweeps report the terminal attempt rather than the first.
fn resolve_terminal_trajectory_path(sweep_dir: &Path, instance_id: &str) -> Option<PathBuf> {
    let instance_dir = sweep_dir.join(instance_id);
    if instance_dir.is_dir() {
        let mut best: Option<(u32, PathBuf)> = None;
        if let Ok(entries) = std::fs::read_dir(&instance_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let n = name
                    .to_string_lossy()
                    .strip_prefix("run-")
                    .and_then(|s| s.strip_suffix(".traj.json"))
                    .and_then(|s| s.parse::<u32>().ok());
                if let Some(n) = n {
                    if best.as_ref().is_none_or(|(bn, _)| n > *bn) {
                        best = Some((n, entry.path()));
                    }
                }
            }
        }
        if let Some((_, path)) = best {
            return Some(path);
        }
        let trajectory = instance_dir.join("trajectory.json");
        if trajectory.exists() {
            return Some(trajectory);
        }
    }
    // Flat and bundled layouts have no retry semantics; delegate to shared helper.
    resolve_trajectory_path(sweep_dir, instance_id)
}

fn extract_terminal_signals(path: &Path) -> Result<(String, String, String, Option<i32>), Error> {
    let trajectory = load_trajectory(path)?;

    let last_assistant = trajectory
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map_or_else(String::new, |m| m.content.clone());

    let last_run = trajectory.messages.iter().rev().find_map(|m| {
        if m.role != "user" {
            return None;
        }
        m.extra
            .other
            .get("run_result")
            .and_then(|v| serde_json::from_value::<RunResult>(v.clone()).ok())
    });

    let (stderr, stdout, exit_code) = last_run.map_or((String::new(), String::new(), None), |r| {
        (r.stderr, r.stdout, Some(r.exit_code))
    });

    Ok((last_assistant, stderr, stdout, exit_code))
}

fn derive_patch_status(instance: &InstanceResult) -> PatchStatus {
    if instance.failure_category == Some(FailureCategory::PatchApplyInvalid) {
        return PatchStatus::InvalidDiff;
    }
    if !instance.patch_present {
        return PatchStatus::NotAttempted;
    }
    if instance.non_empty_patch {
        PatchStatus::Applied
    } else {
        PatchStatus::EmptyDiff
    }
}

fn load_triage_cluster_label(sweep_dir: &Path, instance_id: &str) -> Option<String> {
    let triage_path = sweep_dir.join("triage.json");
    if !triage_path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&triage_path).ok()?;
    let report: TriageReport = serde_json::from_str(&text).ok()?;
    report.clusters.iter().find_map(|cluster| {
        cluster
            .instance_ids
            .iter()
            .any(|id| id == instance_id)
            .then(|| {
                format!(
                    "{} [{}]: {}",
                    cluster.failure_category, cluster.cluster_id, cluster.signature_summary
                )
            })
    })
}

/// Truncate markdown to `max_chars` while preserving the first line (headline)
/// and the triage cluster footer.
fn truncate_preserving_ends(
    text: &str,
    max_chars: usize,
    headline: &str,
    triage_label: &str,
) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }

    let footer_section = format!("\n\n### Triage cluster\n\n{triage_label}");
    let header = format!("{headline}\n");
    let marker = "\n\n... [digest truncated for length] ...\n";

    let fixed_len =
        header.chars().count() + marker.chars().count() + footer_section.chars().count();

    if fixed_len >= max_chars {
        // Budget too small for header+marker+footer: hard-truncate the full text.
        return text.chars().take(max_chars).collect();
    }

    let middle_budget = max_chars - fixed_len;
    let rest = text
        .split_once('\n')
        .map_or("", |x| x.1)
        .trim_start_matches('\n');
    let middle: String = rest.chars().take(middle_budget).collect();

    format!("{header}{middle}{marker}{footer_section}")
}

/// Extract a head+tail excerpt, with middle elision if text exceeds `max_chars`.
fn excerpt_head_tail(text: &str, max_chars: usize) -> String {
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_owned();
    }
    let head_len = max_chars / 2;
    let tail_len = max_chars - head_len;
    let head: String = text.chars().take(head_len).collect();
    let elided = len - max_chars;
    let tail_start = len - tail_len;
    let tail: String = text.chars().skip(tail_start).collect();
    format!("{head}\n... [{elided} chars elided] ...\n{tail}")
}
