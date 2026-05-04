//! End-to-end sweep produces SWE-bench submission artifacts:
//!   * per-instance `<id>.patch` files (unified diff vs. base_commit)
//!   * aggregated `all_preds.jsonl` whose lines parse as the predictions
//!     schema sb-cli expects
//!
//! Runs against a deterministic model + a temp git repo (no network, no
//! Docker) so the harness invariants — not patch *quality* — are what
//! gets exercised.

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::Config;
use rust_swe_agent::run::swebench::{
    SwebenchArgs, patch_path_for_run, run, trajectory_path_for_run,
};
use rust_swe_agent::trajectory::{Trajectory, outcome};

/// Initialize a git repo at `dir` with one tracked file at the base
/// commit. The file's existence is what makes the working tree's
/// post-modification state diff-able.
fn init_repo_with_file(dir: &Path, filename: &str, contents: &str) -> String {
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
    // Some CI environments configure a global commit signing hook that
    // can't reach its signing server from a sandbox; force it off.
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
    std::fs::write(dir.join(filename), contents).unwrap();
    git(dir, &["add", filename]);
    git(dir, &["commit", "-q", "-m", "base"]);

    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn write_dataset(path: &Path, instance_ids: &[&str], base_commit: &str) {
    let mut s = String::new();
    for id in instance_ids {
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\",\"base_commit\":\"{base_commit}\"}}"
        );
    }
    std::fs::write(path, s).unwrap();
}

fn toml_escape_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[tokio::test]
async fn sweep_emits_patch_artifact_for_modifying_agent() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let base_commit = init_repo_with_file(&repo, "hello.txt", "before\n");

    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["mod-instance"], &base_commit);

    // Agent modifies hello.txt with an absolute path — `LocalEnvironment`
    // runs commands in the test process's CWD, not `repo`, so we anchor
    // the path explicitly. Patch capture then runs `git diff` from
    // `repo` (the configured workdir) and sees the change.
    let mod_cmd = format!("echo after > {}/hello.txt", repo.display());
    let responses = vec![
        format!("```bash\n{mod_cmd}\n```"),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nmodified\n```".into(),
    ];

    let toml = format!(
        "[environment]\nworkdir = \"{}\"\n\n[model]\nname = \"scripted-test-model\"\n",
        toml_escape_path(&repo)
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
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
        github_pr: None,
    })
    .await
    .unwrap();

    assert_eq!(results.total, 1);
    assert_eq!(results.submitted, 1);
    assert_eq!(results.with_patch, 1);

    let mod_patch = patch_path_for_run(&output, "mod-instance", 1);
    assert!(mod_patch.exists());
    let patch_text = std::fs::read_to_string(&mod_patch).unwrap();
    assert!(!patch_text.is_empty(), "expected non-empty diff");
    assert!(
        patch_text.contains("hello.txt"),
        "diff should mention the modified file: {patch_text}"
    );
    assert!(
        patch_text.contains("-before") && patch_text.contains("+after"),
        "diff should show old and new content: {patch_text}"
    );

    // Predictions schema: one line, valid JSON, required fields present.
    let preds_text = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    let lines: Vec<&str> = preds_text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected one prediction line: {preds_text}");

    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(
        obj.get("instance_id").and_then(|v| v.as_str()),
        Some("mod-instance")
    );
    assert_eq!(
        obj.get("model_name_or_path").and_then(|v| v.as_str()),
        Some("scripted-test-model")
    );
    // `model_patch` round-trips byte-for-byte from `<id>.patch`.
    assert_eq!(
        obj.get("model_patch").and_then(|v| v.as_str()).unwrap(),
        patch_text
    );

    // Spec invariant: submitted count == predictions line count.
    assert_eq!(results.submitted, lines.len());
}

#[tokio::test]
async fn sweep_emits_empty_patch_when_agent_changes_nothing() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let base_commit = init_repo_with_file(&repo, "hello.txt", "untouched\n");

    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["noop-instance"], &base_commit);

    let toml = format!(
        "[environment]\nworkdir = \"{}\"\n\n[model]\nname = \"scripted-test-model\"\n",
        toml_escape_path(&repo)
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
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
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nnoop\n```".into(),
        ]),
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
        github_pr: None,
    })
    .await
    .unwrap();

    assert_eq!(results.submitted, 1);
    // Empty diff still counts as submitted but not as `with_patch`.
    assert_eq!(results.with_patch, 0);

    let patch_path = patch_path_for_run(&output, "noop-instance", 1);
    assert!(patch_path.exists(), "empty patch must still be written");
    assert!(
        std::fs::read_to_string(&patch_path).unwrap().is_empty(),
        "diff should be empty"
    );

    // Predictions: empty agent still gets a row with `model_patch: ""`,
    // so sb-cli reports it as unresolved rather than missing.
    let preds_text = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    let lines: Vec<&str> = preds_text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v.get("model_patch").and_then(|v| v.as_str()), Some(""));

    let traj: Trajectory = serde_json::from_str(
        &std::fs::read_to_string(trajectory_path_for_run(&output, "noop-instance", 1)).unwrap(),
    )
    .unwrap();
    assert_eq!(traj.info.outcome.as_deref(), Some(outcome::SUBMITTED));
}

#[tokio::test]
async fn missing_workdir_marks_outcome_as_error() {
    // Patch capture must not crash the sweep when the working tree is
    // unreachable (e.g. dataset typo, container teardown). The instance
    // is downgraded to outcome=error and excluded from all_preds.jsonl.
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["broken"], "deadbeef");

    // Workdir points at a path that doesn't exist; `git diff` will fail.
    let toml = format!(
        "[environment]\nworkdir = \"{}\"\n",
        toml_escape_path(&work.path().join("does-not-exist"))
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
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
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
        ]),
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
        github_pr: None,
    })
    .await
    .unwrap();

    // Sweep finished cleanly even though one instance's patch capture
    // failed.
    assert_eq!(results.total, 1);
    assert_eq!(results.errored, 1);
    assert_eq!(results.submitted, 0);

    // Trajectory records the patch_error in `info.other` and outcome=error.
    let traj_path = trajectory_path_for_run(&output, "broken", 1);
    let traj: Trajectory =
        serde_json::from_str(&std::fs::read_to_string(&traj_path).unwrap()).unwrap();
    assert_eq!(traj.info.outcome.as_deref(), Some(outcome::ERROR));
    assert!(
        traj.info.other.contains_key("patch_error"),
        "expected patch_error key in info.other: {:?}",
        traj.info.other
    );

    // No `.patch` file written for the failed capture.
    assert!(!patch_path_for_run(&output, "broken", 1).exists());

    // all_preds.jsonl exists but contains no lines (no submitted instances).
    let preds = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    assert!(preds.is_empty(), "expected empty predictions, got: {preds}");
}
