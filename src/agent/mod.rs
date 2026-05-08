//! The `Agent` trait and supporting types.
//!
//! Termination is typed success, not error. Python used exceptions for
//! `Submitted` / `LimitsExceeded` because it lacks sum types; Rust has
//! them, so `run()` returns `Result<ExitReason, Error>` and any
//! `ExitReason` variant represents a clean finish.

use async_trait::async_trait;

use crate::error::Error;

pub mod default;
pub mod interactive;
pub mod parse;

pub use default::DefaultAgent;
pub use interactive::InteractiveAgent;
pub use parse::{Action, extract_action};

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
/// use rust_swe_agent::agent::ExitReason;
///
/// let exit = ExitReason::Submitted { final_output: "Done!".into() };
/// assert_eq!(exit.label(), "submitted");
/// ```
#[derive(Debug, Clone)]
pub enum ExitReason {
    /// The agent explicitly decided to submit a final answer.
    Submitted {
        /// The final output produced by the agent.
        final_output: String,
    },
    /// The agent reached its configured maximum number of allowed steps.
    StepLimit {
        /// The maximum limit that was reached.
        limit: u32,
    },
    /// The agent exceeded its configured maximum spend.
    CostLimit {
        /// The configured limit in USD.
        limit_usd: f64,
        /// The actual amount spent in USD.
        spent_usd: f64,
    },
    /// The agent's per-task USD budget was exhausted mid-loop.
    BudgetExhausted {
        /// The per-task budget limit in USD.
        limit_usd: f64,
        /// The actual amount spent in USD.
        spent_usd: f64,
    },
    /// A human explicitly cancelled the run.
    UserInterrupt,
    /// The LLM backend refused to complete the prompt (e.g. safety filters).
    ModelRefusal {
        /// The reason provided by the model for refusal.
        reason: String,
    },
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
    /// use rust_swe_agent::agent::ExitReason;
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
/// use rust_swe_agent::agent::{StepOutcome, ExitReason};
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
/// use rust_swe_agent::agent::{Agent, StepOutcome, ExitReason};
/// use rust_swe_agent::error::Error;
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
    /// # use rust_swe_agent::agent::{Agent, StepOutcome, ExitReason};
    /// # use rust_swe_agent::error::Error;
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
