use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::run::args::AuditCmd;

/// Recompute sweep-wide aggregates and reconcile with results.json and evaluation.json.
#[allow(clippy::too_many_lines)]
pub fn run(args: &AuditCmd) -> Result<(), Error> {
    let log_msg = |msg: &str| {
        if args.format == "json" {
            eprintln!("{msg}");
        } else {
            println!("{msg}");
        }
    };

    let results_path = args.sweep.join("results.json");
    if !results_path.exists() {
        return Err(Error::Audit(format!(
            "results.json not found in sweep directory: {}",
            args.sweep.display()
        )));
    }

    let results_content = fs::read_to_string(&results_path)?;
    let results: Value = serde_json::from_str(&results_content)?;

    let evaluation_path = args.sweep.join("evaluation.json");
    let evaluation: Option<Value> = if evaluation_path.exists() {
        let eval_content = fs::read_to_string(&evaluation_path)?;
        Some(serde_json::from_str(&eval_content)?)
    } else {
        None
    };

    // Recursively collect all *.traj.json and trajectory.json files
    let all_trajectories = collect_trajectories_on_disk(&args.sweep)?;

    // Map files to (instance_id, run_index) with deduplication (preferring nested paths)
    let mut instances_runs: HashMap<String, BTreeMap<u32, PathBuf>> = HashMap::new();
    for path in &all_trajectories {
        if let Some((inst_id, run_index)) = parse_trajectory_path(&args.sweep, path) {
            let existing = instances_runs
                .entry(inst_id.clone())
                .or_default()
                .get(&run_index)
                .cloned();
            if let Some(existing_path) = existing {
                let current_count = path.components().count();
                let existing_count = existing_path.components().count();
                if current_count > existing_count {
                    instances_runs
                        .entry(inst_id)
                        .or_default()
                        .insert(run_index, path.clone());
                } else if current_count == existing_count {
                    // Explicitly prefer run-1.traj.json over trajectory.json if they have same depth
                    let current_file = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let existing_file = existing_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    if current_file.contains("run-1") && existing_file.contains("trajectory.json") {
                        instances_runs
                            .entry(inst_id)
                            .or_default()
                            .insert(run_index, path.clone());
                    }
                }
            } else {
                instances_runs
                    .entry(inst_id)
                    .or_default()
                    .insert(run_index, path.clone());
            }
        }
    }

    let traj_instances: HashSet<String> = instances_runs.keys().cloned().collect();

    // Parse instances from results.json
    let mut results_instances = HashSet::new();
    let mut budget_halted_or_skipped_results = HashSet::new();
    let mut instance_exit_reasons = HashMap::new();
    let mut instance_expected_runs = HashMap::new();
    let mut results_outcomes = HashMap::new();
    if let Some(instances) = results
        .get("instances")
        .and_then(serde_json::Value::as_array)
    {
        for inst in instances {
            if let Some(id) = inst.get("instance_id").and_then(serde_json::Value::as_str) {
                results_instances.insert(id.to_string());

                let exit_reason = inst
                    .get("exit_reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let outcome = inst
                    .get("outcome")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                instance_exit_reasons.insert(id.to_string(), exit_reason.to_string());
                results_outcomes.insert(id.to_string(), outcome.to_string());

                let mut expected_runs = inst
                    .get("runs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1);
                if expected_runs == 0 {
                    expected_runs = 1;
                }
                instance_expected_runs.insert(id.to_string(), expected_runs);

                if exit_reason == "budget_halt"
                    || exit_reason == "budget_halted"
                    || outcome == "budget_halted"
                    || outcome == "skipped"
                    || exit_reason == "skipped"
                    || exit_reason == "skipped_resume"
                {
                    budget_halted_or_skipped_results.insert(id.to_string());
                }
            }
        }
    }

    let mut failed = false;
    let mut divergences = Vec::new();

    // Bijective checks: Trajectories vs Results
    for inst_id in &traj_instances {
        if !results_instances.contains(inst_id) {
            let msg = format!("audit:orphan:trajectory:{inst_id}");
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
    }

    for inst_id in &results_instances {
        if !traj_instances.contains(inst_id) {
            // Exempt if it was budget halted or skipped before starting
            if budget_halted_or_skipped_results.contains(inst_id) {
                continue;
            }
            let msg = format!("audit:missing:trajectory:{inst_id}");
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
    }

    // Bijective checks: Trajectories vs Evaluation (supporting legacy sb-cli format)
    let mut eval_instances = HashSet::new();
    let mut eval_resolved = HashMap::new();
    let mut is_legacy_format = false;
    if let Some(eval) = &evaluation {
        if let Some(instances) = eval.get("instances").and_then(serde_json::Value::as_array) {
            for inst in instances {
                if let Some(id) = inst.get("instance_id").and_then(serde_json::Value::as_str) {
                    eval_instances.insert(id.to_string());
                    if let Some(resolved) =
                        inst.get("resolved").and_then(serde_json::Value::as_bool)
                    {
                        eval_resolved.insert(id.to_string(), resolved);
                    }
                }
            }
        } else {
            // Support legacy sb-cli resolved_ids / submitted_ids structure
            is_legacy_format = true;
            if let Some(resolved_ids) = eval
                .get("resolved_ids")
                .and_then(serde_json::Value::as_array)
            {
                for id_val in resolved_ids {
                    if let Some(id) = id_val.as_str() {
                        eval_instances.insert(id.to_string());
                        eval_resolved.insert(id.to_string(), true);
                    }
                }
            }
            if let Some(submitted_ids) = eval
                .get("submitted_ids")
                .and_then(serde_json::Value::as_array)
            {
                for id_val in submitted_ids {
                    if let Some(id) = id_val.as_str() {
                        eval_instances.insert(id.to_string());
                        eval_resolved.entry(id.to_string()).or_insert(false);
                    }
                }
            }
        }

        for inst_id in &traj_instances {
            if !is_legacy_format && !eval_instances.contains(inst_id) {
                let msg = format!("audit:orphan:evaluation:{inst_id}");
                log_msg(&msg);
                divergences.push(msg);
                failed = true;
            }
        }

        for inst_id in &eval_instances {
            if !traj_instances.contains(inst_id) {
                if budget_halted_or_skipped_results.contains(inst_id) {
                    continue;
                }
                let msg = format!("audit:missing:evaluation:{inst_id}");
                log_msg(&msg);
                divergences.push(msg);
                failed = true;
            }
        }
    }

    // Validate rerun trajectory slot completeness per instance
    #[allow(clippy::cast_possible_truncation)]
    for (inst_id, runs) in &instances_runs {
        if let Some(&expected_runs) = instance_expected_runs.get(inst_id) {
            let is_exempt = budget_halted_or_skipped_results.contains(inst_id);
            if is_exempt {
                // If exempt, we just verify that all keys present are contiguous from 1
                let actual_count = runs.len() as u32;
                for r in 1..=actual_count {
                    if !runs.contains_key(&r) {
                        let msg =
                            format!("audit:mismatch:instance:runs:{inst_id} missing run index {r}");
                        log_msg(&msg);
                        divergences.push(msg);
                        failed = true;
                    }
                }
            } else {
                // If not exempt, we require all indices 1..=expected_runs to be present
                for r in 1..=(expected_runs as u32) {
                    if !runs.contains_key(&r) {
                        let msg =
                            format!("audit:mismatch:instance:runs:{inst_id} missing run index {r}");
                        log_msg(&msg);
                        divergences.push(msg);
                        failed = true;
                    }
                }
                let actual_runs = runs.len() as u64;
                if actual_runs != expected_runs {
                    let msg = format!(
                        "audit:mismatch:instance:runs:{inst_id} expected={expected_runs} actual={actual_runs}"
                    );
                    log_msg(&msg);
                    divergences.push(msg);
                    failed = true;
                }
            }
        }
    }

    // Recompute aggregates across all trajectories
    let mut recomputed_total_cost = 0.0;
    let mut recomputed_prompt_tokens = 0;
    let mut recomputed_cache_read_tokens = 0;
    let mut recomputed_cache_creation_tokens = 0;
    let mut recomputed_completion_tokens = 0;
    let mut has_partial_tokens = false;
    let mut has_partial_durations = false;

    let mut instance_durations = HashMap::new();

    let mut recomputed_submitted = 0;
    let mut recomputed_errored = 0;
    let mut recomputed_skipped = 0;
    let mut recomputed_budget_halted = 0;

    for (inst_id, runs) in &instances_runs {
        let is_skipped_resume =
            instance_exit_reasons.get(inst_id).map(String::as_str) == Some("skipped_resume");
        if is_skipped_resume {
            recomputed_skipped += 1;
        }

        // Count outcomes from all runs of this instance
        for path in runs.values() {
            if let Ok(content) = fs::read_to_string(path) {
                if let Ok(val) = serde_json::from_str::<Value>(&content) {
                    if let Some(info) = val.get("info") {
                        if let Some(outcome) =
                            info.get("outcome").and_then(serde_json::Value::as_str)
                        {
                            match outcome {
                                "submitted" if !is_skipped_resume => {
                                    recomputed_submitted += 1;
                                }
                                "errored" | "error" => {
                                    // Counted in errored regardless of whether it's a skipped resume
                                    recomputed_errored += 1;
                                }
                                "skipped" if !is_skipped_resume => {
                                    recomputed_skipped += 1;
                                }
                                "budget_halted" | "budget_halt" => {
                                    recomputed_budget_halted += 1;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        let mut inst_total_dur = 0.0;
        let mut inst_has_dur = false;

        for path in runs.values() {
            if let Ok(content) = fs::read_to_string(path) {
                if let Ok(val) = serde_json::from_str::<Value>(&content) {
                    if let Some(info) = val.get("info") {
                        // Aggregate cost across all attempts
                        let cost = info
                            .get("actual_cost_usd")
                            .and_then(serde_json::Value::as_f64)
                            .or_else(|| {
                                info.get("total_cost_usd")
                                    .and_then(serde_json::Value::as_f64)
                            })
                            .unwrap_or(0.0);
                        recomputed_total_cost += cost;

                        // Aggregate tokens if available
                        if let Some(token_usage) = info.get("token_usage") {
                            let prompt = token_usage
                                .get("prompt_tokens")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            let read = token_usage
                                .get("cache_read_tokens")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            let create = token_usage
                                .get("cache_creation_tokens")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            let completion = token_usage
                                .get("completion_tokens")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);

                            recomputed_prompt_tokens += prompt;
                            recomputed_cache_read_tokens += read;
                            recomputed_cache_creation_tokens += create;
                            recomputed_completion_tokens += completion;
                        } else if !has_partial_tokens {
                            has_partial_tokens = true;
                            let msg = "audit:partial:trajectory:token_usage".to_string();
                            log_msg(&msg);
                            divergences.push(msg);
                        }

                        // Aggregate durations
                        if let Some(dur) = info
                            .get("duration_secs")
                            .and_then(serde_json::Value::as_f64)
                        {
                            inst_total_dur += dur;
                            inst_has_dur = true;
                        }
                    }
                }
            }
        }

        if inst_has_dur {
            instance_durations.insert(inst_id.clone(), inst_total_dur);
        } else {
            has_partial_durations = true;
        }
    }

    // Count budget-halted or skipped rows without trajectories in recomputation
    for inst_id in &results_instances {
        if !traj_instances.contains(inst_id) {
            let exit_reason = instance_exit_reasons
                .get(inst_id)
                .map_or("", String::as_str);
            let outcome = results_outcomes.get(inst_id).map_or("", String::as_str);

            if exit_reason == "budget_halt"
                || exit_reason == "budget_halted"
                || outcome == "budget_halted"
            {
                recomputed_budget_halted += 1;
            } else if exit_reason == "skipped"
                || exit_reason == "skipped_resume"
                || outcome == "skipped"
            {
                recomputed_skipped += 1;
            }
        }
    }

    if has_partial_durations {
        let msg = "audit:partial:trajectory:duration_secs".to_string();
        log_msg(&msg);
        divergences.push(msg);
    }

    // Reconcile Cost (Reconcile against actual_cost_usd when available)
    let expected_cost = results
        .get("actual_cost_usd")
        .and_then(serde_json::Value::as_f64)
        .or_else(|| {
            results
                .get("total_cost_usd")
                .and_then(serde_json::Value::as_f64)
        })
        .unwrap_or(0.0);

    if (recomputed_total_cost - expected_cost).abs() > args.cost_tolerance_usd {
        let msg = format!(
            "audit:mismatch:sweep:total_cost_usd expected={expected_cost} actual={recomputed_total_cost}"
        );
        log_msg(&msg);
        divergences.push(msg);
        failed = true;
    }

    // Reconcile Outcomes
    let expected_submitted = results
        .get("submitted")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let expected_errored = results
        .get("errored")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let expected_skipped = results
        .get("skipped")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let expected_budget_halted = results
        .get("budget_halted")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    if recomputed_submitted != expected_submitted {
        let msg = format!(
            "audit:mismatch:sweep:submitted expected={expected_submitted} actual={recomputed_submitted}"
        );
        log_msg(&msg);
        divergences.push(msg);
        failed = true;
    }
    if recomputed_errored != expected_errored {
        let msg = format!(
            "audit:mismatch:sweep:errored expected={expected_errored} actual={recomputed_errored}"
        );
        log_msg(&msg);
        divergences.push(msg);
        failed = true;
    }
    if recomputed_skipped != expected_skipped {
        let msg = format!(
            "audit:mismatch:sweep:skipped expected={expected_skipped} actual={recomputed_skipped}"
        );
        log_msg(&msg);
        divergences.push(msg);
        failed = true;
    }
    if recomputed_budget_halted != expected_budget_halted {
        let msg = format!(
            "audit:mismatch:sweep:budget_halted expected={expected_budget_halted} actual={recomputed_budget_halted}"
        );
        log_msg(&msg);
        divergences.push(msg);
        failed = true;
    }

    // Reconcile Tokens (supports total_prompt_tokens legacy alias)
    if !has_partial_tokens {
        let expected_input_tokens = results
            .get("total_input_tokens")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                results
                    .get("total_prompt_tokens")
                    .and_then(serde_json::Value::as_u64)
            })
            .unwrap_or(0);
        let expected_cache_read_tokens = results
            .get("total_cache_read_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let expected_cache_creation_tokens = results
            .get("total_cache_creation_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let expected_completion_tokens = results
            .get("total_completion_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        if recomputed_prompt_tokens != expected_input_tokens {
            let msg = format!(
                "audit:mismatch:sweep:total_input_tokens expected={expected_input_tokens} actual={recomputed_prompt_tokens}"
            );
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
        if recomputed_cache_read_tokens != expected_cache_read_tokens {
            let msg = format!(
                "audit:mismatch:sweep:total_cache_read_tokens expected={expected_cache_read_tokens} actual={recomputed_cache_read_tokens}"
            );
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
        if recomputed_cache_creation_tokens != expected_cache_creation_tokens {
            let msg = format!(
                "audit:mismatch:sweep:total_cache_creation_tokens expected={expected_cache_creation_tokens} actual={recomputed_cache_creation_tokens}"
            );
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
        if recomputed_completion_tokens != expected_completion_tokens {
            let msg = format!(
                "audit:mismatch:sweep:total_completion_tokens expected={expected_completion_tokens} actual={recomputed_completion_tokens}"
            );
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
    }

    // Contradiction checks (rerun-aware checking if ANY run had outcome "submitted")
    if evaluation.is_some() {
        for (inst_id, &resolved) in &eval_resolved {
            if resolved {
                let mut has_submitted_run = false;
                if let Some(runs) = instances_runs.get(inst_id) {
                    for path in runs.values() {
                        if let Ok(content) = fs::read_to_string(path) {
                            if let Ok(val) = serde_json::from_str::<Value>(&content) {
                                if let Some(info) = val.get("info") {
                                    if let Some(outcome) =
                                        info.get("outcome").and_then(serde_json::Value::as_str)
                                    {
                                        if outcome == "submitted" {
                                            has_submitted_run = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !has_submitted_run {
                    let msg = format!("audit:contradiction:instance:{inst_id}");
                    log_msg(&msg);
                    divergences.push(msg);
                    failed = true;
                }
            }
        }
    }

    // Reconcile Durations
    if !has_partial_durations {
        if let Some(instances) = results
            .get("instances")
            .and_then(serde_json::Value::as_array)
        {
            for inst in instances {
                if let Some(id) = inst.get("instance_id").and_then(serde_json::Value::as_str) {
                    let expected_dur = inst
                        .get("duration_secs")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                    let actual_dur = instance_durations.get(id).copied().unwrap_or(0.0);
                    if (actual_dur - expected_dur).abs() > args.wallclock_tolerance_secs {
                        let msg = format!(
                            "audit:mismatch:instance:duration_secs:{id} expected={expected_dur} actual={actual_dur}"
                        );
                        log_msg(&msg);
                        divergences.push(msg);
                        failed = true;
                    }
                }
            }
        }
    }

    // Dataset Hash Check
    if let Some(dataset_path) = &args.dataset_path {
        let mut expected_dataset_sha256 = None;

        let manifest_path = args.sweep.join("manifest.json");
        if let Ok(manifest_content) = fs::read_to_string(&manifest_path) {
            if let Ok(val) = serde_json::from_str::<Value>(&manifest_content) {
                if let Some(sha) = val
                    .get("dataset")
                    .and_then(|d| d.get("sha256"))
                    .and_then(serde_json::Value::as_str)
                {
                    expected_dataset_sha256 = Some(sha.to_string());
                }
            }
        }

        if expected_dataset_sha256.is_none() {
            if let Some(manifest) = results.get("manifest") {
                if let Some(sha) = manifest
                    .get("dataset")
                    .and_then(|d| d.get("sha256"))
                    .and_then(serde_json::Value::as_str)
                {
                    expected_dataset_sha256 = Some(sha.to_string());
                }
            }
        }

        if let Some(expected) = expected_dataset_sha256 {
            let actual = compute_sha256(dataset_path)?;
            if actual != expected {
                let msg =
                    format!("audit:mismatch:dataset:sha256 expected={expected} actual={actual}");
                log_msg(&msg);
                divergences.push(msg);
                failed = true;
            }
        } else {
            let msg = "audit:mismatch:dataset:sha256:expected_missing".to_string();
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
    } else {
        log_msg("audit:dataset:skipped");
    }

    let overall_pass_fail = if failed { "fail" } else { "pass" };
    let audit_report = serde_json::json!({
        "artifact_kind": "audit_report",
        "schema_version": {
            "major": 1,
            "minor": 11
        },
        "overall_pass_fail": overall_pass_fail,
        "divergences": divergences,
    });

    let audit_json_path = args.sweep.join("audit.json");
    let audit_report_str = serde_json::to_string_pretty(&audit_report)?;
    fs::write(&audit_json_path, &audit_report_str)?;

    if args.format == "json" {
        println!("{audit_report_str}");
    }

    if failed {
        Err(Error::Audit("Audit validation failed".to_string()))
    } else {
        Ok(())
    }
}

/// Recursively collect all trajectory files in the sweep directory.
pub(crate) fn collect_trajectories_on_disk(dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut files = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_trajectories_on_disk(&path)?);
            } else {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.ends_with(".traj.json") || name == "trajectory.json" {
                    files.push(path);
                }
            }
        }
    }
    Ok(files)
}

/// Parse trajectory paths into (instance_id, run_index).
/// Layouts supported:
/// 1. Root: <sweep_dir>/<instance_id>.traj.json
/// 2. Nested: <sweep_dir>/<instance_id>/run-<run_index>.traj.json or trajectory.json
/// 3. Bundled: <sweep_dir>/trajectories/<instance_id>.traj.json
pub(crate) fn parse_trajectory_path(sweep_dir: &Path, path: &Path) -> Option<(String, u32)> {
    let rel = path.strip_prefix(sweep_dir).ok()?;
    let components: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;

    let file_name = components.last()?;
    if !file_name.ends_with(".traj.json") && *file_name != "trajectory.json" {
        return None;
    }

    match components.len() {
        1 => {
            let inst_id = file_name.strip_suffix(".traj.json")?;
            Some((inst_id.to_string(), 1))
        }
        2 => {
            let parent = components[0];
            if parent == "trajectories" {
                let inst_id = file_name.strip_suffix(".traj.json")?;
                Some((inst_id.to_string(), 1))
            } else {
                let run_index = if *file_name == "trajectory.json" {
                    1
                } else {
                    file_name
                        .strip_prefix("run-")
                        .and_then(|s| s.strip_suffix(".traj.json"))
                        .and_then(|s| s.parse::<u32>().ok())?
                };
                Some((parent.to_string(), run_index))
            }
        }
        _ => None,
    }
}

/// Calculate SHA-256 of a file.
fn compute_sha256(path: &Path) -> Result<String, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    let result = hasher.finalize();
    Ok(format!("{result:x}"))
}
