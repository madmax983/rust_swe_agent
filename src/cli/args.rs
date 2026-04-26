//! clap subcommand arg structs.

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct MiniCmd {
    /// The task prompt.
    #[arg(long)]
    pub task: String,

    /// Additional context appended to the instance prompt.
    #[arg(long)]
    pub extra_context: Option<String>,

    /// Model name (e.g. `claude-opus-4-7`).
    #[arg(long, default_value = "claude-opus-4-7")]
    pub model: String,

    /// Max agent steps.
    #[arg(long, default_value_t = 50)]
    pub step_limit: u32,

    /// Optional path to a YAML config (overlays defaults).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Environment: `local` or `docker`.
    #[arg(long)]
    pub env: Option<String>,

    /// Docker image, if `--env docker`.
    #[arg(long)]
    pub docker_image: Option<String>,

    /// Output directory for trajectories.
    #[arg(long, default_value = "./runs")]
    pub output: PathBuf,

    /// Override trajectory filename (default: slugified task).
    #[arg(long)]
    pub trajectory_name: Option<String>,

    /// Stream trajectory events over HTTP/SSE on the given `host:port`
    /// (e.g. `127.0.0.1:7878`). Use port `0` to let the OS pick. When
    /// unset, no server is started.
    #[arg(long)]
    pub stream: Option<String>,
}

#[derive(Debug, Args)]
pub struct HelloWorldCmd {
    #[arg(long, default_value = "./runs")]
    pub output: PathBuf,
}

#[derive(Debug, Args)]
pub struct ReplayCmd {
    /// Path to the original trajectory JSON file.
    #[arg(long)]
    pub trajectory_path: PathBuf,

    /// Optional path to a YAML config (overlays defaults).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Environment: `local` or `docker`.
    #[arg(long)]
    pub env: Option<String>,

    /// Docker image, if `--env docker`.
    #[arg(long)]
    pub docker_image: Option<String>,

    /// Output directory for trajectories.
    #[arg(long, default_value = "./runs")]
    pub output: PathBuf,

    /// Override trajectory filename (default: derived from task).
    #[arg(long)]
    pub trajectory_name: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum BenchCmd {
    /// Run a SWE-bench sweep over a local JSONL dataset.
    Swebench(SwebenchCmd),
}

#[derive(Debug, Args)]
pub struct SwebenchCmd {
    #[arg(long)]
    pub dataset_path: PathBuf,

    #[arg(long)]
    pub output: PathBuf,

    #[arg(long, default_value_t = 4)]
    pub parallel: usize,

    #[arg(long, default_value = "claude-opus-4-7")]
    pub model: String,

    #[arg(long, default_value_t = 50)]
    pub step_limit: u32,

    #[arg(long)]
    pub config: Option<PathBuf>,
}
