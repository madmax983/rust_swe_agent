//! Operator confirmation callback for issue #312 interactive mode.
//!
//! `DefaultAgent` consults the optional `confirm_callback` after a model-
//! proposed bash/tool action has cleared the policy gate and PreToolUse
//! hooks but before the environment is invoked. Operators can approve,
//! reject (returns a synthetic observation to the model so it can revise),
//! or abort the run (the loop terminates with
//! `ExitReason::UserInterrupt`).
//!
//! Production implementations live in `confirm_cli` and `confirm_tui`;
//! tests use the `ScriptedConfirmer` shipped here.

use std::sync::Mutex;

use async_trait::async_trait;

/// Information shown to the operator at the confirmation prompt.
#[derive(Debug, Clone)]
pub struct ConfirmContext {
    /// `"bash"` for shell actions, or the MCP/command-tool name.
    pub tool_name: String,
    /// The exact command/input string, already redacted for trajectory
    /// display.
    pub command: String,
    /// 0-based current step at the moment the prompt is shown.
    pub step: u32,
    /// `config.agent.step_limit` for this run.
    pub step_limit: u32,
    /// Cumulative spend in USD across the run so far.
    pub cost_usd: f64,
    /// Cache marker mirroring `InteractiveAgent::status_line`:
    /// `"cache:explicit"` or `"cache:auto-or-none"`.
    pub cache_marker: &'static str,
}

/// The operator's decision returned by `ConfirmCallback::confirm`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmDecision {
    /// Execute the proposed action normally.
    Approve,
    /// Skip execution; surface a synthetic observation to the model.
    Reject(Option<String>),
    /// Terminate the run cleanly with `ExitReason::UserInterrupt`.
    Abort,
    /// Edit the proposed command/input.
    Edit(String),
}

impl ConfirmDecision {
    /// Stable lowercase label for trajectory `interactive_decision` entries.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject(_) => "reject",
            Self::Abort => "abort",
            Self::Edit(_) => "edit",
        }
    }
}

/// Pause the agent loop and collect an operator decision.
///
/// Implementors own whatever I/O they need (stdin raw mode, ratatui
/// frame draw, channel send, etc.). The agent loop only sees the typed
/// return value.
#[async_trait]
pub trait ConfirmCallback: Send + Sync {
    /// Ask the operator how to proceed with `ctx`.
    async fn confirm(&self, ctx: &ConfirmContext) -> ConfirmDecision;
}

/// Test/scripted confirmer that returns decisions from a fixed queue.
///
/// When the queue is exhausted the confirmer returns `Approve` so a test
/// that exercises a `Submit`-only flow can pass an empty queue. Use
/// `call_count()` to verify exact call counts.
pub struct ScriptedConfirmer {
    decisions: Mutex<std::collections::VecDeque<ConfirmDecision>>,
    calls: Mutex<u32>,
}

impl ScriptedConfirmer {
    #[must_use]
    pub fn new(decisions: impl IntoIterator<Item = ConfirmDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            calls: Mutex::new(0),
        }
    }

    #[must_use]
    pub fn call_count(&self) -> u32 {
        *self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl ConfirmCallback for ScriptedConfirmer {
    async fn confirm(&self, _ctx: &ConfirmContext) -> ConfirmDecision {
        *self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        self.decisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(ConfirmDecision::Approve)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn scripted_returns_decisions_in_order_then_default_approve() {
        let c = ScriptedConfirmer::new([ConfirmDecision::Reject(None), ConfirmDecision::Abort]);
        let ctx = ConfirmContext {
            tool_name: "bash".into(),
            command: "x".into(),
            step: 0,
            step_limit: 1,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
        };
        assert_eq!(c.confirm(&ctx).await, ConfirmDecision::Reject(None));
        assert_eq!(c.confirm(&ctx).await, ConfirmDecision::Abort);
        assert_eq!(c.confirm(&ctx).await, ConfirmDecision::Approve);
        assert_eq!(c.call_count(), 3);
    }

    #[test]
    fn decision_labels_are_stable() {
        assert_eq!(ConfirmDecision::Approve.label(), "approve");
        assert_eq!(ConfirmDecision::Reject(None).label(), "reject");
        assert_eq!(
            ConfirmDecision::Reject(Some("feedback".to_owned())).label(),
            "reject"
        );
        assert_eq!(ConfirmDecision::Abort.label(), "abort");
        assert_eq!(ConfirmDecision::Edit("foo".to_owned()).label(), "edit");
    }

    #[tokio::test]
    async fn scripted_handles_edit_decision() {
        let c = ScriptedConfirmer::new([ConfirmDecision::Edit("foo".to_owned())]);
        let ctx = ConfirmContext {
            tool_name: "bash".into(),
            command: "x".into(),
            step: 0,
            step_limit: 1,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
        };
        assert_eq!(
            c.confirm(&ctx).await,
            ConfirmDecision::Edit("foo".to_owned())
        );
        assert_eq!(c.call_count(), 1);
    }
}
