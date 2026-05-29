//! `bench contamination-check`: score resolved instances for training-leakage signals.
//!
//! Zero-cost: reads only on-disk trajectory artifacts. No model calls, no network.
//!
//! # Signals
//!
//! | Signal | Description | Range |
//! |--------|-------------|-------|
//! | `edit_before_read_ratio` | Fraction of edited files never read before patching | [0,1] |
//! | `patch_similarity_to_gold` | Normalised similarity between agent patch and gold patch | [0,1] |
//! | `time_to_first_edit` | Suspicion from early first edit (step 0 → 1.0, last step → 0.0) | [0,1] |
//! | `verbatim_recall` | Whether agent message contains ≥ N tokens from gold patch surface | {0,1} |
//!
//! # Risk tiers
//!
//! Weighted sum of signals is clamped to `[0.0, 1.0]`:
//! - **low**: score < `medium_threshold` (default 0.30)
//! - **medium**: `medium_threshold` ≤ score < `high_threshold` (default 0.60)
//! - **high**: score ≥ `high_threshold`

#![allow(clippy::cast_precision_loss)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

// ── public configuration (TOML-configurable) ──────────────────────────────

/// Signal weights and thresholds — overridable via `--config` TOML.
///
/// All weights should sum to approximately 1.0 for meaningful scores, but this
/// is not enforced; raw weighted sums are clamped to `[0.0, 1.0]`.
///
/// `#[serde(default)]` lets a partial TOML file override only the keys it
/// specifies while leaving the rest at their `Default` values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContaminationCheckConfig {
    /// Weight for the edit-before-read ratio signal (default 0.40).
    /// Rationale: file edits without prior reads strongly resemble memorised patches.
    pub weight_edit_before_read: f64,

    /// Weight for the patch-similarity-to-gold signal (default 0.30).
    /// Rationale: near-identical agent patch and gold patch is a direct leakage indicator.
    pub weight_patch_similarity: f64,

    /// Weight for the time-to-first-edit signal (default 0.20).
    /// Rationale: zero-step-to-edit behaviour skips the exploration a genuine solver shows.
    pub weight_time_to_first_edit: f64,

    /// Weight for the verbatim-recall signal (default 0.10).
    /// Rationale: reproducing gold patch text before reading the relevant file is a strong signal.
    pub weight_verbatim_recall: f64,

    /// Score threshold for the "medium" risk tier (inclusive lower bound, default 0.30).
    pub medium_threshold: f64,

    /// Score threshold for the "high" risk tier (inclusive lower bound, default 0.60).
    pub high_threshold: f64,

    /// Minimum token run (whitespace-split) that must appear in an assistant message
    /// to count as a verbatim-recall hit (default 20).
    pub verbatim_recall_min_tokens: usize,
}

impl Default for ContaminationCheckConfig {
    fn default() -> Self {
        Self {
            weight_edit_before_read: 0.40,
            weight_patch_similarity: 0.30,
            weight_time_to_first_edit: 0.20,
            weight_verbatim_recall: 0.10,
            medium_threshold: 0.30,
            high_threshold: 0.60,
            verbatim_recall_min_tokens: 20,
        }
    }
}

impl ContaminationCheckConfig {
    /// Reject invalid weight or threshold values.
    fn validate(&self) -> Result<(), String> {
        for (name, val) in [
            ("weight_edit_before_read", self.weight_edit_before_read),
            ("weight_patch_similarity", self.weight_patch_similarity),
            ("weight_time_to_first_edit", self.weight_time_to_first_edit),
            ("weight_verbatim_recall", self.weight_verbatim_recall),
        ] {
            if !val.is_finite() || val < 0.0 {
                return Err(format!(
                    "contamination-check: {name} must be a finite non-negative number, got {val}"
                ));
            }
        }
        for (name, val) in [
            ("medium_threshold", self.medium_threshold),
            ("high_threshold", self.high_threshold),
        ] {
            if !val.is_finite() || !(0.0..=1.0).contains(&val) {
                return Err(format!(
                    "contamination-check: {name} must be in [0.0, 1.0], got {val}"
                ));
            }
        }
        if self.medium_threshold >= self.high_threshold {
            return Err(format!(
                "contamination-check: medium_threshold ({}) must be less than \
                 high_threshold ({})",
                self.medium_threshold, self.high_threshold
            ));
        }
        Ok(())
    }

    /// Load from a TOML file. Missing keys fall back to defaults.
    pub fn from_toml_file(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::Io(std::io::Error::other(format!(
                "contamination-check: cannot read config `{}`: {e}",
                path.display()
            )))
        })?;
        let cfg: Self = toml::from_str(&text).map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "contamination-check: invalid config TOML `{}`: {e}",
                path.display()
            )))
        })?;
        cfg.validate()
            .map_err(|msg| Error::Config(crate::error::ConfigError::Invalid(msg)))?;
        Ok(cfg)
    }
}

// ── public API types ──────────────────────────────────────────────────────

/// Arguments for `bench contamination-check`.
#[derive(Debug, Clone)]
pub struct ContaminationCheckArgs {
    /// Completed sweep directory.
    pub sweep_dir: PathBuf,
    /// Output path. Defaults to `<sweep_dir>/contamination.json`.
    pub output: Option<PathBuf>,
    /// Optional TOML config overriding default weights/thresholds.
    pub config: Option<PathBuf>,
    /// Exit non-zero when high-risk share exceeds this fraction. `None` → exit 0 always.
    pub fail_on_high: Option<f64>,
}

/// Three-bucket risk classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskTier {
    Low,
    Medium,
    High,
}

impl std::fmt::Display for RiskTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => f.write_str("low"),
            Self::Medium => f.write_str("medium"),
            Self::High => f.write_str("high"),
        }
    }
}

/// Per-signal breakdown for one instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalBreakdown {
    /// Fraction of edited files that had no prior `cat`/`head`/`tail` read in the trajectory.
    pub edit_before_read_ratio: f64,
    /// Normalised similarity between agent patch and gold patch (0 if no gold patch available).
    pub patch_similarity_to_gold: f64,
    /// Suspicion contribution from how early the first file edit occurred (1.0 = step 0).
    pub time_to_first_edit: f64,
    /// 1.0 when an assistant message contains a verbatim run of ≥ N tokens from gold patch.
    pub verbatim_recall: f64,
}

/// Leakage scoring result for one resolved instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceContaminationRow {
    pub instance_id: String,
    /// Weighted, clamped leakage score in [0.0, 1.0].
    pub leakage_score: f64,
    /// Risk tier derived from `leakage_score`.
    pub risk_tier: RiskTier,
    /// Per-signal values before weighting.
    pub signals: SignalBreakdown,
}

/// Sweep-level summary counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContaminationSummary {
    pub total_resolved: usize,
    pub low_count: usize,
    pub medium_count: usize,
    pub high_count: usize,
    /// `high_count / total_resolved`, or 0.0 when `total_resolved == 0`.
    pub high_risk_share: f64,
    /// `(total_resolved - high_count) / total_resolved`, or 1.0 when empty.
    pub contamination_adjusted_resolved_rate: f64,
}

/// Full contamination report returned by [`run`] and written to `contamination.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContaminationReport {
    /// Schema identifier for machine-readable consumers.
    pub schema: String,
    /// Absolute path to the sweep directory.
    pub sweep_path: String,
    /// Per-instance rows, sorted lexicographically by `instance_id`.
    pub instances: Vec<InstanceContaminationRow>,
    /// Sweep-level aggregate counts.
    pub summary: ContaminationSummary,
}

// ── public signal helpers (exported for unit tests) ───────────────────────

/// Compute edit-before-read ratio.
///
/// Returns the fraction of `edits` whose path was never in `reads_before_edit`.
/// Returns `0.0` when `edits` is empty (no suspicion if no edits).
pub fn compute_edit_before_read_ratio<S: std::hash::BuildHasher>(
    reads_before_edit: &HashSet<String, S>,
    edits: &[String],
) -> f64 {
    if edits.is_empty() {
        return 0.0;
    }
    let unread_edits = edits
        .iter()
        .filter(|p| !reads_before_edit.contains(*p))
        .count();
    unread_edits as f64 / edits.len() as f64
}

/// Compute the time-to-first-edit suspicion signal.
///
/// - `first_edit_step`: `None` means no edit was found → returns `0.0`.
/// - Higher return value = more suspicious (edit happened very early).
/// - Formula: `1.0 - (first_edit_step / total_steps)`, clamped to `[0, 1]`.
pub fn compute_time_to_first_edit_signal(
    first_edit_step: Option<usize>,
    total_steps: usize,
) -> f64 {
    match first_edit_step {
        None => 0.0,
        Some(step) if total_steps == 0 => {
            // Single-step trajectory that edited: treat as maximally suspicious.
            if step == 0 { 1.0 } else { 0.0 }
        }
        Some(step) => {
            let frac = step as f64 / (total_steps.saturating_sub(1).max(1)) as f64;
            (1.0 - frac).clamp(0.0, 1.0)
        }
    }
}

/// Classify a `leakage_score` into a `RiskTier` given thresholds.
pub fn score_to_tier(score: f64, cfg: &ContaminationCheckConfig) -> RiskTier {
    if score >= cfg.high_threshold {
        RiskTier::High
    } else if score >= cfg.medium_threshold {
        RiskTier::Medium
    } else {
        RiskTier::Low
    }
}

// ── main entry point ──────────────────────────────────────────────────────

/// Run the contamination check and write `contamination.json` (or `args.output`).
///
/// # Errors
/// * `Error::Io` — sweep directory missing, trajectory unreadable.
/// * `Error::Config` — malformed TOML config.
pub fn run(args: &ContaminationCheckArgs) -> Result<ContaminationReport, Error> {
    let cfg = match &args.config {
        Some(path) => ContaminationCheckConfig::from_toml_file(path)?,
        None => ContaminationCheckConfig::default(),
    };

    let sweep_dir = &args.sweep_dir;
    let results = load_sweep_results(sweep_dir)?;

    // Collect resolved instance IDs, sorted for determinism.
    let mut resolved_ids: Vec<String> = results
        .keys()
        .filter(|id| *results.get(*id).unwrap_or(&false))
        .cloned()
        .collect();
    resolved_ids.sort();

    let mut rows: Vec<InstanceContaminationRow> = Vec::new();
    for instance_id in &resolved_ids {
        let row = score_instance(instance_id, sweep_dir, &cfg)?;
        rows.push(row);
    }

    let total = rows.len();
    let low = rows.iter().filter(|r| r.risk_tier == RiskTier::Low).count();
    let medium = rows
        .iter()
        .filter(|r| r.risk_tier == RiskTier::Medium)
        .count();
    let high = rows
        .iter()
        .filter(|r| r.risk_tier == RiskTier::High)
        .count();

    let high_risk_share = if total == 0 {
        0.0
    } else {
        high as f64 / total as f64
    };
    let contamination_adjusted_resolved_rate = if total == 0 {
        1.0
    } else {
        (total - high) as f64 / total as f64
    };

    let report = ContaminationReport {
        schema: "contamination-check-v1".into(),
        sweep_path: sweep_dir
            .canonicalize()
            .unwrap_or_else(|_| sweep_dir.clone())
            .display()
            .to_string(),
        instances: rows,
        summary: ContaminationSummary {
            total_resolved: total,
            low_count: low,
            medium_count: medium,
            high_count: high,
            high_risk_share,
            contamination_adjusted_resolved_rate,
        },
    };

    // Write output
    let out_path = args
        .output
        .clone()
        .unwrap_or_else(|| sweep_dir.join("contamination.json"));
    let json = serde_json::to_string_pretty(&report).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "contamination-check: JSON serialisation failed: {e}"
        )))
    })?;
    std::fs::write(&out_path, &json).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "contamination-check: cannot write `{}`: {e}",
            out_path.display()
        )))
    })?;

    Ok(report)
}

// ── internal helpers ──────────────────────────────────────────────────────

/// Load a map of `instance_id → is_resolved` from the sweep's `results.json`.
fn load_sweep_results(sweep_dir: &Path) -> Result<HashMap<String, bool>, Error> {
    let results_path = sweep_dir.join("results.json");
    let text = std::fs::read_to_string(&results_path).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "contamination-check: cannot read `{}`: {e}",
            results_path.display()
        )))
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "contamination-check: malformed results.json: {e}"
        )))
    })?;

    let instances = value["instances"].as_array().ok_or_else(|| {
        Error::Io(std::io::Error::other(format!(
            "contamination-check: `{}` is missing or has a non-array `instances` field",
            results_path.display()
        )))
    })?;

    let mut map = HashMap::new();
    for inst in instances {
        let id = match inst["instance_id"].as_str() {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let resolved = inst["resolved_count"].as_u64().unwrap_or(0) > 0
            || inst["pass_at_1"].as_bool().unwrap_or(false)
            // Legacy format (pre-runs field): when `resolved_count` is absent, fall back to
            // outcome=="submitted" with no failure_category as the resolved signal.
            || (inst["resolved_count"].is_null()
                && inst["outcome"].as_str() == Some("submitted")
                && inst["failure_category"].is_null());
        map.insert(id, resolved);
    }
    Ok(map)
}

/// Score a single resolved instance.
fn score_instance(
    instance_id: &str,
    sweep_dir: &Path,
    cfg: &ContaminationCheckConfig,
) -> Result<InstanceContaminationRow, Error> {
    let traj = load_trajectory(sweep_dir, instance_id)?;
    let gold_patch = load_gold_patch(sweep_dir, instance_id);

    let signals = compute_signals(&traj, gold_patch.as_deref(), cfg);
    let leakage_score = compute_weighted_score(&signals, cfg);
    let risk_tier = score_to_tier(leakage_score, cfg);

    Ok(InstanceContaminationRow {
        instance_id: instance_id.to_owned(),
        leakage_score,
        risk_tier,
        signals,
    })
}

/// Raw trajectory representation — only the fields we need.
#[derive(Debug)]
struct TrajData {
    messages: Vec<TrajMessage>,
    total_steps: usize,
}

#[derive(Debug)]
struct TrajMessage {
    role: String,
    #[allow(dead_code)]
    content: String,
    actions: Vec<String>,
}

/// Load trajectory from `<sweep>/<id>/run-1.traj.json` or legacy `<sweep>/<id>.traj.json`.
fn load_trajectory(sweep_dir: &Path, instance_id: &str) -> Result<TrajData, Error> {
    // Try nested path first, then legacy.
    let nested = sweep_dir.join(instance_id).join("run-1.traj.json");
    let legacy = sweep_dir.join(format!("{instance_id}.traj.json"));

    let path = if nested.exists() {
        nested
    } else if legacy.exists() {
        legacy
    } else {
        tracing::warn!(
            instance_id = %instance_id,
            "contamination-check: trajectory not found; scoring as zero (checked {:?} and {:?})",
            sweep_dir.join(instance_id).join("run-1.traj.json"),
            sweep_dir.join(format!("{instance_id}.traj.json")),
        );
        return Ok(TrajData {
            messages: Vec::new(),
            total_steps: 0,
        });
    };

    let text = std::fs::read_to_string(&path).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "contamination-check: cannot read trajectory `{}`: {e}",
            path.display()
        )))
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "contamination-check: malformed trajectory `{}`: {e}",
            path.display()
        )))
    })?;

    let total_steps =
        usize::try_from(value["info"]["steps"].as_u64().unwrap_or(0)).unwrap_or(usize::MAX);

    let mut messages = Vec::new();
    if let Some(msgs) = value["messages"].as_array() {
        for msg in msgs {
            let role = msg["role"].as_str().unwrap_or("").to_owned();
            let content = msg["content"].as_str().unwrap_or("").to_owned();
            let actions: Vec<String> = msg["extra"]["actions"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            messages.push(TrajMessage {
                role,
                content,
                actions,
            });
        }
    }

    Ok(TrajData {
        messages,
        total_steps,
    })
}

/// Try to load the gold patch from `<sweep>/<id>/run-1.patch` or `<sweep>/<id>.patch`.
fn load_gold_patch(sweep_dir: &Path, instance_id: &str) -> Option<String> {
    let nested = sweep_dir.join(instance_id).join("run-1.patch");
    let legacy = sweep_dir.join(format!("{instance_id}.patch"));
    if nested.exists() {
        std::fs::read_to_string(nested).ok()
    } else if legacy.exists() {
        std::fs::read_to_string(legacy).ok()
    } else {
        None
    }
}

/// Compute all four signals from a trajectory.
fn compute_signals(
    traj: &TrajData,
    _gold_patch: Option<&str>,
    _cfg: &ContaminationCheckConfig,
) -> SignalBreakdown {
    // Signal 1: edit-before-read ratio (ordered walk).
    let ebr = compute_ebr_ordered(&traj.messages);

    // Signal 2: patch similarity to gold (0.0 — requires dataset, not trajectory-only).

    // Signal 3: time-to-first-edit.
    let (first_edit_step, step_count) = find_first_edit_step(&traj.messages);
    let total_steps = if traj.total_steps > 0 {
        traj.total_steps
    } else {
        step_count.max(1)
    };
    let ttfe = compute_time_to_first_edit_signal(first_edit_step, total_steps);

    // Signal 4: verbatim recall (0.0 — requires gold patch text, not trajectory-only).

    SignalBreakdown {
        edit_before_read_ratio: ebr,
        patch_similarity_to_gold: 0.0,
        time_to_first_edit: ttfe,
        verbatim_recall: 0.0,
    }
}

/// Return `(first_edit_step, total_assistant_steps)` by walking messages.
fn find_first_edit_step(messages: &[TrajMessage]) -> (Option<usize>, usize) {
    let mut first_edit_step: Option<usize> = None;
    let mut step_index = 0usize;
    for msg in messages {
        if msg.role != "assistant" {
            continue;
        }
        for action in &msg.actions {
            for part in split_compound_action(action.as_str()) {
                if is_write_action(part) && first_edit_step.is_none() {
                    first_edit_step = Some(step_index);
                }
            }
        }
        step_index += 1;
    }
    (first_edit_step, step_index)
}

/// Walk messages in order, tracking which files were read before the first edit.
/// Returns the edit-before-read ratio.
///
/// Compound shell commands (`cmd1 && cmd2`, `cmd1 | cmd2`) are split and each
/// part is classified independently so that a combined read+write command (e.g.
/// `cat a.py && sed -i 's/x/y/' a.py`) records both the read and the write.
///
/// For redirect-based writes (`echo foo > file`), only the redirect target is
/// counted as an edited file — the other arguments of the command are not
/// falsely added to the read or edited sets.
fn compute_ebr_ordered(messages: &[TrajMessage]) -> f64 {
    let mut reads_before: HashSet<String> = HashSet::new();
    let mut edited_unread: HashSet<String> = HashSet::new();
    let mut edited_all: HashSet<String> = HashSet::new();

    for msg in messages {
        if msg.role != "assistant" {
            continue;
        }
        for action in &msg.actions {
            for part in split_compound_action(action.as_str()) {
                // Read branch: collect read inputs, but skip redirect-output targets so
                // that `cat <<'EOF' > dst.py` does not falsely mark dst.py as pre-read.
                if is_read_action(part) {
                    let redirect_targets: HashSet<String> =
                        extract_redirect_targets(part).into_iter().collect();
                    for p in extract_file_args(part) {
                        if !redirect_targets.contains(&p) {
                            reads_before.insert(p);
                        }
                    }
                }

                // Write branch: for redirect-based writes use only the redirect target;
                // for tool writes (sed -i, tee, edit, …) use all file args.
                if is_write_action(part) {
                    let written: Vec<String> = if is_write_tool(part) {
                        extract_file_args(part)
                    } else {
                        extract_redirect_targets(part)
                    };
                    for p in written {
                        if edited_all.insert(p.clone()) && !reads_before.contains(&p) {
                            edited_unread.insert(p);
                        }
                    }
                }
            }
        }
    }

    compute_edit_before_read_ratio(
        &{
            let reads_before_edit: HashSet<String> = edited_all
                .iter()
                .filter(|p| !edited_unread.contains(*p))
                .cloned()
                .collect();
            reads_before_edit
        },
        &edited_all.into_iter().collect::<Vec<_>>(),
    )
}

/// Weighted sum of signal values, clamped to `[0.0, 1.0]`.
fn compute_weighted_score(signals: &SignalBreakdown, cfg: &ContaminationCheckConfig) -> f64 {
    // Using explicit additions to avoid the `mul_add` pedantic lint while keeping readability.
    let ebr = cfg.weight_edit_before_read * signals.edit_before_read_ratio;
    let ps = cfg.weight_patch_similarity * signals.patch_similarity_to_gold;
    let ttfe = cfg.weight_time_to_first_edit * signals.time_to_first_edit;
    let vr = cfg.weight_verbatim_recall * signals.verbatim_recall;
    (ebr + ps + ttfe + vr).clamp(0.0, 1.0)
}

// ── compound action splitting ─────────────────────────────────────────────

/// Split a shell action string into individual commands at `&&`, `||`, `|`, `;`
/// without breaking inside single- or double-quoted strings.
///
/// For example:
/// - `"cat a.py && sed -i 's/x/y/' a.py"` → `["cat a.py", "sed -i 's/x/y/' a.py"]`
/// - `"cat a.py | tee b.py"` → `["cat a.py", "tee b.py"]`
/// - `r#"sed -i "s/foo|bar/baz/" a.py"#` → `["sed -i \"s/foo|bar/baz/\" a.py"]` (not split)
fn split_compound_action(action: &str) -> Vec<&str> {
    let bytes = action.as_bytes();
    let n = bytes.len();
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut i = 0usize;

    while i < n {
        if bytes[i] == b'\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            i += 1;
        } else if bytes[i] == b'"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            i += 1;
        } else if in_single_quote || in_double_quote {
            i += 1;
        } else if i + 1 < n
            && ((bytes[i] == b'&' && bytes[i + 1] == b'&')
                || (bytes[i] == b'|' && bytes[i + 1] == b'|'))
        {
            // `&&` or `||` — split and advance two chars
            parts.push(action[start..i].trim());
            start = i + 2;
            i += 2;
        } else if bytes[i] == b'|' || bytes[i] == b';' {
            parts.push(action[start..i].trim());
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }

    let tail = action[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    if parts.is_empty() {
        parts.push(action.trim());
    }
    parts
}

// ── bash action classification ────────────────────────────────────────────

/// True when a bash action string is primarily a file-read operation.
///
/// Includes pager/dump tools as well as file-search tools (`grep`, `rg`, `ag`, …)
/// so that agents that inspect files via search before editing are not penalised.
fn is_read_action(action: &str) -> bool {
    let head = action.split_whitespace().next().unwrap_or("");
    matches!(
        head,
        "cat"
            | "head"
            | "tail"
            | "less"
            | "more"
            | "bat"
            | "nl"
            | "od"
            | "xxd"
            | "wc"
            | "grep"
            | "fgrep"
            | "egrep"
            | "rg"
            | "ag"
            | "ack"
    )
}

/// True when a bash action uses a known write-mutating tool (not counting redirections).
///
/// Used to decide which path-extraction strategy to apply: tool writes use
/// `extract_file_args`, while redirect-only writes use `extract_redirect_targets`.
fn is_write_tool(action: &str) -> bool {
    let tokens: Vec<&str> = action.split_whitespace().collect();
    let head = tokens.first().copied().unwrap_or("");

    // `sed` is a write only when invoked with `-i` / `--in-place[=SUFFIX]`.
    if head == "sed" {
        return tokens
            .iter()
            .any(|t| *t == "-i" || t.starts_with("-i") || t.starts_with("--in-place"));
    }
    if matches!(head, "awk" | "patch" | "tee" | "dd" | "cp" | "mv") {
        return true;
    }
    if head == "git" && tokens.get(1).copied() == Some("apply") {
        return true;
    }
    if matches!(head, "edit" | "write" | "create_file" | "apply_patch") {
        return true;
    }
    if head.starts_with("write:") || head.starts_with("edit:") {
        return true;
    }
    // Script interpreter + heredoc: `python - <<'PY'` / `python3 - <<EOF`
    // Moved here (from is_write_action) so compute_ebr_ordered calls extract_file_args,
    // which returns a sentinel that fires the EBR signal, not just time_to_first_edit.
    if matches!(head, "python" | "python3" | "ruby" | "perl" | "node") && action.contains("<<") {
        return true;
    }
    false
}

/// True when a path should never be treated as a source-file write target.
///
/// Covers pseudo-filesystems and well-known scratch/transient locations so
/// that commands like `rg foo src/ > /tmp/hits.txt` are not scored as edits.
fn is_discardable_path(path: &str) -> bool {
    path.starts_with("/dev/")
        || path.starts_with("/tmp/")
        || path.starts_with("/var/tmp/")
        || path.starts_with("/run/")
        || path.starts_with("/proc/")
        || path.starts_with("/sys/")
}

/// True when a bash action string writes to at least one source file.
///
/// Returns `true` for known write tools (including script interpreter heredocs)
/// **and** for redirect-based writes (`>`, `>>`).
/// Redirects to transient/pseudo-filesystem paths (e.g. `/dev/null`, `/tmp/…`) are
/// excluded — those discard output rather than modifying repo source files.
fn is_write_action(action: &str) -> bool {
    if is_write_tool(action) {
        return true;
    }
    let tokens: Vec<&str> = action.split_whitespace().collect();
    for (i, token) in tokens.iter().enumerate() {
        if *token == ">" || *token == ">>" {
            let target = tokens.get(i + 1).copied().unwrap_or("");
            if !is_discardable_path(target) {
                return true;
            }
        } else if token.starts_with('>') && !token.starts_with(">&") {
            let target = token.trim_start_matches('>');
            if !is_discardable_path(target) {
                return true;
            }
        }
    }
    false
}

/// Extract only the files that are output-redirect targets (`>` / `>>`) of an action.
///
/// Excludes transient/pseudo-filesystem paths (`/dev/`, `/tmp/`, etc.).  Used
/// together with `is_write_tool` to avoid counting non-redirect command arguments
/// (e.g. the input files of `grep … > /tmp/hits.txt`) as edited files.
fn extract_redirect_targets(action: &str) -> Vec<String> {
    let tokens: Vec<&str> = action.split_whitespace().collect();
    let mut targets = Vec::new();
    let mut take_next = false;
    for token in &tokens {
        if take_next {
            take_next = false;
            let unquoted = token.trim_matches(|c: char| c == '\'' || c == '"');
            if !is_discardable_path(unquoted) && looks_like_path(unquoted) {
                targets.push(normalize_path(unquoted));
            }
        } else if *token == ">" || *token == ">>" {
            take_next = true;
        } else if token.starts_with('>') && !token.starts_with(">&") {
            let path = token.trim_start_matches('>');
            let unquoted = path.trim_matches(|c: char| c == '\'' || c == '"');
            if !unquoted.is_empty() && !is_discardable_path(unquoted) && looks_like_path(unquoted) {
                targets.push(normalize_path(unquoted));
            }
        }
    }
    targets
}

/// Heuristically extract file-path arguments from a bash command.
///
/// For `git apply` and `patch` commands the modified source files cannot be
/// inferred from the command-line tokens alone; a synthetic sentinel `<patch>`
/// is returned so that the EBR and time-to-first-edit signals still fire.
///
/// Conservative: only accepts tokens that plausibly are filesystem paths:
/// - Not a shell flag (starts with `-`)
/// - Quoted tokens have their outer quotes stripped; only accepted if result
///   has a common file extension (avoids confusing `'s/old/new/'` with a path)
/// - Not a shell pattern/substitution (contains `=`, `*`, `[`, `{`, `$`, `(`, `)`)
/// - Looks like a path: contains `/` OR ends with a common extension
fn extract_file_args(action: &str) -> Vec<String> {
    let tokens: Vec<&str> = action.split_whitespace().collect();
    if tokens.is_empty() {
        return Vec::new();
    }

    let head = tokens[0];

    // `git apply` and `patch` modify source files not listed on the command line;
    // return a sentinel so the EBR signal fires for unread patch applications.
    if head == "patch" || (head == "git" && tokens.get(1).copied() == Some("apply")) {
        return vec!["<patch>".to_owned()];
    }

    // Script interpreter + heredoc: we cannot parse paths from the body, so use a
    // sentinel that ensures the EBR signal fires (body may write arbitrary files).
    if matches!(head, "python" | "python3" | "ruby" | "perl" | "node") && action.contains("<<") {
        return vec!["<heredoc>".to_owned()];
    }

    // `cp` and `mv` write only to their destination (last positional argument).
    // Strip balanced outer quotes so that `cp fixed.py 'src/a.py'` records
    // `src/a.py` (matching a prior `cat src/a.py` read), not `'src/a.py'`.
    if head == "cp" || head == "mv" {
        for token in tokens.iter().rev().copied() {
            if token.starts_with('-') {
                continue;
            }
            let unquoted = if (token.starts_with('\'') && token.ends_with('\'') && token.len() >= 2)
                || (token.starts_with('"') && token.ends_with('"') && token.len() >= 2)
            {
                &token[1..token.len() - 1]
            } else {
                token
            };
            if looks_like_path(unquoted) && !is_discardable_path(unquoted) {
                return vec![normalize_path(unquoted)];
            }
        }
        return Vec::new();
    }

    // `dd` uses operand syntax: the edited file is specified as `of=<path>`.
    if head == "dd" {
        for token in tokens.iter().skip(1) {
            if let Some(dest) = token.strip_prefix("of=") {
                let unquoted = dest.trim_matches(|c: char| c == '\'' || c == '"');
                if !is_discardable_path(unquoted) && looks_like_path(unquoted) {
                    return vec![normalize_path(unquoted)];
                }
            }
        }
        return Vec::new();
    }

    let mut paths = Vec::new();
    for token in tokens.iter().skip(1) {
        if token.starts_with('-') {
            continue;
        }

        // Strip balanced outer quotes and validate the unquoted form.
        let unquoted: &str =
            if (token.starts_with('\'') && token.ends_with('\'') && token.len() >= 2)
                || (token.starts_with('"') && token.ends_with('"') && token.len() >= 2)
            {
                let inner = &token[1..token.len() - 1];
                // Only promote to a path if it has a known extension — this avoids
                // treating sed patterns like `'s/old/new/'` as paths.
                if !has_common_extension(inner) {
                    continue;
                }
                inner
            } else if token.starts_with('\'') || token.starts_with('"') {
                // Unbalanced quote — skip entirely.
                continue;
            } else {
                token
            };

        // Reject tokens with shell pattern/substitution characters.
        if unquoted.contains('=')
            || unquoted.contains('*')
            || unquoted.contains('[')
            || unquoted.contains('{')
            || unquoted.contains('$')
            || unquoted.contains('(')
            || unquoted.contains(')')
        {
            continue;
        }
        // Exclude transient/pseudo-filesystem paths.
        if is_discardable_path(unquoted) {
            continue;
        }
        // Reject unquoted sed/awk substitution expressions like `s/old/new/` or `y/a/b/`
        // that contain `/` and would otherwise pass `looks_like_path`.
        if unquoted.starts_with("s/") || unquoted.starts_with("y/") {
            continue;
        }
        if looks_like_path(unquoted) {
            paths.push(normalize_path(unquoted));
        }
    }
    paths
}

fn looks_like_path(token: &str) -> bool {
    // Must contain at least one path separator (relative or absolute path)
    // OR end with a well-known source-file extension.
    // Bare names without `/` and without an extension are ambiguous — skip them.
    token.contains('/') || has_common_extension(token)
}

fn has_common_extension(s: &str) -> bool {
    const EXTS: &[&str] = &[
        ".py", ".rs", ".js", ".ts", ".go", ".java", ".c", ".h", ".cpp", ".rb", ".php", ".sh",
        ".md", ".txt", ".toml", ".yaml", ".yml", ".json", ".xml", ".html", ".css",
    ];
    EXTS.iter().any(|ext| s.ends_with(ext))
}

/// Normalise a path string for deduplication (remove leading `./`).
fn normalize_path(p: &str) -> String {
    p.trim_start_matches("./").to_owned()
}
