//! The `Agent` trait and supporting types.
//!
//! Termination is typed success, not error. Python used exceptions for
//! `Submitted` / `LimitsExceeded` because it lacks sum types; Rust has
//! them, so `run()` returns `Result<ExitReason, Error>` and any
//! `ExitReason` variant represents a clean finish.

use async_trait::async_trait;

use crate::error::Error;

pub mod confirm;
pub mod confirm_cli;
pub mod confirm_tui;
pub mod default;
pub mod interactive;
pub mod parse;

pub use confirm::{ConfirmCallback, ConfirmContext, ConfirmDecision, ScriptedConfirmer};
pub use confirm_cli::StderrCliConfirmer;
pub use confirm_tui::{
    RatatuiDashboard, RatatuiDashboardHandle, bell_enabled, effective_cost_cap_usd,
};

pub use default::DefaultAgent;
pub use interactive::InteractiveAgent;
pub use parse::{
    Action, extract_action, extract_action_for_tools, extract_action_from_model_response,
    strip_action_block,
};

/// Represents the typed, successful termination of an agent loop.
///
/// Because Rust has sum types, we do not need to use exceptions (like Python does)
/// for expected control flow boundaries like hitting a step limit or successfully
/// submitting a solution. An `ExitReason` means the agent finished its run *cleanly*
/// according to the rules of the environment, even if it didn't solve the task.
///
/// True system errors (like network failures or I/O issues) are handled by the
/// separate `crate::error::Error` type.
///
/// ## Examples
///
/// ```
/// use maxwells_daemon::agent::ExitReason;
///
/// let exit = ExitReason::Submitted { final_output: "Done!".into() };
/// assert_eq!(exit.label(), "submitted");
/// ```
#[derive(Debug, Clone)]
pub enum ExitReason {
    /// The agent explicitly decided to submit a final answer.
    Submitted { final_output: String },
    /// The agent reached its configured maximum number of allowed steps.
    StepLimit { limit: u32 },
    /// The agent exceeded its configured maximum spend.
    CostLimit { limit_usd: f64, spent_usd: f64 },
    /// The agent's per-task USD budget was exhausted mid-loop.
    BudgetExhausted { limit_usd: f64, spent_usd: f64 },
    /// A human explicitly cancelled the run.
    UserInterrupt,
    /// The LLM backend refused to complete the prompt (e.g. safety filters).
    ModelRefusal { reason: String },
    /// The agent was halted because it repeated the same action K times in W steps.
    AgentStagnation {
        action_hash: String,
        count: u32,
        window: u32,
    },
    /// The prompt could not be compacted to fit within `history_max_input_tokens`
    /// even after eliding all eligible older observations.
    HistoryCompactionFailed,
}

impl ExitReason {
    /// Returns a standardized string label for the termination reason.
    ///
    /// This is typically used for populating the `outcome` field in trajectory files,
    /// allowing for easy aggregation and metrics when analyzing sweeps.
    ///
    /// ## Examples
    ///
    /// ```
    /// use maxwells_daemon::agent::ExitReason;
    ///
    /// let reason = ExitReason::StepLimit { limit: 10 };
    /// assert_eq!(reason.label(), "step_limit");
    /// ```
    pub fn label(&self) -> &'static str {
        match self {
            Self::Submitted { .. } => "submitted",
            Self::StepLimit { .. } => "step_limit",
            Self::CostLimit { .. } => "cost_limit",
            Self::BudgetExhausted { .. } => "budget_exhausted",
            Self::UserInterrupt => "user_interrupt",
            Self::ModelRefusal { .. } => "model_refusal",
            Self::AgentStagnation { .. } => "agent_stagnation",
            Self::HistoryCompactionFailed => "history_compaction_failed",
        }
    }
}

/// The result of a single agent step, determining whether the loop should continue.
///
/// By separating the state machine transitions into this enum, we avoid
/// polluting the error path with control flow data.
///
/// ## Examples
///
/// ```
/// use maxwells_daemon::agent::{StepOutcome, ExitReason};
///
/// let outcome = StepOutcome::Terminate(ExitReason::UserInterrupt);
/// ```
#[derive(Debug, Clone)]
pub enum StepOutcome {
    /// The step completed successfully and the agent should take another action.
    Continue,
    /// The agent has finished its task or hit a hard constraint and must stop.
    Terminate(ExitReason),
}

/// A state machine that interacts with an environment and a model.
///
/// Implementors define how to construct prompts, parse actions, and record
/// trajectory history. The typical pattern is to initialize an agent and then
/// drive it to completion via `run()`.
///
/// ## Examples
///
/// ```no_run
/// use maxwells_daemon::agent::{Agent, StepOutcome, ExitReason};
/// use maxwells_daemon::error::Error;
/// use async_trait::async_trait;
///
/// struct MyAgent {
///     steps: u32,
/// }
///
/// #[async_trait]
/// impl Agent for MyAgent {
///     async fn step(&mut self) -> Result<StepOutcome, Error> {
///         self.steps += 1;
///         if self.steps > 5 {
///             Ok(StepOutcome::Terminate(ExitReason::StepLimit { limit: 5 }))
///         } else {
///             Ok(StepOutcome::Continue)
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait Agent: Send {
    /// Executes exactly one iteration of the agent's core loop.
    ///
    /// A typical step involves:
    /// 1. Querying the LLM for the next action.
    /// 2. Parsing the model's response.
    /// 3. Executing the action in the environment (e.g. running bash commands).
    /// 4. Recording the new observations.
    ///
    /// Returns `StepOutcome::Continue` if the loop should proceed, or
    /// `StepOutcome::Terminate(ExitReason)` if the run is over.
    async fn step(&mut self) -> Result<StepOutcome, Error>;

    /// Repeatedly calls `step()` until the agent decides to terminate.
    ///
    /// This is the primary entrypoint for driving an agent.
    ///
    /// ## Examples
    ///
    /// ```no_run
    /// # use maxwells_daemon::agent::{Agent, StepOutcome, ExitReason};
    /// # use maxwells_daemon::error::Error;
    /// # use async_trait::async_trait;
    /// # struct MyAgent { steps: u32 }
    /// # #[async_trait]
    /// # impl Agent for MyAgent {
    /// #     async fn step(&mut self) -> Result<StepOutcome, Error> {
    /// #         Ok(StepOutcome::Terminate(ExitReason::UserInterrupt))
    /// #     }
    /// # }
    /// # async fn run_it() -> Result<(), Error> {
    /// let mut agent = MyAgent { steps: 0 };
    /// let reason = agent.run().await?;
    /// println!("Agent finished: {}", reason.label());
    /// # Ok(())
    /// # }
    /// ```
    async fn run(&mut self) -> Result<ExitReason, Error> {
        loop {
            match self.step().await? {
                StepOutcome::Continue => {}
                StepOutcome::Terminate(r) => return Ok(r),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_reason_labels() {
        let cases = vec![
            (
                ExitReason::Submitted {
                    final_output: "Done!".into(),
                },
                "submitted",
            ),
            (ExitReason::StepLimit { limit: 10 }, "step_limit"),
            (
                ExitReason::CostLimit {
                    limit_usd: 1.0,
                    spent_usd: 1.5,
                },
                "cost_limit",
            ),
            (
                ExitReason::BudgetExhausted {
                    limit_usd: 1.0,
                    spent_usd: 1.5,
                },
                "budget_exhausted",
            ),
            (ExitReason::UserInterrupt, "user_interrupt"),
            (
                ExitReason::ModelRefusal {
                    reason: "safety".into(),
                },
                "model_refusal",
            ),
            (
                ExitReason::AgentStagnation {
                    action_hash: "abc".into(),
                    count: 3,
                    window: 5,
                },
                "agent_stagnation",
            ),
            (
                ExitReason::HistoryCompactionFailed,
                "history_compaction_failed",
            ),
        ];

        for (reason, expected_label) in cases {
            assert_eq!(
                reason.label(),
                expected_label,
                "Label mismatch for {reason:?}",
            );
        }
    }
}
