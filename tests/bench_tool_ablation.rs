//! `bench tool-ablation`: per-tool removal ablation integration tests.
//!
//! Tests follow TDD RED→GREEN→REFACTOR per issue #274.
//! Unit-level logic lives in `src/run/tool_ablation.rs`;
//! these tests exercise the CLI binary and the public Rust API.

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use maxwells_daemon::model::ModelUsage;
use maxwells_daemon::run::dataset::DatasetSource;
use maxwells_daemon::run::tool_ablation::{
    PlannedArm, ToolAblationArgs, enumerate_tools, generate_arm_plan, run as ablation_run,
};

mod support;
use support::binary_path;

// ── helpers ───────────────────────────────────────────────────────────────────

fn minimal_dataset(dir: &Path, instances: &[&str]) -> std::path::PathBuf {
    let path = dir.join("dataset.jsonl");
    let content = instances.iter().fold(String::new(), |mut acc, id| {
        let _ = writeln!(
            acc,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"fix it\"}}"
        );
        acc
    });
    std::fs::write(&path, content).unwrap();
    path
}

fn write_config_with_tools(dir: &Path, tools: &[(&str, &str)]) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    let mut content = String::new();
    for (name, cmd) in tools {
        let _ = write!(
            content,
            "[[agent.tools]]\nname = \"{name}\"\ncommand = \"{cmd}\"\n\n"
        );
    }
    std::fs::write(&path, &content).unwrap();
    path
}

fn default_ablation_args(
    config_path: std::path::PathBuf,
    dataset_path: std::path::PathBuf,
    output_dir: std::path::PathBuf,
) -> ToolAblationArgs {
    ToolAblationArgs {
        config_path,
        dataset_source: DatasetSource::LocalPath(dataset_path),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
        output_dir,
        ablate: vec![],
        sweep_cost_limit_usd: None,
        matrix_parallelism: 1,
        resume: false,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        parallel: 1,
        include_pair_ablation: false,
        skip_preflight: true,
        skip_model_probe: true,
        deterministic_responses: Some(vec!["looking at the problem...".into()]),
        deterministic_usage_per_call: None,
        cancel_deadline_secs: 5,
        install_os_signal_handlers: false,
    }
}

// ── CLI smoke tests ───────────────────────────────────────────────────────────

#[test]
fn cli_help_includes_tool_ablation_subcommand() {
    let output = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("tool-ablation"),
        "bench --help should list `tool-ablation` subcommand, got:\n{stdout}"
    );
}

#[test]
fn cli_tool_ablation_help_shows_required_flags() {
    let output = Command::new(binary_path())
        .args(["bench", "tool-ablation", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    for flag in &[
        "--config",
        "--output",
        "--dataset-path",
        "--sweep-cost-limit-usd",
        "--matrix-parallelism",
        "--resume",
        "--render-only",
        "--ablate",
        "--include-pair-ablation",
        "--limit",
        "--sample",
        "--seed",
    ] {
        assert!(stdout.contains(flag), "expected {flag} in help:\n{stdout}");
    }
}

#[test]
fn cli_tool_ablation_requires_config_flag() {
    let output = Command::new(binary_path())
        .args([
            "bench",
            "tool-ablation",
            "--dataset-path",
            "x.jsonl",
            "--output",
            "/tmp/out",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "bench tool-ablation without --config should fail"
    );
}

#[test]
fn cli_tool_ablation_requires_dataset_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let config = write_config_with_tools(tmp.path(), &[]);

    let output = Command::new(binary_path())
        .args([
            "bench",
            "tool-ablation",
            "--config",
            config.to_str().unwrap(),
            "--output",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "bench tool-ablation without --dataset-path should fail"
    );
}

// ── Unit: enumerate_tools ─────────────────────────────────────────────────────

#[test]
fn enumerate_tools_returns_user_defined_tool_names() {
    use maxwells_daemon::config::Config;
    let cfg = Config::from_toml_str(
        "[[agent.tools]]\nname = \"my_tool\"\ncommand = \"echo hi\"\n\
         [[agent.tools]]\nname = \"other_tool\"\ncommand = \"echo bye\"\n",
    )
    .unwrap();
    let tools = enumerate_tools(&cfg);
    assert!(
        tools.contains(&"my_tool".to_string()),
        "should contain my_tool: {tools:?}"
    );
    assert!(
        tools.contains(&"other_tool".to_string()),
        "should contain other_tool: {tools:?}"
    );
}

#[test]
fn enumerate_tools_empty_for_default_config() {
    use maxwells_daemon::config::Config;
    let cfg = Config::defaults().unwrap();
    let tools = enumerate_tools(&cfg);
    assert!(
        tools.is_empty(),
        "default config has no user tools: {tools:?}"
    );
}

// ── Unit: generate_arm_plan ───────────────────────────────────────────────────

#[test]
fn generate_arm_plan_creates_baseline_plus_one_per_tool() {
    let tools = vec!["tool_a".to_string(), "tool_b".to_string()];
    let arms = generate_arm_plan(&tools, &[], false);
    assert_eq!(arms.len(), 3, "baseline + 2 ablation arms: {arms:?}");

    let baseline = arms.iter().find(|a| a.name == "baseline").unwrap();
    assert!(
        baseline.ablated_tool.is_none(),
        "baseline should have no ablated tool"
    );

    let no_a = arms.iter().find(|a| a.name == "no_tool_a").unwrap();
    assert_eq!(no_a.ablated_tool.as_deref(), Some("tool_a"));

    let no_b = arms.iter().find(|a| a.name == "no_tool_b").unwrap();
    assert_eq!(no_b.ablated_tool.as_deref(), Some("tool_b"));
}

#[test]
fn generate_arm_plan_with_ablate_filter_restricts_set() {
    let tools = vec![
        "tool_a".to_string(),
        "tool_b".to_string(),
        "tool_c".to_string(),
    ];
    let ablate = vec!["tool_a".to_string(), "tool_c".to_string()];
    let arms = generate_arm_plan(&tools, &ablate, false);
    // baseline + only tool_a and tool_c
    assert_eq!(arms.len(), 3, "baseline + 2 restricted arms: {arms:?}");
    assert!(arms.iter().any(|a| a.name == "baseline"));
    assert!(arms.iter().any(|a| a.name == "no_tool_a"));
    assert!(arms.iter().any(|a| a.name == "no_tool_c"));
    assert!(
        !arms.iter().any(|a| a.name == "no_tool_b"),
        "tool_b should be excluded by filter"
    );
}

#[test]
fn generate_arm_plan_empty_tools_gives_only_baseline() {
    let arms = generate_arm_plan(&[], &[], false);
    assert_eq!(arms.len(), 1, "only baseline when no tools: {arms:?}");
    assert_eq!(arms[0].name, "baseline");
}

#[test]
fn generate_arm_plan_with_pair_ablation_adds_pair_arms() {
    let tools = vec!["tool_a".to_string(), "tool_b".to_string()];
    let arms = generate_arm_plan(&tools, &[], true);
    // baseline + 2 singles + 1 pair = 4
    assert_eq!(arms.len(), 4, "expected 4 arms: {arms:?}");

    let pair_arm = arms.iter().find(|a| a.ablated_pair.is_some()).unwrap();
    let (t1, t2) = pair_arm.ablated_pair.as_ref().unwrap();
    let pair_set: std::collections::HashSet<&str> =
        [t1.as_str(), t2.as_str()].into_iter().collect();
    assert!(
        pair_set.contains("tool_a") && pair_set.contains("tool_b"),
        "pair should contain both tools: {pair_set:?}"
    );
}

#[test]
fn planned_arm_baseline_has_no_ablated_tool() {
    let arms = generate_arm_plan(&["t".to_string()], &[], false);
    let baseline: &PlannedArm = arms.iter().find(|a| a.name == "baseline").unwrap();
    assert!(baseline.ablated_tool.is_none());
    assert!(baseline.ablated_pair.is_none());
}

// ── Integration: full ablation run ───────────────────────────────────────────

#[tokio::test]
async fn full_ablation_run_creates_per_arm_dirs_and_results() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let args = default_ablation_args(config, dataset, output.clone());
    ablation_run(args).await.unwrap();

    assert!(
        output.join("baseline").join("results.json").exists(),
        "baseline/results.json must exist"
    );
    assert!(
        output.join("no_my_tool").join("results.json").exists(),
        "no_my_tool/results.json must exist"
    );
}

#[tokio::test]
async fn full_ablation_run_writes_tool_ablation_json() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let args = default_ablation_args(config, dataset, output.clone());
    ablation_run(args).await.unwrap();

    let json_path = output.join("tool-ablation.json");
    assert!(json_path.exists(), "tool-ablation.json must exist");

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert_eq!(
        v["schema_version"], "tool-ablation-1.0",
        "schema_version must be tool-ablation-1.0"
    );
    assert!(v["instance_ids"].is_array(), "instance_ids required");
    assert!(v["arms"].is_array(), "arms required");
    assert!(v["generated_at"].is_string(), "generated_at required");
    assert!(v["config_path"].is_string(), "config_path required");
}

#[tokio::test]
async fn full_ablation_json_per_arm_has_required_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let args = default_ablation_args(config, dataset, output.clone());
    ablation_run(args).await.unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("tool-ablation.json")).unwrap())
            .unwrap();
    let arms = v["arms"].as_array().unwrap();
    assert!(!arms.is_empty(), "arms must be non-empty");
    for arm in arms {
        assert!(arm["name"].is_string(), "arm.name required: {arm}");
        assert!(arm["status"].is_string(), "arm.status required: {arm}");
        assert!(arm["resolved"].is_number(), "arm.resolved required: {arm}");
        assert!(arm["errored"].is_number(), "arm.errored required: {arm}");
        assert!(arm["total"].is_number(), "arm.total required: {arm}");
        assert!(arm["cost_usd"].is_number(), "arm.cost_usd required: {arm}");
        assert!(
            arm["step_mean"].is_number(),
            "arm.step_mean required: {arm}"
        );
        assert!(arm["step_p95"].is_number(), "arm.step_p95 required: {arm}");
        assert!(
            arm["delta_resolved_vs_baseline"].is_number(),
            "delta_resolved required: {arm}"
        );
        assert!(
            arm["delta_cost_per_resolve_vs_baseline"].is_number(),
            "delta_cost required: {arm}"
        );
    }
}

#[tokio::test]
async fn full_ablation_baseline_has_null_ablated_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let args = default_ablation_args(config, dataset, output.clone());
    ablation_run(args).await.unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("tool-ablation.json")).unwrap())
            .unwrap();
    let baseline = v["arms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "baseline")
        .unwrap()
        .clone();
    assert!(
        baseline["ablated_tool"].is_null(),
        "baseline ablated_tool must be null: {baseline}"
    );
}

#[tokio::test]
async fn full_ablation_writes_summary_txt() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let args = default_ablation_args(config, dataset, output.clone());
    ablation_run(args).await.unwrap();

    let summary_path = output.join("tool-ablation-summary.txt");
    assert!(
        summary_path.exists(),
        "tool-ablation-summary.txt must exist"
    );
    let content = std::fs::read_to_string(&summary_path).unwrap();
    assert!(
        content.contains("baseline"),
        "summary should mention baseline: {content}"
    );
}

#[tokio::test]
async fn arms_run_against_same_instance_set_as_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2", "inst-3"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let mut args = default_ablation_args(config, dataset, output.clone());
    args.limit = Some(2);
    ablation_run(args).await.unwrap();

    let ablation_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("tool-ablation.json")).unwrap())
            .unwrap();
    let instance_ids = ablation_json["instance_ids"].as_array().unwrap();
    assert_eq!(instance_ids.len(), 2, "limit=2 should select 2 instances");

    let load_arm_ids = |arm_name: &str| -> Vec<String> {
        let path = output.join(arm_name).join("results.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut ids: Vec<String> = v["instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["instance_id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    };

    let baseline_ids = load_arm_ids("baseline");
    let no_tool_ids = load_arm_ids("no_my_tool");
    assert_eq!(
        baseline_ids, no_tool_ids,
        "all arms must run against the same instance set"
    );
}

#[tokio::test]
async fn budget_exhaustion_is_not_failure_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("tool_a", "echo a"), ("tool_b", "echo b")]);
    let output = tmp.path().join("out");

    let mut args = default_ablation_args(config, dataset, output.clone());
    args.deterministic_usage_per_call = Some(ModelUsage {
        input_tokens: 1_000_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: None,
    });
    // Limit $0 so arms after baseline are skipped.
    args.sweep_cost_limit_usd = Some(0.0);

    let result = ablation_run(args).await;
    assert!(
        result.is_ok(),
        "budget exhaustion should not return error: {:?}",
        result.err()
    );

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("tool-ablation.json")).unwrap())
            .unwrap();
    let arms = v["arms"].as_array().unwrap();
    let has_skipped = arms.iter().any(|a| a["status"] == "skipped_budget");
    assert!(has_skipped, "some arms should be skipped_budget: {arms:?}");
}

#[tokio::test]
async fn ablate_flag_restricts_tool_set() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("tool_a", "echo a"), ("tool_b", "echo b")]);
    let output = tmp.path().join("out");

    let mut args = default_ablation_args(config, dataset, output.clone());
    args.ablate = vec!["tool_a".to_string()];
    ablation_run(args).await.unwrap();

    assert!(output.join("baseline").exists(), "baseline must exist");
    assert!(output.join("no_tool_a").exists(), "no_tool_a must exist");
    assert!(
        !output.join("no_tool_b").exists(),
        "no_tool_b should NOT exist (restricted by --ablate)"
    );
}

#[tokio::test]
async fn trajectory_artifacts_live_under_arm_name_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let args = default_ablation_args(config, dataset, output.clone());
    ablation_run(args).await.unwrap();

    // Each arm has a subdirectory with results.json (standard sweep layout).
    assert!(output.join("baseline").join("results.json").exists());
    assert!(output.join("no_my_tool").join("results.json").exists());
}

#[tokio::test]
async fn tool_ablation_json_instance_ids_match_arm_results() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2"]);
    let config = write_config_with_tools(tmp.path(), &[("t", "echo t")]);
    let output = tmp.path().join("out");

    let args = default_ablation_args(config, dataset, output.clone());
    ablation_run(args).await.unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("tool-ablation.json")).unwrap())
            .unwrap();
    let ids: Vec<String> = v["instance_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 2, "should record both instances");
    assert!(ids.contains(&"inst-1".to_string()));
    assert!(ids.contains(&"inst-2".to_string()));
}

// ── CLI render-only ───────────────────────────────────────────────────────────

#[test]
fn cli_render_only_exits_zero_without_running_sweeps() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let result = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "tool-ablation",
            "--config",
            config.to_str().unwrap(),
            "--dataset-path",
            dataset.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--render-only",
        ])
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "render-only should exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    // No sweep artifacts should be written.
    assert!(
        !output.join("baseline").exists(),
        "render-only should not run any sweeps"
    );
    // Should print something to stdout.
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(!stdout.is_empty(), "render-only should print manifest");
    assert!(
        stdout.contains("baseline"),
        "manifest should mention baseline arm: {stdout}"
    );
}

#[test]
fn cli_render_only_json_outputs_schema_versioned_object() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_config_with_tools(tmp.path(), &[("my_tool", "echo hi")]);
    let output = tmp.path().join("out");

    let result = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "tool-ablation",
            "--config",
            config.to_str().unwrap(),
            "--dataset-path",
            dataset.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--render-only",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "render-only --format json should succeed"
    );
    let stdout = String::from_utf8(result.stdout).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("render-only json output must be valid JSON");
    assert_eq!(
        v["schema_version"], "tool-ablation-1.0",
        "schema version required: {v}"
    );
    assert!(v["arms"].is_array(), "arms array required: {v}");
    assert!(v["config_path"].is_string(), "config_path required: {v}");
}
