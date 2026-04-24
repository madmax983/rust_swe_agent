//! One-shot task runner — the port of `run/mini.py`. Resolves backend from
//! the model name, builds `DefaultAgent`, runs to completion, writes a
//! trajectory file and (if submitted) an output artifact.

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

pub struct MiniArgs {
    pub task: String,
    pub extra_context: Option<String>,
    pub config: Config,
    pub output_dir: PathBuf,
    pub trajectory_name: String,
    pub deterministic_responses: Option<Vec<String>>,
}

pub async fn run(args: MiniArgs) -> Result<(), Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    let model = build_model(&args.config, args.deterministic_responses);
    let env = build_env(&args.config).await?;

    let mut agent: DefaultAgent = DefaultAgentBuilder {
        config: args.config.clone(),
        model,
        env,
        task: args.task.clone(),
        extra_context: args.extra_context.clone(),
        renderer: None,
    }
    .build()?;

    let exit = agent.run().await?;

    let traj_path = args
        .output_dir
        .join(format!("{}.traj.json", args.trajectory_name));
    agent.trajectory.save_pretty(&traj_path)?;

    if let crate::agent::ExitReason::Submitted { final_output } = &exit {
        let out_path = args
            .output_dir
            .join(format!("{}.output.txt", args.trajectory_name));
        std::fs::write(&out_path, final_output)?;
    }

    tracing::info!(?traj_path, "trajectory written");
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
