//! `bench tool-coverage`: per-sweep MCP tool usage summary by outcome bucket.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::compare::{load_evaluation_results_checked, load_sweep};
use crate::run::swebench::InstanceResult;
use crate::trajectory::{FailureCategory, Trajectory};

const VALID_BUCKETS: &[&str] = &["resolved", "unresolved", "errored", "all"];

// ── public argument struct ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolCoverageArgs {
    pub sweep_dir: PathBuf,
    pub bucket: Option<String>,
    pub filter: Option<String>,
    pub min_invocations: usize,
    pub per_instance: bool,
}

// ── JSON-deserialization types for toolset stored in trajectory.info.other ────

#[derive(Debug, Clone, Deserialize)]
struct RawToolsetManifest {
    tools: Vec<RawToolManifestEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawToolManifestEntry {
    name: String,
    source: String,
    #[serde(default)]
    mcp_server: Option<String>,
}

// ── report types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUniverseEntry {
    pub name: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeToolMetrics {
    pub total_invocations: usize,
    pub instances_used: usize,
    pub instances_total: usize,
    pub usage_rate: f64,
    pub share_of_bucket_tool_calls: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_rate_when_used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_rate_when_not_used: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetrics {
    pub total_invocations: usize,
    pub instances_used: usize,
    pub mean_invocations_per_using_instance: f64,
    pub share_of_all_tool_calls: f64,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<String>,
    pub by_outcome: BTreeMap<String, OutcomeToolMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsetDriftEntry {
    pub toolset_fingerprint: String,
    pub instance_count: usize,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsetDrift {
    pub toolsets: Vec<ToolsetDriftEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceToolCounts {
    pub instance_id: String,
    pub tool_calls: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCoverageReport {
    pub sweep: String,
    pub generated_at: String,
    pub tool_universe: Vec<ToolUniverseEntry>,
    pub by_tool: BTreeMap<String, ToolMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolset_drift: Option<ToolsetDrift>,
    pub unused_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_instance: Option<Vec<InstanceToolCounts>>,
}

// ── entry point ───────────────────────────────────────────────────────────────

pub fn run(args: &ToolCoverageArgs) -> Result<ToolCoverageReport, Error> {
    let mut report = build_report(args)?;
    report.generated_at = utc_now_iso8601();
    let output_path = args.sweep_dir.join("tool-coverage.json");
    let file = std::fs::File::create(output_path)?;
    serde_json::to_writer_pretty(file, &report)?;
    Ok(report)
}

// ── text rendering ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
pub fn render_text(
    report: &ToolCoverageReport,
    bucket_filter: Option<&str>,
    min_invocations: usize,
) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    // Which by_outcome slice to show in the main table.
    let display_bucket = bucket_filter.unwrap_or("all");
    let bucket_scoped = display_bucket != "all";

    let mut out = String::new();
    out.push_str("\n=== bench tool-coverage ===\n");
    let _ = writeln!(out, "Sweep: {}", report.sweep);
    let _ = writeln!(out, "Tool universe: {} tools", report.tool_universe.len());
    if bucket_filter.is_some() {
        let _ = writeln!(out, "Bucket filter: {display_bucket}");
    }
    out.push('\n');

    if let Some(drift) = &report.toolset_drift {
        let _ = writeln!(
            out,
            "Toolset Drift detected: {} distinct toolsets observed across the sweep.",
            drift.toolsets.len()
        );
        for entry in &drift.toolsets {
            let _ = writeln!(
                out,
                "  [{} instance(s)]: {}",
                entry.instance_count,
                entry.tools.join(", ")
            );
        }
        out.push('\n');
    }

    // Build table with tools sorted by total_invocations desc (global).
    // Apply --min-invocations filter against all-bucket total (never against JSON).
    let mut rows: Vec<(&str, &ToolMetrics)> = report
        .by_tool
        .iter()
        .filter(|(_, m)| m.total_invocations >= min_invocations)
        .map(|(name, m)| (name.as_str(), m))
        .collect();
    rows.sort_by(|a, b| {
        b.1.total_invocations
            .cmp(&a.1.total_invocations)
            .then_with(|| a.0.cmp(b.0))
    });

    if !rows.is_empty() {
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "tool",
                "source",
                "total_invocations",
                "instances_used",
                "mean_calls/using_instance",
                "share",
                "resolved_rate_when_used",
                "resolved_rate_when_not_used",
            ]);

        for (name, m) in &rows {
            let bucket_metrics = m.by_outcome.get(display_bucket);

            // When a bucket filter is active, show bucket-scoped counts.
            #[allow(clippy::cast_precision_loss)]
            let (inv, inst_used, share, mean) = if bucket_scoped {
                bucket_metrics.map_or(
                    (
                        m.total_invocations,
                        m.instances_used,
                        m.share_of_all_tool_calls,
                        m.mean_invocations_per_using_instance,
                    ),
                    |o| {
                        let mean = if o.instances_used > 0 {
                            o.total_invocations as f64 / o.instances_used as f64
                        } else {
                            0.0
                        };
                        (
                            o.total_invocations,
                            o.instances_used,
                            o.share_of_bucket_tool_calls,
                            mean,
                        )
                    },
                )
            } else {
                (
                    m.total_invocations,
                    m.instances_used,
                    m.share_of_all_tool_calls,
                    m.mean_invocations_per_using_instance,
                )
            };

            let (rr_used, rr_not_used) = bucket_metrics.map_or_else(
                || ("—".to_owned(), "—".to_owned()),
                |o| {
                    (
                        o.resolved_rate_when_used
                            .map_or_else(|| "—".to_owned(), |v| format!("{v:.3}")),
                        o.resolved_rate_when_not_used
                            .map_or_else(|| "—".to_owned(), |v| format!("{v:.3}")),
                    )
                },
            );

            table.add_row(vec![
                (*name).to_owned(),
                m.source.clone(),
                inv.to_string(),
                inst_used.to_string(),
                format!("{mean:.2}"),
                format!("{share:.4}"),
                rr_used,
                rr_not_used,
            ]);
        }

        out.push_str(&table.to_string());
        out.push('\n');
    }

    // Unused tools line
    if report.unused_tools.is_empty() {
        out.push_str("Unused tools: (none)\n");
    } else {
        let _ = writeln!(out, "Unused tools: {}", report.unused_tools.join(", "));
    }

    out
}

// ── internal build logic ──────────────────────────────────────────────────────

#[derive(Debug)]
struct InstanceData {
    id: String,
    bucket: OutcomeBucket,
    tool_counts: BTreeMap<String, usize>,
    toolset: Option<RawToolsetManifest>,
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
    is_resolved: bool,
    filter: &str,
) -> Result<bool, Error> {
    let Some((key, value)) = filter.split_once('=') else {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "tool-coverage: --filter expects key=value (e.g. failure_category=model_parse)".into(),
        )));
    };
    let (key, value) = (key.trim(), value.trim());
    match key {
        "resolved" => {
            if value != "true" && value != "false" {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "tool-coverage: resolved filter must be `true` or `false`".into(),
                )));
            }
            Ok(is_resolved == (value == "true"))
        }
        "failure_category" => Ok(instance
            .failure_category
            .is_some_and(|fc| failure_category_label(fc) == value)),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "tool-coverage: unsupported filter key `{other}`; supported: `resolved`, `failure_category`"
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
        FailureCategory::HistoryCompactionFailed => "history_compaction_failed",
        FailureCategory::Unknown => "unknown",
        FailureCategory::ReadOnlyViolation => "read_only_violation",
    }
}

/// Return true when an action string is a non-bash tool call (e.g. `diagnose:{…}`).
/// Tool names may contain alphanumerics, underscores, or hyphens (e.g. `search-web`).
/// Optional ASCII whitespace between the colon and `{` is accepted because
/// `action_label()` does not trim leading whitespace from the fenced-block body.
fn is_tool_call(action: &str) -> bool {
    let action = action.trim();
    let Some(colon_pos) = action.find(':') else {
        return false;
    };
    let prefix = &action[..colon_pos];
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return false;
    }
    action[colon_pos + 1..]
        .trim_start_matches([' ', '\t', '\n'])
        .starts_with('{')
}

/// Extract the tool name from a tool call action string like `tool_name:{"key":"val"}`.
fn tool_call_name(action: &str) -> Option<&str> {
    let action = action.trim();
    let colon_pos = action.find(':')?;
    let prefix = &action[..colon_pos];
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    if action[colon_pos + 1..]
        .trim_start_matches([' ', '\t', '\n'])
        .starts_with('{')
    {
        Some(prefix)
    } else {
        None
    }
}

/// Map raw `source` field from toolset manifest to report source label.
fn map_source(raw: &str) -> &'static str {
    match raw {
        "built_in" => "builtin",
        "mcp_server" => "mcp",
        "command_adapter" => "config",
        _ => "runtime_provider",
    }
}

/// Build a stable fingerprint for a toolset.
///
/// Includes name, source, and mcp_server so that same-named tools from different
/// providers are detected as distinct toolsets.
fn toolset_fingerprint(entries: &[String]) -> String {
    let mut sorted = entries.to_vec();
    sorted.sort();
    sorted.join(",")
}

fn load_trajectory(path: &Path) -> Result<Trajectory, Error> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string())
        .map_err(|err| Error::Trajectory(err.to_string()))?;
    serde_json::from_value(value).map_err(Into::into)
}

fn resolve_trajectory_paths(sweep: &Path, instance_id: &str) -> Vec<PathBuf> {
    let nested = sweep.join(instance_id).join("trajectory.json");
    if nested.exists() {
        return vec![nested];
    }

    let instance_dir = sweep.join(instance_id);
    if instance_dir.is_dir() {
        let run_paths: Vec<PathBuf> = std::fs::read_dir(&instance_dir)
            .map(|entries| {
                let mut paths: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("run-") && n.ends_with(".traj.json"))
                    })
                    .collect();
                paths.sort();
                paths
            })
            .unwrap_or_default();
        if !run_paths.is_empty() {
            return run_paths;
        }
    }

    let flat = sweep.join(format!("{instance_id}.traj.json"));
    if flat.exists() {
        return vec![flat];
    }

    let bundled = sweep
        .join("trajectories")
        .join(format!("{instance_id}.traj.json"));
    if bundled.exists() {
        vec![bundled]
    } else {
        vec![]
    }
}

/// Count tool invocations from a single trajectory's messages.
///
/// Returns a map of tool_name → count. Bash invocations are counted under "bash"
/// (every non-__SUBMIT__, non-tool-call action is one bash invocation).
fn count_tool_invocations(trajectory: &Trajectory) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for msg in &trajectory.messages {
        if msg.role != "assistant" {
            continue;
        }
        let Some(actions) = &msg.extra.actions else {
            continue;
        };
        for action in actions {
            if action == "__SUBMIT__" {
                continue;
            }
            if is_tool_call(action) {
                if let Some(name) = tool_call_name(action) {
                    *counts.entry(name.to_owned()).or_default() += 1;
                }
            } else {
                *counts.entry("bash".to_owned()).or_default() += 1;
            }
        }
    }

    counts
}

/// Parse the toolset manifest from `info.other["toolset"]` if present.
fn parse_toolset(trajectory: &Trajectory) -> Option<RawToolsetManifest> {
    let raw = trajectory.info.other.get("toolset")?;
    match serde_json::from_value(raw.clone()) {
        Ok(ts) => Some(ts),
        Err(e) => {
            eprintln!("tool-coverage: warning: failed to parse toolset manifest: {e}");
            None
        }
    }
}

#[allow(clippy::too_many_lines)]
fn build_report(args: &ToolCoverageArgs) -> Result<ToolCoverageReport, Error> {
    if let Some(b) = &args.bucket {
        if !VALID_BUCKETS.contains(&b.as_str()) {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "tool-coverage: unknown --bucket `{b}`; valid values: resolved, unresolved, errored, all"
            ))));
        }
    }

    let redactor = Redactor::default_enabled();

    let sweep = load_sweep(&args.sweep_dir)?;
    let evaluation = load_evaluation_results_checked(&args.sweep_dir)?;

    // Build resolved_set from evaluation.json when present; fall back to
    // results.json resolved_count when evaluation.json is absent.
    let mut resolved_set: HashSet<String> = evaluation
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
    if evaluation.is_none() {
        for (id, inst) in &sweep.instances {
            if inst.resolved_count > 0 {
                resolved_set.insert(id.clone());
            }
        }
    }

    let mut sorted_ids: Vec<String> = sweep.instances.keys().cloned().collect();
    sorted_ids.sort();

    // First pass: load trajectories, collect toolsets and invocations per instance.
    let mut instance_data: Vec<InstanceData> = Vec::new();

    for id in &sorted_ids {
        let instance = &sweep.instances[id];
        let is_resolved = resolved_set.contains(id);

        if let Some(filter) = &args.filter {
            if !matches_filter(instance, is_resolved, filter)? {
                continue;
            }
        }

        let bucket = classify_outcome(id, instance, &resolved_set);

        let mut combined_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut toolset: Option<RawToolsetManifest> = None;

        let traj_paths = resolve_trajectory_paths(&args.sweep_dir, id);
        if traj_paths.is_empty() {
            eprintln!("tool-coverage: warning: no trajectory files found for instance `{id}`");
        }
        for traj_path in traj_paths {
            match load_trajectory(&traj_path) {
                Ok(traj) => {
                    // Merge toolsets from all run files so tools introduced in
                    // later retries/resumes are not reported as rogue.
                    if let Some(new_ts) = parse_toolset(&traj) {
                        if let Some(existing) = &mut toolset {
                            let existing_names: HashSet<_> =
                                existing.tools.iter().map(|t| t.name.clone()).collect();
                            for entry in new_ts.tools {
                                if !existing_names.contains(&entry.name) {
                                    existing.tools.push(entry);
                                }
                            }
                        } else {
                            toolset = Some(new_ts);
                        }
                    }
                    for (tool, count) in count_tool_invocations(&traj) {
                        *combined_counts.entry(tool).or_default() += count;
                    }
                }
                Err(e) => {
                    eprintln!(
                        "tool-coverage: warning: skipping {}: {e}",
                        traj_path.display()
                    );
                }
            }
        }

        instance_data.push(InstanceData {
            id: id.clone(),
            bucket,
            tool_counts: combined_counts,
            toolset,
        });
    }

    // Collect all raw tool names from manifests + actual calls + implicit "bash".
    let mut all_raw_names: HashSet<String> = HashSet::new();
    all_raw_names.insert("bash".to_owned());
    for data in &instance_data {
        for name in data.tool_counts.keys() {
            all_raw_names.insert(name.clone());
        }
        if let Some(ts) = &data.toolset {
            for entry in &ts.tools {
                all_raw_names.insert(entry.name.clone());
            }
        }
    }

    // Build raw→redacted map; apply once so all downstream keying uses redacted names.
    let raw_to_redacted: HashMap<String, String> = all_raw_names
        .iter()
        .map(|raw| {
            let redacted = redactor.redact_text(raw, surface::TRAJECTORY).text;
            (raw.clone(), redacted)
        })
        .collect();

    // Remap each instance's tool_counts to use redacted names.
    for data in &mut instance_data {
        let remapped: BTreeMap<String, usize> = data
            .tool_counts
            .iter()
            .map(|(raw, &count)| {
                let key = raw_to_redacted
                    .get(raw)
                    .cloned()
                    .unwrap_or_else(|| raw.clone());
                (key, count)
            })
            .collect();
        data.tool_counts = remapped;
    }

    // Build tool universe keyed by redacted name.
    // Preserve first-seen source/mcp_server for each tool from manifests.
    let mut universe_map: BTreeMap<String, ToolUniverseEntry> = BTreeMap::new();
    let mut manifest_tool_names: BTreeSet<String> = BTreeSet::new();

    for data in &instance_data {
        if let Some(ts) = &data.toolset {
            for entry in &ts.tools {
                let redacted_name = raw_to_redacted
                    .get(&entry.name)
                    .cloned()
                    .unwrap_or_else(|| entry.name.clone());
                manifest_tool_names.insert(redacted_name.clone());
                universe_map
                    .entry(redacted_name.clone())
                    .or_insert_with(|| {
                        let mcp_server = entry
                            .mcp_server
                            .as_deref()
                            .map(|s| redactor.redact_text(s, surface::TRAJECTORY).text);
                        ToolUniverseEntry {
                            name: redacted_name,
                            source: map_source(&entry.source).to_owned(),
                            mcp_server,
                        }
                    });
            }
        }
    }

    // Ensure bash is in the universe (it is always available as a builtin).
    universe_map
        .entry("bash".to_owned())
        .or_insert_with(|| ToolUniverseEntry {
            name: "bash".to_owned(),
            source: "builtin".to_owned(),
            mcp_server: None,
        });
    manifest_tool_names.insert("bash".to_owned());

    // Add "rogue" tools: called in trajectories but absent from any manifest.
    for data in &instance_data {
        for redacted_name in data.tool_counts.keys() {
            if !manifest_tool_names.contains(redacted_name) {
                universe_map
                    .entry(redacted_name.clone())
                    .or_insert_with(|| ToolUniverseEntry {
                        name: redacted_name.clone(),
                        source: "unknown".to_owned(),
                        mcp_server: None,
                    });
            }
        }
    }

    let tool_universe: Vec<ToolUniverseEntry> = {
        let mut v: Vec<_> = universe_map.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };
    let universe_names: BTreeSet<String> = universe_map.keys().cloned().collect();

    // Detect toolset drift.
    // Fingerprint includes name, source, and mcp_server so same-named tools from
    // different providers are treated as distinct toolsets.
    let mut fingerprint_groups: HashMap<String, (Vec<String>, BTreeSet<String>)> = HashMap::new();
    for data in &instance_data {
        let entries: Vec<String> = data
            .toolset
            .as_ref()
            .map(|ts| {
                let mut v: Vec<String> = ts
                    .tools
                    .iter()
                    .map(|t| {
                        let redacted = raw_to_redacted
                            .get(&t.name)
                            .cloned()
                            .unwrap_or_else(|| t.name.clone());
                        let src = map_source(&t.source);
                        let mcp = t
                            .mcp_server
                            .as_deref()
                            .map(|s| redactor.redact_text(s, surface::TRAJECTORY).text)
                            .unwrap_or_default();
                        format!("{redacted}::{src}::{mcp}")
                    })
                    .collect();
                v.sort();
                v
            })
            .unwrap_or_default();
        let fp = toolset_fingerprint(&entries);
        let group = fingerprint_groups
            .entry(fp)
            .or_insert_with(|| (Vec::new(), BTreeSet::new()));
        group.0.push(data.id.clone());
        if let Some(ts) = &data.toolset {
            for t in &ts.tools {
                let redacted = raw_to_redacted
                    .get(&t.name)
                    .cloned()
                    .unwrap_or_else(|| t.name.clone());
                group.1.insert(redacted);
            }
        }
    }

    let toolset_drift = if fingerprint_groups.len() > 1 {
        let mut drift_entries: Vec<ToolsetDriftEntry> = fingerprint_groups
            .into_iter()
            .map(|(fp, (instances, tools))| ToolsetDriftEntry {
                toolset_fingerprint: fp,
                instance_count: instances.len(),
                tools: tools.into_iter().collect(),
            })
            .collect();
        drift_entries.sort_by(|a, b| {
            b.instance_count
                .cmp(&a.instance_count)
                .then_with(|| a.toolset_fingerprint.cmp(&b.toolset_fingerprint))
        });
        Some(ToolsetDrift {
            toolsets: drift_entries,
        })
    } else {
        None
    };

    // Build per-tool scope sets: which instances had each tool in their manifest.
    // Per spec: instances_total counts only instances where the tool was in scope.
    // Instances with no recorded toolset are treated as all-in-scope.
    // Bash is always in scope (builtin). Rogue tools (not in any manifest) are
    // treated as globally in scope via map_or(true, …) in the aggregation loop.
    let mut tool_scope: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for data in &instance_data {
        match &data.toolset {
            None => {
                // No toolset recorded: treat all universe tools as in scope.
                for name in &universe_names {
                    tool_scope
                        .entry(name.clone())
                        .or_default()
                        .insert(data.id.clone());
                }
            }
            Some(ts) => {
                for entry in &ts.tools {
                    let redacted_name = raw_to_redacted
                        .get(&entry.name)
                        .cloned()
                        .unwrap_or_else(|| entry.name.clone());
                    tool_scope
                        .entry(redacted_name)
                        .or_default()
                        .insert(data.id.clone());
                }
            }
        }
    }
    // Bash is always available in every instance.
    {
        let bash_scope = tool_scope.entry("bash".to_owned()).or_default();
        for data in &instance_data {
            bash_scope.insert(data.id.clone());
        }
    }

    // Grand total calls across all instances (for share computation).
    let grand_total_calls: usize = instance_data
        .iter()
        .flat_map(|d| d.tool_counts.values())
        .sum();

    let bucket_names = ["resolved", "unresolved", "errored", "all"];

    // Grand total calls per bucket (for per-bucket share computation).
    let mut grand_total_calls_per_bucket: HashMap<&str, usize> = HashMap::new();
    for data in &instance_data {
        let instance_total: usize = data.tool_counts.values().sum();
        for &bname in &bucket_names {
            if bname == "all" || data.bucket.as_str() == bname {
                *grand_total_calls_per_bucket.entry(bname).or_default() += instance_total;
            }
        }
    }

    // Aggregate per-tool metrics.
    let mut by_tool: BTreeMap<String, ToolMetrics> = BTreeMap::new();

    for tool_name in &universe_names {
        let universe_entry = &universe_map[tool_name];

        let mut total_invocations = 0usize;
        let mut instances_used_globally = 0usize;
        let mut per_bucket_invocations: HashMap<&str, usize> = HashMap::new();
        let mut per_bucket_used: HashMap<&str, usize> = HashMap::new();
        let mut per_bucket_total: HashMap<&str, usize> = HashMap::new();
        let mut per_bucket_resolved_used: HashMap<&str, usize> = HashMap::new();
        let mut per_bucket_resolved_total: HashMap<&str, usize> = HashMap::new();

        for &bname in &bucket_names {
            per_bucket_invocations.insert(bname, 0);
            per_bucket_used.insert(bname, 0);
            per_bucket_total.insert(bname, 0);
            per_bucket_resolved_used.insert(bname, 0);
            per_bucket_resolved_total.insert(bname, 0);
        }

        // Whether this tool was in scope for a given instance.
        // Rogue tools (not in any manifest) hit map_or(true,…): all instances
        // count toward the denominator since we have no manifest to restrict by.
        let scope = tool_scope.get(tool_name);

        for data in &instance_data {
            let calls = data.tool_counts.get(tool_name).copied().unwrap_or(0);
            let used = calls > 0;
            let is_resolved = data.bucket == OutcomeBucket::Resolved;
            let in_scope = scope.is_none_or(|s| s.contains(&data.id));

            total_invocations += calls;
            if used {
                instances_used_globally += 1;
            }

            for &bname in &bucket_names {
                let in_bucket = bname == "all" || data.bucket.as_str() == bname;
                if !in_bucket {
                    continue;
                }
                *per_bucket_invocations.entry(bname).or_default() += calls;
                if used {
                    *per_bucket_used.entry(bname).or_default() += 1;
                }
                // Only count toward denominator when the tool was in scope.
                if in_scope {
                    *per_bucket_total.entry(bname).or_default() += 1;
                    *per_bucket_resolved_total.entry(bname).or_default() +=
                        usize::from(is_resolved);
                }
                if used && is_resolved {
                    *per_bucket_resolved_used.entry(bname).or_default() += 1;
                }
            }
        }

        #[allow(clippy::cast_precision_loss)]
        let mean_invocations_per_using_instance = if instances_used_globally > 0 {
            total_invocations as f64 / instances_used_globally as f64
        } else {
            0.0
        };

        #[allow(clippy::cast_precision_loss)]
        let share_of_all_tool_calls = if grand_total_calls > 0 {
            total_invocations as f64 / grand_total_calls as f64
        } else {
            0.0
        };

        let mut by_outcome: BTreeMap<String, OutcomeToolMetrics> = BTreeMap::new();

        for &bname in &bucket_names {
            let b_inv = per_bucket_invocations[bname];
            let b_used = per_bucket_used[bname];
            let b_total = per_bucket_total[bname];
            let b_res_used = per_bucket_resolved_used[bname];
            let b_res_total = per_bucket_resolved_total[bname];
            let b_grand = grand_total_calls_per_bucket
                .get(bname)
                .copied()
                .unwrap_or(0);

            #[allow(clippy::cast_precision_loss)]
            let usage_rate = if b_total > 0 {
                b_used as f64 / b_total as f64
            } else {
                0.0
            };

            #[allow(clippy::cast_precision_loss)]
            let share_of_bucket_tool_calls = if b_grand > 0 {
                b_inv as f64 / b_grand as f64
            } else {
                0.0
            };

            // resolved_rate_when_used: among instances that called this tool (in bucket),
            // how many are resolved?
            #[allow(clippy::cast_precision_loss)]
            let resolved_rate_when_used = if b_used > 0 {
                Some(b_res_used as f64 / b_used as f64)
            } else {
                None
            };

            // resolved_rate_when_not_used: among instances that did NOT call this tool,
            // how many are resolved?
            let not_used = b_total.saturating_sub(b_used);
            let resolved_not_used = b_res_total.saturating_sub(b_res_used);
            #[allow(clippy::cast_precision_loss)]
            let resolved_rate_when_not_used = if not_used > 0 {
                Some(resolved_not_used as f64 / not_used as f64)
            } else {
                None
            };

            by_outcome.insert(
                bname.to_owned(),
                OutcomeToolMetrics {
                    total_invocations: b_inv,
                    instances_used: b_used,
                    instances_total: b_total,
                    usage_rate,
                    share_of_bucket_tool_calls,
                    resolved_rate_when_used,
                    resolved_rate_when_not_used,
                },
            );
        }

        by_tool.insert(
            tool_name.clone(),
            ToolMetrics {
                total_invocations,
                instances_used: instances_used_globally,
                mean_invocations_per_using_instance,
                share_of_all_tool_calls,
                source: universe_entry.source.clone(),
                mcp_server: universe_entry.mcp_server.clone(),
                by_outcome,
            },
        );
    }

    // Unused tools: registered but 0 invocations.
    let mut unused_tools: Vec<String> = universe_names
        .iter()
        .filter(|name| by_tool.get(*name).is_none_or(|m| m.total_invocations == 0))
        .cloned()
        .collect();
    unused_tools.sort();

    // Per-instance rows (only when requested).
    let per_instance = if args.per_instance {
        let mut rows: Vec<InstanceToolCounts> = instance_data
            .iter()
            .map(|data| {
                // Include all universe tools with 0 counts for tools not called.
                let mut tool_calls: BTreeMap<String, usize> = BTreeMap::new();
                for name in &universe_names {
                    let count = data.tool_counts.get(name).copied().unwrap_or(0);
                    tool_calls.insert(name.clone(), count);
                }
                InstanceToolCounts {
                    instance_id: data.id.clone(),
                    tool_calls,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Some(rows)
    } else {
        None
    };

    Ok(ToolCoverageReport {
        sweep: args.sweep_dir.display().to_string(),
        generated_at: String::new(),
        tool_universe,
        by_tool,
        toolset_drift,
        unused_tools,
        per_instance,
    })
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
