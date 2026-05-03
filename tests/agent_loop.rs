//! Integration: DeterministicModel + LocalEnvironment run the full agent
//! loop end-to-end; assert trajectory shape and termination.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use rust_swe_agent::agent::default::{DefaultAgentBuilder, retag_cache_hints};
use rust_swe_agent::{
    Agent, CacheHint, Config, DeterministicModel, Environment, ExitReason, LocalEnvironment,
    Message, Role, ToolHookCfg,
};

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

fn failing_hook_command() -> String {
    if cfg!(windows) {
        "echo hook failed & exit /B 7".into()
    } else {
        "printf 'hook failed\\n'; exit 7".into()
    }
}
