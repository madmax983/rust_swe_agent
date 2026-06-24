//! Drive the Codex CLI (`codex`) as the agent backend.
//!
//! Similar to `claude_driver.rs`, this module lets an operator point the *same*
//! run machinery at the OpenAI Codex CLI instead of the built-in bash-first loop.
//! `codex` runs in `--full-auto --json` mode; we read its newline-delimited JSON
//! stream and translate each event into the harness [`Trajectory`](crate::trajectory::Trajectory) format so that
//! patch capture, verification, `bench inspect`, and evaluation all keep working
//! unchanged.
//!
//! ## Seam
//!
//! [`drive`] takes a fully-built [`DefaultAgent`] — which already owns the
//! environment, redactor, and a trajectory seeded with the system + task messages
//! — and fills in the rest from Codex's stream, returning the same [`ExitReason`]
//! the built-in loop would.
//!
//! ## Stream format
//!
//! Codex emits NDJSON events in `--full-auto --json` mode:
//! - `"session"`: init event carrying `session_id`, `model`, and `tools`.
//! - `"reasoning"`: extended thinking blocks (`content[].text`).
//! - `"local_shell_call"`: a shell tool invocation (the `action.command` field).
//! - `"local_shell_call_output"`: result of a shell call (`output.output`,
//!   `output.exit_code`), paired with the call by `id`.
//! - `"message"`: assistant text (`content[].text` for `output_text` blocks).
//! - `"completed"`: terminal event with `exit_reason`, `result`, `cost_usd`, and
//!   `usage.{input_tokens, output_tokens}`.
//!
//! ## Scope
//!
//! - Local environment only (Codex edits the host working tree directly).
//! - Cost and tokens come from the `completed` event (`CostSource::ProviderReported`).
//! - Model selection is delegated to Codex; the actually-responding model is
//!   recorded back into the trajectory.
//! - The `codex` binary path is overridable via `MAXWELLS_CODEX_BIN` so the
//!   parser can be exercised deterministically at $0 with a fixture script.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::agent::default::truncate_observation_text;
use crate::agent::{DefaultAgent, ExitReason};
use crate::cost::CostSource;
use crate::error::Error;
use crate::model::{Message, MessageExtra};
use crate::redaction::surface;
use crate::stream::StreamEvent;
use crate::trajectory::{
    FailureCategory, TestCommandPattern, TestInvocation, TokenUsage, detect_test_command,
    effective_test_command_patterns, outcome,
};

/// Environment variable that overrides the `codex` binary path. Used by tests
/// to substitute a deterministic fixture script.
const CODEX_BIN_ENV: &str = "MAXWELLS_CODEX_BIN";

/// Grace period for `child.wait()` after the stream closes. Covers normal
/// cleanup; exceeded → kill and continue with whatever was parsed.
const WAIT_GRACE_SECS: Duration = Duration::from_secs(30);

/// A shell call seen in a `local_shell_call` event, awaiting its
/// `local_shell_call_output` so the pass/fail outcome can be paired by `id`.
struct PendingShellCall {
    command: String,
    step_index: u32,
    matched_pattern: String,
}

/// Aggregated state pulled out of the Codex stream.
#[derive(Default)]
struct Parsed {
    /// Number of shell tool invocations — the closest analog to "agent steps".
    steps: u32,
    /// Model name Codex actually used (from `session` or per-message field).
    model: Option<String>,
    /// Session id, recorded for provenance.
    session_id: Option<String>,
    /// Terminal `completed` event, if one was emitted.
    result: Option<CompletedMsg>,
    /// Shell calls awaiting their output, keyed by `id`.
    pending_calls: HashMap<String, PendingShellCall>,
    /// Completed test invocations, paired with their result exit status.
    test_invocations: Vec<TestInvocation>,
    /// Accumulated assistant text from `message` events (last non-empty wins
    /// as final_output on the submitted path).
    last_assistant_text: String,
}

struct CompletedMsg {
    exit_reason: String,
    result: String,
    cost_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
}

/// Drive a single run through the Codex CLI, filling `agent.trajectory` and
/// returning the terminal [`ExitReason`].
///
/// `workdir` is the directory `codex` runs in (and where edits land). When
/// `None`, the current process directory is used.
#[allow(clippy::too_many_lines)]
pub async fn drive(
    agent: &mut DefaultAgent,
    task: String,
    extra_context: Option<&str>,
    workdir: Option<&Path>,
    timeout_secs: Option<u64>,
) -> Result<ExitReason, Error> {
    let bin = std::env::var(CODEX_BIN_ENV).unwrap_or_else(|_| "codex".to_owned());
    let cwd = match workdir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let step_limit = agent.config.root.agent.step_limit;

    // Fold any merged extra-context / active-skill guidance into the prompt.
    let prompt = match extra_context {
        Some(ctx) if !ctx.trim().is_empty() => format!("{task}\n\n{ctx}"),
        _ => task.clone(),
    };

    let mut cancel = agent.cancellation.clone();

    if cancel
        .as_ref()
        .is_some_and(crate::env::CancellationToken::is_cancelled)
    {
        return Ok(ExitReason::UserInterrupt);
    }

    // Stamp the toolset with the Codex shell tool so tool-coverage reports
    // don't mistake it as unavailable.
    record_codex_toolset(agent);

    let mut cmd = Command::new(&bin);
    cmd.kill_on_drop(true)
        .arg("--full-auto")
        .arg("--json")
        .arg("--max-turns")
        .arg(step_limit.to_string())
        .arg(&prompt);

    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    tracing::info!(
        bin = %bin,
        cwd = %cwd.display(),
        max_turns = step_limit,
        "spawning Codex driver"
    );

    let mut child = cmd.spawn().map_err(|e| {
        Error::Trajectory(format!("failed to spawn `{bin}` for --driver codex: {e}"))
    })?;

    // Drain stderr concurrently so a chatty child can never deadlock us.
    let stderr_handle = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            const CAP: usize = 1024 * 1024;
            let mut retained: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            let mut stderr = stderr;
            loop {
                match stderr.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if retained.len() < CAP {
                            let take = (CAP - retained.len()).min(n);
                            retained.extend_from_slice(&tmp[..take]);
                        }
                    }
                }
            }
            String::from_utf8_lossy(&retained).into_owned()
        })
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Trajectory("codex child has no stdout pipe".into()))?;
    let timeout_dur = timeout_secs.map(Duration::from_secs);
    let run_start = Instant::now();

    let outcome = {
        let process = process_stream(agent, stdout);
        tokio::pin!(process);
        tokio::select! {
            res = &mut process => Outcome::Stream(Box::new(res)),
            () = async {
                match timeout_dur {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => Outcome::Timeout,
            () = async {
                match cancel.as_mut() {
                    Some(c) => c.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => Outcome::Cancelled,
        }
    };

    let parsed = match outcome {
        Outcome::Stream(res) => (*res)?,
        Outcome::Timeout => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let dur = timeout_dur.unwrap_or_default();
            agent.finalize_wallclock_timeout(dur);
            return Err(Error::Trajectory(format!(
                "task wallclock timeout after {}s (codex driver)",
                dur.as_secs()
            )));
        }
        Outcome::Cancelled => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(ExitReason::UserInterrupt);
        }
    };

    let wait_deadline = timeout_dur.map_or(WAIT_GRACE_SECS, |d| {
        d.saturating_sub(run_start.elapsed()).min(WAIT_GRACE_SECS)
    });
    let exit_code = if let Ok(Ok(status)) = tokio::time::timeout(wait_deadline, child.wait()).await
    {
        status.code()
    } else {
        // Timed out or error during wait; kill and reap to avoid a zombie.
        let _ = child.start_kill();
        let _ = child.wait().await;
        None
    };

    let stderr_text = match stderr_handle {
        Some(h) => h.await.unwrap_or_default(),
        None => String::new(),
    };

    finalize(agent, parsed, step_limit, exit_code, &stderr_text)
}

/// How the streaming race resolved.
enum Outcome {
    Stream(Box<Result<Parsed, Error>>),
    Timeout,
    Cancelled,
}

/// Overwrite `info.other["toolset"]` with the Codex shell tool, so
/// tool-coverage/drift reports treat it as available rather than missing.
fn record_codex_toolset(agent: &mut DefaultAgent) {
    let tools = vec![serde_json::json!({
        "name": "shell",
        "description": "Codex CLI shell tool",
        "source": "codex",
    })];
    agent
        .trajectory
        .info
        .other
        .insert("toolset".into(), serde_json::json!({ "tools": tools }));
}

/// Atomically persist a `partial: true` checkpoint when a checkpoint path is
/// configured, mirroring the built-in loop's per-turn write.
fn maybe_checkpoint(agent: &mut DefaultAgent, steps: u32) {
    if let Some(path) = agent.checkpoint_path.clone() {
        agent.trajectory.info.steps = Some(steps);
        agent.trajectory.info.actual_cost_usd = Some(agent.total_cost_usd);
        if let Err(e) = agent.trajectory.save_partial_atomic(&path) {
            tracing::warn!(error = %e, "codex driver checkpoint write failed; continuing");
        }
    }
}

/// Read the NDJSON stream from the child's `stdout`, recording turns and tool
/// events into the trajectory as they arrive.
async fn process_stream(
    agent: &mut DefaultAgent,
    stdout: tokio::process::ChildStdout,
) -> Result<Parsed, Error> {
    let test_patterns = effective_test_command_patterns(
        &agent.config.root.agent.test_command_patterns,
        agent.config.root.agent.test_command_patterns_replace,
    )
    .unwrap_or_default();

    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = Parsed::default();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match msg.get("type").and_then(Value::as_str) {
            Some("session") => handle_session(&mut parsed, &msg),
            Some("reasoning") => handle_reasoning(agent, &mut parsed, &msg),
            Some("local_shell_call") => {
                handle_local_shell_call(agent, &mut parsed, &msg, &test_patterns);
            }
            Some("local_shell_call_output") => {
                handle_local_shell_call_output(agent, &mut parsed, &msg);
                maybe_checkpoint(agent, parsed.steps);
            }
            Some("message") => handle_message(agent, &mut parsed, &msg),
            Some("completed") => {
                parsed.result = Some(parse_completed(&msg));
                // Terminal event: stop reading; the process will close stdout
                // shortly and waiting indefinitely risks a hang.
                break;
            }
            _ => {}
        }
    }

    Ok(parsed)
}

fn handle_session(parsed: &mut Parsed, msg: &Value) {
    parsed.session_id = msg
        .get("session_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    parsed.model = msg
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
}

/// Record a `reasoning` event as a thinking-only assistant turn.
fn handle_reasoning(agent: &mut DefaultAgent, _parsed: &mut Parsed, msg: &Value) {
    let thinking = msg
        .get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if thinking.is_empty() {
        return;
    }

    let thinking = agent
        .redactor
        .redact_text(&thinking, surface::MODEL_OBSERVATION)
        .text;

    let ts = chrono::Utc::now().to_rfc3339();
    let mut extra = MessageExtra {
        timestamp: Some(ts),
        ..Default::default()
    };
    extra
        .other
        .insert("thinking".into(), Value::String(thinking));
    agent
        .trajectory
        .record_with_extra(&Message::assistant(String::new()), extra);
}

/// Record a `local_shell_call` as an agent step with an action label.
fn handle_local_shell_call(
    agent: &mut DefaultAgent,
    parsed: &mut Parsed,
    msg: &Value,
    test_patterns: &[TestCommandPattern],
) {
    let id = msg.get("id").and_then(Value::as_str).unwrap_or("");
    let command = msg
        .get("action")
        .and_then(|a| a.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("");

    if command.is_empty() {
        return;
    }

    parsed.steps += 1;
    agent.steps = parsed.steps;

    if !id.is_empty() {
        if let Some(matched) = detect_test_command(command, test_patterns) {
            parsed.pending_calls.insert(
                id.to_owned(),
                PendingShellCall {
                    command: command.to_owned(),
                    step_index: parsed.steps - 1,
                    matched_pattern: matched,
                },
            );
        }
    }

    let action_label = agent
        .redactor
        .redact_text(command, surface::TRAJECTORY)
        .text;

    let ts = chrono::Utc::now().to_rfc3339();
    let mut extra = MessageExtra {
        timestamp: Some(ts.clone()),
        actions: Some(vec![action_label.clone()]),
        ..Default::default()
    };
    // Propagate model if we have it.
    if let Some(m) = parsed.model.clone() {
        extra.other.insert("model".into(), Value::String(m));
    }
    agent
        .trajectory
        .record_with_extra(&Message::assistant(String::new()), extra);
    agent.stream.emit(StreamEvent::AssistantMessage {
        step: parsed.steps,
        content: action_label.clone(),
        cost_usd: None,
        timestamp: ts.clone(),
    });
    // Surface the shell call as a generic tool-activity event. The driver
    // otherwise emits only AssistantMessage/Observation, so a dashboard that
    // infers liveness (issue #649) would render a long external command as a
    // static idle footer; the matching ToolEnd is emitted from the call-output
    // handler. ToolStart is not bash-command telemetry, so consumers that audit
    // shell commands ignore it. Reuse the already-redacted `action_label` since
    // the driver stream is not wrapped in RedactingSink.
    agent.stream.emit(StreamEvent::ToolStart {
        step: parsed.steps,
        label: action_label,
        timestamp: ts,
    });
}

/// Record a `local_shell_call_output` as an observation, and pair any test
/// results into pre-submit test telemetry.
fn handle_local_shell_call_output(agent: &mut DefaultAgent, parsed: &mut Parsed, msg: &Value) {
    let id = msg.get("id").and_then(Value::as_str).unwrap_or("");
    let output_obj = msg.get("output");

    let raw_output = output_obj
        .and_then(|o| o.get("output"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let exit_code = output_obj
        .and_then(|o| o.get("exit_code"))
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(0);

    // Pair with any pending test command.
    if !id.is_empty() {
        if let Some(pending) = parsed.pending_calls.remove(id) {
            let command = agent
                .redactor
                .redact_text(&pending.command, surface::TRAJECTORY)
                .text;
            parsed.test_invocations.push(TestInvocation {
                step_index: pending.step_index,
                command,
                exit_code,
                matched_pattern: pending.matched_pattern,
            });
        }
    }

    // Redact then truncate, matching the built-in loop's order.
    let redacted_raw = agent
        .redactor
        .redact_text(raw_output, surface::MODEL_OBSERVATION)
        .text;
    let redacted = truncate_observation_text(
        &redacted_raw,
        agent.config.root.agent.observation_max_bytes,
        agent.config.root.agent.observation_head_ratio,
    )
    .text;

    let ts = chrono::Utc::now().to_rfc3339();
    let has_error = exit_code != 0;
    let mut extra = MessageExtra {
        timestamp: Some(ts.clone()),
        ..Default::default()
    };
    if has_error {
        extra.other.insert("tool_error".into(), Value::Bool(true));
    }
    let run_result = crate::env::RunResult {
        stdout: redacted_raw,
        stderr: String::new(),
        exit_code,
        timed_out: false,
    };
    extra.other.insert(
        "run_result".into(),
        serde_json::to_value(&run_result).unwrap_or(Value::Null),
    );
    agent
        .trajectory
        .record_with_extra(&Message::user(redacted.clone()), extra);
    // Close the tool-activity span opened in `handle_local_shell_call` so the
    // dashboard (issue #649) leaves the running state before the observation
    // reopens the thinking window.
    agent.stream.emit(StreamEvent::ToolEnd {
        step: parsed.steps,
        timestamp: ts.clone(),
    });
    agent.stream.emit(StreamEvent::Observation {
        step: parsed.steps,
        content: redacted,
        timestamp: ts,
    });
}

/// Record a `message` event (assistant text turn) into the trajectory.
fn handle_message(agent: &mut DefaultAgent, parsed: &mut Parsed, msg: &Value) {
    if msg.get("role").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let text = msg
        .get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("output_text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if text.is_empty() {
        return;
    }

    if let Some(m) = msg.get("model").and_then(Value::as_str) {
        parsed.model = Some(m.to_owned());
    }

    let redacted = agent
        .redactor
        .redact_text(&text, surface::MODEL_OBSERVATION)
        .text;
    parsed.last_assistant_text.clone_from(&redacted);

    let ts = chrono::Utc::now().to_rfc3339();
    let extra = MessageExtra {
        timestamp: Some(ts.clone()),
        ..Default::default()
    };
    agent
        .trajectory
        .record_with_extra(&Message::assistant(redacted.clone()), extra);
    agent.stream.emit(StreamEvent::AssistantMessage {
        step: parsed.steps,
        content: redacted,
        cost_usd: None,
        timestamp: ts,
    });
}

fn parse_completed(msg: &Value) -> CompletedMsg {
    let usage = msg.get("usage");
    let u = |k: &str| -> u64 {
        usage
            .and_then(|x| x.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    CompletedMsg {
        exit_reason: msg
            .get("exit_reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        result: msg
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        cost_usd: msg.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
        input_tokens: u("input_tokens"),
        output_tokens: u("output_tokens"),
    }
}

/// Stamp the trajectory with cost/tokens/outcome from the parsed stream and
/// return the terminal [`ExitReason`].
#[allow(clippy::too_many_lines)]
fn finalize(
    agent: &mut DefaultAgent,
    mut parsed: Parsed,
    step_limit: u32,
    exit_code: Option<i32>,
    stderr_text: &str,
) -> Result<ExitReason, Error> {
    // Provenance block identifies the backend + session.
    if let Some(model) = parsed.model.clone() {
        agent.trajectory.info.model_name = Some(model);
    }
    let mut driver_meta = serde_json::Map::new();
    driver_meta.insert("driver".into(), Value::String("codex".into()));
    if let Some(s) = parsed.session_id {
        driver_meta.insert("session_id".into(), Value::String(s));
    }
    agent
        .trajectory
        .info
        .other
        .insert("codex_driver".into(), Value::Object(driver_meta));

    let Some(CompletedMsg {
        exit_reason,
        result: result_text,
        cost_usd,
        input_tokens,
        output_tokens,
    }) = parsed.result
    else {
        let snippet = agent
            .redactor
            .redact_text(stderr_text.trim(), surface::TRAJECTORY)
            .text;
        return Err(Error::Trajectory(format!(
            "codex driver produced no completed event (exit {:?}): {}",
            exit_code,
            truncate(&snippet, 500)
        )));
    };

    agent.steps = parsed.steps;
    agent.total_cost_usd = cost_usd;
    agent.actual_cost_source = Some(CostSource::ProviderReported);
    agent.prompt_tokens = input_tokens;
    agent.completion_tokens = output_tokens;

    agent.trajectory.info.steps = Some(parsed.steps);
    agent.trajectory.info.total_cost_usd = Some(cost_usd);
    agent.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
    agent.trajectory.info.token_usage = Some(TokenUsage {
        prompt_tokens: input_tokens,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: output_tokens,
    });

    let ran_tests = !parsed.test_invocations.is_empty();
    let last_tests_passed = parsed.test_invocations.last().map(|t| t.exit_code == 0);
    agent.trajectory.info.test_invocations = std::mem::take(&mut parsed.test_invocations);

    let is_max_turns = exit_reason == "max_turns";
    let is_success = matches!(exit_reason.as_str(), "done" | "success");

    let cost_limit_exceeded = agent
        .config
        .root
        .agent
        .cost_limit_usd
        .is_some_and(|cap| cost_usd >= cap);
    let budget_exhausted = !cost_limit_exceeded
        && agent
            .config
            .root
            .agent
            .per_task_budget_usd
            .is_some_and(|cap| cost_usd >= cap);

    let step_overflow = parsed.steps > step_limit;

    if is_success && !cost_limit_exceeded && !budget_exhausted && !step_overflow {
        // Use the `result` field from `completed` as final output; fall back
        // to the last assistant text turn if the field is empty.
        let raw_final = if result_text.is_empty() {
            parsed.last_assistant_text
        } else {
            result_text
        };
        let final_output = agent
            .redactor
            .redact_text(&raw_final, surface::TRAJECTORY)
            .text;
        agent.trajectory.info.exit_reason = Some("submitted".into());
        agent.trajectory.info.failure_category = None;
        agent.trajectory.info.final_output = Some(final_output.clone());
        agent.finalize_run_metadata(outcome::SUBMITTED);
        if ran_tests {
            agent.trajectory.info.tests_run_before_submit = true;
            agent.trajectory.info.last_tests_passed = last_tests_passed;
        }
        emit_ended(agent, "submitted", None, Some(final_output.clone()));
        Ok(ExitReason::Submitted { final_output })
    } else if cost_limit_exceeded {
        let limit_usd = agent.config.root.agent.cost_limit_usd.unwrap_or(0.0);
        agent.trajectory.info.exit_reason = Some("cost_limit".into());
        agent.trajectory.info.failure_category = Some(FailureCategory::CostLimit);
        agent.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
        emit_ended(agent, "cost_limit", Some(FailureCategory::CostLimit), None);
        Ok(ExitReason::CostLimit {
            limit_usd,
            spent_usd: cost_usd,
        })
    } else if budget_exhausted {
        let limit_usd = agent.config.root.agent.per_task_budget_usd.unwrap_or(0.0);
        agent.trajectory.info.exit_reason = Some("budget_exhausted".into());
        agent.trajectory.info.failure_category = Some(FailureCategory::BudgetExhausted);
        agent.finalize_run_metadata(outcome::BUDGET_EXHAUSTED);
        emit_ended(
            agent,
            "budget_exhausted",
            Some(FailureCategory::BudgetExhausted),
            None,
        );
        Ok(ExitReason::BudgetExhausted {
            limit_usd,
            spent_usd: cost_usd,
        })
    } else if is_max_turns || step_overflow {
        agent.trajectory.info.exit_reason = Some("step_limit".into());
        agent.trajectory.info.failure_category = Some(FailureCategory::StepLimit);
        agent.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
        emit_ended(agent, "step_limit", Some(FailureCategory::StepLimit), None);
        Ok(ExitReason::StepLimit { limit: step_limit })
    } else {
        agent.trajectory.info.exit_reason = Some("error".into());
        agent
            .trajectory
            .info
            .failure_category
            .get_or_insert(FailureCategory::AgentInternal);
        agent.trajectory.info.other.insert(
            "codex_exit_reason".into(),
            Value::String(exit_reason.clone()),
        );
        agent.finalize_run_metadata(outcome::ERROR);
        Err(Error::Trajectory(format!(
            "codex driver ended with non-success exit_reason: {exit_reason}"
        )))
    }
}

fn emit_ended(
    agent: &DefaultAgent,
    exit_reason: &str,
    failure_category: Option<FailureCategory>,
    final_output: Option<String>,
) {
    agent.stream.emit(StreamEvent::RunEnded {
        exit_reason: exit_reason.to_owned(),
        failure_category,
        final_output,
        steps: agent.steps,
        total_cost_usd: agent.total_cost_usd,
        ended_at: chrono::Utc::now().to_rfc3339(),
    });
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}
