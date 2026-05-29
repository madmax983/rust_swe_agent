//! `bench matrix`: integration tests for the multi-arm experiment runner.
//!
//! Tests follow TDD RED→GREEN→REFACTOR per issue #162.
//! Unit-level parsing/validation live in `src/run/matrix.rs`;
//! these tests exercise the CLI binary and the public Rust API.

#![allow(clippy::unwrap_used, clippy::large_futures)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use maxwells_daemon::model::ModelUsage;
use maxwells_daemon::run::dataset::DatasetSource;
use maxwells_daemon::run::matrix::{ArmDef, MatrixArgs, MatrixManifest, run as matrix_run};
use maxwells_daemon::run::swebench::StratifyMode;

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

/// Write a matrix.toml with each arm having `step_limit = 1` for fast tests.
fn write_matrix_toml(dir: &Path, arms: &[(&str, &str)]) -> std::path::PathBuf {
    let path = dir.join("matrix.toml");
    let content = arms.iter().fold(String::new(), |mut acc, (name, model)| {
        let _ = write!(
            acc,
            "[[arm]]\nname = \"{name}\"\nmodel = \"{model}\"\nstep_limit = 1\n\n"
        );
        acc
    });
    std::fs::write(&path, &content).unwrap();
    path
}

fn default_matrix_args(
    config_path: std::path::PathBuf,
    dataset_path: std::path::PathBuf,
    output_dir: std::path::PathBuf,
) -> MatrixArgs {
    MatrixArgs {
        config_path,
        dataset_source: DatasetSource::LocalPath(dataset_path),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
        output_dir,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: StratifyMode::Proportional,
        sweep_cost_limit_usd: None,
        matrix_parallelism: 1,
        resume: false,
        parallel: 1,
        skip_preflight: true,
        skip_model_probe: true,
        // Non-submit response: agent hits step_limit=1 cleanly.
        deterministic_responses: Some(vec!["looking at the problem...".into()]),
        deterministic_usage_per_call: None,
        cancel_deadline_secs: 5,
        install_os_signal_handlers: false,
    }
}

// ── CLI smoke tests ───────────────────────────────────────────────────────────

#[test]
fn cli_help_includes_matrix_subcommand() {
    let output = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("matrix"),
        "bench --help should list the `matrix` subcommand, got:\n{stdout}"
    );
}

#[test]
fn cli_matrix_help_shows_required_flags() {
    let output = Command::new(binary_path())
        .args(["bench", "matrix", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("--config"),
        "expected --config in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--output"),
        "expected --output in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--dataset-path"),
        "expected --dataset-path in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--sweep-cost-limit-usd"),
        "expected --sweep-cost-limit-usd in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--matrix-parallelism"),
        "expected --matrix-parallelism in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--resume"),
        "expected --resume in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--limit"),
        "expected --limit in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--sample"),
        "expected --sample in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--seed"),
        "expected --seed in help:\n{stdout}"
    );
}

#[test]
fn cli_matrix_requires_config_flag() {
    let output = Command::new(binary_path())
        .args([
            "bench",
            "matrix",
            "--dataset-path",
            "x.jsonl",
            "--output",
            "/tmp/out",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "bench matrix without --config should fail"
    );
}

#[test]
fn cli_matrix_requires_dataset_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let config = write_matrix_toml(tmp.path(), &[("arm1", "claude-opus-4-7")]);

    let output = Command::new(binary_path())
        .args([
            "bench",
            "matrix",
            "--config",
            config.to_str().unwrap(),
            "--output",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "bench matrix without --dataset-path should fail"
    );
}

// ── Unit: manifest parsing ────────────────────────────────────────────────────

#[test]
fn manifest_parses_minimal_arm_definition() {
    let toml_str = "[[arm]]\nname = \"baseline\"\nmodel = \"claude-opus-4-7\"\n";
    let manifest: MatrixManifest = toml::from_str(toml_str).unwrap();
    assert_eq!(manifest.arms.len(), 1);
    assert_eq!(manifest.arms[0].name, "baseline");
    assert_eq!(manifest.arms[0].model, "claude-opus-4-7");
    assert!(manifest.arms[0].step_limit.is_none());
    assert!(manifest.arms[0].per_task_budget_usd.is_none());
    assert!(manifest.arms[0].prompt_file.is_none());
    assert!(manifest.arms[0].extra_args.is_empty());
}

#[test]
fn manifest_parses_arm_with_all_optional_fields() {
    let toml_str = r#"
[[arm]]
name = "tuned"
model = "claude-sonnet-4-6"
step_limit = 30
per_task_budget_usd = 0.50
prompt_file = "/path/to/prompt.toml"
extra_args = ["--skip-patch-validation"]
"#;
    let manifest: MatrixManifest = toml::from_str(toml_str).unwrap();
    let arm = &manifest.arms[0];
    assert_eq!(arm.name, "tuned");
    assert_eq!(arm.model, "claude-sonnet-4-6");
    assert_eq!(arm.step_limit, Some(30));
    assert_eq!(arm.per_task_budget_usd, Some(0.50));
    assert_eq!(
        arm.prompt_file.as_deref().unwrap().to_str().unwrap(),
        "/path/to/prompt.toml"
    );
    assert_eq!(arm.extra_args, vec!["--skip-patch-validation"]);
}

#[test]
fn manifest_parses_multiple_arms() {
    let content = [("a", "model-a"), ("b", "model-b"), ("c", "model-c")]
        .iter()
        .fold(String::new(), |mut acc, (n, m)| {
            let _ = write!(acc, "[[arm]]\nname = \"{n}\"\nmodel = \"{m}\"\n\n");
            acc
        });
    let manifest: MatrixManifest = toml::from_str(&content).unwrap();
    assert_eq!(manifest.arms.len(), 3);
    assert_eq!(manifest.arms[0].name, "a");
    assert_eq!(manifest.arms[1].name, "b");
    assert_eq!(manifest.arms[2].name, "c");
}

// ── Unit: arm validation ──────────────────────────────────────────────────────

#[test]
fn validate_arms_accepts_unique_names() {
    use maxwells_daemon::run::matrix::validate_arms;
    let arms = vec![
        ArmDef {
            name: "alpha".into(),
            model: "m1".into(),
            ..ArmDef::default()
        },
        ArmDef {
            name: "beta".into(),
            model: "m2".into(),
            ..ArmDef::default()
        },
    ];
    assert!(validate_arms(&arms).is_ok());
}

#[test]
fn validate_arms_rejects_duplicate_names() {
    use maxwells_daemon::run::matrix::validate_arms;
    let arms = vec![
        ArmDef {
            name: "same".into(),
            model: "m1".into(),
            ..ArmDef::default()
        },
        ArmDef {
            name: "same".into(),
            model: "m2".into(),
            ..ArmDef::default()
        },
    ];
    let err = validate_arms(&arms).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("same"),
        "error should name the duplicate: {err}"
    );
    assert!(
        msg.contains("duplicate"),
        "error should say duplicate: {err}"
    );
}

#[test]
fn validate_arms_rejects_empty_name() {
    use maxwells_daemon::run::matrix::validate_arms;
    let arms = vec![ArmDef {
        name: String::new(),
        model: "m1".into(),
        ..ArmDef::default()
    }];
    let err = validate_arms(&arms).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("empty") || msg.contains("name"), "{err}");
}

#[test]
fn validate_arms_rejects_name_with_path_separator() {
    use maxwells_daemon::run::matrix::validate_arms;
    for bad_name in ["/evil", "arm/../attack", "arm\\win"] {
        let arms = vec![ArmDef {
            name: bad_name.into(),
            model: "m".into(),
            ..ArmDef::default()
        }];
        let err = validate_arms(&arms).unwrap_err();
        assert!(
            err.to_string().contains("name"),
            "expected arm name validation error for `{bad_name}`, got: {err}"
        );
    }
}

#[test]
fn validate_arms_rejects_empty_arm_list() {
    use maxwells_daemon::run::matrix::validate_arms;
    let err = validate_arms(&[]).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("arm") || msg.contains("empty"), "{err}");
}

// ── Integration: matrix run ───────────────────────────────────────────────────

#[tokio::test]
async fn matrix_run_creates_per_arm_dirs_and_results() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2"]);
    let config = write_matrix_toml(
        tmp.path(),
        &[("arm-a", "deterministic"), ("arm-b", "deterministic")],
    );
    let output = tmp.path().join("out");

    let args = default_matrix_args(config, dataset, output.clone());
    let summary = matrix_run(args).await.unwrap();

    // Per-arm directories exist with results.json.
    assert!(
        output.join("arm-a").join("results.json").exists(),
        "arm-a/results.json must exist"
    );
    assert!(
        output.join("arm-b").join("results.json").exists(),
        "arm-b/results.json must exist"
    );

    // matrix.json exists and records the instance list.
    let matrix_json_path = output.join("matrix.json");
    assert!(matrix_json_path.exists(), "matrix.json must exist");
    let matrix_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&matrix_json_path).unwrap()).unwrap();
    let instance_ids = matrix_json["instance_ids"].as_array().unwrap();
    assert_eq!(
        instance_ids.len(),
        2,
        "matrix.json should record both instances"
    );
    assert!(
        instance_ids.iter().any(|v| v == "inst-1"),
        "instance_ids should contain inst-1"
    );
    assert!(
        instance_ids.iter().any(|v| v == "inst-2"),
        "instance_ids should contain inst-2"
    );

    assert!(
        output.join("matrix-summary.json").exists(),
        "matrix-summary.json must exist"
    );
    assert!(
        output.join("matrix-summary.txt").exists(),
        "matrix-summary.txt must exist"
    );

    assert_eq!(summary.arms.len(), 2);
    let names: Vec<&str> = summary.arms.iter().map(|a| a.name.as_str()).collect();
    assert!(
        names.contains(&"arm-a"),
        "summary should include arm-a: {names:?}"
    );
    assert!(
        names.contains(&"arm-b"),
        "summary should include arm-b: {names:?}"
    );
}

#[tokio::test]
async fn matrix_parallelism_zero_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(dir.path(), &["inst-1"]);
    let config = write_matrix_toml(dir.path(), &[("arm-a", "scripted")]);
    let output = dir.path().join("out");

    let mut args = default_matrix_args(config, dataset, output);
    args.matrix_parallelism = 0;

    let result = matrix_run(args).await;
    assert!(
        result.is_err(),
        "matrix_parallelism=0 should return an error, got Ok"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("matrix-parallelism"),
        "error should mention --matrix-parallelism, got: {msg}"
    );
}

#[tokio::test]
async fn matrix_all_arms_see_the_same_instances() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2", "inst-3"]);
    let config = write_matrix_toml(
        tmp.path(),
        &[("arm-a", "deterministic"), ("arm-b", "deterministic")],
    );
    let output = tmp.path().join("out");

    // limit=2 so we can verify the limit is applied once and shared.
    let mut args = default_matrix_args(config, dataset, output.clone());
    args.limit = Some(2);

    matrix_run(args).await.unwrap();

    let load_ids = |arm: &str| -> Vec<String> {
        let path = output.join(arm).join("results.json");
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

    let a_ids = load_ids("arm-a");
    let b_ids = load_ids("arm-b");

    assert_eq!(
        a_ids, b_ids,
        "both arms must run against the same instance set"
    );
    assert_eq!(a_ids.len(), 2, "limit=2 should select exactly 2 instances");
}

#[tokio::test]
async fn matrix_budget_limit_skips_arms_that_exceed_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);

    // Arms with step_limit=1 → exactly one model call each.
    let config_str = "[[arm]]\nname = \"arm-first\"\nmodel = \"deterministic\"\nstep_limit = 1\n\n\
         [[arm]]\nname = \"arm-skipped\"\nmodel = \"deterministic\"\nstep_limit = 1\n";
    let config = tmp.path().join("matrix.toml");
    std::fs::write(&config, config_str).unwrap();
    let output = tmp.path().join("out");

    let mut args = default_matrix_args(config, dataset, output.clone());
    // 1M input tokens × $3/MTok = $3.00 per arm with step_limit=1 + 1 instance.
    args.deterministic_usage_per_call = Some(ModelUsage {
        input_tokens: 1_000_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: None,
    });
    // Budget $2.00 < arm-first cost $3.00, so arm-skipped is skipped_budget.
    args.sweep_cost_limit_usd = Some(2.0);

    let summary = matrix_run(args).await.unwrap();

    let skipped = summary
        .arms
        .iter()
        .find(|a| a.name == "arm-skipped")
        .unwrap();
    assert_eq!(
        skipped.state, "skipped_budget",
        "arm-skipped should be skipped_budget when budget exhausted: {summary:?}"
    );

    // matrix.json records the arm state.
    let matrix_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("matrix.json")).unwrap())
            .unwrap();
    let arm_states: Vec<(String, String)> = matrix_json["arms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["name"].as_str().unwrap().to_owned(),
                a["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let (_, skipped_state) = arm_states.iter().find(|(n, _)| n == "arm-skipped").unwrap();
    assert_eq!(skipped_state, "skipped_budget");
}

#[tokio::test]
async fn matrix_resume_skips_complete_arms() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_matrix_toml(
        tmp.path(),
        &[
            ("arm-done", "deterministic"),
            ("arm-pending", "deterministic"),
        ],
    );
    let output = tmp.path().join("out");

    // First run: complete both arms.
    let args = default_matrix_args(config.clone(), dataset.clone(), output.clone());
    matrix_run(args).await.unwrap();

    // Simulate a partial run: mark arm-pending as pending in matrix.json.
    {
        let state_path = output.join("matrix.json");
        let mut state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        for arm in state["arms"].as_array_mut().unwrap() {
            if arm["name"] == "arm-pending" {
                arm["state"] = serde_json::Value::String("pending".into());
            }
        }
        std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();
    }
    // Remove arm-pending's results to prove it will be re-run.
    if output.join("arm-pending").exists() {
        std::fs::remove_dir_all(output.join("arm-pending")).unwrap();
    }

    // Resume run: arm-done is skipped, arm-pending runs.
    let mut args2 = default_matrix_args(config, dataset, output.clone());
    args2.resume = true;
    matrix_run(args2).await.unwrap();

    assert!(
        output.join("arm-pending").join("results.json").exists(),
        "arm-pending should have been run on resume"
    );
}

#[tokio::test]
async fn matrix_summary_rows_are_ranked_contiguously_from_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_matrix_toml(
        tmp.path(),
        &[("arm-a", "deterministic"), ("arm-b", "deterministic")],
    );
    let output = tmp.path().join("out");

    let args = default_matrix_args(config, dataset, output.clone());
    let summary = matrix_run(args).await.unwrap();

    let ranks: Vec<usize> = summary.arms.iter().map(|a| a.rank).collect();
    let expected: Vec<usize> = (1..=summary.arms.len()).collect();
    assert_eq!(
        ranks, expected,
        "ranks must be contiguous starting from 1: {ranks:?}"
    );
}

#[tokio::test]
async fn matrix_summary_json_is_well_formed() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2"]);
    let config = write_matrix_toml(tmp.path(), &[("arm-x", "deterministic")]);
    let output = tmp.path().join("out");

    let args = default_matrix_args(config, dataset, output.clone());
    matrix_run(args).await.unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("matrix-summary.json")).unwrap())
            .unwrap();
    let arms = json["arms"].as_array().unwrap();
    assert_eq!(arms.len(), 1);
    assert_eq!(arms[0]["name"], "arm-x");
    assert!(arms[0]["rank"].is_number());
    assert!(arms[0]["resolved_rate"].is_number());
    assert!(arms[0]["total_cost_usd"].is_number());
    assert!(arms[0]["state"].is_string());
}

#[tokio::test]
async fn matrix_instance_list_is_deterministic_for_same_seed() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-a", "inst-b", "inst-c"]);

    let mut run_ids = Vec::new();
    for run in 0..2 {
        let config = write_matrix_toml(tmp.path(), &[("arm1", "deterministic")]);
        let output = tmp.path().join(format!("run-{run}"));
        let mut args = default_matrix_args(config, dataset.clone(), output.clone());
        args.sample = Some(2);
        args.seed = Some(42);
        matrix_run(args).await.unwrap();

        let path = output.join("matrix.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut ids: Vec<String> = v["instance_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i.as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        run_ids.push(ids);
    }

    assert_eq!(
        run_ids[0], run_ids[1],
        "same seed must produce same instance list"
    );
}

// ── Integration: matrix.json structure ───────────────────────────────────────

#[tokio::test]
async fn matrix_json_has_correct_artifact_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_matrix_toml(tmp.path(), &[("arm1", "deterministic")]);
    let output = tmp.path().join("out");

    let args = default_matrix_args(config, dataset, output.clone());
    matrix_run(args).await.unwrap();

    let state_path = output.join("matrix.json");
    assert!(state_path.exists());
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state["artifact_kind"], "matrix");
    assert!(state["instance_ids"].is_array());
    assert!(state["arms"].is_array());
    assert!(state["filter_spec"].is_object());
}

#[tokio::test]
async fn matrix_arm_dirs_are_nested_inside_output() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_matrix_toml(
        tmp.path(),
        &[("arm-one", "deterministic"), ("arm-two", "deterministic")],
    );
    let output = tmp.path().join("out");

    let args = default_matrix_args(config, dataset, output.clone());
    matrix_run(args).await.unwrap();

    assert!(
        output.join("arm-one").is_dir(),
        "arm-one must be a subdir of output"
    );
    assert!(
        output.join("arm-two").is_dir(),
        "arm-two must be a subdir of output"
    );
    assert!(output.join("arm-one").join("results.json").exists());
    assert!(output.join("arm-two").join("results.json").exists());
}
