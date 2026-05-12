//! `bench triage`: deterministic clustering for unresolved sweep failures.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::env::RunResult;
use crate::error::Error;
use crate::run::compare::{load_evaluation_results_checked, load_sweep};
use crate::run::swebench::{InstanceResult, resolved_count};
use crate::trajectory::{FailureCategory, Trajectory};

const ASSISTANT_TAIL_CHARS: usize = 500;
const SUMMARY_MAX_CHARS: usize = 160;
const CLUSTER_ID_HEX_CHARS: usize = 16;

#[derive(Debug, Clone)]
pub struct TriageArgs {
    pub sweep_dir: PathBuf,
    pub bucket: Option<String>,
    pub min_cluster_size: usize,
    pub top: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageReport {
    pub sweep: String,
    pub generated_at: String,
    pub clusters: Vec<TriageCluster>,
    pub totals: TriageTotals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageCluster {
    pub cluster_id: String,
    pub failure_category: String,
    pub signature_summary: String,
    pub instance_count: usize,
    pub total_cost_usd: f64,
    pub exemplar_instance_id: String,
    pub exemplar_trajectory_path: String,
    pub instance_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TriageTotals {
    pub clusters: usize,
    pub instances: usize,
    pub unclustered_instances: usize,
    pub unresolved_cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FailureSignature {
    failure_category: String,
    assistant_tail: String,
    bash_exit_code: Option<i32>,
    stderr_line: String,
}

impl FailureSignature {
    #[must_use]
    pub fn from_parts(
        failure_category: &str,
        assistant_message: &str,
        bash_exit_code: Option<i32>,
        stderr_line: &str,
    ) -> Self {
        Self {
            failure_category: failure_category.to_owned(),
            assistant_tail: normalize_signature_text(&message_tail(
                assistant_message,
                ASSISTANT_TAIL_CHARS,
            )),
            bash_exit_code,
            stderr_line: normalize_signature_text(stderr_line),
        }
    }

    #[must_use]
    pub fn stable_key(&self) -> String {
        format!(
            "failure_category={}\nassistant_tail={}\nbash_exit_code={}\nstderr_line={}",
            self.failure_category,
            self.assistant_tail,
            self.bash_exit_code
                .map_or_else(|| "none".to_owned(), |code| code.to_string()),
            self.stderr_line
        )
    }

    #[must_use]
    pub fn cluster_id(&self) -> String {
        let digest = Sha256::digest(self.stable_key().as_bytes());
        let mut out = String::with_capacity(CLUSTER_ID_HEX_CHARS);
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
            if out.len() >= CLUSTER_ID_HEX_CHARS {
                break;
            }
        }
        out
    }

    #[must_use]
    pub fn summary(&self) -> String {
        truncate_chars(
            &format!(
                "assistant=\"{}\" exit={} stderr=\"{}\"",
                self.assistant_tail,
                self.bash_exit_code
                    .map_or_else(|| "none".to_owned(), |code| code.to_string()),
                self.stderr_line
            ),
            SUMMARY_MAX_CHARS,
        )
    }
}

#[derive(Debug, Clone)]
struct ClusterMember {
    instance_id: String,
    trajectory_path: String,
    cost_usd: f64,
}

#[derive(Debug, Clone)]
struct ClusterAccumulator {
    signature: FailureSignature,
    signature_summary: String,
    cluster_id: String,
    members: Vec<ClusterMember>,
    total_cost_usd: f64,
    score: f64,
}

impl ClusterAccumulator {
    fn new(signature: FailureSignature) -> Self {
        let signature_summary = signature.summary();
        let cluster_id = signature.cluster_id();
        Self {
            signature,
            signature_summary,
            cluster_id,
            members: Vec::new(),
            total_cost_usd: 0.0,
            score: 0.0,
        }
    }

    fn push_member(&mut self, member: ClusterMember) {
        self.total_cost_usd += member.cost_usd;
        self.members.push(member);
        self.score = Self::score_for(self.members.len(), self.total_cost_usd);
    }

    fn score_for(member_count: usize, total_cost_usd: f64) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        {
            member_count as f64 * total_cost_usd
        }
    }
}

#[derive(Debug, Clone)]
struct TerminalSignals {
    assistant_message: String,
    bash_exit_code: Option<i32>,
    stderr_line: String,
}

/// Normalize one textual signature component.
///
/// The normalizer lowercases, collapses whitespace, replaces tokens that look
/// like filesystem paths with `<path>`, and replaces ASCII digit runs (including
/// simple decimals) with `<num>`.
#[must_use]
pub fn normalize_signature_text(raw: &str) -> String {
    raw.to_lowercase()
        .split_whitespace()
        .map(normalize_signature_token)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_signature_token(token: &str) -> String {
    let core = token.trim_matches(is_wrapping_punctuation);
    if looks_like_path(core) {
        return "<path>".into();
    }
    replace_numbers(token)
}

fn is_wrapping_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
    )
}

fn looks_like_path(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token.starts_with('/')
        || token.starts_with('\\')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with(".\\")
        || token.starts_with("..\\")
    {
        return true;
    }

    let bytes = token.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
    {
        return true;
    }

    token.contains('/') || token.contains('\\')
}

fn replace_numbers(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() {
            out.push_str("<num>");
            consume_number_tail(&mut chars);
            continue;
        }
        out.push(ch);
    }
    out
}

fn consume_number_tail<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    while let Some(next) = chars.peek().copied() {
        if next.is_ascii_digit() {
            let _ = chars.next();
            continue;
        }
        if next == '.' {
            let _ = chars.next();
            continue;
        }
        break;
    }
}

pub fn run(args: &TriageArgs) -> Result<TriageReport, Error> {
    if args.min_cluster_size == 0 {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "triage: --min-cluster-size must be at least 1".into(),
        )));
    }

    let mut report = build_report(args)?;
    report.generated_at = utc_now_iso8601();
    let output_path = args.sweep_dir.join("triage.json");
    let file = std::fs::File::create(output_path)?;
    serde_json::to_writer_pretty(file, &report)?;
    Ok(report)
}

pub fn render_text(report: &TriageReport, top: usize) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let mut out = String::new();
    out.push_str("\n=== bench triage ===\n");
    let _ = writeln!(out, "Sweep: {}", report.sweep);
    let _ = writeln!(
        out,
        "Clusters: {}  instances: {}  unresolved_cost_usd: {:.4}",
        report.totals.clusters, report.totals.instances, report.totals.unresolved_cost_usd
    );
    out.push('\n');

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "rank",
            "failure_category",
            "instance_count",
            "total_usd",
            "% unresolved cost",
            "exemplar_instance_id",
            "signature_summary",
        ]);

    for (idx, cluster) in report.clusters.iter().take(top).enumerate() {
        let share = if report.totals.unresolved_cost_usd > 0.0 {
            100.0 * cluster.total_cost_usd / report.totals.unresolved_cost_usd
        } else {
            0.0
        };
        table.add_row(vec![
            (idx + 1).to_string(),
            cluster.failure_category.clone(),
            cluster.instance_count.to_string(),
            format!("{:.4}", cluster.total_cost_usd),
            format!("{share:.1}"),
            cluster.exemplar_instance_id.clone(),
            truncate_chars(&cluster.signature_summary, 96),
        ]);
    }

    out.push_str(&table.to_string());
    out.push('\n');
    out
}

fn build_report(args: &TriageArgs) -> Result<TriageReport, Error> {
    let sweep = load_sweep(&args.sweep_dir)?;
    let evaluation = load_evaluation_results_checked(&args.sweep_dir)?.ok_or_else(|| {
        Error::Trajectory(crate::error::TrajectoryError::Validation(format!(
            "bench triage: missing evaluation.json in {}; run `bench evaluate --sweep {}` first",
            args.sweep_dir.display(),
            args.sweep_dir.display()
        )))
    })?;

    let candidate_ids = candidate_instance_ids(evaluation.results, &sweep.instances);
    let accumulators = build_accumulators(args, &sweep.instances, candidate_ids)?;

    let total_candidate_instances = accumulators
        .values()
        .map(|cluster| cluster.members.len())
        .sum::<usize>();
    let total_candidate_cost_usd = accumulators
        .values()
        .map(|cluster| cluster.total_cost_usd)
        .sum::<f64>();
    let mut cluster_accs: Vec<ClusterAccumulator> = accumulators
        .into_values()
        .filter(|cluster| cluster.members.len() >= args.min_cluster_size)
        .collect();
    for cluster in &mut cluster_accs {
        cluster
            .members
            .sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    }
    cluster_accs.sort_by(compare_cluster_accumulators);

    let clusters: Vec<TriageCluster> = cluster_accs.iter().map(cluster_from_accumulator).collect();
    let clustered_instances = clusters
        .iter()
        .map(|cluster| cluster.instance_count)
        .sum::<usize>();
    let totals = TriageTotals {
        clusters: clusters.len(),
        instances: clustered_instances,
        unclustered_instances: total_candidate_instances.saturating_sub(clustered_instances),
        unresolved_cost_usd: total_candidate_cost_usd,
    };

    Ok(TriageReport {
        sweep: args.sweep_dir.display().to_string(),
        generated_at: String::new(),
        clusters,
        totals,
    })
}

fn candidate_instance_ids(
    evaluation: crate::run::evaluate::EvaluationResults,
    instances: &HashMap<String, InstanceResult>,
) -> BTreeSet<String> {
    let mut candidate_ids: BTreeSet<String> = evaluation
        .instances
        .into_iter()
        .filter(|row| !row.resolved)
        .map(|row| row.instance_id)
        .collect();
    candidate_ids.extend(
        instances
            .values()
            .filter(|instance| instance_is_errored(instance))
            .map(|instance| instance.instance_id.clone()),
    );
    candidate_ids
}

fn build_accumulators(
    args: &TriageArgs,
    instances: &HashMap<String, InstanceResult>,
    candidate_ids: BTreeSet<String>,
) -> Result<BTreeMap<String, ClusterAccumulator>, Error> {
    let mut accumulators: BTreeMap<String, ClusterAccumulator> = BTreeMap::new();
    for instance_id in candidate_ids {
        let instance = instances.get(&instance_id).ok_or_else(|| {
            Error::Trajectory(crate::error::TrajectoryError::Validation(format!(
                "bench triage: evaluation.json references `{instance_id}` but results.json has no matching instance"
            )))
        })?;
        let trajectory_path =
            resolve_trajectory_path(&args.sweep_dir, &instance_id).ok_or_else(|| {
                Error::Trajectory(crate::error::TrajectoryError::Validation(format!(
                    "bench triage: trajectory not found for unresolved instance `{instance_id}`"
                )))
            })?;
        let trajectory = load_trajectory(&trajectory_path)?;
        let failure_category = instance
            .failure_category
            .or(trajectory.info.failure_category)
            .unwrap_or(FailureCategory::Unknown);
        let category_label = failure_label(failure_category).to_owned();
        if args
            .bucket
            .as_deref()
            .is_some_and(|bucket| bucket != category_label)
        {
            continue;
        }

        let signals = terminal_signals(&trajectory);
        let signature = FailureSignature::from_parts(
            &category_label,
            &signals.assistant_message,
            signals.bash_exit_code,
            &signals.stderr_line,
        );
        let key = signature.stable_key();
        let rel_path = relative_path_string(&args.sweep_dir, &trajectory_path);
        let member = ClusterMember {
            instance_id,
            trajectory_path: rel_path,
            cost_usd: instance.actual_cost_usd().unwrap_or(0.0),
        };
        accumulators
            .entry(key)
            .or_insert_with(|| ClusterAccumulator::new(signature))
            .push_member(member);
    }
    Ok(accumulators)
}

fn instance_is_errored(instance: &InstanceResult) -> bool {
    resolved_count(instance) == 0
        && (instance.outcome.as_deref() == Some(crate::trajectory::outcome::ERROR)
            || instance.failure_category.is_some())
}

fn compare_cluster_accumulators(
    left: &ClusterAccumulator,
    right: &ClusterAccumulator,
) -> std::cmp::Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| right.total_cost_usd.total_cmp(&left.total_cost_usd))
        .then_with(|| {
            left.signature
                .failure_category
                .cmp(&right.signature.failure_category)
        })
        .then_with(|| left.signature_summary.cmp(&right.signature_summary))
        .then_with(|| left.cluster_id.cmp(&right.cluster_id))
}

fn cluster_from_accumulator(acc: &ClusterAccumulator) -> TriageCluster {
    let exemplar = acc.members.first().map_or_else(
        || ClusterMember {
            instance_id: String::new(),
            trajectory_path: String::new(),
            cost_usd: 0.0,
        },
        Clone::clone,
    );
    TriageCluster {
        cluster_id: acc.cluster_id.clone(),
        failure_category: acc.signature.failure_category.clone(),
        signature_summary: acc.signature_summary.clone(),
        instance_count: acc.members.len(),
        total_cost_usd: acc.total_cost_usd,
        exemplar_instance_id: exemplar.instance_id,
        exemplar_trajectory_path: exemplar.trajectory_path,
        instance_ids: acc
            .members
            .iter()
            .map(|member| member.instance_id.clone())
            .collect(),
    }
}

fn load_trajectory(path: &Path) -> Result<Trajectory, Error> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string())
        .map_err(|err| Error::Trajectory(crate::error::TrajectoryError::Format(err.to_string())))?;
    serde_json::from_value(value).map_err(Into::into)
}

fn terminal_signals(trajectory: &Trajectory) -> TerminalSignals {
    let assistant_message = trajectory
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .map_or_else(String::new, |message| message.content.clone());
    let last_run = trajectory.messages.iter().rev().find_map(|message| {
        if message.role != "user" {
            return None;
        }
        message
            .extra
            .other
            .get("run_result")
            .and_then(|value| serde_json::from_value::<RunResult>(value.clone()).ok())
    });
    let (bash_exit_code, stderr_line) = last_run.map_or((None, String::new()), |run| {
        (
            Some(run.exit_code),
            run.stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .to_owned(),
        )
    });
    TerminalSignals {
        assistant_message,
        bash_exit_code,
        stderr_line,
    }
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

fn relative_path_string(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .map_or_else(|_| path_to_forward_slashes(path), path_to_forward_slashes)
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
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

fn message_tail(message: &str, max_chars: usize) -> String {
    let len = message.chars().count();
    if len <= max_chars {
        return message.to_owned();
    }
    message.chars().skip(len - max_chars).collect()
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut iter = s.chars();
    let mut out: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        out.push_str("...");
    }
    out
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_tail_limits_by_chars() {
        assert_eq!(message_tail("abcdef", 3), "def");
        assert_eq!(message_tail("abc", 3), "abc");
    }
}
