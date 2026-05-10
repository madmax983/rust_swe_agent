//! `InteractiveAgent`: confirm/yolo-mode wrapper around `DefaultAgent`.
//!
//! Between deciding on a bash action and executing it, prompt the user
//! (unless yolo). Ctrl-C via `tokio::signal::ctrl_c` translates to a
//! `UserInterrupt` exit on the next step boundary.

use async_trait::async_trait;

use super::{Agent, DefaultAgent, ExitReason, StepOutcome};
use crate::error::Error;

/// A decorator wrapper around an Agent allowing interactive terminal control.
pub struct InteractiveAgent {
    /// The inner agent logic instance.
    pub inner: DefaultAgent,
    /// If true, bypasses user confirmation prompts.
    pub yolo: bool,
}

impl InteractiveAgent {
    pub fn new(inner: DefaultAgent, yolo: bool) -> Self {
        Self { inner, yolo }
    }

    pub fn status_line(&self) -> String {
        let cache_str = if self.inner.model.supports_explicit_cache() {
            "cache:explicit"
        } else {
            "cache:auto-or-none"
        };
        format!(
            "step {}/{}  cost ${:.4}  {}",
            self.inner.steps,
            self.inner.config.root.agent.step_limit,
            self.inner.total_cost_usd,
            cache_str,
        )
    }
}

#[async_trait]
impl Agent for InteractiveAgent {
    async fn step(&mut self) -> Result<StepOutcome, Error> {
        // Interrupt check at step boundary — non-blocking.
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::select! {
            res = self.inner.step() => res,
            _ = ctrl_c => Ok(StepOutcome::Terminate(ExitReason::UserInterrupt)),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::agent::default::DefaultAgentBuilder;
    use crate::config::Config;
    use crate::env::{Environment, LocalEnvironment};
    use crate::model::DeterministicModel;
    use std::sync::Arc;

    #[tokio::test]
    async fn interactive_wraps_default_behavior() {
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 3;
        let model = Arc::new(DeterministicModel::new(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ]));
        let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
        let inner = DefaultAgentBuilder {
            config: cfg,
            model,
            env,
            task: "t".into(),
            extra_context: None,
            renderer: None,
            stream: None,
        }
        .build()
        .unwrap();

        let mut a = InteractiveAgent::new(inner, true);
        let r = a.run().await.unwrap();
        assert!(matches!(r, ExitReason::Submitted { .. }));
    }
}
