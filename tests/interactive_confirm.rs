//! Integration tests for issue #312 interactive-mode confirmation.
//!
//! These tests drive the agent end-to-end with a scripted
//! `ConfirmCallback` and assert:
//!   1. Approve runs the proposed bash command.
//!   2. Reject skips execution, surfaces a synthetic observation, and
//!      tags the trajectory with `interactive_decision: "reject"`.
//!   3. Abort terminates with `ExitReason::UserInterrupt`, leaves the
//!      trajectory flushable, and stamps `interactive_abort` on info.
//!   4. The confirmer is not consulted on `Submit`.
//!   5. `ConfirmContext` carries command, tool name, step, step limit,
//!      and cumulative cost.
//!   6. The full scripted approve→reject→approve→abort sequence works.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use maxwells_daemon::agent::confirm::{
    ConfirmCallback, ConfirmContext, ConfirmDecision, ScriptedConfirmer,
};
use maxwells_daemon::agent::default::DefaultAgentBuilder;
use maxwells_daemon::{
    Agent, Config, DeterministicModel, Environment, ExitReason, LocalEnvironment,
};

#[tokio::test]
async fn approve_lets_command_execute() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho approved-line\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let confirmer = Arc::new(ScriptedConfirmer::new(vec![ConfirmDecision::Approve]));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "approve-test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build()
    .unwrap();
    agent.confirm_callback = Some(confirmer.clone() as Arc<dyn ConfirmCallback>);

    let result = agent.run().await.unwrap();
    assert!(matches!(result, ExitReason::Submitted { .. }));
    assert_eq!(confirmer.call_count(), 1);
    let approved = agent
        .trajectory
        .messages
        .iter()
        .any(|m| m.content.contains("approved-line"));
    assert!(approved, "expected `approved-line` in trajectory");
}

#[tokio::test]
async fn reject_records_synthetic_observation_and_event() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\nrm -rf /tmp/should-never-run\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nrecovered\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let confirmer = Arc::new(ScriptedConfirmer::new(vec![ConfirmDecision::Reject]));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "reject-test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build()
    .unwrap();
    agent.confirm_callback = Some(confirmer.clone() as Arc<dyn ConfirmCallback>);

    let result = agent.run().await.unwrap();
    assert!(matches!(result, ExitReason::Submitted { .. }));
    assert_eq!(confirmer.call_count(), 1);

    // Rejected commands have no run_result observation.
    let executed = agent
        .trajectory
        .messages
        .iter()
        .any(|m| m.extra.other.contains_key("run_result"));
    assert!(!executed);

    let has_rejection = agent.trajectory.messages.iter().any(|m| {
        m.extra
            .other
            .get("interactive_decision")
            .and_then(|v| v.as_str())
            == Some("reject")
    });
    assert!(has_rejection);
}

#[tokio::test]
async fn abort_terminates_with_user_interrupt() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho should-not-execute\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let confirmer = Arc::new(ScriptedConfirmer::new(vec![ConfirmDecision::Abort]));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "abort-test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build()
    .unwrap();
    agent.confirm_callback = Some(confirmer.clone() as Arc<dyn ConfirmCallback>);

    let result = agent.run().await.unwrap();
    assert!(matches!(result, ExitReason::UserInterrupt));

    let executed = agent
        .trajectory
        .messages
        .iter()
        .any(|m| m.extra.other.contains_key("run_result"));
    assert!(!executed);

    let abort_meta = agent.trajectory.info.other.get("interactive_abort");
    assert!(abort_meta.is_some());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    agent.trajectory.save_pretty(tmp.path()).unwrap();
}

#[tokio::test]
async fn scripted_approve_reject_approve_abort_sequence() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 10;
    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho first-approved\n```".into(),
        "```bash\nrm -rf /tmp/rejected-cmd\n```".into(),
        "```bash\necho third-approved\n```".into(),
        "```bash\necho fourth-aborted\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let confirmer = Arc::new(ScriptedConfirmer::new(vec![
        ConfirmDecision::Approve,
        ConfirmDecision::Reject,
        ConfirmDecision::Approve,
        ConfirmDecision::Abort,
    ]));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "scripted".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build()
    .unwrap();
    agent.confirm_callback = Some(confirmer.clone() as Arc<dyn ConfirmCallback>);

    let result = agent.run().await.unwrap();
    assert!(matches!(result, ExitReason::UserInterrupt));
    assert_eq!(confirmer.call_count(), 4);

    let executed: Vec<String> = agent
        .trajectory
        .messages
        .iter()
        .filter_map(|m| {
            if m.role != "user" {
                return None;
            }
            m.extra
                .other
                .get("run_result")
                .and_then(|v| v.get("stdout"))
                .and_then(|s| s.as_str())
                .map(str::to_owned)
        })
        .collect();
    assert!(executed.iter().any(|s| s.contains("first-approved")));
    assert!(executed.iter().any(|s| s.contains("third-approved")));
    assert!(!executed.iter().any(|s| s.contains("fourth-aborted")));
    assert!(!executed.iter().any(|s| s.contains("/tmp/rejected-cmd")));

    let rejects = agent
        .trajectory
        .messages
        .iter()
        .filter(|m| {
            m.extra
                .other
                .get("interactive_decision")
                .and_then(|v| v.as_str())
                == Some("reject")
        })
        .count();
    assert_eq!(rejects, 1);
}

#[tokio::test]
async fn confirmer_not_called_on_submit() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 3;
    let model = Arc::new(DeterministicModel::new(vec![
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let confirmer = Arc::new(ScriptedConfirmer::new(vec![]));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "submit-only".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build()
    .unwrap();
    agent.confirm_callback = Some(confirmer.clone() as Arc<dyn ConfirmCallback>);

    let result = agent.run().await.unwrap();
    assert!(matches!(result, ExitReason::Submitted { .. }));
    assert_eq!(confirmer.call_count(), 0);
}

#[derive(Default)]
struct ContextRecorder {
    seen: Mutex<Vec<ConfirmContext>>,
}

#[async_trait]
impl ConfirmCallback for ContextRecorder {
    async fn confirm(&self, ctx: &ConfirmContext) -> ConfirmDecision {
        self.seen.lock().unwrap().push(ctx.clone());
        ConfirmDecision::Approve
    }
}

#[tokio::test]
async fn confirm_context_carries_command_and_step_metadata() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 7;
    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho context-probe\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let recorder = Arc::new(ContextRecorder::default());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "ctx-probe".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build()
    .unwrap();
    agent.confirm_callback = Some(recorder.clone() as Arc<dyn ConfirmCallback>);

    let _ = agent.run().await.unwrap();
    let ctx = {
        let seen = recorder.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        seen[0].clone()
    };
    assert_eq!(ctx.command.trim(), "echo context-probe");
    assert_eq!(ctx.tool_name, "bash");
    assert_eq!(ctx.step_limit, 7);
    assert_eq!(ctx.step, 0);
    assert!(ctx.cost_usd >= 0.0);
}
