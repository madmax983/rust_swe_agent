//! Graceful cancellation for `bench swebench`.

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use rust_swe_agent::Config;
use rust_swe_agent::run::swebench::{SwebenchArgs, SweepSignal, run, trajectory_path_for_run};
use rust_swe_agent::trajectory::{FailureCategory, Trajectory, outcome};
use tokio::sync::mpsc;

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
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    Config::from_toml_str(&format!("[environment]\nworkdir = \"{workdir}\"\n")).unwrap()
}

fn submit_only_responses() -> Vec<String> {
    vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfresh-run\n```".into()]
}

fn rate_limited_then_submit_responses() -> Vec<String> {
    vec![
        "__rate_limited__:0".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nretry-success\n```".into(),
    ]
}

fn staged_sleep_responses(first_marker: &Path, second_started: &Path) -> Vec<String> {
    let first = first_marker.display().to_string();
    let second = second_started.display().to_string();
    let command = if cfg!(windows) {
        let blocker = first_marker.with_file_name("blocker.cmd");
        std::fs::write(&blocker, "@echo off\r\n:loop\r\ngoto loop\r\n").unwrap();
        let blocker = blocker.display().to_string();
        format!(
            "if exist \"{first}\" (echo started>\"{second}\" & call \"{blocker}\") else (echo done>\"{first}\")"
        )
    } else {
        format!(
            "if [ -f '{first}' ]; then echo started > '{second}'; sleep 30; else echo done > '{first}'; fi"
        )
    };
    vec![
        format!("We need a shell step.\n```bash\n{command}\n```"),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
    ]
}

fn partial_output_sleep_response(started_marker: &Path) -> Vec<String> {
    let marker = started_marker.display().to_string();
    let command = if cfg!(windows) {
        format!(
            "echo partial-before-cancel & echo started>\"{marker}\" & for /L %i in (1,1,2147483647) do @rem"
        )
    } else {
        format!("echo partial-before-cancel; echo started > '{marker}'; sleep 30")
    };
    vec![format!("```bash\n{command}\n```")]
}

async fn wait_for_path(path: PathBuf) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {}", path.display());
}

fn base_args(
    dataset: PathBuf,
    output: PathBuf,
    repo: &Path,
    responses: Vec<String>,
    signal_rx: Option<mpsc::UnboundedReceiver<SweepSignal>>,
) -> SwebenchArgs {
    SwebenchArgs {
        dataset_source: rust_swe_agent::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
        output_dir: output,
        parallel: 1,
        reruns: 1,
        config: config_with_workdir(repo),
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: rust_swe_agent::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(responses),
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
        cancel_deadline_secs: 1,
        install_os_signal_handlers: false,
        cancellation_signals: signal_rx,
        github_pr: None,
        reproduced_from: None,
        abort_on_systemic_failure: true,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
    }
}

#[tokio::test]
async fn graceful_sigint_persists_cancelled_inflight_and_resume_retries_it() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(
        &dataset,
        &["done-before-cancel", "cancelled-mid-step", "never-started"],
    );
    let first_marker = work.path().join("first-marker");
    let second_started = work.path().join("second-started");
    let (signal_tx, signal_rx) = mpsc::unbounded_channel();
    tokio::spawn({
        let second_started = second_started.clone();
        async move {
            wait_for_path(second_started).await;
            signal_tx.send(SweepSignal::Interrupt).unwrap();
        }
    });

    let started = Instant::now();
    let results = run(base_args(
        dataset.clone(),
        output.clone(),
        &repo,
        staged_sleep_responses(&first_marker, &second_started),
        Some(signal_rx),
    ))
    .await
    .unwrap();

    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(results.sweep_status, "cancelled");
    assert_eq!(results.cancel_exit_code, Some(130));
    assert!(results.cancelled_at.is_some());
    assert_eq!(results.completed, 1);
    assert_eq!(results.in_flight_at_cancel, 1);
    assert_eq!(results.not_started, 1);
    assert_eq!(results.submitted, 1);

    let cancelled_path = trajectory_path_for_run(&output, "cancelled-mid-step", 1);
    let cancelled: Trajectory =
        serde_json::from_str(&std::fs::read_to_string(cancelled_path).unwrap()).unwrap();
    assert_eq!(cancelled.info.outcome.as_deref(), Some(outcome::ERROR));
    assert_eq!(cancelled.info.exit_reason.as_deref(), Some("cancelled"));
    assert!(
        !trajectory_path_for_run(&output, "never-started", 1).exists(),
        "not-started instances must not get phantom trajectories"
    );

    let mut resume_args = base_args(dataset, output, &repo, submit_only_responses(), None);
    resume_args.resume = true;
    let resumed = run(resume_args).await.unwrap();
    assert_eq!(resumed.total, 3);
    assert_eq!(resumed.skipped, 1);
    assert_eq!(resumed.submitted, 2);
}

#[tokio::test]
async fn second_sigint_escalates_to_cancelled_exit_137() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["done-before-cancel", "cancelled-mid-step"]);
    let first_marker = work.path().join("first-marker");
    let second_started = work.path().join("second-started");
    let (signal_tx, signal_rx) = mpsc::unbounded_channel();
    tokio::spawn({
        let second_started = second_started.clone();
        async move {
            wait_for_path(second_started).await;
            signal_tx.send(SweepSignal::Interrupt).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            signal_tx.send(SweepSignal::Interrupt).unwrap();
        }
    });

    let mut args = base_args(
        dataset,
        output,
        &repo,
        staged_sleep_responses(&first_marker, &second_started),
        Some(signal_rx),
    );
    args.cancel_deadline_secs = 30;

    let started = Instant::now();
    let results = run(args).await.unwrap();

    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(results.sweep_status, "cancelled");
    assert_eq!(results.cancel_exit_code, Some(137));
    assert_eq!(results.completed, 1);
    assert_eq!(results.in_flight_at_cancel, 1);
}

#[tokio::test]
async fn sigterm_uses_graceful_cancel_exit_130() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["done-before-cancel", "cancelled-mid-step"]);
    let first_marker = work.path().join("first-marker");
    let second_started = work.path().join("second-started");
    let (signal_tx, signal_rx) = mpsc::unbounded_channel();
    tokio::spawn({
        let second_started = second_started.clone();
        async move {
            wait_for_path(second_started).await;
            signal_tx.send(SweepSignal::Terminate).unwrap();
        }
    });

    let mut args = base_args(
        dataset,
        output,
        &repo,
        staged_sleep_responses(&first_marker, &second_started),
        Some(signal_rx),
    );
    args.cancel_deadline_secs = 0;

    let results = run(args).await.unwrap();

    assert_eq!(results.sweep_status, "cancelled");
    assert_eq!(results.cancel_exit_code, Some(130));
    assert_eq!(results.completed, 1);
    assert_eq!(results.in_flight_at_cancel, 1);
}

#[tokio::test]
async fn forced_cancel_preserves_partial_command_output_in_trajectory() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["cancelled-with-output"]);
    let started_marker = work.path().join("command-started");
    let (signal_tx, signal_rx) = mpsc::unbounded_channel();
    tokio::spawn({
        let started_marker = started_marker.clone();
        async move {
            wait_for_path(started_marker).await;
            signal_tx.send(SweepSignal::Interrupt).unwrap();
        }
    });

    let mut args = base_args(
        dataset,
        output.clone(),
        &repo,
        partial_output_sleep_response(&started_marker),
        Some(signal_rx),
    );
    args.cancel_deadline_secs = 0;

    let started = Instant::now();
    let results = run(args).await.unwrap();

    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(results.sweep_status, "cancelled");
    let row = results
        .instances
        .iter()
        .find(|row| row.instance_id == "cancelled-with-output")
        .unwrap();
    assert_eq!(row.exit_reason, "cancelled");
    assert_eq!(row.failure_category, None);
    assert!(
        !results
            .failures_by_category
            .contains_key(&FailureCategory::Unknown),
        "cancelled tasks must not be counted as unknown failures"
    );
    let traj: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, "cancelled-with-output", 1))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(traj.info.exit_reason.as_deref(), Some("cancelled"));
    assert!(
        traj.messages
            .iter()
            .any(|message| message.content.contains("partial-before-cancel")),
        "{}",
        serde_json::to_string_pretty(&traj).unwrap()
    );
}

#[tokio::test]
async fn forced_cancel_interrupts_pre_run_rate_limit_wait() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["rate-limit-winner", "rate-limit-waiter"]);
    let (signal_tx, signal_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        signal_tx.send(SweepSignal::Interrupt).unwrap();
    });

    let mut args = base_args(
        dataset,
        output.clone(),
        &repo,
        submit_only_responses(),
        Some(signal_rx),
    );
    args.parallel = 2;
    args.max_rpm = Some(1);
    args.cancel_deadline_secs = 0;

    let started = Instant::now();
    let results = match tokio::time::timeout(Duration::from_secs(8), run(args)).await {
        Ok(result) => result.unwrap(),
        Err(err) => {
            panic!("forced cancellation should interrupt the rate-limit governor wait: {err}")
        }
    };

    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(results.sweep_status, "cancelled");
    assert_eq!(results.cancel_exit_code, Some(130));
    let Some(cancelled) = results
        .instances
        .iter()
        .find(|row| row.exit_reason == "cancelled")
    else {
        panic!("one worker should be cancelled while waiting for the governor");
    };
    let traj: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, &cancelled.instance_id, 1))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(traj.info.outcome.as_deref(), Some(outcome::ERROR));
    assert_eq!(traj.info.exit_reason.as_deref(), Some("cancelled"));
    assert_eq!(traj.info.failure_category, None);
}

#[tokio::test]
async fn forced_cancel_interrupts_retry_backoff_wait() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset, &["retry-backoff-waiter"]);
    let (signal_tx, signal_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        signal_tx.send(SweepSignal::Interrupt).unwrap();
    });

    let mut args = base_args(
        dataset,
        output.clone(),
        &repo,
        rate_limited_then_submit_responses(),
        Some(signal_rx),
    );
    args.max_retries = 1;
    args.retry_on = Some("model_api".into());
    args.retry_backoff_base_ms = 30_000;
    args.retry_backoff_cap_s = 30;
    args.cancel_deadline_secs = 0;

    let started = Instant::now();
    let results = match tokio::time::timeout(Duration::from_secs(8), run(args)).await {
        Ok(result) => result.unwrap(),
        Err(err) => panic!("forced cancellation should interrupt retry backoff: {err}"),
    };

    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(results.sweep_status, "cancelled");
    assert_eq!(results.cancel_exit_code, Some(130));
    let row = results
        .instances
        .iter()
        .find(|row| row.instance_id == "retry-backoff-waiter")
        .unwrap();
    assert_eq!(row.exit_reason, "cancelled");
    let traj: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, "retry-backoff-waiter", 1))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(traj.info.outcome.as_deref(), Some(outcome::ERROR));
    assert_eq!(traj.info.exit_reason.as_deref(), Some("cancelled"));
    assert_eq!(traj.info.failure_category, None);
}
