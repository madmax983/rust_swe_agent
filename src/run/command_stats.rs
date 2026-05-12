//! `bench command-stats`: surface shell-command behavior by outcome bucket.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::env::RunResult;
use crate::error::Error;
use crate::run::compare::{load_evaluation_results_checked, load_sweep};
use crate::run::swebench::InstanceResult;
use crate::trajectory::{FailureCategory, Trajectory};

#[derive(Debug, Clone)]
pub struct CommandStatsArgs {
    pub sweep_dir: PathBuf,
    pub bucket: Option<String>,
    pub min_invocations: usize,
    pub top: usize,
    pub compare: Option<String>,
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandStatRow {
    pub command_head: String,
    pub instance_count: usize,
    pub invocation_count: usize,
    pub mean_calls_per_instance: f64,
    pub nonzero_exit_rate: f64,
    pub attributed_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandStatsTotals {
    pub trajectories: usize,
    pub bash_steps: usize,
    pub unique_command_heads: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandStatsDelta {
    pub command_head: String,
    pub resolved_share: f64,
    pub unresolved_share: f64,
    pub delta: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandStatsComparison {
    pub name: String,
    pub rows: Vec<CommandStatsDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandStatsReport {
    pub sweep: String,
    pub generated_at: String,
    pub totals: CommandStatsTotals,
    pub by_outcome: BTreeMap<String, Vec<CommandStatRow>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comparisons: Vec<CommandStatsComparison>,
}

pub fn run(args: &CommandStatsArgs) -> Result<CommandStatsReport, Error> {
    let mut report = build_report(args)?;
    report.generated_at = utc_now_iso8601();
    let output_path = args.sweep_dir.join("command-stats.json");
    let file = std::fs::File::create(output_path)?;
    serde_json::to_writer_pretty(file, &report)?;
    Ok(report)
}

pub fn render_text(report: &CommandStatsReport, top: usize) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let mut out = String::new();
    out.push_str("\n=== bench command-stats ===\n");
    let _ = writeln!(out, "Sweep: {}", report.sweep);
    let _ = writeln!(
        out,
        "Trajectories: {}  bash_steps: {}  unique_command_heads: {}",
        report.totals.trajectories,
        report.totals.bash_steps,
        report.totals.unique_command_heads
    );
    out.push('\n');

    for (bucket, rows) in &report.by_outcome {
        let _ = writeln!(out, "--- Outcome: {bucket} ---");
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "rank",
                "command_head",
                "instance_count",
                "invocation_count",
                "mean_calls/instance",
                "nonzero_exit_rate",
                "attributed_cost_usd",
            ]);

        for (idx, row) in rows.iter().take(top).enumerate() {
            table.add_row(vec![
                (idx + 1).to_string(),
                row.command_head.clone(),
                row.instance_count.to_string(),
                row.invocation_count.to_string(),
                format!("{:.2}", row.mean_calls_per_instance),
                format!("{:.3}", row.nonzero_exit_rate),
                format!("{:.6}", row.attributed_cost_usd),
            ]);
        }

        out.push_str(&table.to_string());
        out.push('\n');
    }

    for comparison in &report.comparisons {
        let _ = writeln!(out, "--- Comparison: {} ---", comparison.name);
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "command_head",
                "resolved_share",
                "unresolved_share",
                "delta",
            ]);

        for row in &comparison.rows {
            table.add_row(vec![
                row.command_head.clone(),
                format!("{:.4}", row.resolved_share),
                format!("{:.4}", row.unresolved_share),
                format!("{:.4}", row.delta),
            ]);
        }

        out.push_str(&table.to_string());
        out.push('\n');
    }

    out
}

fn build_report(args: &CommandStatsArgs) -> Result<CommandStatsReport, Error> {
    let sweep = load_sweep(&args.sweep_dir)?;
    let evaluation = load_evaluation_results_checked(&args.sweep_dir)?;

    // Build resolved-instance set from evaluation (keyed by instance_id)
    let resolved_set: HashSet<String> = evaluation
        .as_ref()
        .map(|ev| {
            ev.results
                .instances
                .iter()
                .filter(|i| i.resolved)
                .map(|i| i.instance_id.clone())
                .collect()
        })
        .unwrap_or_default();

    // Classify each instance by outcome bucket, applying the optional filter
    let mut instance_buckets: Vec<(String, OutcomeBucket)> = Vec::new();
    let mut sorted_ids: Vec<String> = sweep.instances.keys().cloned().collect();
    sorted_ids.sort();
    for id in sorted_ids {
        let instance = &sweep.instances[&id];
        let is_resolved = resolved_set.contains(&id);
        if let Some(filter) = &args.filter {
            if !matches_filter(instance, Some(is_resolved), filter)? {
                continue;
            }
        }
        let bucket = classify_outcome(&id, instance, &resolved_set);
        instance_buckets.push((id, bucket));
    }

    // Collect bash steps from all trajectories
    let mut all_steps: Vec<BashStep> = Vec::new();
    for (instance_id, bucket) in &instance_buckets {
        let instance = &sweep.instances[instance_id];
        let Some(trajectory_path) = resolve_trajectory_path(&args.sweep_dir, instance_id) else {
            continue;
        };
        let Ok(trajectory) = load_trajectory(&trajectory_path) else {
            continue;
        };
        let steps = extract_steps_from_trajectory(&trajectory, instance_id, bucket, instance);
        all_steps.extend(steps);
    }

    // Bucket filter: when set, restrict which steps contribute to each outcome bucket
    let active_bucket = args.bucket.as_deref();

    // Aggregate rows per outcome bucket
    let bucket_names = ["resolved", "unresolved", "errored", "all"];
    let mut by_outcome: BTreeMap<String, Vec<CommandStatRow>> = BTreeMap::new();
    for &bucket_name in &bucket_names {
        // Skip this bucket entirely when filtering to a different bucket (non-"all" filter)
        let step_subset: Vec<&BashStep> = all_steps
            .iter()
            .filter(|s| {
                let in_bucket = bucket_name == "all" || s.bucket == bucket_name;
                let in_filter = active_bucket.map_or(true, |fb| {
                    fb == "all" || fb == bucket_name || fb == s.bucket
                });
                in_bucket && in_filter
            })
            .collect();

        let mut rows = aggregate_rows(step_subset.iter().copied());
        rows.retain(|r| r.invocation_count >= args.min_invocations);
        rows.sort_by(|a, b| {
            b.invocation_count
                .cmp(&a.invocation_count)
                .then_with(|| a.command_head.cmp(&b.command_head))
        });
        by_outcome.insert(bucket_name.to_owned(), rows);
    }

    // Totals over all steps (unfiltered by bucket, but filtered by instance filter)
    let total_bash_steps = all_steps.len();
    let unique_heads: HashSet<&str> = all_steps.iter().map(|s| s.command_head.as_str()).collect();
    let unique_command_heads = unique_heads.len();
    let trajectories = instance_buckets.len();

    let totals = CommandStatsTotals {
        trajectories,
        bash_steps: total_bash_steps,
        unique_command_heads,
    };

    let comparisons = build_comparisons(args, &by_outcome);

    Ok(CommandStatsReport {
        sweep: args.sweep_dir.display().to_string(),
        generated_at: String::new(),
        totals,
        by_outcome,
        comparisons,
    })
}

fn build_comparisons(
    args: &CommandStatsArgs,
    by_outcome: &BTreeMap<String, Vec<CommandStatRow>>,
) -> Vec<CommandStatsComparison> {
    let Some(compare) = &args.compare else {
        return Vec::new();
    };
    if compare != "resolved-vs-unresolved" {
        return Vec::new();
    }
    let Some(resolved_rows) = by_outcome.get("resolved") else {
        return Vec::new();
    };
    let Some(unresolved_rows) = by_outcome.get("unresolved") else {
        return Vec::new();
    };
    vec![build_resolved_vs_unresolved(resolved_rows, unresolved_rows)]
}

fn build_resolved_vs_unresolved(
    resolved_rows: &[CommandStatRow],
    unresolved_rows: &[CommandStatRow],
) -> CommandStatsComparison {
    let resolved_total: usize = resolved_rows.iter().map(|r| r.invocation_count).sum();
    let unresolved_total: usize = unresolved_rows.iter().map(|r| r.invocation_count).sum();

    let all_heads: BTreeSet<&str> = resolved_rows
        .iter()
        .map(|r| r.command_head.as_str())
        .chain(unresolved_rows.iter().map(|r| r.command_head.as_str()))
        .collect();

    let mut deltas: Vec<CommandStatsDelta> = all_heads
        .iter()
        .map(|&head| {
            let r_count = resolved_rows
                .iter()
                .find(|r| r.command_head == head)
                .map_or(0, |r| r.invocation_count);
            let u_count = unresolved_rows
                .iter()
                .find(|r| r.command_head == head)
                .map_or(0, |r| r.invocation_count);
            #[allow(clippy::cast_precision_loss)]
            let r_share = if resolved_total > 0 {
                r_count as f64 / resolved_total as f64
            } else {
                0.0
            };
            #[allow(clippy::cast_precision_loss)]
            let u_share = if unresolved_total > 0 {
                u_count as f64 / unresolved_total as f64
            } else {
                0.0
            };
            CommandStatsDelta {
                command_head: head.to_owned(),
                resolved_share: r_share,
                unresolved_share: u_share,
                delta: r_share - u_share,
            }
        })
        .collect();

    deltas.sort_by(|a, b| {
        b.delta
            .total_cmp(&a.delta)
            .then_with(|| a.command_head.cmp(&b.command_head))
    });

    CommandStatsComparison {
        name: "resolved-vs-unresolved".to_owned(),
        rows: deltas,
    }
}

// ── internal types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BashStep {
    command_head: String,
    instance_id: String,
    bucket: String,
    exit_code: Option<i32>,
    cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OutcomeBucket {
    Resolved,
    Unresolved,
    Errored,
}

impl OutcomeBucket {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Unresolved => "unresolved",
            Self::Errored => "errored",
        }
    }
}

// ── aggregation ───────────────────────────────────────────────────────────────

fn aggregate_rows<'a, I>(steps: I) -> Vec<CommandStatRow>
where
    I: Iterator<Item = &'a BashStep>,
{
    let mut by_head: BTreeMap<String, Vec<&'a BashStep>> = BTreeMap::new();
    for step in steps {
        by_head
            .entry(step.command_head.clone())
            .or_default()
            .push(step);
    }

    by_head
        .into_iter()
        .map(|(head, steps)| {
            let invocation_count = steps.len();
            let unique_instances: HashSet<&str> =
                steps.iter().map(|s| s.instance_id.as_str()).collect();
            let instance_count = unique_instances.len();
            #[allow(clippy::cast_precision_loss)]
            let mean_calls_per_instance = invocation_count as f64 / instance_count as f64;
            let nonzero_exits = steps
                .iter()
                .filter(|s| s.exit_code.is_some_and(|c| c != 0))
                .count();
            let exits_known = steps.iter().filter(|s| s.exit_code.is_some()).count();
            #[allow(clippy::cast_precision_loss)]
            let nonzero_exit_rate = if exits_known > 0 {
                nonzero_exits as f64 / exits_known as f64
            } else {
                0.0
            };
            let attributed_cost_usd: f64 = steps.iter().map(|s| s.cost_usd).sum();
            CommandStatRow {
                command_head: head,
                instance_count,
                invocation_count,
                mean_calls_per_instance,
                nonzero_exit_rate,
                attributed_cost_usd,
            }
        })
        .collect()
}

// ── classification ────────────────────────────────────────────────────────────

fn classify_outcome(
    instance_id: &str,
    instance: &InstanceResult,
    resolved_set: &HashSet<String>,
) -> OutcomeBucket {
    if resolved_set.contains(instance_id) {
        return OutcomeBucket::Resolved;
    }
    if instance.outcome.as_deref() == Some(crate::trajectory::outcome::ERROR) {
        return OutcomeBucket::Errored;
    }
    OutcomeBucket::Unresolved
}

fn matches_filter(
    instance: &InstanceResult,
    resolved: Option<bool>,
    filter: &str,
) -> Result<bool, Error> {
    let Some((key, value)) = filter.split_once('=') else {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "command-stats: --filter expects key=value (e.g. failure_category=model_parse)"
                .into(),
        )));
    };
    let (key, value) = (key.trim(), value.trim());
    match key {
        "resolved" => {
            if value != "true" && value != "false" {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "command-stats: resolved filter must be `true` or `false`".into(),
                )));
            }
            let expected = value == "true";
            Ok(resolved == Some(expected))
        }
        "failure_category" => Ok(instance.failure_category.is_some_and(|fc| {
            failure_category_label(fc) == value
        })),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "command-stats: unsupported filter key `{other}`; supported: `resolved`, `failure_category`"
        )))),
    }
}

fn failure_category_label(c: FailureCategory) -> &'static str {
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
        FailureCategory::Unknown => "unknown",
    }
}

// ── step extraction ───────────────────────────────────────────────────────────

fn extract_steps_from_trajectory(
    trajectory: &Trajectory,
    instance_id: &str,
    bucket: &OutcomeBucket,
    _instance: &InstanceResult,
) -> Vec<BashStep> {
    let mut steps = Vec::new();
    let bucket_str = bucket.as_str().to_owned();
    let messages = &trajectory.messages;

    for (i, msg) in messages.iter().enumerate() {
        if msg.role != "assistant" {
            continue;
        }
        let Some(actions) = &msg.extra.actions else {
            continue;
        };
        let cost = msg.extra.cost.unwrap_or(0.0);

        // Find the exit code from the next user observation message
        let exit_code = messages[i + 1..]
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| m.extra.other.get("run_result"))
            .and_then(|v| serde_json::from_value::<RunResult>(v.clone()).ok())
            .map(|rr| rr.exit_code);

        for action in actions {
            for head in extract_command_heads(action) {
                steps.push(BashStep {
                    command_head: head,
                    instance_id: instance_id.to_owned(),
                    bucket: bucket_str.clone(),
                    exit_code,
                    cost_usd: cost,
                });
            }
        }
    }

    steps
}

// ── head extraction (public for unit tests) ───────────────────────────────────

/// Extract the normalized command head(s) from a bash command string.
///
/// Handles:
/// - Pipelines: `a | b | c` → `["a", "b", "c"]` (not `||`)
/// - Leading `sudo` prefix stripped
/// - Leading `time` prefix stripped
/// - Leading `env VAR=val ...` prefix stripped
/// - Leading bare `VAR=val` environment assignments stripped
pub fn extract_command_heads(command: &str) -> Vec<String> {
    let command = command.trim();
    if command.is_empty() {
        return Vec::new();
    }
    split_pipeline(command)
        .into_iter()
        .filter_map(|segment| {
            let head = extract_segment_head(segment.trim());
            if head.is_empty() { None } else { Some(head) }
        })
        .collect()
}

/// Split on `|` but not `||`.
fn split_pipeline(command: &str) -> Vec<&str> {
    let mut segments: Vec<&str> = Vec::new();
    let bytes = command.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'|' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            // logical OR — stop splitting here; treat the rest as one segment
            break;
        }
        if bytes[i] == b'|' {
            segments.push(&command[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    segments.push(&command[start..]);
    segments
}

/// Extract the command head from one pipeline segment, stripping prefixes.
fn extract_segment_head(segment: &str) -> String {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];

        if token == "sudo" || token == "time" {
            i += 1;
            continue;
        }

        if token == "env" {
            i += 1;
            // Skip VAR=val tokens after `env`
            while i < tokens.len()
                && tokens[i].contains('=')
                && !tokens[i].starts_with('-')
            {
                i += 1;
            }
            continue;
        }

        // Bare VAR=val assignment: identifier before `=`
        if let Some(eq) = token.find('=') {
            let var = &token[..eq];
            if !var.is_empty()
                && !var.starts_with('-')
                && !var.starts_with('/')
                && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                i += 1;
                continue;
            }
        }

        return token.to_owned();
    }
    String::new()
}

// ── trajectory loading (mirrors triage.rs) ────────────────────────────────────

fn load_trajectory(path: &Path) -> Result<Trajectory, Error> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string())
        .map_err(|err| Error::Trajectory(err.to_string()))?;
    serde_json::from_value(value).map_err(Into::into)
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
    if flat.exists() {
        return Some(flat);
    }
    let bundled = sweep
        .join("trajectories")
        .join(format!("{instance_id}.traj.json"));
    bundled.exists().then_some(bundled)
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_pipeline_handles_logical_or() {
        let segs = split_pipeline("false || true");
        // The `||` causes us to stop splitting; "false " is the first and only segment
        assert_eq!(segs.len(), 1, "|| should not create a pipeline split");
    }

    #[test]
    fn extract_heads_pipeline_three_segments() {
        assert_eq!(
            extract_command_heads("cat f | grep x | sort"),
            vec!["cat", "grep", "sort"]
        );
    }

    #[test]
    fn extract_heads_strips_nested_prefixes() {
        assert_eq!(
            extract_command_heads("sudo time env A=1 pytest -x"),
            vec!["pytest"]
        );
    }
}
