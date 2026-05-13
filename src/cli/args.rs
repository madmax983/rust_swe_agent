//! clap subcommand arg structs.

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StratifyByArg {
    Repo,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StratifyModeArg {
    Proportional,
    Balanced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnOffArg {
    On,
    Off,
}

#[derive(Debug, Clone, Args)]
pub struct MiniGithubPrArgs {
    /// Open a GitHub pull request from the final patch after a submitted run.
    #[arg(long = "open-pr", default_value_t = false)]
    pub open_pr: bool,

    /// GitHub repository to target, in `owner/name` form.
    #[arg(long)]
    pub target_repo: Option<String>,

    /// Base branch for the pull request and patch capture.
    #[arg(long)]
    pub target_branch: Option<String>,

    /// Environment variable containing a GitHub App installation token or PAT.
    #[arg(long, default_value = "GITHUB_TOKEN")]
    pub github_token_env: String,

    /// Print PR title/body/base/head/patch summary without GitHub API calls.
    #[arg(long, default_value_t = false)]
    pub github_pr_dry_run: bool,

    /// Max seconds spent opening a PR after the run submits.
    #[arg(long, default_value_t = 30)]
    pub github_pr_timeout_secs: u64,

    /// Max retries for rate-limited or transient GitHub API responses.
    #[arg(long, default_value_t = 2)]
    pub github_pr_max_retries: u32,

    /// Base backoff delay in milliseconds for GitHub API retries.
    #[arg(long, default_value_t = 250)]
    pub github_pr_backoff_base_ms: u64,

    /// Deterministic head branch prefix for agent PRs.
    #[arg(long, default_value = "rust-swe-agent")]
    pub github_pr_branch_prefix: String,
}

#[derive(Debug, Clone, Args)]
pub struct SwebenchGithubPrArgs {
    /// Open GitHub pull requests from submitted sweep patch artifacts.
    #[arg(long = "open-prs", default_value_t = false)]
    pub open_prs: bool,

    /// GitHub repository to target, in `owner/name` form.
    #[arg(long)]
    pub target_repo: Option<String>,

    /// Base branch for pull requests.
    #[arg(long)]
    pub target_branch: Option<String>,

    /// Environment variable containing a GitHub App installation token or PAT.
    #[arg(long, default_value = "GITHUB_TOKEN")]
    pub github_token_env: String,

    /// Print PR title/body/base/head/patch summary without GitHub API calls.
    #[arg(long, default_value_t = false)]
    pub github_pr_dry_run: bool,

    /// Max seconds each sweep worker may spend opening a PR.
    #[arg(long, default_value_t = 30)]
    pub github_pr_timeout_secs: u64,

    /// Max retries for rate-limited or transient GitHub API responses.
    #[arg(long, default_value_t = 2)]
    pub github_pr_max_retries: u32,

    /// Base backoff delay in milliseconds for GitHub API retries.
    #[arg(long, default_value_t = 250)]
    pub github_pr_backoff_base_ms: u64,

    /// Deterministic head branch prefix for agent PRs.
    #[arg(long, default_value = "rust-swe-agent")]
    pub github_pr_branch_prefix: String,
}

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
    #[arg(long)]
    pub observation_max_bytes: Option<usize>,
    #[arg(long)]
    pub observation_head_ratio: Option<f64>,

    /// Per-task wallclock timeout in seconds. Default: unset (no timeout).
    /// Orthogonal to `--step-limit`; whichever fires first wins.
    #[arg(long)]
    pub task_timeout_secs: Option<u64>,

    /// Per-task USD ceiling enforced inside the agent loop. When cumulative
    /// task spend meets this value, the loop terminates with
    /// `failure_category: budget_exhausted` and any patch is preserved.
    /// Default: unset (no per-task cap). Opt-in; does not affect the sweep
    /// cost cap (`--sweep-cost-limit-usd`).
    #[arg(long)]
    pub per_task_budget_usd: Option<f64>,

    /// Hide the budget status block from the agent's observations. When set,
    /// the agent cannot see its remaining per-task budget even if
    /// `--per-task-budget-usd` is active. Useful for A/B experiments.
    #[arg(long, default_value_t = false)]
    pub hide_budget_from_agent: bool,

    /// Register an invocation-time MCP stdio server command.
    #[arg(long = "mcp-server", value_name = "COMMAND")]
    pub mcp_servers: Vec<String>,

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

    /// Skip `git apply --check` and empty-diff validation after patch capture.
    /// Escape hatch for non-git environments; not for normal use.
    #[arg(long, default_value_t = false)]
    pub skip_patch_validation: bool,

    /// Operator-supplied verification check. Format: `NAME:COMMAND`.
    /// Can be repeated for multiple checks. After the agent finishes, each
    /// check is run once; if any fail the artifact is marked
    /// `verification_failed` and the command exits non-zero.
    #[arg(long = "verify", value_name = "NAME:COMMAND")]
    pub verify: Vec<String>,

    /// Per-check timeout in seconds for `--verify` checks. Default: 60.
    #[arg(long = "verify-timeout-secs", default_value_t = 60)]
    pub verify_timeout_secs: u64,

    /// Enable or disable in-loop stagnation detection. Omitting the flag
    /// preserves the config-file value (default: on). `--detect-stagnation`
    /// or `--detect-stagnation=true` enables; `--detect-stagnation=false`
    /// disables, overriding any config-file setting.
    #[arg(long = "detect-stagnation", num_args = 0..=1, default_missing_value = "true")]
    pub detect_stagnation: Option<bool>,

    /// Number of times the same action must appear in the trailing window
    /// before stagnation is declared. Overrides the config-file value when set.
    /// Schema default: 4.
    #[arg(long)]
    pub stagnation_repeat_threshold: Option<u32>,

    /// Size of the trailing-steps window examined by the stagnation detector.
    /// Must be >= `--stagnation-repeat-threshold`. Overrides the config-file
    /// value when set. Schema default: 8.
    #[arg(long)]
    pub stagnation_window: Option<u32>,

    /// Token budget for the model-visible prompt. Older tool observations are
    /// elided oldest-first when the projected input would exceed this value.
    /// Default: unset (no cap). Overrides the config-file value when set.
    #[arg(long)]
    pub history_max_input_tokens: Option<u64>,

    /// Keep only the last N tool observations in the model-visible prompt.
    /// Older ones are replaced with a short elision marker. Default: unset.
    /// Overrides the config-file value when set.
    #[arg(long)]
    pub history_keep_last_observations: Option<usize>,

    #[command(flatten)]
    pub github_pr: MiniGithubPrArgs,
}

#[derive(Debug, Args)]
pub struct HelloWorldCmd {
    #[arg(long, default_value = "./runs")]
    pub output: PathBuf,

    /// Optional path to a TOML config (overlays defaults).
    #[arg(long)]
    pub config: Option<PathBuf>,
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

    /// Allow replaying trajectories that have no stored input fingerprints
    /// (pre-feature "legacy" trajectories). A warning is emitted to stderr.
    /// Without this flag, an unfingerprinted trajectory causes a non-zero exit.
    #[arg(long, default_value_t = false)]
    pub allow_unfingerprinted: bool,

    /// Run to completion despite prompt drift, collect all divergent steps into
    /// `replay-drift.json`, and exit 0. Useful for "show me everything that
    /// changed" diagnostics. Without this flag replay stops at the first drift.
    #[arg(long, default_value_t = false)]
    pub report_only: bool,

    /// Maximum bytes of the actual canonical input JSON to include per drift
    /// step in the drift report. Excess is replaced with `[truncated]`.
    #[arg(long, default_value_t = crate::run::replay::DEFAULT_DRIFT_CAP_BYTES)]
    pub drift_cap_bytes: usize,
}

#[derive(Debug, Subcommand)]
pub enum BenchCmd {
    /// Run a SWE-bench sweep over a local JSONL dataset.
    Swebench(SwebenchCmd),
    /// Forecast sweep cost from a reproducible calibration slice.
    Forecast(SwebenchCmd),
    /// Compare a forecast artifact against completed sweep results.
    Calibrate(CalibrateCmd),
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
    /// Cluster unresolved sweep failures into ranked actionable groups.
    Triage(TriageCmd),
    /// Aggregate shell-command frequency and cost by outcome bucket.
    CommandStats(CommandStatsCmd),
    /// Pareto frontier across multiple sweep runs: ASCII chart + JSON dataset.
    Frontier(FrontierCmd),
    /// Replay a saved sweep from its manifest and report reproducibility.
    Reproduce(ReproduceCmd),
    /// Export or verify a portable, redacted sweep archive.
    Bundle(BundleCmd),
    /// Run multiple sweep arms against the same instance set.
    Matrix(MatrixCmd),
    /// Verify the evaluator pipeline using gold patches as a zero-cost preflight.
    EvaluatorSelftest(EvaluatorSelftestCmd),
    /// Produce a shareable markdown or HTML sweep summary.
    Report(ReportCmd),
}

#[derive(Debug, Args)]
pub struct MatrixCmd {
    /// Path to the TOML matrix manifest with `[[arm]]` entries.
    #[arg(long)]
    pub config: PathBuf,

    /// Local JSONL dataset file. Mutually exclusive with `--dataset`.
    #[arg(long)]
    pub dataset_path: Option<PathBuf>,

    /// Named SWE-bench dataset alias (e.g. `verified`). Alternative to `--dataset-path`.
    #[arg(long, value_name = "ALIAS")]
    pub dataset: Option<String>,

    /// Dataset split for named aliases: `train`, `test`, or `dev`.
    #[arg(long, default_value = "test")]
    pub split: Option<String>,

    /// Directory for the named-dataset on-disk cache.
    #[arg(long)]
    pub dataset_cache_dir: Option<PathBuf>,

    /// Root output directory (arm results land in `{output}/{arm_name}/`).
    #[arg(long)]
    pub output: PathBuf,

    /// Shared USD ceiling across all arms. Arms that would start after the
    /// limit is reached are recorded as `skipped_budget`.
    #[arg(long)]
    pub sweep_cost_limit_usd: Option<f64>,

    /// Number of arms to run concurrently (default: 1 = sequential).
    #[arg(long, default_value_t = 1)]
    pub matrix_parallelism: usize,

    /// Resume from a previous run, skipping `complete` and `skipped_budget` arms.
    #[arg(long, default_value_t = false)]
    pub resume: bool,

    /// Dataset subset selector. Either a comma-separated id list
    /// (`id1,id2`) or `@path/to/file.txt` with one id per line.
    #[arg(long)]
    pub instance_ids: Option<String>,

    /// Keep at most N instances after filtering and sampling.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Reproducibly random-subset to N instances (requires `--seed`).
    #[arg(long)]
    pub sample: Option<usize>,

    /// RNG seed used by `--sample`.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Stratify `--sample` by key.
    #[arg(long, value_enum)]
    pub stratify_by: Option<StratifyByArg>,

    /// Allocation mode used with `--stratify-by`.
    #[arg(long, value_enum)]
    pub stratify_mode: Option<StratifyModeArg>,

    /// Worker parallelism per arm sweep.
    #[arg(long, default_value_t = crate::run::swebench::DEFAULT_PARALLEL)]
    pub parallel: usize,

    /// Skip startup preflight checks before launching arm sweeps.
    #[arg(long, default_value_t = false)]
    pub skip_preflight: bool,

    /// Skip model-endpoint probe during preflight.
    #[arg(long, default_value_t = false)]
    pub skip_model_probe: bool,

    /// Seconds each arm sweep waits for in-flight tasks after a cancel signal.
    #[arg(long, default_value_t = 60)]
    pub cancel_deadline_secs: u64,
}

#[derive(Debug, Args)]
pub struct ReproduceCmd {
    /// Source sweep directory to reproduce (must contain `results.json`
    /// with an embedded `ProvenanceManifest`).
    #[arg(long)]
    pub from: PathBuf,

    /// Output directory for the new sweep artifacts and `reproducibility.json`.
    #[arg(long)]
    pub output: PathBuf,

    /// Drift field names to whitelist. Hard-drift fields not in this list
    /// abort with a non-zero exit. May be repeated.
    /// Example: `--allow-drift harness.git_sha`
    #[arg(long = "allow-drift", value_name = "FIELD")]
    pub allow_drift: Vec<String>,

    /// Limit replay to at most N instances (partial replay).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Instance-id filter: comma-separated ids or `@path/to/file.txt`.
    #[arg(long)]
    pub filter: Option<String>,

    /// Override per-task USD ceiling for the replay sweep.
    #[arg(long)]
    pub per_task_budget_usd: Option<f64>,

    /// Number of instances to run in parallel during the replay sweep.
    /// Defaults to 4 when omitted.
    #[arg(long, default_value_t = 4)]
    pub parallel: usize,

    /// Skip model-endpoint probe during preflight (useful for CI fixtures
    /// and dry-run modes that don't spend model credits).
    #[arg(long, default_value_t = false)]
    pub skip_model_probe: bool,
}

#[derive(Debug, Args)]
pub struct BundleCmd {
    /// Source sweep directory to archive.
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with = "verify",
        requires = "output"
    )]
    pub sweep: Option<PathBuf>,

    /// Archive path to write (`.tar.gz`).
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "verify",
        requires = "sweep"
    )]
    pub output: Option<PathBuf>,

    /// Restrict the bundle to one instance id.
    #[arg(long, value_name = "ID", conflicts_with = "verify", requires = "sweep")]
    pub instance: Option<String>,

    /// Verify an existing bundle archive instead of creating one.
    #[arg(
        long,
        value_name = "ARCHIVE",
        conflicts_with_all = ["sweep", "output", "instance"]
    )]
    pub verify: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CalibrateCmd {
    /// Forecast JSON artifact emitted by `bench forecast --format json`.
    #[arg(long)]
    pub forecast: PathBuf,

    /// Completed sweep `results.json` artifact emitted by `bench swebench`.
    #[arg(long)]
    pub results: PathBuf,

    /// Path to write the versioned calibration JSON artifact.
    /// Defaults to `calibration.json` next to `--results`.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Output format for stdout: `text` (default) or `json`.
    /// The JSON artifact is written to `--output` in both modes.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Exit non-zero with `calibration_optimistic` when the verdict is optimistic.
    #[arg(long, default_value_t = false)]
    pub fail_on_optimistic: bool,
}

#[derive(Debug, Args)]
pub struct FrontierCmd {
    /// Sweep output directories to compare (each must contain `results.json`
    /// and optionally `evaluation.json`).
    #[arg(required = true)]
    pub dirs: Vec<std::path::PathBuf>,

    /// Output format: `text` (default, ASCII chart) or `json` (machine-readable).
    #[arg(long, value_enum, default_value_t = crate::run::frontier::FrontierFormat::Text)]
    pub format: crate::run::frontier::FrontierFormat,
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

    /// Exit non-zero when candidate mean lines changed over resolved instances
    /// exceeds baseline by more than N percent. Unset is informational only.
    #[arg(long = "max-patch-size-regression")]
    pub max_patch_size_regression: Option<f64>,

    /// Optional metric breakdown axes (`repo,failure_category`) or `none`.
    #[arg(long, default_value = "none")]
    pub breakdown: String,

    /// Threshold in percentage points used to highlight large breakdown deltas.
    #[arg(long = "breakdown-min-delta-pp", default_value_t = 5.0)]
    pub breakdown_min_delta_pp: f64,

    /// Attribute sweep USD cost to terminal buckets in the compare report.
    #[arg(long, value_enum, default_value_t = OnOffArg::On)]
    pub cost_attribution: OnOffArg,

    /// Highlight cost-attribution deltas whose absolute USD change meets this threshold.
    #[arg(long = "cost-attribution-min-delta-usd", default_value_t = 1.0)]
    pub cost_attribution_min_delta_usd: f64,

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
    /// Local JSONL dataset file. Mutually exclusive with `--dataset`.
    /// Exactly one of `--dataset-path` or `--dataset` must be provided.
    #[arg(long)]
    pub dataset_path: Option<PathBuf>,

    /// Named SWE-bench dataset alias: `full`, `lite`, or `verified`.
    /// Mutually exclusive with `--dataset-path`.
    /// Resolved against the on-disk cache (see `--dataset-cache-dir`).
    #[arg(long, value_name = "ALIAS")]
    pub dataset: Option<String>,

    /// Dataset split for named aliases: `train`, `test`, or `dev`.
    /// Ignored when `--dataset-path` is used.
    #[arg(long, default_value = "test", value_name = "SPLIT")]
    pub split: Option<String>,

    /// Directory for the named-dataset on-disk cache.
    /// Defaults to `~/.cache/rust-swe-agent/datasets`.
    #[arg(long, value_name = "DIR")]
    pub dataset_cache_dir: Option<PathBuf>,

    #[arg(long, alias = "output-dir")]
    pub output: PathBuf,

    #[arg(long, default_value_t = crate::run::swebench::DEFAULT_PARALLEL)]
    pub parallel: usize,

    /// Run each selected SWE-bench instance N independent times.
    #[arg(long = "rerun", alias = "samples", default_value_t = 1)]
    pub reruns: u32,

    #[arg(long, default_value = "claude-opus-4-7")]
    pub model: String,

    #[arg(long, default_value_t = 50)]
    pub step_limit: u32,
    #[arg(long)]
    pub observation_max_bytes: Option<usize>,
    #[arg(long)]
    pub observation_head_ratio: Option<f64>,

    /// Per-task wallclock timeout in seconds. Default: unset (no timeout).
    /// Orthogonal to `--step-limit`; whichever fires first wins.
    #[arg(long)]
    pub task_timeout_secs: Option<u64>,

    /// Per-task USD ceiling enforced inside the agent loop. When cumulative
    /// task spend meets this value, the loop terminates with
    /// `failure_category: budget_exhausted` and any patch is preserved.
    /// Default: unset (no per-task cap). Opt-in; does not affect the sweep
    /// cost cap (`--sweep-cost-limit-usd`).
    #[arg(long)]
    pub per_task_budget_usd: Option<f64>,

    /// Hide the budget status block from the agent's observations. When set,
    /// the agent cannot see its remaining per-task budget even if
    /// `--per-task-budget-usd` is active. Useful for A/B experiments.
    #[arg(long, default_value_t = false)]
    pub hide_budget_from_agent: bool,

    /// Register an invocation-time MCP stdio server command.
    #[arg(long = "mcp-server", value_name = "COMMAND")]
    pub mcp_servers: Vec<String>,

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

    /// Stratify `--sample` by key.
    #[arg(long, value_enum)]
    pub stratify_by: Option<StratifyByArg>,

    /// Allocation mode used with `--stratify-by`.
    #[arg(long, value_enum)]
    pub stratify_mode: Option<StratifyModeArg>,

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

    /// Skip `git apply --check` and empty-diff validation after patch capture.
    /// Escape hatch for non-git environments; not for normal use.
    #[arg(long, default_value_t = false)]
    pub skip_patch_validation: bool,

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

    /// Cap aggregate provider request rate across all workers (requests/min).
    /// When set, workers block (not spin) until budget is available.
    /// Does NOT count blocked time against `--task-timeout`.
    /// When unset, no RPM ceiling is enforced (opt-in, no behavior change).
    #[arg(long)]
    pub max_rpm: Option<u32>,

    /// Cap aggregate input-token rate across all workers (tokens/min).
    /// When set, workers block until the TPM bucket has capacity.
    /// When unset, no TPM ceiling is enforced (opt-in, no behavior change).
    #[arg(long)]
    pub max_input_tpm: Option<u64>,

    /// Seconds to wait after first Ctrl-C before forcing in-flight tasks
    /// to persist `exit_reason: "cancelled"`.
    #[arg(long, default_value_t = 30)]
    pub cancel_deadline: u64,

    /// Enable the mid-sweep circuit breaker that halts when a systemic failure
    /// pattern emerges (default: true).  When >= --systemic-failure-min-samples
    /// instances have completed and >= --systemic-failure-share-pct% share the
    /// same actionable failure category, the sweep stops dispatching new work,
    /// drains in-flight tasks, and exits with code 11.  Actionable categories
    /// are: model_api (bad API key, quota exhausted, wrong model name) and
    /// env_setup (Docker daemon down, unreachable image).  Non-actionable
    /// categories (step_limit, patch_empty, etc.) never trip the breaker.
    /// Pass --abort-on-systemic-failure=false to opt out entirely.
    #[arg(long, default_value_t = true, num_args = 0..=1, default_missing_value = "true")]
    pub abort_on_systemic_failure: bool,

    /// Minimum number of completed instances required before the circuit
    /// breaker is eligible to trip.  Raise this for large sweeps where a few
    /// early failures are expected noise.
    #[arg(long, default_value_t = 5)]
    pub systemic_failure_min_samples: usize,

    /// Percentage share (0–100) of completed instances that must share the
    /// same actionable failure category for the circuit breaker to trip.
    #[arg(long, default_value_t = 80)]
    pub systemic_failure_share_pct: u8,

    /// Enable or disable in-loop stagnation detection. Omitting the flag
    /// preserves the config-file value (default: on). `--detect-stagnation`
    /// or `--detect-stagnation=true` enables; `--detect-stagnation=false`
    /// disables, overriding any config-file setting.
    #[arg(long = "detect-stagnation", num_args = 0..=1, default_missing_value = "true")]
    pub detect_stagnation: Option<bool>,

    /// Number of times the same action must appear within the window before
    /// the stagnation detector trips. Overrides the config-file value when set.
    /// Schema default: 4.
    #[arg(long)]
    pub stagnation_repeat_threshold: Option<u32>,

    /// Sliding window size (in steps) used by the stagnation detector.
    /// Overrides the config-file value when set. Schema default: 8.
    #[arg(long)]
    pub stagnation_window: Option<u32>,

    /// Token budget for the model-visible prompt. Older tool observations are
    /// elided oldest-first when the projected input would exceed this value.
    /// Default: unset (no cap). Overrides the config-file value when set.
    #[arg(long)]
    pub history_max_input_tokens: Option<u64>,

    /// Keep only the last N tool observations in the model-visible prompt.
    /// Older ones are replaced with a short elision marker. Default: unset.
    /// Overrides the config-file value when set.
    #[arg(long)]
    pub history_keep_last_observations: Option<usize>,

    #[command(flatten)]
    pub github_pr: SwebenchGithubPrArgs,
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

    /// Attribute sweep USD cost to terminal buckets in the evaluation report.
    #[arg(long, value_enum, default_value_t = OnOffArg::On)]
    pub cost_attribution: OnOffArg,
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

#[derive(Debug, Args)]
pub struct TriageCmd {
    /// Completed sweep directory produced by `bench swebench` and scored by
    /// `bench evaluate`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Restrict clusters to one failure category bucket, such as
    /// `model_parse` or `env_setup`.
    #[arg(long)]
    pub bucket: Option<String>,

    /// Hide clusters smaller than this size.
    #[arg(long, default_value_t = 1)]
    pub min_cluster_size: usize,

    /// Number of ranked clusters to print in text mode.
    #[arg(long, default_value_t = 10)]
    pub top: usize,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Debug, Args)]
pub struct CommandStatsCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Restrict output to one outcome bucket: `resolved`, `unresolved`,
    /// `errored`, or `all`.
    #[arg(long)]
    pub bucket: Option<String>,

    /// Hide commands with fewer than N total invocations (default: 1).
    #[arg(long, default_value_t = 1)]
    pub min_invocations: usize,

    /// Number of top rows by invocation count to print per bucket (default: 15).
    #[arg(long, default_value_t = 15)]
    pub top: usize,

    /// Emit a delta table comparing two outcome buckets.
    /// Currently only `resolved-vs-unresolved` is supported.
    #[arg(long)]
    pub compare: Option<String>,

    /// Filter instances using the same syntax as `bench inspect --filter`.
    /// Example: `failure_category=model_parse`.
    #[arg(long)]
    pub filter: Option<String>,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Debug, Args)]
pub struct ReportCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Output file path for the report.
    #[arg(long)]
    pub output: PathBuf,

    /// Optional baseline sweep directory for delta comparison using `bench compare` machinery.
    #[arg(long)]
    pub baseline: Option<PathBuf>,

    /// Number of top failed instances to include in the report.
    #[arg(long, default_value_t = 10)]
    pub top_failures: usize,

    /// Output format: `markdown` (default) or `html` (single-file, inline CSS, no JS).
    #[arg(long, default_value = "markdown")]
    pub format: String,
}

#[derive(Debug, Args)]
pub struct EvaluatorSelftestCmd {
    /// Path to the JSONL dataset whose `patch` fields are the gold patches.
    #[arg(long)]
    pub dataset_path: PathBuf,

    /// Directory where `evaluator_selftest.json` will be written.
    #[arg(long, default_value = "./selftest-out")]
    pub output: PathBuf,

    /// Dataset subset selector. Either a comma-separated id list
    /// (`id1,id2`) or `@path/to/file.txt` with one id per line.
    #[arg(long)]
    pub instance_ids: Option<String>,

    /// Keep at most N instances after filtering and sampling.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Reproducibly random-subset to N instances (requires `--seed`).
    #[arg(long)]
    pub sample: Option<usize>,

    /// RNG seed used by `--sample`.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Output format: `text` (default, headline + non-resolved table) or
    /// `json` (emits the JSON artifact only to stdout).
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Evaluation backend: `none` (zero-cost presence check, default) or
    /// `sb-cli` (routes gold patches through the same evaluator pipeline
    /// that `bench evaluate` uses on real sweeps).
    #[arg(long, default_value = "none")]
    pub backend: String,

    /// SWE-bench subset passed to `sb-cli submit` (e.g. `swe-bench-m`,
    /// `swe-bench_lite`). Ignored when `--backend none`.
    #[arg(long, default_value = "swe-bench-m")]
    pub sb_subset: String,

    /// SWE-bench split passed to `sb-cli submit` (e.g. `dev`, `test`).
    /// Ignored when `--backend none`.
    #[arg(long, default_value = "dev")]
    pub sb_split: String,

    /// Per-instance evaluation timeout in seconds for the `sb-cli` backend.
    #[arg(long, default_value_t = 600)]
    pub timeout_per_instance: u64,

    /// Parallel worker count for the `sb-cli` evaluation backend.
    #[arg(long, default_value_t = 4)]
    pub parallel: usize,
}
