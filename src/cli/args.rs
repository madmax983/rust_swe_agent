//! clap subcommand arg structs.

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

// ── Agent subcommands ─────────────────────────────────────────────────────────

/// Validated environment-type selector for `agent env preview`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EnvTypeArg {
    Local,
    Docker,
}

impl EnvTypeArg {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Docker => "docker",
        }
    }
}

/// Validated output-format selector for `agent env preview`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PreviewFormatArg {
    Text,
    Json,
}

/// `agent env preview` — print a structured preview of the agent environment.
#[derive(Debug, Args)]
pub struct EnvPreviewCmd {
    /// Environment type: `local` or `docker`.
    #[arg(long)]
    pub env: EnvTypeArg,

    /// Task description (used for context; passed through the redactor).
    #[arg(long)]
    pub task: String,

    /// Optional path to a TOML config file (overlays defaults).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: PreviewFormatArg,

    /// Show actual env var values instead of `[REDACTED:…]` markers.
    #[arg(long, default_value_t = false)]
    pub show_values: bool,
}

/// `agent env` subcommands.
#[derive(Debug, Subcommand)]
pub enum AgentEnvCmd {
    /// Preview environment configuration for a task without running anything.
    Preview(EnvPreviewCmd),
}

/// `agent` subcommands.
#[derive(Debug, Subcommand)]
pub enum AgentCmd {
    /// Inspect or preview agent environment settings.
    Env {
        #[command(subcommand)]
        cmd: AgentEnvCmd,
    },
    /// Preview which skills will activate for one or more tasks (zero-cost, no model call).
    SkillsPreview(SkillsPreviewCmd),
}

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

/// Confirmation-prompt UI selector for `mini --interactive` (issue #312).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UiKind {
    /// Single-line stderr prompt — the default. Works over any TTY.
    Stderr,
    /// Full-screen ratatui dashboard with a modal prompt and live
    /// trajectory feed.
    Ratatui,
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
    #[arg(long, default_value = "max")]
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
    #[arg(long, default_value = "max")]
    pub github_pr_branch_prefix: String,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct MiniCmd {
    /// The task prompt. Required unless `--resume` or `--task-file` is set; forbidden when `--resume` is set.
    #[arg(long)]
    pub task: Option<String>,

    /// Path to a file containing the task prompt, or '-' to read from stdin.
    #[arg(long)]
    pub task_file: Option<String>,

    /// Resume from a partial (in-progress) trajectory file instead of starting a new run.
    /// The trajectory is the sole source of truth for task, model, env, and budget settings.
    /// Mutually exclusive with `--task`, `--task-file`, `--render-only`, and `--trajectory-name`.
    #[arg(
        long = "resume",
        value_name = "PATH",
        conflicts_with_all = ["render_only", "trajectory_name"]
    )]
    pub resume_from: Option<PathBuf>,

    /// Allow raising `--step-limit`, `--task-timeout-secs`, or `--per-task-budget-usd` on a
    /// resume invocation when the original run hit one of those caps. Without this flag,
    /// supplying any of those flags on resume exits 2.
    #[arg(long, default_value_t = false, requires = "resume_from")]
    pub resume_allow_step_bump: bool,

    /// Additional context appended to the instance prompt.
    #[arg(long)]
    pub extra_context: Option<String>,

    /// Model name (e.g. `claude-opus-4-7`).
    #[arg(long, default_value = "claude-opus-4-7")]
    pub model: String,

    /// Max agent steps. Defaults to 50 when not set via flag or config.
    #[arg(long)]
    pub step_limit: Option<u32>,
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

    /// Run in analysis-only mode: disable mutation-capable surfaces.
    #[arg(long, default_value_t = false)]
    pub read_only: bool,

    /// Allow MCP tool registration in read-only mode.
    #[arg(long, default_value_t = false, requires = "read_only")]
    pub allow_mcp_in_read_only: bool,

    /// Optional path to a TOML config (overlays defaults).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Root directory for the local environment agent run.
    #[arg(long)]
    pub workdir: Option<PathBuf>,

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

    /// POST each `StreamEvent` to this HTTP URL as a JSON envelope.
    /// Optional; absent = no background webhook task spawned. Composable
    /// with `--stream` — both transports see the same event sequence.
    /// Requires the `webhook` Cargo feature (default-enabled).
    #[arg(long)]
    pub webhook_url: Option<String>,

    /// Inject an HTTP request header into every webhook POST.
    /// Format: `"Name: Value"`. Repeatable. Header bytes are not logged.
    /// Example: `--webhook-header "Authorization: Bearer $TOKEN"`
    #[arg(long = "webhook-header", value_name = "NAME: VALUE")]
    pub webhook_headers: Vec<String>,

    /// Append line-delimited JSON events to PATH for live tail/jq workflows.
    #[arg(long = "event-log", value_name = "PATH")]
    pub event_log: Option<PathBuf>,

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

    /// Render the initial prompt surface and exit without running the agent.
    /// Prints the system message, first user message, registered tools, hook
    /// config, token estimate, and upper-bound cost — at $0 and zero network
    /// calls. Mutually exclusive with --per-task-budget-usd and
    /// --task-timeout-secs.
    #[arg(long, default_value_t = false)]
    pub render_only: bool,

    /// Output format for --render-only: `text` (default, human-readable) or
    /// `json` (stable, schema-versioned, suitable for CI diffing).
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Issue #312 — pause before every bash/tool action and ask the
    /// operator to approve, reject, or abort. Mutually exclusive with
    /// `--render-only`. Requires a TTY unless `--yolo` is also set.
    #[arg(long, default_value_t = false, conflicts_with = "render_only")]
    pub interactive: bool,

    /// Run unattended but still print the interactive status line on
    /// each step boundary. Implied by `--interactive --yolo`; usable on
    /// its own when no prompts are wanted but the status line helps.
    #[arg(long, default_value_t = false)]
    pub yolo: bool,

    /// UI for the confirmation prompt: `stderr` (default, single-line)
    /// or `ratatui` (full-screen dashboard).
    #[arg(long, value_enum, default_value_t = UiKind::Stderr)]
    pub ui: UiKind,

    /// Disable per-step atomic trajectory checkpoints (issue #326 opt-out).
    /// By default, `mini` writes the trajectory after every completed agent
    /// step so a crash never destroys the full run budget. Pass this flag to
    /// skip those intermediate writes (useful for benchmarking or pathological
    /// trajectory-size cases where write overhead matters).
    #[arg(long, default_value_t = false)]
    pub no_step_persist: bool,
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

    /// Token budget for the model-visible prompt (same as the run-time flag).
    /// Required when replaying a trajectory produced with this bound so the
    /// elided prompts match and fingerprint comparison succeeds.
    #[arg(long)]
    pub history_max_input_tokens: Option<u64>,

    /// Keep only the last N observations in the model-visible prompt (same as
    /// the run-time flag). Required when replaying a trajectory produced with
    /// this bound so fingerprint comparison succeeds.
    #[arg(long)]
    pub history_keep_last_observations: Option<usize>,
}

#[derive(Debug, Subcommand)]
pub enum BenchCmd {
    /// Run a SWE-bench sweep over a local JSONL dataset.
    Swebench(Box<SwebenchCmd>),
    /// Walk the full sweep pipeline end-to-end at zero cost using gold-patch shadow submissions.
    Rehearsal(Box<SwebenchCmd>),
    /// Forecast sweep cost from a reproducible calibration slice.
    Forecast(Box<SwebenchCmd>),
    /// Compare a forecast artifact against completed sweep results.
    Calibrate(CalibrateCmd),
    /// Validate sweep inputs without launching tasks.
    Doctor(Box<SwebenchCmd>),
    /// Diff two completed sweep runs by instance id; surfaces regressions
    /// and (with `--max-regressions`) gates CI on prompt/harness changes.
    Compare(CompareCmd),
    /// Diff two completed sweep manifests to analyze configuration drift.
    DiffConfig(DiffConfigCmd),
    /// Evaluate a completed sweep with an evaluation backend (e.g. sb-cli).
    Evaluate(EvaluateCmd),
    /// Inspect a single trajectory or list filtered instance summaries.
    Inspect(InspectCmd),
    /// Tail live aggregate progress for a running sweep directory.
    Tail(TailCmd),
    /// Attach to a single in-flight instance and stream its turns live.
    Watch(WatchCmd),
    /// Cluster unresolved sweep failures into ranked actionable groups.
    Triage(TriageCmd),
    /// Diff failure-cluster composition between two sweeps.
    TriageDiff(TriageDiffCmd),
    /// Aggregate shell-command frequency and cost by outcome bucket.
    CommandStats(CommandStatsCmd),
    /// Search every trajectory in a sweep for a regex pattern (zero-cost: reads only on-disk artifacts).
    Grep(GrepCmd),
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
    /// Re-run only the failed instances of a completed sweep and merge results.
    Retry(RetryCmd),
    /// Surface agent action-class mix (read/write/test/…) by outcome bucket.
    Behavior(BehaviorCmd),
    /// Measure MCP tool usage and correlate with outcome across a sweep.
    ToolCoverage(ToolCoverageCmd),
    /// Measure sweep policy impact on outcomes.
    PolicyImpact(PolicyImpactCmd),
    /// Systematic per-tool removal ablation: baseline plus one arm per removed tool.
    ToolAblation(ToolAblationCmd),
    /// Join historical sweeps on instance_id and report resolution history,
    /// stability class, and flip provenance.
    InstanceHistory(InstanceHistoryCmd),
    /// Surface prompt-cache hit rate, savings, and spend for a completed sweep.
    CacheStats(CacheStatsCmd),
    /// Right-size step, cost, and wallclock caps from a completed sweep's distributions.
    BudgetFit(BudgetFitCmd),
    /// Resolved-rate and cost trend across sweeps in a root directory.
    Ladder(LadderCmd),
    /// Run sequential model tiers per instance; short-circuit on first resolved tier.
    Cascade(CascadeCmd),
    /// Compute per-test partial-credit scores across a completed sweep.
    TestProgress(TestProgressCmd),
    /// Fork a trajectory at step N and continue with config overrides.
    Fork(ForkCmd),
    /// Calculate statistical power or required sample size.
    Power(PowerCmd),
    /// Preview SWE-bench dataset composition offline and zero-cost before running a sweep.
    DatasetStats(DatasetStatsCmd),
    /// Identify the commit that introduced a resolved-rate regression.
    Bisect(BisectCmd),
    /// Re-derive and verify sweep aggregates against trajectories.
    Audit(AuditCmd),
    /// Emit a self-contained failure summary for one instance in a completed sweep.
    FailureDigest(FailureDigestCmd),
}

/// `bench failure-digest` — self-contained failure summary for one sweep instance.
#[derive(Debug, Args, Clone)]
pub struct FailureDigestCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Target instance ID. When omitted and the sweep contains exactly one
    /// instance, that instance is used. When the sweep contains more than one
    /// instance and this flag is omitted, the command exits non-zero and names
    /// all candidates.
    #[arg(long)]
    pub instance: Option<String>,

    /// Output format: `markdown` (default) or `json`.
    #[arg(long, default_value = "markdown", value_parser = ["markdown", "json"])]
    pub format: String,

    /// Maximum characters for the markdown output (default: 8000).
    /// Truncation preserves the headline and triage cluster footer.
    #[arg(long, default_value_t = 8000)]
    pub max_chars: usize,
}

/// `bench dataset-stats` — preview SWE-bench dataset composition pre-sweep.
#[derive(Debug, Args, Clone)]
pub struct DatasetStatsCmd {
    /// Local JSONL dataset file. Mutually exclusive with `--dataset`.
    #[arg(long)]
    pub dataset_path: Option<PathBuf>,

    /// Named SWE-bench dataset alias: `full`, `lite`, or `verified`.
    #[arg(long, value_name = "ALIAS")]
    pub dataset: Option<String>,

    /// Dataset split for named aliases: `train`, `test`, or `dev`.
    #[arg(long, default_value = "test", value_name = "SPLIT")]
    pub split: Option<String>,

    /// Directory for the named-dataset on-disk cache.
    #[arg(long, value_name = "DIR")]
    pub dataset_cache_dir: Option<PathBuf>,

    /// Dataset subset selector: comma-separated id list or `@path/to/file.txt`.
    #[arg(long)]
    pub instance_ids: Option<String>,

    /// Keep at most N instances after subsetting.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Reproducibly random-subset to N instances.
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

    /// Runs root directory override for scanning historical sweeps.
    #[arg(long, default_value = "./runs")]
    pub runs_dir: PathBuf,

    /// Format: `text` or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Model name to use for TokenCounter offline estimation.
    #[arg(long, default_value = "gpt-4")]
    pub model: String,
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

/// `bench test-progress` — per-test partial-credit scoring across a sweep.
///
/// Reads existing evaluator output (`evaluation.json`) and dataset JSONL
/// (`dataset.jsonl`) to compute `partial_credit_score`, verdict buckets, and
/// hot-failing-test aggregations. Never re-runs instances or calls a model.
#[derive(Debug, Args)]
pub struct TestProgressCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Restrict the per-instance text table to one verdict bucket.
    /// Valid values: resolved, partial_progress, no_progress, regressed,
    /// evaluator_unavailable.
    #[arg(long)]
    pub bucket: Option<String>,

    /// Number of entries in `hot_failing_tests` and `hot_regressed_tests`
    /// (default: 20).
    #[arg(long, default_value_t = 20)]
    pub hot_tests_n: usize,

    /// Filter instances using the same syntax as `bench inspect --filter`.
    /// Example: `resolved=true`.
    #[arg(long)]
    pub filter: Option<String>,

    /// Exclude instances with fewer than N total tests from sweep-wide means.
    /// Excluded instances still appear in `per_instance` with
    /// `excluded_from_means: true`. Default: 0 (no exclusion).
    #[arg(long, default_value_t = 0)]
    pub min_tests: usize,
}

/// `bench cascade` — cost-optimized model-tier routing per instance.
#[derive(Debug, Args)]
pub struct CascadeCmd {
    /// Path to the TOML cascade manifest with `[[tier]]` entries.
    #[arg(long)]
    pub config: std::path::PathBuf,

    /// Local JSONL dataset file. Mutually exclusive with `--dataset`.
    #[arg(long)]
    pub dataset_path: Option<std::path::PathBuf>,

    /// Named SWE-bench dataset alias (e.g. `verified`). Alternative to `--dataset-path`.
    #[arg(long, value_name = "ALIAS")]
    pub dataset: Option<String>,

    /// Dataset split for named aliases: `train`, `test`, or `dev`.
    #[arg(long, default_value = "test")]
    pub split: Option<String>,

    /// Directory for the named-dataset on-disk cache.
    #[arg(long)]
    pub dataset_cache_dir: Option<std::path::PathBuf>,

    /// Root output directory (tier results land in `{output}/tier-{name}/`).
    #[arg(long)]
    pub output: std::path::PathBuf,

    /// Shared USD ceiling across all tiers.
    #[arg(long)]
    pub sweep_cost_limit_usd: Option<f64>,

    /// Resume from a previous run: resolved and fully-exhausted instances are
    /// skipped; partially-cascaded instances resume on the next unattempted tier.
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

    /// Worker parallelism per tier sweep (parallelism is across instances, not tiers).
    #[arg(long, default_value_t = crate::run::swebench::DEFAULT_PARALLEL)]
    pub parallel: usize,

    /// Evaluation backend for per-tier resolved-rate gating.
    /// Required: cascade will not start without a configured backend.
    #[arg(long, default_value = "sb-cli", value_name = "BACKEND")]
    pub eval_backend: String,

    /// SWE-bench subset for the evaluator (e.g. `swe-bench-m`, `swe-bench_lite`).
    #[arg(long, default_value = "swe-bench-m")]
    pub sb_subset: String,

    /// SWE-bench split for the evaluator (e.g. `test`, `dev`).
    #[arg(long, default_value = "test")]
    pub sb_split: String,

    /// Skip startup preflight checks before launching tier sweeps.
    #[arg(long, default_value_t = false)]
    pub skip_preflight: bool,

    /// Skip model-endpoint probe during preflight.
    #[arg(long, default_value_t = false)]
    pub skip_model_probe: bool,

    /// Per-instance timeout in seconds passed to the evaluation backend.
    #[arg(long, default_value_t = 300)]
    pub eval_timeout_per_instance_secs: u64,

    /// Seconds each tier sweep waits for in-flight tasks after a cancel signal.
    #[arg(long, default_value_t = 60)]
    pub cancel_deadline_secs: u64,
}

/// `bench cache-stats` — surface prompt-cache hit rate per sweep (zero-cost: reads only on-disk artifacts).
#[derive(Debug, Args)]
pub struct CacheStatsCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: std::path::PathBuf,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text", value_name = "FMT")]
    pub format: String,

    /// Number of per-instance rows to display (worst cache efficiency first).
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub top: usize,

    /// Baseline sweep directory. When supplied, prints Δ hit_rate and Δ realized_spend_usd.
    #[arg(long, value_name = "DIR")]
    pub baseline: Option<std::path::PathBuf>,
}

/// `bench budget-fit` — right-size step, cost, and wallclock caps (zero-cost: reads only on-disk artifacts).
#[derive(Debug, Args)]
pub struct BudgetFitCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: std::path::PathBuf,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text", value_name = "FMT")]
    pub format: String,

    /// Restrict output to a single axis: `steps`, `cost_usd`, or `wall_clock_s`.
    #[arg(long, value_name = "AXIS")]
    pub axis: Option<String>,

    /// Fraction of configured cap within which an instance counts as "at-cap".
    /// Range: 0.0–0.5. Default: 0.05 (5%).
    #[arg(long, default_value = "0.05", value_name = "FRAC")]
    pub at_cap_tolerance: f64,

    /// Percentile of the resolved distribution used to compute `recommended_cap`.
    /// Range: 50–99. Default: 95.
    #[arg(long, default_value = "95", value_name = "PCT")]
    pub target_percentile: u8,

    /// Key=value filter applied before analysis (same syntax as `bench inspect --filter`).
    /// May be specified multiple times.
    #[arg(long = "filter", value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    pub filter: Vec<String>,
}

/// `bench ladder` — resolved-rate and cost trend across sweeps (zero-cost: reads only on-disk artifacts).
#[derive(Debug, Args)]
pub struct LadderCmd {
    /// Root directory containing sweep subdirectories to scan.
    #[arg(long, value_name = "DIR")]
    pub root: std::path::PathBuf,

    /// Filter to sweeps whose recorded dataset matches this alias or normalized path.
    #[arg(long, value_name = "ALIAS")]
    pub dataset: Option<String>,

    /// Truncate to the N most recent matching sweeps after sorting.
    #[arg(long, value_name = "N")]
    pub last: Option<usize>,

    /// Sweep directory name to use as baseline; adds a Δ vs baseline column.
    #[arg(long, value_name = "SWEEP_ID")]
    pub baseline: Option<String>,

    /// Output format: `text` (default), `json`, or `markdown`.
    #[arg(long, default_value = "text", value_name = "FMT")]
    pub format: String,
}

/// `bench instance-history` — longitudinal view of instance resolution across sweeps.
#[derive(Debug, Args)]
pub struct InstanceHistoryCmd {
    /// Paths to sweep directories to join. Must be given at least twice.
    #[arg(long = "sweeps", value_name = "DIR", required = true, action = clap::ArgAction::Append)]
    pub sweeps: Vec<std::path::PathBuf>,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text", value_name = "FMT")]
    pub format: String,

    /// Output file path. Use `-` to write JSON to stdout.
    #[arg(long, default_value = "instance-history.json", value_name = "PATH")]
    pub output: std::path::PathBuf,

    /// Resolved-rate threshold for stable classification (default 1.0: only
    /// 0/N or N/N are stable; lower values subdivide the middle band).
    #[arg(long, default_value = "1.0", value_name = "THRESHOLD")]
    pub stable_threshold: f64,

    /// Exit non-zero when the partial-coverage fraction exceeds
    /// `--max-partial-share` or when the intersection is empty.
    #[arg(long)]
    pub require_full_coverage: bool,

    /// Maximum fraction (0.0–1.0) of instances allowed to have partial
    /// coverage, used with `--require-full-coverage`.
    #[arg(long, value_name = "SHARE")]
    pub max_partial_share: Option<f64>,

    /// Restrict the text table to one stability class
    /// (stable_win | stable_loss | flipper | unstable_minority_win | unstable_minority_loss).
    #[arg(long, value_name = "CLASS")]
    pub class: Option<String>,

    /// Maximum rows to print in the text table.
    #[arg(long, default_value = "50", value_name = "N")]
    pub top: usize,

    /// Print only the flipper subset ("operator quick-glance" mode).
    #[arg(long)]
    pub focus: bool,
}

/// `bench retry` — re-run selected failed instances and merge into sweep dir.
#[derive(Debug, Args)]
pub struct RetryCmd {
    /// Path to the completed sweep directory (must contain `results.json`).
    #[arg(long)]
    pub sweep: PathBuf,

    // ── selection flags ───────────────────────────────────────────────────────
    /// Comma-separated `FailureCategory` labels to retry
    /// (e.g. `step_limit,patch_apply_invalid`).
    /// At least one of `--failure-category`, `--outcome`, or `--instance-ids`
    /// is required.
    #[arg(long = "failure-category", value_name = "CATS")]
    pub failure_category: Option<String>,

    /// Comma-separated outcome values to retry
    /// (`errored`, `step_limit_reached`, `submitted`).
    #[arg(long, value_name = "OUTCOMES")]
    pub outcome: Option<String>,

    /// Comma-separated instance ids to retry (intersected with other filters).
    #[arg(long, value_name = "IDS")]
    pub instance_ids: Option<String>,

    /// Cap the selection at N instances after all other filters.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Allow re-running instances that were already `submitted` (extra cost).
    #[arg(long, default_value_t = false)]
    pub allow_resolved_retry: bool,

    // ── gate flags ────────────────────────────────────────────────────────────
    /// Skip the harness git-SHA mismatch check.
    #[arg(long, default_value_t = false)]
    pub allow_harness_mismatch: bool,

    /// Proceed without interactive confirmation (skip dry-run exit).
    #[arg(long, default_value_t = false)]
    pub yes: bool,

    // ── override flags ────────────────────────────────────────────────────────
    /// Override the model name for the retry run.
    #[arg(long)]
    pub model: Option<String>,

    /// Path to a config TOML overlay file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Override the step limit for the retry run.
    #[arg(long)]
    pub step_limit: Option<u32>,

    /// Override the per-task wallclock timeout (seconds).
    #[arg(long)]
    pub task_timeout_secs: Option<u64>,

    /// Override the per-task USD budget ceiling.
    #[arg(long)]
    pub per_task_budget_usd: Option<f64>,

    /// Override the sweep-level USD cost ceiling.
    #[arg(long)]
    pub sweep_cost_limit_usd: Option<f64>,

    /// Override the environment: `local` or `docker`.
    #[arg(long)]
    pub env: Option<String>,

    /// Override the docker image (requires `--env docker`).
    #[arg(long)]
    pub docker_image: Option<String>,

    /// Number of parallel worker slots for the retry run.
    #[arg(long)]
    pub parallel: Option<usize>,

    /// Path to the dataset JSONL file (required unless manifest path is accessible).
    #[arg(long)]
    pub dataset_path: Option<PathBuf>,

    /// Named dataset alias (alternative to `--dataset-path`).
    #[arg(long, value_name = "ALIAS")]
    pub dataset: Option<String>,
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

    /// Resume from a previous run: `complete` trajectories are skipped, `partial`
    /// (mid-run checkpoint) trajectories continue from their last completed turn
    /// without re-spending budget, and `absent`/`corrupted` trajectories run from
    /// step 0.
    #[arg(long, default_value_t = false)]
    pub resume: bool,

    /// Append line-delimited JSON events to PATH for live tail/jq workflows.
    #[arg(long = "event-log", value_name = "PATH")]
    pub event_log: Option<PathBuf>,

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

    /// Treat per-call sampling drift (between source and replay trajectories)
    /// as a hard divergence that aborts with a non-zero exit.
    /// By default sampling drift is a soft divergence that only warns.
    #[arg(long, default_value_t = false)]
    pub strict_sampling: bool,
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

    /// Exit non-zero when the resolved-rate delta is positive but p > alpha
    /// (suspected-noise wins fail CI gates). Unset disables this gate.
    /// Example: --min-significance 0.05
    #[arg(long, value_name = "ALPHA")]
    pub min_significance: Option<f64>,

    /// Exit non-zero when the resolved-rate delta is negative and p <= alpha
    /// (significant regressions are blocked). Unset disables this gate;
    /// insignificant regressions are not blocked by this flag.
    /// Example: --regression-significance 0.05
    #[arg(long, value_name = "ALPHA")]
    pub regression_significance: Option<f64>,

    /// When set, allow significance-based gating to proceed even when the
    /// paired test is underpowered. Without this flag, any underpowered result
    /// causes gating flags to exit non-zero.
    #[arg(long, default_value_t = false)]
    pub allow_underpowered: bool,

    /// Exit non-zero when candidate test-only resolved rate exceeds this threshold.
    /// Range: 0.0–1.0. Unset is informational only.
    #[arg(long = "max-test-only-resolved-rate", value_name = "RATE")]
    pub max_test_only_resolved_rate: Option<f64>,
}

#[derive(Debug, Clone, Args)]
pub struct DiffConfigCmd {
    /// Sweep output directory written by a prior `bench swebench` run.
    /// Treated as the baseline/before side of the diff.
    #[arg(long)]
    pub baseline: std::path::PathBuf,

    /// Sweep output directory to compare against the baseline.
    /// Treated as the candidate/after side of the diff.
    #[arg(long)]
    pub candidate: std::path::PathBuf,

    /// Output format: `text` (default) or `json` (machine-readable report).
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Exit non-zero (exit code 3) when any non-ignored difference is found.
    #[arg(long = "fail-on-change")]
    pub fail_on_change: bool,

    /// Optional comma-separated list of paths or groups to ignore in changed_fields.
    #[arg(long)]
    pub ignore: Option<String>,
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
    /// Defaults to `~/.cache/max/datasets`.
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

    /// Resume from a previous run: `complete` trajectories are skipped, `partial`
    /// (mid-run checkpoint) trajectories continue from their last completed turn
    /// without re-spending budget, and `absent`/`corrupted` trajectories run from
    /// step 0.
    #[arg(long, default_value_t = false)]
    pub resume: bool,

    /// Append line-delimited JSON events to PATH for live tail/jq workflows.
    #[arg(long = "event-log", value_name = "PATH")]
    pub event_log: Option<PathBuf>,

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

    /// Render the initial prompt surface for the first selected instance and
    /// exit without launching any tasks. Requires a dataset source. Use
    /// `--instance-ids` or `--limit 1` to choose a specific row.
    #[arg(long, default_value_t = false)]
    pub render_only: bool,

    /// OTLP/HTTP base URL for trace export (e.g. `http://localhost:4318`).
    /// When set, the sweep streams spans via OTLP/HTTP for every instance,
    /// model call, and tool invocation.  The telemetry layer also checks
    /// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (exact traces URL, highest
    /// priority) and `OTEL_EXPORTER_OTLP_ENDPOINT` (base URL, lower priority)
    /// so that standard OTel env vars work without this flag.
    /// When all are unset, OTLP export is disabled and no sockets are opened.
    #[arg(long, value_name = "URL")]
    pub otlp_endpoint: Option<String>,

    /// Run the zero-cost rehearsal pipeline instead of a real sweep.
    #[arg(long, default_value_t = false)]
    pub rehearse: bool,

    /// Skip running the evaluator stage during rehearsal.
    #[arg(long, default_value_t = false)]
    pub skip_evaluator: bool,

    /// Evaluator backend to use during rehearsal: sb-cli, none, or rehearsal.
    #[arg(long, default_value = "rehearsal")]
    pub eval_backend: String,

    /// SWE-bench subset name required by some evaluator backends (e.g. sb-cli).
    #[arg(long)]
    pub sb_subset: Option<String>,

    /// SWE-bench split name required by some evaluator backends.
    #[arg(long)]
    pub sb_split: Option<String>,

    /// Timeout in seconds for the evaluation stage.
    #[arg(long)]
    pub eval_timeout_secs: Option<u64>,

    /// Compare the current rehearsal artifacts against a saved one and surface drift.
    #[arg(long, value_name = "PATH")]
    pub diff: Option<PathBuf>,
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
    /// In instance mode, also accepts `markdown`, `html`, `csv`, and `mermaid`
    /// (each maps to the corresponding trajectory exporter; feature-gated
    /// formats require the matching Cargo feature at build time).
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Write output to a file instead of stdout. Supported with `markdown`,
    /// `html`, `csv`, and `mermaid` formats in instance mode.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Disable stdout/stderr truncation in transcript mode.
    #[arg(long, default_value_t = false)]
    pub full: bool,

    /// Also print PASS_TO_PASS / FAIL_TO_PASS expected-test groupings from the
    /// sweep's dataset.jsonl (requires dataset.jsonl in the sweep directory).
    #[arg(long, default_value_t = false)]
    pub show_expected: bool,
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

/// `bench watch` — attach to a single in-flight instance and stream turns live.
#[derive(Debug, Args)]
pub struct WatchCmd {
    /// Sweep output directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Instance id to watch.
    #[arg(long)]
    pub instance: String,

    /// Run slot to follow (default 1). Use 2, 3, … for `--reruns` sweeps.
    #[arg(long, default_value_t = 1)]
    pub run_index: u32,

    /// Seconds to wait for the trajectory file to appear (0 = fail immediately if not found).
    #[arg(long, default_value_t = 30)]
    pub wait_secs: u64,

    /// Seconds of no new turns before printing a stall warning (keeps following).
    #[arg(long, default_value_t = 120)]
    pub stall_secs: u64,

    /// Disable stdout/stderr truncation.
    #[arg(long, default_value_t = false)]
    pub full: bool,

    /// Truncation threshold in bytes (default 4096). Ignored when `--full` is set.
    #[arg(long)]
    pub max_bytes: Option<usize>,

    /// Emit one newline-delimited JSON object per turn instead of human-readable text.
    #[arg(long, default_value_t = false)]
    pub ndjson: bool,
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

#[derive(Debug, Args, Clone)]
pub struct TriageDiffCmd {
    /// Baseline sweep directory produced by `bench swebench` and scored by `bench evaluate`.
    #[arg(long)]
    pub baseline: PathBuf,

    /// Candidate sweep directory produced by `bench swebench` and scored by `bench evaluate`.
    #[arg(long)]
    pub candidate: PathBuf,

    /// Run `bench triage` on baseline and/or candidate sweeps if triage.json is missing.
    #[arg(long = "auto-triage", default_value_t = false)]
    pub auto_triage: bool,

    /// Filter delta output to clusters with count >= k on either baseline or candidate.
    #[arg(long, default_value_t = 1)]
    pub min_cluster_size: usize,

    /// Number of ranked clusters to print in text mode.
    #[arg(long, default_value_t = 10)]
    pub top: usize,

    /// Output path for triage-diff.json. Defaults to `triage-diff.json` in candidate sweep dir.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Fail with exit code 1 if the regression set is non-empty.
    #[arg(long = "fail-on-regression", default_value_t = false)]
    pub fail_on_regression: bool,
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

/// `bench grep` — search every trajectory in a sweep for a regex.
///
/// Reads only on-disk artifacts and never calls a model provider (zero-cost guarantee).
/// Exit codes: 0 = at least one match found; 1 = no matches; 2 = usage/config error.
#[derive(Debug, Args)]
pub struct GrepCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Regex pattern to search across trajectory messages.
    pub pattern: String,

    /// Restrict search to specific message roles (repeatable; default: all roles).
    /// Example: `--role assistant --role user`
    #[arg(long = "role", value_name = "ROLE")]
    pub roles: Vec<String>,

    /// Restrict search to a specific field within each message.
    /// `content` (default) searches the message text; `actions` searches bash commands.
    #[arg(long, default_value = "content")]
    pub field: String,

    /// Comma-separated instance IDs to include (mirrors `bench inspect --filter`).
    #[arg(long)]
    pub instance_ids: Option<String>,

    /// Comma-separated instance IDs to exclude.
    #[arg(long)]
    pub exclude_instance_ids: Option<String>,

    /// Filter to instances with one of the specified outcomes (repeatable).
    /// Example: `--outcome error --outcome submitted`
    #[arg(long = "outcome", value_name = "OUTCOME")]
    pub outcomes: Vec<String>,

    /// Characters of context before and after each match in the printed snippet (default: 80).
    #[arg(long, default_value_t = 80)]
    pub context: usize,

    /// Cap per-instance match count to prevent flooding stdout on a broad pattern.
    #[arg(long)]
    pub max_matches_per_instance: Option<usize>,

    /// Output format: `text` (default, tab-separated `instance_id\tturn_index\trole\tsnippet`)
    /// or `json` (one JSON object per match, newline-delimited).
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

/// `bench tool-ablation` — systematic per-tool removal ablation experiment.
#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolAblationCmd {
    /// Path to the base config TOML. Tool list is read from `agent.tools`.
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

    /// Root output directory. Arm results land in `{output}/{arm_name}/`.
    #[arg(long)]
    pub output: PathBuf,

    /// Restrict ablation to this tool name (repeatable). Default: all user tools.
    #[arg(long = "ablate", value_name = "TOOL", action = clap::ArgAction::Append)]
    pub ablate: Vec<String>,

    /// Print the planned arm manifest and exit without running any sweeps.
    #[arg(long, default_value_t = false)]
    pub render_only: bool,

    /// Output format for `--render-only`: `text` (default) or `json`.
    #[arg(long, default_value = "text", value_name = "FMT")]
    pub format: String,

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

    /// Worker parallelism per arm sweep.
    #[arg(long, default_value_t = crate::run::swebench::DEFAULT_PARALLEL)]
    pub parallel: usize,

    /// Add one arm per *pair* of removed tools (O(N²) cost — opt-in).
    /// The CLI prints the expected arm count and projected cost ceiling before starting.
    #[arg(long, default_value_t = false)]
    pub include_pair_ablation: bool,

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

/// `bench tool-coverage` — measure MCP tool usage by outcome bucket.
#[derive(Debug, Args)]
pub struct ToolCoverageCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Restrict output to one outcome bucket: `resolved`, `unresolved`,
    /// `errored`, or `all`.
    #[arg(long)]
    pub bucket: Option<String>,

    /// Filter instances using the same syntax as `bench inspect --filter`.
    /// Example: `failure_category=model_parse`.
    #[arg(long)]
    pub filter: Option<String>,

    /// Hide tools with fewer than N total invocations from the text table.
    /// The JSON artifact always contains all tools so dead-tool surfacing is intact.
    #[arg(long, default_value_t = 0)]
    pub min_invocations: usize,

    /// Emit per-instance tool call counts in the JSON output and artifact.
    #[arg(long, default_value_t = false)]
    pub per_instance: bool,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,
}

/// `bench policy-impact` — measure sweep policy impact on outcomes.
#[derive(Debug, Args)]
pub struct PolicyImpactCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long, short)]
    pub sweep: PathBuf,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,
}

/// `bench behavior` — surface agent action-class mix by outcome bucket.
#[derive(Debug, Args)]
pub struct BehaviorCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Restrict output to one outcome bucket: `resolved`, `unresolved`,
    /// `errored`, or `all`.
    #[arg(long)]
    pub bucket: Option<String>,

    /// Hide action classes whose `all`-bucket share is below this threshold
    /// in text output (0.0–1.0).
    #[arg(long)]
    pub min_share: Option<f64>,

    /// Filter instances using the same syntax as `bench inspect --filter`.
    /// Example: `failure_category=model_parse`.
    #[arg(long)]
    pub filter: Option<String>,

    /// Emit per-instance class counts in the JSON output and artifact.
    #[arg(long, default_value_t = false)]
    pub per_instance: bool,

    /// Output format: `text` (default) or `json`.
    #[arg(long, default_value = "text")]
    pub format: String,
}

/// `agent skills-preview` — static enumeration of skill activation (issue #337).
///
/// Exits 0 on a clean preview, 2 on bad flags, 14 when at least one
/// warning condition is detected (cap hit, missing version, or auto_match).
#[derive(Debug, Args)]
pub struct SkillsPreviewCmd {
    /// Task string to preview. May be repeated for multiple tasks.
    #[arg(long = "task", value_name = "TASK", action = clap::ArgAction::Append)]
    pub tasks: Vec<String>,

    /// File with one task per line; `#`-prefixed lines are ignored.
    #[arg(long, value_name = "FILE")]
    pub task_file: Option<std::path::PathBuf>,

    /// Optional path to a TOML config (overlays defaults).
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,

    /// Output format: `text` (default, human-readable) or `json` (schema-versioned).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_type_arg_as_str_local() {
        assert_eq!(EnvTypeArg::Local.as_str(), "local");
    }

    #[test]
    fn env_type_arg_as_str_docker() {
        assert_eq!(EnvTypeArg::Docker.as_str(), "docker");
    }
}
