//! Tests for `agent apply` — issue #473.
//!
//! RED phase: these tests reference the not-yet-implemented public API and
//! will fail to compile until the implementation is in place.
//! GREEN phase: implement src/run/apply.rs and wire the CLI.
//! REFACTOR phase: clean up.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::path::Path;
use std::process::Command;

use maxwells_daemon::exit_code::ExitCode;
use maxwells_daemon::run::apply::{
    AgentApplyOpts, ApplyError, ApplyReport, PatchSelector, run_agent_apply,
};

// ── Exit code unit tests ──────────────────────────────────────────────────────

#[test]
fn apply_check_failed_exit_code_is_28() {
    assert_eq!(ExitCode::ApplyCheckFailed.as_i32(), 28);
    assert_eq!(
        ExitCode::ApplyCheckFailed.outcome_class(),
        "apply_check_failed"
    );
}

#[test]
fn apply_redacted_refused_exit_code_is_29() {
    assert_eq!(ExitCode::ApplyRedactedRefused.as_i32(), 29);
    assert_eq!(
        ExitCode::ApplyRedactedRefused.outcome_class(),
        "apply_redacted_refused"
    );
}

#[test]
fn apply_dirty_tree_refused_exit_code_is_30() {
    assert_eq!(ExitCode::ApplyDirtyTreeRefused.as_i32(), 30);
    assert_eq!(
        ExitCode::ApplyDirtyTreeRefused.outcome_class(),
        "apply_dirty_tree_refused"
    );
}

// ── Struct-field existence tests ──────────────────────────────────────────────

#[test]
fn apply_report_has_required_fields() {
    use maxwells_daemon::artifact::{ArtifactKind, ArtifactSchemaVersion};

    let report = ApplyReport {
        schema_version: ArtifactSchemaVersion::CURRENT,
        artifact_kind: ArtifactKind::ApplyReport,
        source_patch_path: "/some/path/task.patch".into(),
        target_git_sha: Some("abc123".into()),
        files_changed: vec!["src/lib.rs".into()],
        lines_added: 5,
        lines_removed: 2,
        check_result: "passed".into(),
        applied: true,
        dry_run: false,
    };
    assert_eq!(report.lines_added, 5);
    assert_eq!(report.lines_removed, 2);
    assert!(report.applied);
    assert!(!report.dry_run);
    assert_eq!(report.check_result, "passed");
}

#[test]
fn artifact_kind_has_apply_report_variant() {
    use maxwells_daemon::artifact::ArtifactKind;
    assert_eq!(ArtifactKind::ApplyReport.label(), "apply_report");
}

// ── Helper: create a minimal git repo ─────────────────────────────────────────

fn init_repo(dir: &Path) -> String {
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
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
    std::fs::write(dir.join("hello.txt"), "before\n").unwrap();
    git(dir, &["add", "hello.txt"]);
    git(dir, &["commit", "-q", "-m", "base"]);
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Write a well-formed patch for `hello.txt`: "before" → "after".
fn make_valid_patch(repo: &Path) -> String {
    // Create a patch by actually diffing a modification
    std::fs::write(repo.join("hello.txt"), "after\n").unwrap();
    let out = Command::new("git")
        .args(["diff"])
        .current_dir(repo)
        .output()
        .unwrap();
    let patch = String::from_utf8(out.stdout).unwrap();
    // Restore the working tree
    Command::new("git")
        .args(["checkout", "hello.txt"])
        .current_dir(repo)
        .output()
        .unwrap();
    patch
}

// ── Logic tests ───────────────────────────────────────────────────────────────

#[test]
fn apply_to_non_git_target_returns_not_git_tree_error() {
    let work = tempfile::tempdir().unwrap();
    let not_git = work.path().join("notgit");
    std::fs::create_dir_all(&not_git).unwrap();

    // Create a patch file (content doesn't matter for this test)
    let patch_path = work.path().join("task.patch");
    std::fs::write(
        &patch_path,
        "--- a/foo\n+++ b/foo\n@@ -1 +1 @@\n-old\n+new\n",
    )
    .unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: not_git.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: None,
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::NotGitTree(_)),
        "expected NotGitTree, got {err:?}"
    );
}

#[test]
fn apply_empty_patch_exits_0_with_no_changes() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_path = work.path().join("empty.patch");
    std::fs::write(&patch_path, "").unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path.clone()),
    };
    let report = run_agent_apply(opts).unwrap();
    assert_eq!(report.check_result, "empty");
    assert!(!report.applied);
    assert!(report.files_changed.is_empty());
    // Report file must be written
    assert!(report_path.exists());
}

#[test]
fn apply_dirty_tree_refused_by_default() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    // Make tree dirty
    std::fs::write(repo.join("dirty.txt"), "untracked\n").unwrap();

    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, make_valid_patch(&repo)).unwrap();

    // Re-dirty (make_valid_patch cleans the tracked file, but we added untracked)
    std::fs::write(repo.join("dirty.txt"), "untracked\n").unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: None,
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::DirtyTree(_)),
        "expected DirtyTree, got {err:?}"
    );
}

#[test]
fn apply_dirty_tree_allowed_with_allow_dirty() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Make tree dirty (untracked file)
    std::fs::write(repo.join("dirty.txt"), "untracked\n").unwrap();

    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: true,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path.clone()),
    };
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
    // File was changed
    let content = std::fs::read_to_string(repo.join("hello.txt")).unwrap();
    assert_eq!(content, "after\n");
}

#[test]
fn apply_check_failed_returns_check_failed_error() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    // A patch for a file that doesn't exist / has wrong context
    let bad_patch = concat!(
        "diff --git a/nonexistent.rs b/nonexistent.rs\n",
        "--- a/nonexistent.rs\n",
        "+++ b/nonexistent.rs\n",
        "@@ -1,3 +1,3 @@\n",
        " line1\n",
        "-line2\n",
        "+line2_modified\n",
        " line3\n",
    );
    let patch_path = work.path().join("bad.patch");
    std::fs::write(&patch_path, bad_patch).unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: None,
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::CheckFailed(_)),
        "expected CheckFailed, got {err:?}"
    );
}

#[test]
fn apply_check_failed_leaves_tree_unchanged() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let original_content = std::fs::read_to_string(repo.join("hello.txt")).unwrap();

    let bad_patch = concat!(
        "diff --git a/nonexistent.rs b/nonexistent.rs\n",
        "--- a/nonexistent.rs\n",
        "+++ b/nonexistent.rs\n",
        "@@ -1,3 +1,3 @@\n",
        " line1\n",
        "-line2\n",
        "+line2_modified\n",
        " line3\n",
    );
    let patch_path = work.path().join("bad.patch");
    std::fs::write(&patch_path, bad_patch).unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: None,
    };
    let _err = run_agent_apply(opts).unwrap_err();

    // Tree must be byte-for-byte unchanged
    let after_content = std::fs::read_to_string(repo.join("hello.txt")).unwrap();
    assert_eq!(original_content, after_content, "tree must be unchanged");
}

#[test]
fn apply_dry_run_does_not_modify_tree() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: true,
        three_way: false,
        report_path: Some(report_path.clone()),
    };
    let report = run_agent_apply(opts).unwrap();

    // Not applied in dry-run
    assert!(!report.applied);
    assert!(report.dry_run);
    assert_eq!(report.check_result, "passed");

    // Tree unchanged
    let content = std::fs::read_to_string(repo.join("hello.txt")).unwrap();
    assert_eq!(content, "before\n");

    // Report still written
    assert!(report_path.exists());
}

#[test]
fn apply_success_modifies_tree_and_writes_report() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path.clone()),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path.clone()),
    };
    let report = run_agent_apply(opts).unwrap();

    assert!(report.applied);
    assert!(!report.dry_run);
    assert_eq!(report.check_result, "passed");
    assert!(report.target_git_sha.is_some());
    assert!(!report.files_changed.is_empty());
    assert_eq!(report.source_patch_path, patch_path.to_string_lossy());

    // Tree was modified
    let content = std::fs::read_to_string(repo.join("hello.txt")).unwrap();
    assert_eq!(content, "after\n");

    // Report file exists and is valid JSON
    assert!(report_path.exists());
    let report_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(report_json["applied"], serde_json::json!(true));
    assert_eq!(report_json["check_result"], serde_json::json!("passed"));
}

#[test]
fn apply_success_report_contains_schema_version() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path.clone()),
    };
    run_agent_apply(opts).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    assert!(
        json.get("schema_version").is_some(),
        "report must contain schema_version"
    );
    assert!(
        json.get("artifact_kind").is_some(),
        "report must contain artifact_kind"
    );
}

#[test]
fn apply_report_written_to_custom_path() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let custom_report = work.path().join("subdir").join("my-report.json");
    std::fs::create_dir_all(custom_report.parent().unwrap()).unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(custom_report.clone()),
    };
    run_agent_apply(opts).unwrap();
    assert!(custom_report.exists());
}

#[test]
fn apply_report_lines_added_removed_are_correct() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    let report = run_agent_apply(opts).unwrap();
    // "before" → "after": 1 line removed, 1 line added
    assert_eq!(report.lines_added, 1);
    assert_eq!(report.lines_removed, 1);
}

#[test]
fn apply_selector_from_trajectory_resolves_sibling_patch() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Write both sibling files: task.traj.json and task.patch
    let traj_path = work.path().join("task.traj.json");
    let patch_path = work.path().join("task.patch");
    std::fs::write(
        &traj_path,
        r#"{"format":"mini-swe-agent-1.3","info":{},"messages":[]}"#,
    )
    .unwrap();
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::TrajectoryFile(traj_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
}

#[test]
fn apply_selector_from_sweep_instance_resolves_patch() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Sweep directory structure: <sweep>/<instance>.patch
    let sweep_dir = work.path().join("sweep_run");
    std::fs::create_dir_all(&sweep_dir).unwrap();
    std::fs::write(sweep_dir.join("my-instance.patch"), &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::SweepInstance {
            sweep: sweep_dir,
            instance: "my-instance".into(),
        },
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
}

#[test]
fn apply_redacted_patch_content_refused_by_default() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    // A patch that contains a [REDACTED:...] marker in a line addition
    let redacted_patch = concat!(
        "diff --git a/hello.txt b/hello.txt\n",
        "--- a/hello.txt\n",
        "+++ b/hello.txt\n",
        "@@ -1 +1 @@\n",
        "-before\n",
        "+[REDACTED:sha256:abcdef1234567890] was here\n",
    );
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, redacted_patch).unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: None,
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::RedactedRefused),
        "expected RedactedRefused, got {err:?}"
    );
}

#[test]
fn apply_redacted_patch_content_allowed_with_flag() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    // A patch with a [REDACTED:...] marker — but now we use --allow-redacted
    // We also make the patch apply cleanly (context matches)
    let redacted_patch = concat!(
        "diff --git a/hello.txt b/hello.txt\n",
        "--- a/hello.txt\n",
        "+++ b/hello.txt\n",
        "@@ -1 +1 @@\n",
        "-before\n",
        "+[REDACTED:sha256:abcdef1234567890] was here\n",
    );
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, redacted_patch).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: true,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
}

#[test]
fn apply_3way_flag_is_accepted() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: true,
        report_path: Some(report_path),
    };
    // Should succeed with --3way
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
}

// ── Trajectory-based redaction detection ─────────────────────────────────────

#[test]
fn apply_trajectory_with_patch_submission_redaction_refused() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    // Write a trajectory that records patch_submission redaction
    let traj_json = r#"{
        "format": "mini-swe-agent-1.3",
        "info": {
            "redaction": {
                "enabled": true,
                "redacted": true,
                "counts": [
                    {"surface": "patch_submission", "kind": "configured_literal", "count": 1}
                ]
            }
        },
        "messages": []
    }"#;
    let traj_path = work.path().join("task.traj.json");
    std::fs::write(&traj_path, traj_json).unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::TrajectoryFile(traj_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: None,
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::RedactedRefused),
        "expected RedactedRefused when trajectory records patch_submission redaction, got {err:?}"
    );
}

#[test]
fn apply_trajectory_with_patch_submission_redaction_allowed_with_flag() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let traj_json = r#"{
        "format": "mini-swe-agent-1.3",
        "info": {
            "redaction": {
                "enabled": true,
                "redacted": true,
                "counts": [
                    {"surface": "patch_submission", "kind": "configured_literal", "count": 1}
                ]
            }
        },
        "messages": []
    }"#;
    let traj_path = work.path().join("task.traj.json");
    std::fs::write(&traj_path, traj_json).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::TrajectoryFile(traj_path),
        target: repo.clone(),
        allow_redacted: true,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    // With --allow-redacted, should succeed despite trajectory redaction record
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
}
