//! README quickstart smoke-path verification.

#![allow(clippy::unwrap_used)]

use std::process::Command;

mod support;

fn marked_code_block(readme: &str, marker: &str, language: &str) -> String {
    let marker_text = format!("<!-- {marker} -->");
    let Some(after_marker) = readme.split_once(&marker_text).map(|(_, rest)| rest) else {
        panic!("README is missing marker {marker_text}");
    };
    let fence = format!("```{language}");
    let Some(after_fence) = after_marker.split_once(&fence).map(|(_, rest)| rest) else {
        panic!("README marker {marker_text} is missing a {language} code block");
    };
    let Some((block, _)) = after_fence.split_once("```") else {
        panic!("README marker {marker_text} has an unterminated code block");
    };
    normalize_newlines(block.trim())
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn command_after_cargo_run(command: &str) -> Vec<String> {
    let prefix = "cargo run --quiet -- ";
    let Some(rest) = command.strip_prefix(prefix) else {
        panic!("quickstart smoke command must start with `{prefix}`; got `{command}`");
    };
    assert!(
        !rest.contains(['"', '\'']),
        "quickstart smoke command parser only supports unquoted smoke-path args; got `{command}`"
    );
    rest.split_whitespace().map(str::to_owned).collect()
}

#[test]
fn readme_no_key_smoke_command_writes_documented_artifacts() {
    let readme = std::fs::read_to_string("README.md").unwrap();
    let command_lang = if cfg!(windows) { "powershell" } else { "bash" };
    let command = marked_code_block(&readme, "quickstart-smoke-command", command_lang);
    let expected_output = marked_code_block(&readme, "quickstart-smoke-output", "text");

    let documented_output_dir = "runs/quickstart";
    assert!(
        command.contains("--output runs/quickstart"),
        "smoke command must document --output {documented_output_dir}; got `{command}`"
    );
    assert!(
        expected_output.contains("trajectory: runs/quickstart/hello-world.traj.json"),
        "expected output must name the trajectory artifact:\n{expected_output}"
    );
    assert!(
        expected_output.contains("output: runs/quickstart/hello-world.output.txt"),
        "expected output must name the final-output artifact:\n{expected_output}"
    );

    let temp = tempfile::tempdir().unwrap();
    let output_dir = temp.path().join("runs").join("quickstart");
    let mut args = command_after_cargo_run(&command);
    let Some(output_flag) = args.iter().position(|arg| arg == "--output") else {
        panic!("quickstart smoke command must include --output");
    };
    args[output_flag + 1] = output_dir.display().to_string();

    let out = Command::new(support::binary_path())
        .args(&args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "smoke command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let trajectory_path = output_dir.join("hello-world.traj.json");
    let output_path = output_dir.join("hello-world.output.txt");
    let actual_stdout = String::from_utf8(out.stdout).unwrap();
    let expected_stdout = expected_output
        .replace(
            "runs/quickstart/hello-world.traj.json",
            &trajectory_path.display().to_string(),
        )
        .replace(
            "runs/quickstart/hello-world.output.txt",
            &output_path.display().to_string(),
        );
    assert_eq!(actual_stdout, format!("{expected_stdout}\n"));

    assert!(
        trajectory_path.exists(),
        "missing {}",
        trajectory_path.display()
    );
    assert!(output_path.exists(), "missing {}", output_path.display());

    let trajectory: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&trajectory_path).unwrap()).unwrap();
    assert_eq!(
        trajectory["trajectory_format"].as_str(),
        Some("mini-swe-agent-1.1")
    );
    assert_eq!(trajectory["info"]["outcome"].as_str(), Some("submitted"));
    assert_eq!(trajectory["info"]["total_cost_usd"].as_f64(), Some(0.0));
}

#[test]
fn readme_positions_project_as_measure_first_harness() {
    let readme = std::fs::read_to_string("README.md").unwrap();

    assert!(
        !readme
            .to_ascii_lowercase()
            .contains("port of mini-swe-agent"),
        "README should not lead with port framing"
    );
    assert!(
        readme.contains("minimal harness"),
        "README should describe the project as a minimal harness"
    );
    assert!(
        readme.contains("measure"),
        "README should make the measure-first philosophy visible"
    );
}
