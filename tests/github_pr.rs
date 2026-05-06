#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use rust_swe_agent::config::RedactionCfg;
use rust_swe_agent::run::github_pr::{GithubPrOptions, PublishMode, build_pr_plan, render_dry_run};

const SIMPLE_PATCH: &str = "diff --git a/src/lib.rs b/src/lib.rs\n\
index e69de29..8ab686e 100644\n\
--- a/src/lib.rs\n\
+++ b/src/lib.rs\n\
@@ -0,0 +1,2 @@\n\
+pub fn meaning() -> u32 {\n\
+    42\n\
+}\n";
const EXPECTED_HEAD_BRANCH: &str =
    "rust-swe-agent/sympy-sympy-12345-fee0ebc7d2afbf054f865bcd1d77abec";

fn options() -> GithubPrOptions {
    GithubPrOptions {
        target_repo: "madmax983/rust_swe_agent".into(),
        target_branch: "trunk".into(),
        task_id: "sympy__sympy-12345".into(),
        trajectory_ref: "runs/sympy__sympy-12345/run-1.traj.json".into(),
        patch_path: PathBuf::from("runs/sympy__sympy-12345/run-1.patch"),
        branch_prefix: "rust-swe-agent".into(),
        token_env: "GITHUB_TOKEN".into(),
        mode: PublishMode::DryRun,
        timeout_secs: 30,
        max_retries: 2,
        backoff_base_ms: 10,
        redaction: RedactionCfg::default(),
    }
}

#[test]
fn pr_plan_is_deterministic_and_mentions_trajectory() {
    let first = build_pr_plan(&options(), SIMPLE_PATCH).unwrap();
    let second = build_pr_plan(&options(), SIMPLE_PATCH).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.base_branch, "trunk");
    assert_eq!(first.head_branch, EXPECTED_HEAD_BRANCH);
    assert!(first.title.contains("sympy__sympy-12345"));
    assert!(
        first
            .body
            .contains("runs/sympy__sympy-12345/run-1.traj.json")
    );
    assert!(first.body.contains("src/lib.rs"));
    assert_eq!(first.summary.files_changed, 1);
    assert_eq!(first.summary.additions, 3);
    assert_eq!(first.summary.deletions, 0);
}

#[test]
fn pr_head_branch_preserves_task_id_uniqueness_when_slugs_collide() {
    let mut first_options = options();
    first_options.task_id = "repo_x-123".into();
    let mut second_options = options();
    second_options.task_id = "repo-x-123".into();

    let first = build_pr_plan(&first_options, SIMPLE_PATCH).unwrap();
    let second = build_pr_plan(&second_options, SIMPLE_PATCH).unwrap();
    let first_again = build_pr_plan(&first_options, SIMPLE_PATCH).unwrap();

    assert!(first.head_branch.starts_with("rust-swe-agent/repo-x-123"));
    assert!(second.head_branch.starts_with("rust-swe-agent/repo-x-123"));
    assert_ne!(first.head_branch, second.head_branch);
    assert_eq!(first.head_branch, first_again.head_branch);
}

#[test]
fn pr_plan_rejects_branch_prefix_that_slugs_to_empty() {
    let mut options = options();
    options.branch_prefix = "---___".into();

    let err = build_pr_plan(&options, SIMPLE_PATCH).unwrap_err();

    assert!(err.to_string().contains("branch prefix"), "{err}");
    assert!(err.to_string().contains("slug"), "{err}");
}

#[test]
fn dry_run_renders_pr_fields_without_token_material() {
    let plan = build_pr_plan(&options(), SIMPLE_PATCH).unwrap();
    let rendered = render_dry_run(&plan);

    for expected in [
        "target_repo: madmax983/rust_swe_agent",
        "base: trunk",
        "head: rust-swe-agent/sympy-sympy-12345-fee0ebc7d2afbf054f865bcd1d77abec",
        "title:",
        "body:",
        "patch_summary:",
        "files_changed: 1",
        "additions: 3",
        "deletions: 0",
    ] {
        assert!(
            rendered.contains(expected),
            "missing `{expected}` in dry-run output:\n{rendered}"
        );
    }
    assert!(!rendered.contains("GITHUB_TOKEN_VALUE"));
}

#[test]
fn pr_text_redaction_uses_configured_run_literals() {
    let configured_secret = "configured-pr-secret-value";
    let mut options = options();
    options.task_id = format!("task-{configured_secret}");
    options.trajectory_ref = format!("runs/{configured_secret}/run-1.traj.json");
    options.patch_path = PathBuf::from(format!("runs/{configured_secret}/run-1.patch"));
    options.redaction.secret_literals = vec![configured_secret.into()];

    let plan = build_pr_plan(&options, SIMPLE_PATCH).unwrap();
    let rendered = render_dry_run(&plan);

    assert!(!plan.head_branch.contains(configured_secret), "{plan:#?}");
    assert!(!plan.title.contains(configured_secret), "{plan:#?}");
    assert!(!plan.body.contains(configured_secret), "{plan:#?}");
    assert!(!rendered.contains(configured_secret), "{rendered}");
    assert!(
        rendered.contains("[REDACTED:configured_literal:"),
        "{rendered}"
    );
}
