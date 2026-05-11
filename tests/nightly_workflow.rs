const NIGHTLY_WORKFLOW_PATH: &str = ".github/workflows/swe-bench-nightly.yml";

fn nightly_smoke_run_block(workflow: &str) -> Vec<String> {
    let workflow = workflow.replace("\r\n", "\n");
    let Some((_, after_step)) = workflow.split_once("      - name: Run smoke sweep") else {
        panic!("missing Run smoke sweep step in {NIGHTLY_WORKFLOW_PATH}");
    };
    let Some((_, after_run_marker)) = after_step.split_once("\n        run: |\n") else {
        panic!("Run smoke sweep step must contain a run: | block");
    };

    after_run_marker
        .lines()
        .take_while(|line| !line.starts_with("      - name: "))
        .filter_map(|line| line.strip_prefix("          "))
        .map(str::to_owned)
        .collect()
}

fn nightly_smoke_swebench_command_lines(run_block: &[String]) -> &[String] {
    let Some(start) = run_block
        .iter()
        .position(|line| line.contains(" bench swebench "))
    else {
        panic!("Run smoke sweep block must invoke bench swebench");
    };
    let Some(end) = run_block
        .iter()
        .position(|line| line.trim_start().starts_with("--skip-preflight"))
    else {
        panic!("nightly bench swebench command must include --skip-preflight");
    };

    &run_block[start..=end]
}

#[test]
fn nightly_smoke_swebench_multiline_command_keeps_all_flags_in_one_shell_command() {
    let workflow = std::fs::read_to_string(NIGHTLY_WORKFLOW_PATH)
        .unwrap_or_else(|err| panic!("failed to read {NIGHTLY_WORKFLOW_PATH}: {err}"));
    let run_block = nightly_smoke_run_block(&workflow);
    let command_lines = nightly_smoke_swebench_command_lines(&run_block);

    assert!(
        command_lines
            .iter()
            .any(|line| line.contains("--task-timeout-secs")),
        "nightly smoke command should preserve the whole-task timeout flag"
    );

    for line in &command_lines[..command_lines.len() - 1] {
        assert!(
            line.trim_end().ends_with('\\'),
            "nightly smoke swebench command line must continue with `\\`: `{line}`"
        );
    }

    let final_line = command_lines
        .last()
        .unwrap_or_else(|| panic!("nightly smoke swebench command must not be empty"));
    assert!(
        !final_line.trim_end().ends_with('\\'),
        "final nightly smoke swebench command line should terminate the command"
    );
}
