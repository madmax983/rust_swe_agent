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

    /// Per-task wallclock timeout in seconds. Default: unset (no timeout).
    /// Orthogonal to `--step-limit`; whichever fires first wins.
    #[arg(long)]
    pub task_timeout_secs: Option<u64>,

    /// Optional path to a TOML config (overlays defaults).
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

    /// Optional path to a TOML config (overlays defaults).
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
    /// Forecast sweep cost from a reproducible calibration slice.
    Forecast(SwebenchCmd),
    /// Validate sweep inputs without launching tasks.
    Doctor(SwebenchCmd),
    /// Diff two completed sweep runs by instance id; surfaces regressions
    /// and (with `--max-regressions`) gates CI on prompt/harness changes.
    Compare(CompareCmd),
    /// Evaluate a completed sweep with an evaluation backend (e.g. sb-cli).
    Evaluate(EvaluateCmd),
    /// Inspect a single trajectory or list filtered instance summaries.
    Inspect(InspectCmd),
    /// Tail live aggregate progress for a running sweep directory.
    Tail(TailCmd),
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
    /// (machine-readable report). `unified` is accepted with
    /// `--inspect-diff`.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Exit non-zero when regressed-task count strictly exceeds N. Unset
    /// (default) is informational only — the report prints regardless,
    /// but the process always exits 0. Set to 0 for a strict gate that
    /// fails on any regression.
    #[arg(long)]
    pub max_regressions: Option<usize>,

    /// Optional metric breakdown axes (`repo,failure_category`) or `none`.
    #[arg(long, default_value = "none")]
    pub breakdown: String,

    /// Threshold in percentage points used to highlight large breakdown deltas.
    #[arg(long = "breakdown-min-delta-pp", default_value_t = 5.0)]
    pub breakdown_min_delta_pp: f64,

    /// Render `bench inspect --diff` for this instance id by locating both
    /// trajectory files inside the baseline and candidate sweep directories.
    #[arg(long)]
    pub inspect_diff: Option<String>,

    /// Write a shell script with one `bench inspect --diff` command per
    /// regressed instance. The script is not executed automatically.
    #[arg(long)]
    pub emit_diff_script: Option<PathBuf>,

    /// Include whitespace-only and timestamp-only trajectory differences in
    /// inspect-diff output.
    #[arg(long, default_value_t = false)]
    pub show_noise: bool,
}

#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct SwebenchCmd {
    #[arg(long)]
    pub dataset_path: PathBuf,

    #[arg(long, alias = "output-dir")]
    pub output: PathBuf,

    #[arg(long, default_value_t = 4)]
    pub parallel: usize,

    /// Run each selected SWE-bench instance N independent times.
    #[arg(long = "rerun", alias = "samples", default_value_t = 1)]
    pub reruns: u32,

    #[arg(long, default_value = "claude-opus-4-7")]
    pub model: String,

    #[arg(long, default_value_t = 50)]
    pub step_limit: u32,

    /// Per-task wallclock timeout in seconds. Default: unset (no timeout).
    /// Orthogonal to `--step-limit`; whichever fires first wins.
    #[arg(long)]
    pub task_timeout_secs: Option<u64>,

    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Environment: `local` or `docker`.
    #[arg(long)]
    pub env: Option<String>,

    /// Docker image, if `--env docker`.
    #[arg(long)]
    pub docker_image: Option<String>,

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

    /// Retry transiently-failed instances up to N additional attempts.
    /// `0` disables retries entirely.
    #[arg(long, default_value_t = 2)]
    pub max_retries: u32,

    /// Comma-separated failure categories to retry (defaults to transient set).
    #[arg(long)]
    pub retry_on: Option<String>,

    /// Exponential backoff base delay in milliseconds between retries.
    #[arg(long, default_value_t = 1000)]
    pub retry_backoff_base_ms: u64,

    /// Exponential backoff max delay cap in seconds.
    #[arg(long, default_value_t = 60)]
    pub retry_backoff_cap_s: u64,

    /// With `--resume`, re-run previously completed instances whose stored
    /// `failure_category` is retryable instead of skipping them.
    #[arg(long, default_value_t = false)]
    pub retry_on_resume: bool,

    /// Run preflight checks and exit without launching tasks.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Skip startup preflight checks before launching a sweep.
    #[arg(long, default_value_t = false)]
    pub skip_preflight: bool,

    /// Doctor/dry-run output format.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Skip model backend probe during preflight.
    #[arg(long, default_value_t = false)]
    pub skip_model_probe: bool,

    /// Max seconds per preflight check.
    #[arg(long, default_value_t = 10)]
    pub preflight_check_timeout_s: u64,

    /// Max total seconds for all preflight checks.
    #[arg(long, default_value_t = 60)]
    pub preflight_total_timeout_s: u64,

    /// Run a calibration forecast before the real sweep and launch only
    /// when the forecast clears `--sweep-cost-limit-usd` or `--yes` is set.
    #[arg(long, default_value_t = false)]
    pub forecast_first: bool,

    /// Proceed after `--forecast-first` even without a clear cost-cap pass.
    #[arg(long, default_value_t = false)]
    pub yes: bool,

    /// Instance count for `bench forecast` calibration.
    #[arg(long, default_value_t = 5)]
    pub calibration_n: usize,

    /// Forecast target instance count. Defaults to full post-filter dataset.
    #[arg(long)]
    pub target_n: Option<usize>,

    /// Confidence level percentage for forecast intervals.
    #[arg(long, default_value_t = 80.0)]
    pub confidence: f64,

    /// Exit non-zero when the forecast projects the sweep will exceed cap.
    #[arg(long, default_value_t = false)]
    pub fail_over_cap: bool,
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

    /// Optional metric breakdown axes (`repo,failure_category`) or `none`.
    #[arg(long, default_value = "repo,failure_category")]
    pub breakdown: String,
}

#[derive(Debug, Args)]
pub struct InspectCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: Option<PathBuf>,

    /// One specific instance id to render as a human-readable transcript.
    #[arg(long)]
    pub instance: Option<String>,

    /// Filter mode: `resolved=false` or `failure_category=patch_apply_failed`.
    #[arg(long)]
    pub filter: Option<String>,

    /// Diff two trajectory JSON files for the same instance.
    #[arg(long, value_names = ["BASELINE", "CANDIDATE"], num_args = 2)]
    pub diff: Vec<PathBuf>,

    /// Include whitespace-only and timestamp-only differences in diff mode.
    #[arg(long, default_value_t = false)]
    pub show_noise: bool,

    /// Output format: `text` (default), `json`, or `unified` in diff mode.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Disable stdout/stderr truncation in transcript mode.
    #[arg(long, default_value_t = false)]
    pub full: bool,
}

#[derive(Debug, Args)]
pub struct TailCmd {
    /// Sweep output directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Refresh interval for streaming mode.
    #[arg(long, default_value_t = 2000)]
    pub interval_ms: u64,

    /// Print one snapshot and exit.
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,
}
