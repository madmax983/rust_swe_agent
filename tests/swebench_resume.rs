//! Resume behavior for the `bench swebench` sweep:
//!   * pre-existing valid trajectory + patch → task is skipped (no agent runs)
//!   * pre-existing invalid trajectory → file is treated as absent and the
//!     task re-runs, producing a fresh, valid trajectory
//!   * without `--resume`, valid pre-existing files are *not* skipped

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::Config;
use rust_swe_agent::run::github_pr::{GithubPrSweepConfig, PublishMode};
use rust_swe_agent::run::swebench::{
    SwebenchArgs, patch_path_for_run, run, trajectory_path_for_run,
};
use rust_swe_agent::trajectory::{
    FORMAT_VERSION, FailureCategory, Trajectory, TrajectoryInfo, outcome,
};

fn write_dataset(path: &Path, instance_ids: &[&str]) {
    let mut s = String::new();
    for id in instance_ids {
        // A no-op problem statement is fine — the deterministic model never
        // reads it; only the instance_id matters for trajectory naming.
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\"}}"
        );
    }
    std::fs::write(path, s).unwrap();
}

fn write_valid_trajectory(path: &Path, marker: &str) {
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
    std::fs::write(path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();
}

fn submit_only_responses() -> Vec<String> {
    // First model turn already submits — no shell action required, so the
    // local environment is never invoked beyond template setup.
    vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfresh-run\n```".into()]
}

/// Initialize a git working tree at `dir` with a single empty commit.
/// Patch capture wires `git diff <base> -- .` against this directory, so
/// every test that exercises a fresh sweep needs one — otherwise patch
/// capture would fail and downgrade the run's outcome to `error`.
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
    let toml = format!("[environment]\nworkdir = \"{workdir}\"\n");
    Config::from_toml_str(&toml).unwrap()
}

fn config_with_workdir_and_redaction(dir: &Path, secret: &str) -> Config {
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let toml = format!(
        r#"
[environment]
workdir = "{workdir}"

[redaction]
secret_literals = ["{secret}"]
"#
    );
    Config::from_toml_str(&toml).unwrap()
}

#[tokio::test]
async fn resume_skips_valid_trajectory_and_reruns_invalid() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["pre-valid", "pre-invalid", "fresh"]);

    // Instance 1: pre-existing *valid* trajectory + patch → must be skipped.
    let valid_path = output.join("pre-valid.traj.json");
    write_valid_trajectory(&valid_path, "preserved");
    let valid_before = std::fs::read(&valid_path).unwrap();
    let valid_patch_path = output.join("pre-valid.patch");
    std::fs::write(&valid_patch_path, "preserved-diff\n").unwrap();
    let valid_patch_before = std::fs::read(&valid_patch_path).unwrap();

    // Instance 2: pre-existing *invalid* trajectory → treated as absent.
    let invalid_path = output.join("pre-invalid.traj.json");
    std::fs::write(&invalid_path, "{\"trajectory_format\":\"mini-swe-").unwrap();

    // Instance 3: no pre-existing file → fresh run.

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 2,
        reruns: 1,
        config: cfg,
        resume: true,
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
    })
    .await
    .unwrap();

    assert_eq!(results.total, 3);
    assert_eq!(results.skipped, 1, "exactly one task should be skipped");

    // Skipped instance: trajectory + patch files are byte-identical to what
    // we wrote.
    let valid_after = std::fs::read(&valid_path).unwrap();
    assert_eq!(
        valid_before, valid_after,
        "skipped trajectory should be untouched"
    );
    let valid_patch_after = std::fs::read(&valid_patch_path).unwrap();
    assert_eq!(
        valid_patch_before, valid_patch_after,
        "skipped patch should be untouched"
    );

    // Invalid pre-existing trajectory was overwritten with a fresh, parseable one.
    let reread: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, "pre-invalid", 1)).unwrap(),
    )
    .unwrap();
    assert_eq!(reread.trajectory_format, FORMAT_VERSION);
    assert_eq!(reread.info.outcome.as_deref(), Some(outcome::SUBMITTED));

    // Fresh instance got a brand-new trajectory file too.
    let fresh_path = trajectory_path_for_run(&output, "fresh", 1);
    let fresh: Trajectory =
        serde_json::from_str(&std::fs::read_to_string(&fresh_path).unwrap()).unwrap();
    assert_eq!(fresh.info.outcome.as_deref(), Some(outcome::SUBMITTED));

    // Both freshly-run instances got a `.patch` file (empty, since the agent
    // submitted without modifying anything in `repo`).
    assert!(patch_path_for_run(&output, "pre-invalid", 1).exists());
    assert!(patch_path_for_run(&output, "fresh", 1).exists());

    // Summary table mentions the skipped count.
    let table = results.summary_table();
    assert!(
        table.contains("Skipped:            1 — trajectory already on disk"),
        "missing skipped row: {table}"
    );
}

#[tokio::test]
async fn resume_reruns_submitted_trajectory_with_missing_patch() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["needs-patch"]);

    // Submitted trajectory but the patch file is missing — common shape
    // for sweeps run before #9 landed. Resume must re-run rather than
    // emit `all_preds.jsonl` with an absent diff.
    let traj = output.join("needs-patch.traj.json");
    write_valid_trajectory(&traj, "stale-without-patch");

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
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
    })
    .await
    .unwrap();

    assert_eq!(results.skipped, 0, "missing patch must trigger a re-run");

    // After the re-run both files exist.
    assert!(patch_path_for_run(&output, "needs-patch", 1).exists());
    let reread: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, "needs-patch", 1)).unwrap(),
    )
    .unwrap();
    assert_eq!(reread.info.outcome.as_deref(), Some(outcome::SUBMITTED));
    assert!(
        !reread.info.other.contains_key("test_marker"),
        "stale trajectory was not overwritten"
    );
}

#[tokio::test]
async fn without_resume_existing_trajectories_are_overwritten() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["only"]);

    let traj_path = output.join("only.traj.json");
    write_valid_trajectory(&traj_path, "stale");

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
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
    })
    .await
    .unwrap();

    assert_eq!(results.total, 1);
    assert_eq!(
        results.skipped, 0,
        "no tasks may be skipped without --resume"
    );

    // The stale marker we wrote should have been overwritten by a fresh run.
    let reread: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, "only", 1)).unwrap(),
    )
    .unwrap();
    assert!(
        !reread.info.other.contains_key("test_marker"),
        "stale trajectory was not overwritten without --resume"
    );
}

#[tokio::test]
async fn malformed_results_json_does_not_block_new_non_resume_sweep() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["one"]);
    std::fs::write(output.join("results.json"), "{not-json").unwrap();

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
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
    })
    .await
    .unwrap();

    assert_eq!(results.total, 1);
    assert_eq!(results.submitted, 1);
    assert!(trajectory_path_for_run(&output, "one", 1).exists());
}

#[tokio::test]
async fn resume_uses_on_disk_patch_flags_even_if_prior_summary_is_false() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["one"]);

    let mut traj = Trajectory::new();
    traj.info.outcome = Some(outcome::SUBMITTED.into());
    traj.info.exit_reason = Some("submitted".into());
    traj.info.steps = Some(1);
    std::fs::write(
        output.join("one.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
    std::fs::write(output.join("one.patch"), b"diff --git a/x b/x\n").unwrap();

    // Older/stale-style summary claims no patch even though `.patch` exists.
    let stale = serde_json::json!({
        "total": 1,
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "budget_halted": 0,
        "with_patch": 0,
        "total_prompt_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "retries": 0,
        "retried_instances": 0,
        "filter_spec": {"original_count": 1, "selected_count": 1},
        "instances": [{
            "instance_id": "one",
            "exit_reason": "submitted",
            "outcome": "submitted",
            "steps": 1,
            "patch_present": false,
            "non_empty_patch": false
        }]
    });
    std::thread::sleep(std::time::Duration::from_millis(15));
    std::fs::write(
        output.join("results.json"),
        serde_json::to_string_pretty(&stale).unwrap(),
    )
    .unwrap();

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
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
    })
    .await
    .unwrap();

    assert_eq!(results.skipped, 1);
    let preds = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    assert_eq!(preds.lines().count(), 1, "{preds}");
    let pred: serde_json::Value = serde_json::from_str(preds.lines().next().unwrap()).unwrap();
    assert_eq!(
        pred.get("model_patch").and_then(serde_json::Value::as_str),
        Some("diff --git a/x b/x\n")
    );
}

#[tokio::test]
async fn resume_raw_secret_patch_downgrades_instance_and_writes_results() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["leaky", "clean"]);

    let secret = "legacy-prediction-secret-value";
    write_valid_trajectory(&output.join("leaky.traj.json"), "leaky");
    write_valid_trajectory(&output.join("clean.traj.json"), "clean");
    std::fs::write(
        output.join("leaky.patch"),
        format!("diff --git a/x b/x\n+{secret}\n"),
    )
    .unwrap();
    std::fs::write(output.join("clean.patch"), "diff --git a/x b/x\n+clean\n").unwrap();

    let cfg = config_with_workdir_and_redaction(&repo, secret);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
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
    })
    .await
    .unwrap();

    assert_eq!(results.total, 2);
    assert_eq!(results.skipped, 1, "{results:#?}");
    assert_eq!(results.errored, 1, "{results:#?}");
    assert_eq!(results.submitted, 0, "{results:#?}");
    assert_eq!(
        results
            .failures_by_category
            .get(&FailureCategory::SecretLeakDetected),
        Some(&1)
    );

    let leaky = results
        .instances
        .iter()
        .find(|row| row.instance_id == "leaky")
        .unwrap();
    assert_eq!(leaky.outcome.as_deref(), Some(outcome::ERROR));
    assert_eq!(leaky.exit_reason, "error");
    assert_eq!(
        leaky.failure_category,
        Some(FailureCategory::SecretLeakDetected)
    );
    assert!(!leaky.pass_at_1);
    assert_eq!(leaky.resolved_count, 0);

    let results_json = std::fs::read_to_string(output.join("results.json")).unwrap();
    assert!(results_json.contains("\"sweep_status\": \"completed\""));
    assert!(results_json.contains("secret_leak_detected"));
    assert!(!results_json.contains(secret), "{results_json}");

    let preds = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    assert_eq!(preds.lines().count(), 1, "{preds}");
    assert!(preds.contains("\"instance_id\":\"clean\""), "{preds}");
    assert!(!preds.contains(secret), "{preds}");

    let leaky_patch = std::fs::read_to_string(output.join("leaky.patch")).unwrap();
    assert!(!leaky_patch.contains(secret), "{leaky_patch}");
    assert!(leaky_patch.contains("[REDACTED:configured_literal:"));
}

#[tokio::test]
async fn resume_raw_secret_patch_blocks_github_pr_publication_before_publish() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["leaky-pr"]);

    let secret = "legacy-pr-secret-value";
    write_valid_trajectory(&output.join("leaky-pr.traj.json"), "leaky-pr");
    std::fs::write(
        output.join("leaky-pr.patch"),
        format!("diff --git a/x b/x\n+{secret}\n"),
    )
    .unwrap();

    let cfg = config_with_workdir_and_redaction(&repo, secret);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
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
        github_pr: Some(GithubPrSweepConfig {
            target_repo: "not-a-valid-owner-repo".into(),
            target_branch: "trunk".into(),
            token_env: "GITHUB_TOKEN".into(),
            mode: PublishMode::DryRun,
            timeout_secs: 1,
            max_retries: 0,
            backoff_base_ms: 1,
            branch_prefix: "rust-swe-agent".into(),
        }),
    })
    .await
    .unwrap();

    assert_eq!(results.total, 1);
    assert_eq!(results.errored, 1, "{results:#?}");
    assert_eq!(results.submitted, 0, "{results:#?}");
    assert_eq!(results.github_pr_failures, 0, "{results:#?}");
    assert_eq!(
        results
            .failures_by_category
            .get(&FailureCategory::SecretLeakDetected),
        Some(&1)
    );

    let row = &results.instances[0];
    assert_eq!(row.instance_id, "leaky-pr");
    assert_eq!(row.outcome.as_deref(), Some(outcome::ERROR));
    assert_eq!(
        row.failure_category,
        Some(FailureCategory::SecretLeakDetected)
    );
    assert!(
        row.github_pr_error.is_none(),
        "leaky resumed patch reached PR publisher: {:?}",
        row.github_pr_error
    );

    let leaky_patch = std::fs::read_to_string(output.join("leaky-pr.patch")).unwrap();
    assert!(!leaky_patch.contains(secret), "{leaky_patch}");
    assert!(leaky_patch.contains("[REDACTED:configured_literal:"));

    let preds = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    assert!(preds.is_empty(), "{preds}");
}

#[tokio::test]
async fn resume_skipped_submitted_runs_still_attempt_github_pr_publication() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["already-done"]);

    let traj_path = output.join("already-done.traj.json");
    write_valid_trajectory(&traj_path, "resume-pr");
    std::fs::write(output.join("already-done.patch"), "diff --git a/x b/x\n").unwrap();

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: true,
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
        github_pr: Some(GithubPrSweepConfig {
            target_repo: "not-a-valid-owner-repo".into(),
            target_branch: "trunk".into(),
            token_env: "GITHUB_TOKEN".into(),
            mode: PublishMode::DryRun,
            timeout_secs: 1,
            max_retries: 0,
            backoff_base_ms: 1,
            branch_prefix: "rust-swe-agent".into(),
        }),
    })
    .await
    .unwrap();

    assert_eq!(results.skipped, 1);
    assert_eq!(results.github_pr_failures, 1);
    assert_eq!(results.instances.len(), 1);
    let row = &results.instances[0];
    assert_eq!(row.outcome.as_deref(), Some(outcome::SUBMITTED));
    assert!(row.github_pr_error.is_some());

    let preds = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    assert_eq!(preds.lines().count(), 1, "{preds}");
}
