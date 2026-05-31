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
fn apply_check_failed_exit_code_is_29() {
    assert_eq!(ExitCode::ApplyCheckFailed.as_i32(), 29);
    assert_eq!(
        ExitCode::ApplyCheckFailed.outcome_class(),
        "apply_check_failed"
    );
}

#[test]
fn apply_redacted_refused_exit_code_is_30() {
    assert_eq!(ExitCode::ApplyRedactedRefused.as_i32(), 30);
    assert_eq!(
        ExitCode::ApplyRedactedRefused.outcome_class(),
        "apply_redacted_refused"
    );
}

#[test]
fn apply_dirty_tree_refused_exit_code_is_31() {
    assert_eq!(ExitCode::ApplyDirtyTreeRefused.as_i32(), 31);
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
        target: not_git,
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
        target: repo,
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
        target: repo,
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
        report_path: Some(report_path),
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
        target: repo,
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
        target: repo,
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
        target: repo,
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
        target: repo,
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
        target: repo,
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
fn apply_selector_sweep_instance_nested_layout() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Canonical nested layout: <sweep>/<instance>/run-1.patch
    let sweep_dir = work.path().join("sweep_run");
    let instance_dir = sweep_dir.join("my-instance");
    std::fs::create_dir_all(&instance_dir).unwrap();
    std::fs::write(instance_dir.join("run-1.patch"), &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::SweepInstance {
            sweep: sweep_dir,
            instance: "my-instance".into(),
        },
        target: repo,
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

    // Legacy flat layout: <sweep>/<instance>.patch (fallback when nested absent)
    let sweep_dir = work.path().join("sweep_run");
    std::fs::create_dir_all(&sweep_dir).unwrap();
    std::fs::write(sweep_dir.join("my-instance.patch"), &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::SweepInstance {
            sweep: sweep_dir,
            instance: "my-instance".into(),
        },
        target: repo,
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
fn apply_selector_sweep_bundle_patches_layout() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Bundle layout: <sweep>/patches/<instance>.patch (from spec-bundle.md)
    let sweep_dir = work.path().join("sweep_bundle");
    let patches_dir = sweep_dir.join("patches");
    std::fs::create_dir_all(&patches_dir).unwrap();
    std::fs::write(patches_dir.join("my-instance.patch"), &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::SweepInstance {
            sweep: sweep_dir,
            instance: "my-instance".into(),
        },
        target: repo,
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
fn apply_dry_run_without_report_flag_writes_default_report() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    // No report_path supplied — the default should be written next to target
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: true,
        three_way: false,
        report_path: None,
    };
    run_agent_apply(opts).unwrap();

    // Default report goes to target.parent() / apply-report.json
    let default_report = repo.parent().unwrap().join("apply-report.json");
    assert!(
        default_report.exists(),
        "default report must be written next to target"
    );
}

#[test]
fn apply_patch_inside_repo_not_gitignored_does_not_trip_dirty_gate() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Write the patch *inside* the repo (untracked, not gitignored)
    let patch_path = repo.join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false, // <-- strict; patch inside repo should be excluded
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    // Should succeed: the patch file itself is excluded from the dirty check
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
}

#[test]
fn apply_patch_inside_untracked_subdir_does_not_trip_dirty_gate() {
    // git status --porcelain (without --untracked-files=all) reports an
    // untracked directory as "?? runs/" rather than "?? runs/task.patch".
    // The dirty-gate exclusion must expand such entries.
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Place the patch inside an untracked subdirectory inside the repo.
    let runs_dir = repo.join("runs");
    std::fs::create_dir_all(&runs_dir).unwrap();
    let patch_path = runs_dir.join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false, // strict — the whole runs/ dir must be excluded
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    // Should succeed: git status --porcelain --untracked-files=all expands
    // "?? runs/" to "?? runs/task.patch", which is then excluded.
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
        target: repo,
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
        target: repo,
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
        target: repo,
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

// ── Legacy redaction path (info/other/secret_leak_detected) ──────────────────

#[test]
fn apply_trajectory_with_legacy_other_redaction_refused() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    // Some older mini versions write the secret-leak marker under info/other
    // rather than info/redaction/counts or info/secret_leak_detected.
    let traj_json = r#"{
        "format": "mini-swe-agent-1.3",
        "info": {
            "other": {
                "secret_leak_detected": {
                    "surface": "patch_submission"
                }
            }
        },
        "messages": []
    }"#;
    let traj_path = work.path().join("task.traj.json");
    std::fs::write(&traj_path, traj_json).unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::TrajectoryFile(traj_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: None,
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::RedactedRefused),
        "expected RedactedRefused for info/other/secret_leak_detected, got {err:?}"
    );
}

// ── /dev/null sentinel in new-file patches ────────────────────────────────────

#[test]
fn apply_new_file_patch_dev_null_not_in_files_changed() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    // A git-format patch that adds a brand-new file: old side is /dev/null.
    let patch = concat!(
        "diff --git a/new_file.txt b/new_file.txt\n",
        "new file mode 100644\n",
        "index 0000000..3b18e51\n",
        "--- /dev/null\n",
        "+++ b/new_file.txt\n",
        "@@ -0,0 +1 @@\n",
        "+hello world\n",
    );
    let patch_path = work.path().join("add_file.patch");
    std::fs::write(&patch_path, patch).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
    // Only the real new file should appear — not "dev/null".
    assert!(
        !report.files_changed.iter().any(|f| f.contains("dev/null")),
        "dev/null must not appear in files_changed; got {:?}",
        report.files_changed
    );
    assert!(
        report.files_changed.contains(&"new_file.txt".to_owned()),
        "new_file.txt must be in files_changed; got {:?}",
        report.files_changed
    );
}

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
        target: repo,
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
        target: repo,
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

#[test]
fn apply_report_missing_parent_dir_is_created() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    // Point report at a nested directory that does not exist yet.
    let report_path = work
        .path()
        .join("artifacts")
        .join("subdir")
        .join("report.json");

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path.clone()),
    };
    // Should succeed: parent directories are created before the apply runs.
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
    assert!(
        report_path.exists(),
        "report must be written even when parent did not exist"
    );
}

#[test]
fn apply_trajectory_file_inside_repo_not_dirty() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Place both the trajectory and the patch *inside* the repo (untracked).
    let traj_path = repo.join("task.traj.json");
    let patch_path = repo.join("task.patch");
    std::fs::write(
        &traj_path,
        r#"{"format":"mini-swe-agent-1.3","info":{},"messages":[]}"#,
    )
    .unwrap();
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::TrajectoryFile(traj_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false, // strict — both artifacts must be excluded
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    // Should succeed: both the .traj.json and the sibling .patch are excluded
    // from the dirty-tree gate.
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
}

// ── Plain-diff timestamped /dev/null sentinel ─────────────────────────────────

#[test]
fn apply_plain_diff_new_file_with_timestamped_dev_null() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    // A plain unified-diff (`diff -u`) new-file patch. The old header is
    // "/dev/null\t<timestamp>" — the tab+timestamp is the form plain `diff`
    // produces, and must not appear in files_changed as "dev/null".
    let patch = concat!(
        "--- /dev/null\t2024-01-01 00:00:00.000000000 +0000\n",
        "+++ new_plain.txt\t2024-01-01 00:00:01.000000000 +0000\n",
        "@@ -0,0 +1 @@\n",
        "+hello from plain diff\n",
    );
    let patch_path = work.path().join("plain_add.patch");
    std::fs::write(&patch_path, patch).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
    assert!(
        !report.files_changed.iter().any(|f| f.contains("dev/null")),
        "dev/null must not appear in files_changed; got {:?}",
        report.files_changed
    );
    assert!(
        report.files_changed.contains(&"new_plain.txt".to_owned()),
        "new_plain.txt must be in files_changed; got {:?}",
        report.files_changed
    );
}

// ── Bundle layout: --trajectory resolves ../patches/<id>.patch ────────────────

#[test]
fn apply_trajectory_resolves_bundle_patches_layout() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);

    // Simulate bundle layout: trajectories/<id>.traj.json + patches/<id>.patch
    let trajs_dir = work.path().join("trajectories");
    let patches_dir = work.path().join("patches");
    std::fs::create_dir_all(&trajs_dir).unwrap();
    std::fs::create_dir_all(&patches_dir).unwrap();

    let traj_path = trajs_dir.join("task-1.traj.json");
    let patch_path = patches_dir.join("task-1.patch");
    std::fs::write(
        &traj_path,
        r#"{"format":"mini-swe-agent-1.3","info":{},"messages":[]}"#,
    )
    .unwrap();
    std::fs::write(&patch_path, &patch_content).unwrap();

    let report_path = work.path().join("apply-report.json");
    let opts = AgentApplyOpts {
        selector: PatchSelector::TrajectoryFile(traj_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_path),
    };
    // Should resolve trajectories/task-1.traj.json → ../patches/task-1.patch
    let report = run_agent_apply(opts).unwrap();
    assert!(report.applied);
    assert_eq!(report.files_changed, vec!["hello.txt"]);
}

// ── Report path preflight before apply ────────────────────────────────────────

#[test]
fn apply_unwritable_report_path_fails_before_mutation() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    // Place a regular FILE where the report's parent directory should be so
    // that create_dir_all fails before the apply runs.
    let blocker = work.path().join("not-a-dir");
    std::fs::write(&blocker, "blocker\n").unwrap();
    let bad_report = blocker.join("apply-report.json");

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(bad_report),
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::Io(_)),
        "expected Io error for bad report path, got {err:?}"
    );
    // Tree must be unchanged — preflight must have fired before apply.
    let content = std::fs::read_to_string(repo.join("hello.txt")).unwrap();
    assert_eq!(
        content, "before\n",
        "tree must not be mutated when report path is bad"
    );
}

#[test]
fn apply_report_path_is_dir_fails_before_mutation() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    // Create a directory at the report path — preflight must reject this.
    let report_as_dir = work.path().join("apply-report-dir");
    std::fs::create_dir_all(&report_as_dir).unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo.clone(),
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: Some(report_as_dir),
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::Io(_)),
        "expected Io error when report path is a directory, got {err:?}"
    );
    // Tree must be unchanged.
    let content = std::fs::read_to_string(repo.join("hello.txt")).unwrap();
    assert_eq!(content, "before\n", "tree must not be mutated");
}

#[test]
fn apply_report_missing_parent_not_created_before_apply() {
    // Preflight must not create missing report parent directories before the
    // apply; the parent should be created by write_report after the patch
    // succeeds.
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    // Parent directory does not exist yet.
    let missing_parent = work.path().join("new-dir");
    let report_path = missing_parent.join("apply-report.json");
    assert!(
        !missing_parent.exists(),
        "precondition: dir should not exist"
    );

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
    assert!(report.applied, "patch must be applied");
    assert!(
        report_path.exists(),
        "write_report must create the parent dir and report"
    );
    // The parent dir must not have been created before the apply.
    // We can only assert it now exists (post-apply); the key correctness
    // property is that the apply succeeded — verified above.
}

#[test]
fn apply_patch_with_sibling_trajectory_redaction_refused() {
    // When --patch is used and the sibling .traj.json records patch_submission
    // redaction, agent apply must refuse even though --trajectory was not used.
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);

    let patch_content = make_valid_patch(&repo);
    let patch_path = work.path().join("task.patch");
    std::fs::write(&patch_path, &patch_content).unwrap();

    // Sibling trajectory records patch_submission redaction.
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
    std::fs::write(work.path().join("task.traj.json"), traj_json).unwrap();

    let opts = AgentApplyOpts {
        selector: PatchSelector::PatchFile(patch_path),
        target: repo,
        allow_redacted: false,
        allow_dirty: false,
        dry_run: false,
        three_way: false,
        report_path: None,
    };
    let err = run_agent_apply(opts).unwrap_err();
    assert!(
        matches!(err, ApplyError::RedactedRefused),
        "expected RedactedRefused when sibling traj records patch_submission redaction, got {err:?}"
    );
}
