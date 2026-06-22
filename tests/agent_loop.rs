//! Integration: DeterministicModel + LocalEnvironment run the full agent
//! loop end-to-end; assert trajectory shape and termination.

#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use maxwells_daemon::agent::default::{DefaultAgentBuilder, retag_cache_hints};
use maxwells_daemon::env::CancellationToken;
use maxwells_daemon::error::EnvError;
use maxwells_daemon::stream::{BroadcastSink, StreamEvent, StreamSink};
use maxwells_daemon::{
    Agent, CacheHint, Config, DeterministicModel, Environment, Error, ExitReason, LocalEnvironment,
    McpServerCfg, McpStdioServer, Message, Model, ModelResponse, ModelUsage, QueryOpts, Role,
    RunRequest, RunResult, ToolDefinition, ToolHookCfg, ToolInvocation, ToolOutput, ToolProvider,
};

struct RawResponseModel {
    responses: Mutex<VecDeque<ModelResponse>>,
}

impl RawResponseModel {
    fn new(responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

#[async_trait]
impl Model for RawResponseModel {
    fn name(&self) -> &'static str {
        "raw-response"
    }

    async fn query(
        &self,
        _messages: &[Message],
        _opts: &QueryOpts,
    ) -> Result<ModelResponse, maxwells_daemon::ModelError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| maxwells_daemon::ModelError::Malformed("no scripted response".into()))
    }
}

fn raw_response(content: impl Into<String>, raw: serde_json::Value) -> ModelResponse {
    ModelResponse {
        content: content.into(),
        usage: ModelUsage::default(),
        raw,
        responding_model: None,
        fallback_attempts: Vec::new(),
    }
}

#[tokio::test]
async fn two_turn_echo_submit_produces_well_formed_trajectory() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho round-trip\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model: model.clone(),
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    match exit {
        ExitReason::Submitted { final_output } => assert_eq!(final_output, "final"),
        other => panic!("unexpected exit: {other:?}"),
    }

    // Trajectory: [system, user(instance), assistant(bash), user(obs), assistant(submit)]
    let msgs = &agent.trajectory.messages;
    assert!(
        msgs.len() >= 5,
        "expected at least 5 messages, got {}: {msgs:#?}",
        msgs.len()
    );
    assert_eq!(msgs[0].role, "system");
    assert_eq!(msgs[1].role, "user");
    assert_eq!(msgs[2].role, "assistant");
    // The observation should include stdout from `echo round-trip`.
    assert!(msgs[3].content.contains("round-trip"));
    assert_eq!(msgs[4].role, "assistant");

    // Trajectory `info` populated.
    assert_eq!(
        agent.trajectory.info.exit_reason.as_deref(),
        Some("submitted")
    );
    assert_eq!(agent.trajectory.info.final_output.as_deref(), Some("final"));

    // Model saw 2 calls.
    assert_eq!(model.call_count(), 2);
}

#[tokio::test]
async fn provider_native_bash_tool_call_executes_without_format_error_turn() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(RawResponseModel::new([
        raw_response(
            "Let me inspect the repository.",
            serde_json::json!({
                "choices": [{
                    "message": {
                        "content": "Let me inspect the repository.",
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": "{\"command\":\"echo native-call\"}"
                            }
                        }]
                    }
                }]
            }),
        ),
        raw_response(
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```",
            serde_json::json!({"deterministic": true}),
        ),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("native-call")),
        "native bash tool_call should execute and produce an observation: {:#?}",
        agent.history
    );
    assert!(
        agent.history.iter().any(|m| {
            m.role == Role::Assistant && m.content.contains("```bash\necho native-call\n```")
        }),
        "native bash tool_call should be normalized into assistant history: {:#?}",
        agent.history
    );
    assert!(
        !agent
            .history
            .iter()
            .any(|m| m.content.contains("did not include a valid tool call")),
        "native tool_call should not trigger format-error recovery: {:#?}",
        agent.history
    );
}

#[tokio::test]
async fn fenced_action_wins_over_conflicting_native_tool_call() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(RawResponseModel::new([
        raw_response(
            "```bash\necho fenced-call\n```",
            serde_json::json!({
                "choices": [{
                    "message": {
                        "content": "```bash\necho fenced-call\n```",
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": "{\"command\":\"echo raw-call\"}"
                            }
                        }]
                    }
                }]
            }),
        ),
        raw_response(
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```",
            serde_json::json!({"deterministic": true}),
        ),
    ]));
    let env = RecordingCancellationEnv::default();
    let requests = Arc::clone(&env.requests);
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(env),
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "echo fenced-call");
}

#[tokio::test]
async fn ignores_native_tool_calls_from_unselected_choices() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(RawResponseModel::new([
        raw_response(
            "No tool call here.",
            serde_json::json!({
                "choices": [
                    {
                        "message": {
                            "content": "No tool call here."
                        }
                    },
                    {
                        "message": {
                            "content": "",
                            "tool_calls": [{
                                "id": "call-2",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": "{\"command\":\"echo unselected-choice\"}"
                                }
                            }]
                        }
                    }
                ]
            }),
        ),
        raw_response(
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```",
            serde_json::json!({"deterministic": true}),
        ),
    ]));
    let env = RecordingCancellationEnv::default();
    let requests = Arc::clone(&env.requests);
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(env),
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let requests = requests.lock().unwrap().clone();
    assert!(
        requests.is_empty(),
        "unselected choice tool_calls must not execute: {requests:#?}"
    );
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.content.contains("did not include a valid tool call")),
        "selected choice without an action should follow format-error recovery: {:#?}",
        agent.history
    );
}

#[test]
fn default_system_prompt_discourages_dependency_install_detours() {
    let cfg = Config::defaults().unwrap();
    assert!(
        cfg.root
            .prompts
            .system
            .contains("Do not install dependencies"),
        "default system prompt should keep smoke runs out of dependency-install detours"
    );
}

#[tokio::test]
async fn records_pytest_invocation_before_submit_from_action_text() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\npytest -q\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(FixedExitEnvironment { exit_code: 0 });
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    assert!(agent.trajectory.info.tests_run_before_submit);
    assert_eq!(agent.trajectory.info.last_tests_passed, Some(true));
    assert_eq!(agent.trajectory.info.test_invocations.len(), 1);
    let invocation = &agent.trajectory.info.test_invocations[0];
    assert_eq!(invocation.step_index, 0);
    assert_eq!(invocation.command, "pytest -q");
    assert_eq!(invocation.exit_code, 0);
    assert_eq!(invocation.matched_pattern, "pytest");
}

#[tokio::test]
async fn echoing_pytest_does_not_register_as_test_invocation() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho \"I should run pytest\"\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(FixedExitEnvironment { exit_code: 0 });
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));
    assert!(!agent.trajectory.info.tests_run_before_submit);
    assert_eq!(agent.trajectory.info.last_tests_passed, None);
    assert!(agent.trajectory.info.test_invocations.is_empty());
}

#[tokio::test]
async fn cd_prefix_and_custom_pattern_match_test_commands() {
    let cfg = Config::from_toml_str(
        r#"
[agent]
step_limit = 5
test_command_patterns = ["project-(check|test)"]
"#,
    )
    .unwrap();

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\ncd repo && pytest tests/\n```".into(),
        "```bash\nproject-test --suite smoke\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(FixedExitEnvironment { exit_code: 0 });
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));
    let invocations = &agent.trajectory.info.test_invocations;
    assert_eq!(invocations.len(), 2);
    assert_eq!(invocations[0].command, "cd repo && pytest tests/");
    assert_eq!(invocations[0].matched_pattern, "pytest");
    assert_eq!(invocations[1].command, "project-test --suite smoke");
    assert_eq!(invocations[1].matched_pattern, "project-(check|test)");
}

#[tokio::test]
async fn pipeline_segment_after_single_pipe_counts_test_command() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho \"data\" | pytest -q\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(FixedExitEnvironment { exit_code: 0 });
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));
    let invocations = &agent.trajectory.info.test_invocations;
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].command, "echo \"data\" | pytest -q");
    assert_eq!(invocations[0].matched_pattern, "pytest");
}

#[test]
fn invalid_custom_test_command_regex_rejects_agent_build() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.test_command_patterns = vec!["(".to_owned()];

    let Err(err) = DefaultAgentBuilder {
        config: cfg,
        model: Arc::new(DeterministicModel::new(Vec::new())),
        env: Box::new(LocalEnvironment::new()),
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build() else {
        panic!("invalid regex should reject agent build");
    };

    assert!(matches!(err, Error::Config(_)));
    assert!(
        err.to_string().contains("agent.test_command_patterns"),
        "{err}"
    );
}

#[tokio::test]
async fn submit_without_test_commands_records_no_pre_submit_tests() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(DeterministicModel::new(vec![
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(FixedExitEnvironment { exit_code: 0 });
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));
    assert!(!agent.trajectory.info.tests_run_before_submit);
    assert_eq!(agent.trajectory.info.last_tests_passed, None);
    assert!(agent.trajectory.info.test_invocations.is_empty());
}

#[test]
fn cache_retag_is_minimal_and_idempotent() {
    let _ = Role::System;

    let mut h2 = vec![
        Message::system("sys"),
        Message::user("inst"),
        Message::assistant("a1"),
        Message::user("obs1"),
        Message::assistant("a2"),
        Message::user("obs2"),
    ];
    retag_cache_hints(&mut h2);
    assert!(matches!(h2[0].cache_hint, CacheHint::Breakpoint));
    // second-to-last = h2[4] assistant → not tagged.
    assert!(matches!(h2[4].cache_hint, CacheHint::None));

    // Pop last, now second-to-last is obs1 (User) → Auto.
    h2.pop();
    retag_cache_hints(&mut h2);
    assert!(matches!(h2[3].cache_hint, CacheHint::Auto));

    // Idempotent.
    let snapshot: Vec<CacheHint> = h2.iter().map(|m| m.cache_hint).collect();
    retag_cache_hints(&mut h2);
    let after: Vec<CacheHint> = h2.iter().map(|m| m.cache_hint).collect();
    assert_eq!(snapshot, after);
}

#[test]
fn config_parses_pre_and_post_tool_use_hooks() {
    let cfg = Config::from_toml_str(
        r#"
[agent]
tool_hook_timeout_secs = 2

[[agent.hooks.pre_tool_use]]
name = "guard"
command = "echo pre"
timeout_secs = 1

[[agent.hooks.post_tool_use]]
name = "probe"
command = "echo post"
"#,
    )
    .unwrap();

    assert_eq!(cfg.root.agent.tool_hook_timeout_secs, 2);
    assert_eq!(cfg.root.agent.hooks.pre_tool_use.len(), 1);
    assert_eq!(cfg.root.agent.hooks.post_tool_use.len(), 1);
    let pre = &cfg.root.agent.hooks.pre_tool_use[0];
    assert_eq!(pre.name, "guard");
    assert_eq!(pre.command, "echo pre");
    assert_eq!(pre.timeout_secs, Some(1));
    let post = &cfg.root.agent.hooks.post_tool_use[0];
    assert_eq!(post.name, "probe");
    assert_eq!(post.command, "echo post");
    assert_eq!(post.timeout_secs, None);
}

#[test]
fn config_parses_invocation_time_mcp_servers() {
    let cfg = Config::from_toml_str(
        r#"
[[agent.mcp_servers]]
command = "diagnostic-mcp"
timeout_secs = 3
"#,
    )
    .unwrap();

    assert_eq!(cfg.root.agent.mcp_servers.len(), 1);
    let server = &cfg.root.agent.mcp_servers[0];
    assert_eq!(server.command, "diagnostic-mcp");
    assert_eq!(server.timeout_secs, Some(3));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn command_tool_adapter_executes_from_matching_fenced_block() {
    let cfg = Config::from_toml_str(
        r#"
[agent]
step_limit = 5

[[agent.tools]]
name = "diagnose"
description = "Run a repository diagnostic helper."
command = "diagnose-helper"
timeout_secs = 3
"#,
    )
    .unwrap();

    let model = Arc::new(DeterministicModel::new(vec![
        "```diagnose\ncheck flaky test\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env = PluginToolEnv::default();
    let calls = Arc::clone(&env.calls);
    let bcast = Arc::new(BroadcastSink::default());
    let mut rx = bcast.subscribe();
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(env),
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: Some(bcast.clone() as Arc<dyn StreamSink>),
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    // A command tool runs a real shell via `env.run`; it must surface a generic
    // tool-activity span so activity-inferring consumers (the ratatui dashboard,
    // issue #649) see the in-flight command rather than a static idle footer.
    // BashStart/BashResult stay reserved for the Bash tool, so bash-command
    // telemetry is not polluted by other tool types.
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(
        events.iter().any(
            |e| matches!(e, StreamEvent::ToolStart { label, .. } if label == "diagnose-helper")
        ),
        "command tool should emit ToolStart with the rendered command: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolEnd { .. })),
        "command tool should emit ToolEnd to close the activity span: {events:#?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            StreamEvent::BashStart { .. } | StreamEvent::BashResult { .. }
        )),
        "command tool must not emit bash-command telemetry: {events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            StreamEvent::Observation { content, .. }
                if content.contains("diagnose saw check flaky test")
        )),
        "command tool output should surface in an Observation: {events:#?}"
    );

    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].command, "diagnose-helper");
    assert_eq!(
        calls[0]
            .env
            .get("RUST_SWE_AGENT_TOOL_NAME")
            .map(String::as_str),
        Some("diagnose")
    );
    assert_eq!(
        calls[0].env.get("MAXWELL_TOOL_NAME").map(String::as_str),
        Some("diagnose")
    );
    assert_eq!(
        calls[0]
            .env
            .get("RUST_SWE_AGENT_TOOL_INPUT")
            .map(String::as_str),
        Some("check flaky test")
    );
    assert_eq!(
        calls[0].env.get("MAXWELL_TOOL_INPUT").map(String::as_str),
        Some("check flaky test")
    );
    assert_eq!(
        calls[0]
            .env
            .get("RUST_SWE_AGENT_COMMAND")
            .map(String::as_str),
        Some("check flaky test")
    );
    assert_eq!(
        calls[0].env.get("MAXWELL_COMMAND").map(String::as_str),
        Some("check flaky test")
    );
    assert_eq!(calls[0].stdin.as_deref(), Some("check flaky test"));

    let observation = agent
        .history
        .iter()
        .find(|m| m.role == Role::User && m.content.contains("diagnose saw check flaky test"));
    assert!(
        observation.is_some(),
        "command-adapter output should be model-visible: {:#?}",
        agent.history
    );

    let assistant = agent.trajectory.messages.iter().find(|m| {
        m.role == "assistant"
            && m.extra.actions.as_ref().is_some_and(|actions| {
                actions
                    .iter()
                    .any(|action| action == "diagnose:check flaky test")
            })
    });
    assert!(
        assistant.is_some(),
        "trajectory should record the command-adapter action: {:#?}",
        agent.trajectory.messages
    );
}

#[tokio::test]
async fn command_tool_adapter_rendered_command_is_policy_checked() {
    let cfg = Config::from_toml_str(
        r#"
[agent]
step_limit = 5

[[agent.tools]]
name = "diagnose"
description = "Run a repository diagnostic helper."
command = "diagnose-helper {{ tool_input }}"
timeout_secs = 3

[policy]
profile = "safe"
extra_deny_patterns = ["forbidden-file"]
"#,
    )
    .unwrap();

    let model = Arc::new(DeterministicModel::new(vec![
        "```diagnose\nforbidden-file\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env = PluginToolEnv::default();
    let calls = Arc::clone(&env.calls);
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(env),
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    assert!(
        calls.lock().unwrap().is_empty(),
        "policy-denied command adapter must not execute"
    );
    assert!(
        agent.history.iter().any(|m| {
            m.role == Role::User && m.content.contains("Command blocked by policy rule")
        }),
        "policy denial should be model-visible: {:#?}",
        agent.history
    );
    assert_eq!(agent.trajectory.info.policy_counts.blocked, 1);
}

#[tokio::test]
async fn runtime_tool_provider_executes_without_command_tool_config() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(DeterministicModel::new(vec![
        "```diagnose\ncheck flaky test\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let provider = Arc::new(InMemoryToolProvider::new("diagnose"));
    let calls = Arc::clone(&provider.calls);
    let bcast = Arc::new(BroadcastSink::default());
    let mut rx = bcast.subscribe();
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(PanicEnvironment),
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: Some(bcast.clone() as Arc<dyn StreamSink>),
        resume_from: None,
        read_only: false,
    }
    .build_with_tool_providers(vec![provider])
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    // A runtime/MCP provider tool runs real work (an MCP server runs a command
    // via `env.run`); like a command tool it must surface a generic activity
    // span so a slow call reaches the stall indicator (issue #649) instead of a
    // static idle footer, without polluting bash-command telemetry.
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(
        events.iter().any(
            |e| matches!(e, StreamEvent::ToolStart { label, .. } if label == "tool: diagnose")
        ),
        "provider tool should emit ToolStart labeled with the tool name: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolEnd { .. })),
        "provider tool should emit ToolEnd to close the activity span: {events:#?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            StreamEvent::BashStart { .. } | StreamEvent::BashResult { .. }
        )),
        "provider tool must not emit bash-command telemetry: {events:#?}"
    );

    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "diagnose");
    assert_eq!(calls[0].input, "check flaky test");
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("provider saw check flaky test")),
        "provider output should be model-visible: {:#?}",
        agent.history
    );
    let toolset = agent.trajectory.info.other.get("toolset").unwrap();
    assert_eq!(toolset["tools"][1]["name"], "diagnose");
    assert_eq!(toolset["tools"][1]["source"], "runtime_provider");
}

#[tokio::test]
async fn mcp_server_tool_executes_from_matching_fenced_block() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let env = McpToolEnv::default();
    let requests = Arc::clone(&env.requests);
    let provider = Arc::new(
        McpStdioServer::discover(
            &env,
            &McpServerCfg {
                command: "diagnostic-mcp".into(),
                timeout_secs: Some(3),
            },
            10,
            None,
        )
        .await
        .unwrap(),
    );
    let model = Arc::new(DeterministicModel::new(vec![
        "```diagnose\n{\"query\":\"check flaky test\"}\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(env),
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build_with_tool_providers(vec![provider])
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].stdin.as_deref().unwrap().contains("tools/list"));
    assert!(requests[1].stdin.as_deref().unwrap().contains("tools/call"));
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("mcp saw check flaky test")),
        "MCP output should be model-visible: {:#?}",
        agent.history
    );
    let toolset = agent.trajectory.info.other.get("toolset").unwrap();
    assert_eq!(toolset["tools"][1]["name"], "diagnose");
    assert_eq!(toolset["tools"][1]["source"], "mcp_server");
}

#[tokio::test]
async fn pre_tool_use_hook_can_block_the_bash_command() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.pre_tool_use = vec![ToolHookCfg {
        name: "guard".into(),
        command: failing_hook_command(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho should-not-run\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let observation = agent
        .history
        .iter()
        .find(|m| m.role == Role::User && m.content.contains("Tool use blocked"));
    let Some(observation) = observation else {
        panic!(
            "blocked tool observation should be model-visible: {:#?}",
            agent.history
        );
    };
    assert!(observation.content.contains("[guard] exit_code=7"));
    assert!(observation.content.contains("hook failed"));
    assert!(!observation.content.contains("should-not-run"));
}

#[tokio::test]
async fn blocked_pre_tool_use_test_command_does_not_count_as_test_invocation() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.pre_tool_use = vec![ToolHookCfg {
        name: "guard".into(),
        command: failing_hook_command(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\npytest -q\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));
    assert!(agent.trajectory.info.test_invocations.is_empty());
    assert!(!agent.trajectory.info.tests_run_before_submit);
    assert_eq!(agent.trajectory.info.last_tests_passed, None);
}

#[tokio::test]
async fn post_tool_use_hook_output_is_added_to_next_observation() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.post_tool_use = vec![ToolHookCfg {
        name: "probe".into(),
        command: hook_command(&[
            "hook-step",
            "RUST_SWE_AGENT_STEP",
            "hook-command",
            "RUST_SWE_AGENT_COMMAND",
            "hook-code",
            "RUST_SWE_AGENT_EXIT_CODE",
        ]),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho primary\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let observation = agent
        .history
        .iter()
        .find(|m| m.role == Role::User && m.content.contains("PostToolUse hooks:"));
    let Some(observation) = observation else {
        panic!(
            "post hook observation should be model-visible: {:#?}",
            agent.history
        );
    };
    assert!(observation.content.contains("[probe] exit_code=0"));
    assert!(observation.content.contains("hook-step=0"));
    assert!(observation.content.contains("hook-command=echo primary"));
    assert!(observation.content.contains("hook-code=0"));
}

#[tokio::test]
async fn tool_hooks_receive_cancellation_token() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.pre_tool_use = vec![ToolHookCfg {
        name: "pre".into(),
        command: "echo pre-hook".into(),
        timeout_secs: None,
    }];
    cfg.root.agent.hooks.post_tool_use = vec![ToolHookCfg {
        name: "post".into(),
        command: "echo post-hook".into(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho primary\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env = RecordingCancellationEnv::default();
    let requests = Arc::clone(&env.requests);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(env),
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();
    agent.cancellation = Some(CancellationToken::new(cancel_rx));

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let requests = requests.lock().unwrap().clone();
    assert_eq!(
        requests.as_slice(),
        &[
            ("echo pre-hook".to_owned(), true),
            ("echo primary".to_owned(), true),
            ("echo post-hook".to_owned(), true),
        ]
    );
}

#[tokio::test]
async fn failing_post_tool_use_hook_is_reported_but_does_not_abort() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.post_tool_use = vec![ToolHookCfg {
        name: "failing-probe".into(),
        command: failing_hook_command(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho primary\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let observation = agent
        .history
        .iter()
        .find(|m| m.role == Role::User && m.content.contains("[failing-probe]"));
    let Some(observation) = observation else {
        panic!(
            "failing post hook result should be model-visible: {:#?}",
            agent.history
        );
    };
    assert!(observation.content.contains("[failing-probe] exit_code=7"));
    assert!(observation.content.contains("hook failed"));
}

#[tokio::test]
async fn post_tool_use_env_payload_is_capped_for_large_outputs() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.post_tool_use = vec![ToolHookCfg {
        name: "payload-check".into(),
        command: "echo hook-ok".into(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\nproduce lots\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LargeOutputHookEnv::default());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));
}

#[tokio::test]
async fn post_tool_use_environment_error_is_reported_not_propagated() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.post_tool_use = vec![ToolHookCfg {
        name: "spawn-fail".into(),
        command: "echo hook".into(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho primary\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(HookSpawnFailureEnv::default());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let observation = agent
        .history
        .iter()
        .find(|m| m.role == Role::User && m.content.contains("[spawn-fail]"));
    let Some(observation) = observation else {
        panic!(
            "hook environment error should be model-visible: {:#?}",
            agent.history
        );
    };
    assert!(observation.content.contains("[spawn-fail] exit_code=-1"));
    assert!(observation.content.contains("hook environment error"));
}

#[tokio::test]
async fn post_tool_use_hook_output_is_truncated_before_observation_rendering() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.observation_max_bytes = 256;
    cfg.root.agent.hooks.post_tool_use = vec![ToolHookCfg {
        name: "large-probe".into(),
        command: "echo hook".into(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho primary\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LargeHookOutputEnv::default());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let observation = agent
        .history
        .iter()
        .find(|m| m.role == Role::User && m.content.contains("[large-probe]"));
    let Some(observation) = observation else {
        panic!(
            "large post hook observation should be model-visible: {:#?}",
            agent.history
        );
    };
    assert!(
        observation.content.len() < 2_000,
        "hook output should be capped before template rendering, got {} bytes",
        observation.content.len()
    );
    assert!(observation.content.contains("[truncated"));
}

#[tokio::test]
async fn pre_tool_use_environment_error_is_propagated_not_reported_as_blocked_tool() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.pre_tool_use = vec![ToolHookCfg {
        name: "spawn-fail".into(),
        command: "echo hook".into(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho primary\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(PreHookSpawnFailureEnv);
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let err = agent.run().await.unwrap_err();
    assert!(matches!(
        err,
        Error::Env(EnvError::CommandFailed(message))
            if message == "simulated pre hook spawn failure"
    ));
    assert!(
        agent
            .history
            .iter()
            .all(|m| !m.content.contains("Tool use blocked")),
        "pre-hook infrastructure errors should not be model-visible policy denials: {:#?}",
        agent.history
    );
}

#[tokio::test]
async fn pre_tool_use_hook_env_preserves_full_command_for_policy_checks() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.pre_tool_use = vec![ToolHookCfg {
        name: "policy".into(),
        command: "echo hook".into(),
        timeout_secs: None,
    }];

    let long_command = format!("echo {} BLOCKED_SUFFIX", "x".repeat(2_000));
    let model = Arc::new(DeterministicModel::new(vec![
        format!("```bash\n{long_command}\n```"),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(CommandPolicyHookEnv::default());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "round trip".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));
}

#[tokio::test]
async fn tool_hook_task_context_keeps_raw_task_while_trajectory_is_redacted() {
    let configured_secret = "task-hook-secret-value";
    let structured_secret = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    let task = format!("fix {configured_secret} for {structured_secret}");
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.redaction.secret_literals = vec![configured_secret.into()];
    cfg.root.agent.hooks.pre_tool_use = vec![ToolHookCfg {
        name: "task-probe".into(),
        command: "hook-task={{ task }}".into(),
        timeout_secs: None,
    }];

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho primary\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(TaskHookEnv {
        expected_task: task.clone(),
        calls: Mutex::new(0),
    });
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: task.clone(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let trajectory_json = agent.trajectory.to_json_pretty().unwrap();
    for leaked in [configured_secret, structured_secret] {
        assert!(!trajectory_json.contains(leaked), "{trajectory_json}");
    }
    assert!(
        agent
            .trajectory
            .info
            .task
            .as_deref()
            .is_some_and(|task| task.contains("[REDACTED:configured_literal:")
                && task.contains("[REDACTED:github_token:")),
        "{trajectory_json}"
    );
}

fn hook_command(vars: &[&str]) -> String {
    if cfg!(windows) {
        vars.chunks_exact(2)
            .map(|pair| format!("echo {}=%{}%", pair[0], pair[1]))
            .collect::<Vec<_>>()
            .join(" & ")
    } else {
        vars.chunks_exact(2)
            .map(|pair| format!("printf '{}=%s\\n' \"${}\"", pair[0], pair[1]))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn fake_mcp_stdout(stdin: &str) -> String {
    let mut responses = Vec::new();
    for line in stdin.lines() {
        let request: serde_json::Value = serde_json::from_str(line).unwrap();
        let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match method {
            "initialize" => responses.push(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "diagnostic-mcp", "version": "1.0.0"},
                },
            })),
            "notifications/initialized" => {}
            "tools/list" => responses.push(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "tools": [{
                        "name": "diagnose",
                        "description": "Run diagnostics.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": {"type": "string"}
                            },
                            "required": ["query"]
                        },
                    }]
                },
            })),
            "tools/call" => {
                let query = request["params"]["arguments"]["query"]
                    .as_str()
                    .unwrap_or_default();
                responses.push(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": {
                        "content": [{"type": "text", "text": format!("mcp saw {query}")}],
                        "isError": false,
                    },
                }));
            }
            other => panic!("unexpected MCP method: {other}"),
        }
    }
    responses
        .into_iter()
        .map(|response| serde_json::to_string(&response).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn failing_hook_command() -> String {
    if cfg!(windows) {
        "echo hook failed & exit /B 7".into()
    } else {
        "printf 'hook failed\\n'; exit 7".into()
    }
}

#[derive(Default)]
struct LargeOutputHookEnv {
    calls: std::sync::Mutex<u32>,
}

#[derive(Default)]
struct HookSpawnFailureEnv {
    calls: std::sync::Mutex<u32>,
}

#[derive(Default)]
struct LargeHookOutputEnv {
    calls: std::sync::Mutex<u32>,
}

#[derive(Default)]
struct CommandPolicyHookEnv {
    calls: std::sync::Mutex<u32>,
}

struct TaskHookEnv {
    expected_task: String,
    calls: Mutex<u32>,
}

struct FixedExitEnvironment {
    exit_code: i32,
}

#[derive(Default)]
struct InMemoryToolProvider {
    tools: Vec<ToolDefinition>,
    calls: Arc<Mutex<Vec<ToolInvocation>>>,
}

impl InMemoryToolProvider {
    fn new(name: &str) -> Self {
        Self {
            tools: vec![ToolDefinition {
                name: name.into(),
                description: "In-memory test provider.".into(),
                input_schema: None,
            }],
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ToolProvider for InMemoryToolProvider {
    fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    async fn call(
        &self,
        _env: &dyn Environment,
        invocation: ToolInvocation,
        _cancellation: Option<CancellationToken>,
    ) -> Result<ToolOutput, Error> {
        self.calls.lock().unwrap().push(invocation.clone());
        Ok(ToolOutput {
            stdout: format!("provider saw {}\n", invocation.input),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }
}

struct PanicEnvironment;

#[async_trait]
impl Environment for PanicEnvironment {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        panic!("runtime provider should not require env command execution: {req:?}");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PluginToolCall {
    command: String,
    env: std::collections::BTreeMap<String, String>,
    stdin: Option<String>,
}

#[derive(Default)]
struct PluginToolEnv {
    calls: Arc<Mutex<Vec<PluginToolCall>>>,
}

#[derive(Default)]
struct McpToolEnv {
    requests: Arc<Mutex<Vec<RunRequest>>>,
}

struct PreHookSpawnFailureEnv;

#[derive(Default)]
struct RecordingCancellationEnv {
    requests: Arc<Mutex<Vec<(String, bool)>>>,
}

#[async_trait]
impl Environment for RecordingCancellationEnv {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        self.requests
            .lock()
            .unwrap()
            .push((req.command, req.cancellation.is_some()));
        Ok(RunResult {
            stdout: "ok\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }
}

#[async_trait]
impl Environment for FixedExitEnvironment {
    async fn run(&self, _req: RunRequest) -> Result<RunResult, EnvError> {
        Ok(RunResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: self.exit_code,
            timed_out: false,
        })
    }
}

#[async_trait]
impl Environment for PluginToolEnv {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        self.calls.lock().unwrap().push(PluginToolCall {
            command: req.command.clone(),
            env: req.env.clone(),
            stdin: req.stdin.clone(),
        });
        let input = req.stdin.unwrap_or_default();
        Ok(RunResult {
            stdout: format!("diagnose saw {input}\n"),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }
}

#[async_trait]
impl Environment for McpToolEnv {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        self.requests.lock().unwrap().push(req.clone());
        Ok(RunResult {
            stdout: fake_mcp_stdout(req.stdin.as_deref().unwrap_or_default()),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }
}

#[async_trait]
impl Environment for PreHookSpawnFailureEnv {
    async fn run(&self, _req: RunRequest) -> Result<RunResult, EnvError> {
        Err(EnvError::CommandFailed(
            "simulated pre hook spawn failure".into(),
        ))
    }
}

#[async_trait]
impl Environment for LargeHookOutputEnv {
    async fn run(&self, _req: RunRequest) -> Result<RunResult, EnvError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        match *calls {
            1 => Ok(RunResult {
                stdout: "primary\n".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            }),
            2 => Ok(RunResult {
                stdout: format!("hook stdout {}\n", "x".repeat(50_000)),
                stderr: format!("hook stderr {}\n", "y".repeat(50_000)),
                exit_code: 0,
                timed_out: false,
            }),
            other => Err(EnvError::CommandFailed(format!(
                "unexpected env call {other}"
            ))),
        }
    }
}

#[async_trait]
impl Environment for CommandPolicyHookEnv {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        match *calls {
            1 => {
                let Some(command) = req.env.get("RUST_SWE_AGENT_COMMAND") else {
                    return Err(EnvError::CommandFailed("missing command env".into()));
                };
                if !command.ends_with("BLOCKED_SUFFIX") {
                    return Err(EnvError::CommandFailed(format!(
                        "policy suffix missing from command env: {command}"
                    )));
                }
                Ok(RunResult {
                    stdout: "policy-ok\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                })
            }
            2 => Ok(RunResult {
                stdout: "primary\n".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            }),
            other => Err(EnvError::CommandFailed(format!(
                "unexpected env call {other}"
            ))),
        }
    }
}

#[async_trait]
impl Environment for TaskHookEnv {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        match *calls {
            1 => {
                let expected_command = format!("hook-task={}", self.expected_task);
                if req.command != expected_command {
                    return Err(EnvError::CommandFailed(format!(
                        "hook command used redacted task: {}",
                        req.command
                    )));
                }
                let Some(task_env) = req.env.get("RUST_SWE_AGENT_TASK") else {
                    return Err(EnvError::CommandFailed("missing task env".into()));
                };
                if task_env != &self.expected_task {
                    return Err(EnvError::CommandFailed(format!(
                        "task env used redacted task: {task_env}"
                    )));
                }
                let context_json = req.env.get("RUST_SWE_AGENT_CONTEXT_JSON").unwrap();
                let context: serde_json::Value = serde_json::from_str(context_json).unwrap();
                assert_eq!(context["task"].as_str(), Some(self.expected_task.as_str()));
                Ok(RunResult {
                    stdout: "hook-ok\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                })
            }
            2 => {
                assert_eq!(req.command, "echo primary");
                Ok(RunResult {
                    stdout: "primary\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                })
            }
            other => Err(EnvError::CommandFailed(format!(
                "unexpected env call {other}"
            ))),
        }
    }
}

#[async_trait]
impl Environment for HookSpawnFailureEnv {
    async fn run(&self, _req: RunRequest) -> Result<RunResult, EnvError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        match *calls {
            1 => Ok(RunResult {
                stdout: "primary\n".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            }),
            2 => Err(EnvError::CommandFailed("simulated spawn failure".into())),
            other => Err(EnvError::CommandFailed(format!(
                "unexpected env call {other}"
            ))),
        }
    }
}

#[async_trait]
impl Environment for LargeOutputHookEnv {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        match *calls {
            1 => Ok(RunResult {
                stdout: "x".repeat(200_000),
                stderr: "y".repeat(100_000),
                exit_code: 0,
                timed_out: false,
            }),
            2 => {
                for key in [
                    "RUST_SWE_AGENT_STDOUT",
                    "RUST_SWE_AGENT_STDERR",
                    "RUST_SWE_AGENT_OUTPUT",
                    "RUST_SWE_AGENT_CONTEXT_JSON",
                ] {
                    let Some(value) = req.env.get(key) else {
                        return Err(EnvError::CommandFailed(format!("missing env key {key}")));
                    };
                    if value.len() > 32_768 {
                        return Err(EnvError::CommandFailed(format!(
                            "{key} was not capped before hook spawn: {} bytes",
                            value.len()
                        )));
                    }
                }
                let context_json = req.env.get("RUST_SWE_AGENT_CONTEXT_JSON").unwrap();
                let context: serde_json::Value = serde_json::from_str(context_json).unwrap();
                let stdout = context["stdout"].as_str().unwrap();
                assert!(
                    stdout.len() <= 1024,
                    "context stdout should be capped, got {} bytes",
                    stdout.len()
                );
                assert!(
                    stdout.contains("[truncated: original_bytes=200000]"),
                    "context stdout should explain truncation: {stdout}"
                );
                Ok(RunResult {
                    stdout: "hook-ok\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                })
            }
            other => Err(EnvError::CommandFailed(format!(
                "unexpected env call {other}"
            ))),
        }
    }
}
