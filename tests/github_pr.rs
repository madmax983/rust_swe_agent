#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use rust_swe_agent::run::github_pr::{GithubPrOptions, PublishMode, build_pr_plan, render_dry_run};

const SIMPLE_PATCH: &str = "diff --git a/src/lib.rs b/src/lib.rs\n\
index e69de29..8ab686e 100644\n\
--- a/src/lib.rs\n\
+++ b/src/lib.rs\n\
@@ -0,0 +1,2 @@\n\
+pub fn meaning() -> u32 {\n\
+    42\n\
+}\n";

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
    }
}

#[test]
fn pr_plan_is_deterministic_and_mentions_trajectory() {
    let first = build_pr_plan(&options(), SIMPLE_PATCH).unwrap();
    let second = build_pr_plan(&options(), SIMPLE_PATCH).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.base_branch, "trunk");
    assert_eq!(first.head_branch, "rust-swe-agent/sympy-sympy-12345");
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
fn dry_run_renders_pr_fields_without_token_material() {
    let plan = build_pr_plan(&options(), SIMPLE_PATCH).unwrap();
    let rendered = render_dry_run(&plan);

    for expected in [
        "target_repo: madmax983/rust_swe_agent",
        "base: trunk",
        "head: rust-swe-agent/sympy-sympy-12345",
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
