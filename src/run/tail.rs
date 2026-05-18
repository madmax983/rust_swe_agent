//! `bench tail`: read-only live snapshots for a SWE-bench sweep directory.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Serialize;

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::cost::{BASELINE_COST_MODEL, estimate_cost_usd, is_free_tier_model};
use crate::error::Error;
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
    pub status: String,
    pub cancelling_seconds_left: Option<i64>,
    pub completed: usize,
    pub in_flight: usize,
    pub pending: usize,
    pub total: usize,
    pub failure_counts: BTreeMap<FailureCategory, usize>,
    pub cumulative_cost_usd: f64,
    pub baseline_cumulative_cost_usd: f64,
    pub burn_rate_usd_per_min: f64,
    pub eta_seconds: Option<i64>,
    pub budget_cap_usd: Option<f64>,
    pub pct_of_cap_used: Option<f64>,
    pub started_at: Option<String>,
    pub last_event_at: Option<String>,
    pub is_complete: bool,
    pub abort_reason: Option<String>,
    pub warnings: Vec<String>,
    pub total_fallbacks: u64,
    pub model_mix: BTreeMap<String, usize>,
    /// Non-None when actionable failures are building up or the breaker tripped.
    pub circuit_breaker_status: Option<String>,
    /// Count of trajectories persisted with `partial: true` on disk.
    /// Non-zero when a prior sweep was interrupted and the sweep directory
    /// has not yet been resumed. Zero during a live run (partials are
    /// in-flight, not persisted-and-stale).
    pub partial_persisted: usize,
}

#[derive(Debug, Clone, Default)]
struct SweepMeta {
    total: Option<usize>,
    accounted_count: usize,
    estimated_cost_usd: Option<f64>,
    actual_cost_usd: Option<f64>,
    baseline_cost_usd: Option<f64>,
    baseline_cost_model: Option<String>,
    budget_cap_usd: Option<f64>,
    budget_halted: usize,
    status: Option<String>,
    abort_reason: Option<String>,
    cancel_deadline_at: Option<DateTime<Utc>>,
    in_flight_at_cancel: Option<usize>,
    not_started: Option<usize>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    parallelism: Option<usize>,
    failure_counts: BTreeMap<FailureCategory, usize>,
    total_fallbacks: u64,
    model_mix: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
struct TerminalRecord {
    instance_id: String,
    outcome: Option<String>,
    exit_reason: Option<String>,
    failure_category: Option<FailureCategory>,
    cost_usd: Option<f64>,
    cost_is_legacy_estimate: bool,
    baseline_cost_usd: Option<f64>,
    baseline_cost_model: Option<String>,
    prompt_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    fallback_count: Option<u32>,
    final_model: Option<String>,
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
        if self.cost_usd.is_none() || (self.cost_is_legacy_estimate && other.cost_usd.is_some()) {
            self.cost_usd = other.cost_usd;
            self.cost_is_legacy_estimate = other.cost_is_legacy_estimate;
        }
        if self.baseline_cost_usd.is_none() {
            self.baseline_cost_usd = other.baseline_cost_usd;
        }
        if self.baseline_cost_model.is_none() {
            self.baseline_cost_model
                .clone_from(&other.baseline_cost_model);
        }
        if self.prompt_tokens.is_none() {
            self.prompt_tokens = other.prompt_tokens;
        }
        if self.cache_read_tokens.is_none() {
            self.cache_read_tokens = other.cache_read_tokens;
        }
        if self.cache_creation_tokens.is_none() {
            self.cache_creation_tokens = other.cache_creation_tokens;
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
        self.fallback_count = match (self.fallback_count, other.fallback_count) {
            (None, v) | (v, None) => v,
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
        };
        if self.final_model.is_none() {
            self.final_model.clone_from(&other.final_model);
        }
    }

    fn actual_cost(&self) -> Option<f64> {
        self.cost_usd
    }

    fn baseline_cost(&self, sweep_baseline_model: Option<&str>) -> Option<f64> {
        if let Some(cost) = self.baseline_cost_usd {
            return Some(cost);
        }
        let baseline_model = self
            .baseline_cost_model
            .as_deref()
            .or(sweep_baseline_model)
            .unwrap_or(BASELINE_COST_MODEL);
        Some(estimate_cost_usd(
            self.prompt_tokens?,
            self.cache_read_tokens.unwrap_or(0),
            self.cache_creation_tokens.unwrap_or(0),
            self.completion_tokens?,
            baseline_model,
        ))
    }
}

#[allow(clippy::too_many_lines)]
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

    let scanned_slots = scan_trajectories(sweep_dir, &mut warnings)?;
    // Compute fallback totals from raw per-slot records before merging so that
    // reruns using different fallback models are all counted in the model_mix.
    let (scanned_fallbacks, scanned_mix) = fallback_totals_from_records(scanned_slots.iter());
    for record in scanned_slots {
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

    let sweep_baseline_model = meta.baseline_cost_model.as_deref();
    let record_cost = records
        .values()
        .filter_map(TerminalRecord::actual_cost)
        .sum::<f64>();
    let record_baseline_cost = records
        .values()
        .filter_map(|record| record.baseline_cost(sweep_baseline_model))
        .sum::<f64>();
    let cumulative_cost_usd = if records.is_empty() {
        meta.actual_cost_usd
            .or(meta.estimated_cost_usd)
            .unwrap_or(0.0)
    } else {
        record_cost
    };
    let baseline_cumulative_cost_usd = if records.is_empty() {
        meta.baseline_cost_usd
            .or(meta.estimated_cost_usd)
            .unwrap_or(0.0)
    } else {
        record_baseline_cost
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
    let status_cancelled = meta
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("cancelled"));
    let status_cancelling = meta
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("cancelling"));
    let is_complete = status_complete
        || status_cancelled
        || meta.finished_at.is_some()
        || (total > 0 && completed >= total);
    let abort_reason = abort_reason(&meta, cumulative_cost_usd, completed, total);
    let remaining = total.saturating_sub(completed);
    let running = started.is_some() && !is_complete && abort_reason.is_none();
    let estimated_in_flight = remaining.min(meta.parallelism.unwrap_or(DEFAULT_PARALLELISM));
    let in_flight = if status_cancelling {
        meta.in_flight_at_cancel.unwrap_or(estimated_in_flight)
    } else if running {
        estimated_in_flight
    } else {
        0
    };
    let pending = if status_cancelling {
        meta.not_started
            .unwrap_or_else(|| remaining.saturating_sub(in_flight))
    } else {
        remaining.saturating_sub(in_flight)
    };
    let burn_rate_usd_per_min = burn_rate(records.values(), options);
    let eta_seconds = eta_seconds(started, options.now, completed, total, is_complete);
    let pct_of_cap_used = meta
        .budget_cap_usd
        .filter(|cap| *cap > 0.0)
        .map(|cap| (cumulative_cost_usd / cap) * 100.0);
    let status = meta.status.clone().unwrap_or_else(|| {
        if is_complete {
            "completed".to_owned()
        } else {
            "running".to_owned()
        }
    });
    let cancelling_seconds_left = if status_cancelling {
        meta.cancel_deadline_at.map(|deadline| {
            deadline
                .signed_duration_since(options.now)
                .num_seconds()
                .max(0)
        })
    } else {
        None
    };

    // During an in-progress sweep results.json hasn't been written yet, so
    // meta.total_fallbacks and meta.model_mix are zero/empty. Derive live
    // totals from the scanned trajectory records instead.
    let (total_fallbacks, model_mix) =
        if meta.total_fallbacks == 0 && meta.model_mix.is_empty() && !records.is_empty() {
            (scanned_fallbacks, scanned_mix)
        } else {
            (meta.total_fallbacks, std::mem::take(&mut meta.model_mix))
        };

    let circuit_breaker_status = circuit_breaker_status_line(&meta, &failure_counts, completed);
    Ok(TailSnapshot {
        sweep_dir: sweep_dir.to_path_buf(),
        status,
        cancelling_seconds_left,
        completed,
        in_flight,
        pending,
        total,
        failure_counts,
        cumulative_cost_usd,
        baseline_cumulative_cost_usd,
        burn_rate_usd_per_min,
        eta_seconds,
        budget_cap_usd: meta.budget_cap_usd,
        pct_of_cap_used,
        started_at: started.map(format_ts),
        last_event_at: last_event.map(format_ts),
        is_complete,
        abort_reason,
        warnings,
        total_fallbacks,
        model_mix,
        circuit_breaker_status,
        partial_persisted: count_partial_trajectories(sweep_dir),
    })
}

fn count_partial_trajectories(sweep_dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(sweep_dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(nested) = std::fs::read_dir(&path) else {
            continue;
        };
        for nested_entry in nested.flatten() {
            let nested_path = nested_entry.path();
            let name = nested_path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("");
            if name.starts_with("run-") && name.ends_with(".traj.json") {
                if let Ok(text) = std::fs::read_to_string(&nested_path) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if v.pointer("/info/partial")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                        {
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    count
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
        Ok(v) => {
            let compat =
                classify_json_value(&v, ArtifactKind::SweepResults, path.display().to_string())
                    .map_err(|err| Error::Trajectory(err.to_string()))?;
            warnings.extend(compat.warnings);
            Ok(Some(v))
        }
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
            .or_else(|| get_f64(value, "total_cost_usd"))
            .or_else(|| get_f64(value, "cumulative_cost_usd")),
        actual_cost_usd: get_f64(value, "actual_cost_usd"),
        baseline_cost_usd: get_f64(value, "baseline_cost_usd"),
        baseline_cost_model: get_str(value, "baseline_cost_model").map(ToOwned::to_owned),
        budget_cap_usd: get_f64(value, "budget_cap_usd")
            .or_else(|| get_f64(value, "cost_limit_usd"))
            .or_else(|| get_f64(value, "sweep_cost_limit_usd")),
        budget_halted,
        status: get_str(value, "sweep_status")
            .or_else(|| get_str(value, "status"))
            .map(ToOwned::to_owned),
        abort_reason: get_str(value, "fatal_error")
            .or_else(|| get_str(value, "abort_reason"))
            .or_else(|| get_str(value, "error"))
            .map(ToOwned::to_owned),
        cancel_deadline_at: get_str(value, "cancel_deadline_at").and_then(parse_ts),
        in_flight_at_cancel: get_usize(value, "in_flight_at_cancel"),
        not_started: get_usize(value, "not_started"),
        started_at,
        finished_at,
        parallelism,
        failure_counts: parse_failure_counts(value),
        total_fallbacks: get_u64(value, "total_fallbacks").unwrap_or(0),
        model_mix: value
            .get("model_mix")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| {
                        v.as_u64()
                            .map(|n| (k.clone(), usize::try_from(n).unwrap_or(usize::MAX)))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn parse_result_records(
    value: &serde_json::Value,
    warnings: &mut Vec<String>,
) -> Vec<TerminalRecord> {
    let mut records = Vec::new();
    let result_costs_are_legacy =
        value.get("actual_cost_usd").is_none() && value.get("baseline_cost_usd").is_none();
    for item in value
        .get("instances")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(record) = record_from_result_value(item, result_costs_are_legacy) {
            records.push(record);
        } else {
            warnings.push("results.json: skipped instance entry without instance_id".into());
        }
    }
    records
}

fn record_from_result_value(
    value: &serde_json::Value,
    result_costs_are_legacy: bool,
) -> Option<TerminalRecord> {
    let instance_id = get_str(value, "instance_id")?.to_owned();
    Some(TerminalRecord {
        instance_id,
        outcome: get_str(value, "outcome").map(ToOwned::to_owned),
        exit_reason: get_str(value, "exit_reason").map(ToOwned::to_owned),
        failure_category: parse_failure_category_value(value.get("failure_category")),
        cost_usd: get_f64(value, "actual_cost_usd")
            .or_else(|| get_f64(value, "cost_usd"))
            .or_else(|| get_f64(value, "total_cost_usd"))
            .or_else(|| get_f64(value, "cumulative_cost_usd")),
        cost_is_legacy_estimate: result_costs_are_legacy
            && get_f64(value, "actual_cost_usd").is_none(),
        baseline_cost_usd: get_f64(value, "baseline_cost_usd"),
        baseline_cost_model: get_str(value, "baseline_cost_model").map(ToOwned::to_owned),
        prompt_tokens: get_u64(value, "total_input_tokens")
            .or_else(|| get_u64(value, "prompt_tokens")),
        cache_read_tokens: get_u64(value, "total_cache_read_tokens")
            .or_else(|| get_u64(value, "cache_read_tokens")),
        cache_creation_tokens: get_u64(value, "total_cache_creation_tokens")
            .or_else(|| get_u64(value, "cache_creation_tokens")),
        completion_tokens: get_u64(value, "total_completion_tokens")
            .or_else(|| get_u64(value, "completion_tokens")),
        started_at: get_str(value, "started_at").and_then(parse_ts),
        ended_at: get_str(value, "ended_at")
            .or_else(|| get_str(value, "finished_at"))
            .and_then(parse_ts),
        fallback_count: get_u64(value, "fallback_count")
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX)),
        final_model: get_str(value, "final_model").map(ToOwned::to_owned),
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
                        terminal_record_from_trajectory(&nested_path, &instance_id, warnings)?
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
        if let Some(record) = terminal_record_from_trajectory(&path, instance_id, warnings)? {
            records.push(record);
        }
    }
    Ok(records)
}

fn terminal_record_from_trajectory(
    path: &Path,
    instance_id: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<TerminalRecord>, Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            warnings.push(format!("{}: failed to read ({err})", path.display()));
            return Ok(None);
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            warnings.push(format!(
                "{}: partial or invalid JSON ({err})",
                path.display()
            ));
            return Ok(None);
        }
    };
    match classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string()) {
        Ok(compat) => warnings.extend(compat.warnings),
        Err(err) => return Err(Error::Trajectory(err.to_string())),
    }
    let traj: Trajectory = match serde_json::from_value(value) {
        Ok(traj) => traj,
        Err(err) => {
            warnings.push(format!(
                "{}: partial or invalid JSON ({err})",
                path.display()
            ));
            return Ok(None);
        }
    };
    let info = traj.info;
    if info.partial {
        // Partial trajectories are not terminal records — don't include them
        // in completed/outcome counts. The partial count is tracked separately.
        return Ok(None);
    }
    let actual_cost_usd = info.actual_cost_usd.or_else(|| {
        if info.model_name.as_deref().is_some_and(is_free_tier_model) {
            Some(0.0)
        } else {
            info.total_cost_usd
        }
    });
    let (prompt_tokens, cache_read_tokens, cache_creation_tokens, completion_tokens) = info
        .token_usage
        .as_ref()
        .map_or((None, None, None, None), |t| {
            (
                Some(t.prompt_tokens),
                Some(t.cache_read_tokens),
                Some(t.cache_creation_tokens),
                Some(t.completion_tokens),
            )
        });
    Ok(Some(TerminalRecord {
        instance_id: instance_id.to_owned(),
        outcome: info.outcome,
        exit_reason: info.exit_reason,
        failure_category: info.failure_category,
        cost_usd: actual_cost_usd,
        cost_is_legacy_estimate: false,
        baseline_cost_usd: info.baseline_cost_usd,
        baseline_cost_model: info.baseline_cost_model,
        prompt_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        completion_tokens,
        started_at: info.started_at.as_deref().and_then(parse_ts),
        ended_at: info.ended_at.as_deref().and_then(parse_ts),
        fallback_count: info.fallback_summary.as_ref().map(|s| s.fallback_count),
        // Exclude all-failed runs: no model produced a response, so they
        // should not appear in model_mix.
        final_model: info.fallback_summary.as_ref().and_then(|s| {
            if s.all_failed {
                None
            } else {
                Some(s.final_model.clone())
            }
        }),
    }))
}

fn fallback_totals_from_records<'a>(
    records: impl Iterator<Item = &'a TerminalRecord>,
) -> (u64, BTreeMap<String, usize>) {
    let mut total_fallbacks: u64 = 0;
    let mut model_mix: BTreeMap<String, usize> = BTreeMap::new();
    for record in records {
        if let Some(count) = record.fallback_count {
            total_fallbacks = total_fallbacks.saturating_add(u64::from(count));
        }
        if let Some(ref model) = record.final_model {
            *model_mix.entry(model.clone()).or_insert(0) += 1;
        }
    }
    (total_fallbacks, model_mix)
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
    if meta
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("systemic_halt"))
    {
        return Some(format!(
            "circuit breaker tripped: {} instance(s) not started",
            meta.not_started.unwrap_or(0)
        ));
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

/// Returns a one-line circuit-breaker status string for display in `bench tail`.
///
/// Shows progress toward a trip during a live sweep and the tripped state for
/// a completed one.  Returns `None` when there are no actionable failures and
/// the sweep did not end as `systemic_halt`.
fn circuit_breaker_status_line(
    meta: &SweepMeta,
    failure_counts: &BTreeMap<FailureCategory, usize>,
    completed: usize,
) -> Option<String> {
    if meta
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("systemic_halt"))
    {
        return Some("tripped — sweep halted early".to_owned());
    }

    // Find the dominant actionable category, if any.
    let actionable: Vec<(FailureCategory, usize)> = failure_counts
        .iter()
        .filter(|(cat, _)| cat.is_actionable())
        .map(|(cat, count)| (*cat, *count))
        .collect();
    if actionable.is_empty() || completed == 0 {
        return None;
    }
    let total_actionable: usize = actionable.iter().map(|(_, n)| n).sum();
    let (dominant_cat, dominant_count) = actionable
        .iter()
        .max_by_key(|(_, n)| n)
        .copied()
        .unwrap_or(actionable[0]);
    #[allow(clippy::cast_precision_loss)]
    let share_pct = dominant_count as f64 / completed as f64 * 100.0;
    Some(format!(
        "armed — {total_actionable}/{completed} actionable failure(s), \
         dominant: {} ({share_pct:.0}%)",
        failure_label(dominant_cat),
    ))
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
        .filter_map(TerminalRecord::actual_cost)
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
    if snapshot.partial_persisted > 0 {
        let _ = writeln!(
            out,
            "Partial:     {} — mid-run checkpoint(s) from prior interrupted run (re-run with --resume)",
            snapshot.partial_persisted
        );
    }
    let _ = writeln!(
        out,
        "Actual cost: ${:.4}  burn ${:.4}/min",
        snapshot.cumulative_cost_usd, snapshot.burn_rate_usd_per_min
    );
    let _ = writeln!(
        out,
        "Baseline:    ${:.4}",
        snapshot.baseline_cumulative_cost_usd
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
    if let Some(cb) = &snapshot.circuit_breaker_status {
        let _ = writeln!(out, "Circuit breaker: {cb}");
    }
    if let Some(reason) = &snapshot.abort_reason {
        let _ = writeln!(out, "Abort:       {reason}");
    } else if snapshot.status == "cancelling" {
        let left = snapshot
            .cancelling_seconds_left
            .map_or_else(|| "deadline unknown".to_owned(), format_countdown);
        let _ = writeln!(out, "Status:      cancelling ({left} left)");
    } else if snapshot.status == "cancelled" {
        out.push_str("Status:      cancelled\n");
    } else if snapshot.status == "systemic_halt" {
        out.push_str("Status:      systemic halt (circuit breaker tripped)\n");
    } else if snapshot.is_complete {
        out.push_str("Status:      completed\n");
    } else {
        out.push_str("Status:      running\n");
    }
    if snapshot.total_fallbacks > 0 || !snapshot.model_mix.is_empty() {
        out.push_str("Model mix:\n");
        for (model, count) in &snapshot.model_mix {
            let _ = writeln!(out, "  - {model}: {count}");
        }
        if snapshot.total_fallbacks > 0 {
            let _ = writeln!(out, "Fallbacks:   {}", snapshot.total_fallbacks);
        }
    }
    for warning in &snapshot.warnings {
        let _ = writeln!(out, "Warning:     {warning}");
    }
    out
}

fn format_countdown(secs: i64) -> String {
    let mins = secs / 60;
    let rem = secs % 60;
    format!("{mins}m:{rem:02}s")
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
