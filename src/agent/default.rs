//! `DefaultAgent`: the minimal agent loop with bash plus runtime tools.
//!
//! Loop:
//!   1. Limit check → Terminate if exceeded
//!   2. Retag cache hints on history
//!   3. model.query
//!   4. parse::extract_action_from_model_response
//!   5. tool dispatch through env.run
//!   6. observation template → push as user message, record in trajectory
//!   7. bump steps, Continue

use async_trait::async_trait;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{
    Action, Agent, ExitReason, StepOutcome, extract_action_for_tools,
    extract_action_from_model_response,
};
use crate::config::{Config, ToolHookCfg};
use crate::cost::{BASELINE_COST_MODEL, CostSource, estimate_cost_usd, is_free_tier_model};
use crate::env::{CancellationToken, Environment, RunRequest, RunResult};
use crate::error::{Error, ModelError};
use crate::model::{
    CacheHint, FallbackAttemptRecord, Message, MessageExtra, Model, ModelResponse, QueryOpts, Role,
};
use crate::policy::{PolicyDecision, PolicyEngine, PolicyProfile};
use crate::redaction::{RedactingSink, Redactor, surface};
use crate::stagnation::StagnationDetector;
use crate::stream::{NullSink, StreamEvent, StreamSink};
use crate::template::Renderer;
use crate::tool::{
    BASH_TOOL_NAME, CommandTool, ToolCall, ToolInvocation, ToolProvider, ToolRegistry,
};
use crate::trajectory::{
    FailureCategory, FallbackSummary, TestCommandPattern, TestInvocation, TokenUsage, Trajectory,
    detect_test_command, effective_test_command_patterns, exit_reason, outcome,
};

const MAX_TOOL_HOOK_ENV_VALUE_BYTES: usize = 1024;
const WALLCLOCK_WARNING_BEFORE_SECS: u64 = 30;

#[derive(Debug, Clone)]
struct WallclockDeadline {
    deadline: Instant,
    timeout: Duration,
    warn_before: Duration,
    warned: bool,
}

fn wallclock_warning_before(timeout: Duration) -> Duration {
    let default = Duration::from_secs(WALLCLOCK_WARNING_BEFORE_SECS);
    if timeout > default {
        default
    } else if timeout.is_zero() {
        Duration::ZERO
    } else {
        Duration::from_secs((timeout.as_secs() / 2).max(1))
    }
}

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

/// Wall-clock elapsed since `since` in milliseconds, saturating to `u64::MAX`.
fn elapsed_ms_since(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
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
    pub actual_cost_source: Option<CostSource>,
    /// Wall-clock start, used to compute `duration_secs` on terminate.
    pub started_at_instant: Instant,
    /// End of the most recently measured stage (model query or tool exec).
    /// `harness_overhead_ms` for the next recorded turn is `Instant::now() -
    /// last_measurement_end`. Initialized to `started_at_instant`.
    last_measurement_end: Instant,
    wallclock_deadline: Option<WallclockDeadline>,
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
    pub redactor: Redactor,
    pub cancellation: Option<CancellationToken>,
    pub policy_engine: PolicyEngine,
    pub tool_registry: ToolRegistry,
    raw_task: String,
    test_command_patterns: Vec<TestCommandPattern>,
    /// Accumulated fallback failure records across every model call in this run.
    fallback_failed_attempts: Vec<FallbackAttemptRecord>,
    /// The model name that produced the most recent successful response.
    last_responding_model: Option<String>,
    /// All model names that produced a successful response across every step.
    /// Used to build a complete `attempted_models` list even in multi-step runs
    /// where the responding model changes between steps.
    all_step_responders: Vec<String>,
    /// In-loop stagnation detector; `None` when detection is disabled.
    stagnation_detector: Option<StagnationDetector>,
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
        self.build_with_tool_providers(Vec::new())
    }

    #[allow(clippy::too_many_lines)]
    pub fn build_with_tool_providers(
        self,
        tool_providers: Vec<Arc<dyn ToolProvider>>,
    ) -> Result<DefaultAgent, Error> {
        let renderer = self.renderer.unwrap_or_else(|| Arc::new(Renderer::new()));
        let tool_registry =
            ToolRegistry::from_config_and_providers(&self.config.root.agent.tools, tool_providers)?;
        let prompt_tools = tool_registry.prompt_tools();

        let system_rendered = renderer.render_str(
            &self.config.root.prompts.system,
            &serde_json::json!({
                "task": self.task,
                "extra_context": self.extra_context,
                "tools": &prompt_tools,
            }),
        )?;
        let instance_rendered = renderer.render_str(
            &self.config.root.prompts.instance,
            &serde_json::json!({
                "task": self.task,
                "extra_context": self.extra_context,
                "tools": &prompt_tools,
            }),
        )?;

        let history = vec![
            Message::system(system_rendered),
            Message::user(instance_rendered),
        ];

        let started_at = chrono::Utc::now().to_rfc3339();
        let redactor = Redactor::from_config(&self.config.root.redaction).map_err(|err| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid redaction config: {err}"
            )))
        })?;

        let mut trajectory = Trajectory::new();
        trajectory.info.task = Some(redactor.redact_text(&self.task, surface::TRAJECTORY).text);
        trajectory.info.model_name = Some(self.model.name().to_owned());
        trajectory.info.started_at = Some(started_at.clone());
        trajectory.info.other.insert(
            "toolset".into(),
            serde_json::to_value(tool_registry.manifest())?,
        );
        for m in &history {
            record_redacted_message(&mut trajectory, m, m.extra.clone(), &redactor);
        }

        let stream: Arc<dyn StreamSink> = self.stream.map_or_else(
            || Arc::new(NullSink) as Arc<dyn StreamSink>,
            |sink| Arc::new(RedactingSink::new(sink, redactor.clone())) as Arc<dyn StreamSink>,
        );
        let test_command_patterns = effective_test_command_patterns(
            &self.config.root.agent.test_command_patterns,
            self.config.root.agent.test_command_patterns_replace,
        )
        .map_err(|err| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid agent.test_command_patterns regex: {err}"
            )))
        })?;
        let policy_engine = PolicyEngine::from_cfg(&self.config.root.policy).map_err(|err| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid policy config: {err}"
            )))
        })?;
        // Validate and build the stagnation detector.
        let agent_cfg = &self.config.root.agent;
        let stagnation_detector = if agent_cfg.detect_stagnation {
            let k = agent_cfg.stagnation_repeat_threshold;
            let w = agent_cfg.stagnation_window;
            if k == 0 {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "--stagnation-repeat-threshold must be >= 1".into(),
                )));
            }
            if w == 0 {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "--stagnation-window must be >= 1".into(),
                )));
            }
            if w < k {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "--stagnation-window ({w}) must be >= --stagnation-repeat-threshold ({k})"
                ))));
            }
            Some(StagnationDetector::new(k, w))
        } else {
            None
        };

        stream.emit(StreamEvent::RunStarted {
            task: self.task.clone(),
            model: self.model.name().to_owned(),
            started_at,
        });

        let started_at_instant = Instant::now();
        Ok(DefaultAgent {
            config: self.config,
            model: self.model,
            env: self.env,
            renderer,
            history,
            trajectory,
            steps: 0,
            total_cost_usd: 0.0,
            actual_cost_source: None,
            started_at_instant,
            last_measurement_end: started_at_instant,
            wallclock_deadline: None,
            prompt_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            completion_tokens: 0,
            stream,
            redactor,
            cancellation: None,
            policy_engine,
            tool_registry,
            raw_task: self.task,
            test_command_patterns,
            fallback_failed_attempts: Vec::new(),
            last_responding_model: None,
            all_step_responders: Vec::new(),
            stagnation_detector,
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

fn record_redacted_message(
    trajectory: &mut Trajectory,
    message: &Message,
    extra: MessageExtra,
    redactor: &Redactor,
) {
    let mut redacted = message.clone();
    redacted.content = redactor
        .redact_text(&message.content, surface::TRAJECTORY)
        .text;
    trajectory.record_with_extra(
        &redacted,
        redact_message_extra(extra, redactor, surface::TRAJECTORY),
    );
}

fn redact_message_extra(
    mut extra: MessageExtra,
    redactor: &Redactor,
    surface_name: &str,
) -> MessageExtra {
    if let Some(actions) = &mut extra.actions {
        for action in actions {
            *action = redactor.redact_text(action, surface_name).text;
        }
    }
    if let Some(response) = &mut extra.response {
        redactor.redact_json_value(response, surface_name);
    }
    for value in extra.other.values_mut() {
        redactor.redact_json_value(value, surface_name);
    }
    extra
}

fn assistant_content_with_normalized_action(
    provider_content: &str,
    action: &Action,
    tool_names: &[String],
) -> String {
    if !matches!(
        extract_action_for_tools(provider_content, tool_names),
        Action::None
    ) {
        return provider_content.to_owned();
    }

    let Some(fenced_action) = action_as_fenced_block(action) else {
        return provider_content.to_owned();
    };

    if provider_content.trim().is_empty() {
        fenced_action
    } else {
        format!("{}\n{}", provider_content.trim_end(), fenced_action)
    }
}

fn action_as_fenced_block(action: &Action) -> Option<String> {
    match action {
        Action::Bash(cmd) => Some(format!("```bash\n{cmd}\n```")),
        Action::Tool(call) => Some(format!("```{}\n{}\n```", call.name, call.input)),
        Action::Submit(_) | Action::None => None,
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
        self.maybe_warn_wallclock_deadline();

        // 2. Retag cache hints (one line; backend handles capping).
        retag_cache_hints(&mut self.history);

        // 3. model.query.
        let opts = QueryOpts {
            temperature: self.config.root.model.temperature,
            max_tokens: Some(self.config.root.model.max_tokens),
            extra: serde_json::Map::new(),
        };
        // Harness overhead leading up to this assistant turn = time since
        // the previous measurement boundary (start of run or end of the
        // previous tool exec). Captured BEFORE model.query so the harness
        // bucket does not double-count model wall-clock.
        let assistant_harness_ms = elapsed_ms_since(self.last_measurement_end);
        let model_query_start = Instant::now();
        let query_result = query_model_until_cancelled(
            self.model.as_ref(),
            &self.history,
            &opts,
            self.cancellation.clone(),
        )
        .await;
        let model_latency_ms_value = elapsed_ms_since(model_query_start);
        let model_latency_recorded = if self.model.skip_latency_telemetry() {
            None
        } else {
            Some(model_latency_ms_value)
        };
        self.last_measurement_end = Instant::now();
        // When every model in a fallback chain fails transiently the error
        // carries the structured attempt records. Capture them before
        // propagating so finalize_run_metadata can still emit a summary.
        if let Err(ModelError::AllCandidatesFailed(_, ref attempts)) = query_result {
            self.fallback_failed_attempts
                .extend(attempts.iter().map(|a| FallbackAttemptRecord {
                    model: a.model.clone(),
                    failure_reason: a.reason.clone(),
                    retry_after_secs: a.retry_after_secs,
                }));
        }
        let Some(resp) = query_result? else {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        };
        self.record_model_cost(resp.usage.cost_usd);
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
        // Accumulate fallback telemetry from this response.
        self.fallback_failed_attempts
            .extend(resp.fallback_attempts.iter().cloned());
        self.last_responding_model
            .clone_from(&resp.responding_model);
        if let Some(m) = &resp.responding_model {
            self.all_step_responders.push(m.clone());
        }

        if self.cancellation_requested() {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        }

        // 4. Parse action.
        let tool_names = self.tool_registry.tool_names();
        let action = extract_action_from_model_response(&resp.content, &resp.raw, &tool_names);
        let assistant_content =
            assistant_content_with_normalized_action(&resp.content, &action, &tool_names);

        // Record assistant message in trajectory with raw + cost.
        let asst_ts = chrono::Utc::now().to_rfc3339();
        let mut asst = Message::assistant(assistant_content.clone());
        asst.extra.cost = resp.usage.cost_usd;
        asst.extra.response = Some(resp.raw.clone());
        asst.extra.timestamp = Some(asst_ts.clone());
        asst.extra.model_latency_ms = model_latency_recorded;
        asst.extra.harness_overhead_ms = Some(assistant_harness_ms);

        // Store input fingerprint + canonical for replay drift detection (issue #155).
        // Fingerprint the TRAJECTORY-redacted, marker-normalized view of history so
        // that replay computes the same hash even when:
        //   (a) the initial task/context contained secrets (redacted before hashing), or
        //   (b) tool observations contained secrets that were redacted with a per-run
        //       salt; normalize_redaction_markers strips the salt-bearing hash segment
        //       from [REDACTED:kind:size:hash] → [REDACTED:kind:size] so recording and
        //       replay produce identical canonical JSON for the same logical content.
        //
        // Two passes over history:
        //   1. Redact (TRAJECTORY surface) — produces hashed markers like
        //      [REDACTED:kind:size:HASH].  This form is stored in the trajectory so
        //      the canonical doesn't introduce a second, hash-free marker variant
        //      that would break the "one stable marker per run" invariant.
        //   2. Normalize (strip the per-run salt hash) — produces stable markers
        //      like [REDACTED:kind:size].  Used only to compute a run-independent
        //      fingerprint hash; NOT stored in the trajectory.
        let redacted_history: Vec<crate::model::Message> = self
            .history
            .iter()
            .map(|m| {
                let mut m2 = m.clone();
                m2.content = self.redactor.redact_text_scratch(&m.content);
                m2
            })
            .collect();
        let normalized_history: Vec<crate::model::Message> = redacted_history
            .iter()
            .map(|m| {
                let mut m2 = m.clone();
                m2.content = crate::fingerprint::normalize_redaction_markers(&m.content);
                m2
            })
            .collect();
        let fp = crate::fingerprint::compute_input_fingerprint(&normalized_history);
        // Store the redacted (hashed-marker) canonical — replay normalizes it
        // before diffing so per-run salts don't pollute the drift report.
        let raw_canonical = crate::fingerprint::canonical_json(&redacted_history);
        let (canonical_stored, canonical_truncated) = crate::fingerprint::cap_canonical(
            &raw_canonical,
            crate::run::replay::CANONICAL_CAP_BYTES,
        );
        asst.extra.other.insert(
            "model_call".to_owned(),
            serde_json::json!({
                "input_fingerprint": fp.hex,
                "input_canonical_size": fp.canonical_size,
                "input_canonical": canonical_stored,
                "input_canonical_truncated": canonical_truncated
            }),
        );

        self.stream.emit(StreamEvent::AssistantMessage {
            step: self.steps,
            content: resp.content.clone(),
            cost_usd: resp.usage.cost_usd,
            timestamp: asst_ts,
        });

        match &action {
            Action::Submit(output) => {
                asst.extra.actions = Some(vec!["__SUBMIT__".into()]);
                self.history.push(Message::assistant(
                    self.redactor
                        .redact_text(&assistant_content, surface::MODEL_OBSERVATION)
                        .text,
                ));
                record_redacted_message(
                    &mut self.trajectory,
                    &asst,
                    asst.extra.clone(),
                    &self.redactor,
                );
                let final_output = self.redactor.redact_text(output, surface::TRAJECTORY).text;
                self.trajectory.info.exit_reason = Some("submitted".into());
                self.trajectory.info.failure_category = None;
                self.trajectory.info.final_output = Some(final_output.clone());
                self.trajectory.info.steps = Some(self.steps);
                self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
                self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
                self.finalize_run_metadata(outcome::SUBMITTED);
                self.emit_run_ended("submitted", None, Some(output.clone()));
                return Ok(StepOutcome::Terminate(ExitReason::Submitted {
                    final_output,
                }));
            }
            Action::Bash(cmd) => {
                asst.extra.actions = Some(vec![cmd.clone()]);
            }
            Action::Tool(call) => {
                asst.extra.actions = Some(vec![call.action_label()]);
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
                self.history.push(Message::assistant(
                    self.redactor
                        .redact_text(&assistant_content, surface::MODEL_OBSERVATION)
                        .text,
                ));
                record_redacted_message(
                    &mut self.trajectory,
                    &asst,
                    asst.extra.clone(),
                    &self.redactor,
                );
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
                let mut obs = Message::user(
                    self.redactor
                        .redact_text(&err, surface::MODEL_OBSERVATION)
                        .text,
                );
                obs.extra.harness_overhead_ms = Some(elapsed_ms_since(self.last_measurement_end));
                self.history.push(obs.clone());
                let obs_extra = obs.extra.clone();
                record_redacted_message(&mut self.trajectory, &obs, obs_extra, &self.redactor);
                self.last_measurement_end = Instant::now();
                self.steps += 1;
                return Ok(StepOutcome::Continue);
            }
        }

        // 5. Policy gate: check bash commands before hooks or execution.
        let tool_call = match action {
            Action::Bash(cmd) => ToolCall::bash(cmd),
            Action::Tool(call) => call,
            Action::Submit(_) | Action::None => {
                unreachable!("Submit and None handled above");
            }
        };
        let tool_name = tool_call.name;
        let tool_input = tool_call.input;
        let is_bash = tool_name == BASH_TOOL_NAME;
        if !self.tool_registry.contains(&tool_name) {
            unreachable!("Submit and None handled above");
        }

        // Record the assistant proposal in history & trajectory before any
        // gating decision so blocked attempts are still audited.
        self.history.push(Message::assistant(
            self.redactor
                .redact_text(&assistant_content, surface::MODEL_OBSERVATION)
                .text,
        ));
        record_redacted_message(
            &mut self.trajectory,
            &asst,
            asst.extra.clone(),
            &self.redactor,
        );

        let policy_command = if is_bash {
            Some(tool_input.clone())
        } else if let Some(tool) = self.tool_registry.command_tool(&tool_name) {
            let context = self.command_tool_context(tool, &tool_input);
            Some(self.renderer.render_str(&tool.command, &context)?)
        } else {
            None
        };

        if let Some(policy_command) = policy_command.as_deref() {
            // `DefaultAgent` is the unattended runner (sweeps, CI), so per the
            // spec for issue #90 we use the non-interactive resolver: any `Ask`
            // decision fails closed before a child process is launched. This
            // applies to bash and command-adapter tools because both execute
            // shell commands.
            let policy_decision = self
                .policy_engine
                .check_command_non_interactive(policy_command);
            if let PolicyDecision::Deny { ref label } = policy_decision {
                self.trajectory.info.policy_counts.record(&policy_decision);
                let rejection = format!(
                    "Exit code: 1\nOutput:\nCommand blocked by policy rule '{label}'. \
                     The command was not executed. Please attempt a safer alternative.",
                );
                let obs_msg = Message::user(rejection.clone());
                self.history.push(obs_msg.clone());
                let mut obs_extra = crate::model::MessageExtra {
                    harness_overhead_ms: Some(elapsed_ms_since(self.last_measurement_end)),
                    ..crate::model::MessageExtra::default()
                };
                obs_extra
                    .other
                    .insert("policy_blocked".into(), serde_json::Value::Bool(true));
                obs_extra.other.insert(
                    "policy_rule".into(),
                    serde_json::Value::String(label.clone()),
                );
                obs_extra.other.insert(
                    "blocked_command".into(),
                    serde_json::Value::String(
                        self.redactor
                            .redact_text(policy_command, surface::TRAJECTORY)
                            .text,
                    ),
                );
                record_redacted_message(&mut self.trajectory, &obs_msg, obs_extra, &self.redactor);
                self.stream.emit(StreamEvent::Observation {
                    step: self.steps,
                    content: rejection,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
                self.last_measurement_end = Instant::now();
                self.steps += 1;
                return Ok(StepOutcome::Continue);
            }
            if *self.policy_engine.profile() == PolicyProfile::Yolo {
                self.trajectory.info.policy_counts.record_yolo_bypass();
            } else {
                self.trajectory.info.policy_counts.record(&policy_decision);
            }
        }

        // 5c. PreToolUse hooks, then tool execution if not blocked.
        let pre_hook_results = self
            .run_tool_hooks(
                ToolHookPhase::PreToolUse,
                &self.config.root.agent.hooks.pre_tool_use,
                &tool_name,
                &tool_input,
                None,
            )
            .await?;
        let tool_use_blocked = pre_hook_results.iter().any(ToolHookResult::blocks_tool_use);
        if self.cancellation_requested() {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        }

        // Harness overhead from the end of the model query to just before
        // the tool starts executing: policy gate, pre-hooks, message
        // construction.
        let obs_harness_ms = elapsed_ms_since(self.last_measurement_end);
        let (result, post_hook_results, tool_latency_recorded) = if tool_use_blocked {
            (blocked_run_result(&pre_hook_results), Vec::new(), None)
        } else {
            let tool_start = Instant::now();
            let result = if is_bash {
                self.stream.emit(StreamEvent::BashStart {
                    step: self.steps,
                    command: tool_input.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
                let run_req = RunRequest::new(&tool_input).with_timeout(Duration::from_secs(
                    self.config.root.environment.timeout_secs,
                ));
                let run_req = if let Some(cancellation) = self.cancellation.clone() {
                    run_req.with_cancellation(cancellation)
                } else {
                    run_req
                };
                let result = self.env.run(run_req).await?;
                self.stream.emit(StreamEvent::BashResult {
                    step: self.steps,
                    exit_code: result.exit_code,
                    stdout: result.stdout.clone(),
                    stderr: result.stderr.clone(),
                    timed_out: result.timed_out,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
                result
            } else {
                self.run_non_bash_tool(&tool_name, &tool_input).await?
            };
            let tool_latency = elapsed_ms_since(tool_start);
            self.last_measurement_end = Instant::now();
            let post_hook_results = self
                .run_tool_hooks(
                    ToolHookPhase::PostToolUse,
                    &self.config.root.agent.hooks.post_tool_use,
                    &tool_name,
                    &tool_input,
                    Some(&result),
                )
                .await?;
            (result, post_hook_results, Some(tool_latency))
        };

        if is_bash && !tool_use_blocked {
            self.record_test_invocation_if_matched(&tool_input, result.exit_code);
        }

        let result_for_observation = RunResult {
            stdout: self
                .redactor
                .redact_text(&result.stdout, surface::MODEL_OBSERVATION)
                .text,
            stderr: self
                .redactor
                .redact_text(&result.stderr, surface::MODEL_OBSERVATION)
                .text,
            exit_code: result.exit_code,
            timed_out: result.timed_out,
        };
        let result_for_trajectory = RunResult {
            stdout: self
                .redactor
                .redact_text(&result.stdout, surface::TRAJECTORY)
                .text,
            stderr: self
                .redactor
                .redact_text(&result.stderr, surface::TRAJECTORY)
                .text,
            exit_code: result.exit_code,
            timed_out: result.timed_out,
        };

        let trunc_stdout = truncate_observation_text(
            &result_for_observation.stdout,
            self.config.root.agent.observation_max_bytes,
            self.config.root.agent.observation_head_ratio,
        );
        let trunc_stderr = truncate_observation_text(
            &result_for_observation.stderr,
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
        let pre_hook_results_for_observation = redact_hook_results_for_surface(
            truncate_hook_results_for_observation(
                &pre_hook_results,
                self.config.root.agent.observation_max_bytes,
                self.config.root.agent.observation_head_ratio,
            ),
            &self.redactor,
            surface::MODEL_OBSERVATION,
        );
        let post_hook_results_for_observation = redact_hook_results_for_surface(
            truncate_hook_results_for_observation(
                &post_hook_results,
                self.config.root.agent.observation_max_bytes,
                self.config.root.agent.observation_head_ratio,
            ),
            &self.redactor,
            surface::MODEL_OBSERVATION,
        );
        let tool_input_for_observation = self
            .redactor
            .redact_text(&tool_input, surface::MODEL_OBSERVATION)
            .text;
        let tool_name_for_observation = self
            .redactor
            .redact_text(&tool_name, surface::MODEL_OBSERVATION)
            .text;
        // 6. Render observation.
        let obs_text = self.renderer.render_str(
            &self.config.root.agent.observation_template,
            &serde_json::json!({
                "returncode": result.exit_code,
                "output": trunc_output.text,
                "stdout": trunc_stdout.text,
                "stderr": trunc_stderr.text,
                "timed_out": result.timed_out,
                "command": tool_input_for_observation,
                "tool_name": tool_name_for_observation,
                "tool_input": tool_input_for_observation,
                "step": self.steps,
                "tool_use_blocked": tool_use_blocked,
                "pre_tool_use_hooks": pre_hook_results_for_observation,
                "post_tool_use_hooks": post_hook_results_for_observation,
            }),
        )?;

        // 6b. Optionally append the budget block.
        let obs_text = self.append_budget_block(obs_text)?;
        let obs_text = self
            .redactor
            .redact_text(&obs_text, surface::MODEL_OBSERVATION)
            .text;

        // (assistant turn was recorded before the policy gate at step 5)

        // Record user observation.
        let obs_ts = chrono::Utc::now().to_rfc3339();
        let obs_msg = Message::user(obs_text.clone());
        self.history.push(obs_msg.clone());
        let mut obs_extra = MessageExtra::default();
        obs_extra.other.insert(
            "run_result".into(),
            serde_json::to_value(&result_for_trajectory).unwrap_or(serde_json::Value::Null),
        );
        let mut pre_hook_value = serde_json::to_value(&pre_hook_results)?;
        self.redactor
            .redact_json_value(&mut pre_hook_value, surface::TRAJECTORY);
        obs_extra
            .other
            .insert("pre_tool_use_hooks".into(), pre_hook_value);
        let mut post_hook_value = serde_json::to_value(&post_hook_results)?;
        self.redactor
            .redact_json_value(&mut post_hook_value, surface::TRAJECTORY);
        obs_extra
            .other
            .insert("post_tool_use_hooks".into(), post_hook_value);
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
        obs_extra.tool_latency_ms = tool_latency_recorded;
        obs_extra.harness_overhead_ms = Some(obs_harness_ms);
        record_redacted_message(&mut self.trajectory, &obs_msg, obs_extra, &self.redactor);

        self.stream.emit(StreamEvent::Observation {
            step: self.steps,
            content: obs_text,
            timestamp: obs_ts,
        });

        self.steps += 1;
        // Cancellation takes priority: if the operator interrupted during the
        // Kth repeated command we must record UserInterrupt (exit 130), not
        // agent_stagnation (exit 12).
        if self.cancellation_requested() {
            self.finalize_cancelled();
            return Ok(StepOutcome::Terminate(ExitReason::UserInterrupt));
        }

        // Check for stagnation after cancellation so step_index = self.steps - 1.
        if is_bash && !tool_use_blocked {
            if let Some(detector) = &mut self.stagnation_detector {
                if let Some(trip) = detector.observe(self.steps - 1, &tool_input) {
                    return Ok(self.terminate_stagnation(trip));
                }
            }
        }
        Ok(StepOutcome::Continue)
    }
}

impl DefaultAgent {
    pub fn set_wallclock_deadline(&mut self, timeout: Duration) {
        self.wallclock_deadline = Some(WallclockDeadline {
            deadline: Instant::now() + timeout,
            timeout,
            warn_before: wallclock_warning_before(timeout),
            warned: false,
        });
    }

    #[cfg(test)]
    pub(crate) fn set_wallclock_deadline_for_test(&mut self, deadline: Instant, timeout: Duration) {
        self.wallclock_deadline = Some(WallclockDeadline {
            deadline,
            timeout,
            warn_before: wallclock_warning_before(timeout),
            warned: false,
        });
    }

    fn maybe_warn_wallclock_deadline(&mut self) {
        let Some((timeout, remaining)) = self.wallclock_deadline.as_mut().and_then(|deadline| {
            if deadline.warned {
                return None;
            }
            let remaining = deadline.deadline.saturating_duration_since(Instant::now());
            if remaining > deadline.warn_before {
                return None;
            }
            deadline.warned = true;
            Some((deadline.timeout, remaining))
        }) else {
            return;
        };
        self.push_wallclock_deadline_warning(timeout, remaining);
    }

    fn push_wallclock_deadline_warning(&mut self, timeout: Duration, remaining: Duration) {
        let remaining_secs = remaining.as_secs();
        let timeout_secs = timeout.as_secs();
        let content = format!(
            "The task wallclock deadline is near: about {remaining_secs}s remain from the \
             {timeout_secs}s task budget. If you already have a useful patch, stop exploratory \
             work and submit now with COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT. Do not install \
             dependencies unless they are strictly required to produce the final patch."
        );
        let msg = Message::user(
            self.redactor
                .redact_text(&content, surface::MODEL_OBSERVATION)
                .text,
        );
        self.history.push(msg.clone());
        let mut extra = MessageExtra::default();
        extra.other.insert(
            "wallclock_deadline_warning".into(),
            serde_json::Value::Bool(true),
        );
        record_redacted_message(&mut self.trajectory, &msg, extra, &self.redactor);
    }

    fn record_model_cost(&mut self, cost_usd: Option<f64>) {
        let source = match cost_usd {
            Some(cost) => {
                self.total_cost_usd += cost;
                CostSource::RateCardEstimate
            }
            None if is_free_tier_model(self.model.name()) => CostSource::FreeTierInferred,
            None => CostSource::Unknown,
        };
        self.actual_cost_source = Some(
            self.actual_cost_source
                .map_or(source, |current| current.combine(source)),
        );
    }

    fn baseline_cost_usd(&self) -> f64 {
        estimate_cost_usd(
            self.prompt_tokens,
            self.cache_read_tokens,
            self.cache_creation_tokens,
            self.completion_tokens,
            BASELINE_COST_MODEL,
        )
    }

    fn stamp_cost_metadata(&mut self) {
        self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
        self.trajectory.info.actual_cost_usd = Some(self.total_cost_usd);
        self.trajectory.info.actual_cost_source =
            Some(self.actual_cost_source.unwrap_or(CostSource::Unknown));
        self.trajectory.info.baseline_cost_usd = Some(self.baseline_cost_usd());
        self.trajectory.info.baseline_cost_model = Some(BASELINE_COST_MODEL.to_owned());
    }

    fn cancellation_requested(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    fn emit_run_ended(
        &mut self,
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
        self.trajectory.info.redaction = Some(self.redactor.summary());
    }

    /// Stamp the trajectory with the coarse `outcome`, accumulated
    /// `token_usage`, and wall-clock `duration_secs`. Call from every
    /// terminal path so every `.traj.json` carries these first-class
    /// fields without callers needing to remember.
    pub fn finalize_run_metadata(&mut self, outcome_label: &str) {
        self.trajectory.info.outcome = Some(outcome_label.to_owned());
        self.stamp_cost_metadata();
        self.trajectory.info.token_usage = Some(TokenUsage {
            prompt_tokens: self.prompt_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            completion_tokens: self.completion_tokens,
        });
        self.trajectory.info.duration_secs = Some(self.started_at_instant.elapsed().as_secs_f64());
        self.trajectory.info.redaction = Some(self.redactor.summary());
        self.refresh_test_metadata();
        // Populate fallback summary only when fallback was configured and used.
        let primary = self.model.name().to_owned();
        // When every model failed transiently, last_responding_model is None.
        // Avoid fabricating primary as final_model — use the last attempted.
        let all_failed =
            !self.fallback_failed_attempts.is_empty() && self.last_responding_model.is_none();
        let final_model = self.last_responding_model.clone().unwrap_or_else(|| {
            if all_failed {
                self.fallback_failed_attempts
                    .last()
                    .map_or_else(|| primary.clone(), |a| a.model.clone())
            } else {
                primary.clone()
            }
        });
        if !self.fallback_failed_attempts.is_empty() {
            // Build attempted_models from all models that were tried (failed or
            // responded) across every step, deduped while preserving order.
            let mut seen = std::collections::HashSet::new();
            let mut attempted_models: Vec<String> = Vec::new();
            for m in self
                .fallback_failed_attempts
                .iter()
                .map(|a| &a.model)
                .chain(self.all_step_responders.iter())
            {
                if seen.insert(m.as_str()) {
                    attempted_models.push(m.clone());
                }
            }
            let fallback_count =
                u32::try_from(self.fallback_failed_attempts.len()).unwrap_or(u32::MAX);
            self.trajectory.info.fallback_summary = Some(FallbackSummary {
                primary_model: primary,
                final_model,
                fallback_happened: true,
                fallback_count,
                attempted_models,
                failed_attempts: self.fallback_failed_attempts.clone(),
                all_failed,
            });
        } else if self.last_responding_model.is_some() {
            // FallbackModel succeeded on primary — record a "no fallback" summary
            // so operators can confirm the primary model was used.
            self.trajectory.info.fallback_summary = Some(FallbackSummary {
                primary_model: primary.clone(),
                final_model,
                fallback_happened: false,
                fallback_count: 0,
                attempted_models: vec![primary],
                failed_attempts: Vec::new(),
                all_failed: false,
            });
        }
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

    fn terminate_stagnation(&mut self, trip: crate::stagnation::StagnationTrip) -> StepOutcome {
        self.trajectory.info.exit_reason = Some("agent_stagnation".into());
        self.trajectory.info.failure_category = Some(FailureCategory::AgentStagnation);
        self.trajectory.info.steps = Some(self.steps);
        self.trajectory.info.total_cost_usd = Some(self.total_cost_usd);
        self.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
        self.trajectory.info.other.insert(
            "stagnation".into(),
            serde_json::json!({
                "action_hash": trip.action_hash,
                "count": trip.count,
                "window": trip.window,
                "step_indices": trip.step_indices,
            }),
        );
        self.finalize_run_metadata(outcome::ERROR);
        self.emit_run_ended(
            "agent_stagnation",
            Some(FailureCategory::AgentStagnation),
            None,
        );
        StepOutcome::Terminate(ExitReason::AgentStagnation {
            action_hash: trip.action_hash,
            count: trip.count,
            window: trip.window,
        })
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
                command: self.redactor.redact_text(command, surface::TRAJECTORY).text,
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
        tool_input: &str,
        result: Option<&RunResult>,
    ) -> Result<Vec<ToolHookResult>, Error> {
        let mut reports = Vec::new();
        for hook in hooks {
            reports.push(
                self.run_tool_hook(phase, hook, tool_name, tool_input, result)
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
        tool_input: &str,
        result: Option<&RunResult>,
    ) -> Result<ToolHookResult, Error> {
        let context = self.tool_hook_context(phase, hook, tool_name, tool_input, result);
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
        tool_input: &str,
        result: Option<&RunResult>,
    ) -> serde_json::Value {
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
            "task": self.raw_task,
            "model": model,
            "step": self.steps,
            "command": tool_input,
            "tool_input": tool_input,
            "returncode": returncode,
            "stdout": stdout,
            "stderr": stderr,
            "output": output,
            "timed_out": timed_out,
            "total_cost_usd": self.total_cost_usd,
        })
    }

    async fn run_command_tool(
        &self,
        tool_name: &str,
        tool_input: &str,
    ) -> Result<RunResult, Error> {
        let Some(tool) = self.tool_registry.command_tool(tool_name) else {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown tool `{tool_name}`"
            ))));
        };
        let context = self.command_tool_context(tool, tool_input);
        let rendered_command = self.renderer.render_str(&tool.command, &context)?;
        let timeout_secs = tool
            .timeout_secs
            .unwrap_or(self.config.root.environment.timeout_secs);
        let mut req = RunRequest::new(rendered_command)
            .with_timeout(Duration::from_secs(timeout_secs))
            .with_stdin(tool_input.to_owned());
        if let Some(cancellation) = self.cancellation.clone() {
            req = req.with_cancellation(cancellation);
        }
        req.env = tool_process_env(&context)?;
        Ok(self.env.run(req).await?)
    }

    async fn run_non_bash_tool(
        &self,
        tool_name: &str,
        tool_input: &str,
    ) -> Result<RunResult, Error> {
        if self.tool_registry.command_tool(tool_name).is_some() {
            return self.run_command_tool(tool_name, tool_input).await;
        }
        let Some(provider) = self.tool_registry.provider_for(tool_name) else {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown tool `{tool_name}`"
            ))));
        };
        let invocation = ToolInvocation {
            name: tool_name.to_owned(),
            input: tool_input.to_owned(),
            task: self.raw_task.clone(),
            model: self.trajectory.info.model_name.clone().unwrap_or_default(),
            step: self.steps,
            total_cost_usd: self.total_cost_usd,
        };
        Ok(provider
            .call(self.env.as_ref(), invocation, self.cancellation.clone())
            .await?
            .into())
    }

    fn command_tool_context(&self, tool: &CommandTool, tool_input: &str) -> serde_json::Value {
        let model = self
            .trajectory
            .info
            .model_name
            .as_deref()
            .unwrap_or_default();
        serde_json::json!({
            "tool": {
                "name": tool.name,
                "description": tool.description,
            },
            "task": self.raw_task,
            "model": model,
            "step": self.steps,
            "command": tool_input,
            "tool_input": tool_input,
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

fn redact_hook_results_for_surface(
    results: Vec<ToolHookResult>,
    redactor: &Redactor,
    surface_name: &str,
) -> Vec<ToolHookResult> {
    results
        .into_iter()
        .map(|result| ToolHookResult {
            phase: result.phase,
            name: result.name,
            command: redactor.redact_text(&result.command, surface_name).text,
            stdout: redactor.redact_text(&result.stdout, surface_name).text,
            stderr: redactor.redact_text(&result.stderr, surface_name).text,
            output: redactor.redact_text(&result.output, surface_name).text,
            exit_code: result.exit_code,
            timed_out: result.timed_out,
        })
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
    insert_json_str_untruncated(
        &mut env,
        "RUST_SWE_AGENT_TOOL_INPUT",
        &context["tool_input"],
    );
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

fn tool_process_env(context: &serde_json::Value) -> Result<BTreeMap<String, String>, Error> {
    let mut env = BTreeMap::new();
    let env_context = capped_env_context(context);
    insert_json_str(
        &mut env,
        "RUST_SWE_AGENT_TOOL_NAME",
        &env_context["tool"]["name"],
    );
    insert_json_str(&mut env, "RUST_SWE_AGENT_TASK", &env_context["task"]);
    insert_json_str(&mut env, "RUST_SWE_AGENT_MODEL", &env_context["model"]);
    insert_json_str(&mut env, "RUST_SWE_AGENT_STEP", &env_context["step"]);
    insert_json_str_untruncated(&mut env, "RUST_SWE_AGENT_COMMAND", &context["command"]);
    insert_json_str_untruncated(
        &mut env,
        "RUST_SWE_AGENT_TOOL_INPUT",
        &context["tool_input"],
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
    use crate::model::fallback::FallbackModel;
    use crate::model::{DeterministicModel, ModelResponse, ModelUsage, QueryOpts};
    use crate::trajectory::FailureCategory;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::watch;

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
                responding_model: None,
                fallback_attempts: Vec::new(),
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
                responding_model: None,
                fallback_attempts: Vec::new(),
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
                .any(|m| m.content.contains("did not include a valid tool call"))
        );
    }

    #[tokio::test]
    async fn wallclock_deadline_warning_is_visible_before_model_query() {
        let model = Arc::new(DeterministicModel::new([
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
        ]));
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let mut agent = DefaultAgentBuilder {
            config: cfg,
            model: model.clone(),
            env: Box::new(LocalEnvironment::new()),
            task: "test".into(),
            extra_context: None,
            renderer: None,
            stream: None,
        }
        .build()
        .unwrap();

        agent.set_wallclock_deadline_for_test(
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            Duration::from_secs(300),
        );

        let exit = agent.step().await.unwrap();
        assert!(matches!(
            exit,
            StepOutcome::Terminate(ExitReason::Submitted { .. })
        ));

        let recorded = model.recorded_inputs();
        let first_query = recorded.first().unwrap();
        assert!(
            first_query.iter().any(|m| {
                m.role == Role::User
                    && m.content.contains("wallclock deadline")
                    && m.content.contains("submit")
            }),
            "deadline warning should be model-visible before the query: {first_query:#?}"
        );
        assert!(
            agent
                .trajectory
                .messages
                .iter()
                .any(|m| m.content.contains("wallclock deadline")),
            "deadline warning should be recorded in the trajectory"
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
    async fn submit_via_fallback_model_records_no_fallback_summary() {
        // When the primary model succeeds, finalize_run_metadata should record
        // a fallback_summary with fallback_happened=false so operators can confirm
        // which model was used even when no fallback occurred.
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let inner = Box::new(DeterministicModel::new(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
        ]));
        let model = Arc::new(FallbackModel::new(vec![inner]));
        let mut agent = DefaultAgentBuilder {
            config: cfg,
            model,
            env: Box::new(LocalEnvironment::new()),
            task: "test".into(),
            extra_context: None,
            renderer: None,
            stream: None,
        }
        .build()
        .unwrap();
        let _ = agent.run().await.unwrap();
        let summary = agent.trajectory.info.fallback_summary.as_ref().unwrap();
        assert!(!summary.fallback_happened);
        assert_eq!(summary.fallback_count, 0);
        assert!(summary.failed_attempts.is_empty());
        assert!(!summary.all_failed);
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
