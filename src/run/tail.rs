//! `bench tail`: read-only live snapshots for a SWE-bench sweep directory.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Serialize;

use crate::error::Error;
use crate::run::swebench::estimate_cost_usd;
use crate::trajectory::{FailureCategory, Trajectory};

const DEFAULT_PARALLELISM: usize = 4;

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub now: DateTime<Utc>,
    pub burn_rate_window: Duration,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            now: Utc::now(),
            burn_rate_window: Duration::minutes(5),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TailSnapshot {
    pub sweep_dir: PathBuf,
    pub completed: usize,
    pub in_flight: usize,
    pub pending: usize,
    pub total: usize,
    pub failure_counts: BTreeMap<FailureCategory, usize>,
    pub cumulative_cost_usd: f64,
    pub burn_rate_usd_per_min: f64,
    pub eta_seconds: Option<i64>,
    pub budget_cap_usd: Option<f64>,
    pub pct_of_cap_used: Option<f64>,
    pub started_at: Option<String>,
    pub last_event_at: Option<String>,
    pub is_complete: bool,
    pub abort_reason: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct SweepMeta {
    total: Option<usize>,
    accounted_count: usize,
    estimated_cost_usd: Option<f64>,
    budget_cap_usd: Option<f64>,
    budget_halted: usize,
    status: Option<String>,
    abort_reason: Option<String>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    parallelism: Option<usize>,
    failure_counts: BTreeMap<FailureCategory, usize>,
}

#[derive(Debug, Clone)]
struct TerminalRecord {
    instance_id: String,
    outcome: Option<String>,
    exit_reason: Option<String>,
    failure_category: Option<FailureCategory>,
    cost_usd: Option<f64>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
}

impl TerminalRecord {
    fn merge_trajectory(&mut self, other: &Self) {
        if self.outcome.is_none() {
            self.outcome.clone_from(&other.outcome);
        }
        if self.exit_reason.is_none() {
            self.exit_reason.clone_from(&other.exit_reason);
        }
        if self.failure_category.is_none() {
            self.failure_category = other.failure_category;
        }
        if self.cost_usd.is_none() {
            self.cost_usd = other.cost_usd;
        }
        if self.prompt_tokens.is_none() {
            self.prompt_tokens = other.prompt_tokens;
        }
        if self.completion_tokens.is_none() {
            self.completion_tokens = other.completion_tokens;
        }
        if self.started_at.is_none() {
            self.started_at = other.started_at;
        }
        if self.ended_at.is_none() {
            self.ended_at = other.ended_at;
        }
    }

    fn cost(&self) -> Option<f64> {
        self.cost_usd.or_else(|| {
            Some(estimate_cost_usd(
                self.prompt_tokens?,
                self.completion_tokens?,
            ))
        })
    }
}

pub fn snapshot(sweep_dir: &Path, options: &SnapshotOptions) -> Result<TailSnapshot, Error> {
    if !sweep_dir.exists() {
        return Err(Error::Trajectory(format!(
            "tail: sweep directory does not exist: {}",
            sweep_dir.display()
        )));
    }

    let mut warnings = Vec::new();
    let results_value = read_json_value(&sweep_dir.join("results.json"), &mut warnings)?;
    let mut meta = results_value
        .as_ref()
        .map(parse_sweep_meta)
        .unwrap_or_default();
    let mut records = BTreeMap::new();

    if let Some(value) = results_value.as_ref() {
        for record in parse_result_records(value, &mut warnings) {
            records.insert(record.instance_id.clone(), record);
        }
    }

    for record in scan_trajectories(sweep_dir, &mut warnings)? {
        records
            .entry(record.instance_id.clone())
            .and_modify(|existing| existing.merge_trajectory(&record))
            .or_insert(record);
    }

    let mut total = meta.total.unwrap_or(records.len());
    if total == 0 && !records.is_empty() {
        total = records.len();
    }

    let completed_from_records = records.len();
    let mut completed = completed_from_records.max(meta.accounted_count);
    if total > 0 {
        completed = completed.min(total);
    }

    let mut failure_counts = failure_counts_from_records(records.values());
    if failure_counts.is_empty() {
        failure_counts = std::mem::take(&mut meta.failure_counts);
    }

    let record_cost = records
        .values()
        .filter_map(TerminalRecord::cost)
        .sum::<f64>();
    let cumulative_cost_usd = if records.is_empty() {
        meta.estimated_cost_usd.unwrap_or(0.0)
    } else {
        record_cost
    };

    let started = meta
        .started_at
        .or_else(|| records.values().filter_map(|r| r.started_at).min());
    let last_event = records
        .values()
        .filter_map(|r| r.ended_at.or(r.started_at))
        .max()
        .or(meta.finished_at)
        .or(started);
    let status_complete = meta
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("completed"));
    let is_complete =
        status_complete || meta.finished_at.is_some() || (total > 0 && completed >= total);
    let abort_reason = abort_reason(&meta, cumulative_cost_usd, completed, total);
    let remaining = total.saturating_sub(completed);
    let running = started.is_some() && !is_complete && abort_reason.is_none();
    let in_flight = if running {
        remaining.min(meta.parallelism.unwrap_or(DEFAULT_PARALLELISM))
    } else {
        0
    };
    let pending = remaining.saturating_sub(in_flight);
    let burn_rate_usd_per_min = burn_rate(records.values(), options);
    let eta_seconds = eta_seconds(started, options.now, completed, total, is_complete);
    let pct_of_cap_used = meta
        .budget_cap_usd
        .filter(|cap| *cap > 0.0)
        .map(|cap| (cumulative_cost_usd / cap) * 100.0);

    Ok(TailSnapshot {
        sweep_dir: sweep_dir.to_path_buf(),
        completed,
        in_flight,
        pending,
        total,
        failure_counts,
        cumulative_cost_usd,
        burn_rate_usd_per_min,
        eta_seconds,
        budget_cap_usd: meta.budget_cap_usd,
        pct_of_cap_used,
        started_at: started.map(format_ts),
        last_event_at: last_event.map(format_ts),
        is_complete,
        abort_reason,
        warnings,
    })
}

fn read_json_value(
    path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Option<serde_json::Value>, Error> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    match serde_json::from_str(&text) {
        Ok(v) => Ok(Some(v)),
        Err(err) => {
            warnings.push(format!(
                "{}: partial or invalid JSON ({err})",
                path.display()
            ));
            Ok(None)
        }
    }
}

fn parse_sweep_meta(value: &serde_json::Value) -> SweepMeta {
    let total = get_usize(value, "total")
        .or_else(|| {
            value
                .pointer("/filter_spec/selected_count")
                .and_then(value_as_usize)
        })
        .or_else(|| {
            value
                .pointer("/manifest/dataset/instance_count")
                .and_then(value_as_usize)
        });
    let accounted_count = ["submitted", "skipped", "errored", "budget_halted"]
        .iter()
        .filter_map(|key| get_usize(value, key))
        .sum();
    let budget_halted = get_usize(value, "budget_halted").unwrap_or(0);
    let started_at = get_str(value, "started_at").and_then(parse_ts).or_else(|| {
        value
            .pointer("/manifest/runtime/started_at_utc")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_ts)
    });
    let finished_at = get_str(value, "finished_at")
        .and_then(parse_ts)
        .or_else(|| {
            value
                .pointer("/manifest/runtime/finished_at_utc")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_ts)
        });
    let parallelism = value
        .pointer("/manifest/cli/argv")
        .and_then(serde_json::Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .and_then(|argv| parse_parallelism(&argv));
    SweepMeta {
        total,
        accounted_count,
        estimated_cost_usd: get_f64(value, "estimated_cost_usd")
            .or_else(|| get_f64(value, "cumulative_cost_usd")),
        budget_cap_usd: get_f64(value, "budget_cap_usd")
            .or_else(|| get_f64(value, "cost_limit_usd"))
            .or_else(|| get_f64(value, "sweep_cost_limit_usd")),
        budget_halted,
        status: get_str(value, "status").map(ToOwned::to_owned),
        abort_reason: get_str(value, "fatal_error")
            .or_else(|| get_str(value, "abort_reason"))
            .or_else(|| get_str(value, "error"))
            .map(ToOwned::to_owned),
        started_at,
        finished_at,
        parallelism,
        failure_counts: parse_failure_counts(value),
    }
}

fn parse_result_records(
    value: &serde_json::Value,
    warnings: &mut Vec<String>,
) -> Vec<TerminalRecord> {
    let mut records = Vec::new();
    for item in value
        .get("instances")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(record) = record_from_result_value(item) {
            records.push(record);
        } else {
            warnings.push("results.json: skipped instance entry without instance_id".into());
        }
    }
    records
}

fn record_from_result_value(value: &serde_json::Value) -> Option<TerminalRecord> {
    let instance_id = get_str(value, "instance_id")?.to_owned();
    Some(TerminalRecord {
        instance_id,
        outcome: get_str(value, "outcome").map(ToOwned::to_owned),
        exit_reason: get_str(value, "exit_reason").map(ToOwned::to_owned),
        failure_category: parse_failure_category_value(value.get("failure_category")),
        cost_usd: get_f64(value, "cost_usd")
            .or_else(|| get_f64(value, "total_cost_usd"))
            .or_else(|| get_f64(value, "cumulative_cost_usd")),
        prompt_tokens: get_u64(value, "prompt_tokens"),
        completion_tokens: get_u64(value, "completion_tokens"),
        started_at: get_str(value, "started_at").and_then(parse_ts),
        ended_at: get_str(value, "ended_at")
            .or_else(|| get_str(value, "finished_at"))
            .and_then(parse_ts),
    })
}

fn scan_trajectories(
    sweep_dir: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<TerminalRecord>, Error> {
    let mut records = Vec::new();
    for entry in std::fs::read_dir(sweep_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let Some(instance_id) = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_owned)
            else {
                continue;
            };
            for nested in std::fs::read_dir(&path)? {
                let nested = nested?;
                let nested_path = nested.path();
                if !nested_path.is_file() {
                    continue;
                }
                let Some(name) = nested_path.file_name().and_then(std::ffi::OsStr::to_str) else {
                    continue;
                };
                if name.starts_with("run-") && name.ends_with(".traj.json") {
                    if let Some(record) =
                        terminal_record_from_trajectory(&nested_path, &instance_id, warnings)
                    {
                        records.push(record);
                    }
                }
            }
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(instance_id) = name.strip_suffix(".traj.json") else {
            continue;
        };
        if let Some(record) = terminal_record_from_trajectory(&path, instance_id, warnings) {
            records.push(record);
        }
    }
    Ok(records)
}

fn terminal_record_from_trajectory(
    path: &Path,
    instance_id: &str,
    warnings: &mut Vec<String>,
) -> Option<TerminalRecord> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            warnings.push(format!("{}: failed to read ({err})", path.display()));
            return None;
        }
    };
    let traj: Trajectory = match serde_json::from_str(&text) {
        Ok(traj) => traj,
        Err(err) => {
            warnings.push(format!(
                "{}: partial or invalid JSON ({err})",
                path.display()
            ));
            return None;
        }
    };
    let info = traj.info;
    let (prompt_tokens, completion_tokens) = info.token_usage.as_ref().map_or((None, None), |t| {
        (Some(t.prompt_tokens), Some(t.completion_tokens))
    });
    Some(TerminalRecord {
        instance_id: instance_id.to_owned(),
        outcome: info.outcome,
        exit_reason: info.exit_reason,
        failure_category: info.failure_category,
        cost_usd: info.total_cost_usd,
        prompt_tokens,
        completion_tokens,
        started_at: info.started_at.as_deref().and_then(parse_ts),
        ended_at: info.ended_at.as_deref().and_then(parse_ts),
    })
}

fn failure_counts_from_records<'a>(
    records: impl Iterator<Item = &'a TerminalRecord>,
) -> BTreeMap<FailureCategory, usize> {
    let mut counts = BTreeMap::new();
    for record in records {
        if let Some(category) = record.failure_category {
            *counts.entry(category).or_insert(0) += 1;
        }
    }
    counts
}

fn parse_failure_counts(value: &serde_json::Value) -> BTreeMap<FailureCategory, usize> {
    let mut counts = BTreeMap::new();
    let Some(obj) = value
        .get("failures_by_category")
        .and_then(serde_json::Value::as_object)
    else {
        return counts;
    };
    for (key, value) in obj {
        let Some(count) = value_as_usize(value) else {
            continue;
        };
        if let Some(category) = parse_failure_category_label(key) {
            counts.insert(category, count);
        }
    }
    counts
}

fn parse_failure_category_value(value: Option<&serde_json::Value>) -> Option<FailureCategory> {
    value.and_then(|v| serde_json::from_value::<FailureCategory>(v.clone()).ok())
}

fn parse_failure_category_label(raw: &str) -> Option<FailureCategory> {
    serde_json::from_value::<FailureCategory>(serde_json::Value::String(raw.to_owned())).ok()
}

fn abort_reason(
    meta: &SweepMeta,
    cumulative_cost_usd: f64,
    completed: usize,
    total: usize,
) -> Option<String> {
    if let Some(reason) = &meta.abort_reason {
        return Some(reason.clone());
    }
    if let Some(status) = meta.status.as_deref() {
        if matches!(
            status.to_ascii_lowercase().as_str(),
            "aborted" | "abort" | "failed" | "fatal" | "error"
        ) {
            return Some(format!("sweep status: {status}"));
        }
    }
    if meta.budget_halted > 0 {
        return Some(format!(
            "budget cap hit: {} instance(s) never started",
            meta.budget_halted
        ));
    }
    if let Some(cap) = meta.budget_cap_usd {
        if cap > 0.0 && cumulative_cost_usd >= cap && completed < total {
            return Some(format!(
                "budget cap hit: ${cumulative_cost_usd:.4} of ${cap:.4} used"
            ));
        }
    }
    None
}

fn burn_rate<'a>(
    records: impl Iterator<Item = &'a TerminalRecord>,
    options: &SnapshotOptions,
) -> f64 {
    let window_secs = options.burn_rate_window.num_seconds().max(1);
    let cutoff = options.now - options.burn_rate_window;
    let cost = records
        .filter(|record| {
            record
                .ended_at
                .is_some_and(|ended_at| ended_at >= cutoff && ended_at <= options.now)
        })
        .filter_map(TerminalRecord::cost)
        .sum::<f64>();
    #[allow(clippy::cast_precision_loss)]
    let window_minutes = window_secs as f64 / 60.0;
    cost / window_minutes
}

fn eta_seconds(
    started_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    completed: usize,
    total: usize,
    is_complete: bool,
) -> Option<i64> {
    if total == 0 {
        return None;
    }
    if is_complete {
        return Some(0);
    }
    if completed == 0 {
        return None;
    }
    let elapsed = now.signed_duration_since(started_at?).num_seconds();
    if elapsed <= 0 {
        return None;
    }
    let remaining = total.saturating_sub(completed);
    #[allow(clippy::cast_possible_wrap)]
    let completed_i64 = completed as i64;
    #[allow(clippy::cast_possible_wrap)]
    let remaining_i64 = remaining as i64;
    Some((remaining_i64 * elapsed + completed_i64 - 1) / completed_i64)
}

pub fn render_text(snapshot: &TailSnapshot) -> String {
    let mut out = String::new();
    out.push_str("\n=== bench tail ===\n");
    let _ = writeln!(out, "Sweep:       {}", snapshot.sweep_dir.display());
    let _ = writeln!(
        out,
        "Progress:    {}/{} completed, {} in flight, {} pending",
        snapshot.completed, snapshot.total, snapshot.in_flight, snapshot.pending
    );
    let _ = writeln!(
        out,
        "Cost:        ${:.4}  burn ${:.4}/min",
        snapshot.cumulative_cost_usd, snapshot.burn_rate_usd_per_min
    );
    if let Some(cap) = snapshot.budget_cap_usd {
        let pct = snapshot.pct_of_cap_used.unwrap_or(0.0);
        let _ = writeln!(out, "Budget cap:  ${cap:.4} ({pct:.1}% used)");
    }
    let eta = snapshot
        .eta_seconds
        .map_or_else(|| "n/a".to_owned(), |secs| format!("{secs}s"));
    let _ = writeln!(out, "ETA:         {eta}");
    let _ = writeln!(
        out,
        "Started:     {}",
        snapshot.started_at.as_deref().unwrap_or("unknown")
    );
    let _ = writeln!(
        out,
        "Last event:  {}",
        snapshot.last_event_at.as_deref().unwrap_or("unknown")
    );
    if snapshot.failure_counts.is_empty() {
        out.push_str("Failures:    none\n");
    } else {
        out.push_str("Failures:\n");
        for (category, count) in &snapshot.failure_counts {
            let _ = writeln!(out, "  - {}: {count}", failure_label(*category));
        }
    }
    if let Some(reason) = &snapshot.abort_reason {
        let _ = writeln!(out, "Abort:       {reason}");
    } else if snapshot.is_complete {
        out.push_str("Status:      completed\n");
    } else {
        out.push_str("Status:      running\n");
    }
    for warning in &snapshot.warnings {
        let _ = writeln!(out, "Warning:     {warning}");
    }
    out
}

fn parse_parallelism(argv: &[String]) -> Option<usize> {
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if let Some(raw) = arg.strip_prefix("--parallel=") {
            return raw.parse().ok().filter(|n| *n > 0);
        }
        if arg == "--parallel" {
            return iter
                .next()
                .and_then(|raw| raw.parse().ok())
                .filter(|n| *n > 0);
        }
    }
    None
}

fn get_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

fn get_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn get_usize(value: &serde_json::Value, key: &str) -> Option<usize> {
    value.get(key).and_then(value_as_usize)
}

fn value_as_usize(value: &serde_json::Value) -> Option<usize> {
    usize::try_from(value.as_u64()?).ok()
}

fn get_f64(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(serde_json::Value::as_f64)
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(std::convert::Into::into)
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn failure_label(category: FailureCategory) -> &'static str {
    match category {
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
