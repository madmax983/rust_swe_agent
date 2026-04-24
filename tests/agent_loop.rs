//! Integration: DeterministicModel + LocalEnvironment run the full agent
//! loop end-to-end; assert trajectory shape and termination.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use rust_swe_agent::agent::default::{DefaultAgentBuilder, retag_cache_hints};
use rust_swe_agent::{
    Agent, CacheHint, Config, DeterministicModel, Environment, ExitReason, LocalEnvironment,
    Message, Role,
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
