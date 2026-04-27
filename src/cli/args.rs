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
    /// Evaluate a completed sweep with an evaluation backend (e.g. sb-cli).
    Evaluate(EvaluateCmd),
    /// Inspect a single trajectory or list filtered instance summaries.
    Inspect(InspectCmd),
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

    /// Dataset subset selector. Either a comma-separated id list
    /// (`id1,id2`) or `@path/to/file.txt` with one id per line.
    #[arg(long)]
    pub instance_ids: Option<String>,

    /// Keep at most N instances after `--instance-ids` and `--sample`.
    /// Composition order is stable: `instance-ids` -> `sample` -> `limit`.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Reproducibly random-subset to N instances (requires `--seed`).
    #[arg(long)]
    pub sample: Option<usize>,

    /// RNG seed used by `--sample`.
    #[arg(long)]
    pub seed: Option<u64>,
}

#[derive(Debug, Args)]
pub struct EvaluateCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Optional dataset JSONL path passed through to the evaluator.
    #[arg(long)]
    pub dataset: Option<PathBuf>,

    /// SWE-bench subset for sb-cli (`swe-bench-m`, `swe-bench_lite`, ...).
    #[arg(long, default_value = "swe-bench-m")]
    pub sb_subset: String,

    /// SWE-bench split for sb-cli (`dev` or `test` depending on subset).
    #[arg(long, default_value = "dev")]
    pub sb_split: String,

    /// Optional sb-cli run id. When unset, one is generated automatically.
    #[arg(long)]
    pub run_id: Option<String>,

    /// Evaluation backend: `sb-cli` or `none`.
    #[arg(long, default_value = "sb-cli")]
    pub backend: String,

    /// Per-instance evaluation timeout in seconds.
    #[arg(long, default_value_t = 600)]
    pub timeout_per_instance: u64,

    /// Parallel worker count for evaluation backend.
    #[arg(long, default_value_t = 4)]
    pub parallel: usize,
}

#[derive(Debug, Args)]
pub struct InspectCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// One specific instance id to render as a human-readable transcript.
    #[arg(long)]
    pub instance: Option<String>,

    /// Filter mode: `resolved=false` or `failure_category=patch_apply_failed`.
    #[arg(long)]
    pub filter: Option<String>,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Disable stdout/stderr truncation in transcript mode.
    #[arg(long, default_value_t = false)]
    pub full: bool,
}
