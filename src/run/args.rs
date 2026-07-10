use clap::Args;
use std::path::PathBuf;

/// `bench power` — statistical power, sample size, or MDE calculations.
#[derive(Debug, Args, Clone)]
pub struct PowerCmd {
    /// Baseline resolved rate as a float (e.g. `0.35`). Required unless `--from-sweep` is provided.
    #[arg(long)]
    pub baseline_rate: Option<f64>,

    /// Absolute percentage points delta (e.g. `0.05`).
    /// Mutually exclusive with `--n` (Mode A).
    #[arg(long, conflicts_with = "n")]
    pub delta: Option<f64>,

    /// Sample size per arm.
    /// Mutually exclusive with `--delta` (Mode B).
    #[arg(long, conflicts_with = "delta")]
    pub n: Option<usize>,

    /// Sweep directory to load baseline resolved rate from.
    /// Mutually exclusive with `--baseline-rate`.
    #[arg(long, conflicts_with = "baseline_rate")]
    pub from_sweep: Option<PathBuf>,

    /// Significance level / Type I error rate.
    #[arg(long, default_value_t = 0.05)]
    pub alpha: f64,

    /// Target statistical power / 1 - Type II error rate.
    #[arg(long, default_value_t = 0.80)]
    pub power: f64,

    /// Run a one-sided test instead of a two-sided test.
    #[arg(long, default_value_t = false)]
    pub one_sided: bool,

    /// Number of study arms (default is 2). Bonferroni correction is applied if arms > 2.
    #[arg(long, default_value_t = 2)]
    pub arms: usize,

    /// Optional average run cost in USD per instance.
    #[arg(long)]
    pub cost_per_instance: Option<f64>,

    /// Optional path to a forecast report JSON to compute cost from.
    #[arg(long, conflicts_with = "cost_per_instance")]
    pub from_forecast: Option<PathBuf>,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,
}

/// `bench fork` — fork a trajectory at step N and continue with config overrides.
#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ForkCmd {
    /// Completed sweep directory containing the instance trajectory.
    #[arg(long)]
    pub sweep: std::path::PathBuf,

    /// Specific instance id to fork.
    #[arg(long)]
    pub instance: String,

    /// The step number to fork from (zero-indexed).
    #[arg(long = "from-step")]
    pub from_step: u32,

    /// Output directory for the new trajectory.
    #[arg(long)]
    pub output: std::path::PathBuf,

    /// Override model name for the tail (e.g. `claude-opus-4-7`).
    #[arg(long)]
    pub model: Option<String>,

    /// Override system prompt file for the tail.
    #[arg(long = "system-prompt-file")]
    pub system_prompt_file: Option<std::path::PathBuf>,

    /// Override max agent steps.
    #[arg(long)]
    pub step_limit: Option<u32>,

    /// Override per-task USD budget cap for the tail.
    #[arg(long)]
    pub per_task_budget_usd: Option<f64>,

    /// Override MCP stdio server command(s).
    #[arg(long = "mcp-server", value_name = "COMMAND")]
    pub mcp_servers: Vec<String>,

    /// Override path to an MCP config JSON file.
    #[arg(long = "mcp-config")]
    pub mcp_config: Option<std::path::PathBuf>,

    /// Allow forking from a parent trajectory that has no stored input fingerprints.
    #[arg(long, default_value_t = false)]
    pub allow_unfingerprinted: bool,
}

/// `bench merge` — combine completed sharded sweep directories into one canonical aggregate.
///
/// Recombines K independent sharded sweeps into a single canonical sweep directory
/// that is a drop-in for `bench evaluate`, `bench report`, `bench triage`, and `bench audit`.
/// Runs entirely offline with no model or network calls.
#[derive(Debug, Args)]
pub struct MergeCmd {
    /// A completed sweep directory to merge. Repeat for each shard (2+ required).
    #[arg(long = "shard", required = true, action = clap::ArgAction::Append)]
    pub shards: Vec<std::path::PathBuf>,

    /// Destination directory for the merged canonical sweep. Created if absent; must be empty otherwise.
    #[arg(long)]
    pub output: std::path::PathBuf,

    /// Collision policy when the same instance_id appears in more than one shard.
    /// `error` (default): fail with a clear message listing all colliding IDs.
    /// `first-wins`: keep the occurrence from the earliest --shard.
    /// `last-wins`: keep the occurrence from the latest --shard.
    #[arg(long = "on-collision", value_enum, default_value_t = MergeCollisionPolicy::Error)]
    pub on_collision: MergeCollisionPolicy,

    /// Optional human-readable shard labels, positionally aligned with --shard.
    /// Defaults to the directory name of each shard.
    #[arg(long = "label", action = clap::ArgAction::Append)]
    pub labels: Vec<String>,

    /// Overwrite the output directory if it is non-empty.
    #[arg(long, default_value_t = false)]
    pub force: bool,

    /// Output format: `text` (default) or `json`.
    /// The `json` format emits a machine-readable summary to stdout.
    #[arg(long, value_enum, default_value_t = MergeFormat::Text)]
    pub format: MergeFormat,
}

/// Collision policy for `bench merge` when the same instance_id appears in multiple shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MergeCollisionPolicy {
    /// Fail loudly if any instance_id appears in two or more shards (default).
    Error,
    /// Keep the occurrence from the earliest --shard; log duplicates in the report.
    FirstWins,
    /// Keep the occurrence from the latest --shard; log duplicates in the report.
    LastWins,
}

/// Output format for `bench merge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MergeFormat {
    /// Human-readable text summary (default).
    Text,
    /// Machine-readable JSON summary.
    Json,
}

/// `bench audit` — re-derive and verify sweep aggregates against trajectories.
#[derive(Debug, Args, Clone)]
pub struct AuditCmd {
    /// Path to a completed sweep directory or extracted bench bundle directory.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Local dataset file to verify the recorded manifest hash.
    #[arg(long)]
    pub dataset_path: Option<PathBuf>,

    /// USD tolerance for cost reconciliation.
    #[arg(long, default_value_t = 0.0001)]
    pub cost_tolerance_usd: f64,

    /// Seconds tolerance for wall-clock reconciliation.
    #[arg(long, default_value_t = 1.0)]
    pub wallclock_tolerance_secs: f64,

    /// Output format: `text` or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,
}

/// `bench bisect` — identify the commit that introduced a resolved-rate regression.
#[derive(Debug, Args, Clone)]
pub struct BisectCmd {
    /// Known-good sweep directory containing a results.json with manifest.
    #[arg(long)]
    pub good: PathBuf,

    /// Known-bad sweep directory containing a results.json with manifest.
    #[arg(long)]
    pub bad: PathBuf,

    /// Number of smoke instances to sample for the sweep.
    #[arg(long, default_value_t = 5)]
    pub smoke_instances: usize,

    /// RNG seed for sampling candidate smoke instances (defaults to good manifest hash).
    #[arg(long)]
    pub smoke_seed: Option<u64>,

    /// The model to run the smoke sweep against. Defaults to cheapest registered model.
    #[arg(long)]
    pub smoke_model: Option<String>,

    /// Margin under which resolved rate is considered a regression.
    #[arg(long, default_value_t = 0.20)]
    pub regression_margin: f64,

    /// Maximum USD cost before halting and writing partial results.
    #[arg(long)]
    pub max_cost_usd: Option<f64>,

    /// Path to a bisect.json file to resume a previously interrupted run.
    #[arg(long)]
    pub resume: Option<PathBuf>,
}
