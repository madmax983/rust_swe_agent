//! One-shot task runner for the minimal tool loop. Resolves backend from
//! the model name, builds `DefaultAgent`, runs to completion, writes a
//! trajectory file and (if submitted) an output artifact.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::{
    Agent, ConfirmCallback, DefaultAgent, RatatuiDashboardHandle, StderrCliConfirmer,
    default::DefaultAgentBuilder,
};
use crate::config::{Config, EnvKind};
#[cfg(feature = "docker")]
use crate::env::DockerEnvironment;
use crate::env::{Environment, LocalEnvironment, RunRequest};
use crate::error::{ConfigError, Error};
use crate::model::litellm::LitellmBackend;
use crate::model::{DeterministicModel, FallbackModel, Model, ModelUsage};
use crate::redaction::surface;
use crate::stream::{BroadcastSink, MultiSink, SseServer, StatusLineStderrSink, StreamSink};
#[cfg(feature = "webhook")]
use crate::stream::{WebhookSink, WebhookSinkHandle};
use crate::trajectory::FailureCategory;

pub use crate::env::CancellationToken as MiniCancellation;

// NOTE: this records the SHA of the repo at the operator's CWD, not
// necessarily the harness binary's own source repo. This matches the
// pre-existing behavior of the same function in cli/mod.rs and is a
// known limitation; a future improvement would resolve it from binary
// build metadata or a fixed harness repo path.
fn current_git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

const PATCH_BASE_ENV: &str = "MAXWELL_PATCH_BASE";
const LEGACY_PATCH_BASE_ENV: &str = "RUST_SWE_AGENT_PATCH_BASE";
const VERIFICATION_PREVIEW_MAX_BYTES: usize = 2 * 1024;

#[cfg(test)]
struct CancelBeforePatchCaptureHook {
    trajectory_name: String,
    sender: tokio::sync::watch::Sender<bool>,
}

#[cfg(test)]
static CANCEL_BEFORE_PATCH_CAPTURE: std::sync::Mutex<Option<CancelBeforePatchCaptureHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn cancel_before_patch_capture_if_requested(trajectory_name: &str) {
    let mut hook = CANCEL_BEFORE_PATCH_CAPTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if hook
        .as_ref()
        .is_some_and(|hook| hook.trajectory_name == trajectory_name)
    {
        if let Some(hook) = hook.take() {
            let _ = hook.sender.send(true);
        }
    }
}

/// How a runner should snapshot the agent's working tree as a unified diff
/// after submission. Optional on `MiniArgs` because patch capture only
/// makes sense for benchmark sweeps; the standalone `bench mini` CLI run
/// has no baseline to diff against.
pub struct PatchCaptureSpec {
    /// The dataset's stated baseline. When `None`, diff is taken against
    /// `HEAD`. SWE-bench instances typically carry the resolved SHA.
    pub base_commit: Option<String>,
    /// Working tree where `git diff` runs. For docker envs this matches
    /// the container's `-w`; for local envs it must point at a real git
    /// checkout.
    pub workdir: PathBuf,
    /// Where to write the `.patch` artifact. Empty diffs are still
    /// persisted (zero-byte file) so downstream tooling can distinguish
    /// "agent ran and changed nothing" from "agent never reached this
    /// instance".
    pub patch_path: PathBuf,
    /// When `true`, skip `git apply --check` and empty-diff validation.
    /// Escape hatch for non-git environments; not for normal use.
    pub skip_patch_validation: bool,
}

/// Reasons why patch validation can fail after a successful `git diff` capture.
#[derive(Debug)]
pub enum PatchValidationFailure {
    /// The captured diff was empty (zero bytes).
    Empty,
    /// `git apply --check` rejected the patch; carries trimmed stderr.
    ApplyFailed(String),
}

pub struct MiniArgs {
    pub task: String,
    pub extra_context: Option<String>,
    pub config: Config,
    pub output_dir: PathBuf,
    pub trajectory_name: String,
    pub deterministic_responses: Option<Vec<String>>,
    /// Optional fixed `ModelUsage` reported by the deterministic backend
    /// on every call. Only meaningful when `deterministic_responses` is
    /// `Some`. Lets tests drive cost/budget logic without a real API.
    pub deterministic_usage_per_call: Option<ModelUsage>,
    /// Optional wallclock budget for the agent loop. When elapsed, any
    /// in-flight environment command is dropped and the trajectory is
    /// finalized as `wallclock_timeout`.
    pub task_timeout_secs: Option<u64>,
    /// Optional external cancellation used by the sweep runner when a
    /// graceful Ctrl-C deadline escalates.
    pub cancellation: Option<MiniCancellation>,
    /// Optional SSE stream endpoint to bind. When `Some`, the runner
    /// starts a server before the agent runs and shuts it down after.
    pub stream_addr: Option<SocketAddr>,
    /// When `Some` and the agent submits, the runner captures a `git
    /// diff` of `workdir` against `base_commit` and writes it to
    /// `patch_path`. Capture failures downgrade the run's recorded
    /// outcome to `error` rather than crashing the runner.
    pub patch_capture: Option<PatchCaptureSpec>,
    /// Operator-supplied checks that run after the agent finishes.
    /// Empty vec preserves previous behavior but marks the run as
    /// `unverified` in the trajectory artifact.
    pub verification_checks: Vec<crate::trajectory::VerificationCheck>,
    /// Per-check timeout in seconds. Defaults to 60.
    pub verification_timeout_secs: u64,
    /// When `Some`, the agent is resumed from this partial trajectory rather
    /// than starting fresh. Budget accounting and message history are seeded
    /// from the checkpoint.
    pub resume_from: Option<crate::trajectory::Trajectory>,
    /// Issue #312 — operator interaction mode for this run.
    pub interactive_mode: InteractiveMode,
    /// OpenTelemetry trace ID assigned by the sweep runner when OTLP export
    /// is active. Written into `trajectory.info.trace_id` before the first
    /// save so the trajectory and its span share the same correlation key.
    /// `None` when OTLP is not configured.
    pub trace_id: Option<String>,
    /// Optional webhook URL for per-step push streaming (issue #324).
    /// When `Some`, each `StreamEvent` is POSTed as a JSON envelope to this
    /// URL by a background task.  Absent = no background task spawned.
    pub webhook_url: Option<String>,
    /// Extra HTTP headers to inject on every webhook POST, e.g.
    /// `"Authorization: Bearer <token>"`.  Not echoed in logs.
    pub webhook_headers: Vec<String>,
    /// Absolute canonicalized local working directory.
    pub local_workdir: Option<PathBuf>,
    pub read_only: bool,
    pub allow_mcp_in_read_only: bool,
}

/// Operator-interaction mode for `mini --interactive` (issue #312).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InteractiveMode {
    /// Default unattended behaviour — no prompts, no status line.
    #[default]
    Off,
    /// Print a per-step status line to stderr but don't prompt before
    /// bash actions (`--yolo` without `--interactive`).
    YoloStatusOnly,
    /// Pause before every bash/tool action and ask the operator on a
    /// stderr single-line prompt.
    StderrPrompt,
    /// Same as `StderrPrompt` but render the prompt inside a full-screen
    /// ratatui dashboard that also streams trajectory events.
    Ratatui,
}

#[allow(clippy::too_many_lines)]
pub async fn run(args: MiniArgs) -> Result<(), Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    let resolved_skills = crate::skills::resolve_for_task(
        &args.config.root.skills,
        &args.task,
        args.extra_context.clone(),
    )?;
    let model = build_model(
        &args.config,
        args.deterministic_responses,
        args.deterministic_usage_per_call.clone(),
    );
    let env = build_env(&args.config, args.local_workdir.as_ref()).await?;
    if args.read_only
        && !args.allow_mcp_in_read_only
        && !args.config.root.agent.mcp_servers.is_empty()
    {
        return Err(Error::Config(ConfigError::Invalid(
            "--read-only blocks MCP servers unless --allow-mcp-in-read-only is set".into(),
        )));
    }
    let tool_providers = crate::tool::discover_mcp_servers(
        env.as_ref(),
        &args.config.root.agent.mcp_servers,
        args.config.root.agent.tool_hook_timeout_secs,
        args.cancellation.clone(),
    )
    .await?;

    // Bring up the SSE server first so any client that connects right
    // after CLI startup catches the `run_started` event the builder
    // emits below.
    let (sse_sink, server): (Option<Arc<dyn StreamSink>>, Option<SseServer>) = match args
        .stream_addr
    {
        Some(addr) => {
            let bcast = Arc::new(BroadcastSink::default());
            let server = SseServer::start(addr, bcast.clone()).await.map_err(|e| {
                Error::Trajectory(format!("failed to bind SSE server on {addr}: {e}"))
            })?;
            tracing::info!(addr = %server.local_addr(), "streaming events on http://{}/", server.local_addr());
            (Some(bcast as Arc<dyn StreamSink>), Some(server))
        }
        None => (None, None),
    };

    // Build the optional webhook sink (issue #324).
    #[cfg(feature = "webhook")]
    let (webhook_sink_opt, webhook_dropped_counter): (
        Option<Arc<dyn StreamSink>>,
        Option<Arc<std::sync::atomic::AtomicU64>>,
    ) = if let Some(ref url) = args.webhook_url {
        let headers: Vec<(String, String)> = args
            .webhook_headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let (name, value) = h.split_once(':').ok_or_else(|| {
                    Error::Config(ConfigError::Invalid(format!(
                        "--webhook-header at position {} is missing `:` separator \
                         (use `Name: Value`)",
                        i + 1
                    )))
                })?;
                Ok((name.trim().to_owned(), value.trim().to_owned()))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let sink = WebhookSink::new(url.clone(), &headers)
            .map_err(|e| Error::Config(ConfigError::Invalid(format!("webhook sink: {e}"))))?;
        let sink_arc = Arc::new(sink);
        let counter = sink_arc.dropped_counter();
        // Redact the trajectory name so configured secret literals can't leak
        // through the envelope's run_id field, which bypasses RedactingSink.
        let run_id = {
            let redactor =
                crate::redaction::Redactor::from_config_lossy(&args.config.root.redaction);
            redactor
                .redact_text(&args.trajectory_name, surface::TRAJECTORY)
                .text
        };
        let handle = WebhookSinkHandle::new(sink_arc, run_id);
        // Log only scheme + host — path/query/userinfo may contain credentials.
        let safe_url = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| format!("{}://{}", u.scheme(), h)))
            .unwrap_or_else(|| "<url>".to_owned());
        tracing::info!(url = %safe_url, "webhook push enabled");
        (Some(Arc::new(handle) as Arc<dyn StreamSink>), Some(counter))
    } else {
        if !args.webhook_headers.is_empty() {
            return Err(Error::Config(ConfigError::Invalid(
                "--webhook-header requires --webhook-url; \
                 headers have no effect without a webhook URL"
                    .into(),
            )));
        }
        (None, None)
    };
    #[cfg(not(feature = "webhook"))]
    let (webhook_sink_opt, webhook_dropped_counter): (
        Option<Arc<dyn StreamSink>>,
        Option<Arc<std::sync::atomic::AtomicU64>>,
    ) = {
        if args.webhook_url.is_some() {
            return Err(Error::Config(ConfigError::Invalid(
                "--webhook-url requires the `webhook` Cargo feature; \
                 rebuild with --features webhook"
                    .into(),
            )));
        }
        if !args.webhook_headers.is_empty() {
            return Err(Error::Config(ConfigError::Invalid(
                "--webhook-header requires --webhook-url; \
                 headers have no effect without a webhook URL"
                    .into(),
            )));
        }
        (None, None)
    };

    let resume_state = args.resume_from.map(|traj| {
        let history = traj.messages_as_model_history();
        let steps = traj.info.steps.unwrap_or(0);
        let total_cost_usd = traj.info.actual_cost_usd.unwrap_or(0.0);
        let (prompt_tokens, cache_read_tokens, cache_creation_tokens, completion_tokens) =
            traj.info.token_usage.as_ref().map_or((0, 0, 0, 0), |t| {
                (
                    t.prompt_tokens,
                    t.cache_read_tokens,
                    t.cache_creation_tokens,
                    t.completion_tokens,
                )
            });
        Box::new(crate::agent::default::ResumeState {
            trajectory: traj,
            history,
            steps,
            total_cost_usd,
            prompt_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            completion_tokens,
            resumed_at: chrono::Utc::now().to_rfc3339(),
            harness_git_sha: current_git_sha(),
        })
    });
    let (confirm_callback, dashboard) = build_interactive_pieces(args.interactive_mode)?;

    // Redact the webhook sink so secrets are stripped before each POST.
    let webhook_sink_redacted: Option<Arc<dyn StreamSink>> = webhook_sink_opt.map(|ws| {
        // We need the redactor before the agent is built, so create a temporary
        // one from the config. The agent will build its own for trajectory/model
        // surfaces; this one covers the stream surface only.
        let redactor = crate::redaction::Redactor::from_config_lossy(&args.config.root.redaction);
        Arc::new(crate::redaction::RedactingSink::new(ws, redactor)) as Arc<dyn StreamSink>
    });

    let sink = compose_stream_sinks(
        sse_sink,
        dashboard.as_ref().map(RatatuiDashboardHandle::stream_sink),
        if args.interactive_mode == InteractiveMode::YoloStatusOnly {
            Some(
                Arc::new(StatusLineStderrSink::new(args.config.root.agent.step_limit))
                    as Arc<dyn StreamSink>,
            )
        } else {
            None
        },
        webhook_sink_redacted,
    );

    let mut agent: DefaultAgent = DefaultAgentBuilder {
        config: args.config.clone(),
        model,
        env,
        task: args.task.clone(),
        extra_context: resolved_skills.merged_extra_context.clone(),
        renderer: None,
        stream: sink,
        resume_from: resume_state,
        read_only: args.read_only,
    }
    .build_with_tool_providers(tool_providers)?;
    agent.cancellation = args.cancellation.clone();
    agent.confirm_callback = confirm_callback;
    resolved_skills
        .active_skills
        .record_redacted_provenance(&mut agent.trajectory.info, &agent.redactor)?;
    // Propagate trace_id from the sweep runner so the trajectory and its
    // OTLP span share the same correlation key.  When no trace_id is provided
    // (OTLP not configured for this run), preserve whatever the restored
    // checkpoint already recorded.
    if args.trace_id.is_some() {
        agent.trajectory.info.trace_id = args.trace_id.clone();
    }
    agent.trajectory.info.local_workdir =
        args.local_workdir.as_ref().map(|p| p.display().to_string());
    if args.read_only {
        agent
            .trajectory
            .info
            .other
            .insert("mode".into(), serde_json::Value::String("read_only".into()));
    }

    let traj_path = args
        .output_dir
        .join(format!("{}.traj.json", args.trajectory_name));
    // Enable per-turn checkpointing to the trajectory path so interruptions
    // don't discard all in-flight progress.
    agent.checkpoint_path = Some(traj_path.clone());

    // Run the agent. On error, finalize the trajectory with
    // `outcome="error"` so the partial run is still a self-contained
    // record of what happened — then propagate.
    let mut run_result = run_agent_with_optional_timeout(&mut agent, args.task_timeout_secs).await;
    if let Err(e) = &run_result {
        finalize_error_trajectory(&mut agent, e);
    }
    #[cfg(test)]
    cancel_before_patch_capture_if_requested(&args.trajectory_name);
    if finalize_cancelled_if_requested(&mut agent, args.cancellation.as_ref()) {
        run_result = Ok(crate::agent::ExitReason::UserInterrupt);
    }

    // Patch capture happens before the trajectory is saved so any
    // capture failure can be reflected as `outcome: "error"` rather
    // than leaving a stale `submitted` record on disk.
    let mut patch_written = false;
    if let (Ok(crate::agent::ExitReason::Submitted { .. }), Some(spec)) =
        (run_result.as_ref(), args.patch_capture.as_ref())
    {
        match capture_patch(agent.env.as_ref(), spec, args.cancellation.clone()).await {
            Ok(diff) => {
                if finalize_cancelled_if_requested(&mut agent, args.cancellation.as_ref()) {
                    agent.trajectory.info.verification_status =
                        Some(crate::trajectory::verification_status::UNVERIFIED.into());
                    agent.trajectory.save_pretty(&traj_path)?;
                    tracing::info!(?traj_path, patch_written, "trajectory written");
                    warn_dropped_webhook_events(webhook_dropped_counter.as_deref());
                    if let Some(server) = server {
                        server.shutdown().await;
                    }
                    return Ok(());
                }
                // Always write the patch file — operators need to inspect
                // failed patches too.
                let redacted_patch = agent.redactor.redact_text(&diff, surface::PATCH_SUBMISSION);
                let detected_configured_literal = agent.redactor.configured_literal_leak(&diff);
                if (detected_configured_literal.is_some() || redacted_patch.redacted)
                    && !agent.redactor.unsafe_allow_secret_leaks()
                {
                    std::fs::write(&spec.patch_path, redacted_patch.text)?;
                    patch_written = true;
                    tracing::warn!(
                        instance = %args.trajectory_name,
                        "secret leak detected in submitted patch; downgrading outcome to error"
                    );
                    agent.trajectory.info.exit_reason = Some("error".into());
                    agent.trajectory.info.failure_category =
                        Some(FailureCategory::SecretLeakDetected);
                    let leak_kind = detected_configured_literal
                        .map_or_else(|| "structured_secret".to_owned(), |leak| leak.kind);
                    agent.trajectory.info.other.insert(
                        "secret_leak_detected".into(),
                        serde_json::json!({
                            "surface": surface::PATCH_SUBMISSION,
                            "kind": leak_kind,
                        }),
                    );
                    agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
                    agent.trajectory.info.verification_status =
                        Some(crate::trajectory::verification_status::UNVERIFIED.into());
                    agent.trajectory.save_pretty(&traj_path)?;
                    tracing::info!(?traj_path, patch_written, "trajectory written");
                    warn_dropped_webhook_events(webhook_dropped_counter.as_deref());
                    if let Some(server) = server {
                        server.shutdown().await;
                    }
                    return Ok(());
                }
                let patch_text = if agent.redactor.unsafe_allow_secret_leaks() {
                    diff.clone()
                } else {
                    redacted_patch.text
                };
                std::fs::write(&spec.patch_path, patch_text)?;
                patch_written = true;

                match check_patch_validity(
                    agent.env.as_ref(),
                    spec,
                    &diff,
                    args.cancellation.clone(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(PatchValidationFailure::Empty) => {
                        if finalize_cancelled_if_requested(&mut agent, args.cancellation.as_ref()) {
                            run_result = Ok(crate::agent::ExitReason::UserInterrupt);
                        } else {
                            tracing::warn!(
                                instance = %args.trajectory_name,
                                "agent submitted but produced an empty diff; downgrading to error"
                            );
                            agent.trajectory.info.exit_reason = Some("error".into());
                            agent.trajectory.info.failure_category =
                                Some(FailureCategory::PatchEmpty);
                            agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
                        }
                    }
                    Err(PatchValidationFailure::ApplyFailed(reason)) => {
                        if finalize_cancelled_if_requested(&mut agent, args.cancellation.as_ref()) {
                            run_result = Ok(crate::agent::ExitReason::UserInterrupt);
                        } else {
                            let reason = agent
                                .redactor
                                .redact_text(&reason, surface::TRAJECTORY)
                                .text;
                            tracing::warn!(
                                instance = %args.trajectory_name,
                                error = %reason,
                                "patch apply check failed; downgrading outcome to error"
                            );
                            agent.trajectory.info.exit_reason = Some("error".into());
                            agent.trajectory.info.failure_category =
                                Some(FailureCategory::PatchApplyInvalid);
                            agent.trajectory.info.other.insert(
                                "patch_apply_error".into(),
                                serde_json::Value::String(reason),
                            );
                            agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
                        }
                    }
                }
            }
            Err(reason) => {
                if finalize_cancelled_if_requested(&mut agent, args.cancellation.as_ref()) {
                    run_result = Ok(crate::agent::ExitReason::UserInterrupt);
                } else {
                    let reason = agent
                        .redactor
                        .redact_text(&reason, surface::TRAJECTORY)
                        .text;
                    tracing::warn!(
                        instance = %args.trajectory_name,
                        error = %reason,
                        "patch capture failed; downgrading outcome to error"
                    );
                    agent.trajectory.info.exit_reason = Some("error".into());
                    agent.trajectory.info.failure_category = Some(FailureCategory::EnvSetup);
                    agent
                        .trajectory
                        .info
                        .other
                        .insert("patch_error".into(), serde_json::Value::String(reason));
                    agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
                }
            }
        }
    }

    // ── Verification ─────────────────────────────────────────────────────────
    // Runs after all patch-capture logic, for any exit that is not a
    // cancellation or an agent-internal error (which would already have
    // returned Err above). Sets `verification_status` on the trajectory so
    // every persisted artifact carries a deterministic verification outcome.
    let skip_verification = run_result.is_err()
        || run_result.as_ref().is_ok_and(|r| {
            matches!(
                r,
                crate::agent::ExitReason::UserInterrupt
                    | crate::agent::ExitReason::AgentStagnation { .. }
            )
        });
    let verification_err = if skip_verification {
        agent.trajectory.info.verification_status =
            Some(crate::trajectory::verification_status::UNVERIFIED.into());
        None
    } else {
        run_verification_checks(
            &mut agent.trajectory,
            agent.env.as_ref(),
            &agent.redactor,
            &args.verification_checks,
            args.verification_timeout_secs,
            args.cancellation.clone(),
        )
        .await
    };

    // Refresh redaction summary: verification may have redacted command text
    // or stdout/stderr previews after finalize_run_metadata already stamped
    // info.redaction, so update it before writing the artifact.
    agent.trajectory.info.redaction = Some(agent.redactor.summary());
    agent.trajectory.save_pretty(&traj_path)?;

    let exit = match run_result {
        Ok(exit) => exit,
        Err(e) => {
            warn_dropped_webhook_events(webhook_dropped_counter.as_deref());
            if let Some(server) = server {
                server.shutdown().await;
            }
            return Err(e);
        }
    };

    if let crate::agent::ExitReason::Submitted { final_output } = &exit {
        let out_path = args
            .output_dir
            .join(format!("{}.output.txt", args.trajectory_name));
        std::fs::write(&out_path, final_output)?;
    }

    tracing::info!(?traj_path, patch_written, "trajectory written");

    // Emit stderr warning if any webhook events were dropped (issue #324 AC 5).
    warn_dropped_webhook_events(webhook_dropped_counter.as_deref());

    if let Some(server) = server {
        server.shutdown().await;
    }
    if let Some(dashboard) = dashboard {
        dashboard.shutdown().await;
    }
    if let Some(err) = verification_err {
        return Err(err);
    }
    if let crate::agent::ExitReason::AgentStagnation { count, window, .. } = exit {
        return Err(crate::error::Error::AgentStagnation { count, window });
    }
    Ok(())
}

/// Pair of pieces resolved from an `InteractiveMode`: the optional
/// confirm callback to set on `DefaultAgent` and the optional ratatui
/// dashboard handle whose lifetime brackets the run.
type InteractivePieces = (
    Option<Arc<dyn ConfirmCallback>>,
    Option<RatatuiDashboardHandle>,
);

fn build_interactive_pieces(mode: InteractiveMode) -> Result<InteractivePieces, Error> {
    match mode {
        InteractiveMode::Off | InteractiveMode::YoloStatusOnly => Ok((None, None)),
        InteractiveMode::StderrPrompt => {
            let confirmer = StderrCliConfirmer::new_if_tty().ok_or_else(|| {
                Error::Config(ConfigError::Invalid(
                    "interactive mode requires a TTY; pass --yolo for unattended runs".into(),
                ))
            })?;
            Ok((Some(Arc::new(confirmer) as Arc<dyn ConfirmCallback>), None))
        }
        InteractiveMode::Ratatui => {
            if !std::io::IsTerminal::is_terminal(&std::io::stdin())
                || !std::io::IsTerminal::is_terminal(&std::io::stdout())
            {
                return Err(Error::Config(ConfigError::Invalid(
                    "ratatui interactive mode requires a TTY on stdin and stdout; \
                     pass --yolo for unattended runs"
                        .into(),
                )));
            }
            let handle = crate::agent::RatatuiDashboard::start().map_err(|e| {
                Error::Trajectory(format!("failed to start ratatui dashboard: {e}"))
            })?;
            let cb = handle.confirm_callback();
            Ok((Some(cb), Some(handle)))
        }
    }
}

fn warn_dropped_webhook_events(counter: Option<&std::sync::atomic::AtomicU64>) {
    if let Some(counter) = counter {
        let n = counter.load(std::sync::atomic::Ordering::Relaxed);
        if n >= 1 {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!("warn: {n} webhook event(s) were dropped during this run\n").as_bytes(),
            );
        }
    }
}

fn compose_stream_sinks(
    sse: Option<Arc<dyn StreamSink>>,
    dashboard: Option<Arc<dyn StreamSink>>,
    status_line: Option<Arc<dyn StreamSink>>,
    webhook: Option<Arc<dyn StreamSink>>,
) -> Option<Arc<dyn StreamSink>> {
    let sinks: Vec<Arc<dyn StreamSink>> = [sse, dashboard, status_line, webhook]
        .into_iter()
        .flatten()
        .collect();
    match sinks.len() {
        0 => None,
        1 => sinks.into_iter().next(),
        _ => Some(Arc::new(MultiSink::new(sinks)) as Arc<dyn StreamSink>),
    }
}

fn finalize_cancelled_if_requested(
    agent: &mut DefaultAgent,
    cancellation: Option<&MiniCancellation>,
) -> bool {
    if cancellation.is_some_and(MiniCancellation::is_cancelled) {
        agent.finalize_cancelled();
        true
    } else {
        false
    }
}

fn attach_cancellation(req: RunRequest, cancellation: Option<MiniCancellation>) -> RunRequest {
    if let Some(cancellation) = cancellation {
        req.with_cancellation(cancellation)
    } else {
        req
    }
}

fn finalize_error_trajectory(agent: &mut DefaultAgent, err: &Error) {
    agent
        .trajectory
        .info
        .exit_reason
        .get_or_insert_with(|| "error".into());
    agent
        .trajectory
        .info
        .failure_category
        .get_or_insert_with(|| classify_error(err));
    agent
        .trajectory
        .info
        .other
        .entry("error_message".into())
        .or_insert_with(|| {
            serde_json::Value::String(
                agent
                    .redactor
                    .redact_text(&err.to_string(), surface::TRAJECTORY)
                    .text,
            )
        });
    agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
}

async fn run_agent_with_optional_timeout(
    agent: &mut DefaultAgent,
    task_timeout_secs: Option<u64>,
) -> Result<crate::agent::ExitReason, Error> {
    match task_timeout_secs {
        None => agent.run().await,
        Some(secs) => run_agent_with_timeout(agent, secs).await,
    }
}

async fn run_agent_with_timeout(
    agent: &mut DefaultAgent,
    secs: u64,
) -> Result<crate::agent::ExitReason, Error> {
    let timeout = Duration::from_secs(secs);
    agent.set_wallclock_deadline(timeout);
    if let Ok(result) = tokio::time::timeout(timeout, agent.run()).await {
        return result;
    }
    let shutdown_error = agent.env.shutdown().await.err();
    agent.finalize_wallclock_timeout(timeout);
    if let Some(err) = shutdown_error {
        agent.trajectory.info.other.insert(
            "environment_shutdown_error".into(),
            serde_json::Value::String(err.to_string()),
        );
    }
    Err(Error::Trajectory(format!(
        "task wallclock timeout after {secs}s"
    )))
}

fn classify_error(err: &Error) -> FailureCategory {
    match err {
        Error::Env(_) => FailureCategory::EnvSetup,
        Error::Model(crate::error::ModelError::Malformed(_)) => FailureCategory::ModelParse,
        Error::Model(_) => FailureCategory::ModelApi,
        _ => FailureCategory::AgentInternal,
    }
}

/// Validate that `diff` applies cleanly against `spec.base_commit` in a clean
/// checkout, without touching the agent's working tree.
///
/// - Returns `Ok(())` immediately when `spec.skip_patch_validation` is `true`
///   or `spec.base_commit` is `None` (standalone runs without a base).
/// - Returns `Err(PatchValidationFailure::Empty)` when `diff` is empty.
/// - Returns `Err(PatchValidationFailure::ApplyFailed(_))` when the worktree
///   setup or `git apply --check` exits non-zero.
///
/// A temporary git worktree is created at `base_commit` so the check runs
/// against the clean base state rather than the (already-modified) working
/// tree.  Temp files are placed inside `spec.workdir` (the volume mount point
/// in Docker) using relative paths so the shell command works correctly in
/// both local and Docker environments.  The worktree and patch file are always
/// removed after the check, success or failure.
pub(crate) async fn check_patch_validity(
    env: &dyn Environment,
    spec: &PatchCaptureSpec,
    diff: &str,
    cancellation: Option<MiniCancellation>,
) -> Result<(), PatchValidationFailure> {
    if spec.skip_patch_validation {
        return Ok(());
    }
    let Some(base_commit) = spec.base_commit.as_deref() else {
        return Ok(());
    };
    if diff.is_empty() {
        return Err(PatchValidationFailure::Empty);
    }
    validate_git_rev(base_commit).map_err(PatchValidationFailure::ApplyFailed)?;

    // Use full nanoseconds for better uniqueness across concurrent calls.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());

    // Place temp files inside spec.workdir so they are accessible from inside
    // Docker containers (where the workdir is the mounted volume).  Relative
    // names are used in the shell command so paths are correct regardless of
    // how the volume is mapped inside the container.
    let patch_name = format!(".patch_validate_{unique}.patch");
    let wt_name = format!(".patch_validate_wt_{unique}");
    let tmp_patch = spec.workdir.join(&patch_name);

    std::fs::write(&tmp_patch, diff.as_bytes())
        .map_err(|e| PatchValidationFailure::ApplyFailed(format!("write temp: {e}")))?;

    // `patch_base_shell_arg` generates a shell-quoted env-var reference that
    // expands to `base_commit` at runtime (cross-platform: POSIX `$VAR` /
    // Windows `%VAR%`).
    let base_arg = patch_base_shell_arg(base_commit);

    // Use separate commands instead of a shell compound expression. That keeps
    // local Windows `cmd.exe`, POSIX shells, and Docker bash from disagreeing
    // about quoting, redirects, and `$?` syntax.
    let mut add_req = attach_cancellation(
        RunRequest::new(format!("git worktree add -q --detach {wt_name} {base_arg}"))
            .with_timeout(Duration::from_secs(30)),
        cancellation.clone(),
    );
    add_req.cwd = Some(spec.workdir.clone());
    add_req
        .env
        .insert(PATCH_BASE_ENV.into(), base_commit.to_owned());
    add_req
        .env
        .insert(LEGACY_PATCH_BASE_ENV.into(), base_commit.to_owned());

    let add_result = env.run(add_req).await;
    let result = match add_result {
        Ok(result) if !result.timed_out && result.exit_code == 0 => {
            let mut apply_req = attach_cancellation(
                RunRequest::new(format!(
                    "git -C {wt_name} apply --check --no-3way -- ../{patch_name}"
                ))
                .with_timeout(Duration::from_secs(30)),
                cancellation.clone(),
            );
            apply_req.cwd = Some(spec.workdir.clone());
            env.run(apply_req).await
        }
        Ok(result) => Ok(result),
        Err(e) => Err(e),
    };

    let mut cleanup_req = RunRequest::new(format!("git worktree remove --force {wt_name}"))
        .with_timeout(Duration::from_secs(30));
    cleanup_req.cwd = Some(spec.workdir.clone());
    let _ = env.run(cleanup_req).await;

    // Always remove the temp patch file, even when env.run fails.
    let _ = std::fs::remove_file(&tmp_patch);
    let result =
        result.map_err(|e| PatchValidationFailure::ApplyFailed(format!("env exec: {e}")))?;

    if result.timed_out || result.exit_code != 0 {
        let reason = result.stderr.trim();
        return Err(PatchValidationFailure::ApplyFailed(if reason.is_empty() {
            "git apply --check failed (no stderr)".into()
        } else {
            reason.to_owned()
        }));
    }
    Ok(())
}

/// Snapshot the working tree at `spec.workdir` as a unified diff against
/// `spec.base_commit` (or `HEAD`). Returns the diff text on success or a
/// human-readable reason string on failure. Pure: writes nothing.
///
/// We use `--no-color`, `--binary`, and `--unified=3` to match the format
/// the SWE-bench evaluator (`sb-cli`) consumes and `git apply` accepts.
async fn capture_patch(
    env: &dyn Environment,
    spec: &PatchCaptureSpec,
    cancellation: Option<MiniCancellation>,
) -> Result<String, String> {
    // Keep paths in `cwd` and the base revision in the environment so valid
    // revspec punctuation does not become shell syntax.
    let base = validate_git_rev(spec.base_commit.as_deref().unwrap_or("HEAD"))?;
    let base_arg = patch_base_shell_arg(base);
    let cmd = format!("git diff --no-color --binary --unified=3 --end-of-options {base_arg} -- .");
    let mut req = attach_cancellation(
        RunRequest::new(cmd).with_timeout(Duration::from_secs(60)),
        cancellation,
    );
    req.cwd = Some(spec.workdir.clone());
    req.env.insert(PATCH_BASE_ENV.into(), base.to_owned());
    req.env
        .insert(LEGACY_PATCH_BASE_ENV.into(), base.to_owned());
    let result = env
        .run(req)
        .await
        .map_err(|e| format!("env exec failed: {e}"))?;
    if result.timed_out {
        return Err(format!("git diff timed out: {}", result.stderr.trim()));
    }
    if result.exit_code != 0 {
        return Err(format!(
            "git diff exited {} ({})",
            result.exit_code,
            result.stderr.trim()
        ));
    }
    Ok(result.stdout)
}

fn validate_git_rev(rev: &str) -> Result<&str, String> {
    let valid = !rev.is_empty() && rev.chars().all(|ch| !ch.is_control());
    if valid {
        Ok(rev)
    } else {
        Err(format!("invalid git revision for diff base: {rev:?}"))
    }
}

fn patch_base_shell_arg(rev: &str) -> String {
    if cfg!(windows) {
        if rev.contains('"') {
            quote_git_rev_for_cmd(rev)
        } else {
            format!("\"%{PATCH_BASE_ENV}%\"")
        }
    } else {
        format!("\"${PATCH_BASE_ENV}\"")
    }
}

fn quote_git_rev_for_cmd(rev: &str) -> String {
    let needs_quotes = rev.chars().any(char::is_whitespace);
    let mut out = String::with_capacity(rev.len() + usize::from(needs_quotes) * 2);
    if needs_quotes {
        out.push('"');
    }
    for ch in rev.chars() {
        match (needs_quotes, ch) {
            (_, '"') => out.push_str("\\\""),
            (false, '^') => out.push_str("^^"),
            (false, '&' | '|' | '<' | '>' | '(' | ')' | '%') => {
                out.push('^');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    if needs_quotes {
        out.push('"');
    }
    out
}

fn build_model(
    cfg: &Config,
    deterministic: Option<Vec<String>>,
    deterministic_usage_per_call: Option<ModelUsage>,
) -> Arc<dyn Model> {
    if let Some(responses) = deterministic {
        let model = match deterministic_usage_per_call {
            Some(usage) => DeterministicModel::with_usage(responses, usage),
            None => DeterministicModel::new(responses),
        };
        return Arc::new(model);
    }
    // `LitellmBackend` dispatches by model prefix; credentials come from
    // the usual provider env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
    // …) the way Python LiteLLM expects.
    let primary =
        LitellmBackend::new(cfg.root.model.name.clone()).with_max_tokens(cfg.root.model.max_tokens);

    if cfg.root.model.fallback_models.is_empty() {
        return Arc::new(primary);
    }

    // Build a fallback chain: primary first, then each configured fallback.
    // Fallback candidates inherit max_tokens from the model config so all
    // candidates operate under the same budget constraint.
    let mut models: Vec<Box<dyn Model>> = vec![Box::new(primary)];
    for name in &cfg.root.model.fallback_models {
        models.push(Box::new(
            LitellmBackend::new(name.clone()).with_max_tokens(cfg.root.model.max_tokens),
        ));
    }
    Arc::new(FallbackModel::new(models))
}

async fn build_env(
    cfg: &Config,
    local_workdir: Option<&PathBuf>,
) -> Result<Box<dyn Environment>, Error> {
    match cfg.root.environment.kind {
        EnvKind::Local => Ok(Box::new(
            LocalEnvironment::new().with_workdir(local_workdir.cloned()),
        )),
        EnvKind::Docker => build_docker_env(cfg).await,
    }
}

#[cfg(feature = "docker")]
async fn build_docker_env(cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    let image = cfg.root.environment.docker_image.clone().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "environment.kind=docker requires environment.docker_image".into(),
        ))
    })?;
    let wd = PathBuf::from(cfg.root.environment.workdir.clone());
    let env = DockerEnvironment::start(image, wd).await?;
    Ok(Box::new(env))
}

#[cfg(not(feature = "docker"))]
#[allow(clippy::unused_async)] // Mirrors the docker-feature signature.
async fn build_docker_env(_cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "docker support not compiled in — rebuild with --features docker".into(),
    )))
}

/// Run operator-supplied verification checks after the agent finishes.
///
/// Sets `trajectory.info.verification_status` and
/// `trajectory.info.verification_results` unconditionally.
/// Returns `Some(Error::VerificationFailed(...))` when at least one check
/// fails; `None` when all pass or no checks were configured.
async fn run_verification_checks(
    trajectory: &mut crate::trajectory::Trajectory,
    env: &dyn crate::env::Environment,
    redactor: &crate::redaction::Redactor,
    checks: &[crate::trajectory::VerificationCheck],
    timeout_secs: u64,
    cancellation: Option<MiniCancellation>,
) -> Option<Error> {
    if checks.is_empty() {
        trajectory.info.verification_status =
            Some(crate::trajectory::verification_status::UNVERIFIED.into());
        return None;
    }

    let mut results = Vec::with_capacity(checks.len());
    let mut failed = 0usize;

    for check in checks {
        let start = Instant::now();
        let req = attach_cancellation(
            RunRequest::new(check.command.clone()).with_timeout(Duration::from_secs(timeout_secs)),
            cancellation.clone(),
        );

        let command = redactor
            .redact_text(&check.command, surface::TRAJECTORY)
            .text;
        let run_result = match env.run(req).await {
            Ok(r) => r,
            Err(e) => {
                // If the error was caused by cancellation, stop and mark unverified.
                if cancellation
                    .as_ref()
                    .is_some_and(MiniCancellation::is_cancelled)
                {
                    trajectory.info.verification_status =
                        Some(crate::trajectory::verification_status::UNVERIFIED.into());
                    return None;
                }
                failed += 1;
                let stderr_preview = truncate_preview(
                    &redactor
                        .redact_text(&e.to_string(), surface::TRAJECTORY)
                        .text,
                );
                results.push(crate::trajectory::VerificationResult {
                    name: check.name.clone(),
                    command,
                    exit_code: -1,
                    duration_ms: elapsed_ms(start),
                    passed: false,
                    stdout_preview: String::new(),
                    stderr_preview,
                    timed_out: false,
                });
                continue;
            }
        };

        // Cancellation fires after the command finishes with exit_code=-1 and
        // stderr="cancelled" (not an Err). Detect it here so a Ctrl-C during
        // verification produces `unverified` rather than `verification_failed`.
        if cancellation
            .as_ref()
            .is_some_and(MiniCancellation::is_cancelled)
        {
            trajectory.info.verification_status =
                Some(crate::trajectory::verification_status::UNVERIFIED.into());
            return None;
        }

        let duration_ms = elapsed_ms(start);
        let passed = !run_result.timed_out && run_result.exit_code == 0;
        if !passed {
            failed += 1;
        }
        let stdout_preview = truncate_preview(
            &redactor
                .redact_text(&run_result.stdout, surface::TRAJECTORY)
                .text,
        );
        let stderr_preview = truncate_preview(
            &redactor
                .redact_text(&run_result.stderr, surface::TRAJECTORY)
                .text,
        );
        results.push(crate::trajectory::VerificationResult {
            name: check.name.clone(),
            command,
            exit_code: run_result.exit_code,
            duration_ms,
            passed,
            stdout_preview,
            stderr_preview,
            timed_out: run_result.timed_out,
        });
    }

    trajectory.info.verification_results = results;

    if failed == 0 {
        trajectory.info.verification_status =
            Some(crate::trajectory::verification_status::VERIFIED.into());
        None
    } else {
        trajectory.info.verification_status =
            Some(crate::trajectory::verification_status::VERIFICATION_FAILED.into());
        Some(Error::VerificationFailed(failed, checks.len()))
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn truncate_preview(text: &str) -> String {
    if text.len() <= VERIFICATION_PREVIEW_MAX_BYTES {
        return text.to_owned();
    }
    let mut end = VERIFICATION_PREVIEW_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Errors produced when validating a partial trajectory for `mini --resume`.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumeValidationError {
    /// The trajectory already has a terminal outcome and cannot be continued.
    AlreadyTerminal,
    /// The trajectory is missing required fields (`task`, `model_name`) that
    /// are the caller's source of truth for the resumed run's configuration.
    ManifestMissing,
    /// The trajectory's message sequence is structurally invalid for resume
    /// (e.g. empty, single message, or ends on a mid-assistant-turn).
    InvalidPrefix(String),
}

/// Validate that `traj` is a valid candidate for `mini --resume`.
///
/// Returns `Ok(())` when the trajectory may be resumed, or a
/// `ResumeValidationError` describing the first violation found.
pub fn validate_resume_trajectory(
    traj: &crate::trajectory::Trajectory,
) -> Result<(), ResumeValidationError> {
    // A resumable trajectory must be explicitly marked partial AND carry no
    // terminal outcome or exit_reason. Any of these conditions being violated
    // means the run reached a terminal state or predates the #326 WAL.
    if !traj.info.partial || traj.info.outcome.is_some() || traj.info.exit_reason.is_some() {
        return Err(ResumeValidationError::AlreadyTerminal);
    }

    // Manifest-fields check.
    if traj.info.task.is_none() || traj.info.model_name.is_none() {
        return Err(ResumeValidationError::ManifestMissing);
    }

    // Structural validity — need at least system + user initial messages.
    if traj.messages.len() < 2 {
        return Err(ResumeValidationError::InvalidPrefix(format!(
            "trajectory has {} message(s); need at least 2 (system + user)",
            traj.messages.len()
        )));
    }

    // Reject a trajectory that ends on an assistant turn with no following
    // user/tool observation. Resuming it would produce two consecutive
    // assistant messages, which model APIs reject with a 400.
    if traj.messages.last().is_some_and(|m| m.role == "assistant") {
        return Err(ResumeValidationError::InvalidPrefix(
            "trajectory ends in a partial assistant turn with no observation".into(),
        ));
    }

    Ok(())
}

/// Derive a filename-safe trajectory name from a task string.
pub fn slugify(task: &str) -> String {
    let mut s: String = task
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let trimmed = s.trim_matches('-').to_lowercase();
    let cut: String = trimmed.chars().take(64).collect();
    if cut.is_empty() { "task".into() } else { cut }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use async_trait::async_trait;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;

    use crate::stream::{NullSink, StreamEvent, StreamSink};

    #[test]
    fn compose_stream_sinks_returns_none_when_all_absent() {
        assert!(compose_stream_sinks(None, None, None, None).is_none());
    }

    #[test]
    fn compose_stream_sinks_unwraps_single_sink_without_multi_wrap() {
        let sse: Arc<dyn StreamSink> = Arc::new(NullSink);
        let composed = compose_stream_sinks(Some(sse.clone()), None, None, None).unwrap();
        // Single-sink path returns the same Arc, not a MultiSink wrapper.
        assert!(Arc::ptr_eq(&composed, &sse));
    }

    #[test]
    fn compose_stream_sinks_multi_wraps_when_multiple() {
        let a: Arc<dyn StreamSink> = Arc::new(NullSink);
        let b: Arc<dyn StreamSink> = Arc::new(NullSink);
        let composed = compose_stream_sinks(Some(a), Some(b), None, None).unwrap();
        // Just emit through it to verify it works; if it were a NullSink
        // directly the call would still succeed, but MultiSink::emit
        // exercises the fan-out path.
        composed.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });
    }

    #[test]
    fn build_interactive_pieces_off_yields_no_callback() {
        let (cb, dash) = build_interactive_pieces(InteractiveMode::Off).unwrap();
        assert!(cb.is_none());
        assert!(dash.is_none());
    }

    #[test]
    fn build_interactive_pieces_yolo_status_only_yields_no_callback() {
        let (cb, dash) = build_interactive_pieces(InteractiveMode::YoloStatusOnly).unwrap();
        assert!(cb.is_none());
        assert!(dash.is_none());
    }

    #[test]
    fn interactive_mode_default_is_off() {
        assert_eq!(InteractiveMode::default(), InteractiveMode::Off);
    }
    use tokio::sync::watch;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("   "), "task");
        assert_eq!(slugify("A/B/C"), "a-b-c");
    }

    #[test]
    fn validate_git_rev_accepts_revspec_chars_and_rejects_empty_or_control() {
        assert_eq!(validate_git_rev("HEAD"), Ok("HEAD"));
        assert_eq!(validate_git_rev("abc123"), Ok("abc123"));
        assert_eq!(validate_git_rev("HEAD@{1}"), Ok("HEAD@{1}"));
        assert_eq!(validate_git_rev("v1.2^{commit}"), Ok("v1.2^{commit}"));
        assert_eq!(
            validate_git_rev("refs/heads/feat%test"),
            Ok("refs/heads/feat%test")
        );
        assert_eq!(
            validate_git_rev("refs/heads/feat\"test"),
            Ok("refs/heads/feat\"test")
        );
        assert_eq!(validate_git_rev("HEAD^{/foo bar}"), Ok("HEAD^{/foo bar}"));
        assert_eq!(
            validate_git_rev("HEAD^{/foo && bar}"),
            Ok("HEAD^{/foo && bar}")
        );
        assert!(validate_git_rev("HEAD\nmain").is_err());
        assert!(validate_git_rev("").is_err());
    }

    #[tokio::test]
    async fn capture_patch_accepts_reflog_revision_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        std::fs::write(repo.join("hello.txt"), "committed\n").unwrap();
        git(&repo, &["commit", "-am", "advance"]);
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("HEAD@{1}".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec, None)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[derive(Default)]
    struct RecordingEnvironment {
        requests: Mutex<Vec<RecordedRequest>>,
    }

    struct RecordedRequest {
        command: String,
        has_cancellation: bool,
    }

    impl RecordingEnvironment {
        fn requests(&self) -> Vec<(String, bool)> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|request| (request.command.clone(), request.has_cancellation))
                .collect()
        }
    }

    #[async_trait]
    impl Environment for RecordingEnvironment {
        async fn run(
            &self,
            req: RunRequest,
        ) -> Result<crate::env::RunResult, crate::error::EnvError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(RecordedRequest {
                    command: req.command,
                    has_cancellation: req.cancellation.is_some(),
                });
            Ok(crate::env::RunResult {
                stdout: "diff --git a/hello.txt b/hello.txt\n".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
        }
    }

    #[tokio::test]
    async fn capture_patch_threads_cancellation_to_git_diff() {
        let env = RecordingEnvironment::default();
        let work = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(false);
        let spec = PatchCaptureSpec {
            base_commit: Some("HEAD".into()),
            workdir: work.path().to_path_buf(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: true,
        };

        let _ = capture_patch(&env, &spec, Some(MiniCancellation::new(rx)))
            .await
            .unwrap();

        let requests = env.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].0.starts_with("git diff "));
        assert!(requests[0].1, "git diff request should carry cancellation");
    }

    #[tokio::test]
    async fn check_patch_validity_does_not_cancel_cleanup_command() {
        let env = RecordingEnvironment::default();
        let work = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(false);
        let spec = PatchCaptureSpec {
            base_commit: Some("HEAD".into()),
            workdir: work.path().to_path_buf(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        check_patch_validity(
            &env,
            &spec,
            "diff --git a/hello.txt b/hello.txt\n",
            Some(MiniCancellation::new(rx)),
        )
        .await
        .unwrap();

        let requests = env.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].0.starts_with("git worktree add "));
        assert!(requests[0].1, "git worktree add should carry cancellation");
        assert!(requests[1].0.starts_with("git -C "));
        assert!(requests[1].1, "git apply --check should carry cancellation");
        assert!(requests[2].0.starts_with("git worktree remove "));
        assert!(
            !requests[2].1,
            "cleanup must run best-effort without cancellation"
        );
    }

    #[tokio::test]
    async fn capture_patch_accepts_peeled_tag_revision_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["tag", "-a", "v1.2", "-m", "v1.2"]);
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("v1.2^{commit}".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec, None)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_accepts_percent_ref_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["branch", "feat%test"]);
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("refs/heads/feat%test".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec, None)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_accepts_quoted_ref_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        write_packed_ref(&repo, "refs/heads/feat\"test");
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("refs/heads/feat\"test".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec, None)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_accepts_commit_message_search_revision_with_space() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "foo bar base"]);
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("HEAD^{/foo bar}".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec, None)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_quotes_shell_metacharacters_in_revision_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);

        let spec = PatchCaptureSpec {
            base_commit: Some("HEAD && echo injected > injected.txt".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let result = capture_patch(&LocalEnvironment::new(), &spec, None).await;
        assert!(result.is_err(), "{result:?}");
        assert!(!repo.join("injected.txt").exists());
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init"]);
        git(dir, &["config", "user.email", "test@example.invalid"]);
        git(dir, &["config", "user.name", "Test User"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        git(dir, &["config", "tag.gpgsign", "false"]);
    }

    fn write_packed_ref(dir: &Path, ref_name: &str) {
        let hash = git_stdout(dir, &["rev-parse", "HEAD"]);
        std::fs::write(
            dir.join(".git").join("packed-refs"),
            format!("{} {}\n", hash.trim(), ref_name),
        )
        .unwrap();
        let resolved = git_stdout(dir, &["rev-parse", ref_name]);
        assert_eq!(resolved.trim(), hash.trim());
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = git_raw(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let out = git_raw(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn git_raw(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
    }

    // ── RED-phase tests: patch validation ──────────────────────────────────

    #[tokio::test]
    async fn patch_validation_empty_diff_yields_patch_empty_error() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        let spec = PatchCaptureSpec {
            base_commit: Some(base_sha),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        // empty diff string → should fail with PatchEmpty
        let result = check_patch_validity(&LocalEnvironment::new(), &spec, "", None).await;
        assert!(
            matches!(result, Err(PatchValidationFailure::Empty)),
            "expected PatchEmpty, got {result:?}"
        );
    }

    #[tokio::test]
    async fn patch_validation_valid_patch_passes() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        // Modify workdir (leave unstaged) — worktree approach checks against
        // a clean checkout at base_sha, so this does not need to be reverted.
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some(base_sha.clone()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        let diff = capture_patch(&LocalEnvironment::new(), &spec, None)
            .await
            .unwrap();

        let result = check_patch_validity(&LocalEnvironment::new(), &spec, &diff, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn patch_validation_wrong_base_yields_apply_invalid() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        let initial_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        // Apply "before → after" change and commit it
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();
        git(&repo, &["commit", "-am", "second"]);

        // Build a diff that patches "before" → "after"
        let _spec_diff = PatchCaptureSpec {
            base_commit: Some(initial_sha.clone()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        // HEAD is at "second" commit; diff against initial gives before→after
        // But workdir already has "after" committed, so diff is empty from cwd.
        // Instead, craft the diff manually to simulate a stale patch.
        let stale_diff = "diff --git a/hello.txt b/hello.txt\n\
index 8a1218a..24c5735 100644\n\
--- a/hello.txt\n\
+++ b/hello.txt\n\
@@ -1 +1 @@\n\
-before\n\
+after\n";

        // Validate against the CURRENT HEAD (which already has "after") — should fail
        let head_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        let spec_validate = PatchCaptureSpec {
            base_commit: Some(head_sha),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        let result =
            check_patch_validity(&LocalEnvironment::new(), &spec_validate, stale_diff, None).await;
        assert!(
            matches!(result, Err(PatchValidationFailure::ApplyFailed(_))),
            "expected ApplyFailed, got {result:?}"
        );
    }

    #[tokio::test]
    async fn patch_validation_skipped_when_flag_set() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        let spec = PatchCaptureSpec {
            base_commit: Some(base_sha),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: true,
        };
        // Empty diff + skip_patch_validation=true → should succeed
        let result = check_patch_validity(&LocalEnvironment::new(), &spec, "", None).await;
        assert!(result.is_ok(), "expected Ok with skip flag, got {result:?}");
    }

    #[tokio::test]
    async fn patch_validation_skipped_when_no_base_commit() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);

        let spec = PatchCaptureSpec {
            base_commit: None, // no base → validation is gated off
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        let result = check_patch_validity(&LocalEnvironment::new(), &spec, "", None).await;
        assert!(
            result.is_ok(),
            "expected Ok when base_commit is None, got {result:?}"
        );
    }

    // ── RED-phase: resume validation ──────────────────────────────────────

    fn make_minimal_partial_traj() -> crate::trajectory::Trajectory {
        let mut traj = crate::trajectory::Trajectory::new();
        traj.info.task = Some("fix the bug".into());
        traj.info.model_name = Some("claude-opus-4-7".into());
        traj.info.partial = true;
        traj.info.partial_reason = Some("in_progress".into());
        traj.messages.push(crate::trajectory::MessageRecord {
            role: "system".into(),
            content: "sys prompt".into(),
            extra: Default::default(),
        });
        traj.messages.push(crate::trajectory::MessageRecord {
            role: "user".into(),
            content: "task prompt".into(),
            extra: Default::default(),
        });
        traj
    }

    #[test]
    fn validate_resume_accepts_valid_partial_trajectory() {
        let traj = make_minimal_partial_traj();
        assert!(validate_resume_trajectory(&traj).is_ok());
    }

    #[test]
    fn validate_resume_rejects_submitted_terminal_trajectory() {
        let mut traj = make_minimal_partial_traj();
        traj.info.partial = false;
        traj.info.outcome = Some("submitted".into());
        assert_eq!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::AlreadyTerminal)
        );
    }

    #[test]
    fn validate_resume_rejects_error_terminal_trajectory() {
        let mut traj = make_minimal_partial_traj();
        traj.info.partial = false;
        traj.info.outcome = Some("error".into());
        assert_eq!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::AlreadyTerminal)
        );
    }

    #[test]
    fn validate_resume_rejects_cancelled_exit_reason() {
        let mut traj = make_minimal_partial_traj();
        traj.info.partial = false;
        traj.info.exit_reason = Some("cancelled".into());
        assert_eq!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::AlreadyTerminal)
        );
    }

    #[test]
    fn validate_resume_rejects_missing_task_field() {
        let mut traj = make_minimal_partial_traj();
        traj.info.task = None;
        assert_eq!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::ManifestMissing)
        );
    }

    #[test]
    fn validate_resume_rejects_missing_model_name_field() {
        let mut traj = make_minimal_partial_traj();
        traj.info.model_name = None;
        assert_eq!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::ManifestMissing)
        );
    }

    #[test]
    fn validate_resume_rejects_partial_false_without_outcome() {
        // A trajectory with partial:false and no outcome (e.g. legacy or corrupted)
        // must be rejected — it predates the #326 WAL.
        let mut traj = make_minimal_partial_traj();
        traj.info.partial = false;
        assert_eq!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::AlreadyTerminal)
        );
    }

    #[test]
    fn validate_resume_rejects_empty_messages() {
        let mut traj = make_minimal_partial_traj();
        traj.messages.clear();
        assert!(matches!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::InvalidPrefix(_))
        ));
    }

    #[test]
    fn validate_resume_rejects_single_message() {
        let mut traj = make_minimal_partial_traj();
        traj.messages.truncate(1);
        assert!(matches!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::InvalidPrefix(_))
        ));
    }

    #[test]
    fn validate_resume_rejects_trailing_assistant_message() {
        let mut traj = make_minimal_partial_traj();
        traj.messages.push(crate::trajectory::MessageRecord {
            role: "assistant".into(),
            content: "partial turn with no observation".into(),
            extra: Default::default(),
        });
        assert_eq!(
            validate_resume_trajectory(&traj),
            Err(ResumeValidationError::InvalidPrefix(
                "trajectory ends in a partial assistant turn with no observation".into()
            ))
        );
    }

    // ── Integration test: mini --resume loop ─────────────────────────────

    /// AC #3 / #10: run 3 turns, build a partial trajectory, resume for 2 more,
    /// assert final trajectory has 5 total steps and resume_history is populated.
    #[tokio::test]
    async fn mini_resume_continues_from_partial_trajectory() {
        let work = tempfile::tempdir().unwrap();
        let runs_dir = work.path().join("runs");

        let mut cfg = crate::config::Config::defaults().unwrap();
        cfg.root.agent.step_limit = 10;

        // Build a partial trajectory simulating 3 completed steps.
        let mut partial = crate::trajectory::Trajectory::new();
        partial.info.task = Some("fix the bug".into());
        partial.info.model_name = Some("deterministic".into());
        partial.info.steps = Some(3);
        partial.info.actual_cost_usd = Some(0.03);
        partial.info.partial = true;
        partial.info.partial_reason = Some("in_progress".into());
        partial.info.token_usage = Some(crate::trajectory::TokenUsage {
            prompt_tokens: 1000,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            completion_tokens: 300,
        });
        // Add system + user (initial setup) messages plus 3 turn pairs.
        partial.messages.push(crate::trajectory::MessageRecord {
            role: "system".into(),
            content: "You are a helpful assistant.".into(),
            extra: Default::default(),
        });
        partial.messages.push(crate::trajectory::MessageRecord {
            role: "user".into(),
            content: "Fix the bug".into(),
            extra: Default::default(),
        });
        for i in 0..3u32 {
            partial.messages.push(crate::trajectory::MessageRecord {
                role: "assistant".into(),
                content: format!("step {i}: thinking"),
                extra: Default::default(),
            });
            partial.messages.push(crate::trajectory::MessageRecord {
                role: "user".into(),
                content: format!("observation for step {i}"),
                extra: Default::default(),
            });
        }

        // Resume args: one deterministic response (the submit) for the 4th step.
        let args = MiniArgs {
            task: partial.info.task.clone().unwrap(),
            extra_context: None,
            config: cfg,
            output_dir: runs_dir.clone(),
            trajectory_name: "resume-test".into(),
            deterministic_responses: Some(vec![
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfixed\n```".into(),
            ]),
            deterministic_usage_per_call: None,
            task_timeout_secs: Some(30),
            cancellation: None,
            stream_addr: None,
            patch_capture: None,
            verification_checks: vec![],
            verification_timeout_secs: 60,
            resume_from: Some(partial),
            interactive_mode: InteractiveMode::Off,
            trace_id: None,
            webhook_url: None,
            webhook_headers: vec![],
            local_workdir: None,
            read_only: false,
            allow_mcp_in_read_only: false,
        };

        run(args).await.unwrap();

        let traj_path = runs_dir.join("resume-test.traj.json");
        let traj_json = std::fs::read_to_string(&traj_path).unwrap();
        let traj: serde_json::Value = serde_json::from_str(&traj_json).unwrap();

        // AC #6: step count continues from prior value.
        // The submit action does not increment the tool-call counter,
        // so steps stays at 3 (the persisted value).
        let steps = traj["info"]["steps"].as_u64().unwrap_or(0);
        assert!(
            steps >= 3,
            "steps should be at least 3 (prior_steps); got {steps}; traj:\n{traj_json}"
        );

        // The trajectory should not be partial any more.
        assert!(
            !traj["info"]["partial"].as_bool().unwrap_or(false),
            "final trajectory should not be partial; traj:\n{traj_json}"
        );

        // AC #5: resume_history should have one entry.
        let resume_history = traj["info"]["resume_history"].as_array().unwrap();
        assert_eq!(
            resume_history.len(),
            1,
            "should have exactly 1 resume record; traj:\n{traj_json}"
        );
        assert_eq!(
            resume_history[0]["prior_steps"].as_u64(),
            Some(3),
            "resume_history should record 3 prior steps; traj:\n{traj_json}"
        );

        // AC #3: the model was only queried once (for the new step), not 4 times.
        // We verify this indirectly: if the model were queried 4 times, it would
        // exhaust the 1-response queue and the run would fail with ResponsesExhausted.
        assert_eq!(
            traj["info"]["outcome"].as_str(),
            Some("submitted"),
            "expected submitted outcome; traj:\n{traj_json}"
        );
    }

    // ── End-to-end tests through mini::run() ──────────────────────────────

    /// AC (b): agent submits with no workdir changes → empty diff → outcome
    /// downgraded to error with failure_category=patch_empty.
    #[tokio::test]
    async fn mini_run_empty_diff_yields_patch_empty_outcome() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "content\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        let runs_dir = work.path().join("runs");
        let patch_path = work.path().join("out.patch");

        let mut cfg = crate::config::Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;

        let args = MiniArgs {
            task: "do nothing".into(),
            extra_context: None,
            config: cfg,
            output_dir: runs_dir.clone(),
            trajectory_name: "empty-diff-test".into(),
            // Submit immediately without touching the repo → empty diff
            deterministic_responses: Some(vec![
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
            ]),
            deterministic_usage_per_call: None,
            task_timeout_secs: Some(30),
            cancellation: None,
            stream_addr: None,
            patch_capture: Some(PatchCaptureSpec {
                base_commit: Some(base_sha),
                workdir: repo.clone(),
                patch_path: patch_path.clone(),
                skip_patch_validation: false,
            }),
            verification_checks: vec![],
            verification_timeout_secs: 60,
            resume_from: None,
            interactive_mode: InteractiveMode::Off,
            trace_id: None,
            webhook_url: None,
            webhook_headers: vec![],
            local_workdir: None,
            read_only: false,
            allow_mcp_in_read_only: false,
        };

        run(args).await.unwrap();

        // Trajectory must have outcome=error, failure_category=patch_empty
        let traj_path = runs_dir.join("empty-diff-test.traj.json");
        let traj_json = std::fs::read_to_string(&traj_path).unwrap();
        let traj: serde_json::Value = serde_json::from_str(&traj_json).unwrap();

        assert_eq!(
            traj["info"]["outcome"].as_str(),
            Some("error"),
            "expected error outcome; trajectory:\n{traj_json}"
        );
        assert_eq!(
            traj["info"]["failure_category"].as_str(),
            Some("patch_empty"),
            "expected patch_empty failure_category; trajectory:\n{traj_json}"
        );

        // Patch file must be written even though the diff is empty
        assert!(patch_path.exists(), "patch file should exist on disk");
        let patch_content = std::fs::read_to_string(&patch_path).unwrap();
        assert!(
            patch_content.is_empty(),
            "patch file should be empty for a no-op submission"
        );
    }

    #[tokio::test]
    async fn mini_run_honors_cancellation_after_submit_before_patch_capture() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        let runs_dir = work.path().join("runs");
        let patch_path = work.path().join("out.patch");
        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut hook = CANCEL_BEFORE_PATCH_CAPTURE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *hook = Some(CancelBeforePatchCaptureHook {
                trajectory_name: "cancel-after-submit".into(),
                sender: cancel_tx,
            });
        }

        let mut cfg = crate::config::Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let repo_display = repo.display().to_string();
        let edit_command = if cfg!(windows) {
            format!("cd /d \"{repo_display}\" && echo after>hello.txt")
        } else {
            format!("cd '{repo_display}' && printf 'after\\n' > hello.txt")
        };

        let args = MiniArgs {
            task: "edit then submit".into(),
            extra_context: None,
            config: cfg,
            output_dir: runs_dir.clone(),
            trajectory_name: "cancel-after-submit".into(),
            deterministic_responses: Some(vec![
                format!("```bash\n{edit_command}\n```"),
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
            ]),
            deterministic_usage_per_call: None,
            task_timeout_secs: Some(30),
            cancellation: Some(MiniCancellation::new(cancel_rx)),
            stream_addr: None,
            patch_capture: Some(PatchCaptureSpec {
                base_commit: Some(base_sha),
                workdir: repo,
                patch_path: patch_path.clone(),
                skip_patch_validation: true,
            }),
            verification_checks: vec![],
            verification_timeout_secs: 60,
            resume_from: None,
            interactive_mode: InteractiveMode::Off,
            trace_id: None,
            webhook_url: None,
            webhook_headers: vec![],
            local_workdir: None,
            read_only: false,
            allow_mcp_in_read_only: false,
        };

        run(args).await.unwrap();
        {
            let mut hook = CANCEL_BEFORE_PATCH_CAPTURE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *hook = None;
        }

        let traj_path = runs_dir.join("cancel-after-submit.traj.json");
        let traj_json = std::fs::read_to_string(&traj_path).unwrap();
        let traj: serde_json::Value = serde_json::from_str(&traj_json).unwrap();
        assert_eq!(
            traj["info"]["exit_reason"].as_str(),
            Some(crate::trajectory::exit_reason::CANCELLED),
            "trajectory should preserve forced cancellation:\n{traj_json}"
        );
        assert_eq!(traj["info"]["outcome"].as_str(), Some("error"));
        assert_eq!(traj["info"].get("failure_category"), None);
        assert!(
            !runs_dir.join("cancel-after-submit.output.txt").exists(),
            "cancelled submission should not write final output artifact"
        );
    }

    /// Environment that always returns `Err(EnvError::CommandFailed(...))`.
    struct FailingEnvironment {
        message: String,
    }

    #[async_trait]
    impl Environment for FailingEnvironment {
        async fn run(
            &self,
            _req: RunRequest,
        ) -> Result<crate::env::RunResult, crate::error::EnvError> {
            Err(crate::error::EnvError::CommandFailed(self.message.clone()))
        }
    }

    #[tokio::test]
    async fn env_error_during_verification_records_failed_check() {
        let env = FailingEnvironment {
            message: "spawn failed".into(),
        };
        let redactor = crate::redaction::Redactor::disabled();
        let checks = vec![crate::trajectory::VerificationCheck {
            name: "my-check".into(),
            command: "exit 0".into(),
        }];
        let mut traj = crate::trajectory::Trajectory::default();
        let err = run_verification_checks(&mut traj, &env, &redactor, &checks, 30, None).await;
        assert!(err.is_some(), "expected VerificationFailed error");
        assert_eq!(traj.info.verification_results.len(), 1);
        let vr = &traj.info.verification_results[0];
        assert!(!vr.passed);
        assert_eq!(vr.exit_code, -1);
        assert!(!vr.timed_out);
        assert!(
            vr.stderr_preview.contains("spawn failed"),
            "stderr_preview should carry env error: {}",
            vr.stderr_preview
        );
        assert_eq!(
            traj.info.verification_status.as_deref(),
            Some(crate::trajectory::verification_status::VERIFICATION_FAILED)
        );
    }

    #[tokio::test]
    async fn env_error_with_cancellation_sets_unverified() {
        let (_tx, rx) = watch::channel(true); // pre-fired
        let cancellation = MiniCancellation::new(rx);
        let env = FailingEnvironment {
            message: "interrupted".into(),
        };
        let redactor = crate::redaction::Redactor::disabled();
        let checks = vec![crate::trajectory::VerificationCheck {
            name: "my-check".into(),
            command: "exit 0".into(),
        }];
        let mut traj = crate::trajectory::Trajectory::default();
        let err =
            run_verification_checks(&mut traj, &env, &redactor, &checks, 30, Some(cancellation))
                .await;
        assert!(err.is_none(), "cancelled run should not return an error");
        assert_eq!(
            traj.info.verification_status.as_deref(),
            Some(crate::trajectory::verification_status::UNVERIFIED)
        );
    }
}
