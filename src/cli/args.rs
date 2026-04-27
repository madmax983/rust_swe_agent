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
    /// Diff two completed sweep runs by instance id; surfaces regressions
    /// and (with `--max-regressions`) gates CI on prompt/harness changes.
    Compare(CompareCmd),
}

#[derive(Debug, Args)]
pub struct CompareCmd {
    /// Sweep output directory written by a prior `bench swebench` run
    /// (must contain `results.json` or per-instance `*.traj.json` files).
    /// Treated as the "before" side of the diff.
    #[arg(long)]
    pub baseline: PathBuf,

    /// Sweep output directory to compare against the baseline. Treated as
    /// the "after" side; regressions are tasks that passed in baseline
    /// but failed here.
    #[arg(long)]
    pub candidate: PathBuf,

    /// Output format: `text` (default, terminal-friendly) or `json`
    /// (machine-readable diff document).
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Exit non-zero when regressed-task count strictly exceeds N. Unset
    /// (default) is informational only — the report prints regardless,
    /// but the process always exits 0. Set to 0 for a strict gate that
    /// fails on any regression.
    #[arg(long)]
    pub max_regressions: Option<usize>,
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

    /// Skip tasks whose output trajectory file already exists on disk and
    /// parses as valid JSON. Lets an interrupted sweep resume without
    /// re-spending API budget on already-completed work. Files that fail
    /// JSON parsing (e.g. truncated by a mid-write crash) are treated as
    /// absent and re-run.
    #[arg(long, default_value_t = false)]
    pub resume: bool,

    /// Maximum total USD spend for the entire sweep. When set, the runner
    /// stops dequeuing new tasks once cumulative cost reaches the limit;
    /// in-flight tasks are allowed to finish so trajectories and patch
    /// artifacts are not corrupted. Tasks that never started are
    /// recorded with `exit_reason: "budget_halt"` and excluded from
    /// `submitted` / `errored`. When unset, behavior is unchanged.
    #[arg(long)]
    pub sweep_cost_limit_usd: Option<f64>,
}
