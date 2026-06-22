//! Deterministic, $0 coverage for `--driver codex`.
//!
//! Driving the real `codex` CLI costs money and is non-deterministic, so the
//! driver reads its binary path from `MAXWELLS_CODEX_BIN`. Here we point it at
//! a fixture shell script that (a) makes a real working-tree edit — exactly as
//! Codex would — and (b) emits a canned NDJSON transcript whose message shapes
//! match the Codex `--full-auto --json` wire format. This exercises the whole
//! bridge (spawn → stream parse → trajectory finalize → patch capture) without
//! a network call or an API key.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
#[cfg(unix)]
use std::process::Command;

use maxwells_daemon::{
    config::Config,
    run::mini::{InteractiveMode, MiniArgs, RunDriver, run},
};

#[cfg(unix)]
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@test"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    std::fs::write(dir.join("a.txt"), "line one\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "base"]);
}

/// Write an executable fake `codex` that appends to `a.txt` (a real edit in
/// its cwd) and prints a canned NDJSON transcript. Returns its path.
#[cfg(unix)]
fn write_fake_codex(dir: &Path) -> std::path::PathBuf {
    // The transcript mirrors the real Codex `--full-auto --json` wire shapes:
    // a session init event, a reasoning turn, a test shell call + output, a
    // second shell call + output, a final message turn, then the completed event.
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
# Perform the real edit in the current working directory, like Codex.
echo 'line two' >> a.txt
cat <<'JSON'
{"type":"session","session_id":"sess-abc123","model":"o4-mini"}
{"type":"reasoning","content":[{"type":"thinking","text":"I will run the tests, then append the requested line."}]}
{"type":"local_shell_call","id":"lsc_test","action":{"type":"exec","command":"pytest -q","timeout":30000,"working_directory":"."}}
{"type":"local_shell_call_output","id":"lsc_test","output":{"type":"exec_result","output":"1 passed in 0.10s","exit_code":0,"metadata":{}}}
{"type":"local_shell_call","id":"lsc_1","action":{"type":"exec","command":"echo 'line two' >> a.txt","timeout":30000,"working_directory":"."}}
{"type":"local_shell_call_output","id":"lsc_1","output":{"type":"exec_result","output":"","exit_code":0,"metadata":{}}}
{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done. Appended line two to a.txt."}]}
{"type":"completed","exit_reason":"done","result":"Done. Appended line two to a.txt.","cost_usd":0.0123,"usage":{"input_tokens":100,"output_tokens":50}}
JSON
"#;
    let path = dir.join("fake_codex.sh");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// Point `MAXWELLS_CODEX_BIN` at a single shared fixture, exactly once.
#[cfg(unix)]
fn ensure_fake_codex() {
    use std::sync::OnceLock;
    static FAKE: OnceLock<std::path::PathBuf> = OnceLock::new();
    FAKE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("maxwells_cx_fake_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_fake_codex(&dir);
        // SAFETY: runs once under OnceLock with all other threads blocked
        // until it completes, so no other thread reads env concurrently.
        unsafe {
            std::env::set_var("MAXWELLS_CODEX_BIN", &path);
        }
        path
    });
}

fn base_args(repo: &Path, out: &Path, name: &str) -> MiniArgs {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 10;
    // codex driver requires yolo: it executes tools itself and cannot enforce
    // the built-in safe/ask deny corpus.
    cfg.root.policy.profile = "yolo".into();
    // Stagnation detection runs inside DefaultAgent::step; the external CLI
    // manages its own loop so the detector never fires.
    cfg.root.agent.detect_stagnation = false;
    MiniArgs {
        driver: RunDriver::Codex,
        driver_append_system_prompt: false,
        driver_isolated: false,
        task: "Append a line saying 'line two' to a.txt".into(),
        extra_context: None,
        config: cfg,
        output_dir: out.to_path_buf(),
        trajectory_name: name.into(),
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        task_timeout_secs: Some(30),
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 10,
        interactive_mode: InteractiveMode::Off,
        no_bell: false,
        resume_from: None,
        trace_id: None,
        webhook_url: None,
        webhook_headers: vec![],
        event_log: None,
        event_log_instance_id: None,
        local_workdir: Some(repo.to_path_buf()),
        read_only: false,
        allow_mcp_in_read_only: false,
        rehearsal_gold_patch: None,
        no_step_persist: false,
        parent_sweep_run_id: None,
        continue_from: None,
        issue_provenance: None,
    }
}

#[cfg(unix)]
fn read_traj(out: &Path, name: &str) -> serde_json::Value {
    let path = out.join(format!("{name}.traj.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
#[cfg(unix)]
async fn codex_driver_produces_valid_trajectory() {
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    ensure_fake_codex();

    run(base_args(repo.path(), out.path(), "cx"))
        .await
        .expect("codex driver run should succeed");

    let traj = read_traj(out.path(), "cx");

    assert_eq!(traj["trajectory_format"], "mini-swe-agent-1.3");
    assert_eq!(traj["info"]["outcome"], "submitted");
    assert_eq!(traj["info"]["exit_reason"], "submitted");
    // Two local_shell_call blocks (pytest + echo) → two steps.
    assert_eq!(traj["info"]["steps"], 2);
    assert!((traj["info"]["total_cost_usd"].as_f64().unwrap() - 0.0123).abs() < 1e-9);
    assert_eq!(traj["info"]["actual_cost_source"], "provider_reported");
    assert_eq!(traj["info"]["token_usage"]["prompt_tokens"], 100);
    assert_eq!(traj["info"]["token_usage"]["completion_tokens"], 50);
    assert_eq!(traj["info"]["model_name"], "o4-mini");
    assert_eq!(
        traj["info"]["final_output"],
        "Done. Appended line two to a.txt."
    );

    // Provenance block identifies the backend + session.
    assert_eq!(traj["info"]["codex_driver"]["driver"], "codex");
    assert_eq!(traj["info"]["codex_driver"]["session_id"], "sess-abc123");

    // Messages: system + task (seeded by the builder), then the streamed turns.
    let msgs = traj["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles.first(), Some(&"system"));
    // The shell command surfaces as an assistant action.
    let shell_action = msgs.iter().any(|m| {
        m["extra"]["actions"].as_array().is_some_and(|a| {
            a.iter()
                .any(|v| v.as_str() == Some("echo 'line two' >> a.txt"))
        })
    });
    assert!(
        shell_action,
        "expected the shell command recorded as an action"
    );

    // The shell output surfaces as a user observation with synthesized run_result.
    let obs = msgs
        .iter()
        .find(|m| m["role"] == "user" && m["extra"]["run_result"].is_object())
        .expect("expected a tool_result observation with run_result");
    let rr = &obs["extra"]["run_result"];
    assert_eq!(rr["exit_code"], 0);
    assert_eq!(rr["timed_out"], false);

    // info.toolset reflects the Codex shell tool.
    assert!(
        traj["info"]["toolset"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"].as_str() == Some("shell"))
    );

    // The driver ran `codex` in the repo, so the real edit landed.
    let contents = std::fs::read_to_string(repo.path().join("a.txt")).unwrap();
    assert_eq!(contents, "line one\nline two\n");
}

/// The codex driver surfaces shell calls as ToolStart/ToolEnd activity events
/// so activity-inferring consumers (the ratatui dashboard, issue #649) see the
/// in-flight command instead of a static idle footer; the driver otherwise
/// emits only AssistantMessage/Observation. These are generic liveness events,
/// not bash-command telemetry.
#[tokio::test]
#[cfg(unix)]
async fn codex_driver_emits_tool_activity_events() {
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    ensure_fake_codex();

    let log = out.path().join("events.jsonl");
    let mut args = base_args(repo.path(), out.path(), "cx-events");
    args.event_log = Some(log.clone());
    args.event_log_instance_id = Some("cx".into());

    run(args).await.expect("codex driver run should succeed");

    let events: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();

    // Two local_shell_call blocks in the fixture → a ToolStart/ToolEnd pair
    // for each.
    assert_eq!(
        types.iter().filter(|t| **t == "tool_start").count(),
        2,
        "expected a tool_start per shell call: {types:?}"
    );
    assert_eq!(
        types.iter().filter(|t| **t == "tool_end").count(),
        2,
        "expected a tool_end per shell call: {types:?}"
    );
    // The shell call is never mislabeled as bash-command telemetry.
    assert!(
        !types.iter().any(|t| *t == "bash_start" || *t == "bash_result"),
        "driver must not emit bash telemetry: {types:?}"
    );

    // The running command rides on ToolStart so the dashboard can label the
    // footer, and each ToolStart precedes its ToolEnd.
    let start = events
        .iter()
        .position(|e| e["event_type"] == "tool_start" && e["label"] == "echo 'line two' >> a.txt")
        .expect("tool_start for the echo command");
    let end = events
        .iter()
        .skip(start)
        .position(|e| e["event_type"] == "tool_end")
        .expect("a tool_end after the echo tool_start");
    assert!(end > 0, "tool_end should follow its tool_start");
}

/// `--driver codex` is rejected with `--env docker`.
#[tokio::test]
async fn codex_driver_rejects_docker_env() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-docker");
    args.config = Config::from_toml_str("[environment]\nkind = \"docker\"\n").unwrap();
    args.config.root.agent.step_limit = 10;
    args.driver = RunDriver::Codex;

    let err = run(args).await.expect_err("docker should be rejected");
    assert!(
        err.to_string().contains("local environment"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected with `--read-only`.
#[tokio::test]
async fn codex_driver_rejects_read_only() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-ro");
    args.read_only = true;

    let err = run(args).await.expect_err("read-only should be rejected");
    assert!(
        err.to_string().contains("read-only"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected with interactive confirmation modes.
#[tokio::test]
async fn codex_driver_rejects_interactive_confirmation() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-int");
    args.interactive_mode = InteractiveMode::StderrPrompt;

    let err = run(args).await.expect_err("interactive should be rejected");
    assert!(
        err.to_string().contains("interactive"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected with a non-yolo policy profile.
#[tokio::test]
async fn codex_driver_rejects_non_yolo_policy() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-safe-policy");
    args.config.root.policy.profile = "safe".into();

    let err = run(args).await.expect_err("safe policy should be rejected");
    assert!(
        err.to_string().contains("policy"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected when a custom command policy is set.
#[tokio::test]
async fn codex_driver_rejects_custom_policy() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-policy");
    args.config.root.policy.extra_deny_patterns = vec!["rm -rf".into()];

    let err = run(args)
        .await
        .expect_err("custom policy should be rejected");
    assert!(
        err.to_string().contains("policy"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected when `--resume` is used.
#[tokio::test]
async fn codex_driver_rejects_resume() {
    use maxwells_daemon::trajectory::Trajectory;
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-resume");
    args.resume_from = Some(Trajectory::default());

    let err = run(args).await.expect_err("resume should be rejected");
    assert!(
        err.to_string().contains("resume") || err.to_string().contains("continue"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected when chaos injection is configured.
#[tokio::test]
async fn codex_driver_rejects_chaos() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-chaos");
    args.config.root.environment.chaos_fail_every = 2;

    let err = run(args).await.expect_err("chaos should be rejected");
    assert!(err.to_string().contains("chaos"), "unexpected error: {err}");
}

/// `--driver codex` is rejected when stagnation detection is on.
#[tokio::test]
async fn codex_driver_rejects_stagnation_detection() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-stagnation");
    args.config.root.agent.detect_stagnation = true;

    let err = run(args)
        .await
        .expect_err("stagnation detection should be rejected");
    assert!(
        err.to_string().contains("stagnation"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected when MCP servers are configured.
#[tokio::test]
async fn codex_driver_rejects_mcp_servers() {
    use maxwells_daemon::config::McpServerCfg;
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-mcp");
    args.config.root.agent.mcp_servers = vec![McpServerCfg {
        command: "false".into(),
        timeout_secs: None,
    }];

    let err = run(args).await.expect_err("mcp servers should be rejected");
    assert!(
        err.to_string().contains("MCP") || err.to_string().contains("mcp"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected when `environment.timeout_secs` is non-default.
#[tokio::test]
async fn codex_driver_rejects_non_default_cmd_timeout() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-cmd-timeout");
    args.config.root.environment.timeout_secs = 30;

    let err = run(args)
        .await
        .expect_err("non-default command timeout should be rejected");
    assert!(
        err.to_string().contains("timeout"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected when budget-visibility is enabled.
#[tokio::test]
async fn codex_driver_rejects_budget_visibility() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-budget-vis");
    args.config.root.agent.per_task_budget_usd = Some(1.0);
    args.config.root.agent.hide_budget_from_agent = false;

    let err = run(args)
        .await
        .expect_err("budget visibility should be rejected");
    assert!(
        err.to_string().contains("budget"),
        "unexpected error: {err}"
    );
}

/// `--driver codex` is rejected when `agent.tools` defines custom command tools.
#[tokio::test]
async fn codex_driver_rejects_configured_tools() {
    use maxwells_daemon::config::ToolCfg;
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cx-tools");
    args.config.root.agent.tools = vec![ToolCfg {
        name: "lint".into(),
        description: None,
        command: "true".into(),
        timeout_secs: None,
    }];

    let err = run(args)
        .await
        .expect_err("configured tools should be rejected");
    assert!(err.to_string().contains("tools"), "unexpected error: {err}");
}

/// A Codex shell call matching a test pattern (`pytest`) is paired into
/// pre-submit test telemetry.
#[tokio::test]
#[cfg(unix)]
async fn codex_driver_records_pre_submit_test_telemetry() {
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    ensure_fake_codex();

    run(base_args(repo.path(), out.path(), "cx-tests"))
        .await
        .expect("run should complete");
    let traj = read_traj(out.path(), "cx-tests");

    assert_eq!(traj["info"]["tests_run_before_submit"], true);
    assert_eq!(traj["info"]["last_tests_passed"], true);
    let invocations = traj["info"]["test_invocations"].as_array().unwrap();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0]["command"], "pytest -q");
    assert_eq!(invocations[0]["exit_code"], 0);
}

/// A run that exceeds the configured per-task budget is recorded as
/// `budget_exhausted`, never `submitted`.
#[tokio::test]
#[cfg(unix)]
async fn codex_driver_downgrades_over_budget_run() {
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    ensure_fake_codex();

    let mut args = base_args(repo.path(), out.path(), "cx-budget");
    // Fixture reports cost_usd = 0.0123; set a cap well below that.
    args.config.root.agent.per_task_budget_usd = Some(0.001);
    args.config.root.agent.hide_budget_from_agent = true;

    run(args).await.expect("run should complete");
    let traj = read_traj(out.path(), "cx-budget");
    assert_eq!(traj["info"]["outcome"], "budget_exhausted");
    assert_eq!(traj["info"]["failure_category"], "budget_exhausted");
}

/// When parsed shell-call count exceeds `step_limit`, outcome is downgraded
/// to `step_limit` so a run never reports `submitted` after taking more tool
/// actions than the cap allows.
#[tokio::test]
#[cfg(unix)]
async fn codex_driver_downgrades_step_overflow() {
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    ensure_fake_codex();

    let mut args = base_args(repo.path(), out.path(), "cx-steps");
    // The fixture transcript emits two local_shell_call blocks; cap at one.
    args.config.root.agent.step_limit = 1;
    run(args).await.expect("run should complete");

    let traj = read_traj(out.path(), "cx-steps");
    assert_eq!(traj["info"]["steps"], 2);
    assert_eq!(traj["info"]["exit_reason"], "step_limit");
    assert_eq!(traj["info"]["failure_category"], "step_limit");
}
