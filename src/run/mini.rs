//! One-shot task runner — the port of `run/mini.py`. Resolves backend from
//! the model name, builds `DefaultAgent`, runs to completion, writes a
//! trajectory file and (if submitted) an output artifact.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{Agent, DefaultAgent, default::DefaultAgentBuilder};
use crate::config::{Config, EnvKind};
#[cfg(feature = "docker")]
use crate::env::DockerEnvironment;
use crate::env::{Environment, LocalEnvironment};
use crate::error::Error;
use crate::model::litellm::LitellmBackend;
use crate::model::{DeterministicModel, Model};
use crate::stream::{BroadcastSink, SseServer, StreamSink};

pub struct MiniArgs {
    pub task: String,
    pub extra_context: Option<String>,
    pub config: Config,
    pub output_dir: PathBuf,
    pub trajectory_name: String,
    pub deterministic_responses: Option<Vec<String>>,
    /// Optional SSE stream endpoint to bind. When `Some`, the runner
    /// starts a server before the agent runs and shuts it down after.
    pub stream_addr: Option<SocketAddr>,
}

pub async fn run(args: MiniArgs) -> Result<(), Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    let model = build_model(&args.config, args.deterministic_responses);
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
        agent
            .trajectory
            .info
            .other
            .entry("error_message".into())
            .or_insert_with(|| serde_json::Value::String(e.to_string()));
        agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
    }

    agent.trajectory.save_pretty(&traj_path)?;

    let exit = run_result?;

    if let crate::agent::ExitReason::Submitted { final_output } = &exit {
        let out_path = args
            .output_dir
            .join(format!("{}.output.txt", args.trajectory_name));
        std::fs::write(&out_path, final_output)?;
    }

    tracing::info!(?traj_path, "trajectory written");

    if let Some(server) = server {
        server.shutdown().await;
    }
    Ok(())
}

fn build_model(cfg: &Config, deterministic: Option<Vec<String>>) -> Arc<dyn Model> {
    if let Some(responses) = deterministic {
        return Arc::new(DeterministicModel::new(responses));
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
}
