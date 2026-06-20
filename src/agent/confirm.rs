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
    /// The agent's stated reasoning for the pending action: the assistant
    /// message prose that carried the command block (issue #655).
    ///
    /// Already passed through the redactor (`surface::TRAJECTORY`) and
    /// length-capped via [`cap_rationale`] before it reaches the operator, so
    /// an adversarially long assistant message cannot exhaust dashboard
    /// memory. May be empty when the agent proposed a command with no
    /// accompanying prose; surfaces are responsible for degrading gracefully.
    pub rationale: String,
}

/// Maximum bytes of rationale retained for display. Content past this cap is
/// truncated with a visible marker (see [`cap_rationale`]).
pub const RATIONALE_MAX_BYTES: usize = 4000;

/// Maximum lines of rationale retained for display. Content past this cap is
/// truncated with a visible marker (see [`cap_rationale`]).
pub const RATIONALE_MAX_LINES: usize = 100;

/// Bound a rationale string to [`RATIONALE_MAX_BYTES`] / [`RATIONALE_MAX_LINES`]
/// for display, appending a visible (never silent) truncation marker when the
/// input exceeds either cap.
///
/// Truncation is UTF-8 boundary-safe: an input whose byte cap falls inside a
/// multi-byte codepoint is trimmed back to the nearest char boundary rather
/// than panicking. Strings within both caps are returned unchanged.
#[must_use]
pub fn cap_rationale(s: &str) -> String {
    let line_truncated = s.lines().nth(RATIONALE_MAX_LINES).is_some();
    let mut out: String = if line_truncated {
        s.lines()
            .take(RATIONALE_MAX_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        s.to_string()
    };

    let byte_truncated = out.len() > RATIONALE_MAX_BYTES;
    if byte_truncated {
        let mut cut = RATIONALE_MAX_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }

    if line_truncated || byte_truncated {
        use std::fmt::Write as _;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        let _ = write!(
            out,
            "… [rationale truncated: {RATIONALE_MAX_BYTES}-byte / {RATIONALE_MAX_LINES}-line cap]"
        );
    }
    out
}

impl ConfirmContext {
    /// Extracts the scope (program name for bash, tool name for others).
    #[must_use]
    pub fn derive_scope(&self) -> String {
        if self.tool_name == "bash" {
            self.command
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            self.tool_name.clone()
        }
    }
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
    /// Automatically approve the action for a given scope.
    AutoApprove(String),
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
            Self::AutoApprove(_) => "auto-approve",
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
            rationale: String::new(),
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
            rationale: String::new(),
        };
        assert_eq!(
            c.confirm(&ctx).await,
            ConfirmDecision::Edit("foo".to_owned())
        );
        assert_eq!(c.call_count(), 1);
    }

    #[test]
    fn confirm_context_derives_correct_scope() {
        let ctx_bash = ConfirmContext {
            tool_name: "bash".into(),
            command: "cargo test --all".into(),
            step: 0,
            step_limit: 10,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
            rationale: String::new(),
        };
        assert_eq!(ctx_bash.derive_scope(), "cargo");

        let ctx_tool = ConfirmContext {
            tool_name: "read_file".into(),
            command: "src/lib.rs".into(),
            step: 0,
            step_limit: 10,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
            rationale: String::new(),
        };
        assert_eq!(ctx_tool.derive_scope(), "read_file");
    }

    #[test]
    fn auto_approve_decision_has_label() {
        let d = ConfirmDecision::AutoApprove("cargo".to_string());
        assert_eq!(d.label(), "auto-approve");
    }

    #[test]
    fn cap_rationale_passes_short_text_unchanged() {
        let s = "I will list the files first.\nThen run the tests.";
        assert_eq!(cap_rationale(s), s);
        assert!(!cap_rationale(s).contains("truncated"));
    }

    #[test]
    fn cap_rationale_truncates_by_byte_cap_with_visible_marker() {
        let s = "x".repeat(RATIONALE_MAX_BYTES + 500);
        let out = cap_rationale(&s);
        assert!(out.len() <= RATIONALE_MAX_BYTES + 64);
        assert!(out.contains("truncated"), "marker must be visible: {out}");
    }

    #[test]
    fn cap_rationale_truncates_by_line_cap_with_visible_marker() {
        let s = (0..RATIONALE_MAX_LINES + 50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = cap_rationale(&s);
        assert!(out.lines().count() <= RATIONALE_MAX_LINES + 1);
        assert!(out.contains("truncated"), "marker must be visible: {out}");
    }

    #[test]
    fn cap_rationale_is_utf8_boundary_safe() {
        // Multi-byte chars right at the cap must not panic or split a codepoint.
        let s = "🦀".repeat(RATIONALE_MAX_BYTES);
        let out = cap_rationale(&s);
        // Round-trips as valid UTF-8 (would have panicked on a bad boundary).
        assert!(out.contains("truncated"));
    }
}
