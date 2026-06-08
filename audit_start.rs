use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::cli::args::AuditCmd;
use crate::error::Error;

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
