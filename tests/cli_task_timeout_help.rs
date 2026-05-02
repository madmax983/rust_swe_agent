#![allow(clippy::unwrap_used)]

use clap::CommandFactory as _;
use rust_swe_agent::cli::Cli;

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
    assert!(help.contains("--skip-patch-validation"));
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
    assert!(help.contains("--skip-patch-validation"));
}
