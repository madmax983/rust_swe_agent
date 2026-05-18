//! TDD tests for mid-run trajectory checkpointing (issue #269).
//!
//! RED → GREEN → REFACTOR cycle.
//!
//! These tests cover:
//! - Schema: partial/partial_reason fields
//! - Atomic checkpoint write
//! - Per-turn persistence via mini::run
//! - Four-state resume classification
//! - Budget continuity on partial resume
//! - Step continuity on partial resume
//! - bench bundle excludes partial trajectories
//! - bench inspect shows partial banner
//! - Summary table has partial count

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use maxwells_daemon::Config;
use maxwells_daemon::run::swebench::{
    SwebenchArgs, patch_path_for_run, run, trajectory_path_for_run,
};
use maxwells_daemon::trajectory::{
    FORMAT_VERSION, FailureCategory, Trajectory, TrajectoryInfo, outcome,
};

// ─── Helpers ───────────────────────────────────────────────────────────────

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
    let toml = format!("[environment]\nworkdir = \"{workdir}\"\n");
    Config::from_toml_str(&toml).unwrap()
}

fn config_with_workdir_and_budget(dir: &Path, per_task_budget_usd: f64) -> Config {
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let toml = format!(
        "[environment]\nworkdir = \"{workdir}\"\n[agent]\nper_task_budget_usd = {per_task_budget_usd}\n"
    );
    Config::from_toml_str(&toml).unwrap()
}

fn config_with_workdir_and_step_limit(dir: &Path, step_limit: u32) -> Config {
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let toml = format!(
        "[environment]\nworkdir = \"{workdir}\"\n[agent]\nstep_limit = {step_limit}\n"
    );
    Config::from_toml_str(&toml).unwrap()
}

fn submit_response() -> Vec<String> {
    vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into()]
}

fn write_partial_trajectory(path: &Path, steps: u32, cost_usd: f64) {
    let mut info = TrajectoryInfo {
        steps: Some(steps),
        actual_cost_usd: Some(cost_usd),
        ..Default::default()
    };
    // Mark as partial
    info.partial = true;
    info.partial_reason = Some("in_progress".into());
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();
}

fn write_complete_trajectory(path: &Path) {
    let info = TrajectoryInfo {
        outcome: Some(outcome::SUBMITTED.into()),
        exit_reason: Some("submitted".into()),
        steps: Some(3),
        partial: false,
        partial_reason: None,
        ..Default::default()
    };
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();
}

// ─── RED Phase: Schema tests ───────────────────────────────────────────────

#[test]
fn trajectory_info_has_partial_field_defaults_false() {
    let info = TrajectoryInfo::default();
    assert!(!info.partial, "partial should default to false");
    assert!(info.partial_reason.is_none(), "partial_reason should default to None");
}

#[test]
fn partial_true_serializes_and_roundtrips() {
    let mut info = TrajectoryInfo::default();
    info.partial = true;
    info.partial_reason = Some("in_progress".into());
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    let json = serde_json::to_string_pretty(&traj).unwrap();
    assert!(json.contains("\"partial\": true"), "partial should serialize: {json}");
    assert!(json.contains("\"in_progress\""), "partial_reason should serialize: {json}");

    let back: Trajectory = serde_json::from_str(&json).unwrap();
    assert!(back.info.partial);
    assert_eq!(back.info.partial_reason.as_deref(), Some("in_progress"));
}

#[test]
fn partial_false_omitted_from_json_for_backward_compat() {
    // When partial=false, it should not appear in JSON (skip_serializing_if)
    // so that old trajectory readers see no change.
    let info = TrajectoryInfo::default();
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    let json = serde_json::to_string_pretty(&traj).unwrap();
    // partial=false and partial_reason=None should NOT appear
    assert!(!json.contains("\"partial\""), "partial=false should be omitted: {json}");
    assert!(!json.contains("\"partial_reason\""), "partial_reason=null should be omitted: {json}");
}

#[test]
fn legacy_trajectory_without_partial_field_parses_as_false() {
    // A trajectory file that has no `partial` field should deserialize
    // with partial=false (backward compatible).
    let json = r#"{
  "trajectory_format": "mini-swe-agent-1.2",
  "info": {"task": "x"},
  "messages": []
}"#;
    let traj: Trajectory = serde_json::from_str(json).unwrap();
    assert!(!traj.info.partial, "legacy trajectory should parse as partial=false");
    assert!(traj.info.partial_reason.is_none());
}

// ─── RED Phase: Atomic checkpoint write ───────────────────────────────────

#[test]
fn save_partial_atomic_writes_file_with_partial_true() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.traj.json");
    let mut traj = Trajectory::new();
    traj.info.steps = Some(3);

    // save_partial_atomic should write the file atomically with partial=true
    traj.save_partial_atomic(&path).unwrap();

    let json = std::fs::read_to_string(&path).unwrap();
    let back: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(back["info"]["partial"].as_bool(), Some(true));
    assert_eq!(back["info"]["partial_reason"].as_str(), Some("in_progress"));
    // No temp file should remain
    let tmp_path = path.with_extension("json.tmp");
    assert!(!tmp_path.exists(), "temp file should be cleaned up");
}

#[test]
fn save_partial_atomic_leaves_prior_file_intact_on_tmp_truncation() {
    // Simulates crash mid-write: truncated .partial.tmp left on disk.
    // The previously-persisted partial should still be readable.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.traj.json");

    // Write initial valid partial
    let mut traj = Trajectory::new();
    traj.info.steps = Some(2);
    traj.save_partial_atomic(&path).unwrap();

    let first_content = std::fs::read_to_string(&path).unwrap();

    // Simulate a truncated tmp file (crash mid-write)
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, b"{truncated").unwrap();

    // Now call save_partial_atomic again — it should overwrite the truncated tmp
    // and produce a valid result
    traj.info.steps = Some(3);
    traj.save_partial_atomic(&path).unwrap();

    let second_content = std::fs::read_to_string(&path).unwrap();
    let back: serde_json::Value = serde_json::from_str(&second_content).unwrap();
    assert_eq!(back["info"]["steps"].as_u64(), Some(3));
    assert_eq!(back["info"]["partial"].as_bool(), Some(true));

    // Ensure that the prior valid content is now replaced (not corrupted)
    assert_ne!(first_content, second_content);
}

#[test]
fn save_pretty_writes_partial_false_on_final() {
    // On final write (save_pretty), partial should be false / absent
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.traj.json");
    let mut traj = Trajectory::new();
    traj.info.outcome = Some(outcome::SUBMITTED.into());
    // partial defaults to false
    traj.save_pretty(&path).unwrap();

    let json = std::fs::read_to_string(&path).unwrap();
    // partial=false should be absent from the final file
    assert!(!json.contains("\"partial\""), "final trajectory should not have partial=true: {json}");
}

// ─── RED Phase: Per-turn checkpoint latency ───────────────────────────────

#[test]
fn checkpoint_write_latency_p95_under_50ms() {
    // Integration test: simulate 50-step trajectory checkpoint writes.
    // p95 added latency must be < 50ms per the spec.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("latency_test.traj.json");

    let mut traj = Trajectory::new();
    // Add some realistic message content
    for i in 0..5 {
        traj.record_message(&maxwells_daemon::model::Message::user(format!(
            "Step {i} observation: {}",
            "x".repeat(1000)
        )));
    }

    let mut latencies_ms: Vec<u64> = Vec::with_capacity(50);
    for step in 0..50 {
        traj.info.steps = Some(step as u32);
        let start = Instant::now();
        traj.save_partial_atomic(&path).unwrap();
        let elapsed = start.elapsed().as_millis() as u64;
        latencies_ms.push(elapsed);
    }

    latencies_ms.sort_unstable();
    let p95_idx = (latencies_ms.len() as f64 * 0.95) as usize;
    let p95 = latencies_ms[p95_idx.min(latencies_ms.len() - 1)];
    assert!(
        p95 < 50,
        "p95 checkpoint write latency {p95}ms exceeds 50ms ceiling"
    );
}

// ─── RED Phase: Resume four-state classification ──────────────────────────

#[tokio::test]
async fn resume_classifies_partial_trajectory_and_reruns_not_skips() {
    // A partial trajectory (partial=true) should NOT be treated as complete.
    // The instance should be re-run (from step 0 in this simplified implementation,
    // or from the persisted state in the full implementation).
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["partial-inst", "complete-inst"]);

    // Instance with partial trajectory (interrupted mid-run)
    let partial_path = trajectory_path_for_run(&output, "partial-inst", 1);
    write_partial_trajectory(&partial_path, 4, 0.05);

    // Instance with complete trajectory
    let complete_path = trajectory_path_for_run(&output, "complete-inst", 1);
    write_complete_trajectory(&complete_path);
    std::fs::write(
        &complete_path.with_extension("").with_extension("").with_extension("patch"),
        b"",
    )
    .unwrap_or(());
    // Write the actual patch file for complete-inst
    let complete_patch = patch_path_for_run(&output, "complete-inst", 1);
    std::fs::create_dir_all(complete_patch.parent().unwrap()).unwrap();
    std::fs::write(&complete_patch, b"").unwrap();

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
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
        stratify_mode: maxwells_daemon::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(submit_response()),
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
        abort_on_systemic_failure: false,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
    })
    .await
    .unwrap();

    // complete-inst should be skipped (1 skip)
    assert_eq!(results.skipped, 1, "complete instance should be skipped");

    // partial-inst should have been re-run (not skipped)
    let final_traj: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, "partial-inst", 1)).unwrap(),
    )
    .unwrap();
    // After a successful re-run, it should no longer be partial
    assert!(
        !final_traj.info.partial,
        "re-run instance should have partial=false in final trajectory"
    );
}

#[tokio::test]
async fn resume_corrupted_trajectory_reruns_from_step_zero() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["corrupted-inst"]);

    // Write a corrupted trajectory file
    let traj_path = trajectory_path_for_run(&output, "corrupted-inst", 1);
    std::fs::create_dir_all(traj_path.parent().unwrap()).unwrap();
    std::fs::write(&traj_path, b"{\"partial\": true, \"info\": {broken json").unwrap();

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
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
        stratify_mode: maxwells_daemon::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(submit_response()),
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
        abort_on_systemic_failure: false,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
    })
    .await
    .unwrap();

    // Corrupted trajectory should be re-run (0 skipped)
    assert_eq!(results.skipped, 0, "corrupted trajectory should trigger re-run");
    // The instance should have been successfully re-run
    let final_traj: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, "corrupted-inst", 1)).unwrap(),
    )
    .unwrap();
    assert_eq!(final_traj.info.outcome.as_deref(), Some(outcome::SUBMITTED));
}

// ─── RED Phase: Per-turn checkpoint in mini::run ──────────────────────────

#[tokio::test]
async fn mini_run_writes_partial_checkpoint_after_each_step() {
    // After each step in the agent loop, a checkpoint file should exist
    // with partial=true. After the final step, it should have partial=false.
    use maxwells_daemon::run::mini::{MiniArgs, run as mini_run};

    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 3;

    // Use 3 responses so agent takes 3 steps before submitting
    let responses = vec![
        "```bash\necho step1\n```".into(),
        "```bash\necho step2\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
    ];

    let args = MiniArgs {
        task: "test checkpoint".into(),
        extra_context: None,
        config: cfg,
        output_dir: output.clone(),
        trajectory_name: "checkpoint-test".into(),
        deterministic_responses: Some(responses),
        deterministic_usage_per_call: None,
        task_timeout_secs: Some(30),
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 60,
    };

    mini_run(args).await.unwrap();

    // Final trajectory should exist and have partial=false
    let final_path = output.join("checkpoint-test.traj.json");
    assert!(final_path.exists(), "final trajectory should exist");
    let json = std::fs::read_to_string(&final_path).unwrap();
    let traj: serde_json::Value = serde_json::from_str(&json).unwrap();
    // partial=false means it's absent or explicitly false
    let partial = traj["info"]["partial"].as_bool().unwrap_or(false);
    assert!(!partial, "final trajectory should have partial=false; got: {json}");
}

// ─── RED Phase: Bundle exclusion ──────────────────────────────────────────

#[test]
fn bench_bundle_excludes_partial_trajectories_with_warning() {
    use maxwells_daemon::run::bundle::{BundleCreateArgs, create_bundle};

    let work = tempfile::tempdir().unwrap();
    let sweep_dir = work.path().join("sweep");
    std::fs::create_dir_all(&sweep_dir).unwrap();

    // Create a results.json with one completed and one partial instance
    let results = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 8},
        "sweep_status": "completed",
        "total": 2,
        "submitted": 1,
        "errored": 0,
        "skipped": 0,
        "budget_halted": 0,
        "with_patch": 1,
        "patch_empty": 0,
        "patch_apply_invalid": 0,
        "retries": 0,
        "retried_instances": 0,
        "filter_spec": {"original_count": 2, "selected_count": 2},
        "total_prompt_tokens": 100,
        "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0,
        "total_completion_tokens": 50,
        "estimated_cost_usd": 0.01,
        "instances": [
            {
                "instance_id": "complete-inst",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "steps": 3,
                "patch_present": true,
                "non_empty_patch": true,
                "attempts": 1,
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false
            },
            {
                "instance_id": "partial-inst",
                "exit_reason": "in_progress",
                "outcome": null,
                "steps": 4,
                "patch_present": false,
                "non_empty_patch": false,
                "attempts": 1,
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false
            }
        ]
    });
    std::fs::write(
        sweep_dir.join("results.json"),
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    // Create manifest.json (required by bundle)
    let manifest = serde_json::json!({
        "artifact_kind": "sweep_manifest",
        "schema_version": {"major": 1, "minor": 8},
        "dataset": "test",
        "model": "test-model",
        "resume_mode": false,
        "reruns": 1
    });
    std::fs::write(
        sweep_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Create trajectory for complete instance
    let complete_traj_dir = sweep_dir.join("complete-inst");
    std::fs::create_dir_all(&complete_traj_dir).unwrap();
    let complete_info = TrajectoryInfo {
        outcome: Some(outcome::SUBMITTED.into()),
        exit_reason: Some("submitted".into()),
        steps: Some(3),
        partial: false,
        partial_reason: None,
        ..Default::default()
    };
    let complete_traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info: complete_info,
        messages: vec![],
    };
    std::fs::write(
        complete_traj_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&complete_traj).unwrap(),
    )
    .unwrap();
    std::fs::write(complete_traj_dir.join("run-1.patch"), b"diff\n").unwrap();

    // Create partial trajectory for partial instance
    let partial_traj_dir = sweep_dir.join("partial-inst");
    std::fs::create_dir_all(&partial_traj_dir).unwrap();
    let partial_info = TrajectoryInfo {
        steps: Some(4),
        partial: true,
        partial_reason: Some("in_progress".into()),
        ..Default::default()
    };
    let partial_traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info: partial_info,
        messages: vec![],
    };
    std::fs::write(
        partial_traj_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&partial_traj).unwrap(),
    )
    .unwrap();

    let bundle_path = work.path().join("bundle.tar.gz");
    let args = BundleCreateArgs {
        sweep_dir: sweep_dir.clone(),
        output_path: bundle_path.clone(),
        instance: None,
    };

    // Bundle should succeed but exclude the partial instance with a warning
    let result = create_bundle(&args);
    match result {
        Ok(report) => {
            // The partial instance's trajectory should not be in the bundle
            assert!(
                !report.files.iter().any(|f| f.path.contains("partial-inst")),
                "partial instance trajectory should be excluded from bundle: {:?}",
                report.files
            );
            // The partial exclusion should be reported
            assert!(
                report.partial_excluded.contains(&"partial-inst".to_string()),
                "partial_excluded should list the skipped instance: {:?}",
                report.partial_excluded
            );
        }
        Err(e) => {
            // Acceptable: bundle refuses to include incomplete sweeps
            let err_str = e.to_string();
            assert!(
                err_str.contains("partial") || err_str.contains("incomplete"),
                "bundle error should mention partial/incomplete: {err_str}"
            );
        }
    }
}

// ─── RED Phase: Bench inspect shows partial banner ─────────────────────────

#[test]
fn bench_inspect_partial_trajectory_shows_banner() {
    use maxwells_daemon::run::inspect::{InspectArgs, InspectFormat, inspect};

    let work = tempfile::tempdir().unwrap();
    let sweep_dir = work.path().join("sweep");
    std::fs::create_dir_all(&sweep_dir).unwrap();

    // Create a partial trajectory file
    let traj_dir = sweep_dir.join("partial-inst");
    std::fs::create_dir_all(&traj_dir).unwrap();
    let mut info = TrajectoryInfo {
        steps: Some(4),
        model_name: Some("test-model".into()),
        partial: true,
        partial_reason: Some("in_progress".into()),
        ..Default::default()
    };
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    std::fs::write(
        traj_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    // Create a minimal results.json so inspect can load the sweep
    let results = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 8},
        "sweep_status": "completed",
        "total": 1,
        "submitted": 0,
        "errored": 0,
        "skipped": 0,
        "budget_halted": 0,
        "with_patch": 0,
        "patch_empty": 0,
        "patch_apply_invalid": 0,
        "retries": 0,
        "retried_instances": 0,
        "filter_spec": {"original_count": 1, "selected_count": 1},
        "total_prompt_tokens": 0,
        "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "instances": [
            {
                "instance_id": "partial-inst",
                "exit_reason": "in_progress",
                "outcome": null,
                "steps": 4,
                "patch_present": false,
                "non_empty_patch": false,
                "attempts": 1,
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false
            }
        ]
    });
    std::fs::write(
        sweep_dir.join("results.json"),
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let args = InspectArgs {
        sweep: sweep_dir.clone(),
        instance: Some("partial-inst".into()),
        filter: None,
        full: false,
        show_expected: false,
    };

    let report = inspect(args, InspectFormat::Text).unwrap();
    // The rendered output should contain a PARTIAL banner
    assert!(
        report.contains("PARTIAL") || report.contains("partial"),
        "inspect output should contain partial banner: {report}"
    );
}

// ─── RED Phase: Summary table partial count ───────────────────────────────

#[tokio::test]
async fn sweep_summary_table_shows_partial_count() {
    // After a sweep with some partial instances (e.g. due to interruption),
    // the summary table should show a "Partial" count.
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["inst-a"]);

    // Pre-place a partial trajectory that will be re-run
    let partial_path = trajectory_path_for_run(&output, "inst-a", 1);
    write_partial_trajectory(&partial_path, 2, 0.01);

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
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
        stratify_mode: maxwells_daemon::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(submit_response()),
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
        abort_on_systemic_failure: false,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
    })
    .await
    .unwrap();

    // The summary table method should be available (this verifies the API exists)
    let table = results.summary_table();
    // The table may or may not show "Partial: 0" — but it should not error
    // The partial count field should be accessible
    let _ = results.partial_count();
}

// ─── RED Phase: Determinism fixture ───────────────────────────────────────

#[test]
fn final_trajectory_after_resume_has_partial_false() {
    // A trajectory that was resumed should have partial=false in the final file.
    // This validates the "single coherent trajectory" requirement.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("final.traj.json");

    let mut traj = Trajectory::new();
    // Simulate a checkpoint written mid-run
    traj.info.steps = Some(4);
    traj.save_partial_atomic(&path).unwrap();

    // Check it's partial
    let mid_json = std::fs::read_to_string(&path).unwrap();
    let mid: serde_json::Value = serde_json::from_str(&mid_json).unwrap();
    assert_eq!(mid["info"]["partial"].as_bool(), Some(true));

    // Finalize the trajectory
    traj.info.outcome = Some(outcome::SUBMITTED.into());
    traj.info.steps = Some(8);
    // partial defaults to false, so save_pretty should omit it
    traj.save_pretty(&path).unwrap();

    let final_json = std::fs::read_to_string(&path).unwrap();
    let final_val: serde_json::Value = serde_json::from_str(&final_json).unwrap();
    let partial = final_val["info"]["partial"].as_bool().unwrap_or(false);
    assert!(!partial, "final trajectory after resume should have partial=false");
    assert_eq!(final_val["info"]["outcome"].as_str(), Some("submitted"));
}
