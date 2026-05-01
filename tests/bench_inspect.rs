//! `bench inspect`: CLI integration tests.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use rust_swe_agent::trajectory::{FailureCategory, TokenUsage, Trajectory, outcome};

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
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
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

fn write_traj_with_stderr(dir: &Path, instance_id: &str, stderr: &str) {
    let mut t = Trajectory::new();
    t.info.model_name = Some("deterministic-test".into());
    t.info.outcome = Some(outcome::ERROR.into());
    t.info.failure_category = Some(FailureCategory::StepLimit);

    let mut asst = rust_swe_agent::model::Message::assistant("```bash\necho hi\n```");
    asst.extra.actions = Some(vec!["echo hi".into()]);
    t.record_message(&asst);

    let mut obs = rust_swe_agent::model::Message::user("Exit code: 0\nOutput:\nhi");
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

fn write_diff_traj(
    path: &Path,
    instance_id: &str,
    failure_category: Option<FailureCategory>,
    cost_usd: f64,
    steps: &[(&str, &str, &str, &str, i32)],
) {
    let mut t = Trajectory::new();
    t.info
        .other
        .insert("instance_id".into(), serde_json::json!(instance_id));
    t.info.outcome = Some(if failure_category.is_some() {
        outcome::ERROR.into()
    } else {
        outcome::SUBMITTED.into()
    });
    t.info.failure_category = failure_category;
    t.info.total_cost_usd = Some(cost_usd);
    t.info.token_usage = Some(TokenUsage {
        prompt_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 20,
    });
    t.info.steps = Some(u32::try_from(steps.len()).unwrap_or(u32::MAX));

    for (assistant, command, stdout, stderr, exit_code) in steps {
        let mut asst = rust_swe_agent::model::Message::assistant(*assistant);
        asst.extra.actions = Some(vec![(*command).into()]);
        t.record_message(&asst);

        let mut obs = rust_swe_agent::model::Message::user("tool result");
        obs.extra.other.insert(
            "run_result".into(),
            serde_json::json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "timed_out": false
            }),
        );
        t.record_message(&obs);
    }

    std::fs::write(path, serde_json::to_string_pretty(&t).unwrap()).unwrap();
}

fn write_prompted_diff_traj(
    path: &Path,
    instance_id: &str,
    system_prompt: &str,
    user_prompt: &str,
    assistant: &str,
    command: &str,
    stdout: &str,
) {
    let mut t = Trajectory::new();
    t.info
        .other
        .insert("instance_id".into(), serde_json::json!(instance_id));
    t.info.outcome = Some(outcome::SUBMITTED.into());
    t.info.total_cost_usd = Some(0.10);
    t.info.token_usage = Some(TokenUsage {
        prompt_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 20,
    });
    t.info.steps = Some(1);

    t.record_message(&rust_swe_agent::model::Message::system(system_prompt));
    t.record_message(&rust_swe_agent::model::Message::user(user_prompt));

    let mut asst = rust_swe_agent::model::Message::assistant(assistant);
    asst.extra.actions = Some(vec![command.into()]);
    t.record_message(&asst);

    let mut obs = rust_swe_agent::model::Message::user("tool result");
    obs.extra.other.insert(
        "run_result".into(),
        serde_json::json!({
            "stdout": stdout,
            "stderr": "",
            "exit_code": 0,
            "timed_out": false
        }),
    );
    t.record_message(&obs);

    std::fs::write(path, serde_json::to_string_pretty(&t).unwrap()).unwrap();
}

fn write_orphan_tool_alignment_traj(path: &Path, instance_id: &str, include_orphan_tool: bool) {
    let mut t = Trajectory::new();
    t.info
        .other
        .insert("instance_id".into(), serde_json::json!(instance_id));
    t.info.outcome = Some(outcome::SUBMITTED.into());
    t.info.total_cost_usd = Some(0.10);
    t.info.token_usage = Some(TokenUsage {
        prompt_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 20,
    });
    t.info.steps = Some(2);

    record_assistant_tool_step(&mut t, "```bash\necho one\n```", "echo one", "one\n");
    if include_orphan_tool {
        record_tool_result(&mut t, "orphan\n");
    }
    record_assistant_tool_step(&mut t, "```bash\necho two\n```", "echo two", "two\n");

    std::fs::write(path, serde_json::to_string_pretty(&t).unwrap()).unwrap();
}

fn write_trailing_prompt_traj(path: &Path, instance_id: &str, include_trailing_prompt: bool) {
    let mut t = Trajectory::new();
    t.info
        .other
        .insert("instance_id".into(), serde_json::json!(instance_id));
    t.info.outcome = Some(outcome::SUBMITTED.into());
    t.info.total_cost_usd = Some(0.10);
    t.info.token_usage = Some(TokenUsage {
        prompt_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 20,
    });
    t.info.steps = Some(2);

    record_assistant_tool_step(&mut t, "```bash\necho one\n```", "echo one", "one\n");
    record_assistant_tool_step(&mut t, "```bash\necho two\n```", "echo two", "two\n");
    if include_trailing_prompt {
        t.record_message(&rust_swe_agent::model::Message::user(
            "continue with next check",
        ));
    }

    std::fs::write(path, serde_json::to_string_pretty(&t).unwrap()).unwrap();
}

fn record_assistant_tool_step(
    trajectory: &mut Trajectory,
    assistant: &str,
    command: &str,
    stdout: &str,
) {
    let mut asst = rust_swe_agent::model::Message::assistant(assistant);
    asst.extra.actions = Some(vec![command.into()]);
    trajectory.record_message(&asst);
    record_tool_result(trajectory, stdout);
}

fn record_tool_result(trajectory: &mut Trajectory, stdout: &str) {
    let mut obs = rust_swe_agent::model::Message::user("tool result");
    obs.extra.other.insert(
        "run_result".into(),
        serde_json::json!({
            "stdout": stdout,
            "stderr": "",
            "exit_code": 0,
            "timed_out": false
        }),
    );
    trajectory.record_message(&obs);
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
    for flag in [
        "--sweep",
        "--instance",
        "--filter",
        "--format",
        "--full",
        "--diff",
        "--show-noise",
    ] {
        assert!(stdout.contains(flag), "missing {flag} in:\n{stdout}");
    }
}

#[test]
fn diff_identical_trajectories_collapse_to_all_match_output() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[("```bash\necho hi\n```", "echo hi", "hi\n", "", 0)],
    );
    write_diff_traj(
        &candidate,
        "abc",
        None,
        0.10,
        &[("```bash\necho hi\n```", "echo hi", "hi\n", "", 0)],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("=== bench inspect diff ==="), "{stdout}");
    assert!(stdout.contains("instance_id: abc"), "{stdout}");
    assert!(stdout.contains("[step 0 - identical]"), "{stdout}");
    assert!(!stdout.contains("assistant.content"), "{stdout}");
}

#[test]
fn diff_single_step_divergence_expands_only_that_step() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[
            ("```bash\necho same\n```", "echo same", "same\n", "", 0),
            ("```bash\npytest -q\n```", "pytest -q", "1 failed\n", "", 1),
        ],
    );
    write_diff_traj(
        &candidate,
        "abc",
        Some(FailureCategory::StepLimit),
        0.20,
        &[
            ("```bash\necho same\n```", "echo same", "same\n", "", 0),
            (
                "```bash\npytest tests\n```",
                "pytest tests",
                "2 failed\n",
                "",
                1,
            ),
        ],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("first_divergent_step: 1"), "{stdout}");
    assert!(stdout.contains("[step 0 - identical]"), "{stdout}");
    assert!(stdout.contains("[step 1 - diverge]"), "{stdout}");
    assert!(stdout.contains("assistant.content"), "{stdout}");
    assert!(stdout.contains("bash.command"), "{stdout}");
    assert!(stdout.contains("tool.stdout"), "{stdout}");
}

#[test]
fn diff_length_mismatch_renders_unmatched_tail() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[("```bash\necho one\n```", "echo one", "one\n", "", 0)],
    );
    write_diff_traj(
        &candidate,
        "abc",
        None,
        0.12,
        &[
            ("```bash\necho one\n```", "echo one", "one\n", "", 0),
            ("```bash\necho two\n```", "echo two", "two\n", "", 0),
        ],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["steps"][0]["status"], "match");
    assert_eq!(v["steps"][1]["status"], "candidate_only");
    assert_eq!(v["steps"][1]["candidate"]["bash"], "echo two");
}

#[test]
fn diff_length_mismatch_renders_baseline_only_tail() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.12,
        &[
            ("```bash\necho one\n```", "echo one", "one\n", "", 0),
            ("```bash\necho old\n```", "echo old", "old\n", "", 0),
        ],
    );
    write_diff_traj(
        &candidate,
        "abc",
        None,
        0.10,
        &[("```bash\necho one\n```", "echo one", "one\n", "", 0)],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["steps"][1]["index"], 1);
    assert_eq!(v["steps"][1]["role"], "assistant");
    assert_eq!(v["steps"][1]["status"], "baseline_only");
    assert_eq!(v["steps"][1]["baseline"]["bash"], "echo old");
    assert!(v["steps"][1]["candidate"].is_null());
}

#[test]
fn diff_groups_initial_prompt_assistant_and_tool_result_as_one_role_keyed_step() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_prompted_diff_traj(
        &baseline,
        "abc",
        "You are a careful agent.",
        "Fix issue #29.",
        "```bash\npytest -q\n```",
        "pytest -q",
        "1 failed\n",
    );
    write_prompted_diff_traj(
        &candidate,
        "abc",
        "You are a careful agent.",
        "Fix issue #29.",
        "```bash\npytest -q\n```",
        "pytest -q",
        "1 failed\n",
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(
        steps.len(),
        1,
        "system+user prompt, assistant, and tool result should form one semantic step: {v:#}"
    );
    assert_eq!(steps[0]["index"], 0);
    assert_eq!(steps[0]["role"], "assistant");
    assert_eq!(steps[0]["status"], "match");
    assert_eq!(
        steps[0]["baseline"]["prompt"],
        "system: You are a careful agent.\nuser: Fix issue #29."
    );
    assert_eq!(steps[0]["baseline"]["bash"], "pytest -q");
    assert_eq!(steps[0]["baseline"]["stdout"], "1 failed\n");
}

#[test]
fn diff_orphan_tool_record_does_not_shift_later_assistant_indices() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_orphan_tool_alignment_traj(&baseline, "abc", true);
    write_orphan_tool_alignment_traj(&candidate, "abc", false);

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["header"]["first_divergent_step_index"], 0, "{v:#}");
    assert_eq!(v["header"]["first_divergent_step_role"], "tool", "{v:#}");
    let steps = v["steps"].as_array().unwrap();
    let assistant_one = steps
        .iter()
        .find(|step| step["role"] == "assistant" && step["index"] == 1)
        .unwrap_or_else(|| panic!("missing assistant index 1 in {v:#}"));
    assert_eq!(assistant_one["status"], "match", "{v:#}");
    assert_eq!(assistant_one["baseline"]["bash"], "echo two");
    assert_eq!(assistant_one["candidate"]["bash"], "echo two");
    assert!(
        steps.iter().any(|step| {
            step["role"] == "tool"
                && step["status"] == "baseline_only"
                && step["baseline"]["stdout"] == "orphan\n"
        }),
        "orphan tool should be isolated as a tool-only diff: {v:#}"
    );
    assert!(
        !steps.iter().any(|step| {
            step["role"] == "assistant"
                && (step["status"] == "baseline_only" || step["status"] == "candidate_only")
        }),
        "orphan tool must not turn matching assistant turns into tails: {v:#}"
    );
}

#[test]
fn diff_trailing_prompt_tail_uses_next_logical_step_index() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_trailing_prompt_traj(&baseline, "abc", false);
    write_trailing_prompt_traj(&candidate, "abc", true);

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["header"]["first_divergent_step_index"], 2, "{v:#}");
    assert_eq!(v["header"]["first_divergent_step_role"], "prompt", "{v:#}");
    let prompt_tail = v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["role"] == "prompt")
        .unwrap_or_else(|| panic!("missing prompt tail in {v:#}"));
    assert_eq!(prompt_tail["index"], 2);
    assert_eq!(prompt_tail["status"], "candidate_only");
    assert_eq!(
        prompt_tail["candidate"]["prompt"],
        "user: continue with next check"
    );
}

#[test]
fn diff_instance_id_mismatch_fails_fast() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[("```bash\necho hi\n```", "echo hi", "hi\n", "", 0)],
    );
    write_diff_traj(
        &candidate,
        "xyz",
        None,
        0.10,
        &[("```bash\necho hi\n```", "echo hi", "hi\n", "", 0)],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("instance_id mismatch"), "{stderr}");
    assert!(stderr.contains("abc"), "{stderr}");
    assert!(stderr.contains("xyz"), "{stderr}");
}

#[test]
fn diff_json_output_schema_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[("```bash\npytest -q\n```", "pytest -q", "1 failed\n", "", 1)],
    );
    write_diff_traj(
        &candidate,
        "abc",
        Some(FailureCategory::StepLimit),
        0.20,
        &[(
            "```bash\npytest tests\n```",
            "pytest tests",
            "2 failed\n",
            "",
            1,
        )],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["instance_id"], "abc");
    assert_eq!(v["header"]["baseline_failure_category"], "none");
    assert_eq!(v["header"]["candidate_failure_category"], "step_limit");
    assert_eq!(v["header"]["baseline_total_steps"], 1);
    assert_eq!(v["header"]["candidate_total_steps"], 1);
    assert_eq!(v["header"]["first_divergent_step_index"], 0);
    assert_eq!(v["header"]["first_divergent_step_role"], "assistant");
    assert_eq!(v["steps"][0]["index"], 0);
    assert_eq!(v["steps"][0]["role"], "assistant");
    assert_eq!(v["steps"][0]["status"], "diverge");
    assert_eq!(
        v["steps"][0]["diff_fields"],
        serde_json::json!(["assistant.content", "bash.command", "tool.stdout"])
    );
}

#[test]
fn diff_suppresses_whitespace_noise_unless_requested() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[("hello world", "echo hi", "done\n", "", 0)],
    );
    write_diff_traj(
        &candidate,
        "abc",
        None,
        0.10,
        &[("hello   world", "echo hi", "done\n", "", 0)],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[step 0 - identical]"), "{stdout}");

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--show-noise")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[step 0 - diverge]"), "{stdout}");
    assert!(stdout.contains("assistant.content"), "{stdout}");
}

#[test]
fn diff_suppresses_timestamp_noise_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[(
            "```bash\ncat log\n```",
            "cat log",
            "finished_at=2026-04-27T12:00:00Z\n",
            "",
            0,
        )],
    );
    write_diff_traj(
        &candidate,
        "abc",
        None,
        0.10,
        &[(
            "```bash\ncat log\n```",
            "cat log",
            "finished_at=2026-04-27T12:00:01Z\n",
            "",
            0,
        )],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[step 0 - identical]"), "{stdout}");
}

#[test]
fn diff_unified_format_suppresses_noise_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[(
            "hello world",
            "cat log",
            "finished_at=2026-04-27T12:00:00Z\n",
            "",
            0,
        )],
    );
    write_diff_traj(
        &candidate,
        "abc",
        None,
        0.10,
        &[(
            "hello   world",
            "cat log",
            "finished_at=2026-04-27T12:00:01Z\n",
            "",
            0,
        )],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--format")
        .arg("unified")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout, "--- baseline\n+++ candidate\n", "{stdout}");
}

#[test]
fn diff_unified_format_renders_canonical_unified_diff() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[("```bash\npytest -q\n```", "pytest -q", "1 failed\n", "", 1)],
    );
    write_diff_traj(
        &candidate,
        "abc",
        Some(FailureCategory::StepLimit),
        0.20,
        &[(
            "```bash\npytest tests\n```",
            "pytest tests",
            "2 failed\n",
            "",
            1,
        )],
    );

    let out = Command::new(binary_path())
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .arg("--format")
        .arg("unified")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("--- baseline\n+++ candidate\n"),
        "{stdout}"
    );
    assert!(stdout.contains("@@ -1,"), "{stdout}");
    assert!(stdout.contains(" instance_id: abc"), "{stdout}");
    assert!(stdout.contains("-bash.command: pytest -q"), "{stdout}");
    assert!(stdout.contains("+bash.command: pytest tests"), "{stdout}");
    assert!(!stdout.contains("@@ trajectory abc @@"), "{stdout}");
}

#[test]
fn diff_text_highlights_changed_fields_when_color_is_forced() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj(
        &baseline,
        "abc",
        None,
        0.10,
        &[("hello world", "echo hi", "done\n", "", 0)],
    );
    write_diff_traj(
        &candidate,
        "abc",
        None,
        0.10,
        &[("goodbye world", "echo hi", "done\n", "", 0)],
    );

    let out = Command::new(binary_path())
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .arg("bench")
        .arg("inspect")
        .arg("--diff")
        .arg(&baseline)
        .arg(&candidate)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\u{1b}[1;31m!= assistant.content\u{1b}[0m"),
        "expected ANSI highlighting for changed field labels:\n{stdout}"
    );
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
    let stderr_block = stdout
        .split("stderr:\n")
        .nth(1)
        .and_then(|s| s.split("\n… [").next())
        .unwrap_or_default();
    let stderr_lines = stderr_block
        .lines()
        .filter(|l| l.starts_with("line-"))
        .count();
    assert!(
        stderr_lines <= 40,
        "expected at most 40 lines of stderr, got {stderr_lines}\n{stdout}"
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
fn truncation_is_utf8_safe_for_multibyte_logs() {
    let sweep = tempfile::tempdir().unwrap();
    let stderr: String = (0..300).map(|_| "测试🙂").collect();
    write_traj_with_stderr(sweep.path(), "utf8", &stderr);

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "utf8",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected success for UTF-8 logs; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("full output at trajectory.json#/steps/1"),
        "{stdout}"
    );
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
