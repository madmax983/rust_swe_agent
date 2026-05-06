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
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Action, Agent, ExitReason, StepOutcome, extract_action};
use crate::config::{Config, ToolHookCfg};
use crate::env::{CancellationToken, Environment, RunRequest, RunResult};
use crate::error::Error;
use crate::model::{CacheHint, Message, MessageExtra, Model, ModelResponse, QueryOpts, Role};
use crate::stream::{NullSink, StreamEvent, StreamSink};
use crate::template::Renderer;
use crate::trajectory::{
    FailureCategory, TestCommandPattern, TestInvocation, TokenUsage, Trajectory,
    detect_test_command, effective_test_command_patterns, exit_reason, outcome,
};

const MAX_TOOL_HOOK_ENV_VALUE_BYTES: usize = 1024;

#[derive(Debug, Clone)]
struct TruncateResult {
    text: String,
    bytes_omitted: usize,
    truncated: bool,
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn truncate_observation_text(input: &str, max_bytes: usize, head_ratio: f64) -> TruncateResult {
    if max_bytes == 0 {
        return TruncateResult {
            text: String::new(),
            bytes_omitted: input.len(),
            truncated: !input.is_empty(),
        };
    }
    if input.len() <= max_bytes {
        return TruncateResult {
            text: input.to_owned(),
            bytes_omitted: 0,
            truncated: false,
        };
    }
    let marker_base = "\n... [truncated] ...\n";
    let marker_budget = marker_base.len().min(max_bytes.saturating_sub(1));
    let content_budget = max_bytes.saturating_sub(marker_budget);
    let head_budget = ((content_budget as f64) * head_ratio.clamp(0.0, 1.0)).floor() as usize;
    let tail_budget = content_budget.saturating_sub(head_budget);
    let head_end = floor_char_boundary(input, head_budget.min(input.len()));
    let tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
    let (head_end, tail_start) = if tail_start < head_end {
        (head_end, head_end)
    } else {
        (head_end, tail_start)
    };
    let head = &input[..head_end];
    let tail = &input[tail_start..];
    let omitted = input.len().saturating_sub(head.len() + tail.len());
    let elided_lines = input[head_end..tail_start]
        .bytes()
        .filter(|b| *b == b'\n')
        .count();
    let marker = format!("\n... [truncated {omitted} bytes, {elided_lines} lines] ...\n");
    let marker = if marker.len() <= marker_budget {
        marker
    } else {
        marker_base[..floor_char_boundary(marker_base, marker_budget)].to_owned()
    };
    let text = format!("{head}{marker}{tail}");
    debug_assert!(text.len() <= max_bytes);
    TruncateResult {
        text,
        bytes_omitted: omitted,
        truncated: true,
    }
}

fn floor_char_boundary(input: &str, idx: usize) -> usize {
    let mut i = idx.min(input.len());
    while i > 0 && !input.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(input: &str, idx: usize) -> usize {
    let mut i = idx.min(input.len());
    while i < input.len() && !input.is_char_boundary(i) {
        i += 1;
    }
    i
}

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
    pub cancellation: Option<CancellationToken>,
    test_command_patterns: Vec<TestCommandPattern>,
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
        let test_command_patterns = effective_test_command_patterns(
            &self.config.root.agent.test_command_patterns,
            self.config.root.agent.test_command_patterns_replace,
        )
        .map_err(|err| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid agent.test_command_patterns regex: {err}"
            )))
        })?;
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
            cancellation: None,
            test_command_patterns,
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

async fn query_model_until_cancelled(
    model: &dyn Model,
    history: &[Message],
    opts: &QueryOpts,
    cancellation: Option<CancellationToken>,
) -> Result<Option<ModelResponse>, crate::error::ModelError> {
    let Some(mut cancellation) = cancellation else {
        return model.query(history, opts).await.map(Some);
    };
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    tokio::select! {
        result = model.query(history, opts) => result.map(Some),
        () = cancellation.cancelled() => Ok(None),
    }
}

#[async_trait]
impl Agent for DefaultAgent {
    // The step body walks through 7 sequential phases (limit checks →
    // model query → action parse → bash → observation → trajectory
    // record → bump). Splitting it out would obscure the linear flow
    // for no real reuse benefit.
    #[allow(clippy::too_many_lines)]
    async fn step(&mut self) -> Result<StepOutcome, Error> {
        if self.cancellation_requested() {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        }

        // 1. Limit checks.
        if self.steps >= self.config.root.agent.step_limit {
            self.trajectory.info.exit_reason = Some("step_limit".into());
            self.trajectory.info.failure_category = Some(FailureCategory::StepLimit);
            self.trajectory.info.steps = Some(self.steps);
            self.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
            self.emit_run_ended("step_limit", Some(FailureCategory::StepLimit), None);
            return Ok(StepOutcome::Terminate(ExitReason::StepLimit {
                limit: self.config.root.agent.step_limit,
            }));
        }
        if let Some(limit) = self.config.root.agent.cost_limit_usd {
            if self.total_cost_usd >= limit {
                self.trajectory.info.exit_reason = Some("cost_limit".into());
                self.trajectory.info.failure_category = Some(FailureCategory::CostLimit);
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                // Cost limit is also a resource limit; map to the same
                // coarse outcome as step limit per the three-value spec.
                self.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
                self.emit_run_ended("cost_limit", Some(FailureCategory::CostLimit), None);
                return Ok(StepOutcome::Terminate(ExitReason::CostLimit {
                    limit_usd: limit,
                    spent_usd: self.total_cost_usd,
                }));
            }
        }
        if let Some(limit) = self.config.root.agent.per_task_budget_usd {
            if self.total_cost_usd >= limit {
                self.trajectory.info.exit_reason = Some("budget_exhausted".into());
                self.trajectory.info.failure_category = Some(FailureCategory::BudgetExhausted);
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
                self.finalize_run_metadata(outcome::BUDGET_EXHAUSTED);
                self.emit_run_ended(
                    "budget_exhausted",
                    Some(FailureCategory::BudgetExhausted),
                    None,
                );
                return Ok(StepOutcome::Terminate(ExitReason::BudgetExhausted {
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
        let Some(resp) = query_model_until_cancelled(
            self.model.as_ref(),
            &self.history,
            &opts,
            self.cancellation.clone(),
        )
        .await?
        else {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        };
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

        if self.cancellation_requested() {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        }

        // Record assistant message in trajectory with raw + cost.
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

        // 4. Parse action.
        let action = extract_action(&resp.content);
        match &action {
            Action::Submit(output) => {
                asst.extra.actions = Some(vec!["__SUBMIT__".into()]);
                self.history.push(Message::assistant(resp.content.clone()));
                self.trajectory.record_message(&asst);
                self.trajectory.info.exit_reason = Some("submitted".into());
                self.trajectory.info.failure_category = None;
                self.trajectory.info.final_output = Some(output.clone());
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
                self.finalize_run_metadata(outcome::SUBMITTED);
                self.emit_run_ended("submitted", None, Some(output.clone()));
                return Ok(StepOutcome::Terminate(ExitReason::Submitted {
                    final_output: output.clone(),
                }));
            }
            Action::Bash(cmd) => {
                asst.extra.actions = Some(vec![cmd.clone()]);
            }
            Action::Ripgrep(args) => {
                asst.extra.actions = Some(vec![ripgrep_command(args)]);
            }
            Action::None => {
                // Keep a breadcrumb that at least one model response could
                // not be parsed into a valid action. Terminal limit checks
                // above still take precedence if the run eventually ends on
                // step/cost exhaustion.
                self.trajectory
                    .info
                    .failure_category
                    .get_or_insert(FailureCategory::ModelParse);
                self.history.push(Message::assistant(resp.content.clone()));
                self.trajectory.record_message(&asst);
                // Observation = format_error_template, verbatim (no vars in
                // default template, but we still render to pick up any
                // future placeholders).
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
                self.steps += 1;
                return Ok(StepOutcome::Continue);
            }
        }

        // 5. Determine which tool to run and build the shell command.
        let (tool_name, run_command) = match action {
            Action::Bash(cmd) => ("bash", cmd),
            Action::Ripgrep(args) => ("ripgrep", ripgrep_command(&args)),
            Action::Submit(_) | Action::None => unreachable!("handled above"),
        };

        // 5a. PreToolUse hooks, then env.run if not blocked.
        let pre_hook_results = self
            .run_tool_hooks(
                ToolHookPhase::PreToolUse,
                &self.config.root.agent.hooks.pre_tool_use,
                tool_name,
                &run_command,
                None,
            )
            .await?;
        let tool_use_blocked = pre_hook_results.iter().any(ToolHookResult::blocks_tool_use);
        if self.cancellation_requested() {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        }

        let (result, post_hook_results) = if tool_use_blocked {
            (blocked_run_result(&pre_hook_results), Vec::new())
        } else {
            match tool_name {
                "bash" => self.stream.emit(StreamEvent::BashStart {
                    step: self.steps,
                    command: run_command.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                }),
                "ripgrep" => self.stream.emit(StreamEvent::RipgrepStart {
                    step: self.steps,
                    command: run_command.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                }),
                _ => {}
            }
            let run_req = RunRequest::new(&run_command).with_timeout(Duration::from_secs(
                self.config.root.environment.timeout_secs,
            ));
            let run_req = if let Some(cancellation) = self.cancellation.clone() {
                run_req.with_cancellation(cancellation)
            } else {
                run_req
            };
            let result = self.env.run(run_req).await?;
            match tool_name {
                "bash" => self.stream.emit(StreamEvent::BashResult {
                    step: self.steps,
                    exit_code: result.exit_code,
                    stdout: result.stdout.clone(),
                    stderr: result.stderr.clone(),
                    timed_out: result.timed_out,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                }),
                "ripgrep" => self.stream.emit(StreamEvent::RipgrepResult {
                    step: self.steps,
                    exit_code: result.exit_code,
                    stdout: result.stdout.clone(),
                    stderr: result.stderr.clone(),
                    timed_out: result.timed_out,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                }),
                _ => {}
            }
            let post_hook_results = self
                .run_tool_hooks(
                    ToolHookPhase::PostToolUse,
                    &self.config.root.agent.hooks.post_tool_use,
                    tool_name,
                    &run_command,
                    Some(&result),
                )
                .await?;
            (result, post_hook_results)
        };

        if !tool_use_blocked {
            self.record_test_invocation_if_matched(&run_command, result.exit_code);
        }

        let trunc_stdout = truncate_observation_text(
            &result.stdout,
            self.config.root.agent.observation_max_bytes,
            self.config.root.agent.observation_head_ratio,
        );
        let trunc_stderr = truncate_observation_text(
            &result.stderr,
            self.config.root.agent.observation_max_bytes,
            self.config.root.agent.observation_head_ratio,
        );
        let merged_output = match (trunc_stdout.text.is_empty(), trunc_stderr.text.is_empty()) {
            (true, true) => String::new(),
            (false, true) => trunc_stdout.text.clone(),
            (true, false) => trunc_stderr.text.clone(),
            (false, false) => format!("{}\n{}", trunc_stdout.text, trunc_stderr.text),
        };
        let trunc_output = truncate_observation_text(
            &merged_output,
            self.config.root.agent.observation_max_bytes,
            self.config.root.agent.observation_head_ratio,
        );
        let pre_hook_results_for_observation = truncate_hook_results_for_observation(
            &pre_hook_results,
            self.config.root.agent.observation_max_bytes,
            self.config.root.agent.observation_head_ratio,
        );
        let post_hook_results_for_observation = truncate_hook_results_for_observation(
            &post_hook_results,
            self.config.root.agent.observation_max_bytes,
            self.config.root.agent.observation_head_ratio,
        );
        // 6. Render observation.
        let obs_text = self.renderer.render_str(
            &self.config.root.agent.observation_template,
            &serde_json::json!({
                "returncode": result.exit_code,
                "output": trunc_output.text,
                "stdout": trunc_stdout.text,
                "stderr": trunc_stderr.text,
                "timed_out": result.timed_out,
                "command": run_command,
                "step": self.steps,
                "tool_use_blocked": tool_use_blocked,
                "pre_tool_use_hooks": pre_hook_results_for_observation,
                "post_tool_use_hooks": post_hook_results_for_observation,
            }),
        )?;

        // 6b. Optionally append the budget block.
        let obs_text = self.append_budget_block(obs_text)?;

        // Record assistant turn in history & trajectory.
        self.history.push(Message::assistant(resp.content.clone()));
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
        obs_extra.other.insert(
            "pre_tool_use_hooks".into(),
            serde_json::to_value(&pre_hook_results)?,
        );
        obs_extra.other.insert(
            "post_tool_use_hooks".into(),
            serde_json::to_value(&post_hook_results)?,
        );
        obs_extra.other.insert(
            "tool_use_blocked".into(),
            serde_json::Value::Bool(tool_use_blocked),
        );
        obs_extra.other.insert(
            "observation_truncated".into(),
            serde_json::json!(
                trunc_stdout.truncated || trunc_stderr.truncated || trunc_output.truncated
            ),
        );
        obs_extra.other.insert(
            "stdout_bytes_omitted".into(),
            serde_json::json!(trunc_stdout.bytes_omitted),
        );
        obs_extra.other.insert(
            "stderr_bytes_omitted".into(),
            serde_json::json!(trunc_stderr.bytes_omitted),
        );
        obs_extra.other.insert(
            "output_bytes_omitted".into(),
            serde_json::json!(trunc_output.bytes_omitted),
        );
        obs_extra.timestamp = Some(obs_ts.clone());
        self.trajectory.record_with_extra(&obs_msg, obs_extra);

        self.stream.emit(StreamEvent::Observation {
            step: self.steps,
            content: obs_text,
            timestamp: obs_ts,
        });

        self.steps += 1;
        if self.cancellation_requested() {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        }
        Ok(StepOutcome::Continue)
    }
}

impl DefaultAgent {
    fn cancellation_requested(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
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
        self.refresh_test_metadata();
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

    pub fn finalize_cancelled(&mut self) {
        self.trajectory.info.exit_reason = Some(exit_reason::CANCELLED.into());
        self.trajectory.info.failure_category = None;
        self.trajectory.info.steps = Some(self.steps);
        self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
        self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
        self.finalize_run_metadata(outcome::ERROR);
        self.emit_run_ended(exit_reason::CANCELLED, None, None);
    }

    #[allow(clippy::cast_precision_loss)]
    fn append_budget_block(&self, obs: String) -> Result<String, Error> {
        let Some(limit) = self.config.root.agent.per_task_budget_usd else {
            return Ok(obs);
        };
        if self.config.root.agent.hide_budget_from_agent {
            return Ok(obs);
        }
        let remaining_pct = if limit > 0.0 {
            ((limit - self.total_cost_usd) / limit * 100.0).max(0.0)
        } else {
            0.0
        };
        let block = self.renderer.render_str(
            &self.config.root.agent.budget_block_template,
            &serde_json::json!({
                "budget_used": format!("{:.4}", self.total_cost_usd),
                "budget_limit": format!("{:.4}", limit),
                "budget_remaining_pct": format!("{:.0}", remaining_pct),
                "turn": self.steps + 1,
                "max_turns": self.config.root.agent.step_limit,
            }),
        )?;
        Ok(format!("{obs}{block}"))
    }

    fn record_test_invocation_if_matched(&mut self, command: &str, exit_code: i32) {
        if let Some(matched_pattern) = detect_test_command(command, &self.test_command_patterns) {
            self.trajectory.info.test_invocations.push(TestInvocation {
                step_index: self.steps,
                command: command.to_owned(),
                exit_code,
                matched_pattern,
            });
        }
    }

    fn refresh_test_metadata(&mut self) {
        let submit_step = self
            .trajectory
            .messages
            .iter()
            .any(message_is_submit_action)
            .then_some(self.trajectory.info.steps)
            .flatten();
        let last_pre_submit = submit_step.and_then(|step| {
            self.trajectory
                .info
                .test_invocations
                .iter()
                .rev()
                .find(|invocation| invocation.step_index < step)
        });
        self.trajectory.info.tests_run_before_submit = last_pre_submit.is_some();
        self.trajectory.info.last_tests_passed =
            last_pre_submit.map(|invocation| invocation.exit_code == 0);
    }

    async fn run_tool_hooks(
        &self,
        phase: ToolHookPhase,
        hooks: &[ToolHookCfg],
        tool_name: &str,
        command: &str,
        result: Option<&RunResult>,
    ) -> Result<Vec<ToolHookResult>, Error> {
        let mut reports = Vec::new();
        for hook in hooks {
            reports.push(
                self.run_tool_hook(phase, hook, tool_name, command, result)
                    .await?,
            );
        }
        Ok(reports)
    }

    async fn run_tool_hook(
        &self,
        phase: ToolHookPhase,
        hook: &ToolHookCfg,
        tool_name: &str,
        command: &str,
        result: Option<&RunResult>,
    ) -> Result<ToolHookResult, Error> {
        let context = self.tool_hook_context(phase, hook, tool_name, command, result);
        let rendered_command = self.renderer.render_str(&hook.command, &context)?;
        let timeout_secs = hook
            .timeout_secs
            .unwrap_or(self.config.root.agent.tool_hook_timeout_secs);
        let mut req = RunRequest::new(rendered_command.clone())
            .with_timeout(Duration::from_secs(timeout_secs));
        if let Some(cancellation) = self.cancellation.clone() {
            req = req.with_cancellation(cancellation);
        }
        req.env = tool_hook_env(&context)?;
        let hook_result = match self.env.run(req).await {
            Ok(result) => result,
            Err(err) => {
                if matches!(phase, ToolHookPhase::PreToolUse) {
                    return Err(err.into());
                }
                let message = format!("hook environment error: {err}");
                return Ok(ToolHookResult {
                    phase,
                    name: hook.name.clone(),
                    command: rendered_command,
                    stdout: String::new(),
                    stderr: message.clone(),
                    output: message,
                    exit_code: -1,
                    timed_out: false,
                });
            }
        };

        Ok(ToolHookResult {
            phase,
            name: hook.name.clone(),
            command: rendered_command,
            stdout: hook_result.stdout.clone(),
            stderr: hook_result.stderr.clone(),
            output: hook_result.combined_output(),
            exit_code: hook_result.exit_code,
            timed_out: hook_result.timed_out,
        })
    }

    fn tool_hook_context(
        &self,
        phase: ToolHookPhase,
        hook: &ToolHookCfg,
        tool_name: &str,
        command: &str,
        result: Option<&RunResult>,
    ) -> serde_json::Value {
        let task = self.trajectory.info.task.as_deref().unwrap_or_default();
        let model = self
            .trajectory
            .info
            .model_name
            .as_deref()
            .unwrap_or_default();
        let returncode = result.map_or(serde_json::Value::Null, |r| {
            serde_json::Value::Number(r.exit_code.into())
        });
        let stdout = result.map_or("", |r| r.stdout.as_str());
        let stderr = result.map_or("", |r| r.stderr.as_str());
        let output = result.map_or_else(String::new, RunResult::combined_output);
        let timed_out = result.is_some_and(|r| r.timed_out);
        serde_json::json!({
            "hook": {
                "phase": phase.as_str(),
                "name": hook.name,
            },
            "tool": {
                "name": tool_name,
            },
            "task": task,
            "model": model,
            "step": self.steps,
            "command": command,
            "returncode": returncode,
            "stdout": stdout,
            "stderr": stderr,
            "output": output,
            "timed_out": timed_out,
            "total_cost_usd": self.total_cost_usd,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
struct ToolHookResult {
    phase: ToolHookPhase,
    name: String,
    command: String,
    stdout: String,
    stderr: String,
    output: String,
    exit_code: i32,
    timed_out: bool,
}

impl ToolHookResult {
    const fn blocks_tool_use(&self) -> bool {
        matches!(self.phase, ToolHookPhase::PreToolUse) && (self.exit_code != 0 || self.timed_out)
    }

    fn truncated_for_observation(&self, max_bytes: usize, head_ratio: f64) -> Self {
        Self {
            phase: self.phase,
            name: self.name.clone(),
            command: self.command.clone(),
            stdout: truncate_observation_text(&self.stdout, max_bytes, head_ratio).text,
            stderr: truncate_observation_text(&self.stderr, max_bytes, head_ratio).text,
            output: truncate_observation_text(&self.output, max_bytes, head_ratio).text,
            exit_code: self.exit_code,
            timed_out: self.timed_out,
        }
    }
}

fn truncate_hook_results_for_observation(
    results: &[ToolHookResult],
    max_bytes: usize,
    head_ratio: f64,
) -> Vec<ToolHookResult> {
    results
        .iter()
        .map(|result| result.truncated_for_observation(max_bytes, head_ratio))
        .collect()
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ToolHookPhase {
    PreToolUse,
    PostToolUse,
}

impl ToolHookPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
        }
    }
}

/// Build the shell command for a ripgrep action.
///
/// Multiline args are joined with spaces so a block like:
///
/// ```text
/// --type rust
/// "fn main" src/
/// ```
///
/// becomes `rg --color never --type rust "fn main" src/` rather than two
/// shell commands separated by a newline. We intentionally do not apply
/// further shell-quoting (e.g. via shell_words) because that would strip
/// backslashes from regex patterns — `\bTODO\b` would become `bTODOb`.
/// The ripgrep block has the same trust level as a bash block; `pre_tool_use`
/// hooks are the appropriate layer for deployments that need tighter control.
fn ripgrep_command(args: &str) -> String {
    let normalized = args
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    format!("rg --color never {normalized}")
}

fn blocked_run_result(pre_hook_results: &[ToolHookResult]) -> RunResult {
    let mut stderr = "tool use blocked by PreToolUse hook".to_owned();
    if let Some(hook) = pre_hook_results
        .iter()
        .find(|result| result.blocks_tool_use())
    {
        stderr.push_str(": ");
        stderr.push_str(&hook.name);
    }
    RunResult {
        stdout: String::new(),
        stderr,
        exit_code: 126,
        timed_out: false,
    }
}

fn message_is_submit_action(message: &crate::trajectory::MessageRecord) -> bool {
    message
        .extra
        .actions
        .as_ref()
        .is_some_and(|actions| actions.iter().any(|action| action == "__SUBMIT__"))
}

fn tool_hook_env(context: &serde_json::Value) -> Result<BTreeMap<String, String>, Error> {
    let mut env = BTreeMap::new();
    let env_context = capped_env_context(context);
    insert_json_str(
        &mut env,
        "RUST_SWE_AGENT_HOOK_NAME",
        &env_context["hook"]["name"],
    );
    insert_json_str(
        &mut env,
        "RUST_SWE_AGENT_HOOK_PHASE",
        &env_context["hook"]["phase"],
    );
    insert_json_str(
        &mut env,
        "RUST_SWE_AGENT_TOOL_NAME",
        &env_context["tool"]["name"],
    );
    insert_json_str(&mut env, "RUST_SWE_AGENT_TASK", &env_context["task"]);
    insert_json_str(&mut env, "RUST_SWE_AGENT_MODEL", &env_context["model"]);
    insert_json_str(&mut env, "RUST_SWE_AGENT_STEP", &env_context["step"]);
    insert_json_str_untruncated(&mut env, "RUST_SWE_AGENT_COMMAND", &context["command"]);
    insert_json_str(
        &mut env,
        "RUST_SWE_AGENT_EXIT_CODE",
        &env_context["returncode"],
    );
    insert_json_str(&mut env, "RUST_SWE_AGENT_STDOUT", &env_context["stdout"]);
    insert_json_str(&mut env, "RUST_SWE_AGENT_STDERR", &env_context["stderr"]);
    insert_json_str(&mut env, "RUST_SWE_AGENT_OUTPUT", &env_context["output"]);
    insert_json_str(
        &mut env,
        "RUST_SWE_AGENT_TIMED_OUT",
        &env_context["timed_out"],
    );
    insert_json_str(
        &mut env,
        "RUST_SWE_AGENT_TOTAL_COST_USD",
        &env_context["total_cost_usd"],
    );
    env.insert(
        "RUST_SWE_AGENT_CONTEXT_JSON".into(),
        serde_json::to_string(&env_context)?,
    );
    Ok(env)
}

fn insert_json_str(env: &mut BTreeMap<String, String>, key: &str, value: &serde_json::Value) {
    env.insert(key.into(), truncate_for_hook_env(&json_str(value)));
}

fn insert_json_str_untruncated(
    env: &mut BTreeMap<String, String>,
    key: &str,
    value: &serde_json::Value,
) {
    env.insert(key.into(), json_str(value));
}

fn json_str(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn capped_env_context(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(truncate_for_hook_env(s)),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(capped_env_context).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), capped_env_context(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn truncate_for_hook_env(value: &str) -> String {
    if value.len() <= MAX_TOOL_HOOK_ENV_VALUE_BYTES {
        return value.to_owned();
    }

    let marker = format!("\n[truncated: original_bytes={}]", value.len());
    let keep_bytes = MAX_TOOL_HOOK_ENV_VALUE_BYTES.saturating_sub(marker.len());
    let keep_bytes = floor_char_boundary(value, keep_bytes);
    format!("{}{}", &value[..keep_bytes], marker)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::env::LocalEnvironment;
    use crate::model::{DeterministicModel, ModelResponse, ModelUsage, QueryOpts};
    use crate::trajectory::FailureCategory;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::watch;

    #[test]
    fn ripgrep_command_joins_multiline_args() {
        assert_eq!(
            ripgrep_command("--type rust\n\"fn main\" src/"),
            "rg --color never --type rust \"fn main\" src/"
        );
    }

    #[test]
    fn ripgrep_command_strips_blank_lines() {
        assert_eq!(
            ripgrep_command("pattern\n\npath/"),
            "rg --color never pattern path/"
        );
    }

    #[test]
    fn ripgrep_command_single_line_unchanged() {
        assert_eq!(ripgrep_command("TODO src/"), "rg --color never TODO src/");
    }

    #[test]
    fn ripgrep_command_preserves_regex_backslashes() {
        // Backslashes must survive so regex patterns like \bTODO\b work.
        assert_eq!(
            ripgrep_command(r"\bTODO\b src/"),
            r"rg --color never \bTODO\b src/"
        );
    }

    #[derive(Clone)]
    struct StaticEnvironment {
        result: RunResult,
    }

    #[async_trait::async_trait]
    impl Environment for StaticEnvironment {
        async fn run(&self, _req: RunRequest) -> Result<RunResult, crate::error::EnvError> {
            Ok(self.result.clone())
        }
    }

    struct CancelBeforeSubmitModel {
        cancel_tx: watch::Sender<bool>,
    }

    #[async_trait::async_trait]
    impl Model for CancelBeforeSubmitModel {
        fn name(&self) -> &'static str {
            "cancel-before-submit"
        }

        async fn query(
            &self,
            _messages: &[Message],
            _opts: &QueryOpts,
        ) -> Result<ModelResponse, crate::error::ModelError> {
            let _ = self.cancel_tx.send(true);
            Ok(ModelResponse {
                content: "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nlate submit\n```".into(),
                usage: ModelUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    cost_usd: Some(0.02),
                },
                raw: serde_json::json!({"cancelled_during_query": true}),
            })
        }
    }

    struct SlowModel {
        query_started_tx: watch::Sender<bool>,
        query_returned: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Model for SlowModel {
        fn name(&self) -> &'static str {
            "slow-model"
        }

        async fn query(
            &self,
            _messages: &[Message],
            _opts: &QueryOpts,
        ) -> Result<ModelResponse, crate::error::ModelError> {
            let _ = self.query_started_tx.send(true);
            tokio::time::sleep(Duration::from_secs(30)).await;
            self.query_returned.store(true, Ordering::SeqCst);
            Ok(ModelResponse {
                content: "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nlate submit\n```".into(),
                usage: ModelUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    cost_usd: Some(0.02),
                },
                raw: serde_json::json!({"slow_model": true}),
            })
        }
    }

    fn make_agent(responses: Vec<String>) -> DefaultAgent {
        make_agent_with_env(responses, Box::new(LocalEnvironment::new()))
    }

    fn make_agent_with_run_result(responses: Vec<String>, result: RunResult) -> DefaultAgent {
        make_agent_with_env(responses, Box::new(StaticEnvironment { result }))
    }

    fn make_agent_with_env(responses: Vec<String>, env: Box<dyn Environment>) -> DefaultAgent {
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let model = Arc::new(DeterministicModel::new(responses));
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
    async fn cancellation_after_model_response_wins_over_submit_action() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let mut agent = DefaultAgentBuilder {
            config: cfg,
            model: Arc::new(CancelBeforeSubmitModel { cancel_tx }),
            env: Box::new(StaticEnvironment {
                result: RunResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                },
            }),
            task: "test".into(),
            extra_context: None,
            renderer: None,
            stream: None,
        }
        .build()
        .unwrap();
        agent.cancellation = Some(CancellationToken::new(cancel_rx));

        let exit = agent.run().await.unwrap();

        assert!(matches!(exit, ExitReason::UserInterrupt));
        assert_eq!(
            agent.trajectory.info.exit_reason.as_deref(),
            Some(exit_reason::CANCELLED)
        );
        assert_eq!(
            agent.trajectory.info.outcome.as_deref(),
            Some(outcome::ERROR)
        );
        assert_eq!(agent.trajectory.info.final_output, None);
        assert_eq!(agent.trajectory.info.token_usage.unwrap().prompt_tokens, 11);
    }

    #[tokio::test]
    async fn forced_cancellation_interrupts_in_flight_model_query() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (query_started_tx, mut query_started_rx) = watch::channel(false);
        let query_returned = Arc::new(AtomicBool::new(false));
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let mut agent = DefaultAgentBuilder {
            config: cfg,
            model: Arc::new(SlowModel {
                query_started_tx,
                query_returned: Arc::clone(&query_returned),
            }),
            env: Box::new(StaticEnvironment {
                result: RunResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                },
            }),
            task: "test".into(),
            extra_context: None,
            renderer: None,
            stream: None,
        }
        .build()
        .unwrap();
        agent.cancellation = Some(CancellationToken::new(cancel_rx));

        tokio::spawn(async move {
            while query_started_rx.changed().await.is_ok() {
                if *query_started_rx.borrow() {
                    let _ = cancel_tx.send(true);
                    return;
                }
            }
        });

        let exit = match tokio::time::timeout(Duration::from_secs(1), agent.run()).await {
            Ok(result) => result.unwrap(),
            Err(err) => panic!("forced cancellation should interrupt model.query: {err}"),
        };

        assert!(matches!(exit, ExitReason::UserInterrupt));
        assert!(!query_returned.load(Ordering::SeqCst));
        assert_eq!(
            agent.trajectory.info.exit_reason.as_deref(),
            Some(exit_reason::CANCELLED)
        );
        assert_eq!(
            agent.trajectory.info.outcome.as_deref(),
            Some(outcome::ERROR)
        );
        assert_eq!(agent.trajectory.info.final_output, None);
        assert_eq!(agent.trajectory.info.steps, Some(0));
        assert_eq!(agent.trajectory.info.token_usage.unwrap().prompt_tokens, 0);
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
                .any(|m| m.content.contains("did not include a valid action"))
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

    #[test]
    fn truncate_observation_text_elides_middle() {
        let input = "aaaaabbbbbcccccdddddeeeee";
        let t = truncate_observation_text(input, 20, 0.5);
        assert!(t.truncated);
        assert!(t.bytes_omitted > 0);
        assert!(t.text.contains("[truncated"));
    }

    #[test]
    fn truncate_observation_text_handles_utf8_boundaries() {
        let input = "αβγδεζηθικλμνξοπρστυφχψω";
        let t = truncate_observation_text(input, 11, 0.5);
        assert!(t.truncated);
        assert!(std::str::from_utf8(t.text.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_observation_text_respects_max_bytes_hard_cap() {
        let input = "x".repeat(10_000);
        let t = truncate_observation_text(&input, 128, 0.5);
        assert!(t.truncated);
        assert!(t.text.len() <= 128);
    }

    #[test]
    fn truncate_observation_text_zero_cap_returns_empty() {
        let t = truncate_observation_text("abcdef", 0, 0.5);
        assert!(t.truncated);
        assert_eq!(t.bytes_omitted, 6);
        assert!(t.text.is_empty());
    }

    #[test]
    fn truncate_observation_text_zero_cap_empty_input_not_truncated() {
        let t = truncate_observation_text("", 0, 0.5);
        assert!(!t.truncated);
        assert_eq!(t.bytes_omitted, 0);
        assert!(t.text.is_empty());
    }

    #[tokio::test]
    async fn oversized_stdout_is_truncated_for_model_but_preserved_on_disk() {
        let stdout = format!("{}\n", "x".repeat(100_000));
        let stdout_len = stdout.len();
        let mut a = make_agent_with_run_result(
            vec![
                "```bash\nemit-large-stdout\n```".into(),
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
            ],
            RunResult {
                stdout,
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            },
        );
        a.config.root.agent.observation_max_bytes = 1024;
        let _ = a.run().await.unwrap();
        let obs = a
            .history
            .iter()
            .find(|m| m.role == Role::User && m.content.contains("Exit code"))
            .unwrap();
        assert!(obs.content.len() < 4000);
        let run_result = a
            .trajectory
            .messages
            .iter()
            .find(|m| m.role == "user" && m.extra.other.contains_key("run_result"))
            .unwrap();
        let stdout = run_result.extra.other["run_result"]["stdout"]
            .as_str()
            .unwrap();
        assert_eq!(stdout.len(), stdout_len);
    }

    #[tokio::test]
    async fn combined_output_field_is_capped_when_stdout_and_stderr_are_large() {
        let mut a = make_agent_with_run_result(
            vec![
                "```bash\nemit-large-stdout-stderr\n```".into(),
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
            ],
            RunResult {
                stdout: format!("{}\n", "o".repeat(6_000)),
                stderr: format!("{}\n", "e".repeat(6_000)),
                exit_code: 0,
                timed_out: false,
            },
        );
        a.config.root.agent.observation_max_bytes = 512;
        a.config.root.agent.observation_template = "{{ output }}".into();
        let _ = a.run().await.unwrap();
        let obs = a
            .history
            .iter()
            .rev()
            .find(|m| m.role == Role::User && m.content.contains("truncated"))
            .unwrap();
        assert!(obs.content.len() <= 512);
    }

    #[tokio::test]
    async fn observation_truncated_true_when_only_combined_output_is_elided() {
        let mut a = make_agent_with_run_result(
            vec![
                "```bash\nemit-moderate-stdout-stderr\n```".into(),
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
            ],
            RunResult {
                stdout: format!("{}\n", "o".repeat(300)),
                stderr: format!("{}\n", "e".repeat(300)),
                exit_code: 0,
                timed_out: false,
            },
        );
        a.config.root.agent.observation_max_bytes = 512;
        a.config.root.agent.observation_template = "{{ output }}".into();
        let _ = a.run().await.unwrap();
        let rec = a
            .trajectory
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user" && m.extra.other.contains_key("observation_truncated"))
            .unwrap();
        assert_eq!(
            rec.extra.other["stdout_bytes_omitted"],
            serde_json::json!(0)
        );
        assert_eq!(
            rec.extra.other["stderr_bytes_omitted"],
            serde_json::json!(0)
        );
        assert!(rec.extra.other["output_bytes_omitted"].as_u64().unwrap() > 0);
        assert_eq!(
            rec.extra.other["observation_truncated"],
            serde_json::json!(true)
        );
    }

    // ── Per-task budget: RED-phase tests ──────────────────────────────────

    fn make_agent_with_budget(
        responses: Vec<String>,
        usage: ModelUsage,
        per_task_budget_usd: Option<f64>,
        hide: bool,
    ) -> DefaultAgent {
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 10;
        cfg.root.agent.per_task_budget_usd = per_task_budget_usd;
        cfg.root.agent.hide_budget_from_agent = hide;
        let model = Arc::new(DeterministicModel::with_usage(responses, usage));
        DefaultAgentBuilder {
            config: cfg,
            model,
            env: Box::new(LocalEnvironment::new()),
            task: "test".into(),
            extra_context: None,
            renderer: None,
            stream: None,
        }
        .build()
        .unwrap()
    }

    #[tokio::test]
    async fn per_task_budget_terminates_with_budget_exhausted() {
        let usage = ModelUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: Some(0.10),
        };
        let mut agent = make_agent_with_budget(
            vec![
                "```bash\necho hello\n```".into(),
                "```bash\necho world\n```".into(),
            ],
            usage,
            Some(0.05),
            false,
        );
        let exit = agent.run().await.unwrap();
        assert!(
            matches!(exit, crate::agent::ExitReason::BudgetExhausted { .. }),
            "expected BudgetExhausted, got {exit:?}"
        );
        assert_eq!(
            agent.trajectory.info.failure_category,
            Some(FailureCategory::BudgetExhausted),
        );
    }

    #[tokio::test]
    async fn budget_block_appears_in_observation_when_enabled() {
        let usage = ModelUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: Some(0.01),
        };
        let mut agent = make_agent_with_budget(
            vec![
                "```bash\necho hello\n```".into(),
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
            ],
            usage,
            Some(1.00),
            false,
        );
        let _ = agent.run().await.unwrap();
        let has_budget_block = agent
            .history
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("Budget:"));
        assert!(
            has_budget_block,
            "expected budget block in observation; history: {:?}",
            agent
                .history
                .iter()
                .map(|m| (m.role, &m.content))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn budget_block_hidden_from_agent_when_flag_set() {
        let usage = ModelUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: Some(0.01),
        };
        let mut agent = make_agent_with_budget(
            vec![
                "```bash\necho hello\n```".into(),
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
            ],
            usage,
            Some(1.00),
            true,
        );
        let _ = agent.run().await.unwrap();
        let has_budget_block = agent
            .history
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("Budget:"));
        assert!(
            !has_budget_block,
            "budget block should be hidden from agent"
        );
    }

    #[tokio::test]
    async fn budget_block_absent_when_no_per_task_budget() {
        let mut agent = make_agent(vec![
            "```bash\necho hello\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
        ]);
        let _ = agent.run().await.unwrap();
        let has_budget_block = agent
            .history
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("Budget:"));
        assert!(
            !has_budget_block,
            "budget block should not appear when no per-task budget is set"
        );
    }

    #[tokio::test]
    async fn budget_exhausted_records_cost_and_failure_category() {
        let usage = ModelUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: Some(0.10),
        };
        let mut agent = make_agent_with_budget(
            vec!["```bash\necho hello\n```".into()],
            usage,
            Some(0.05),
            false,
        );
        let _ = agent.run().await.unwrap();
        assert!(
            agent.trajectory.info.total_cost_usd.is_some(),
            "total_cost_usd should be set on budget exhaustion"
        );
        assert_eq!(
            agent.trajectory.info.failure_category,
            Some(FailureCategory::BudgetExhausted)
        );
    }
}
