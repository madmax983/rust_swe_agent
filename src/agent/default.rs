//! `DefaultAgent`: the port of mini-swe-agent's ~100-line `agents/default.py`.
//!
//! Loop:
//!   1. Limit check → Terminate if exceeded
//!   2. Retag cache hints on history
//!   3. model.query
//!   4. parse::extract_action
//!   5. env.run
//!   6. observation template → push as user message, record in trajectory
//!   7. bump steps, Continue

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

use super::{Action, Agent, ExitReason, StepOutcome, extract_action};
use crate::config::Config;
use crate::env::{Environment, RunRequest};
use crate::error::Error;
use crate::model::{CacheHint, Message, MessageExtra, Model, QueryOpts, Role};
use crate::template::Renderer;
use crate::trajectory::Trajectory;

pub struct DefaultAgent {
    pub config: Config,
    pub model: Arc<dyn Model>,
    pub env: Box<dyn Environment>,
    pub renderer: Arc<Renderer>,
    pub history: Vec<Message>,
    pub trajectory: Trajectory,
    pub steps: u32,
    pub total_cost_usd: f64,
}

pub struct DefaultAgentBuilder {
    pub config: Config,
    pub model: Arc<dyn Model>,
    pub env: Box<dyn Environment>,
    pub task: String,
    pub extra_context: Option<String>,
    pub renderer: Option<Arc<Renderer>>,
}

impl DefaultAgentBuilder {
    pub fn build(self) -> Result<DefaultAgent, Error> {
        let renderer = self.renderer.unwrap_or_else(|| Arc::new(Renderer::new()));

        let system_rendered = renderer.render_str(
            &self.config.root.prompts.system,
            &serde_json::json!({
                "task": self.task,
                "extra_context": self.extra_context,
            }),
        )?;
        let instance_rendered = renderer.render_str(
            &self.config.root.prompts.instance,
            &serde_json::json!({
                "task": self.task,
                "extra_context": self.extra_context,
            }),
        )?;

        let history = vec![
            Message::system(system_rendered),
            Message::user(instance_rendered),
        ];

        let mut trajectory = Trajectory::new();
        trajectory.info.task = Some(self.task.clone());
        trajectory.info.model_name = Some(self.model.name().to_owned());
        trajectory.info.started_at = Some(chrono::Utc::now().to_rfc3339());
        for m in &history {
            trajectory.record_message(m);
        }

        Ok(DefaultAgent {
            config: self.config,
            model: self.model,
            env: self.env,
            renderer,
            history,
            trajectory,
            steps: 0,
            total_cost_usd: 0.0,
        })
    }
}

/// Retag cache hints on history following the "rolling observation window"
/// policy:
/// * System prompt (index 0, role System) → Breakpoint.
/// * The observation at (history.len() - 2), if a User/Tool message → Auto.
/// * Everything else → None.
///
/// The backend enforces Anthropic's 4-breakpoint cap; the agent stays naive.
pub fn retag_cache_hints(history: &mut [Message]) {
    for (i, m) in history.iter_mut().enumerate() {
        m.cache_hint = match (i, m.role) {
            (0, Role::System) => CacheHint::Breakpoint,
            _ => CacheHint::None,
        };
    }
    // Rolling auto marker: second-from-last, if it's a User/Tool obs.
    if history.len() >= 2 {
        let idx = history.len() - 2;
        if matches!(history[idx].role, Role::User | Role::Tool) {
            history[idx].cache_hint = CacheHint::Auto;
        }
    }
}

#[async_trait]
impl Agent for DefaultAgent {
    async fn step(&mut self) -> Result<StepOutcome, Error> {
        // 1. Limit checks.
        if self.steps >= self.config.root.agent.step_limit {
            self.trajectory.info.exit_reason = Some("step_limit".into());
            self.trajectory.info.steps = Some(self.steps);
            return Ok(StepOutcome::Terminate(ExitReason::StepLimit {
                limit: self.config.root.agent.step_limit,
            }));
        }
        if let Some(limit) = self.config.root.agent.cost_limit_usd {
            if self.total_cost_usd >= limit {
                self.trajectory.info.exit_reason = Some("cost_limit".into());
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                return Ok(StepOutcome::Terminate(ExitReason::CostLimit {
                    limit_usd: limit,
                    spent_usd: self.total_cost_usd,
                }));
            }
        }

        // 2. Retag cache hints (one line; backend handles capping).
        retag_cache_hints(&mut self.history);

        // 3. model.query.
        let opts = QueryOpts {
            temperature: self.config.root.model.temperature,
            max_tokens: Some(self.config.root.model.max_tokens),
            extra: serde_json::Map::new(),
        };
        let resp = self.model.query(&self.history, &opts).await?;
        self.total_cost_usd += resp.usage.cost_usd.unwrap_or(0.0);

        // Record assistant message in trajectory with raw + cost.
        let mut asst = Message::assistant(resp.content.clone());
        asst.extra.cost = resp.usage.cost_usd;
        asst.extra.response = Some(resp.raw.clone());
        asst.extra.timestamp = Some(chrono::Utc::now().to_rfc3339());

        // 4. Parse action.
        let action = extract_action(&resp.content);
        match &action {
            Action::Submit(output) => {
                asst.extra.actions = Some(vec!["__SUBMIT__".into()]);
                self.history.push(Message::assistant(resp.content.clone()));
                self.trajectory.record_message(&asst);
                self.trajectory.info.exit_reason = Some("submitted".into());
                self.trajectory.info.final_output = Some(output.clone());
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
                return Ok(StepOutcome::Terminate(ExitReason::Submitted {
                    final_output: output.clone(),
                }));
            }
            Action::Bash(cmd) => {
                asst.extra.actions = Some(vec![cmd.clone()]);
            }
            Action::None => {
                self.history.push(Message::assistant(resp.content.clone()));
                self.trajectory.record_message(&asst);
                // Observation = format_error_template, verbatim (no vars in
                // default template, but we still render to pick up any
                // future placeholders).
                let err = self.renderer.render_str(
                    &self.config.root.agent.format_error_template,
                    &serde_json::json!({}),
                )?;
                let obs = Message::user(err);
                self.history.push(obs.clone());
                self.trajectory.record_message(&obs);
                self.steps += 1;
                return Ok(StepOutcome::Continue);
            }
        }

        // 5. env.run.
        let Action::Bash(cmd) = action else {
            unreachable!("Submit and None handled above");
        };
        let run_req = RunRequest::new(&cmd).with_timeout(Duration::from_secs(
            self.config.root.environment.timeout_secs,
        ));
        let result = self.env.run(run_req).await?;

        // 6. Render observation.
        let obs_text = self.renderer.render_str(
            &self.config.root.agent.observation_template,
            &serde_json::json!({
                "returncode": result.exit_code,
                "output": result.combined_output(),
                "stdout": result.stdout,
                "stderr": result.stderr,
                "timed_out": result.timed_out,
            }),
        )?;

        // Record assistant turn in history & trajectory.
        self.history.push(Message::assistant(resp.content.clone()));
        self.trajectory.record_message(&asst);

        // Record user observation.
        let obs_msg = Message::user(obs_text);
        self.history.push(obs_msg.clone());
        let mut obs_extra = MessageExtra::default();
        obs_extra.other.insert(
            "run_result".into(),
            serde_json::to_value(&result).unwrap_or(serde_json::Value::Null),
        );
        obs_extra.timestamp = Some(chrono::Utc::now().to_rfc3339());
        self.trajectory.record_with_extra(&obs_msg, obs_extra);

        self.steps += 1;
        Ok(StepOutcome::Continue)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::env::LocalEnvironment;
    use crate::model::DeterministicModel;

    fn make_agent(responses: Vec<String>) -> DefaultAgent {
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let model = Arc::new(DeterministicModel::new(responses));
        let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
        DefaultAgentBuilder {
            config: cfg,
            model,
            env,
            task: "test".into(),
            extra_context: None,
            renderer: None,
        }
        .build()
        .unwrap()
    }

    #[tokio::test]
    async fn submit_on_first_turn_terminates() {
        let mut a = make_agent(vec![
            "done\nCOMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
        ]);
        let exit = a.run().await.unwrap();
        match exit {
            ExitReason::Submitted { final_output } => assert_eq!(final_output, "final"),
            other => panic!("wrong exit: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bash_then_submit_produces_observation() {
        let mut a = make_agent(vec![
            "```bash\necho xyz\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nall good\n```".into(),
        ]);
        let exit = a.run().await.unwrap();
        assert!(matches!(exit, ExitReason::Submitted { .. }));
        // History: [system, instance, asst(bash), user(obs), asst(submit)]
        assert!(a.history.iter().any(|m| m.content.contains("xyz")));
    }

    #[tokio::test]
    async fn step_limit_terminates() {
        let mut a = make_agent(vec![
            "```bash\necho 1\n```".into(),
            "```bash\necho 2\n```".into(),
            "```bash\necho 3\n```".into(),
            "```bash\necho 4\n```".into(),
            "```bash\necho 5\n```".into(),
            "```bash\necho 6\n```".into(),
        ]);
        let exit = a.run().await.unwrap();
        assert!(matches!(exit, ExitReason::StepLimit { limit: 5 }));
    }

    #[tokio::test]
    async fn malformed_response_triggers_format_error() {
        let mut a = make_agent(vec![
            "I'll think about it.".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\n\n```".into(),
        ]);
        let exit = a.run().await.unwrap();
        assert!(matches!(exit, ExitReason::Submitted { .. }));
        // format_error_template got rendered as a user message.
        assert!(
            a.history
                .iter()
                .any(|m| m.content.contains("did not include a shell command"))
        );
    }

    #[test]
    fn retag_marks_system_breakpoint_and_rolling_auto() {
        let mut h = vec![
            Message::system("sys"),
            Message::user("instance"),
            Message::assistant("resp1"),
            Message::user("obs1"),
            Message::assistant("resp2"),
            Message::user("obs2"),
        ];
        retag_cache_hints(&mut h);
        assert!(matches!(h[0].cache_hint, CacheHint::Breakpoint));
        // second-to-last is h[4] (assistant resp2): not User/Tool, skip.
        assert!(matches!(h[4].cache_hint, CacheHint::None));

        // Pop the last, now second-to-last is obs1 (User).
        h.pop();
        retag_cache_hints(&mut h);
        assert!(matches!(h[3].cache_hint, CacheHint::Auto));
        assert!(matches!(h[0].cache_hint, CacheHint::Breakpoint));
    }
}
