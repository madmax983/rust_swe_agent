//! Per-instance reruns for `bench swebench`.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use maxwells_daemon::Config;
use maxwells_daemon::run::swebench::{
    SwebenchArgs, SweepResults, patch_path_for_run, predictions_path_for_run, run,
    trajectory_path_for_run,
};
use maxwells_daemon::trajectory::{FORMAT_VERSION, Trajectory, TrajectoryInfo, outcome};

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

fn submit_only_responses() -> Vec<String> {
    vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfresh-run\n```".into()]
}

fn init_repo(dir: &Path) {
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@test"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
}

fn config_with_workdir(dir: &Path) -> Config {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.environment.workdir = dir.display().to_string();
    cfg
}

fn base_args(dataset: std::path::PathBuf, output: std::path::PathBuf, cfg: Config) -> SwebenchArgs {
    SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
        output_dir: output,
        parallel: 2,
        config: cfg,
        resume: false,
        reruns: 3,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: maxwells_daemon::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(submit_only_responses()),
        deterministic_usage_per_call: None,
        config_overlay_paths: Vec::new(),
        dry_run: false,
        skip_preflight: true,
        preflight_format: "text".into(),
        skip_model_probe: true,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "test".into(),
        skip_patch_validation: true,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: 30,
        install_os_signal_handlers: false,
        cancellation_signals: None,
        github_pr: None,
        reproduced_from: None,
        abort_on_systemic_failure: true,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
        otlp_endpoint: None,
    }
}

fn write_valid_run(output: &Path, instance_id: &str, run_index: u32, marker: &str) {
    let mut info = TrajectoryInfo {
        outcome: Some(outcome::SUBMITTED.into()),
        exit_reason: Some("submitted".into()),
        steps: Some(0),
        ..Default::default()
    };
    info.other.insert(
        "test_marker".into(),
        serde_json::Value::String(marker.into()),
    );
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    let traj_path = trajectory_path_for_run(output, instance_id, run_index);
    std::fs::create_dir_all(traj_path.parent().unwrap()).unwrap();
    std::fs::write(&traj_path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();
    std::fs::write(patch_path_for_run(output, instance_id, run_index), b"").unwrap();
}

fn write_valid_tested_run(output: &Path, instance_id: &str, run_index: u32, marker: &str) {
    let mut info = TrajectoryInfo {
        outcome: Some(outcome::SUBMITTED.into()),
        exit_reason: Some("submitted".into()),
        steps: Some(0),
        tests_run_before_submit: true,
        last_tests_passed: Some(true),
        ..Default::default()
    };
    info.other.insert(
        "test_marker".into(),
        serde_json::Value::String(marker.into()),
    );
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    let traj_path = trajectory_path_for_run(output, instance_id, run_index);
    std::fs::create_dir_all(traj_path.parent().unwrap()).unwrap();
    std::fs::write(&traj_path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();
    std::fs::write(patch_path_for_run(output, instance_id, run_index), b"").unwrap();
}

#[tokio::test]
async fn rerun_writes_nested_run_files_and_pass_at_k_summary() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["task-a"]);

    let results = run(base_args(
        dataset,
        output.clone(),
        config_with_workdir(&repo),
    ))
    .await
    .unwrap();

    assert_eq!(results.total, 1);
    assert_eq!(results.submitted, 3);
    assert!((results.pass_at_k - 1.0).abs() < f64::EPSILON);
    assert_eq!(results.instances.len(), 1);
    let row = &results.instances[0];
    assert_eq!(row.instance_id, "task-a");
    assert_eq!(row.runs, 3);
    assert_eq!(row.resolved_count, 3);
    assert!(row.pass_at_1);

    for run_index in 1..=3 {
        assert!(
            trajectory_path_for_run(&output, "task-a", run_index).exists(),
            "missing run-{run_index} trajectory"
        );
        assert!(
            patch_path_for_run(&output, "task-a", run_index).exists(),
            "missing run-{run_index} patch"
        );
    }
    assert!(
        !output.join("task-a.traj.json").exists(),
        "reruns should use nested per-run trajectories"
    );

    let persisted: SweepResults =
        serde_json::from_str(&std::fs::read_to_string(output.join("results.json")).unwrap())
            .unwrap();
    assert_eq!(persisted.instances[0].runs, 3);
    assert_eq!(persisted.instances[0].resolved_count, 3);

    let table = results.summary_table();
    assert!(table.contains("Pass@3:            100.00%"), "{table}");
    assert!(
        table.contains("Effective tasks:    3 (1 instances * 3 runs)"),
        "{table}"
    );

    let all_preds = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    let mut aggregate_ids = BTreeSet::new();
    let mut aggregate_run_indexes = BTreeSet::new();
    for (idx, line) in all_preds.lines().enumerate() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let prediction_id = row
            .get("instance_id")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(
            aggregate_ids.insert(prediction_id.to_owned()),
            "duplicate aggregate prediction id on line {idx}: {prediction_id}"
        );
        assert_eq!(
            row.get("original_instance_id")
                .and_then(serde_json::Value::as_str),
            Some("task-a")
        );
        aggregate_run_indexes.insert(row.get("run_index").and_then(serde_json::Value::as_u64));
    }
    assert_eq!(aggregate_ids.len(), 3);
    assert_eq!(
        aggregate_run_indexes,
        BTreeSet::from([Some(1), Some(2), Some(3)])
    );

    for run_index in 1..=3 {
        let run_preds = std::fs::read_to_string(predictions_path_for_run(&output, run_index))
            .unwrap_or_else(|err| panic!("missing run-{run_index} predictions: {err}"));
        let rows = run_preds.lines().collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "run-{run_index} predictions: {run_preds}");
        let row: serde_json::Value = serde_json::from_str(rows[0]).unwrap();
        assert_eq!(
            row.get("instance_id").and_then(serde_json::Value::as_str),
            Some("task-a")
        );
        assert_eq!(
            row.get("run_index").and_then(serde_json::Value::as_u64),
            Some(u64::from(run_index))
        );
    }
}

#[tokio::test]
async fn resume_skips_only_completed_run_slots() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["task-a"]);

    write_valid_run(&output, "task-a", 1, "preserved-run-1");
    let before = std::fs::read(trajectory_path_for_run(&output, "task-a", 1)).unwrap();

    let mut args = base_args(dataset, output.clone(), config_with_workdir(&repo));
    args.resume = true;
    let results = run(args).await.unwrap();

    assert_eq!(results.total, 1);
    assert_eq!(results.skipped, 1, "only run-1 should resume-skip");
    assert_eq!(results.submitted, 2, "run-2 and run-3 should launch");
    assert_eq!(results.instances[0].runs, 3);
    assert_eq!(results.instances[0].resolved_count, 3);
    assert_eq!(
        before,
        std::fs::read(trajectory_path_for_run(&output, "task-a", 1)).unwrap(),
        "resume-skipped run should be untouched"
    );
    assert!(trajectory_path_for_run(&output, "task-a", 2).exists());
    assert!(trajectory_path_for_run(&output, "task-a", 3).exists());
}

#[tokio::test]
async fn resumed_tested_run_does_not_inflate_fresh_submitted_with_tests_count() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["task-a"]);

    write_valid_tested_run(&output, "task-a", 1, "preserved-tested-run-1");

    let mut args = base_args(dataset, output.clone(), config_with_workdir(&repo));
    args.resume = true;
    args.reruns = 2;
    let results = run(args).await.unwrap();

    assert_eq!(results.submitted, 1, "only run-2 should launch fresh");
    assert_eq!(results.skipped, 1, "run-1 should resume-skip");
    assert_eq!(
        results.submitted_with_tests, 0,
        "resumed test telemetry should not count against fresh submitted denominator"
    );
    let table = results.summary_table();
    assert!(table.contains("Submitted w/tests:  0/1"), "{table}");
}
