//! `bench behavior`: surface agent action-class mix by outcome bucket.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::error::Error;
use crate::run::compare::{load_evaluation_results_checked, load_sweep};
use crate::run::swebench::InstanceResult;
use crate::trajectory::{FailureCategory, Trajectory};

// ── taxonomy ──────────────────────────────────────────────────────────────────

pub const TAXONOMY_VERSION: u32 = 1;

const ALL_CLASSES: &[&str] = &[
    "test", "write", "build", "search", "read", "nav", "git", "other", "noop",
];

const VALID_BUCKETS: &[&str] = &["resolved", "unresolved", "errored", "all"];

/// Semantic action class for a single agent turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionClass {
    Test,
    Write,
    Build,
    Search,
    Read,
    Nav,
    Git,
    Other,
    Noop,
}

impl ActionClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Write => "write",
            Self::Build => "build",
            Self::Search => "search",
            Self::Read => "read",
            Self::Nav => "nav",
            Self::Git => "git",
            Self::Other => "other",
            Self::Noop => "noop",
        }
    }

    /// Higher value = higher priority when multiple classes appear in one turn.
    fn priority(&self) -> u8 {
        match self {
            Self::Test => 8,
            Self::Write => 7,
            Self::Build => 6,
            Self::Search => 5,
            Self::Read => 4,
            Self::Nav => 3,
            Self::Git => 2,
            Self::Other => 1,
            Self::Noop => 0,
        }
    }
}

// ── public classification API (used by unit tests) ────────────────────────────

/// Classify a single bash action string (may span multiple commands / pipelines).
pub fn classify_action(action: &str) -> ActionClass {
    split_and_pipeline(action)
        .iter()
        .map(|seg| classify_segment(seg).0)
        .reduce(|a, b| if a.priority() >= b.priority() { a } else { b })
        .unwrap_or(ActionClass::Noop)
}

/// Classify a turn's list of action strings into the primary action class.
///
/// - Empty slice → `Noop`
/// - All `__SUBMIT__` or tool calls → `Noop`
/// - Otherwise → highest-priority class across all actions
pub fn classify_turn(actions: &[&str]) -> ActionClass {
    actions
        .iter()
        .filter(|&&a| a != "__SUBMIT__" && !is_tool_call(a))
        .map(|&a| classify_action(a))
        .fold(ActionClass::Noop, |best, class| {
            if class.priority() > best.priority() {
                class
            } else {
                best
            }
        })
}

// ── public argument / report types ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BehaviorArgs {
    pub sweep_dir: PathBuf,
    pub bucket: Option<String>,
    pub min_share: Option<f64>,
    pub filter: Option<String>,
    pub per_instance: bool,
}

/// Per-class metrics for one outcome bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassMetrics {
    pub turn_count: usize,
    pub share: f64,
    pub mean_turns_per_instance: f64,
    pub attributed_cost_usd: f64,
}

/// Simplified metrics for the `totals` section (no per-instance breakdown).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TotalsMetrics {
    pub turn_count: usize,
    pub share: f64,
}

/// Per-class share delta between the resolved and unresolved buckets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShapeDelta {
    pub action_class: String,
    pub resolved_share: f64,
    pub unresolved_share: f64,
    pub share_delta: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BehaviorComparisons {
    pub resolved_vs_unresolved: Vec<ShapeDelta>,
}

/// Per-instance class counts (emitted only with `--per-instance`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceBehavior {
    pub instance_id: String,
    pub class_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorReport {
    pub sweep: String,
    pub generated_at: String,
    pub taxonomy_version: u32,
    /// Whole-sweep per-class turn counts and shares.
    pub totals: BTreeMap<String, TotalsMetrics>,
    /// Per-class metrics broken down by outcome bucket.
    pub by_outcome: BTreeMap<String, BTreeMap<String, ClassMetrics>>,
    /// Shape comparisons (resolved vs unresolved).
    pub comparisons: BehaviorComparisons,
    /// Per-instance class counts, present only when `--per-instance` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_instance: Option<Vec<InstanceBehavior>>,
    /// Command heads that fell into the `other` class, with invocation counts.
    pub unclassified_heads: BTreeMap<String, usize>,
}

// ── entry point ───────────────────────────────────────────────────────────────

pub fn run(args: &BehaviorArgs) -> Result<BehaviorReport, Error> {
    let mut report = build_report(args)?;
    report.generated_at = utc_now_iso8601();
    let output_path = args.sweep_dir.join("behavior.json");
    let file = std::fs::File::create(output_path)?;
    serde_json::to_writer_pretty(file, &report)?;
    Ok(report)
}

// ── text rendering ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
pub fn render_text(report: &BehaviorReport, bucket_filter: Option<&str>, min_share: f64) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let mut out = String::new();
    out.push_str("\n=== bench behavior ===\n");
    let _ = writeln!(out, "Sweep: {}", report.sweep);
    let total_turns: usize = report.totals.values().map(|m| m.turn_count).sum();
    let _ = writeln!(
        out,
        "Taxonomy version: {}  total turns: {}",
        report.taxonomy_version, total_turns
    );
    out.push('\n');

    let buckets_to_show: Vec<&str> = match bucket_filter {
        Some("all") | None => VALID_BUCKETS.to_vec(),
        Some(b) => vec![b],
    };

    for bucket_name in buckets_to_show {
        let Some(class_map) = report.by_outcome.get(bucket_name) else {
            continue;
        };

        // Apply min_share filter using the all-bucket share for each class
        let all_bucket = report.by_outcome.get("all");
        let visible: Vec<(&str, &ClassMetrics)> = ALL_CLASSES
            .iter()
            .filter_map(|&cls| {
                let metrics = class_map.get(cls)?;
                let all_share = all_bucket
                    .and_then(|b| b.get(cls))
                    .map_or(metrics.share, |m| m.share);
                if all_share >= min_share {
                    Some((cls, metrics))
                } else {
                    None
                }
            })
            .collect();

        if visible.is_empty() {
            let _ = writeln!(
                out,
                "--- Outcome: {bucket_name} --- (no classes above min-share threshold)"
            );
            continue;
        }

        let _ = writeln!(out, "--- Outcome: {bucket_name} ---");
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "action_class",
                "turn_count",
                "share",
                "mean_turns/instance",
                "attributed_cost_usd",
            ]);

        for (cls, m) in visible {
            table.add_row(vec![
                cls.to_owned(),
                m.turn_count.to_string(),
                format!("{:.4}", m.share),
                format!("{:.2}", m.mean_turns_per_instance),
                format!("{:.6}", m.attributed_cost_usd),
            ]);
        }

        out.push_str(&table.to_string());
        out.push('\n');
    }

    // Action-shape diff section
    let deltas = &report.comparisons.resolved_vs_unresolved;
    if !deltas.is_empty() {
        // Produce a headline for the biggest delta
        if let Some(top) = deltas.first() {
            let direction = if top.share_delta > 0.0 {
                "more"
            } else {
                "less"
            };
            let _ = writeln!(
                out,
                "Action-shape headline: agent is {direction} {}-heavy in resolved vs unresolved ({:+.0}pp)",
                top.action_class,
                top.share_delta * 100.0
            );
        }
        out.push_str("--- Action-Shape: resolved vs unresolved ---\n");
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "action_class",
                "resolved_share",
                "unresolved_share",
                "share_delta",
            ]);
        for d in deltas {
            table.add_row(vec![
                d.action_class.clone(),
                format!("{:.4}", d.resolved_share),
                format!("{:.4}", d.unresolved_share),
                format!("{:+.4}", d.share_delta),
            ]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if !report.unclassified_heads.is_empty() {
        let _ = writeln!(out, "Unclassified heads (other):");
        for (head, count) in &report.unclassified_heads {
            let _ = writeln!(out, "  {head}: {count}");
        }
    }

    out
}

/// Try to load behavior.json from two sweep directories and produce a shape diff paragraph.
/// Returns `None` when either file is absent or unreadable.
pub fn behavior_compare_section(baseline: &Path, candidate: &Path) -> Option<String> {
    let b_path = baseline.join("behavior.json");
    let c_path = candidate.join("behavior.json");

    if !b_path.exists() || !c_path.exists() {
        return None;
    }

    let b_text = std::fs::read_to_string(&b_path).ok()?;
    let c_text = std::fs::read_to_string(&c_path).ok()?;
    let b: BehaviorReport = serde_json::from_str(&b_text).ok()?;
    let c: BehaviorReport = serde_json::from_str(&c_text).ok()?;

    let b_all = b.by_outcome.get("all")?;
    let c_all = c.by_outcome.get("all")?;

    let all_classes: BTreeSet<&str> = b_all
        .keys()
        .chain(c_all.keys())
        .map(String::as_str)
        .collect();

    let mut deltas: Vec<(String, f64)> = all_classes
        .iter()
        .map(|&cls| {
            let b_share = b_all.get(cls).map_or(0.0, |m| m.share);
            let c_share = c_all.get(cls).map_or(0.0, |m| m.share);
            (cls.to_owned(), c_share - b_share)
        })
        .collect();

    deltas.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()).then_with(|| a.0.cmp(&b.0)));

    let biggest = deltas.first()?;
    if biggest.1.abs() < 0.01 {
        return Some(
            "\n--- Action-Shape Diff ---\nNo significant action-shape shift detected.\n".to_owned(),
        );
    }

    let direction = if biggest.1 > 0.0 { "more" } else { "less" };
    let mut out = String::from("\n--- Action-Shape Diff ---\n");
    let _ = writeln!(
        out,
        "Agent shifted to {} {}-heavy: {:+.0}pp {}",
        direction,
        biggest.0,
        biggest.1 * 100.0,
        deltas
            .iter()
            .take(3)
            .map(|(cls, d)| format!("{:+.0}pp {cls}", d * 100.0))
            .collect::<Vec<_>>()
            .join(", ")
    );

    Some(out)
}

// ── internal build logic ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct TurnRecord {
    bucket: String,
    action_class: String,
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

#[allow(clippy::too_many_lines)]
fn build_report(args: &BehaviorArgs) -> Result<BehaviorReport, Error> {
    if let Some(b) = &args.bucket {
        if !VALID_BUCKETS.contains(&b.as_str()) {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "behavior: unknown --bucket `{b}`; valid values: resolved, unresolved, errored, all"
            ))));
        }
    }

    let sweep = load_sweep(&args.sweep_dir)?;
    let evaluation = load_evaluation_results_checked(&args.sweep_dir)?;

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

    let mut sorted_ids: Vec<String> = sweep.instances.keys().cloned().collect();
    sorted_ids.sort();

    let mut instance_buckets: Vec<(String, OutcomeBucket)> = Vec::new();
    for id in &sorted_ids {
        let instance = &sweep.instances[id];
        let is_resolved = resolved_set.contains(id);
        if let Some(filter) = &args.filter {
            if !matches_filter(instance, Some(is_resolved), filter)? {
                continue;
            }
        }
        let bucket = classify_outcome(id, instance, &resolved_set);
        instance_buckets.push((id.clone(), bucket));
    }

    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut unclassified_heads: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_instance_rows: Vec<InstanceBehavior> = Vec::new();

    for (instance_id, bucket) in &instance_buckets {
        let mut instance_class_counts: BTreeMap<String, usize> = BTreeMap::new();

        for trajectory_path in resolve_trajectory_paths(&args.sweep_dir, instance_id) {
            let Ok(trajectory) = load_trajectory(&trajectory_path) else {
                continue;
            };

            let traj_turns = extract_turns_from_trajectory(
                &trajectory,
                instance_id,
                bucket,
                &mut unclassified_heads,
            );

            for t in &traj_turns {
                *instance_class_counts
                    .entry(t.action_class.clone())
                    .or_default() += 1;
            }
            turns.extend(traj_turns);
        }

        if args.per_instance {
            per_instance_rows.push(InstanceBehavior {
                instance_id: instance_id.clone(),
                class_counts: instance_class_counts,
            });
        }
    }

    // Build by_outcome
    let bucket_names = ["resolved", "unresolved", "errored", "all"];
    let mut by_outcome: BTreeMap<String, BTreeMap<String, ClassMetrics>> = BTreeMap::new();

    for &bucket_name in &bucket_names {
        let bucket_turns: Vec<&TurnRecord> = turns
            .iter()
            .filter(|t| bucket_name == "all" || t.bucket == bucket_name)
            .collect();

        // Total instances in this bucket — used as the denominator for
        // mean_turns_per_instance so the metric is a population mean (all
        // instances, not just those with at least one turn of the class).
        #[allow(clippy::cast_precision_loss)]
        let bucket_instance_count = instance_buckets
            .iter()
            .filter(|(_, b)| bucket_name == "all" || b.as_str() == bucket_name)
            .count();

        let total_turns = bucket_turns.len();
        let mut class_map: BTreeMap<String, ClassMetrics> = BTreeMap::new();

        for &class_name in ALL_CLASSES {
            let class_turns: Vec<&&TurnRecord> = bucket_turns
                .iter()
                .filter(|t| t.action_class == class_name)
                .collect();

            if class_turns.is_empty() {
                continue;
            }

            let turn_count = class_turns.len();

            #[allow(clippy::cast_precision_loss)]
            let share = if total_turns > 0 {
                turn_count as f64 / total_turns as f64
            } else {
                0.0
            };
            #[allow(clippy::cast_precision_loss)]
            let mean_turns_per_instance = if bucket_instance_count > 0 {
                turn_count as f64 / bucket_instance_count as f64
            } else {
                0.0
            };
            let attributed_cost_usd: f64 = class_turns.iter().map(|t| t.cost_usd).sum();

            class_map.insert(
                class_name.to_owned(),
                ClassMetrics {
                    turn_count,
                    share,
                    mean_turns_per_instance,
                    attributed_cost_usd,
                },
            );
        }

        by_outcome.insert(bucket_name.to_owned(), class_map);
    }

    // Build totals
    let total_turns = turns.len();
    let mut totals: BTreeMap<String, TotalsMetrics> = BTreeMap::new();
    for &class_name in ALL_CLASSES {
        #[allow(clippy::cast_precision_loss)]
        let count = turns
            .iter()
            .filter(|t| t.action_class == class_name)
            .count();
        if count > 0 {
            totals.insert(
                class_name.to_owned(),
                TotalsMetrics {
                    turn_count: count,
                    #[allow(clippy::cast_precision_loss)]
                    share: if total_turns > 0 {
                        count as f64 / total_turns as f64
                    } else {
                        0.0
                    },
                },
            );
        }
    }

    let comparisons = build_comparisons(&by_outcome);

    let per_instance = if args.per_instance {
        Some(per_instance_rows)
    } else {
        None
    };

    Ok(BehaviorReport {
        sweep: args.sweep_dir.display().to_string(),
        generated_at: String::new(),
        taxonomy_version: TAXONOMY_VERSION,
        totals,
        by_outcome,
        comparisons,
        per_instance,
        unclassified_heads,
    })
}

fn build_comparisons(
    by_outcome: &BTreeMap<String, BTreeMap<String, ClassMetrics>>,
) -> BehaviorComparisons {
    let (Some(resolved), Some(unresolved)) =
        (by_outcome.get("resolved"), by_outcome.get("unresolved"))
    else {
        return BehaviorComparisons::default();
    };

    let all_classes: BTreeSet<&str> = resolved
        .keys()
        .chain(unresolved.keys())
        .map(String::as_str)
        .collect();

    let mut deltas: Vec<ShapeDelta> = all_classes
        .iter()
        .map(|&cls| {
            let r_share = resolved.get(cls).map_or(0.0, |m| m.share);
            let u_share = unresolved.get(cls).map_or(0.0, |m| m.share);
            ShapeDelta {
                action_class: cls.to_owned(),
                resolved_share: r_share,
                unresolved_share: u_share,
                share_delta: r_share - u_share,
            }
        })
        .collect();

    deltas.sort_by(|a, b| {
        b.share_delta
            .abs()
            .total_cmp(&a.share_delta.abs())
            .then_with(|| a.action_class.cmp(&b.action_class))
    });

    BehaviorComparisons {
        resolved_vs_unresolved: deltas,
    }
}

fn extract_turns_from_trajectory(
    trajectory: &Trajectory,
    _instance_id: &str,
    bucket: &OutcomeBucket,
    unclassified_heads: &mut BTreeMap<String, usize>,
) -> Vec<TurnRecord> {
    let mut records = Vec::new();
    let bucket_str = bucket.as_str().to_owned();

    for msg in &trajectory.messages {
        if msg.role != "assistant" {
            continue;
        }

        let cost = msg.extra.cost.unwrap_or(0.0);

        let action_class = match &msg.extra.actions {
            Some(actions) if !actions.is_empty() => {
                classify_turn_tracking(actions, unclassified_heads)
            }
            _ => ActionClass::Noop,
        };

        records.push(TurnRecord {
            bucket: bucket_str.clone(),
            action_class: action_class.as_str().to_owned(),
            cost_usd: cost,
        });
    }

    records
}

/// Like `classify_turn` but also populates `unclassified_heads` for `other`-class segments.
fn classify_turn_tracking(
    actions: &[String],
    unclassified_heads: &mut BTreeMap<String, usize>,
) -> ActionClass {
    let mut best = ActionClass::Noop;

    for action in actions {
        if action == "__SUBMIT__" || is_tool_call(action) {
            continue;
        }
        for seg in split_and_pipeline(action) {
            let (class, maybe_head) = classify_segment(seg);
            if let Some(head) = maybe_head {
                *unclassified_heads.entry(head).or_default() += 1;
            }
            if class.priority() > best.priority() {
                best = class;
            }
        }
    }

    best
}

// ── classification internals ──────────────────────────────────────────────────

/// Split on `|` but not `||` (logical-OR stops pipeline classification).
/// Respects single- and double-quoted strings so `echo 'a|b' > file` is not split.
fn split_pipeline(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let bytes = command.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'|' if !in_single && !in_double => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    break;
                }
                segments.push(&command[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    segments.push(&command[start..]);
    segments
}

/// Split a multi-command action string (newlines, `&&`, `;`) then split each
/// fragment on pipeline `|`. Returns all segments for classification.
fn split_and_pipeline(action: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for cmd in split_commands(action) {
        out.extend(split_pipeline(cmd));
    }
    out
}

/// Split `action` on `&&`, `;`, and newlines while respecting single- and
/// double-quoted strings, so `echo 'a;b' > file` stays as one command.
fn split_commands(action: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = action.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'\n' | b';' if !in_single && !in_double => {
                let seg = action[start..i].trim();
                if !seg.is_empty() {
                    result.push(seg);
                }
                start = i + 1;
            }
            b'&' if !in_single && !in_double && i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                let seg = action[start..i].trim();
                if !seg.is_empty() {
                    result.push(seg);
                }
                start = i + 2;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let seg = action[start..].trim();
    if !seg.is_empty() {
        result.push(seg);
    }
    result
}

/// Return true when an action string is a non-bash tool call (e.g. `diagnose:{…}`).
fn is_tool_call(action: &str) -> bool {
    // Tool calls in mini-swe-agent look like `name:{json}` where the name is a
    // bare identifier (word chars only). Require the prefix before `:{` to be
    // all word chars so that bash actions containing JSON (e.g. `echo '{"k":"v"}'
    // > file`) are not mistakenly excluded.
    let action = action.trim();
    let Some(colon_pos) = action.find(":{") else {
        return false;
    };
    let prefix = &action[..colon_pos];
    !prefix.is_empty()
        && prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Return the first arg in `args` that is a member of `known`, skipping over flags.
///
/// This lets dispatch commands like `cargo --locked test` or `gradle --no-daemon test`
/// find their subcommand even when global options appear before it.
fn find_subcommand<'a>(args: &[&'a str], known: &[&str]) -> &'a str {
    args.iter()
        .find(|&&t| known.contains(&t))
        .copied()
        .unwrap_or("")
}

/// Classify one pipeline segment. Returns `(ActionClass, Some(head))` when class is `Other`.
fn classify_segment(segment: &str) -> (ActionClass, Option<String>) {
    let segment = segment.trim();
    if segment.is_empty() {
        return (ActionClass::Noop, None);
    }

    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let start = prefix_start_index(&tokens);

    if start >= tokens.len() {
        return (ActionClass::Noop, None);
    }

    let head = tokens[start];
    let rest = &tokens[start + 1..];

    let class = match head {
        // Test (direct invocation)
        "pytest" | "tox" | "nose" | "jest" | "mocha" | "vitest" | "phpunit" | "rspec" => {
            ActionClass::Test
        }
        // Dispatch commands: test vs build depends on subcommand.
        // Use find_subcommand so global flags (e.g. `cargo --locked test`,
        // `gradle --no-daemon build`, `mvn -q test`) don't mask the subcommand.
        "cargo" => {
            match find_subcommand(
                rest,
                &[
                    "test", "t", "nextest", "build", "b", "check", "c", "clippy", "fmt",
                ],
            ) {
                "test" | "t" | "nextest" => ActionClass::Test,
                "build" | "b" | "check" | "c" | "clippy" | "fmt" => ActionClass::Build,
                _ => ActionClass::Other,
            }
        }
        "npm" | "yarn" => match find_subcommand(rest, &["test", "build", "run"]) {
            "test" => ActionClass::Test,
            "build" => ActionClass::Build,
            "run" => {
                let run_pos = rest.iter().position(|&t| t == "run").unwrap_or(rest.len());
                match find_subcommand(&rest[run_pos + 1..], &["build", "test"]) {
                    "build" => ActionClass::Build,
                    "test" => ActionClass::Test,
                    _ => ActionClass::Other,
                }
            }
            _ => ActionClass::Other,
        },
        "go" | "gradle" => match find_subcommand(rest, &["test", "build"]) {
            "test" => ActionClass::Test,
            "build" => ActionClass::Build,
            _ => ActionClass::Other,
        },
        "mvn" => match find_subcommand(rest, &["test", "package"]) {
            "test" => ActionClass::Test,
            "package" => ActionClass::Build,
            _ => ActionClass::Other,
        },
        // Read
        "cat" | "head" | "tail" | "less" | "more" | "bat" | "file" | "wc" | "od" | "xxd"
        | "column" | "ls" => ActionClass::Read,
        // Search
        "grep" | "rg" | "find" | "fd" | "ack" | "ag" | "locate" => ActionClass::Search,
        // Write (direct head match)
        "sed" | "awk" | "tee" | "patch" | "dd" => ActionClass::Write,
        // Write only when followed by output redirection (handles `echo > f` and `echo foo>f`)
        "echo" => {
            if rest.iter().any(|t| t.contains('>')) {
                ActionClass::Write
            } else {
                ActionClass::Other
            }
        }
        // Nav
        "cd" | "pwd" | "which" | "whereis" | "pushd" | "popd" | "dirs" => ActionClass::Nav,
        // Git
        "git" => ActionClass::Git,
        // Build (standalone tools)
        "make" | "ninja" | "cmake" | "tsc" => ActionClass::Build,
        // Not in any known class
        _ => ActionClass::Other,
    };

    let other_head = if class == ActionClass::Other {
        Some(head.to_owned())
    } else {
        None
    };

    (class, other_head)
}

/// Return the index of the first non-prefix token (strips sudo, time, env VAR=val, VAR=val).
fn prefix_start_index(tokens: &[&str]) -> usize {
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];

        if token == "sudo" {
            i += 1;
            while i < tokens.len() && tokens[i].starts_with('-') {
                let flag = tokens[i];
                i += 1;
                // Short and long sudo flags that each take one argument
                let takes_arg = [
                    "-u",
                    "-g",
                    "-p",
                    "-C",
                    "-R",
                    "-T",
                    "--user",
                    "--group",
                    "--prompt",
                    "--chdir",
                    "--other-user",
                    "--host",
                ]
                .contains(&flag);
                if takes_arg && i < tokens.len() && !tokens[i].starts_with('-') {
                    i += 1;
                }
            }
            continue;
        }

        if token == "time" {
            i += 1;
            // time flags (e.g. -p for POSIX format) never take arguments
            while i < tokens.len() && tokens[i].starts_with('-') {
                i += 1;
            }
            continue;
        }

        if token == "env" {
            i += 1;
            while i < tokens.len() {
                let t = tokens[i];
                if !t.starts_with('-') && t.contains('=') {
                    // VAR=val assignment
                    i += 1;
                } else if matches!(
                    t,
                    "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string"
                ) {
                    // env flags that each take one argument
                    i += 1;
                    if i < tokens.len() {
                        i += 1;
                    }
                } else if t.starts_with('-') {
                    // boolean env flag (e.g. -i / --ignore-environment)
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }

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

        return i;
    }
    tokens.len()
}

// ── shared helpers (mirrors command_stats / triage) ───────────────────────────

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
            "behavior: --filter expects key=value (e.g. failure_category=model_parse)".into(),
        )));
    };
    let (key, value) = (key.trim(), value.trim());
    match key {
        "resolved" => {
            if value != "true" && value != "false" {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "behavior: resolved filter must be `true` or `false`".into(),
                )));
            }
            Ok(resolved == Some(value == "true"))
        }
        "failure_category" => Ok(instance
            .failure_category
            .is_some_and(|fc| failure_category_label(fc) == value)),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "behavior: unsupported filter key `{other}`; supported: `resolved`, `failure_category`"
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
        let mut run_paths = Vec::new();
        let mut n = 1usize;
        loop {
            let p = instance_dir.join(format!("run-{n}.traj.json"));
            if !p.exists() {
                break;
            }
            run_paths.push(p);
            n += 1;
        }
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

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
