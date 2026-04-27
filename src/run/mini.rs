//! One-shot task runner — the port of `run/mini.py`. Resolves backend from
//! the model name, builds `DefaultAgent`, runs to completion, writes a
//! trajectory file and (if submitted) an output artifact.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::agent::{Agent, DefaultAgent, default::DefaultAgentBuilder};
use crate::config::{Config, EnvKind};
#[cfg(feature = "docker")]
use crate::env::DockerEnvironment;
use crate::env::{Environment, LocalEnvironment, RunRequest};
use crate::error::Error;
use crate::model::litellm::LitellmBackend;
use crate::model::{DeterministicModel, Model, ModelUsage};
use crate::stream::{BroadcastSink, SseServer, StreamSink};
use crate::trajectory::FailureCategory;

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
    /// Optional SSE stream endpoint to bind. When `Some`, the runner
    /// starts a server before the agent runs and shuts it down after.
    pub stream_addr: Option<SocketAddr>,
    /// When `Some` and the agent submits, the runner captures a `git
    /// diff` of `workdir` against `base_commit` and writes it to
    /// `patch_path`. Capture failures downgrade the run's recorded
    /// outcome to `error` rather than crashing the runner.
    pub patch_capture: Option<PatchCaptureSpec>,
}

pub async fn run(args: MiniArgs) -> Result<(), Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    let model = build_model(
        &args.config,
        args.deterministic_responses,
        args.deterministic_usage_per_call.clone(),
    );
    let env = build_env(&args.config).await?;

    // Bring up the SSE server first so any client that connects right
    // after CLI startup catches the `run_started` event the builder
    // emits below.
    let (sink, server): (Option<Arc<dyn StreamSink>>, Option<SseServer>) = match args.stream_addr {
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

    let mut agent: DefaultAgent = DefaultAgentBuilder {
        config: args.config.clone(),
        model,
        env,
        task: args.task.clone(),
        extra_context: args.extra_context.clone(),
        renderer: None,
        stream: sink,
    }
    .build()?;

    let traj_path = args
        .output_dir
        .join(format!("{}.traj.json", args.trajectory_name));

    // Run the agent. On error, finalize the trajectory with
    // `outcome="error"` so the partial run is still a self-contained
    // record of what happened — then propagate.
    let run_result = agent.run().await;
    if let Err(e) = &run_result {
        agent
            .trajectory
            .info
            .exit_reason
            .get_or_insert_with(|| "error".into());
        agent.trajectory.info.failure_category = Some(classify_error(e));
        agent
            .trajectory
            .info
            .other
            .entry("error_message".into())
            .or_insert_with(|| serde_json::Value::String(e.to_string()));
        agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
    }

    // Patch capture happens before the trajectory is saved so any
    // capture failure can be reflected as `outcome: "error"` rather
    // than leaving a stale `submitted` record on disk.
    let mut patch_written = false;
    if let (Ok(crate::agent::ExitReason::Submitted { .. }), Some(spec)) =
        (run_result.as_ref(), args.patch_capture.as_ref())
    {
        match capture_patch(agent.env.as_ref(), spec).await {
            Ok(diff) => {
                if diff.is_empty() {
                    tracing::warn!(
                        instance = %args.trajectory_name,
                        "agent submitted but produced an empty diff"
                    );
                }
                std::fs::write(&spec.patch_path, &diff)?;
                patch_written = true;
            }
            Err(reason) => {
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

    agent.trajectory.save_pretty(&traj_path)?;

    let exit = run_result?;

    if let crate::agent::ExitReason::Submitted { final_output } = &exit {
        let out_path = args
            .output_dir
            .join(format!("{}.output.txt", args.trajectory_name));
        std::fs::write(&out_path, final_output)?;
    }

    tracing::info!(?traj_path, patch_written, "trajectory written");

    if let Some(server) = server {
        server.shutdown().await;
    }
    Ok(())
}

fn classify_error(err: &Error) -> FailureCategory {
    match err {
        Error::Env(_) => FailureCategory::EnvSetup,
        Error::Model(crate::error::ModelError::Malformed(_)) => FailureCategory::ModelParse,
        Error::Model(_) => FailureCategory::ModelApi,
        _ => FailureCategory::AgentInternal,
    }
}

/// Snapshot the working tree at `spec.workdir` as a unified diff against
/// `spec.base_commit` (or `HEAD`). Returns the diff text on success or a
/// human-readable reason string on failure. Pure: writes nothing.
///
/// We use `--no-color`, `--binary`, and `--unified=3` to match the format
/// the SWE-bench evaluator (`sb-cli`) consumes and `git apply` accepts.
async fn capture_patch(env: &dyn Environment, spec: &PatchCaptureSpec) -> Result<String, String> {
    // `git -C <workdir>` keeps us independent of the env's idea of cwd
    // (the local env inherits the process cwd, the docker env uses `-w`).
    // The `--` ensures the trailing argument is a pathspec rather than a
    // ref, which matters when the workdir is empty or unborn.
    let base = spec.base_commit.as_deref().unwrap_or("HEAD");
    // Quote `base` to defuse hostile dataset values; SWE-bench commits are
    // hex SHAs but a malformed instance shouldn't be able to inject shell.
    let cmd = format!(
        "git -C {workdir} diff --no-color --binary --unified=3 {base} -- .",
        workdir = shell_quote(&spec.workdir.to_string_lossy()),
        base = shell_quote(base),
    );
    let req = RunRequest::new(cmd).with_timeout(Duration::from_secs(60));
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

/// Single-quote-escape a string for safe inclusion in a `bash -c` argv.
/// Closes the quote, emits an escaped literal `'`, reopens — the standard
/// POSIX shell idiom.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
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
    let backend =
        LitellmBackend::new(cfg.root.model.name.clone()).with_max_tokens(cfg.root.model.max_tokens);
    Arc::new(backend)
}

async fn build_env(cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    match cfg.root.environment.kind {
        EnvKind::Local => Ok(Box::new(LocalEnvironment::new())),
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
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("   "), "task");
        assert_eq!(slugify("A/B/C"), "a-b-c");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a"), "'a'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        // Already-quoted-looking strings round-trip safely.
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }
}
