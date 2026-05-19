use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use crate::error::Error;

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

pub fn run(args: &PolicyImpactArgs) -> Result<PolicyImpactReport, Error> {
    // Basic stub that returns an empty/zero report
    let totals = SweepPolicyTotals {
        allowed: 0,
        asked: 0,
        blocked: 0,
        yolo_bypassed: 0,
    };
    let group = GroupCorrelation {
        total_count: 0,
        resolved_count: 0,
        unresolved_count: 0,
        errored_count: 0,
        resolved_rate: 0.0,
    };
    let outcome_correlation = OutcomeCorrelation {
        blocked_group: group.clone(),
        unblocked_group: group,
        delta_resolved_rate: 0.0,
    };
    let inner = PolicyImpactReportInner {
        schema_version: "1.0".to_string(),
        timestamp: "2026-05-19T18:07:03-05:00".to_string(),
        totals,
        rules: Vec::new(),
        outcome_correlation,
    };
    Ok(PolicyImpactReport {
        policy_impact_report: inner,
    })
}

pub fn render_text(_report: &PolicyImpactReport) -> String {
    "Sweep Policy Totals\nAllowed: 0\n".to_string()
}
