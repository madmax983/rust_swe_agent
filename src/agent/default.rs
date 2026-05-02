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
use std::time::{Duration, Instant};

use super::{Action, Agent, ExitReason, StepOutcome, extract_action};
use crate::config::Config;
use crate::env::{Environment, RunRequest};
use crate::error::Error;
use crate::model::{CacheHint, Message, MessageExtra, Model, QueryOpts, Role};
use crate::stream::{NullSink, StreamEvent, StreamSink};
use crate::template::Renderer;
use crate::trajectory::{FailureCategory, TokenUsage, Trajectory, exit_reason, outcome};

pub struct DefaultAgent {
    pub config: Config,
    pub model: Arc<dyn Model>,
    pub env: Box<dyn Environment>,
    pub renderer: Arc<Renderer>,
    pub history: Vec<Message>,
    pub trajectory: Trajectory,
    pub steps: u32,
    pub total_cost_usd: f64,
    /// Wall-clock start, used to compute `duration_secs` on terminate.
    pub started_at_instant: Instant,
    /// Accumulated uncached input tokens across every model call in this run.
    pub prompt_tokens: u64,
    /// Accumulated prompt-cache reads across every model call in this run.
    pub cache_read_tokens: u64,
    /// Accumulated prompt-cache creations across every model call in this run.
    pub cache_creation_tokens: u64,
    /// Accumulated completion tokens across every model call in this run.
    pub completion_tokens: u64,
    /// Real-time event sink. Defaults to `NullSink` so non-streaming
    /// callers pay no cost beyond a vtable call.
    pub stream: Arc<dyn StreamSink>,
}

pub struct DefaultAgentBuilder {
    pub config: Config,
    pub model: Arc<dyn Model>,
    pub env: Box<dyn Environment>,
    pub task: String,
    pub extra_context: Option<String>,
    pub renderer: Option<Arc<Renderer>>,
    pub stream: Option<Arc<dyn StreamSink>>,
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

        let started_at = chrono::Utc::now().to_rfc3339();
        let mut trajectory = Trajectory::new();
        trajectory.info.task = Some(self.task.clone());
        trajectory.info.model_name = Some(self.model.name().to_owned());
        trajectory.info.started_at = Some(started_at.clone());
        for m in &history {
            trajectory.record_message(m);
        }

        let stream: Arc<dyn StreamSink> = self.stream.unwrap_or_else(|| Arc::new(NullSink));
        stream.emit(StreamEvent::RunStarted {
            task: self.task.clone(),
            model: self.model.name().to_owned(),
            started_at,
        });

        Ok(DefaultAgent {
            config: self.config,
            model: self.model,
            env: self.env,
            renderer,
            history,
            trajectory,
            steps: 0,
            total_cost_usd: 0.0,
            started_at_instant: Instant::now(),
            prompt_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            completion_tokens: 0,
            stream,
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
        if let Some(exit_reason) = self.check_limits() {
            return Ok(StepOutcome::Terminate(exit_reason));
        }

        // 2. Retag cache hints (one line; backend handles capping).
        retag_cache_hints(&mut self.history);

        // 3. model.query.
        let (resp_content, asst) = self.query_model().await?;

        // 4. Parse action and 5. env.run.
        let action = extract_action(&resp_content);
        if let Some(exit) = self.handle_action(action, resp_content, asst).await? {
            return Ok(StepOutcome::Terminate(exit));
        }

        self.steps += 1;
        Ok(StepOutcome::Continue)
    }
}

impl DefaultAgent {
    async fn handle_action(
        &mut self,
        action: Action,
        resp_content: String,
        mut asst: Message,
    ) -> Result<Option<ExitReason>, Error> {
        match action {
            Action::Submit(output) => {
                asst.extra.actions = Some(vec!["__SUBMIT__".into()]);
                self.history.push(Message::assistant(resp_content));
                self.trajectory.record_message(&asst);
                self.trajectory.info.exit_reason = Some("submitted".into());
                self.trajectory.info.failure_category = None;
                self.trajectory.info.final_output = Some(output.clone());
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
                self.finalize_run_metadata(outcome::SUBMITTED);
                self.emit_run_ended("submitted", None, Some(output.clone()));
                Ok(Some(ExitReason::Submitted {
                    final_output: output,
                }))
            }
            Action::None => {
                self.trajectory
                    .info
                    .failure_category
                    .get_or_insert(FailureCategory::ModelParse);
                self.history.push(Message::assistant(resp_content));
                self.trajectory.record_message(&asst);

                let err = self.renderer.render_str(
                    &self.config.root.agent.format_error_template,
                    &serde_json::json!({}),
                )?;
                self.stream.emit(StreamEvent::FormatError {
                    step: self.steps,
                    content: err.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
                let obs = Message::user(err);
                self.history.push(obs.clone());
                self.trajectory.record_message(&obs);

                Ok(None)
            }
            Action::Bash(cmd) => {
                asst.extra.actions = Some(vec![cmd.clone()]);
                self.execute_action(cmd, resp_content, asst).await?;
                Ok(None)
            }
        }
    }

    async fn query_model(&mut self) -> Result<(String, Message), Error> {
        let opts = QueryOpts {
            temperature: self.config.root.model.temperature,
            max_tokens: Some(self.config.root.model.max_tokens),
            extra: serde_json::Map::new(),
        };
        let resp = self.model.query(&self.history, &opts).await?;
        self.total_cost_usd += resp.usage.cost_usd.unwrap_or(0.0);
        self.prompt_tokens = self.prompt_tokens.saturating_add(resp.usage.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(resp.usage.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(resp.usage.cache_creation_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(resp.usage.output_tokens);

        let asst_ts = chrono::Utc::now().to_rfc3339();
        let mut asst = Message::assistant(resp.content.clone());
        asst.extra.cost = resp.usage.cost_usd;
        asst.extra.response = Some(resp.raw.clone());
        asst.extra.timestamp = Some(asst_ts.clone());

        self.stream.emit(StreamEvent::AssistantMessage {
            step: self.steps,
            content: resp.content.clone(),
            cost_usd: resp.usage.cost_usd,
            timestamp: asst_ts,
        });

        Ok((resp.content, asst))
    }

    async fn execute_action(
        &mut self,
        cmd: String,
        resp_content: String,
        asst: Message,
    ) -> Result<(), Error> {
        self.stream.emit(StreamEvent::BashStart {
            step: self.steps,
            command: cmd.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        });
        let run_req = RunRequest::new(&cmd).with_timeout(Duration::from_secs(
            self.config.root.environment.timeout_secs,
        ));
        let result = self.env.run(run_req).await?;
        self.stream.emit(StreamEvent::BashResult {
            step: self.steps,
            exit_code: result.exit_code,
            stdout: result.stdout.clone(),
            stderr: result.stderr.clone(),
            timed_out: result.timed_out,
            timestamp: chrono::Utc::now().to_rfc3339(),
        });

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
        self.history.push(Message::assistant(resp_content));
        self.trajectory.record_message(&asst);

        // Record user observation.
        let obs_ts = chrono::Utc::now().to_rfc3339();
        let obs_msg = Message::user(obs_text.clone());
        self.history.push(obs_msg.clone());
        let mut obs_extra = MessageExtra::default();
        obs_extra.other.insert(
            "run_result".into(),
            serde_json::to_value(&result).unwrap_or(serde_json::Value::Null),
        );
        obs_extra.timestamp = Some(obs_ts.clone());
        self.trajectory.record_with_extra(&obs_msg, obs_extra);

        self.stream.emit(StreamEvent::Observation {
            step: self.steps,
            content: obs_text,
            timestamp: obs_ts,
        });

        Ok(())
    }

    fn check_limits(&mut self) -> Option<ExitReason> {
        if self.steps >= self.config.root.agent.step_limit {
            self.trajectory.info.exit_reason = Some("step_limit".into());
            self.trajectory.info.failure_category = Some(FailureCategory::StepLimit);
            self.trajectory.info.steps = Some(self.steps);
            self.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
            self.emit_run_ended("step_limit", Some(FailureCategory::StepLimit), None);
            return Some(ExitReason::StepLimit {
                limit: self.config.root.agent.step_limit,
            });
        }
        if let Some(limit) = self.config.root.agent.cost_limit_usd {
            if self.total_cost_usd >= limit {
                self.trajectory.info.exit_reason = Some("cost_limit".into());
                self.trajectory.info.failure_category = Some(FailureCategory::CostLimit);
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                self.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
                self.emit_run_ended("cost_limit", Some(FailureCategory::CostLimit), None);
                return Some(ExitReason::CostLimit {
                    limit_usd: limit,
                    spent_usd: self.total_cost_usd,
                });
            }
        }
        None
    }

    fn emit_run_ended(
        &self,
        exit_reason: &str,
        failure_category: Option<FailureCategory>,
        final_output: Option<String>,
    ) {
        self.stream.emit(StreamEvent::RunEnded {
            exit_reason: exit_reason.to_owned(),
            failure_category,
            final_output,
            steps: self.steps,
            total_cost_usd: self.total_cost_usd,
            ended_at: chrono::Utc::now().to_rfc3339(),
        });
    }

    /// Stamp the trajectory with the coarse `outcome`, accumulated
    /// `token_usage`, and wall-clock `duration_secs`. Call from every
    /// terminal path so every `.traj.json` carries these first-class
    /// fields without callers needing to remember.
    pub fn finalize_run_metadata(&mut self, outcome_label: &str) {
        self.trajectory.info.outcome = Some(outcome_label.to_owned());
        self.trajectory.info.token_usage = Some(TokenUsage {
            prompt_tokens: self.prompt_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            completion_tokens: self.completion_tokens,
        });
        self.trajectory.info.duration_secs = Some(self.started_at_instant.elapsed().as_secs_f64());
    }

    pub fn finalize_wallclock_timeout(&mut self, timeout: Duration) {
        self.trajectory.info.exit_reason = Some(exit_reason::WALLCLOCK_TIMEOUT.into());
        self.trajectory.info.failure_category = Some(FailureCategory::WallclockTimeout);
        self.trajectory.info.steps = Some(self.steps);
        self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
        self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
        self.trajectory.info.other.insert(
            "task_timeout_secs".into(),
            serde_json::json!(timeout.as_secs()),
        );
        self.finalize_run_metadata(outcome::ERROR);
        self.emit_run_ended(
            exit_reason::WALLCLOCK_TIMEOUT,
            Some(FailureCategory::WallclockTimeout),
            None,
        );
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
            stream: None,
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
    async fn submit_records_outcome_and_token_usage() {
        let mut a = make_agent(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
        ]);
        let _ = a.run().await.unwrap();
        assert_eq!(
            a.trajectory.info.outcome.as_deref(),
            Some(crate::trajectory::outcome::SUBMITTED)
        );
        // DeterministicModel zeroes its usage, so the totals are exactly 0/0.
        let usage = a.trajectory.info.token_usage.clone().unwrap();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        assert!(a.trajectory.info.duration_secs.unwrap() >= 0.0);
    }

    #[tokio::test]
    async fn step_limit_records_outcome() {
        let mut a = make_agent(vec![
            "```bash\necho 1\n```".into(),
            "```bash\necho 2\n```".into(),
            "```bash\necho 3\n```".into(),
            "```bash\necho 4\n```".into(),
            "```bash\necho 5\n```".into(),
            "```bash\necho 6\n```".into(),
        ]);
        let _ = a.run().await.unwrap();
        assert_eq!(
            a.trajectory.info.outcome.as_deref(),
            Some(crate::trajectory::outcome::STEP_LIMIT_REACHED)
        );
        assert!(a.trajectory.info.token_usage.is_some());
        assert!(a.trajectory.info.duration_secs.is_some());
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
