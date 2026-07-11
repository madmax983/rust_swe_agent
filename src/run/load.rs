use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::SystemTime;

use crate::artifact::{ArtifactCompatibility, ArtifactKind, classify_json_value};
use crate::error::Error;
use crate::run::swebench::{FilterSpec, InstanceResult, ProvenanceManifest, SweepResults};
use crate::trajectory::{Trajectory, outcome};

// ─────────────────────────────────────────────────────────────────────────────

/// back to scanning per-instance `*.traj.json` files when no `results.json`
/// exists, reconstructing minimal `InstanceResult`s. Tolerant of missing
/// newer fields: defaults flow through serde.
pub fn load_run(dir: &Path) -> Result<HashMap<String, InstanceResult>, Error> {
    Ok(load_sweep(dir)?.instances)
}

#[derive(Debug, Clone)]
pub struct LoadedSweep {
    pub instances: HashMap<String, InstanceResult>,
    pub manifest: Option<ProvenanceManifest>,
    pub filter_spec: Option<FilterSpec>,
    pub rate_limit_events: Option<crate::run::rate_limit::RateLimitEvents>,
    pub artifact: Option<ArtifactCompatibility>,
    pub artifact_warnings: Vec<String>,
    pub total_fallbacks: u64,
    pub model_mix: std::collections::BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct LoadedRunSlot {
    pub instance_id: String,
    pub run_index: u32,
    pub result: InstanceResult,
}

#[allow(clippy::too_many_lines)]
pub fn load_sweep(dir: &Path) -> Result<LoadedSweep, Error> {
    let results_path = dir.join("results.json");
    if results_path.exists() {
        let text = std::fs::read_to_string(&results_path)?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        let artifact = classify_json_value(
            &value,
            ArtifactKind::SweepResults,
            results_path.display().to_string(),
        )
        .map_err(|err| Error::Trajectory(err.to_string()))?;
        let artifact_warnings = artifact.warnings.clone();
        let filter_spec_present = value.get("filter_spec").is_some();
        let sweep: SweepResults = serde_json::from_value(value)?;
        let rate_limit_events = sweep.rate_limit_events.clone();
        let total_fallbacks = sweep.total_fallbacks;
        let model_mix = sweep.model_mix.clone();
        let partial_incomplete = sweep
            .manifest
            .as_ref()
            .is_some_and(|m| m.runtime.finished_at_utc.is_none());
        if partial_incomplete {
            let resume_mode = sweep
                .manifest
                .as_ref()
                .is_some_and(manifest_indicates_resume);
            let min_mtime = if resume_mode {
                None
            } else {
                sweep
                    .manifest
                    .as_ref()
                    .and_then(|m| {
                        chrono::DateTime::parse_from_rfc3339(&m.runtime.started_at_utc).ok()
                    })
                    .map(std::convert::Into::into)
            };
            let scanned_slots = scan_trajectory_run_slots(dir, min_mtime)?;
            let (slot_fallbacks, slot_mix) = fallback_totals_from_slots(&scanned_slots);
            let scanned = aggregate_scanned_results(scanned_slots);
            let manifest = sweep.manifest;
            // When trajectory files are fresher than results.json, use scanned
            // instances and re-derive fallback totals from per-run-slot data so
            // model-mix warnings count all reruns, not just the winning slot.
            let (effective_fallbacks, effective_mix) = if scanned.is_empty() {
                (total_fallbacks, model_mix)
            } else {
                (slot_fallbacks, slot_mix)
            };
            return Ok(LoadedSweep {
                instances: if scanned.is_empty() {
                    sweep
                        .instances
                        .into_iter()
                        .map(|r| (r.instance_id.clone(), r))
                        .collect()
                } else {
                    scanned
                },
                manifest,
                filter_spec: if filter_spec_present {
                    Some(sweep.filter_spec)
                } else {
                    None
                },
                rate_limit_events,
                artifact: Some(artifact),
                artifact_warnings,
                total_fallbacks: effective_fallbacks,
                model_mix: effective_mix,
            });
        }
        return Ok(LoadedSweep {
            instances: sweep
                .instances
                .into_iter()
                .map(|r| (r.instance_id.clone(), r))
                .collect(),
            manifest: sweep.manifest,
            filter_spec: if filter_spec_present {
                Some(sweep.filter_spec)
            } else {
                None
            },
            rate_limit_events,
            artifact: Some(artifact),
            artifact_warnings,
            total_fallbacks,
            model_mix,
        });
    }
    if !dir.exists() {
        return Err(Error::Trajectory(format!(
            "compare: directory does not exist: {}",
            dir.display()
        )));
    }

    let slots = scan_trajectory_run_slots(dir, None)?;
    let (total_fallbacks, model_mix) = fallback_totals_from_slots(&slots);
    let out = aggregate_scanned_results(slots);
    Ok(LoadedSweep {
        instances: out,
        manifest: None,
        filter_spec: None,
        rate_limit_events: None,
        artifact: None,
        artifact_warnings: Vec::new(),
        total_fallbacks,
        model_mix,
    })
}

fn manifest_indicates_resume(manifest: &ProvenanceManifest) -> bool {
    manifest.runtime.resume_mode || manifest.cli.argv.iter().any(|arg| arg == "--resume")
}

/// Compute `total_fallbacks` and `model_mix` from per-run-slot data before
/// aggregation, so that reruns with different responding models are all counted.
pub fn fallback_totals_from_slots(
    slots: &[LoadedRunSlot],
) -> (u64, std::collections::BTreeMap<String, usize>) {
    let total_fallbacks: u64 = slots
        .iter()
        .filter_map(|s| s.result.fallback_count)
        .map(u64::from)
        .sum();
    let mut model_mix: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for s in slots {
        if let Some(model) = s.result.final_model.as_deref() {
            *model_mix.entry(model.to_owned()).or_insert(0) += 1;
        }
    }
    (total_fallbacks, model_mix)
}

fn scan_trajectory_run_slots(
    dir: &Path,
    min_mtime: Option<SystemTime>,
) -> Result<Vec<LoadedRunSlot>, Error> {
    let mut instance_dirs: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut root_trajectories: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
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
            instance_dirs.push((instance_id, path));
            continue;
        }
        if !path_passes_mtime(&path, min_mtime) {
            continue;
        }
        let Some(name_str) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        let Some(id) = name_str.strip_suffix(".traj.json") else {
            continue;
        };
        root_trajectories.push((id.to_owned(), path));
    }
    instance_dirs.sort_by(|a, b| a.0.cmp(&b.0));
    root_trajectories.sort_by(|a, b| a.0.cmp(&b.0));

    let mut scanned = Vec::new();
    for (instance_id, path) in instance_dirs {
        scan_nested_run_trajectories(&path, &instance_id, min_mtime, &mut scanned)?;
    }
    for (id, path) in root_trajectories {
        if let Some(result) = instance_result_from_trajectory(&id, &path)? {
            scanned.push(LoadedRunSlot {
                instance_id: id,
                run_index: 1,
                result,
            });
        }
    }
    Ok(dedupe_run_slots(scanned))
}

fn scan_nested_run_trajectories(
    instance_dir: &Path,
    instance_id: &str,
    min_mtime: Option<SystemTime>,
    out: &mut Vec<LoadedRunSlot>,
) -> Result<(), Error> {
    let mut run_trajectories: Vec<(u32, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(instance_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || !path_passes_mtime(&path, min_mtime) {
            continue;
        }
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        let Some(run_index) = name
            .strip_prefix("run-")
            .and_then(|s| s.strip_suffix(".traj.json"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        run_trajectories.push((run_index, path));
    }
    run_trajectories.sort_by_key(|(run_index, _)| *run_index);
    for (run_index, path) in run_trajectories {
        if let Some(result) = instance_result_from_trajectory(instance_id, &path)? {
            out.push(LoadedRunSlot {
                instance_id: instance_id.to_owned(),
                run_index,
                result,
            });
        }
    }
    Ok(())
}

fn dedupe_run_slots(scanned: Vec<LoadedRunSlot>) -> Vec<LoadedRunSlot> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::with_capacity(scanned.len());
    for slot in scanned {
        let key = (slot.instance_id.clone(), slot.run_index);
        if seen.insert(key) {
            deduped.push(slot);
        }
    }
    deduped
}

fn path_passes_mtime(path: &Path, min_mtime: Option<SystemTime>) -> bool {
    let Some(min) = min_mtime else {
        return true;
    };
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .is_some_and(|modified| modified >= min)
}

fn instance_result_from_trajectory(
    instance_id: &str,
    path: &Path,
) -> Result<Option<InstanceResult>, Error> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string())
        .map_err(|err| Error::Trajectory(err.to_string()))?;
    let traj: Trajectory = match serde_json::from_value(value) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let info = traj.info;
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
    let resolved =
        info.outcome.as_deref() == Some(outcome::SUBMITTED) && info.failure_category.is_none();
    Ok(Some(InstanceResult {
        instance_id: instance_id.to_owned(),
        exit_reason: info.exit_reason.clone().unwrap_or_default(),
        outcome: info.outcome.clone(),
        failure_category: info.failure_category,
        steps: info.steps,
        cost_usd: info.actual_cost_usd.or(info.total_cost_usd),
        prompt_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        completion_tokens,
        duration_secs: info.duration_secs,
        error: None,
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: u32::from(resolved),
        pass_at_1: resolved,
        tests_run_before_submit: info.tests_run_before_submit,
        last_tests_passed: info.last_tests_passed,
        fallback_count: info.fallback_summary.as_ref().map(|s| s.fallback_count),
        // Exclude all-failed runs from model_mix — final_model is only the
        // last attempted model when all_failed=true, not a responding model.
        final_model: info.fallback_summary.as_ref().and_then(|s| {
            if s.all_failed {
                None
            } else {
                Some(s.final_model.clone())
            }
        }),
        retry_id: None,
        previous_failure_category: None,
        trace_id: info.trace_id,
        context_pressure: Default::default(),
        peak_memory_bytes: info.peak_memory_bytes,
        cpu_seconds: info.cpu_seconds,
    }))
}

pub fn load_run_slots<S: std::hash::BuildHasher>(
    dir: &Path,
    fallback: &HashMap<String, InstanceResult, S>,
) -> Result<Vec<LoadedRunSlot>, Error> {
    let mut slots = scan_trajectory_run_slots(dir, None)?;
    let active_ids: BTreeSet<&str> = fallback.keys().map(String::as_str).collect();
    slots.retain(|slot| active_ids.contains(slot.instance_id.as_str()));
    let seen_ids: BTreeSet<String> = slots.iter().map(|slot| slot.instance_id.clone()).collect();
    if slots.is_empty() {
        slots.extend(fallback.iter().map(|(instance_id, result)| LoadedRunSlot {
            instance_id: instance_id.clone(),
            run_index: 1,
            result: result.clone(),
        }));
    } else {
        slots.extend(
            fallback
                .iter()
                .filter(|(instance_id, _)| !seen_ids.contains(*instance_id))
                .map(|(instance_id, result)| LoadedRunSlot {
                    instance_id: instance_id.clone(),
                    run_index: 1,
                    result: result.clone(),
                }),
        );
    }
    slots.sort_by(|a, b| {
        a.instance_id
            .cmp(&b.instance_id)
            .then_with(|| a.run_index.cmp(&b.run_index))
    });
    Ok(slots)
}

fn aggregate_scanned_results(scanned: Vec<LoadedRunSlot>) -> HashMap<String, InstanceResult> {
    let mut grouped: BTreeMap<String, Vec<(u32, InstanceResult)>> = BTreeMap::new();
    for slot in scanned {
        grouped
            .entry(slot.instance_id)
            .or_default()
            .push((slot.run_index, slot.result));
    }
    let mut out = HashMap::new();
    for (id, mut rows) in grouped {
        rows.sort_by_key(|(run_index, _)| *run_index);
        let Some((_, first)) = rows.first() else {
            continue;
        };
        let mut aggregate = first.clone();
        aggregate.runs = rows
            .iter()
            .map(|(run_index, _)| *run_index)
            .max()
            .unwrap_or(1);
        aggregate.resolved_count = rows
            .iter()
            .filter(|(_, result)| result.resolved_count > 0)
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        aggregate.pass_at_1 = rows
            .iter()
            .find(|(run_index, _)| *run_index == 1)
            .is_some_and(|(_, result)| result.resolved_count > 0);
        aggregate.tests_run_before_submit = rows
            .iter()
            .any(|(_, result)| result.tests_run_before_submit);
        aggregate.last_tests_passed = rows
            .iter()
            .rev()
            .find_map(|(_, result)| result.last_tests_passed);
        aggregate.cost_usd = optional_sum(rows.iter().filter_map(|(_, result)| result.cost_usd));
        // Sum fallback counts across all run slots; keep final_model from the
        // first (pass@1 representative) run.
        aggregate.fallback_count = Some(
            rows.iter()
                .filter_map(|(_, result)| result.fallback_count)
                .fold(0u32, u32::saturating_add),
        );
        aggregate.final_model.clone_from(&first.final_model);
        aggregate.prompt_tokens = Some(
            rows.iter()
                .filter_map(|(_, result)| result.prompt_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.cache_read_tokens = Some(
            rows.iter()
                .filter_map(|(_, result)| result.cache_read_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.cache_creation_tokens = Some(
            rows.iter()
                .filter_map(|(_, result)| result.cache_creation_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.completion_tokens = Some(
            rows.iter()
                .filter_map(|(_, result)| result.completion_tokens)
                .fold(0u64, u64::saturating_add),
        );
        out.insert(id, aggregate);
    }
    out
}

pub fn optional_sum(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut seen = false;
    let mut total = 0.0;
    for value in values {
        seen = true;
        total += value;
    }
    seen.then_some(total)
}
