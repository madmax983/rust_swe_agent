#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::fmt::Write as _;
use std::path::Path;

use rust_swe_agent::Config;
use rust_swe_agent::run::swebench::{SwebenchArgs, run};
use rust_swe_agent::trajectory::{
    FORMAT_VERSION, FailureCategory, Trajectory, TrajectoryInfo, outcome,
};

fn write_dataset(path: &Path, instance_ids: &[&str]) {
    let mut s = String::new();
    for id in instance_ids {
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\"}}"
        );
    }
    std::fs::write(path, s).unwrap();
}

fn write_traj(output: &Path, id: &str, info: TrajectoryInfo) {
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    std::fs::write(
        output.join(format!("{id}.traj.json")),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn sweep_counts_failure_categories_and_preserves_legacy_unclassified() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    let ids = [
        "ok", "env", "api", "parse", "step", "cost", "internal", "unknown", "legacy",
    ];
    write_dataset(&dataset, &ids);

    write_traj(
        &output,
        "ok",
        TrajectoryInfo {
            outcome: Some(outcome::SUBMITTED.into()),
            exit_reason: Some("submitted".into()),
            ..Default::default()
        },
    );
    std::fs::write(output.join("ok.patch"), b"").unwrap();

    for (id, cat) in [
        ("env", FailureCategory::EnvSetup),
        ("api", FailureCategory::ModelApi),
        ("parse", FailureCategory::ModelParse),
        ("step", FailureCategory::StepLimit),
        ("cost", FailureCategory::CostLimit),
        ("internal", FailureCategory::AgentInternal),
        ("unknown", FailureCategory::Unknown),
    ] {
        write_traj(
            &output,
            id,
            TrajectoryInfo {
                outcome: Some(outcome::ERROR.into()),
                exit_reason: Some("error".into()),
                failure_category: Some(cat),
                ..Default::default()
            },
        );
    }

    // Simulate a legacy trajectory from before `failure_category` existed.
    write_traj(
        &output,
        "legacy",
        TrajectoryInfo {
            outcome: Some(outcome::ERROR.into()),
            exit_reason: Some("error".into()),
            failure_category: None,
            ..Default::default()
        },
    );

    let cfg = Config::defaults().unwrap();
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output,
        parallel: 2,
        config: cfg,
        resume: true,
        cost_limit_usd: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        config_overlay_paths: Vec::new(),
    })
    .await
    .unwrap();

    assert_eq!(results.total, ids.len());
    assert_eq!(results.skipped, ids.len());

    for cat in [
        FailureCategory::EnvSetup,
        FailureCategory::ModelApi,
        FailureCategory::ModelParse,
        FailureCategory::StepLimit,
        FailureCategory::CostLimit,
        FailureCategory::AgentInternal,
        FailureCategory::Unknown,
    ] {
        assert_eq!(results.failures_by_category.get(&cat), Some(&1));
    }

    for r in &results.instances {
        match r.instance_id.as_str() {
            "ok" | "legacy" => assert!(r.failure_category.is_none()),
            "env" => assert_eq!(r.failure_category, Some(FailureCategory::EnvSetup)),
            "api" => assert_eq!(r.failure_category, Some(FailureCategory::ModelApi)),
            "parse" => assert_eq!(r.failure_category, Some(FailureCategory::ModelParse)),
            "step" => assert_eq!(r.failure_category, Some(FailureCategory::StepLimit)),
            "cost" => assert_eq!(r.failure_category, Some(FailureCategory::CostLimit)),
            "internal" => assert_eq!(r.failure_category, Some(FailureCategory::AgentInternal)),
            "unknown" => assert_eq!(r.failure_category, Some(FailureCategory::Unknown)),
            other => panic!("unexpected id: {other}"),
        }
    }

    let table = results.summary_table();
    assert!(table.contains("Failures by category:"), "got: {table}");
    assert!(table.contains("  - env_setup: 1"), "got: {table}");
    assert!(table.contains("  - model_api: 1"), "got: {table}");
    assert!(table.contains("  - model_parse: 1"), "got: {table}");
    assert!(table.contains("  - step_limit: 1"), "got: {table}");
    assert!(table.contains("  - cost_limit: 1"), "got: {table}");
    assert!(table.contains("  - agent_internal: 1"), "got: {table}");
    assert!(table.contains("  - unknown: 1"), "got: {table}");
    assert!(
        table.contains("  - unclassified (legacy): 1"),
        "got: {table}"
    );
}
