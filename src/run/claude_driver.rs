//! Drive the Claude Code CLI (`claude`) as the agent backend.
//!
//! The harness ships a built-in bash-first loop, but this module lets an
//! operator point the *same* run machinery at the Claude Code CLI instead.
//! `claude` runs headless in `--output-format stream-json` mode; we read its
//! newline-delimited message stream and translate each message into the
//! harness [`Trajectory`](crate::trajectory::Trajectory) so that patch
//! capture, verification, `bench inspect`, and evaluation all keep working
//! unchanged. The exploration question this answers: *can a more capable
//! coding agent drive the loop and still leave us an inspectable receipt?*
//!
//! ## Seam
//!
//! [`drive`] takes a fully-built [`DefaultAgent`] — which already owns the
//! environment, redactor, and a trajectory seeded with the system + task
//! messages — and fills in the rest of the trajectory from Claude Code's
//! stream, mirroring the finalization that `DefaultAgent::step` performs on
//! its own terminal paths. It returns the same [`ExitReason`] the built-in
//! loop would, so `mini::run` can treat both backends identically.
//!
//! ## Scope (this slice)
//!
//! - Local environment only (the CLI edits the host working tree directly).
//! - Cost and tokens are read from Claude Code's authoritative `result`
//!   message (`CostSource::ProviderReported`).
//! - Model selection is delegated to Claude Code's own configuration; the
//!   actually-responding model is recorded back into the trajectory.
//! - The `claude` binary path is overridable via `MAXWELLS_CLAUDE_BIN` so the
//!   parser can be exercised deterministically at $0 with a fixture script.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::agent::{DefaultAgent, ExitReason};
use crate::cost::CostSource;
use crate::error::Error;
use crate::model::{Message, MessageExtra};
use crate::redaction::surface;
use crate::stream::StreamEvent;
use crate::trajectory::{FailureCategory, TokenUsage, outcome};

/// Full toolset handed to Claude Code. Mirrors the operator's choice to let
/// the agent edit directly; mutations still land in the working tree, so
/// `git diff` patch capture in `mini::run` is agnostic to *how* they were
/// made. Web/Task tools are intentionally omitted to keep runs local and
/// reproducible.
const ALLOWED_TOOLS: &str = "Bash Edit MultiEdit Write Read Glob Grep NotebookEdit";

/// Environment variable that overrides the `claude` binary path. Used by
/// tests to substitute a deterministic fixture script.
const CLAUDE_BIN_ENV: &str = "MAXWELLS_CLAUDE_BIN";

/// Drive a single run through the Claude Code CLI, filling `agent.trajectory`
/// and returning the terminal [`ExitReason`].
///
/// `workdir` is the directory `claude` runs in (and where edits land). When
/// `None`, the current process directory is used — matching the built-in
/// local environment's behavior.
pub async fn drive(
    agent: &mut DefaultAgent,
    task: String,
    workdir: Option<&Path>,
    timeout_secs: Option<u64>,
) -> Result<ExitReason, Error> {
    let bin = std::env::var(CLAUDE_BIN_ENV).unwrap_or_else(|_| "claude".to_owned());
    let cwd = match workdir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let step_limit = agent.config.root.agent.step_limit;

    if agent.config.root.model.name != "claude-opus-4-7" {
        // The default is just clap's placeholder; only warn when the operator
        // actively set a model, since we don't forward it to Claude Code.
        tracing::warn!(
            model = %agent.config.root.model.name,
            "--driver claude-code delegates model selection to Claude Code; \
             --model is not forwarded"
        );
    }

    let mut cmd = Command::new(&bin);
    cmd.arg("-p")
        .arg(&task)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--max-turns")
        .arg(step_limit.to_string())
        .arg("--allowedTools")
        .arg(ALLOWED_TOOLS)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    tracing::info!(bin = %bin, cwd = %cwd.display(), max_turns = step_limit, "spawning Claude Code driver");

    let mut child = cmd.spawn().map_err(|e| {
        Error::Trajectory(format!(
            "failed to spawn `{bin}` for --driver claude-code: {e}"
        ))
    })?;

    // Drain stderr concurrently so a chatty child can never deadlock us.
    let stderr_handle = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut buf = String::new();
            let mut r = BufReader::new(stderr);
            let _ = r.read_to_string(&mut buf).await;
            buf
        })
    });

    let process = process_stream(agent, &mut child, step_limit);

    let parsed = match timeout_secs {
        None => process.await?,
        Some(secs) => {
            let dur = Duration::from_secs(secs);
            let Ok(res) = tokio::time::timeout(dur, process).await else {
                let _ = child.start_kill();
                let _ = child.wait().await;
                agent.finalize_wallclock_timeout(dur);
                return Err(Error::Trajectory(format!(
                    "task wallclock timeout after {secs}s (claude driver)"
                )));
            };
            res?
        }
    };

    let status = child.wait().await?;
    let stderr_text = match stderr_handle {
        Some(h) => h.await.unwrap_or_default(),
        None => String::new(),
    };

    finalize(agent, parsed, step_limit, status.code(), &stderr_text)
}

/// Aggregated state pulled out of the Claude Code stream.
#[derive(Default)]
struct Parsed {
    /// Number of tool invocations — the closest analog to "agent steps".
    steps: u32,
    /// Model name Claude Code actually used (from `system/init` or `result`).
    model: Option<String>,
    /// Session id, recorded for provenance.
    session_id: Option<String>,
    claude_version: Option<String>,
    /// Terminal `result` message, if one was emitted.
    result: Option<ResultMsg>,
}

struct ResultMsg {
    subtype: String,
    is_error: bool,
    final_text: String,
    total_cost_usd: f64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    output_tokens: u64,
}

/// Read the NDJSON stream from `child`'s stdout, recording assistant turns and
/// tool observations into the trajectory as they arrive.
async fn process_stream(
    agent: &mut DefaultAgent,
    child: &mut tokio::process::Child,
    _step_limit: u32,
) -> Result<Parsed, Error> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Trajectory("claude child has no stdout pipe".into()))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = Parsed::default();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            // Non-JSON noise on stdout (shouldn't happen in stream-json) — skip.
            continue;
        };
        match msg.get("type").and_then(Value::as_str) {
            Some("system") => handle_system(&mut parsed, &msg),
            Some("assistant") => handle_assistant(agent, &mut parsed, &msg),
            Some("user") => handle_user(agent, &msg),
            Some("result") => parsed.result = Some(parse_result(&msg)),
            _ => {} // rate_limit_event, stream_event, etc. — ignored.
        }
    }

    Ok(parsed)
}

fn handle_system(parsed: &mut Parsed, msg: &Value) {
    if msg.get("subtype").and_then(Value::as_str) == Some("init") {
        parsed.session_id = msg
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        parsed.model = msg
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        parsed.claude_version = msg
            .get("claude_code_version")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
}

/// Record one assistant turn: join text blocks into the message content,
/// fold thinking into `extra.other`, and capture each `tool_use` as an action.
fn handle_assistant(agent: &mut DefaultAgent, parsed: &mut Parsed, msg: &Value) {
    let Some(blocks) = msg
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut actions: Vec<String> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(t.to_owned());
                }
            }
            Some("thinking") => {
                if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                    thinking_parts.push(t.to_owned());
                }
            }
            Some("tool_use") => {
                parsed.steps += 1;
                actions.push(tool_action_label(block));
            }
            _ => {}
        }
    }

    if let Some(m) = msg
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
    {
        parsed.model = Some(m.to_owned());
    }

    // Skip pure-thinking turns with no text and no action (keeps the
    // trajectory close to the built-in loop's one-action-per-turn shape).
    if text_parts.is_empty() && actions.is_empty() && thinking_parts.is_empty() {
        return;
    }

    let content = text_parts.join("\n");
    let redacted = agent
        .redactor
        .redact_text(&content, surface::MODEL_OBSERVATION)
        .text;

    let mut extra = MessageExtra {
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    if !actions.is_empty() {
        extra.actions = Some(actions);
    }
    if !thinking_parts.is_empty() {
        let thinking = agent
            .redactor
            .redact_text(&thinking_parts.join("\n"), surface::MODEL_OBSERVATION)
            .text;
        extra
            .other
            .insert("thinking".into(), Value::String(thinking));
    }

    agent
        .trajectory
        .record_with_extra(&Message::assistant(redacted), extra);
}

/// Build a one-line action label for a `tool_use` block, mirroring the
/// built-in loop's `extra.actions` convention (bash command verbatim; other
/// tools as `Name(target)`).
fn tool_action_label(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
    let input = block.get("input");
    if name == "Bash" {
        if let Some(cmd) = input.and_then(|i| i.get("command")).and_then(Value::as_str) {
            return cmd.to_owned();
        }
    }
    if let Some(path) = input
        .and_then(|i| i.get("file_path").or_else(|| i.get("path")))
        .and_then(Value::as_str)
    {
        return format!("{name}({path})");
    }
    if let Some(pattern) = input.and_then(|i| i.get("pattern")).and_then(Value::as_str) {
        return format!("{name}({pattern})");
    }
    name.to_owned()
}

/// Record a `tool_result` (delivered as a `user` message) as an observation.
fn handle_user(agent: &mut DefaultAgent, msg: &Value) {
    let Some(blocks) = msg
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = tool_result_text(block.get("content"));
        let redacted = agent
            .redactor
            .redact_text(&text, surface::MODEL_OBSERVATION)
            .text;
        let mut extra = MessageExtra {
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        if is_error {
            extra.other.insert("tool_error".into(), Value::Bool(true));
        }
        agent
            .trajectory
            .record_with_extra(&Message::user(redacted), extra);
    }
}

/// Flatten a `tool_result` `content` field, which is either a string or an
/// array of `{type:"text", text}` blocks.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_result(msg: &Value) -> ResultMsg {
    let usage = msg.get("usage");
    let u = |k: &str| -> u64 {
        usage
            .and_then(|x| x.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    ResultMsg {
        subtype: msg
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        is_error: msg
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        final_text: msg
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        total_cost_usd: msg
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        input_tokens: u("input_tokens"),
        cache_read_tokens: u("cache_read_input_tokens"),
        cache_creation_tokens: u("cache_creation_input_tokens"),
        output_tokens: u("output_tokens"),
    }
}

/// Stamp the trajectory with cost/tokens/outcome from the parsed stream and
/// return the terminal [`ExitReason`], mirroring `DefaultAgent`'s own
/// finalization on each terminal path.
fn finalize(
    agent: &mut DefaultAgent,
    parsed: Parsed,
    step_limit: u32,
    exit_code: Option<i32>,
    stderr_text: &str,
) -> Result<ExitReason, Error> {
    // Provenance: record the Claude Code session so the trajectory is
    // self-describing about which backend produced it.
    if let Some(model) = parsed.model.clone() {
        agent.trajectory.info.model_name = Some(model);
    }
    let mut driver_meta = serde_json::Map::new();
    driver_meta.insert("driver".into(), Value::String("claude-code".into()));
    if let Some(s) = parsed.session_id {
        driver_meta.insert("session_id".into(), Value::String(s));
    }
    if let Some(v) = parsed.claude_version {
        driver_meta.insert("claude_code_version".into(), Value::String(v));
    }
    agent
        .trajectory
        .info
        .other
        .insert("claude_driver".into(), Value::Object(driver_meta));

    let Some(result) = parsed.result else {
        // No terminal result line: the CLI crashed or was killed. Surface
        // stderr (redacted) and let mini::run finalize as an error.
        let snippet = agent
            .redactor
            .redact_text(stderr_text.trim(), surface::TRAJECTORY)
            .text;
        return Err(Error::Trajectory(format!(
            "claude driver produced no result message (exit {:?}): {}",
            exit_code,
            truncate(&snippet, 500)
        )));
    };

    // Cost and tokens are authoritative from the result message.
    agent.steps = parsed.steps;
    agent.total_cost_usd = result.total_cost_usd;
    agent.actual_cost_source = Some(CostSource::ProviderReported);
    agent.prompt_tokens = result.input_tokens;
    agent.cache_read_tokens = result.cache_read_tokens;
    agent.cache_creation_tokens = result.cache_creation_tokens;
    agent.completion_tokens = result.output_tokens;

    agent.trajectory.info.steps = Some(parsed.steps);
    agent.trajectory.info.total_cost_usd = Some(result.total_cost_usd);
    agent.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
    agent.trajectory.info.token_usage = Some(TokenUsage {
        prompt_tokens: result.input_tokens,
        cache_read_tokens: result.cache_read_tokens,
        cache_creation_tokens: result.cache_creation_tokens,
        completion_tokens: result.output_tokens,
    });

    let is_max_turns = result.subtype.contains("max_turns");

    if result.subtype == "success" && !result.is_error {
        let final_output = agent
            .redactor
            .redact_text(&result.final_text, surface::TRAJECTORY)
            .text;
        agent.trajectory.info.exit_reason = Some("submitted".into());
        agent.trajectory.info.failure_category = None;
        agent.trajectory.info.final_output = Some(final_output.clone());
        agent.finalize_run_metadata(outcome::SUBMITTED);
        emit_ended(agent, "submitted", None, Some(final_output.clone()));
        Ok(ExitReason::Submitted { final_output })
    } else if is_max_turns {
        agent.trajectory.info.exit_reason = Some("step_limit".into());
        agent.trajectory.info.failure_category = Some(FailureCategory::StepLimit);
        agent.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
        emit_ended(agent, "step_limit", Some(FailureCategory::StepLimit), None);
        Ok(ExitReason::StepLimit { limit: step_limit })
    } else {
        // Any other terminal result (error subtype) — record as error and let
        // mini::run skip patch capture/verification.
        agent.trajectory.info.exit_reason = Some("error".into());
        agent
            .trajectory
            .info
            .failure_category
            .get_or_insert(FailureCategory::AgentInternal);
        agent.trajectory.info.other.insert(
            "claude_result_subtype".into(),
            Value::String(result.subtype.clone()),
        );
        agent.finalize_run_metadata(outcome::ERROR);
        Err(Error::Trajectory(format!(
            "claude driver ended with non-success result: {}",
            result.subtype
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
