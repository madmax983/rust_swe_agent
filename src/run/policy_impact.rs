#![allow(clippy::too_many_lines, clippy::cast_precision_loss)]

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::retry::load_sweep_results;
use crate::trajectory::Trajectory;

#[derive(Debug, Clone)]
pub struct PolicyImpactArgs {
    pub sweep_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepPolicyTotals {
    pub allowed: u64,
    pub asked: u64,
    pub blocked: u64,
    pub yolo_bypassed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleImpact {
    pub rule_label: String,
    pub block_count: u64,
    pub affected_instances: u64,
    pub affected_instance_ids: Vec<String>,
    pub top_blocked_command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCorrelation {
    pub total_count: u64,
    pub resolved_count: u64,
    pub unresolved_count: u64,
    pub errored_count: u64,
    pub resolved_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeCorrelation {
    pub blocked_group: GroupCorrelation,
    pub unblocked_group: GroupCorrelation,
    pub delta_resolved_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyImpactReportInner {
    pub schema_version: String,
    pub timestamp: String,
    pub totals: SweepPolicyTotals,
    pub rules: Vec<RuleImpact>,
    pub outcome_correlation: OutcomeCorrelation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyImpactReport {
    pub policy_impact_report: PolicyImpactReportInner,
}

struct RuleAggregate {
    block_count: u64,
    affected_instances: HashSet<String>,
    blocked_commands: HashMap<String, usize>,
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

fn load_trajectory(path: &Path) -> Result<Trajectory, Error> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string())
        .map_err(|err| Error::Trajectory(err.to_string()))?;
    serde_json::from_value(value).map_err(Into::into)
}

pub fn run(args: &PolicyImpactArgs) -> Result<PolicyImpactReport, Error> {
    if !args.sweep_dir.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "policy-impact: sweep directory does not exist: {}",
            args.sweep_dir.display()
        ))));
    }

    let results_path = args.sweep_dir.join("results.json");
    if !results_path.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "policy-impact: results.json missing in sweep directory {}",
            args.sweep_dir.display()
        ))));
    }

    let sweep_results = load_sweep_results(&args.sweep_dir)?;
    let redactor = Redactor::default_enabled();

    let mut allowed_total = 0;
    let mut asked_total = 0;
    let mut blocked_total = 0;
    let mut yolo_bypassed_total = 0;

    let mut rules_map: HashMap<String, RuleAggregate> = HashMap::new();
    let mut instance_has_block: HashMap<String, bool> = HashMap::new();

    for instance in &sweep_results.instances {
        let id = &instance.instance_id;
        let traj_paths = resolve_trajectory_paths(&args.sweep_dir, id);
        if traj_paths.is_empty() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "policy-impact: no trajectory file found for instance {id}",
            ))));
        }

        let mut instance_blocked = false;

        for path in traj_paths {
            let traj = load_trajectory(&path).map_err(|e| {
                Error::Trajectory(format!(
                    "policy-impact: failed to load trajectory {}: {}",
                    path.display(),
                    e
                ))
            })?;

            allowed_total += traj.info.policy_counts.allowed;
            asked_total += traj.info.policy_counts.asked;
            blocked_total += traj.info.policy_counts.blocked;
            yolo_bypassed_total += traj.info.policy_counts.yolo_bypassed;

            if traj.info.policy_counts.blocked > 0 {
                instance_blocked = true;
            }

            for msg in &traj.messages {
                let is_blocked = msg
                    .extra
                    .other
                    .get("policy_blocked")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                if is_blocked {
                    instance_blocked = true;

                    let rule_label = msg
                        .extra
                        .other
                        .get("policy_rule")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();

                    let blocked_cmd = msg
                        .extra
                        .other
                        .get("blocked_command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let rule_agg = rules_map
                        .entry(rule_label)
                        .or_insert_with(|| RuleAggregate {
                            block_count: 0,
                            affected_instances: HashSet::new(),
                            blocked_commands: HashMap::new(),
                        });

                    rule_agg.block_count += 1;
                    rule_agg.affected_instances.insert(id.clone());
                    if !blocked_cmd.is_empty() {
                        *rule_agg.blocked_commands.entry(blocked_cmd).or_default() += 1;
                    }
                }
            }
        }

        instance_has_block.insert(id.clone(), instance_blocked);
    }

    let mut rules = Vec::new();
    for (rule_label, rule_agg) in rules_map {
        let mut affected_instance_ids: Vec<String> =
            rule_agg.affected_instances.into_iter().collect();
        affected_instance_ids.sort();

        let mut top_raw_cmd = String::new();
        let mut max_count = 0;
        for (cmd, &count) in &rule_agg.blocked_commands {
            if count > max_count {
                max_count = count;
                top_raw_cmd.clone_from(cmd);
            } else if count == max_count && cmd < &top_raw_cmd {
                top_raw_cmd.clone_from(cmd);
            }
        }

        let redacted_cmd = if top_raw_cmd.is_empty() {
            String::new()
        } else {
            redactor.redact_text(&top_raw_cmd, surface::INSPECT).text
        };

        rules.push(RuleImpact {
            rule_label,
            block_count: rule_agg.block_count,
            affected_instances: affected_instance_ids.len() as u64,
            affected_instance_ids,
            top_blocked_command: redacted_cmd,
        });
    }

    rules.sort_by(|a, b| {
        b.block_count
            .cmp(&a.block_count)
            .then_with(|| a.rule_label.cmp(&b.rule_label))
    });

    let mut blocked_group = GroupCorrelation {
        total_count: 0,
        resolved_count: 0,
        unresolved_count: 0,
        errored_count: 0,
        resolved_rate: 0.0,
    };
    let mut unblocked_group = GroupCorrelation {
        total_count: 0,
        resolved_count: 0,
        unresolved_count: 0,
        errored_count: 0,
        resolved_rate: 0.0,
    };

    for instance in &sweep_results.instances {
        let id = &instance.instance_id;
        let is_blocked = instance_has_block.get(id).copied().unwrap_or(false);

        let group = if is_blocked {
            &mut blocked_group
        } else {
            &mut unblocked_group
        };

        group.total_count += 1;
        if instance.resolved_count > 0 {
            group.resolved_count += 1;
        } else if instance.outcome.as_deref() == Some("error") {
            group.errored_count += 1;
        } else {
            group.unresolved_count += 1;
        }
    }

    if blocked_group.total_count > 0 {
        blocked_group.resolved_rate =
            blocked_group.resolved_count as f64 / blocked_group.total_count as f64;
    }
    if unblocked_group.total_count > 0 {
        unblocked_group.resolved_rate =
            unblocked_group.resolved_count as f64 / unblocked_group.total_count as f64;
    }

    let delta_resolved_rate = blocked_group.resolved_rate - unblocked_group.resolved_rate;

    let totals = SweepPolicyTotals {
        allowed: allowed_total,
        asked: asked_total,
        blocked: blocked_total,
        yolo_bypassed: yolo_bypassed_total,
    };

    let outcome_correlation = OutcomeCorrelation {
        blocked_group,
        unblocked_group,
        delta_resolved_rate,
    };

    let inner = PolicyImpactReportInner {
        schema_version: "1.0".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        totals,
        rules,
        outcome_correlation,
    };

    Ok(PolicyImpactReport {
        policy_impact_report: inner,
    })
}

pub fn render_text(report: &PolicyImpactReport) -> String {
    let mut out = String::new();
    let r = &report.policy_impact_report;

    out.push_str("=== Sweep Policy Totals ===\n\n");
    let mut totals_table = crate::ui::create_table();
    totals_table.set_header(vec!["Metric", "Count"]);
    totals_table.add_row(vec!["Allowed", &r.totals.allowed.to_string()]);
    totals_table.add_row(vec!["Asked", &r.totals.asked.to_string()]);
    totals_table.add_row(vec!["Blocked", &r.totals.blocked.to_string()]);
    totals_table.add_row(vec!["YOLO Bypassed", &r.totals.yolo_bypassed.to_string()]);
    out.push_str(&totals_table.to_string());
    out.push_str("\n\n");

    out.push_str("=== Policy Rule Impact ===\n\n");
    if r.rules.is_empty() {
        out.push_str("No policy rules were triggered.\n");
    } else {
        let mut rules_table = crate::ui::create_table();
        rules_table.set_header(vec![
            "Rule Label",
            "Block Count",
            "Affected Instances",
            "Top Blocked Command",
        ]);
        for rule in &r.rules {
            rules_table.add_row(vec![
                rule.rule_label.clone(),
                rule.block_count.to_string(),
                rule.affected_instances.to_string(),
                rule.top_blocked_command.clone(),
            ]);
        }
        out.push_str(&rules_table.to_string());
    }
    out.push_str("\n\n");

    out.push_str("=== Outcome Correlation ===\n\n");
    let mut corr_table = crate::ui::create_table();
    corr_table.set_header(vec![
        "Group",
        "Total",
        "Resolved",
        "Unresolved",
        "Errored",
        "Resolved Rate",
    ]);

    let bg = &r.outcome_correlation.blocked_group;
    corr_table.add_row(vec![
        "Blocked Group".to_string(),
        bg.total_count.to_string(),
        bg.resolved_count.to_string(),
        bg.unresolved_count.to_string(),
        bg.errored_count.to_string(),
        format!("{:.1}%", bg.resolved_rate * 100.0),
    ]);

    let ug = &r.outcome_correlation.unblocked_group;
    corr_table.add_row(vec![
        "Unblocked Group".to_string(),
        ug.total_count.to_string(),
        ug.resolved_count.to_string(),
        ug.unresolved_count.to_string(),
        ug.errored_count.to_string(),
        format!("{:.1}%", ug.resolved_rate * 100.0),
    ]);
    out.push_str(&corr_table.to_string());
    out.push_str("\n\n");

    let _ = writeln!(
        out,
        "Delta Resolved Rate: {:.1}%",
        r.outcome_correlation.delta_resolved_rate * 100.0
    );

    out
}
