//! `bench inspect`: CLI integration tests.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use rust_swe_agent::trajectory::{FailureCategory, Trajectory, outcome};

fn binary_path() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_rust-swe-agent").map_or_else(
        |_| {
            let mut p = std::env::current_exe().unwrap();
            p.pop();
            p.pop();
            p.push("rust-swe-agent");
            p
        },
        std::path::PathBuf::from,
    )
}

fn write_traj(dir: &Path, instance_id: &str, huge_stderr: bool) {
    let mut t = Trajectory::new();
    t.info.model_name = Some("deterministic-test".into());
    t.info.outcome = Some(outcome::ERROR.into());
    t.info.failure_category = Some(FailureCategory::StepLimit);
    t.info.total_cost_usd = Some(0.55);
    t.info.token_usage = Some(rust_swe_agent::trajectory::TokenUsage {
        prompt_tokens: 123,
        completion_tokens: 45,
    });

    let mut asst = rust_swe_agent::model::Message::assistant("```bash\necho hi\n```");
    asst.extra.actions = Some(vec!["echo hi".into()]);
    t.record_message(&asst);

    let mut obs = rust_swe_agent::model::Message::user("Exit code: 0\nOutput:\nhi");
    let stderr = if huge_stderr {
        (0..90)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    obs.extra.other.insert(
        "run_result".into(),
        serde_json::json!({
            "stdout": "hi\n",
            "stderr": stderr,
            "exit_code": 0,
            "timed_out": false
        }),
    );
    t.record_message(&obs);

    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();
}

#[test]
fn help_lists_inspect_subcommand_and_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("inspect"), "stdout:\n{stdout}");

    let out = Command::new(binary_path())
        .args(["bench", "inspect", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in ["--sweep", "--instance", "--filter", "--format", "--full"] {
        assert!(stdout.contains(flag), "missing {flag} in:\n{stdout}");
    }
}

#[test]
fn instance_mode_renders_header_and_steps() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "abc", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "abc",
                "resolved": false,
                "tests_passed": [],
                "tests_failed": [],
                "eval_exit_reason": "unresolved"
            }]
        })
        .to_string(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "abc",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("=== bench inspect ==="), "{stdout}");
    assert!(stdout.contains("instance_id:      abc"), "{stdout}");
    assert!(stdout.contains("[step 0] assistant"), "{stdout}");
    assert!(stdout.contains("[step 1] bash"), "{stdout}");
    assert!(stdout.contains("resolved:         false"), "{stdout}");
}

#[test]
fn truncates_long_output_unless_full() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "abc", true);

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "abc",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("full output at trajectory.json#/steps/1"),
        "{stdout}"
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "abc",
            "--full",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("full output at trajectory.json#/steps/1"),
        "{stdout}"
    );
    assert!(stdout.contains("line-89"), "{stdout}");
}

#[test]
fn filter_mode_lists_matching_instances() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "a", false);
    write_traj(sweep.path(), "b", false);
    let results = serde_json::json!({
        "total": 2,
        "submitted": 0,
        "skipped": 0,
        "errored": 2,
        "budget_halted": 0,
        "with_patch": 0,
        "total_prompt_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "instances": [
            {
                "instance_id": "a",
                "exit_reason": "error",
                "outcome": "error",
                "failure_category": "step_limit",
                "cost_usd": 0.1,
                "patch_present": false,
                "non_empty_patch": false
            },
            {
                "instance_id": "b",
                "exit_reason": "error",
                "outcome": "error",
                "failure_category": "model_api",
                "cost_usd": 0.2,
                "patch_present": false,
                "non_empty_patch": false
            }
        ]
    });
    std::fs::write(
        sweep.path().join("results.json"),
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--filter",
            "failure_category=step_limit",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("instance_id | outcome"), "{stdout}");
    assert!(stdout.contains("a | error | step_limit"), "{stdout}");
    assert!(!stdout.contains("b | error | model_api"), "{stdout}");
}
