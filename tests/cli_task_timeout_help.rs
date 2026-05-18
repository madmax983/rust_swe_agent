#![allow(clippy::unwrap_used)]

use clap::CommandFactory as _;
use maxwells_daemon::cli::Cli;

#[test]
fn cli_uses_maxwell_branding() {
    let root = Cli::command();

    assert_eq!(root.get_name(), "max");
    assert!(
        root.get_about()
            .is_some_and(|about| about.to_string().contains("Maxwell's Daemon")),
        "root CLI about text should use Maxwell's Daemon branding"
    );
}

#[test]
fn mini_help_documents_task_timeout_seconds_and_step_limit_interaction() {
    let mut root = Cli::command();
    let mini = root.find_subcommand_mut("mini").unwrap();
    let help = mini.render_long_help().to_string();

    assert!(help.contains("--task-timeout-secs"));
    assert!(help.contains("seconds"));
    assert!(help.contains("unset"));
    assert!(help.contains("step-limit"));
    assert!(help.contains("whichever fires first"));
}

#[test]
fn swebench_help_documents_task_timeout_seconds_and_step_limit_interaction() {
    let mut root = Cli::command();
    let bench = root.find_subcommand_mut("bench").unwrap();
    let swebench = bench.find_subcommand_mut("swebench").unwrap();
    let help = swebench.render_long_help().to_string();

    assert!(help.contains("--task-timeout-secs"));
    assert!(help.contains("seconds"));
    assert!(help.contains("unset"));
    assert!(help.contains("step-limit"));
    assert!(help.contains("whichever fires first"));
}

#[test]
fn github_pr_flags_are_visible_for_mini_and_swebench() {
    let mut root = Cli::command();
    let mini = root.find_subcommand_mut("mini").unwrap();
    let mini_help = mini.render_long_help().to_string();
    for flag in [
        "--open-pr",
        "--target-repo",
        "--target-branch",
        "--github-token-env",
        "--github-pr-dry-run",
    ] {
        assert!(mini_help.contains(flag), "mini help missing {flag}");
    }

    let bench = root.find_subcommand_mut("bench").unwrap();
    let swebench = bench.find_subcommand_mut("swebench").unwrap();
    let swebench_help = swebench.render_long_help().to_string();
    for flag in [
        "--open-prs",
        "--target-repo",
        "--target-branch",
        "--github-token-env",
        "--github-pr-dry-run",
        "--github-pr-timeout-secs",
    ] {
        assert!(swebench_help.contains(flag), "swebench help missing {flag}");
    }
}
