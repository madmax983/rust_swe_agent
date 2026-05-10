use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{Agent, DefaultAgent, default::DefaultAgentBuilder};
use crate::config::{Config, EnvKind};
#[cfg(feature = "docker")]
use crate::env::DockerEnvironment;
use crate::env::{Environment, LocalEnvironment};
use crate::error::Error;
use crate::model::{DeterministicModel, Model};
use crate::trajectory::Trajectory;

pub struct ReplayArgs {
    pub trajectory_path: PathBuf,
    pub config: Config,
    pub output_dir: PathBuf,
    pub trajectory_name: Option<String>,
}

pub async fn run(args: ReplayArgs) -> Result<(), Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    // 1. Load the original trajectory
    let file_content = std::fs::read_to_string(&args.trajectory_path)?;
    let orig_trajectory: Trajectory = serde_json::from_str(&file_content).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "Invalid trajectory JSON: {e}"
        )))
    })?;

    // 2. Extract assistant messages (the agent's actions)
    let responses: Vec<String> = orig_trajectory
        .messages
        .into_iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.content)
        .collect();

    if responses.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "No assistant messages found in trajectory to replay".into(),
        )));
    }

    // 3. Setup the replay environment and model
    let model: Arc<dyn Model> = Arc::new(DeterministicModel::new(responses));
    let env = build_env(&args.config).await?;
    let tool_providers = crate::tool::discover_mcp_servers(
        env.as_ref(),
        &args.config.root.agent.mcp_servers,
        args.config.root.agent.tool_hook_timeout_secs,
        None,
    )
    .await?;

    let task = orig_trajectory
        .info
        .task
        .unwrap_or_else(|| "Replayed Task".into());
    let resolved_skills = crate::skills::resolve_for_task(&args.config.root.skills, &task, None)?;
    let traj_name = args
        .trajectory_name
        .unwrap_or_else(|| crate::run::mini::slugify(&task));

    let mut agent: DefaultAgent = DefaultAgentBuilder {
        config: args.config.clone(),
        model,
        env,
        task,
        extra_context: resolved_skills.merged_extra_context,
        renderer: None,
        stream: None,
    }
    .build_with_tool_providers(tool_providers)?;
    if !resolved_skills.active_skills.is_empty() {
        agent.trajectory.info.other.insert(
            "active_skills".into(),
            serde_json::to_value(resolved_skills.active_skills.provenance())?,
        );
    }

    // 4. Run the replay
    tracing::info!(?args.trajectory_path, "starting replay mode");
    let exit = agent.run().await?;

    // 5. Save the new trajectory
    let traj_path = args.output_dir.join(format!("{traj_name}.traj.json"));
    agent.trajectory.save_pretty(&traj_path)?;

    if let crate::agent::ExitReason::Submitted { final_output } = &exit {
        let out_path = args.output_dir.join(format!("{traj_name}.output.txt"));
        std::fs::write(&out_path, final_output)?;
    }

    tracing::info!(?traj_path, "replay trajectory written");
    Ok(())
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::MessageExtra;
    use crate::trajectory::MessageRecord;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_replay_mode() {
        let dir = tempdir().unwrap();

        // Setup a dummy trajectory file
        let traj = Trajectory {
            trajectory_format: "mini-swe-agent-1.1".into(),
            info: crate::trajectory::TrajectoryInfo {
                task: Some("dummy task".into()),
                ..Default::default()
            },
            messages: vec![
                MessageRecord {
                    role: "user".into(),
                    content: "dummy task".into(),
                    extra: MessageExtra::default(),
                },
                MessageRecord {
                    role: "assistant".into(),
                    content: "```bash\necho hi\n```".into(),
                    extra: MessageExtra::default(),
                },
                MessageRecord {
                    role: "user".into(),
                    content: "hi".into(),
                    extra: MessageExtra::default(),
                },
                MessageRecord {
                    role: "assistant".into(),
                    content: "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nhi\n```".into(),
                    extra: MessageExtra::default(),
                },
            ],
        };
        let dummy_path = dir.path().join("input.traj.json");
        traj.save_pretty(&dummy_path).unwrap();

        let cfg = Config::defaults().unwrap();
        let args = ReplayArgs {
            trajectory_path: dummy_path,
            config: cfg,
            output_dir: dir.path().to_path_buf(),
            trajectory_name: Some("replayed-run".into()),
        };

        run(args).await.unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();

        let traj_written = entries
            .iter()
            .any(|e| e.file_name().to_string_lossy() == "replayed-run.traj.json");
        assert!(traj_written);

        let output_written = entries
            .iter()
            .any(|e| e.file_name().to_string_lossy() == "replayed-run.output.txt");
        assert!(output_written);
    }
}
