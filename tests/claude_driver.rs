//! Deterministic, $0 coverage for `--driver claude-code`.
//!
//! Driving the real `claude` CLI costs money and is non-deterministic, so the
//! driver reads its binary path from `MAXWELLS_CLAUDE_BIN`. Here we point it at
//! a fixture shell script that (a) makes a real working-tree edit — exactly as
//! Claude Code would — and (b) emits a canned `stream-json` transcript whose
//! message shapes were captured from a real `claude` v2.1 run. This exercises
//! the whole bridge (spawn → stream parse → trajectory finalize → patch
//! capture) without a network call or an API key.

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

/// Write an executable fake `claude` that appends to `a.txt` (a real edit in
/// its cwd) and prints a canned stream-json transcript. Returns its path.
#[cfg(unix)]
fn write_fake_claude(dir: &Path) -> std::path::PathBuf {
    // The transcript mirrors the real wire shapes: system/init, a thinking
    // turn, a Bash tool_use turn, the tool_result delivered as a `user`
    // message, a final text turn, then the authoritative result message.
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
# Perform the real edit in the current working directory, like Claude Code.
echo 'line two' >> a.txt
cat <<'JSON'
{"type":"system","subtype":"init","cwd":".","session_id":"sess-abc123","model":"claude-sonnet-4-6","claude_code_version":"2.1.160","tools":["Bash"],"permissionMode":"default","apiKeySource":"none"}
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}
{"type":"assistant","message":{"model":"claude-sonnet-4-6","role":"assistant","content":[{"type":"thinking","thinking":"I will run the tests, then append the requested line."}],"usage":{"input_tokens":3,"output_tokens":5}}}
{"type":"assistant","message":{"model":"claude-sonnet-4-6","role":"assistant","content":[{"type":"tool_use","id":"toolu_test","name":"Bash","input":{"command":"pytest -q","description":"run tests"}}],"usage":{"input_tokens":8,"output_tokens":12}}}
{"type":"user","message":{"content":[{"tool_use_id":"toolu_test","type":"tool_result","content":"1 passed in 0.10s","is_error":false}]}}
{"type":"assistant","message":{"model":"claude-sonnet-4-6","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo 'line two' >> a.txt","description":"append"}}],"usage":{"input_tokens":10,"output_tokens":20}}}
{"type":"user","message":{"content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"(Bash completed with no output)","is_error":false}]}}
{"type":"assistant","message":{"model":"claude-sonnet-4-6","role":"assistant","content":[{"type":"text","text":"Done. Appended line two to a.txt."}],"usage":{"input_tokens":40,"output_tokens":12}}}
{"type":"result","subtype":"success","is_error":false,"result":"Done. Appended line two to a.txt.","num_turns":3,"total_cost_usd":0.0123,"usage":{"input_tokens":100,"cache_read_input_tokens":10,"cache_creation_input_tokens":20,"output_tokens":50}}
JSON
"#;
    let path = dir.join("fake_claude.sh");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// Point `MAXWELLS_CLAUDE_BIN` at a single shared fixture, exactly once.
/// `OnceLock` serializes initialization, so the var is set before any test
/// reads it and is never mutated again — avoiding a concurrent env-var race
/// between the parallel `#[tokio::test]` cases.
#[cfg(unix)]
fn ensure_fake_claude() {
    use std::sync::OnceLock;
    static FAKE: OnceLock<std::path::PathBuf> = OnceLock::new();
    FAKE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("maxwells_cc_fake_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_fake_claude(&dir);
        // SAFETY: runs once under OnceLock with all other threads blocked
        // until it completes, so no other thread reads env concurrently.
        unsafe {
            std::env::set_var("MAXWELLS_CLAUDE_BIN", &path);
        }
        path
    });
}

fn base_args(repo: &Path, out: &Path, name: &str) -> MiniArgs {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 10;
    // claude-code driver requires yolo: it executes tools itself and cannot
    // enforce the built-in safe/ask deny corpus.
    cfg.root.policy.profile = "yolo".into();
    // Stagnation detection runs inside DefaultAgent::step; the external CLI
    // manages its own loop so the detector never fires.
    cfg.root.agent.detect_stagnation = false;
    MiniArgs {
        driver: RunDriver::ClaudeCode,
        driver_append_system_prompt: false,
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
    }
}

#[cfg(unix)]
fn read_traj(out: &Path, name: &str) -> serde_json::Value {
    let path = out.join(format!("{name}.traj.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
#[cfg(unix)]
async fn claude_driver_produces_valid_trajectory() {
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    ensure_fake_claude();

    run(base_args(repo.path(), out.path(), "cc"))
        .await
        .expect("claude driver run should succeed");

    let traj = read_traj(out.path(), "cc");

    // Schema/contract fields the rest of the tooling depends on.
    assert_eq!(traj["trajectory_format"], "mini-swe-agent-1.3");
    assert_eq!(traj["info"]["outcome"], "submitted");
    assert_eq!(traj["info"]["exit_reason"], "submitted");
    // Two tool_use blocks (pytest + echo) in the transcript → two steps.
    assert_eq!(traj["info"]["steps"], 2);
    // Cost and tokens come from the authoritative result message.
    assert!((traj["info"]["total_cost_usd"].as_f64().unwrap() - 0.0123).abs() < 1e-9);
    assert_eq!(traj["info"]["actual_cost_source"], "provider_reported");
    assert_eq!(traj["info"]["token_usage"]["prompt_tokens"], 100);
    assert_eq!(traj["info"]["token_usage"]["completion_tokens"], 50);
    // The actually-responding model is recorded, not the (unforwarded) --model.
    assert_eq!(traj["info"]["model_name"], "claude-sonnet-4-6");
    assert_eq!(
        traj["info"]["final_output"],
        "Done. Appended line two to a.txt."
    );

    // Provenance block identifies the backend + session.
    assert_eq!(traj["info"]["claude_driver"]["driver"], "claude-code");
    assert_eq!(traj["info"]["claude_driver"]["session_id"], "sess-abc123");

    // Messages: system + task (seeded by the builder), then the streamed turns.
    let msgs = traj["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles.first(), Some(&"system"));
    // The Bash command surfaces as an assistant action.
    let bash_action = msgs.iter().any(|m| {
        m["extra"]["actions"].as_array().is_some_and(|a| {
            a.iter()
                .any(|v| v.as_str() == Some("echo 'line two' >> a.txt"))
        })
    });
    assert!(
        bash_action,
        "expected the bash command recorded as an action"
    );
    // The tool_result surfaces as a user observation.
    let obs = msgs
        .iter()
        .any(|m| m["content"].as_str() == Some("(Bash completed with no output)"));
    assert!(obs, "expected the tool_result recorded as an observation");

    // The driver ran `claude` in the repo, so the real edit landed.
    let contents = std::fs::read_to_string(repo.path().join("a.txt")).unwrap();
    assert_eq!(contents, "line one\nline two\n");
}

/// `--driver claude-code` is rejected with `--env docker` (the CLI edits the
/// host tree and has no path into a container).
#[tokio::test]
async fn claude_driver_rejects_docker_env() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-docker");
    args.config = Config::from_toml_str("[environment]\nkind = \"docker\"\n").unwrap();
    args.config.root.agent.step_limit = 10;
    args.driver = RunDriver::ClaudeCode;

    let err = run(args).await.expect_err("docker should be rejected");
    let msg = err.to_string();
    assert!(msg.contains("local environment"), "unexpected error: {msg}");
}

/// `--driver claude-code` is rejected with `--read-only`: the CLI auto-allows
/// mutation tools, so it cannot honor an analysis-only contract.
#[tokio::test]
async fn claude_driver_rejects_read_only() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-ro");
    args.read_only = true;

    let err = run(args).await.expect_err("read-only should be rejected");
    assert!(
        err.to_string().contains("read-only"),
        "unexpected error: {err}"
    );
}

/// `--driver claude-code` is rejected with interactive confirmation modes: the
/// driver cannot route Claude's tool calls through the operator confirmer.
#[tokio::test]
async fn claude_driver_rejects_interactive_confirmation() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-int");
    args.interactive_mode = InteractiveMode::StderrPrompt;

    let err = run(args).await.expect_err("interactive should be rejected");
    assert!(
        err.to_string().contains("interactive"),
        "unexpected error: {err}"
    );
}

/// `--driver claude-code` is rejected when a custom command policy is set:
/// the CLI runs tools itself and never consults the policy engine.
#[tokio::test]
async fn claude_driver_rejects_custom_policy() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-policy");
    args.config.root.policy.extra_deny_patterns = vec!["rm -rf".into()];

    let err = run(args)
        .await
        .expect_err("custom policy should be rejected");
    assert!(
        err.to_string().contains("policy"),
        "unexpected error: {err}"
    );
}

/// `--driver claude-code` is rejected when chaos injection is configured: it
/// bypasses the wrapped environment, so the faults would never fire.
#[tokio::test]
async fn claude_driver_rejects_chaos() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-chaos");
    args.config.root.environment.chaos_fail_every = 2;

    let err = run(args).await.expect_err("chaos should be rejected");
    assert!(err.to_string().contains("chaos"), "unexpected error: {err}");
}

/// A Claude `Bash` test command (`pytest`) is paired with its result into
/// pre-submit test telemetry, matching the built-in loop.
#[tokio::test]
#[cfg(unix)]
async fn claude_driver_records_pre_submit_test_telemetry() {
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    ensure_fake_claude();

    run(base_args(repo.path(), out.path(), "cc-tests"))
        .await
        .expect("run should complete");
    let traj = read_traj(out.path(), "cc-tests");

    assert_eq!(traj["info"]["tests_run_before_submit"], true);
    assert_eq!(traj["info"]["last_tests_passed"], true);
    let invocations = traj["info"]["test_invocations"].as_array().unwrap();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0]["command"], "pytest -q");
    assert_eq!(invocations[0]["exit_code"], 0);
}

/// A successful run that exceeds the configured per-task budget is recorded as
/// `budget_exhausted`, never `submitted`, so spend controls hold.
#[tokio::test]
#[cfg(unix)]
async fn claude_driver_downgrades_over_budget_run() {
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    ensure_fake_claude();

    let mut args = base_args(repo.path(), out.path(), "cc-budget");
    // Fixture reports total_cost_usd = 0.0123; set a cap well below that.
    args.config.root.agent.per_task_budget_usd = Some(0.001);

    run(args).await.expect("run should complete");
    let traj = read_traj(out.path(), "cc-budget");
    assert_eq!(traj["info"]["outcome"], "budget_exhausted");
    assert_eq!(traj["info"]["failure_category"], "budget_exhausted");
}

/// `--driver claude-code` is rejected when stagnation detection is on: the
/// detector runs inside `DefaultAgent::step` and never fires for the external
/// CLI's own loop.
#[tokio::test]
async fn claude_driver_rejects_stagnation_detection() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-stagnation");
    // Revert to the default (true) that base_args overrides to false.
    args.config.root.agent.detect_stagnation = true;

    let err = run(args)
        .await
        .expect_err("stagnation detection should be rejected");
    assert!(
        err.to_string().contains("stagnation"),
        "unexpected error: {err}"
    );
}

/// `--driver claude-code` is rejected when MCP servers are configured: the
/// CLI manages its own tool routing and never calls the harness ToolRegistry.
#[tokio::test]
async fn claude_driver_rejects_mcp_servers() {
    use maxwells_daemon::config::McpServerCfg;
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-mcp");
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

/// `--driver claude-code` is rejected with a non-yolo policy profile: the
/// built-in deny corpus is enforced inside `DefaultAgent::step` and never fires
/// when the external CLI owns tool execution.
#[tokio::test]
async fn claude_driver_rejects_non_yolo_policy() {
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-safe-policy");
    // Revert to the default safe profile that base_args overrides to yolo.
    args.config.root.policy.profile = "safe".into();

    let err = run(args).await.expect_err("safe policy should be rejected");
    assert!(
        err.to_string().contains("policy"),
        "unexpected error: {err}"
    );
}

/// `--driver claude-code` is rejected when `--resume` is used: the external
/// CLI cannot be seeded with prior message history.
#[tokio::test]
async fn claude_driver_rejects_resume() {
    use maxwells_daemon::trajectory::Trajectory;
    let out = tempfile::tempdir().unwrap();
    let mut args = base_args(out.path(), out.path(), "cc-resume");
    // Inject a dummy partial trajectory to trigger the resume guard.
    args.resume_from = Some(Trajectory::default());

    let err = run(args).await.expect_err("resume should be rejected");
    assert!(
        err.to_string().contains("resume") || err.to_string().contains("continue"),
        "unexpected error: {err}"
    );
}
