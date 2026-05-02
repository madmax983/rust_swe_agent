//! The `Agent` trait and supporting types.
//!
//! Termination is typed success, not error. Python used exceptions for
//! `Submitted` / `LimitsExceeded` because it lacks sum types; Rust has
//! them, so `run()` returns `Result<ExitReason, Error>` and any
//! `ExitReason` variant represents a clean finish.

use async_trait::async_trait;

use crate::error::Error;

pub mod default;
pub(crate) mod interactive;
pub mod parse;

pub use default::DefaultAgent;
pub use interactive::InteractiveAgent;
pub use parse::{Action, extract_action};

#[derive(Debug, Clone)]
pub enum ExitReason {
    Submitted { final_output: String },
    StepLimit { limit: u32 },
    CostLimit { limit_usd: f64, spent_usd: f64 },
    UserInterrupt,
    ModelRefusal { reason: String },
}

impl ExitReason {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Submitted { .. } => "submitted",
            Self::StepLimit { .. } => "step_limit",
            Self::CostLimit { .. } => "cost_limit",
            Self::UserInterrupt => "user_interrupt",
            Self::ModelRefusal { .. } => "model_refusal",
        }
    }
}

#[derive(Debug, Clone)]
pub enum StepOutcome {
    Continue,
    Terminate(ExitReason),
}

#[async_trait]
pub trait Agent: Send {
    async fn step(&mut self) -> Result<StepOutcome, Error>;

    async fn run(&mut self) -> Result<ExitReason, Error> {
        loop {
            match self.step().await? {
                StepOutcome::Continue => {}
                StepOutcome::Terminate(r) => return Ok(r),
            }
        }
    }
}
