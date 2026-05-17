//! `bench inspect`: CLI integration tests.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use maxwells_daemon::trajectory::{
    FailureCategory, TestInvocation, TokenUsage, Trajectory, outcome,
};

mod support;
use support::binary_path;

fn write_traj(dir: &Path, instance_id: &str, huge_stderr: bool) {
    write_traj_with_tokens(
        dir,
        instance_id,
        huge_stderr,
        TokenUsage {
            prompt_tokens: 123,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            completion_tokens: 45,
        },
    );
}

fn write_traj_with_tokens(
    dir: &Path,
    instance_id: &str,
    huge_stderr: bool,
    token_usage: TokenUsage,
) {
    let mut t = Trajectory::new();
    t.info.model_name = Some("deterministic-test".into());
    t.info.outcome = Some(outcome::ERROR.into());
    t.info.failure_category = Some(FailureCategory::StepLimit);
    t.info.total_cost_usd = Some(0.55);
    t.info.token_usage = Some(token_usage);

    let mut asst = maxwells_daemon::model::Message::assistant("```bash\necho hi\n```");
    asst.extra.actions = Some(vec!["echo hi".into()]);
    t.record_message(&asst);

    let mut obs = maxwells_daemon::model::Message::user("Exit code: 0\nOutput:\nhi");
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

    let mut asst = maxwells_daemon::model::Message::assistant("```bash\necho hi\n```");
    asst.extra.actions = Some(vec!["echo hi".into()]);
    t.record_message(&asst);

    let mut obs = maxwells_daemon::model::Message::user("Exit code: 0\nOutput:\nhi");
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
    write_diff_traj_with_tokens(
        path,
        instance_id,
        failure_category,
        cost_usd,
        TokenUsage {
            prompt_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            completion_tokens: 20,
        },
        steps,
    );
}

fn write_diff_traj_with_tokens(
    path: &Path,
    instance_id: &str,
    failure_category: Option<FailureCategory>,
    cost_usd: f64,
    token_usage: TokenUsage,
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
    t.info.token_usage = Some(token_usage);
    t.info.steps = Some(u32::try_from(steps.len()).unwrap_or(u32::MAX));

    for (assistant, command, stdout, stderr, exit_code) in steps {
        let mut asst = maxwells_daemon::model::Message::assistant(*assistant);
        asst.extra.actions = Some(vec![(*command).into()]);
        t.record_message(&asst);

        let mut obs = maxwells_daemon::model::Message::user("tool result");
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

    t.record_message(&maxwells_daemon::model::Message::system(system_prompt));
    t.record_message(&maxwells_daemon::model::Message::user(user_prompt));

    let mut asst = maxwells_daemon::model::Message::assistant(assistant);
    asst.extra.actions = Some(vec![command.into()]);
    t.record_message(&asst);

    let mut obs = maxwells_daemon::model::Message::user("tool result");
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
        t.record_message(&maxwells_daemon::model::Message::user(
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
    let mut asst = maxwells_daemon::model::Message::assistant(assistant);
    asst.extra.actions = Some(vec![command.into()]);
    trajectory.record_message(&asst);
    record_tool_result(trajectory, stdout);
}

fn record_tool_result(trajectory: &mut Trajectory, stdout: &str) {
    let mut obs = maxwells_daemon::model::Message::user("tool result");
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
        "--show-expected",
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
fn diff_header_reports_total_prompt_tokens_with_cache_breakdown() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.traj.json");
    let candidate = dir.path().join("candidate.traj.json");
    write_diff_traj_with_tokens(
        &baseline,
        "abc",
        None,
        0.10,
        TokenUsage {
            prompt_tokens: 100,
            cache_read_tokens: 800,
            cache_creation_tokens: 100,
            completion_tokens: 20,
        },
        &[("```bash\necho hi\n```", "echo hi", "hi\n", "", 0)],
    );
    write_diff_traj_with_tokens(
        &candidate,
        "abc",
        None,
        0.12,
        TokenUsage {
            prompt_tokens: 200,
            cache_read_tokens: 500,
            cache_creation_tokens: 0,
            completion_tokens: 30,
        },
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
    assert!(
        stdout.contains(
            "tokens: prompt=1000 (input=100 cache_read=800 cache_creation=100) completion=20 -> prompt=700 (input=200 cache_read=500 cache_creation=0) completion=30"
        ),
        "{stdout}"
    );
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
fn instance_mode_renders_patch_stats_from_evaluation_json() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "abc", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "abc",
                "resolved": true,
                "tests_passed": [],
                "tests_failed": [],
                "eval_exit_reason": "resolved",
                "patch_stats": {
                    "files_changed": 2,
                    "hunks": 3,
                    "lines_added": 8,
                    "lines_removed": 2,
                    "is_empty": false,
                    "touches_test_files": true,
                    "touches_lock_or_generated": true,
                    "gold_files_iou": 0.5,
                    "gold_lines_overlap": 0.25,
                    "gold_size_ratio": 1.25
                }
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "patch_stats:      files=2 hunks=3 +8 -2 empty=false tests=true lock_or_generated=true"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("gold_distance:    files_iou=0.500 lines_overlap=0.250 size_ratio=1.250"),
        "{stdout}"
    );
}

#[test]
fn instance_mode_renders_test_telemetry_header_line() {
    let sweep = tempfile::tempdir().unwrap();
    let mut t = Trajectory::new();
    t.info.model_name = Some("deterministic-test".into());
    t.info.outcome = Some(outcome::SUBMITTED.into());
    t.info.tests_run_before_submit = true;
    t.info.last_tests_passed = Some(true);
    t.info.test_invocations = vec![TestInvocation {
        step_index: 0,
        command: "pytest -q".into(),
        exit_code: 0,
        matched_pattern: "pytest".into(),
    }];
    record_assistant_tool_step(&mut t, "```bash\npytest -q\n```", "pytest -q", "ok\n");
    std::fs::write(
        sweep.path().join("abc.traj.json"),
        serde_json::to_string_pretty(&t).unwrap(),
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
    assert!(
        stdout.contains(
            "tests:            count=1 last_exit_code=0 last_passed=true submitted_without_tests=false"
        ),
        "{stdout}"
    );
}

#[test]
fn instance_mode_reports_total_prompt_tokens_with_cache_breakdown() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj_with_tokens(
        sweep.path(),
        "cached",
        false,
        TokenUsage {
            prompt_tokens: 100,
            cache_read_tokens: 800,
            cache_creation_tokens: 50,
            completion_tokens: 20,
        },
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "cached",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "tokens:           prompt=950 (input=100 cache_read=800 cache_creation=50) completion=20"
        ),
        "{stdout}"
    );
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
    assert!(stdout.contains("│ instance_id ┆ outcome"), "{stdout}");
    assert!(stdout.contains("│ a           ┆ error"), "{stdout}");
    assert!(!stdout.contains("│ b           ┆ error"), "{stdout}");
}

// ── issue-175: failing-test names in bench inspect ────────────────────────────

#[test]
fn resolved_instance_has_no_failing_tests_section() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": true,
                "tests_passed": ["tests/test_widgets.py::test_render"],
                "tests_failed": [],
                "eval_exit_reason": "resolved"
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
            "my-instance",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("Failing tests"),
        "resolved instance should not show Failing tests section:\n{stdout}"
    );
}

#[test]
fn unresolved_with_evaluator_failures_shows_failing_tests() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": false,
                "tests_passed": [],
                "tests_failed": [
                    "tests/test_widgets.py::test_widget_render",
                    "tests/test_widgets.py::test_widget_init"
                ],
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
            "my-instance",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Failing tests (2):"),
        "expected count header:\n{stdout}"
    );
    assert!(
        stdout.contains("tests/test_widgets.py::test_widget_render"),
        "expected first test name:\n{stdout}"
    );
    assert!(
        stdout.contains("tests/test_widgets.py::test_widget_init"),
        "expected second test name:\n{stdout}"
    );
    // order must be preserved
    let render_pos = stdout
        .find("test_widget_render")
        .unwrap_or_else(|| panic!("test_widget_render not found in:\n{stdout}"));
    let init_pos = stdout
        .find("test_widget_init")
        .unwrap_or_else(|| panic!("test_widget_init not found in:\n{stdout}"));
    assert!(
        render_pos < init_pos,
        "test names should appear in evaluator-reported order"
    );
}

#[test]
fn unresolved_without_test_names_shows_reason_from_eval_exit_reason() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": false,
                "tests_passed": [],
                "tests_failed": [],
                "eval_exit_reason": "eval_error"
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
            "my-instance",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Failing tests: <"),
        "expected unavailable reason format:\n{stdout}"
    );
    assert!(
        stdout.contains("eval_error"),
        "expected eval_error reason in output:\n{stdout}"
    );
}

#[test]
fn json_format_includes_failing_tests_field() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": false,
                "tests_passed": [],
                "tests_failed": ["tests/test_core.py::test_main"],
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
            "my-instance",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\n{stdout}"));
    let ft = value
        .pointer("/failing_tests")
        .unwrap_or_else(|| panic!("failing_tests missing from JSON output:\n{stdout}"));
    assert_eq!(
        ft["tests"],
        serde_json::json!(["tests/test_core.py::test_main"]),
        "failing_tests.tests mismatch"
    );
    assert_eq!(ft["source"], "evaluator", "failing_tests.source mismatch");
}

#[test]
fn json_format_resolved_instance_has_no_failing_tests_field() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": true,
                "tests_passed": ["tests/test_core.py::test_main"],
                "tests_failed": [],
                "eval_exit_reason": "resolved"
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
            "my-instance",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\n{stdout}"));
    assert!(
        value.pointer("/failing_tests").is_none(),
        "resolved instance should not have failing_tests in JSON:\n{stdout}"
    );
}

#[test]
fn show_expected_parses_json_encoded_string_form() {
    // SWE-bench Hugging Face exports store PASS_TO_PASS / FAIL_TO_PASS as
    // a JSON-encoded string (e.g. "[\"test_a\"]") not a native JSON array.
    // Verify --show-expected handles that form correctly.
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": false,
                "tests_passed": [],
                "tests_failed": [],
                "eval_exit_reason": "unresolved"
            }]
        })
        .to_string(),
    )
    .unwrap();
    // Store PASS_TO_PASS / FAIL_TO_PASS as JSON-encoded strings (HF export format)
    std::fs::write(
        sweep.path().join("dataset.jsonl"),
        serde_json::json!({
            "instance_id": "my-instance",
            "repo": "test/repo",
            "PASS_TO_PASS": "[\"tests/test_core.py::test_existing\"]",
            "FAIL_TO_PASS": "[\"tests/test_core.py::test_target\"]"
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "my-instance",
            "--show-expected",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("tests/test_core.py::test_existing"),
        "expected PASS_TO_PASS test from JSON-encoded string:\n{stdout}"
    );
    assert!(
        stdout.contains("tests/test_core.py::test_target"),
        "expected FAIL_TO_PASS test from JSON-encoded string:\n{stdout}"
    );
}

#[test]
fn show_expected_renders_pass_to_pass_and_fail_to_pass() {
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": false,
                "tests_passed": [],
                "tests_failed": ["tests/test_core.py::test_fail"],
                "eval_exit_reason": "unresolved"
            }]
        })
        .to_string(),
    )
    .unwrap();
    // Write a minimal dataset.jsonl with PASS_TO_PASS / FAIL_TO_PASS
    std::fs::write(
        sweep.path().join("dataset.jsonl"),
        serde_json::json!({
            "instance_id": "my-instance",
            "repo": "test/repo",
            "PASS_TO_PASS": ["tests/test_core.py::test_existing"],
            "FAIL_TO_PASS": ["tests/test_core.py::test_fail"]
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--instance",
            "my-instance",
            "--show-expected",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PASS_TO_PASS"),
        "expected PASS_TO_PASS section:\n{stdout}"
    );
    assert!(
        stdout.contains("FAIL_TO_PASS"),
        "expected FAIL_TO_PASS section:\n{stdout}"
    );
    assert!(
        stdout.contains("tests/test_core.py::test_existing"),
        "expected PASS_TO_PASS test:\n{stdout}"
    );
    assert!(
        stdout.contains("tests/test_core.py::test_fail"),
        "expected FAIL_TO_PASS test:\n{stdout}"
    );
}

#[test]
fn failing_tests_json_shape_round_trips() {
    // Verify the JSON schema documented in the spec: { tests, source, reason }
    let ft = maxwells_daemon::run::inspect::FailingTests {
        tests: vec!["tests/test_core.py::test_foo".into()],
        source: "evaluator".into(),
        reason: String::new(),
    };
    let json = serde_json::to_string(&ft).unwrap();
    let back: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(back["tests"][0], "tests/test_core.py::test_foo");
    assert_eq!(back["source"], "evaluator");

    let ft_unavail = maxwells_daemon::run::inspect::FailingTests {
        tests: vec![],
        source: "unavailable".into(),
        reason: "eval_error".into(),
    };
    let json2 = serde_json::to_string(&ft_unavail).unwrap();
    let back2: serde_json::Value = serde_json::from_str(&json2).unwrap();
    assert_eq!(back2["source"], "unavailable");
    assert_eq!(back2["reason"], "eval_error");
}

#[test]
fn unresolved_with_patch_apply_failed_shows_reason() {
    // eval_exit_reason other than eval_error is also surfaced as the reason.
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": false,
                "tests_passed": [],
                "tests_failed": [],
                "eval_exit_reason": "patch_apply_failed"
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
            "my-instance",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Failing tests: <patch_apply_failed>"),
        "expected patch_apply_failed reason:\n{stdout}"
    );
}

#[test]
fn failing_test_names_are_redacted_at_view_time() {
    // A secret-shaped string embedded in a test name should be redacted.
    // The token uses a bracket delimiter so the regex \b word-boundary fires.
    let secret = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    let sweep = tempfile::tempdir().unwrap();
    write_traj(sweep.path(), "my-instance", false);
    std::fs::write(
        sweep.path().join("evaluation.json"),
        serde_json::json!({
            "instances": [{
                "instance_id": "my-instance",
                "resolved": false,
                "tests_passed": [],
                "tests_failed": [format!("tests/test_core.py::test_secret[{secret}]")],
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
            "my-instance",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains(secret),
        "secret should be redacted from failing test name:\n{stdout}"
    );
    assert!(
        stdout.contains("Failing tests"),
        "Failing tests section should still appear:\n{stdout}"
    );
}
