//! `bench triage-diff`: diff failure-cluster composition between two sweeps.

#![allow(clippy::too_many_lines, clippy::cast_possible_wrap, clippy::cast_precision_loss)]

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::compare::{load_evaluation_results_checked, load_sweep};
use crate::run::triage::{
    candidate_instance_ids, failure_label, load_trajectory, resolve_trajectory_path,
    terminal_signals, FailureSignature, TriageArgs, TriageCluster, TriageReport,
};

#[derive(Debug, Clone)]
pub struct TriageDiffArgs {
    pub baseline_dir: PathBuf,
    pub candidate_dir: PathBuf,
    pub auto_triage: bool,
    pub min_cluster_size: usize,
    pub top: usize,
    pub output: Option<PathBuf>,
    pub format: String,
    pub fail_on_regression: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterDelta {
    pub cluster_id: String,
    pub failure_category: String,
    pub signature_summary: String,
    pub baseline_count: usize,
    pub candidate_count: usize,
    pub delta: isize,
    pub delta_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageDiffClusterInfo {
    pub cluster_id: String,
    pub failure_category: String,
    pub signature_summary: String,
    pub instance_count: usize,
    pub instance_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageDiffGroupedInstances {
    pub cluster_id: String,
    pub failure_category: String,
    pub signature_summary: String,
    pub instance_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageDiffReport {
    pub schema_version: String,
    pub baseline_sweep: String,
    pub candidate_sweep: String,
    pub cluster_deltas: Vec<ClusterDelta>,
    pub new_clusters: Vec<TriageDiffClusterInfo>,
    pub resolved_clusters: Vec<TriageDiffClusterInfo>,
    pub regression_instances: Vec<TriageDiffGroupedInstances>,
    pub win_instances: Vec<TriageDiffGroupedInstances>,
}

pub fn run(args: &TriageDiffArgs) -> Result<TriageDiffReport, Error> {
    // 1. Check/generate baseline triage.json
    let baseline_triage_path = args.baseline_dir.join("triage.json");
    if !baseline_triage_path.exists() {
        if !args.auto_triage {
            return Err(Error::Trajectory(format!(
                "Run `bench triage --sweep {}` first, or pass `--auto-triage`.",
                args.baseline_dir.display()
            )));
        }
        crate::run::triage::run(&TriageArgs {
            sweep_dir: args.baseline_dir.clone(),
            bucket: None,
            min_cluster_size: 1,
            top: 10,
        })?;
    }

    // 2. Check/generate candidate triage.json
    let candidate_triage_path = args.candidate_dir.join("triage.json");
    if !candidate_triage_path.exists() {
        if !args.auto_triage {
            return Err(Error::Trajectory(format!(
                "Run `bench triage --sweep {}` first, or pass `--auto-triage`.",
                args.candidate_dir.display()
            )));
        }
        crate::run::triage::run(&TriageArgs {
            sweep_dir: args.candidate_dir.clone(),
            bucket: None,
            min_cluster_size: 1,
            top: 10,
        })?;
    }

    // 3. Load triage reports
    let baseline_report: TriageReport = serde_json::from_reader(std::fs::File::open(&baseline_triage_path)?)?;
    let candidate_report: TriageReport = serde_json::from_reader(std::fs::File::open(&candidate_triage_path)?)?;

    // 4. Load full sweeps to determine accurate resolved/unresolved status per instance
    let baseline_sweep = load_sweep(&args.baseline_dir)?;
    let baseline_eval = load_evaluation_results_checked(&args.baseline_dir)?.ok_or_else(|| {
        Error::Trajectory(format!(
            "missing evaluation.json in baseline sweep {}",
            args.baseline_dir.display()
        ))
    })?.results;

    let candidate_sweep = load_sweep(&args.candidate_dir)?;
    let candidate_eval = load_evaluation_results_checked(&args.candidate_dir)?.ok_or_else(|| {
        Error::Trajectory(format!(
            "missing evaluation.json in candidate sweep {}",
            args.candidate_dir.display()
        ))
    })?.results;

    // Use triage candidate logic to determine exactly what failed
    let baseline_unresolved = candidate_instance_ids(baseline_eval, &baseline_sweep.instances);
    let candidate_unresolved = candidate_instance_ids(candidate_eval, &candidate_sweep.instances);

    // Maps to look up clusters by id
    let baseline_clusters_map: HashMap<String, &TriageCluster> = baseline_report
        .clusters
        .iter()
        .map(|c| (c.cluster_id.clone(), c))
        .collect();
    let candidate_clusters_map: HashMap<String, &TriageCluster> = candidate_report
        .clusters
        .iter()
        .map(|c| (c.cluster_id.clone(), c))
        .collect();

    // All unique cluster ids across both reports
    let mut all_cluster_ids = BTreeSet::new();
    all_cluster_ids.extend(baseline_clusters_map.keys().cloned());
    all_cluster_ids.extend(candidate_clusters_map.keys().cloned());

    // 5. Generate Cluster Deltas
    let mut cluster_deltas = Vec::new();
    for id in all_cluster_ids {
        let baseline_count = baseline_clusters_map.get(&id).map_or(0, |c| c.instance_count);
        let candidate_count = candidate_clusters_map.get(&id).map_or(0, |c| c.instance_count);

        // Apply `--min-cluster-size` filter (surfaces if baseline OR candidate count >= k)
        if baseline_count < args.min_cluster_size && candidate_count < args.min_cluster_size {
            continue;
        }

        let delta = candidate_count as isize - baseline_count as isize;
        let delta_pct = if baseline_count == 0 {
            None
        } else {
            Some((delta as f64 / baseline_count as f64) * 100.0)
        };

        let (failure_category, signature_summary) = if let Some(c) = candidate_clusters_map.get(&id) {
            (c.failure_category.clone(), c.signature_summary.clone())
        } else if let Some(c) = baseline_clusters_map.get(&id) {
            (c.failure_category.clone(), c.signature_summary.clone())
        } else {
            ("unknown".to_owned(), "unknown".to_owned())
        };

        cluster_deltas.push(ClusterDelta {
            cluster_id: id,
            failure_category,
            signature_summary,
            baseline_count,
            candidate_count,
            delta,
            delta_pct,
        });
    }

    // Rank deltas by absolute delta descending
    cluster_deltas.sort_by(|a, b| {
        b.delta
            .abs()
            .cmp(&a.delta.abs())
            .then_with(|| b.candidate_count.cmp(&a.candidate_count))
            .then_with(|| b.baseline_count.cmp(&a.baseline_count))
            .then_with(|| a.cluster_id.cmp(&b.cluster_id))
    });

    // 6. Identify NEW clusters (present in candidate, absent in baseline)
    let mut new_clusters = Vec::new();
    for candidate_cluster in &candidate_report.clusters {
        if !baseline_clusters_map.contains_key(&candidate_cluster.cluster_id) {
            new_clusters.push(TriageDiffClusterInfo {
                cluster_id: candidate_cluster.cluster_id.clone(),
                failure_category: candidate_cluster.failure_category.clone(),
                signature_summary: candidate_cluster.signature_summary.clone(),
                instance_count: candidate_cluster.instance_count,
                instance_ids: candidate_cluster.instance_ids.clone(),
            });
        }
    }
    new_clusters.sort_by(|a, b| {
        b.instance_count
            .cmp(&a.instance_count)
            .then_with(|| a.cluster_id.cmp(&b.cluster_id))
    });

    // 7. Identify RESOLVED clusters (present in baseline, absent in candidate)
    let mut resolved_clusters = Vec::new();
    for baseline_cluster in &baseline_report.clusters {
        if !candidate_clusters_map.contains_key(&baseline_cluster.cluster_id) {
            resolved_clusters.push(TriageDiffClusterInfo {
                cluster_id: baseline_cluster.cluster_id.clone(),
                failure_category: baseline_cluster.failure_category.clone(),
                signature_summary: baseline_cluster.signature_summary.clone(),
                instance_count: baseline_cluster.instance_count,
                instance_ids: baseline_cluster.instance_ids.clone(),
            });
        }
    }
    resolved_clusters.sort_by(|a, b| {
        b.instance_count
            .cmp(&a.instance_count)
            .then_with(|| a.cluster_id.cmp(&b.cluster_id))
    });

    // Maps for fast trajectory/signature fallback
    let mut candidate_inst_to_cluster_info = HashMap::new();
    for cluster in &candidate_report.clusters {
        for inst_id in &cluster.instance_ids {
            candidate_inst_to_cluster_info.insert(
                inst_id.clone(),
                (
                    cluster.cluster_id.clone(),
                    cluster.failure_category.clone(),
                    cluster.signature_summary.clone(),
                ),
            );
        }
    }

    let mut baseline_inst_to_cluster_info = HashMap::new();
    for cluster in &baseline_report.clusters {
        for inst_id in &cluster.instance_ids {
            baseline_inst_to_cluster_info.insert(
                inst_id.clone(),
                (
                    cluster.cluster_id.clone(),
                    cluster.failure_category.clone(),
                    cluster.signature_summary.clone(),
                ),
            );
        }
    }

    // Helper to get signature dynamically if not in report map
    let get_candidate_signature = |id: &str| -> Result<(String, String, String), Error> {
        if let Some(info) = candidate_inst_to_cluster_info.get(id) {
            Ok(info.clone())
        } else if let Some(traj_path) = resolve_trajectory_path(&args.candidate_dir, id) {
            let trajectory = load_trajectory(&traj_path)?;
            let instance = candidate_sweep.instances.get(id).ok_or_else(|| {
                Error::Trajectory(format!("instance {id} not found in candidate sweep"))
            })?;
            let failure_cat = instance
                .failure_category
                .or(trajectory.info.failure_category)
                .unwrap_or(crate::trajectory::FailureCategory::Unknown);
            let category_label = failure_label(failure_cat).to_owned();
            let signals = terminal_signals(&trajectory);
            let signature = FailureSignature::from_parts(
                &category_label,
                &signals.assistant_message,
                signals.bash_exit_code,
                &signals.stderr_line,
            );
            Ok((signature.cluster_id(), category_label, signature.summary()))
        } else {
            Ok(("unknown".to_owned(), "unknown".to_owned(), "Trajectory not found".to_owned()))
        }
    };

    let get_baseline_signature = |id: &str| -> Result<(String, String, String), Error> {
        if let Some(info) = baseline_inst_to_cluster_info.get(id) {
            Ok(info.clone())
        } else if let Some(traj_path) = resolve_trajectory_path(&args.baseline_dir, id) {
            let trajectory = load_trajectory(&traj_path)?;
            let instance = baseline_sweep.instances.get(id).ok_or_else(|| {
                Error::Trajectory(format!("instance {id} not found in baseline sweep"))
            })?;
            let failure_cat = instance
                .failure_category
                .or(trajectory.info.failure_category)
                .unwrap_or(crate::trajectory::FailureCategory::Unknown);
            let category_label = failure_label(failure_cat).to_owned();
            let signals = terminal_signals(&trajectory);
            let signature = FailureSignature::from_parts(
                &category_label,
                &signals.assistant_message,
                signals.bash_exit_code,
                &signals.stderr_line,
            );
            Ok((signature.cluster_id(), category_label, signature.summary()))
        } else {
            Ok(("unknown".to_owned(), "unknown".to_owned(), "Trajectory not found".to_owned()))
        }
    };

    // 8. Generate Regressions Set: solved in baseline, unresolved/failed in candidate
    let mut regression_groups: HashMap<String, (String, String, Vec<String>)> = HashMap::new();
    for id in candidate_sweep.instances.keys() {
        if baseline_sweep.instances.contains_key(id)
            && !baseline_unresolved.contains(id)
            && candidate_unresolved.contains(id)
        {
            let (cluster_id, failure_category, signature_summary) = get_candidate_signature(id)?;
            let entry = regression_groups
                .entry(cluster_id)
                .or_insert_with(|| (failure_category, signature_summary, Vec::new()));
            entry.2.push(id.clone());
        }
    }

    let mut regression_instances = Vec::new();
    for (cluster_id, (failure_category, signature_summary, mut instance_ids)) in regression_groups {
        instance_ids.sort();
        regression_instances.push(TriageDiffGroupedInstances {
            cluster_id,
            failure_category,
            signature_summary,
            instance_ids,
        });
    }
    regression_instances.sort_by(|a, b| {
        b.instance_ids
            .len()
            .cmp(&a.instance_ids.len())
            .then_with(|| a.cluster_id.cmp(&b.cluster_id))
    });

    // 9. Generate Wins Set: unresolved/failed in baseline, solved in candidate
    let mut win_groups: HashMap<String, (String, String, Vec<String>)> = HashMap::new();
    for id in baseline_sweep.instances.keys() {
        if candidate_sweep.instances.contains_key(id)
            && baseline_unresolved.contains(id)
            && !candidate_unresolved.contains(id)
        {
            let (cluster_id, failure_category, signature_summary) = get_baseline_signature(id)?;
            let entry = win_groups
                .entry(cluster_id)
                .or_insert_with(|| (failure_category, signature_summary, Vec::new()));
            entry.2.push(id.clone());
        }
    }

    let mut win_instances = Vec::new();
    for (cluster_id, (failure_category, signature_summary, mut instance_ids)) in win_groups {
        instance_ids.sort();
        win_instances.push(TriageDiffGroupedInstances {
            cluster_id,
            failure_category,
            signature_summary,
            instance_ids,
        });
    }
    win_instances.sort_by(|a, b| {
        b.instance_ids
            .len()
            .cmp(&a.instance_ids.len())
            .then_with(|| a.cluster_id.cmp(&b.cluster_id))
    });

    let report = TriageDiffReport {
        schema_version: "triage-diff-1.0".to_owned(),
        baseline_sweep: args.baseline_dir.display().to_string(),
        candidate_sweep: args.candidate_dir.display().to_string(),
        cluster_deltas,
        new_clusters,
        resolved_clusters,
        regression_instances,
        win_instances,
    };

    // 10. Write JSON report
    let default_output = args.candidate_dir.join("triage-diff.json");
    let output_path = args.output.as_ref().unwrap_or(&default_output);
    let file = std::fs::File::create(output_path)?;
    serde_json::to_writer_pretty(file, &report)?;

    // Handle `--fail-on-regression` exit gate
    if args.fail_on_regression && !report.regression_instances.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "Triage diff contains {} regression cluster(s).",
            report.regression_instances.len()
        ))));
    }

    Ok(report)
}

pub fn render_text(report: &TriageDiffReport, top: usize) -> String {
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;
    use comfy_table::Table;

    let mut out = String::new();
    out.push_str("\n=== bench triage-diff ===\n");
    let _ = writeln!(out, "Baseline:  {}", report.baseline_sweep);
    let _ = writeln!(out, "Candidate: {}", report.candidate_sweep);
    out.push('\n');

    // 1. Regressions Set
    out.push_str("=== Regressions ===\n");
    if report.regression_instances.is_empty() {
        out.push_str("None\n");
    } else {
        out.push_str("Grouped by candidate-side failure cluster:\n");
        for group in &report.regression_instances {
            let _ = writeln!(
                out,
                "- {}  {:<14}  \"{}\"",
                group.cluster_id, group.failure_category, group.signature_summary
            );
            let _ = writeln!(out, "  Instances: {:?}", group.instance_ids);
        }
    }
    out.push('\n');

    // 2. Wins Set
    out.push_str("=== Wins ===\n");
    if report.win_instances.is_empty() {
        out.push_str("None\n");
    } else {
        out.push_str("Grouped by baseline-side failure cluster:\n");
        for group in &report.win_instances {
            let _ = writeln!(
                out,
                "- {}  {:<14}  \"{}\"",
                group.cluster_id, group.failure_category, group.signature_summary
            );
            let _ = writeln!(out, "  Instances: {:?}", group.instance_ids);
        }
    }
    out.push('\n');

    // 3. Cluster Deltas Table
    out.push_str("=== Failure Cluster Deltas ===\n");
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "rank",
            "cluster_signature_short",
            "failure_category",
            "baseline",
            "candidate",
            "delta",
            "delta_pct",
            "signature_summary",
        ]);

    for (idx, delta) in report.cluster_deltas.iter().take(top).enumerate() {
        let pct_str = delta.delta_pct.map_or_else(
            || {
                if delta.candidate_count > 0 {
                    "+inf%".to_owned()
                } else {
                    "0.0%".to_owned()
                }
            },
            |pct| {
                if pct == 0.0 {
                    "0.0%".to_owned()
                } else {
                    format!("{pct:+.1}%")
                }
            },
        );

        table.add_row(vec![
            (idx + 1).to_string(),
            delta.cluster_id.clone(),
            delta.failure_category.clone(),
            delta.baseline_count.to_string(),
            delta.candidate_count.to_string(),
            if delta.delta == 0 {
                "0".to_owned()
            } else {
                format!("{:+}", delta.delta)
            },
            pct_str,
            crate::run::triage::truncate_chars(&delta.signature_summary, 64),
        ]);
    }
    out.push_str(&table.to_string());
    out.push_str("\n\n");

    // 4. New Clusters
    out.push_str("=== New Clusters ===\n");
    if report.new_clusters.is_empty() {
        out.push_str("None\n");
    } else {
        for cluster in report.new_clusters.iter().take(top) {
            let _ = writeln!(
                out,
                "- {}  {:<14} (count: {})  \"{}\"",
                cluster.cluster_id,
                cluster.failure_category,
                cluster.instance_count,
                cluster.signature_summary
            );
            let _ = writeln!(out, "  Instances: {:?}", cluster.instance_ids);
        }
    }
    out.push('\n');

    // 5. Resolved Clusters
    out.push_str("=== Resolved Clusters ===\n");
    if report.resolved_clusters.is_empty() {
        out.push_str("None\n");
    } else {
        for cluster in report.resolved_clusters.iter().take(top) {
            let _ = writeln!(
                out,
                "- {}  {:<14} (count: {})  \"{}\"",
                cluster.cluster_id,
                cluster.failure_category,
                cluster.instance_count,
                cluster.signature_summary
            );
            let _ = writeln!(out, "  Instances: {:?}", cluster.instance_ids);
        }
    }
    out.push('\n');

    out
}
