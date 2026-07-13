//! Command-line interface. `clap` derive; subcommand dispatch.
// The dispatch functions in this module call large async subsystems.  The
// Box::pin calls on the hot paths heap-allocate the inner futures, but the
// outer dispatch state machines can still cross the 16 KiB threshold on some
// compiler builds.  The lint is informational here; the allocation behaviour
// is already correct.
#![allow(clippy::large_futures)]


use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::error::Error;
use crate::exit_code::ExitCode;

pub mod agent;
pub mod args;
pub mod bench;
pub mod catalog;
pub mod explain;
pub mod mini;

#[derive(Debug, Parser)]
#[command(
    name = "max",
    version,
    about = "Maxwell's Daemon: measure-first SWE agent harness"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Global log level.
    #[arg(long, env = "MAXWELL_LOG")]
    pub log: Option<String>,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Run one task end-to-end and write a trajectory.
    Mini(Box<args::MiniCmd>),
    /// Smoke-test: scripted model + local env writes a trajectory.
    HelloWorld(args::HelloWorldCmd),
    /// Replay an existing trajectory using a deterministic model.
    Replay(Box<args::ReplayCmd>),
    /// SWE-bench parallel sweep.
    Bench {
        #[command(subcommand)]
        cmd: Box<args::BenchCmd>,
    },
    /// Agent inspection and preview utilities.
    Agent {
        #[command(subcommand)]
        cmd: Box<args::AgentCmd>,
    },
    /// Serve a read-only local sweep browser (requires the `ui-server` feature).
    Ui(args::UiCmd),
    /// Self-describing command catalog for operator discoverability.
    Catalog(args::CatalogCmd),
    /// Explain an exit code, outcome class, or failure category offline.
    Explain(args::ExplainCmd),
    /// Generate shell completion scripts.
    Completions(args::CompletionsCmd),
    /// Reap leftover Maxwell's Daemon containers, including legacy labels.
    Cleanup,
}

#[allow(clippy::too_many_lines)]
pub async fn run() -> Result<(), Error> {
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        // Print clap's formatted error or help text, then add the outcome label
        // for non-zero exits (exit 0 means --help / --version, not an error).
        let _ = e.print();
        if e.exit_code() != 0 {
            eprintln!("outcome_class: {}", ExitCode::UsageError.outcome_class());
            eprintln!("error: {e}");
        }
        std::process::exit(e.exit_code());
    });
    let log = effective_log_level(cli.log.as_deref());
    init_logging(&log);

    match cli.command {
        Command::Mini(m) => Box::pin(mini::mini_cmd(*m)).await,
        Command::HelloWorld(h) => {
            Box::pin(crate::run::hello_world::main(h.output, h.config.as_deref())).await
        }
        Command::Replay(r) => Box::pin(replay_cmd(*r)).await,
        Command::Bench { cmd } => match *cmd {
            args::BenchCmd::Swebench(s) => Box::pin(bench::bench_swebench(*s)).await,
            args::BenchCmd::Rehearsal(mut s) => {
                s.rehearse = true;
                Box::pin(bench::bench_swebench(*s)).await
            }
            args::BenchCmd::Forecast(s) => Box::pin(bench::bench_forecast(*s)).await,
            args::BenchCmd::Calibrate(c) => bench::bench_calibrate(c),
            args::BenchCmd::Doctor(s) => Box::pin(bench::bench_doctor(*s)).await,
            args::BenchCmd::Compare(c) => bench::bench_compare(c),
            args::BenchCmd::DiffConfig(c) => bench::bench_diff_config(c),
            args::BenchCmd::Evaluate(e) => bench::bench_evaluate(e),
            args::BenchCmd::Inspect(i) => bench::bench_inspect(i),
            args::BenchCmd::Tail(t) => bench::bench_tail(t).await,
            args::BenchCmd::Watch(w) => bench::bench_watch(w).await,
            args::BenchCmd::Triage(t) => bench::bench_triage(t),
            args::BenchCmd::TriageDiff(t) => bench::bench_triage_diff(t),
            args::BenchCmd::CommandStats(c) => bench::bench_command_stats(c),
            args::BenchCmd::Grep(g) => bench::bench_grep(g),
            args::BenchCmd::Events(e) => bench::bench_events(e),
            args::BenchCmd::Frontier(f) => bench::bench_frontier(f),
            args::BenchCmd::Reproduce(r) => Box::pin(bench::bench_reproduce(r)).await,
            args::BenchCmd::Bundle(b) => bench::bench_bundle(b),
            args::BenchCmd::Matrix(m) => Box::pin(bench::bench_matrix(m)).await,
            args::BenchCmd::EvaluatorSelftest(s) => bench::bench_evaluator_selftest(s),
            args::BenchCmd::Report(r) => bench::bench_report(r),
            args::BenchCmd::Retry(r) => Box::pin(bench::bench_retry(r)).await,
            args::BenchCmd::Behavior(b) => bench::bench_behavior(b),
            args::BenchCmd::ToolCoverage(t) => bench::bench_tool_coverage(t),
            args::BenchCmd::SkillCoverage(t) => bench::bench_skill_coverage(t),
            args::BenchCmd::PolicyImpact(p) => bench::bench_policy_impact(p),
            args::BenchCmd::InstanceHistory(h) => bench::bench_instance_history(h),
            args::BenchCmd::CacheStats(c) => bench::bench_cache_stats(c),
            args::BenchCmd::ContextPressure(c) => bench::bench_context_pressure(c),
            args::BenchCmd::BudgetFit(b) => bench::bench_budget_fit(b),
            args::BenchCmd::ToolAblation(t) => Box::pin(bench::bench_tool_ablation(t)).await,
            args::BenchCmd::Ladder(l) => bench::bench_ladder(l),
            args::BenchCmd::Cascade(c) => Box::pin(bench::bench_cascade(c)).await,
            args::BenchCmd::TestProgress(t) => bench::bench_test_progress(t),
            args::BenchCmd::Fork(f) => Box::pin(crate::run::fork::run(f)).await,
            args::BenchCmd::Power(p) => bench::bench_power(&p),
            args::BenchCmd::DatasetStats(s) => bench::bench_dataset_stats(s),
            args::BenchCmd::DatasetVerify(s) => bench::bench_dataset_verify(s),
            args::BenchCmd::Bisect(b) => Box::pin(bench::bench_bisect(b)).await,
            args::BenchCmd::Audit(a) => bench::bench_audit(a),
            args::BenchCmd::FailureDigest(f) => bench::bench_failure_digest(f),
            args::BenchCmd::EvalFlake(f) => bench::bench_eval_flake(f),
            args::BenchCmd::Annotate(a) => bench::bench_annotate(a),
            args::BenchCmd::StagnationReport(s) => bench::bench_stagnation_report(s),
            args::BenchCmd::SelfCheck(s) => bench::bench_self_check(s),
            args::BenchCmd::Import(i) => bench::bench_import(i),
            args::BenchCmd::ExportCi(c) => bench::bench_export_ci(c),
            args::BenchCmd::ContaminationCheck(c) => bench::bench_contamination_check(c),
            args::BenchCmd::ScriptabilityCheck(s) => {
                Box::pin(bench::bench_scriptability_check(s)).await
            }
            args::BenchCmd::NearMiss(n) => bench::bench_near_miss(n),
            args::BenchCmd::Assert(a) => bench::bench_assert(a),
            args::BenchCmd::Subset(s) => bench::bench_subset(s),
            args::BenchCmd::EvalParity(p) => bench::bench_eval_parity(p),
            args::BenchCmd::Utilization(u) => bench::bench_utilization(u),
            args::BenchCmd::ExportOtlp(c) => Box::pin(bench::bench_export_otlp(c)).await,
            args::BenchCmd::Variance(v) => bench::bench_variance(v),
            args::BenchCmd::Merge(m) => bench::bench_merge(&m),
            args::BenchCmd::Shard(s) => bench::bench_shard(s),
            args::BenchCmd::Ledger(l) => bench::bench_ledger(l),
            args::BenchCmd::Du(d) => bench::bench_du(d),
        },
        Command::Agent { cmd } => match *cmd {
            args::AgentCmd::SkillsPreview(s) => agent::agent_skills_preview_cmd(&s),
            args::AgentCmd::RedactCheck(r) => agent::agent_redact_check_cmd(&r),
            args::AgentCmd::RedactAudit(a) => agent::agent_redact_audit_cmd(&a),
            args::AgentCmd::InjectionAudit(a) => agent::agent_injection_audit_cmd(&a),
            args::AgentCmd::Env {
                cmd: args::AgentEnvCmd::Preview(ref p),
            } => agent::agent_env_preview_cmd(p),
            args::AgentCmd::Config {
                cmd: args::AgentConfigCmd::Resolve(ref r),
            } => agent::agent_config_resolve_cmd(r),
            args::AgentCmd::Stability(s) => Box::pin(agent::agent_stability_cmd(*s)).await,
            args::AgentCmd::Suite(s) => Box::pin(agent::agent_suite_cmd(*s)).await,
            args::AgentCmd::PolicyCheck(p) => agent::agent_policy_check_cmd(&p),
            args::AgentCmd::Apply(a) => agent::agent_apply_cmd(&a),
            args::AgentCmd::BestOf(b) => Box::pin(agent::agent_best_of_cmd(*b)).await,
            args::AgentCmd::Profile(p) => agent::agent_profile_cmd(&p),
            args::AgentCmd::Runs(r) => agent::agent_runs_cmd(&r),
            args::AgentCmd::FsAudit(a) => agent::agent_fs_audit_cmd(&a),
            args::AgentCmd::ArtifactCheck(a) => agent::agent_artifact_check_cmd(&a),
            args::AgentCmd::Doctor(d) => agent::agent_doctor_cmd(&d),
            args::AgentCmd::Annotate(a) => agent::agent_annotate_cmd(&a),
        },
        Command::Catalog(c) => catalog::run_catalog(c),
        Command::Explain(c) => explain::run_explain(&c),
        Command::Completions(c) => {
            use clap::CommandFactory;
            use std::io::Write as _;
            let mut cmd = Cli::command();
            let bin_name = cmd.get_name().to_string();
            let stdout = std::io::stdout();
            let mut writer = std::io::BufWriter::new(stdout.lock());
            clap_complete::generate(c.shell, &mut cmd, bin_name, &mut writer);
            writer.flush()?;
            Ok(())
        }
        Command::Ui(u) => Box::pin(ui_cmd(u)).await,
        #[cfg(feature = "docker")]
        Command::Cleanup => Box::pin(cleanup_cmd()).await,
        #[cfg(not(feature = "docker"))]
        Command::Cleanup => cleanup_cmd(),
    }
}

fn print_doctor_text(report: &crate::run::agent_doctor::DoctorReport) {
    use crate::run::agent_doctor::CheckStatus;
    println!("[agent doctor] host-readiness check (no model call, $0):");
    for c in &report.checks {
        let label = match c.status {
            CheckStatus::Pass => "pass",
            CheckStatus::Fail => "fail",
            CheckStatus::Skip => "skip",
        };
        // `detail` carries the remediation hint on failures; no secret values
        // are ever placed in it (credential checks are presence-only).
        println!("  [{label}] {}: {}", c.check, c.detail);
    }
    let verdict = if report.ready { "ready" } else { "NOT ready" };
    println!("[agent doctor] host is {verdict}.");
}

fn print_env_preview_text(preview: &crate::run::env_preview::EnvPreview) {
    print!("{}", crate::run::env_preview::format_preview_text(preview));
}

fn effective_log_level(cli_log: Option<&str>) -> String {
    cli_log
        .map(str::to_owned)
        .or_else(|| std::env::var("MAXWELL_LOG").ok())
        .or_else(|| std::env::var("RUST_SWE_AGENT_LOG").ok())
        .unwrap_or_else(|| "info".into())
}

fn init_logging(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn resolve_and_validate_workdir(
    workdir_opt: Option<&std::path::PathBuf>,
    cfg: &crate::config::Config,
) -> Result<Option<std::path::PathBuf>, Error> {
    if let Some(wd) = workdir_opt {
        if !wd.exists() || !wd.is_dir() {
            return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                "--workdir {} does not exist or is not a directory",
                wd.display()
            ))));
        }
        if matches!(cfg.root.environment.kind, crate::config::EnvKind::Docker) {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "docker container workdir is fixed; --workdir cannot be used with docker environment".to_string()
            )));
        }
        let canonical = std::fs::canonicalize(wd).map_err(|e| {
            Error::Config(crate::error::ConfigError::Usage(format!(
                "failed to canonicalize --workdir {}: {e}",
                wd.display()
            )))
        })?;
        Ok(Some(canonical))
    } else {
        Ok(None)
    }
}

/// Recursively redact string values in a JSON tree without touching numeric,
/// boolean, or key text — prevents redaction from corrupting machine output.
fn redact_json_strings(v: &mut serde_json::Value, redactor: &crate::redaction::Redactor) {
    match v {
        serde_json::Value::String(s) => {
            *s = redactor
                .redact_text(s, crate::redaction::surface::TRAJECTORY)
                .text;
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                redact_json_strings(item, redactor);
            }
        }
        serde_json::Value::Object(map) => {
            for val in map.values_mut() {
                redact_json_strings(val, redactor);
            }
        }
        _ => {}
    }
}

/// Load and JSON-parse a trajectory file, mapping errors to `Error::Config`.
fn load_resume_traj(path: &std::path::Path) -> Result<crate::trajectory::Trajectory, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "--resume: cannot read trajectory file `{}`: {e}",
            path.display()
        )))
    })?;
    serde_json::from_str(&text).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "--resume: trajectory file `{}` is not valid JSON: {e}",
            path.display()
        )))
    })
}

/// Validate `traj` for resume and exit the process on the first violation.
fn validate_resume_or_exit(traj: &crate::trajectory::Trajectory, path: &std::path::Path) {
    use crate::run::mini::ResumeValidationError;
    match crate::run::mini::validate_resume_trajectory(traj) {
        Ok(()) => {}
        Err(ResumeValidationError::AlreadyTerminal) => exit_with_outcome(
            ExitCode::ResumeAlreadyTerminal,
            &format!(
                "cannot resume `{}`: trajectory already has a terminal outcome \
                 (outcome={:?}, exit_reason={:?})",
                path.display(),
                traj.info.outcome,
                traj.info.exit_reason,
            ),
        ),
        Err(ResumeValidationError::ManifestMissing) => exit_with_outcome(
            ExitCode::ResumeManifestMissing,
            &format!(
                "cannot resume `{}`: trajectory is missing required fields \
                 (task and/or model_name); the file may pre-date the manifest schema",
                path.display()
            ),
        ),
        Err(ResumeValidationError::InvalidPrefix(reason)) => exit_with_outcome(
            ExitCode::ResumeInvalidPrefix,
            &format!("cannot resume `{}`: {reason}", path.display()),
        ),
    }
}

/// Enforce that no cap flags were raised without `--resume-allow-step-bump`.
/// Exits if any disallowed flag is present.
fn reject_cap_bump_without_flag(m: &args::MiniCmd) {
    let bumped: Vec<&str> = [
        (m.step_limit.is_some(), "--step-limit"),
        (m.task_timeout_secs.is_some(), "--task-timeout-secs"),
        (m.per_task_budget_usd.is_some(), "--per-task-budget-usd"),
    ]
    .into_iter()
    .filter_map(|(set, name)| set.then_some(name))
    .collect();
    if !bumped.is_empty() {
        exit_with_outcome(
            ExitCode::UsageError,
            &format!(
                "--resume: {} cannot be changed on resume without --resume-allow-step-bump",
                bumped.join(", ")
            ),
        );
    }
}

/// Validate `traj` for `--continue` and exit the process on the first violation.
fn validate_continue_or_exit(traj: &crate::trajectory::Trajectory, path: &std::path::Path) {
    use crate::run::mini::ContinueValidationError;
    match crate::run::mini::validate_continue_trajectory(traj) {
        Ok(()) => {}
        Err(ContinueValidationError::NonTerminal) => exit_with_outcome(
            ExitCode::ContinueNonTerminal,
            &format!(
                "--continue: `{}` is non-terminal (partial=true with no outcome or exit_reason); \
                 use `--resume` to continue an in-progress run instead",
                path.display()
            ),
        ),
        Err(ContinueValidationError::ManifestMissing) => exit_with_outcome(
            ExitCode::ResumeManifestMissing,
            &format!(
                "--continue: `{}` is missing required fields (task and/or model_name); \
                 the file may pre-date the manifest schema",
                path.display()
            ),
        ),
        Err(ContinueValidationError::InvalidPrefix(reason)) => exit_with_outcome(
            ExitCode::ResumeInvalidPrefix,
            &format!(
                "--continue: `{}` has an invalid message prefix: {}",
                path.display(),
                reason
            ),
        ),
    }
}

/// Enforce that no cap flags were raised without `--continue-allow-step-bump`.
fn reject_cap_bump_without_flag_continue(m: &args::MiniCmd) {
    let bumped: Vec<&str> = [
        (m.step_limit.is_some(), "--step-limit"),
        (m.task_timeout_secs.is_some(), "--task-timeout-secs"),
        (m.per_task_budget_usd.is_some(), "--per-task-budget-usd"),
    ]
    .into_iter()
    .filter_map(|(set, name)| set.then_some(name))
    .collect();
    if !bumped.is_empty() {
        exit_with_outcome(
            ExitCode::UsageError,
            &format!(
                "--continue: {} cannot be changed on a continuation without \
                 --continue-allow-step-bump",
                bumped.join(", ")
            ),
        );
    }
}

fn apply_read_only_policy(m: &args::MiniCmd, cfg: &Config) -> Result<(), Error> {
    if !m.read_only {
        return Ok(());
    }
    if m.github_pr.open_pr
        || m.github_pr.target_repo.is_some()
        || m.github_pr.target_branch.is_some()
    {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--read-only is incompatible with --open-pr, --target-repo, and --target-branch".into(),
        )));
    }
    if !m.allow_mcp_in_read_only && !cfg.root.agent.mcp_servers.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--read-only blocks MCP servers unless --allow-mcp-in-read-only is set".into(),
        )));
    }
    Ok(())
}

async fn replay_cmd(r: args::ReplayCmd) -> Result<(), Error> {
    let mut cfg = match &r.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };
    if let Some(kind) = &r.env {
        cfg.root.environment.kind = parse_env_kind(kind.as_str())?;
    }
    if let Some(img) = r.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }
    if let Some(v) = r.history_max_input_tokens {
        cfg.root.agent.history_max_input_tokens = Some(v);
    }
    if let Some(v) = r.history_keep_last_observations {
        cfg.root.agent.history_keep_last_observations = Some(v);
    }

    let args = crate::run::replay::ReplayArgs {
        trajectory_path: r.trajectory_path,
        config: cfg,
        output_dir: r.output,
        trajectory_name: r.trajectory_name,
        allow_unfingerprinted: r.allow_unfingerprinted,
        report_only: r.report_only,
        drift_cap_bytes: r.drift_cap_bytes,
    };
    crate::run::replay::run(args).await
}

/// Print a stable `outcome_class` label followed by the error detail, then exit.
///
/// Used for outcomes that are driven by explicit CLI logic (regression gate,
/// budget-halt forecast, tail abort) rather than propagated `Error` variants.
fn exit_with_outcome(code: ExitCode, detail: &str) -> ! {
    eprintln!("outcome_class: {}", code.outcome_class());
    eprintln!("error: {detail}");
    // Flush stdout so piped consumers receive any buffered report output
    // before the process terminates (process::exit bypasses Drop).
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(code.as_i32());
}

fn cancellation_exit_code(results: &crate::run::swebench::SweepResults) -> Option<i32> {
    (results.sweep_status == crate::run::swebench::SWEEP_STATUS_CANCELLED).then_some(
        results
            .cancel_exit_code
            .unwrap_or(crate::run::swebench::CANCEL_EXIT_CODE_GRACEFUL),
    )
}

fn exit_if_cancelled_sweep(results: &crate::run::swebench::SweepResults) {
    if let Some(code) = cancellation_exit_code(results) {
        let outcome = if code == crate::run::swebench::CANCEL_EXIT_CODE_GRACEFUL {
            ExitCode::Interrupted
        } else {
            ExitCode::Killed
        };
        exit_with_outcome(outcome, "sweep was cancelled");
    }
}

fn exit_if_systemic_halt_sweep(results: &crate::run::swebench::SweepResults) {
    if results.sweep_status == crate::run::swebench::SWEEP_STATUS_SYSTEMIC_HALT {
        let category = results
            .systemic_halt_category
            .map_or_else(|| "unknown".to_owned(), |c| format!("{c:?}"));
        exit_with_outcome(
            ExitCode::SystemicHalt,
            &format!("sweep halted: systemic failure detected (dominant category: {category})"),
        );
    }
}

async fn run_forecast_from_cmd(
    mut s: args::SwebenchCmd,
) -> Result<crate::run::forecast::ForecastOutcome, Error> {
    if s.rehearse {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "rehearsal mode cannot be used with forecast-first; forecast projects real sweep costs"
                .to_owned(),
        )));
    }
    s.github_pr.open_prs = false;
    s.github_pr.github_pr_dry_run = false;
    let calibration_n = s.calibration_n;
    let seed = s.seed.unwrap_or(42);
    let target_n = s.target_n;
    let confidence_pct = s.confidence;
    if s.sample.is_none() {
        s.seed = None;
    }
    let cfg = swebench_config_from_cmd(&s)?;
    let sweep = swebench_args_from_cmd(s, cfg, "forecast")?;
    #[allow(clippy::large_futures)]
    crate::run::forecast::run(crate::run::forecast::ForecastArgs {
        sweep,
        calibration_n,
        seed,
        target_n,
        confidence_pct,
    })
    .await
}

fn print_dry_run_summary(results: &crate::run::swebench::SweepResults, output_format: &str) {
    if output_format != "json" {
        print!("{}", results.summary_table());
    }
}

fn print_forecast_report(
    report: &crate::run::forecast::ForecastReport,
    output_format: &str,
) -> Result<(), Error> {
    match output_format {
        "text" => {
            print!("{}", crate::run::forecast::render_text(report));
            Ok(())
        }
        "json" => {
            println!("{}", crate::run::forecast::to_json(report)?);
            Ok(())
        }
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "unknown --format `{other}` (expected `text` or `json`)"
        )))),
    }
}

fn swebench_config_from_cmd(s: &args::SwebenchCmd) -> Result<Config, Error> {
    if s.stratify_by.is_none() && s.stratify_mode.is_some() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "`--stratify-mode` requires `--stratify-by`".into(),
        )));
    }
    let mut cfg = match &s.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };
    cfg.root.model.name.clone_from(&s.model);
    cfg.root.agent.step_limit = s.step_limit;
    if let Some(v) = s.observation_max_bytes {
        cfg.root.agent.observation_max_bytes = v;
    }
    if let Some(v) = s.observation_head_ratio {
        validate_observation_head_ratio(v)?;
        cfg.root.agent.observation_head_ratio = v;
    }
    if let Some(kind) = &s.env {
        cfg.root.environment.kind = parse_env_kind(kind.as_str())?;
    }
    if let Some(img) = s.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }
    if let Some(nm) = s.network_mode {
        cfg.root.environment.network_mode = parse_network_mode(nm.as_str())?;
    }
    if s.chaos_fail_every > 0 {
        cfg.root.environment.chaos_fail_every = s.chaos_fail_every;
    }
    if let Some(v) = s.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(v);
    }
    if s.hide_budget_from_agent {
        cfg.root.agent.hide_budget_from_agent = true;
    }
    if let Some(v) = s.detect_stagnation {
        cfg.root.agent.detect_stagnation = v;
    }
    if let Some(v) = s.stagnation_repeat_threshold {
        cfg.root.agent.stagnation_repeat_threshold = v;
    }
    if let Some(v) = s.stagnation_window {
        cfg.root.agent.stagnation_window = v;
    }
    if let Some(v) = s.history_max_input_tokens {
        cfg.root.agent.history_max_input_tokens = Some(v);
    }
    if let Some(v) = s.history_keep_last_observations {
        cfg.root.agent.history_keep_last_observations = Some(v);
    }
    apply_mcp_server_overrides(&mut cfg, &s.mcp_servers)?;
    Ok(cfg)
}

fn apply_mcp_server_overrides(cfg: &mut Config, commands: &[String]) -> Result<(), Error> {
    for command in commands {
        let command = command.trim();
        if command.is_empty() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "--mcp-server command cannot be empty".into(),
            )));
        }
        cfg.root
            .agent
            .mcp_servers
            .push(crate::config::McpServerCfg {
                command: command.to_owned(),
                timeout_secs: None,
            });
    }
    Ok(())
}

fn validate_observation_head_ratio(value: f64) -> Result<(), Error> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "--observation-head-ratio must be a finite value in [0,1], got {value}"
        ))))
    }
}

/// Reject `--result-format json` combined with the ratatui dashboard UI.
///
/// The ratatui dashboard enters the alternate screen and renders to
/// `std::io::stdout()` (see `agent::confirm_tui`), which would interleave
/// terminal-control bytes with the JSON result and break the clean-stdout
/// contract. The two modes are fundamentally incompatible, so reject early.
fn reject_json_with_ratatui(
    result_format: crate::run::mini::ResultFormat,
    interactive_mode: crate::run::mini::InteractiveMode,
) -> Result<(), Error> {
    use crate::run::mini::{InteractiveMode, ResultFormat};
    if result_format == ResultFormat::Json
        && matches!(
            interactive_mode,
            InteractiveMode::Ratatui | InteractiveMode::RatatuiMonitor
        )
    {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--result-format json cannot be combined with --ui ratatui; the dashboard \
             renders to stdout and would corrupt the JSON result stream. Use the default \
             --ui stderr."
                .into(),
        )));
    }
    Ok(())
}

/// Reject `bench tail --ui ratatui` combined with flags it can't coexist
/// with (issue #641).
///
/// `--once` is a contradiction: the dashboard is inherently long-running and
/// interactive. `--format json` is a contradiction for the same reason
/// `reject_json_with_ratatui` rejects it for `mini` — the dashboard owns the
/// alternate screen and stdout, which would corrupt a JSON snapshot stream.
fn reject_incompatible_tail_ratatui_ui(once: bool, format: TailFormat) -> Result<(), Error> {
    if once {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "bench tail --ui ratatui cannot be combined with --once; the dashboard is \
             inherently interactive and long-running. Omit --ui ratatui for a one-shot \
             snapshot."
                .into(),
        )));
    }
    if format == TailFormat::Json {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "bench tail --ui ratatui cannot be combined with --format json; the dashboard \
             renders to the alternate screen and would corrupt a JSON snapshot stream. Use \
             the default --format text, or omit --ui ratatui for JSON output."
                .into(),
        )));
    }
    Ok(())
}

/// Lightweight view over a trajectory file that deserializes only the `info`
/// block. A trajectory carries the full message history and tool outputs
/// (potentially megabytes) that `emit_mini_result` never reads.
#[derive(serde::Deserialize)]
struct TrajectoryInfoOnly {
    info: crate::trajectory::TrajectoryInfo,
}

/// Emit a machine-readable run result to stdout when `--result-format json` is active.
///
/// Emission scope: only when the trajectory records `outcome == "submitted"`,
/// covering a clean submit (exit 0) and a submitted-then-verification-failed run
/// (exit 7). Hard errors before a trajectory exists, and unsubmitted runs (step
/// limit, budget, stagnation), print no result object.
///
/// The reported `exit_code`/`exit_outcome_class` reflect the **effective** final
/// exit: a run error takes precedence (it is propagated first by the caller),
/// otherwise a GitHub PR-publish error, otherwise success. This keeps the JSON
/// honest even when `--open-pr` publishing fails after a successful submit.
fn emit_mini_result(
    format: crate::run::mini::ResultFormat,
    run_result: &Result<(), Error>,
    publish_result: &Result<(), Error>,
    traj_path: &std::path::Path,
    patch_path: Option<&std::path::Path>,
    redactor: &crate::redaction::Redactor,
) -> Result<(), Error> {
    if format != crate::run::mini::ResultFormat::Json {
        return Ok(());
    }
    // Only Ok and VerificationFailed guarantee a trajectory (and patch) on disk.
    // Any other run error is a hard pre-/mid-trajectory failure → no result object;
    // the caller's `run_result?` propagates it and `main` prints the outcome class.
    if !(run_result.is_ok() || matches!(run_result, Err(Error::VerificationFailed(..)))) {
        return Ok(());
    }
    // Effective exit code: run error first (caller propagates it before the publish
    // error), then a publish error, otherwise success.
    let exit_code = match (run_result, publish_result) {
        (Err(e), _) | (Ok(()), Err(e)) => crate::exit_code::ExitCode::from_error(e),
        (Ok(()), Ok(())) => crate::exit_code::ExitCode::Success,
    };
    // Deserialize only the `info` block (see TrajectoryInfoOnly) to avoid
    // loading the full message history just to read summary fields.
    let traj_json = std::fs::read_to_string(traj_path).map_err(Error::Io)?;
    let traj: TrajectoryInfoOnly = serde_json::from_str(&traj_json).map_err(Error::Json)?;
    // Only emit when the agent actually submitted a patch — the unifying condition
    // behind both contract cases. Unsubmitted runs (incl. a `--verify` failure on a
    // step-limit/budget run) are represented only by the trajectory.
    if traj.info.outcome.as_deref() != Some(crate::trajectory::outcome::SUBMITTED) {
        return Ok(());
    }
    // The spec promises absolute paths. Canonicalize where possible (the files
    // exist on disk by this point); fall back to the as-given path if the
    // filesystem call fails so we never panic on an unusual path.
    let abs_traj_path = traj_path
        .canonicalize()
        .unwrap_or_else(|_| traj_path.to_path_buf());
    let abs_patch_path = patch_path
        .filter(|p| p.exists())
        .and_then(|p| p.canonicalize().ok());
    let result = crate::run::mini_result::MiniResult::from_trajectory_info(
        &traj.info,
        exit_code,
        &abs_traj_path,
        abs_patch_path.as_deref(),
    );
    let json = result.to_redacted_json(redactor).map_err(Error::Json)?;
    println!("{json}");
    Ok(())
}

#[allow(clippy::single_option_map)]
fn build_patch_capture_spec(
    github_pr: Option<&crate::run::github_pr::GithubPrOptions>,
    resolved_workdir: Option<&std::path::PathBuf>,
    cfg: &Config,
    skip_patch_validation: bool,
) -> Option<crate::run::mini::PatchCaptureSpec> {
    github_pr.map(|options| crate::run::mini::PatchCaptureSpec {
        base_commit: Some(options.target_branch.clone()),
        workdir: resolved_workdir
            .cloned()
            .unwrap_or_else(|| std::path::PathBuf::from(cfg.root.environment.workdir.clone())),
        patch_path: options.patch_path.clone(),
        skip_patch_validation,
    })
}

fn swebench_github_pr_config(
    github: &args::SwebenchGithubPrArgs,
) -> Option<crate::run::github_pr::GithubPrSweepConfig> {
    if !github.open_prs && !github.github_pr_dry_run {
        return None;
    }
    Some(crate::run::github_pr::GithubPrSweepConfig {
        target_repo: github.target_repo.clone().unwrap_or_default(),
        target_branch: github.target_branch.clone().unwrap_or_default(),
        token_env: github.github_token_env.clone(),
        mode: if github.github_pr_dry_run {
            crate::run::github_pr::PublishMode::DryRun
        } else {
            crate::run::github_pr::PublishMode::Open
        },
        timeout_secs: github.github_pr_timeout_secs,
        max_retries: github.github_pr_max_retries,
        backoff_base_ms: github.github_pr_backoff_base_ms,
        branch_prefix: github.github_pr_branch_prefix.clone(),
    })
}

fn validate_swebench_github_pr_args(github: &args::SwebenchGithubPrArgs) -> Result<(), Error> {
    if !github.open_prs && !github.github_pr_dry_run {
        return Ok(());
    }
    let _ = required_github_arg(github.target_repo.as_deref(), "--target-repo")?;
    let _ = required_github_arg(github.target_branch.as_deref(), "--target-branch")?;
    crate::run::github_pr::validate_branch_prefix(&github.github_pr_branch_prefix)
        .map_err(Error::Config)?;
    Ok(())
}

fn required_github_arg(value: Option<&str>, name: &str) -> Result<String, Error> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "{name} is required with --open-pr/--github-pr-dry-run"
            )))
        })
}

fn trajectory_submitted(path: &std::path::Path) -> Result<bool, Error> {
    let text = std::fs::read_to_string(path)?;
    let trajectory: crate::trajectory::Trajectory = serde_json::from_str(&text)?;
    Ok(trajectory.info.outcome.as_deref() == Some(crate::trajectory::outcome::SUBMITTED))
}

async fn publish_github_pr(
    options: crate::run::github_pr::GithubPrOptions,
    to_stderr: bool,
) -> Result<(), Error> {
    let result = crate::run::github_pr::publish(options).await?;
    // When the run result is being emitted as JSON, this human-facing PR text
    // must not pollute stdout — stdout has to be exactly one JSON object.
    if let Some(output) = result.dry_run_output {
        if to_stderr {
            eprint!("{output}");
        } else {
            print!("{output}");
        }
    } else if let Some(url) = result.url {
        if to_stderr {
            eprintln!("github_pr_url: {url}");
        } else {
            println!("github_pr_url: {url}");
        }
    }
    Ok(())
}

async fn maybe_publish_mini_github_pr(
    github_pr: Option<crate::run::github_pr::GithubPrOptions>,
    json_result_mode: bool,
) -> Result<(), Error> {
    if let Some(options) = github_pr {
        let traj_path = options.trajectory_ref.clone();
        if trajectory_submitted(std::path::Path::new(&traj_path))? {
            publish_github_pr(options, json_result_mode).await?;
        } else {
            tracing::info!(trajectory = %traj_path, "github PR skipped because run did not submit");
        }
    }
    Ok(())
}

fn github_pr_failure_count(results: &crate::run::swebench::SweepResults) -> usize {
    results.github_pr_failures
}

fn parse_dataset_source(
    s: &args::SwebenchCmd,
) -> Result<(crate::run::dataset::DatasetSource, std::path::PathBuf), Error> {
    let cache_dir = s
        .dataset_cache_dir
        .clone()
        .unwrap_or_else(crate::run::dataset::default_cache_dir);

    match (&s.dataset_path, &s.dataset) {
        (Some(_), Some(_)) => Err(Error::Config(crate::error::ConfigError::Invalid(
            "--dataset-path and --dataset are mutually exclusive; provide only one".into(),
        ))),
        (None, None) => Err(Error::Config(crate::error::ConfigError::Invalid(
            "one of --dataset-path or --dataset is required".into(),
        ))),
        (Some(path), None) => Ok((
            crate::run::dataset::DatasetSource::LocalPath(path.clone()),
            cache_dir,
        )),
        (None, Some(alias_str)) => {
            let alias = alias_str
                .parse::<crate::run::dataset::SwebenchAlias>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            let split_str = s.split.as_deref().unwrap_or("test");
            let split = split_str
                .parse::<crate::run::dataset::SwebenchSplit>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            Ok((
                crate::run::dataset::DatasetSource::Named { alias, split },
                cache_dir,
            ))
        }
    }
}

fn swebench_args_from_cmd(
    s: args::SwebenchCmd,
    cfg: Config,
    preflight_mode: &str,
) -> Result<crate::run::swebench::SwebenchArgs, Error> {
    let cfg_max_rpm = cfg.root.sweep.max_rpm;
    let cfg_max_input_tpm = cfg.root.sweep.max_input_tpm;
    let github_pr = swebench_github_pr_config(&s.github_pr);
    let (dataset_source, dataset_cache_dir) = parse_dataset_source(&s)?;
    Ok(crate::run::swebench::SwebenchArgs {
        dataset_source,
        dataset_cache_dir,
        output_dir: s.output,
        parallel: s.parallel,
        config: cfg,
        reruns: s.reruns,
        resume: s.resume,
        cost_limit_usd: s.sweep_cost_limit_usd,
        task_timeout_secs: s.task_timeout_secs,
        instance_ids: s.instance_ids,
        limit: s.limit,
        sample: s.sample,
        seed: s.seed,
        stratify_by: s.stratify_by.map(|v| match v {
            args::StratifyByArg::Repo => crate::run::swebench::StratifyBy::Repo,
        }),
        stratify_mode: match s
            .stratify_mode
            .unwrap_or(args::StratifyModeArg::Proportional)
        {
            args::StratifyModeArg::Proportional => crate::run::swebench::StratifyMode::Proportional,
            args::StratifyModeArg::Balanced => crate::run::swebench::StratifyMode::Balanced,
        },
        max_retries: s.max_retries,
        retry_on: s.retry_on,
        retry_backoff_base_ms: s.retry_backoff_base_ms,
        retry_backoff_cap_s: s.retry_backoff_cap_s,
        retry_on_resume: s.retry_on_resume,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        config_overlay_paths: s.config.into_iter().collect(),
        dry_run: s.dry_run,
        skip_preflight: s.skip_preflight,
        preflight_format: s.format,
        skip_model_probe: s.skip_model_probe,
        preflight_check_timeout_s: s.preflight_check_timeout_s,
        preflight_total_timeout_s: s.preflight_total_timeout_s,
        preflight_mode: preflight_mode.into(),
        skip_patch_validation: s.skip_patch_validation,
        event_log: s.event_log,
        max_rpm: s.max_rpm.or(cfg_max_rpm),
        max_input_tpm: s.max_input_tpm.or(cfg_max_input_tpm),
        cancel_deadline_secs: s.cancel_deadline,
        install_os_signal_handlers: true,
        cancellation_signals: None,
        github_pr,
        reproduced_from: None,
        abort_on_systemic_failure: s.abort_on_systemic_failure,
        systemic_failure_min_samples: s.systemic_failure_min_samples,
        systemic_failure_share_pct: s.systemic_failure_share_pct,
        otlp_endpoint: s.otlp_endpoint,
        otlp_metrics_interval_secs: s.otlp_metrics_interval_secs,
        rehearse: s.rehearse,
        skip_evaluator: s.skip_evaluator,
        eval_backend: s.eval_backend,
        sb_subset: s.sb_subset,
        sb_split: s.sb_split,
        eval_timeout_secs: s.eval_timeout_secs,
        notify_webhook_url: s.notify_webhook,
        notify_webhook_headers: s.notify_webhook_headers,
    })
}

/// Map `(interactive, yolo, ui)` CLI flags onto a `run::mini::InteractiveMode`.
fn resolve_interactive_mode(
    interactive: bool,
    yolo: bool,
    ui: args::UiKind,
) -> crate::run::mini::InteractiveMode {
    use crate::run::mini::InteractiveMode;
    match (interactive, yolo) {
        (false, false) => InteractiveMode::Off,
        (_, true) => match ui {
            args::UiKind::Stderr => InteractiveMode::YoloStatusOnly,
            args::UiKind::Ratatui => InteractiveMode::RatatuiMonitor,
        },
        (true, false) => match ui {
            args::UiKind::Stderr => InteractiveMode::StderrPrompt,
            args::UiKind::Ratatui => InteractiveMode::Ratatui,
        },
    }
}

fn parse_verify_checks(
    specs: &[String],
) -> Result<Vec<crate::trajectory::VerificationCheck>, Error> {
    specs
        .iter()
        .map(|s| {
            let colon = s.find(':').ok_or_else(|| {
                Error::Config(crate::error::ConfigError::Invalid(format!(
                    "--verify must be in NAME:COMMAND format, got `{s}`"
                )))
            })?;
            let name = s[..colon].trim();
            let command = s[colon + 1..].trim();
            if name.is_empty() || command.is_empty() {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "--verify NAME:COMMAND requires non-empty name and command, got `{s}`"
                ))));
            }
            Ok(crate::trajectory::VerificationCheck {
                name: name.to_owned(),
                command: command.to_owned(),
            })
        })
        .collect()
}

/// Load a `contamination.json` and emit a contamination-adjusted resolved-rate section.
fn print_contamination_adjusted_rate(
    path: &std::path::Path,
    compare_report: &crate::run::compare::CompareReport,
    candidate_dir: &std::path::Path,
) -> Result<(), Error> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "compare: cannot read contamination report `{}`: {e}",
            path.display()
        )))
    })?;
    let contamination: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "compare: malformed contamination.json `{}`: {e}",
            path.display()
        )))
    })?;

    // Validate that the contamination report was produced for this candidate sweep.
    if let Some(report_sweep) = contamination["sweep_path"].as_str() {
        let candidate_canonical = candidate_dir
            .canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if !candidate_canonical.is_empty() && report_sweep != candidate_canonical {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "compare: contamination report was produced for '{}' but candidate sweep is '{}'; \
                 re-run `bench contamination-check --sweep {}` to refresh the report",
                report_sweep,
                candidate_canonical,
                candidate_dir.display()
            ))));
        }
    }

    let total_resolved = usize::try_from(
        contamination["summary"]["total_resolved"]
            .as_u64()
            .unwrap_or(0),
    )
    .unwrap_or(usize::MAX);
    let high_count = usize::try_from(contamination["summary"]["high_count"].as_u64().unwrap_or(0))
        .unwrap_or(usize::MAX);
    let high_share = contamination["summary"]["high_risk_share"]
        .as_f64()
        .ok_or_else(|| {
            Error::Io(std::io::Error::other(format!(
                "compare: contamination report `{}` is missing or has non-numeric \
                 `summary.high_risk_share` field",
                path.display()
            )))
        })?;

    let raw_rate = compare_report.candidate_resolved_rate;
    let adjusted_absolute = raw_rate * (1.0 - high_share);

    println!(
        "\ncontamination-adjusted resolved-rate (candidate):\n  \
         raw resolved-rate       : {raw:.1}%\n  \
         high-risk instances     : {high} of {total} resolved ({pct:.1}%)\n  \
         contamination-adjusted  : {adj:.1}%\n",
        raw = raw_rate * 100.0,
        high = high_count,
        total = total_resolved,
        pct = high_share * 100.0,
        adj = adjusted_absolute * 100.0,
    );

    Ok(())
}

fn apply_significance_gates(
    report: &crate::run::compare::CompareReport,
    min_significance: Option<f64>,
    regression_significance: Option<f64>,
    allow_underpowered: bool,
) {
    let sig = &report.resolved_rate_significance;

    // Validate alpha values before any gating: must be a finite probability in (0, 1).
    for (flag, alpha) in [
        ("--min-significance", min_significance),
        ("--regression-significance", regression_significance),
    ] {
        if let Some(a) = alpha {
            if !a.is_finite() || a <= 0.0 || a >= 1.0 {
                exit_with_outcome(
                    ExitCode::RegressionGateFailure,
                    &format!("compare: {flag} alpha must be a probability in (0, 1), got {a}"),
                );
            }
        }
    }

    // Check underpowered block first — applies whenever a significance gate is active.
    let any_gate_active = min_significance.is_some() || regression_significance.is_some();
    if any_gate_active && sig.underpowered && !allow_underpowered {
        tracing::error!(
            paired_n = sig.paired_n,
            underpowered_reason = sig.underpowered_reason.as_deref().unwrap_or(""),
            "compare: significance test is underpowered; pass --allow-underpowered to override"
        );
        exit_with_outcome(
            ExitCode::RegressionGateFailure,
            "compare: significance test is underpowered (add --allow-underpowered to override)",
        );
    }

    // Use the paired-subset delta direction (fail_to_pass vs pass_to_fail) for gating.
    // This ensures the gate direction matches the data the p-value was computed from,
    // which is important when sweeps have non-overlapping instances.
    let paired_positive = sig.fail_to_pass > sig.pass_to_fail;
    let paired_negative = sig.pass_to_fail > sig.fail_to_pass;

    if let Some(alpha) = min_significance {
        // Gate fires when the paired delta is positive AND p > alpha (noise win).
        if paired_positive {
            let p = sig.p_value.unwrap_or(1.0);
            if p > alpha {
                tracing::error!(
                    p_value = p,
                    alpha = alpha,
                    "compare: positive paired delta is not significant at --min-significance threshold"
                );
                exit_with_outcome(
                    ExitCode::RegressionGateFailure,
                    &format!(
                        "compare: positive paired delta is not significant (p={p:.4} > alpha={alpha})"
                    ),
                );
            }
        }
    }

    if let Some(alpha) = regression_significance {
        // Gate fires when the paired delta is negative AND p <= alpha (significant regression).
        if paired_negative {
            let p = sig.p_value.unwrap_or(1.0);
            if p <= alpha {
                tracing::error!(
                    p_value = p,
                    alpha = alpha,
                    "compare: negative paired delta is significant at --regression-significance threshold"
                );
                exit_with_outcome(
                    ExitCode::RegressionGateFailure,
                    &format!("compare: significant regression (p={p:.4} <= alpha={alpha})"),
                );
            }
        }
    }
}

fn parse_compare_format(raw: &str) -> Result<crate::run::compare::CompareFormat, Error> {
    match raw {
        "text" => Ok(crate::run::compare::CompareFormat::Text),
        "json" => Ok(crate::run::compare::CompareFormat::Json),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "unknown --format `{other}` (expected `text` or `json`)"
        )))),
    }
}

fn parse_trajectory_diff_format(
    raw: &str,
) -> Result<crate::run::trajectory_diff::TrajectoryDiffFormat, Error> {
    match raw {
        "text" => Ok(crate::run::trajectory_diff::TrajectoryDiffFormat::Text),
        "json" => Ok(crate::run::trajectory_diff::TrajectoryDiffFormat::Json),
        "unified" => Ok(crate::run::trajectory_diff::TrajectoryDiffFormat::Unified),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "unknown --format `{other}` (expected `text`, `json`, or `unified`)"
        )))),
    }
}

fn print_trajectory_diff(
    report: &crate::run::trajectory_diff::TrajectoryDiffReport,
    format: crate::run::trajectory_diff::TrajectoryDiffFormat,
) -> Result<(), Error> {
    match format {
        crate::run::trajectory_diff::TrajectoryDiffFormat::Text => {
            print!("{}", crate::run::trajectory_diff::render_text(report));
        }
        crate::run::trajectory_diff::TrajectoryDiffFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
        }
        crate::run::trajectory_diff::TrajectoryDiffFormat::Unified => {
            print!("{}", crate::run::trajectory_diff::render_unified(report));
        }
    }
    Ok(())
}

#[cfg(feature = "ui-server")]
async fn ui_cmd(u: args::UiCmd) -> Result<(), Error> {
    crate::run::ui::run(crate::run::ui::UiArgs {
        sweep: u.sweep,
        port: u.port,
        bind: u.bind,
        open: u.open,
    })
    .await
}

#[cfg(not(feature = "ui-server"))]
#[allow(clippy::unused_async)]
async fn ui_cmd(_u: args::UiCmd) -> Result<(), Error> {
    exit_with_outcome(
        ExitCode::FeatureUnavailable,
        "the `ui` command requires the `ui-server` Cargo feature, which was not compiled in. \
         Rebuild with `cargo build --features ui-server`. \
         See docs/spec-web-ui.md for details.",
    );
}

#[cfg(feature = "docker")]
async fn cleanup_cmd() -> Result<(), Error> {
    let reaped = crate::env::docker::cleanup_orphans().await?;
    tracing::info!(count = reaped.len(), "reaped orphan containers");
    for id in reaped {
        println!("{id}");
    }
    Ok(())
}

#[cfg(not(feature = "docker"))]
fn cleanup_cmd() -> Result<(), Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "docker feature not compiled in".into(),
    )))
}

/// Compare annotations between original and replay sweep directories.
/// Best-effort — prints a warning when annotations differ; silent on errors.
fn render_reproduce_annotation_diff(
    from: &std::path::Path,
    output: &std::path::Path,
    replayed_ids: &std::collections::HashSet<&str>,
) {
    use crate::annotation::{AnnotationStore, DEFAULT_STORE_FILENAME};
    let orig_path = from.join(DEFAULT_STORE_FILENAME);
    let replay_path = output.join(DEFAULT_STORE_FILENAME);

    if !orig_path.is_file() {
        return;
    }

    let Ok(orig) = AnnotationStore::load_or_default(&orig_path) else {
        return;
    };
    let Ok(replay) = AnnotationStore::load_or_default(&replay_path) else {
        return;
    };

    if orig.list(None, None).is_empty() && replay.list(None, None).is_empty() {
        return;
    }

    let (mut only_orig, mut only_replay) =
        crate::run::annotate::diff_annotation_stores(&orig, &replay);

    // For partial replays, suppress false-drift signals from skipped instances.
    if !replayed_ids.is_empty() {
        only_orig.retain(|(iid, _)| replayed_ids.contains(iid.as_str()));
        only_replay.retain(|(iid, _)| replayed_ids.contains(iid.as_str()));
    }

    if only_orig.is_empty() && only_replay.is_empty() {
        eprintln!("reproduce: annotations match between original and replay sweeps");
        return;
    }

    eprintln!(
        "reproduce: annotation diff — {} annotation(s) only in original, {} only in replay",
        only_orig.len(),
        only_replay.len()
    );
    for (iid, tag) in &only_orig {
        eprintln!("  - original only: {iid} [{tag}]");
    }
    for (iid, tag) in &only_replay {
        eprintln!("  + replay only:   {iid} [{tag}]");
    }
}

/// Build a current-environment manifest for drift comparison by cloning the
/// source manifest and overwriting every field that reflects the runtime
/// environment (not the intentional replay settings).
fn build_current_manifest_for_reproduce(
    source: &crate::run::swebench::ProvenanceManifest,
) -> crate::run::swebench::ProvenanceManifest {
    let mut current = source.clone();
    current.harness.git_sha = current_git_sha();
    current.harness.git_dirty = None;
    current.runtime.started_at_utc = chrono_now_utc();
    current.runtime.finished_at_utc = None;
    current.runtime.host_os = std::env::consts::OS.into();
    current.runtime.rust_version = current_rust_version();
    current
}

fn current_git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn current_rust_version() -> Option<String> {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn chrono_now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn load_sweep_results(
    sweep_dir: &std::path::Path,
) -> Result<crate::run::swebench::SweepResults, Error> {
    let path = sweep_dir.join("results.json");
    let file = std::fs::File::open(&path).map_err(Error::Io)?;
    serde_json::from_reader(std::io::BufReader::new(file)).map_err(Error::Json)
}

fn hash_manifest(manifest: &crate::run::swebench::ProvenanceManifest) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_string(manifest).unwrap_or_default();
    let hash = Sha256::digest(json.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in hash {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    format!("sha256:{hex}")
}

#[allow(clippy::too_many_lines)]
fn reproduce_swebench_args(
    r: &args::ReproduceCmd,
    manifest: &crate::run::swebench::ProvenanceManifest,
    source_results: &crate::run::swebench::SweepResults,
    source_manifest_hash: &str,
) -> Result<crate::run::swebench::SwebenchArgs, Error> {
    use crate::run::dataset::DatasetSource;

    let mut cfg = Config::defaults()?;
    cfg.root.model.name.clone_from(&manifest.model.name);
    // Re-apply the source sweep's chaos cadence so a reproduction injects the
    // same deterministic failures (issue #340).
    cfg.root.environment.chaos_fail_every = manifest.chaos_fail_every;

    if let Some(budget) = r.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(budget);
    }

    // Reconstruct dataset source from manifest.
    let dataset_source = match manifest.dataset.source_kind.as_str() {
        "named" => {
            let alias_str = manifest.dataset.alias.as_deref().unwrap_or("verified");
            let split_str = manifest.dataset.split.as_deref().unwrap_or("test");
            let alias = alias_str
                .parse::<crate::run::dataset::SwebenchAlias>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            let split = split_str
                .parse::<crate::run::dataset::SwebenchSplit>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            DatasetSource::Named { alias, split }
        }
        _ => {
            // Local path — use the recorded path as-is.
            DatasetSource::LocalPath(
                bundle_reproduce_dataset_path(r, manifest)?
                    .unwrap_or_else(|| std::path::PathBuf::from(&manifest.dataset.path)),
            )
        }
    };

    // Determine the exact instance subset to replay.
    // Priority: explicit --filter > recorded filter_spec.instance_ids > actual
    // instance list from the source results. The fallback ensures that sweeps
    // originally run with --limit or --sample (no explicit instance-id list)
    // still reproduce only the recorded subset rather than the entire dataset.
    let instance_ids = if let Some(filter) = &r.filter {
        Some(filter.clone())
    } else if let Some(ids) = source_results.filter_spec.instance_ids.as_ref() {
        Some(ids.join(","))
    } else {
        let ids: Vec<&str> = source_results
            .instances
            .iter()
            .map(|i| i.instance_id.as_str())
            .collect();
        if ids.is_empty() {
            None
        } else {
            Some(ids.join(","))
        }
    };

    Ok(crate::run::swebench::SwebenchArgs {
        dataset_source,
        dataset_cache_dir: crate::run::dataset::default_cache_dir(),
        output_dir: r.output.clone(),
        parallel: r.parallel,
        config: cfg,
        reruns: 1,
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids,
        limit: r.limit,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: crate::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 1000,
        retry_backoff_cap_s: 60,
        retry_on_resume: false,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        config_overlay_paths: vec![],
        dry_run: false,
        skip_preflight: false,
        preflight_format: "text".into(),
        skip_model_probe: r.skip_model_probe,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "sweep".into(),
        skip_patch_validation: false,
        event_log: None,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: 30,
        install_os_signal_handlers: true,
        cancellation_signals: None,
        github_pr: None,
        reproduced_from: Some((
            source_manifest_hash.to_owned(),
            r.from.display().to_string(),
        )),
        // Re-apply the source sweep's circuit-breaker config for apples-to-apples
        // reproducibility; fall back to defaults when the source predates this feature.
        abort_on_systemic_failure: manifest
            .circuit_breaker
            .as_ref()
            .is_none_or(|cb| cb.enabled),
        systemic_failure_min_samples: manifest
            .circuit_breaker
            .as_ref()
            .map_or(5, |cb| cb.min_samples),
        systemic_failure_share_pct: manifest
            .circuit_breaker
            .as_ref()
            .map_or(80, |cb| cb.share_pct),
        otlp_endpoint: None,
        otlp_metrics_interval_secs: None,
        rehearse: false,
        skip_evaluator: false,
        eval_backend: "rehearsal".to_string(),
        sb_subset: None,
        sb_split: None,
        eval_timeout_secs: None,
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    })
}

fn bundle_reproduce_dataset_path(
    r: &args::ReproduceCmd,
    manifest: &crate::run::swebench::ProvenanceManifest,
) -> Result<Option<std::path::PathBuf>, Error> {
    if !r
        .from
        .join(crate::run::bundle::BUNDLE_MANIFEST_PATH)
        .exists()
    {
        return Ok(None);
    }
    let recorded = std::path::PathBuf::from(&manifest.dataset.path);
    if !recorded.is_absolute() {
        let bundled_relative = r.from.join(&recorded);
        if bundled_relative.exists() {
            return Ok(Some(bundled_relative));
        }
    }
    if recorded.exists() {
        return Ok(Some(recorded));
    }
    Err(Error::Config(crate::error::ConfigError::Invalid(format!(
        "bundle reproduce requires the original local dataset `{}` for positive replays; \
         use --limit 0 for bundle readability smoke checks or make the recorded dataset path available",
        manifest.dataset.path
    ))))
}

fn parse_breakdown_selection(
    raw: &str,
    allow_default: bool,
) -> Result<crate::run::evaluate::BreakdownSelection, Error> {
    if raw == "none" {
        return Ok(crate::run::evaluate::BreakdownSelection::none());
    }
    let mut axes = Vec::new();
    for tok in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let axis_kind = match tok {
            "repo" => crate::run::evaluate::BreakdownAxis::Repo,
            "failure_category" => crate::run::evaluate::BreakdownAxis::FailureCategory,
            other => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "unknown --breakdown axis `{other}`"
                ))));
            }
        };
        if !axes.contains(&axis_kind) {
            axes.push(axis_kind);
        }
    }
    if axes.is_empty() && allow_default {
        return Ok(crate::run::evaluate::BreakdownSelection::default_axes());
    }
    Ok(crate::run::evaluate::BreakdownSelection { axes })
}

/// Directories whose recorded resolved redaction policy (in `manifest.json` /
/// `results.json`) applies to the event log at `path`, for
/// [`merge_recorded_sweep_redaction`](crate::run::redact_audit::merge_recorded_sweep_redaction).
///
/// A sweep records its policy at the sweep-dir root, while the event log is
/// conventionally either inside that dir (`runs/sweep/…`) or its
/// `{dir}.events.jsonl` sibling (the documented `--output runs/sweep --event-log
/// runs/sweep.events.jsonl` layout). So probe the path itself when it is a
/// directory, and for a `*.events.jsonl` file the dir formed by stripping that
/// suffix plus the file's parent. Non-existent or manifest-less dirs are harmless:
/// the merge is best-effort and leaves the config unchanged.
fn events_recorded_config_dirs(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if path.is_dir() {
        dirs.push(path.to_path_buf());
        return dirs;
    }
    if let (Some(parent), Some(stem)) = (
        path.parent(),
        path.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".events.jsonl")),
    ) {
        dirs.push(parent.join(stem));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            dirs.push(parent.to_path_buf());
        }
    }
    dirs
}

#[allow(clippy::too_many_lines)]
fn retry_swebench_args(
    r: &args::RetryCmd,
    results: &crate::run::swebench::SweepResults,
    instance_ids_csv: &str,
) -> Result<crate::run::swebench::SwebenchArgs, Error> {
    use crate::run::dataset::DatasetSource;

    let manifest = results.manifest.as_ref();

    let mut cfg = match &r.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };
    if let Some(model) = &r.model {
        cfg.root.model.name.clone_from(model);
    } else if let Some(m) = manifest {
        cfg.root.model.name.clone_from(&m.model.name);
    }
    if let Some(v) = r.step_limit {
        cfg.root.agent.step_limit = v;
    }
    if let Some(v) = r.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(v);
    }
    if let Some(kind) = &r.env {
        cfg.root.environment.kind = parse_env_kind(kind.as_str())?;
    }
    if let Some(img) = r.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }

    let dataset_cache_dir = crate::run::dataset::default_cache_dir();

    let dataset_source = if let Some(path) = &r.dataset_path {
        // Explicit --dataset-path is relative to cwd, matching bench swebench behavior.
        DatasetSource::LocalPath(path.clone())
    } else if let Some(alias_str) = &r.dataset {
        let alias = alias_str
            .parse::<crate::run::dataset::SwebenchAlias>()
            .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
        // Reuse the manifest split when available so the correct dataset bytes
        // are used; fall back to "test" only when there is no manifest.
        let split_str = manifest
            .and_then(|m| m.dataset.split.as_deref())
            .unwrap_or("test");
        let split = split_str
            .parse::<crate::run::dataset::SwebenchSplit>()
            .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
        DatasetSource::Named { alias, split }
    } else if let Some(m) = manifest {
        if m.dataset.source_kind.as_str() == "named" {
            // When the manifest recorded an exact cache file path, use it as a
            // local path directly so the dataset is not re-downloaded when the
            // original sweep used a non-default cache location. Fall back to
            // Named (which uses the default cache dir) when cache_path is absent.
            if let Some(cp) = &m.dataset.cache_path {
                DatasetSource::LocalPath(std::path::PathBuf::from(cp))
            } else {
                let alias_str = m.dataset.alias.as_deref().unwrap_or("verified");
                let split_str = m.dataset.split.as_deref().unwrap_or("test");
                let alias = alias_str
                    .parse::<crate::run::dataset::SwebenchAlias>()
                    .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
                let split = split_str
                    .parse::<crate::run::dataset::SwebenchSplit>()
                    .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
                DatasetSource::Named { alias, split }
            }
        } else {
            // Resolve relative dataset paths recorded in the manifest
            // against the sweep directory so the retry works from any cwd.
            let recorded = std::path::PathBuf::from(&m.dataset.path);
            let resolved = if recorded.is_relative() {
                r.sweep.join(&recorded)
            } else {
                recorded
            };
            DatasetSource::LocalPath(resolved)
        }
    } else {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "bench retry: no dataset source available; pass --dataset-path or --dataset".into(),
        )));
    };

    let cfg_max_rpm = cfg.root.sweep.max_rpm;
    let cfg_max_input_tpm = cfg.root.sweep.max_input_tpm;

    Ok(crate::run::swebench::SwebenchArgs {
        dataset_source,
        dataset_cache_dir,
        output_dir: r.sweep.clone(),
        parallel: r.parallel.unwrap_or(4),
        config: cfg,
        reruns: 1,
        resume: false,
        cost_limit_usd: r.sweep_cost_limit_usd,
        task_timeout_secs: r.task_timeout_secs,
        instance_ids: Some(instance_ids_csv.to_owned()),
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: crate::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 1000,
        retry_backoff_cap_s: 60,
        retry_on_resume: false,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        config_overlay_paths: r
            .config
            .as_ref()
            .map(|p| vec![p.clone()])
            .unwrap_or_default(),
        dry_run: false,
        skip_preflight: false,
        preflight_format: "text".into(),
        skip_model_probe: false,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "sweep".into(),
        skip_patch_validation: false,
        event_log: None,
        max_rpm: cfg_max_rpm,
        max_input_tpm: cfg_max_input_tpm,
        cancel_deadline_secs: 30,
        install_os_signal_handlers: true,
        cancellation_signals: None,
        github_pr: None,
        reproduced_from: None,
        abort_on_systemic_failure: true,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
        otlp_endpoint: None,
        otlp_metrics_interval_secs: None,
        rehearse: false,
        skip_evaluator: false,
        eval_backend: "rehearsal".to_string(),
        sb_subset: None,
        sb_split: None,
        eval_timeout_secs: None,
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    })
}

fn bundle_error_to_error(err: crate::run::bundle::BundleError) -> Error {
    match err {
        crate::run::bundle::BundleError::MissingSource(message)
        | crate::run::bundle::BundleError::Schema(message)
        | crate::run::bundle::BundleError::InvalidArchive(message) => {
            Error::Config(crate::error::ConfigError::Invalid(message))
        }
        crate::run::bundle::BundleError::Io(err) => Error::Io(err),
        crate::run::bundle::BundleError::Json(err) => Error::Json(err),
        crate::run::bundle::BundleError::RedactionRetrigger { path } => Error::Config(
            crate::error::ConfigError::Invalid(format!("redaction:retrigger:{path}")),
        ),
    }
}

fn parse_env_kind(kind: &str) -> Result<crate::config::EnvKind, Error> {
    match kind {
        "local" => Ok(crate::config::EnvKind::Local),
        "docker" => Ok(crate::config::EnvKind::Docker),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "unknown --env `{other}` (expected `local` or `docker`)"
        )))),
    }
}

fn parse_network_mode(mode: &str) -> Result<crate::config::NetworkMode, Error> {
    mode.parse()
        .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))
}

/// Print a non-fatal skills-preview informational section for `bench doctor`.
/// POST a `doctor_probe` event to the webhook URL and return a short status
/// string for the non-fatal doctor output line.  Best-effort; never aborts.
#[cfg(feature = "webhook")]
async fn doctor_probe_webhook(url: &str, headers: &[(String, String)]) -> String {
    use serde_json::json;
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5));
    let mut header_map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            header_map.insert(n, v);
        }
    }
    builder = builder.default_headers(header_map);
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => return format!("client build failed: {e}"),
    };
    let payload = json!({
        "schema_version": { "major": 1, "minor": 0 },
        "sweep_id": "doctor_probe",
        "event": { "type": "doctor_probe", "sweep_id": "doctor_probe" },
        "emitted_at": chrono::Utc::now().to_rfc3339(),
    });
    match client.post(url).json(&payload).send().await {
        Ok(resp) => format!("HTTP {}", resp.status().as_u16()),
        Err(e) if e.is_timeout() => "timeout".to_owned(),
        Err(e) if e.is_connect() => "connection refused".to_owned(),
        Err(_) => "network error".to_owned(),
    }
}

fn print_doctor_skills_preview(cfg: &crate::config::Config) {
    let redactor = crate::redaction::Redactor::from_config_lossy(&cfg.root.redaction);
    println!("\n--- skills-preview (informational) ---");
    // Use a synthetic task representing the sweep intent for the informational preview.
    let tasks =
        vec!["<sweep task — run agent skills-preview --task for a specific task>".to_owned()];
    match crate::run::skills_preview::preview(&crate::run::skills_preview::SkillsPreviewArgs {
        tasks,
        config: cfg.clone(),
    }) {
        Ok(crate::run::skills_preview::PreviewResult::Disabled(msg)) => println!("{msg}"),
        Ok(crate::run::skills_preview::PreviewResult::Report(outcome)) => {
            let report = match outcome {
                crate::run::skills_preview::PreviewOutcome::Clean(r)
                | crate::run::skills_preview::PreviewOutcome::Warning(r, _) => r,
            };
            print!(
                "{}",
                crate::run::skills_preview::format_text(&report, &redactor)
            );
        }
        Err(e) => eprintln!("skills-preview error (non-fatal): {e}"),
    }
}

fn parse_dataset_source_stats(
    s: &args::DatasetStatsCmd,
) -> Result<(crate::run::dataset::DatasetSource, std::path::PathBuf), Error> {
    let cache_dir = s
        .dataset_cache_dir
        .clone()
        .unwrap_or_else(crate::run::dataset::default_cache_dir);

    match (&s.dataset_path, &s.dataset) {
        (Some(_), Some(_)) => Err(Error::Config(crate::error::ConfigError::Invalid(
            "--dataset-path and --dataset are mutually exclusive; provide only one".into(),
        ))),
        (None, None) => Err(Error::Config(crate::error::ConfigError::Invalid(
            "one of --dataset-path or --dataset is required".into(),
        ))),
        (Some(path), None) => Ok((
            crate::run::dataset::DatasetSource::LocalPath(path.clone()),
            cache_dir,
        )),
        (None, Some(alias_str)) => {
            let alias = alias_str
                .parse::<crate::run::dataset::SwebenchAlias>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            let split_str = s.split.as_deref().unwrap_or("test");
            let split = split_str
                .parse::<crate::run::dataset::SwebenchSplit>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            Ok((
                crate::run::dataset::DatasetSource::Named { alias, split },
                cache_dir,
            ))
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn compare_rehearsals(
    baseline: &std::path::Path,
    candidate: &std::path::Path,
) -> Result<(), Error> {
    println!("=== Comparing Rehearsal Results ===");
    println!("Baseline:  {}", baseline.display());
    println!("Candidate: {}", candidate.display());

    let base_results_path = baseline.join("results.json");
    let cand_results_path = candidate.join("results.json");

    if !base_results_path.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "Baseline results file not found: {}",
            base_results_path.display()
        ))));
    }
    if !cand_results_path.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "Candidate results file not found: {}",
            cand_results_path.display()
        ))));
    }

    let base_sweep: crate::run::swebench::SweepResults =
        serde_json::from_str(&std::fs::read_to_string(&base_results_path)?)
            .map_err(|e| Error::Trajectory(format!("Failed to parse baseline results: {e}")))?;
    let cand_sweep: crate::run::swebench::SweepResults =
        serde_json::from_str(&std::fs::read_to_string(&cand_results_path)?)
            .map_err(|e| Error::Trajectory(format!("Failed to parse candidate results: {e}")))?;

    let base_eval_path = baseline.join("evaluation.json");
    let cand_eval_path = candidate.join("evaluation.json");

    let base_eval: Option<crate::run::evaluate::EvaluationResults> =
        if base_eval_path.exists() {
            let text = std::fs::read_to_string(&base_eval_path)?;
            Some(serde_json::from_str(&text).map_err(|e| {
                Error::Trajectory(format!("Failed to parse baseline evaluation: {e}"))
            })?)
        } else {
            None
        };

    let cand_eval: Option<crate::run::evaluate::EvaluationResults> =
        if cand_eval_path.exists() {
            let text = std::fs::read_to_string(&cand_eval_path)?;
            Some(serde_json::from_str(&text).map_err(|e| {
                Error::Trajectory(format!("Failed to parse candidate evaluation: {e}"))
            })?)
        } else {
            None
        };

    let mut regressions = Vec::new();
    let mut drift_messages = Vec::new();

    let base_instances: std::collections::HashMap<_, _> = base_sweep
        .instances
        .iter()
        .map(|i| (&i.instance_id, i))
        .collect();
    let cand_instances: std::collections::HashMap<_, _> = cand_sweep
        .instances
        .iter()
        .map(|i| (&i.instance_id, i))
        .collect();

    for (id, base_inst) in &base_instances {
        match cand_instances.get(id) {
            None => {
                regressions.push(format!("Instance {id} is missing from candidate results."));
            }
            Some(cand_inst) => {
                if base_inst.outcome != cand_inst.outcome {
                    drift_messages.push(format!(
                        "Instance {id} outcome changed from {:?} to {:?}",
                        base_inst.outcome, cand_inst.outcome
                    ));
                    if base_inst.outcome.as_deref() == Some("submitted")
                        && cand_inst.outcome.as_deref() != Some("submitted")
                    {
                        regressions.push(format!(
                            "Instance {id} failed to submit in candidate (outcome: {:?}).",
                            cand_inst.outcome
                        ));
                    }
                }
                if base_inst.resolved_count != cand_inst.resolved_count {
                    drift_messages.push(format!(
                        "Instance {id} results resolved count changed from {} to {}",
                        base_inst.resolved_count, cand_inst.resolved_count
                    ));
                    if base_inst.resolved_count > cand_inst.resolved_count {
                        regressions.push(format!(
                            "Instance {id} results resolved count regressed from {} to {} in candidate.",
                            base_inst.resolved_count, cand_inst.resolved_count
                        ));
                    }
                }
                if base_inst.pass_at_1 != cand_inst.pass_at_1 {
                    drift_messages.push(format!(
                        "Instance {id} results pass@1 status changed from {} to {}",
                        base_inst.pass_at_1, cand_inst.pass_at_1
                    ));
                    if base_inst.pass_at_1 && !cand_inst.pass_at_1 {
                        regressions.push(format!(
                            "Instance {id} results pass@1 regressed from true to false in candidate."
                        ));
                    }
                }
            }
        }
    }

    for id in cand_instances.keys() {
        if !base_instances.contains_key(id) {
            regressions.push(format!(
                "Instance {id} is present in candidate but missing from baseline results."
            ));
        }
    }

    match (base_eval, cand_eval) {
        (Some(b_eval), Some(c_eval)) => {
            let base_eval_map: std::collections::HashMap<_, _> = b_eval
                .instances
                .iter()
                .map(|i| (&i.instance_id, i))
                .collect();
            let cand_eval_map: std::collections::HashMap<_, _> = c_eval
                .instances
                .iter()
                .map(|i| (&i.instance_id, i))
                .collect();

            for (id, base_eval_inst) in &base_eval_map {
                match cand_eval_map.get(id) {
                    Some(cand_eval_inst) => {
                        if base_eval_inst.resolved != cand_eval_inst.resolved {
                            drift_messages.push(format!(
                                "Instance {id} resolved status changed from {} to {}",
                                base_eval_inst.resolved, cand_eval_inst.resolved
                            ));
                            if base_eval_inst.resolved && !cand_eval_inst.resolved {
                                regressions.push(format!(
                                    "Instance {id} was resolved in baseline but is unresolved in candidate."
                                ));
                            }
                        }
                        if base_eval_inst.resolved_count != cand_eval_inst.resolved_count {
                            drift_messages.push(format!(
                                "Instance {id} resolved count changed from {} to {}",
                                base_eval_inst.resolved_count, cand_eval_inst.resolved_count
                            ));
                            if base_eval_inst.resolved_count > cand_eval_inst.resolved_count {
                                regressions.push(format!(
                                    "Instance {id} resolved count regressed from {} to {} in candidate.",
                                    base_eval_inst.resolved_count, cand_eval_inst.resolved_count
                                ));
                            }
                        }
                        if base_eval_inst.pass_at_1 != cand_eval_inst.pass_at_1 {
                            drift_messages.push(format!(
                                "Instance {id} pass@1 status changed from {} to {}",
                                base_eval_inst.pass_at_1, cand_eval_inst.pass_at_1
                            ));
                            if base_eval_inst.pass_at_1 && !cand_eval_inst.pass_at_1 {
                                regressions.push(format!(
                                    "Instance {id} pass@1 regressed from true to false in candidate."
                                ));
                            }
                        }
                    }
                    None => {
                        regressions.push(format!(
                            "Instance {id} is present in baseline evaluation but missing from candidate evaluation."
                        ));
                    }
                }
            }

            for id in cand_eval_map.keys() {
                if !base_eval_map.contains_key(id) {
                    regressions.push(format!(
                        "Instance {id} has evaluation in candidate but missing from baseline."
                    ));
                }
            }
        }
        (None, None) => {}
        (Some(_), None) => {
            regressions.push(
                "Baseline has evaluation data, but candidate is missing evaluation data."
                    .to_owned(),
            );
        }
        (None, Some(_)) => {
            regressions.push(
                "Candidate has evaluation data, but baseline is missing evaluation data."
                    .to_owned(),
            );
        }
    }

    println!("\n=== Comparison Summary ===");
    for msg in &drift_messages {
        println!("  [DRIFT] {msg}");
    }

    if !regressions.is_empty() {
        eprintln!("\n❌ REGRESSIONS DETECTED:");
        for reg in &regressions {
            eprintln!("  - {reg}");
        }
        return Err(Error::Trajectory(
            "Drift/regression comparison failed: regressions detected.".to_owned(),
        ));
    }

    println!("\n✅ No regressions detected between baseline and candidate.");
    Ok(())
}

// ── agent suite ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
        #![allow(clippy::unwrap_used)]
    use super::mini::mini_github_pr_options;
    use super::{
        Cli, args, cancellation_exit_code, maybe_publish_mini_github_pr, parse_verify_checks, required_github_arg, resolve_interactive_mode, swebench_args_from_cmd,
        swebench_github_pr_config, trajectory_submitted, validate_observation_head_ratio,
        validate_swebench_github_pr_args,
    };
    use crate::error::Error;
    use crate::run::github_pr::PublishMode;
    use crate::run::swebench::{CANCEL_EXIT_CODE_ESCALATED, SweepResults};
    use crate::trajectory::{Trajectory, outcome};
    use clap::Parser as _;
    use std::path::{Path, PathBuf};

    // ── doctor_probe_webhook tests (feature = "webhook") ──────────────────

    #[cfg(feature = "webhook")]
    mod webhook_doctor {
        use super::super::doctor_probe_webhook;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        async fn accept_respond(listener: &TcpListener, status: u16) {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let resp =
                format!("HTTP/1.1 {status} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = sock.write_all(resp.as_bytes()).await;
        }

        #[tokio::test]
        async fn returns_http_200_on_success() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://{addr}");
            let server = tokio::spawn(async move { accept_respond(&listener, 200).await });
            let result = doctor_probe_webhook(&url, &[]).await;
            let _ = server.await;
            assert_eq!(result, "HTTP 200");
        }

        #[tokio::test]
        async fn returns_http_401_on_non_success() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://{addr}");
            let server = tokio::spawn(async move { accept_respond(&listener, 401).await });
            let result = doctor_probe_webhook(&url, &[]).await;
            let _ = server.await;
            assert_eq!(result, "HTTP 401");
        }

        #[tokio::test]
        async fn returns_connection_refused_when_no_listener() {
            // Bind to get a free port, then drop the listener so nothing accepts.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            let url = format!("http://{addr}");
            let result = doctor_probe_webhook(&url, &[]).await;
            assert_eq!(result, "connection refused");
        }

        #[tokio::test]
        async fn passes_custom_headers() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://{addr}");
            let headers = vec![("X-Test".to_owned(), "my-value".to_owned())];
            let server = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                // Verify header was forwarded
                assert!(
                    req.contains("x-test: my-value") || req.contains("X-Test: my-value"),
                    "header not found in request: {req}"
                );
                let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(resp).await;
            });
            let result = doctor_probe_webhook(&url, &headers).await;
            let _ = server.await;
            assert_eq!(result, "HTTP 200");
        }
    }

    #[test]
    fn observation_head_ratio_accepts_closed_unit_interval() {
        assert!(validate_observation_head_ratio(0.0).is_ok());
        assert!(validate_observation_head_ratio(0.5).is_ok());
        assert!(validate_observation_head_ratio(1.0).is_ok());
    }

    #[test]
    fn observation_head_ratio_rejects_invalid_values() {
        assert!(validate_observation_head_ratio(-0.01).is_err());
        assert!(validate_observation_head_ratio(1.01).is_err());
        assert!(validate_observation_head_ratio(f64::NAN).is_err());
        assert!(validate_observation_head_ratio(f64::INFINITY).is_err());
    }

    #[test]
    fn swebench_github_pr_validation_rejects_empty_slug_branch_prefix() {
        let err = validate_swebench_github_pr_args(&args::SwebenchGithubPrArgs {
            open_prs: true,
            target_repo: Some("madmax983/maxwells-daemon".into()),
            target_branch: Some("trunk".into()),
            github_token_env: "GITHUB_TOKEN".into(),
            github_pr_dry_run: false,
            github_pr_timeout_secs: 30,
            github_pr_max_retries: 2,
            github_pr_backoff_base_ms: 250,
            github_pr_branch_prefix: "---___".into(),
        })
        .unwrap_err();

        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("branch prefix"), "{err}");
        assert!(err.to_string().contains("slug"), "{err}");
    }

    #[test]
    fn mini_github_pr_options_builds_expected_patch_capture_inputs() {
        let cfg = crate::config::Config::defaults().unwrap();
        assert!(
            mini_github_pr_options(&mini_cmd(false, false), &cfg, "task")
                .unwrap()
                .is_none()
        );

        let options = mini_github_pr_options(&mini_cmd(false, true), &cfg, "task")
            .unwrap()
            .unwrap();
        assert_eq!(options.target_repo, "madmax983/maxwells-daemon");
        assert_eq!(options.target_branch, "trunk");
        assert_eq!(options.task_id, "task");
        assert_eq!(options.patch_path, PathBuf::from("runs").join("task.patch"));
        assert_eq!(options.mode, PublishMode::DryRun);

        let open_options = mini_github_pr_options(&mini_cmd(true, false), &cfg, "task")
            .unwrap()
            .unwrap();
        assert_eq!(open_options.mode, PublishMode::Open);
    }

    #[test]
    fn build_patch_capture_spec_respects_workdir_override() {
        let cfg = crate::config::Config::defaults().unwrap();
        let m = mini_cmd(false, true);
        let github_pr = mini_github_pr_options(&m, &cfg, "task").unwrap();

        // 1. With NO workdir override, should use cfg workdir
        let spec_no_override = super::build_patch_capture_spec(
            github_pr.as_ref(),
            None,
            &cfg,
            m.skip_patch_validation,
        )
        .unwrap();
        assert_eq!(
            spec_no_override.workdir,
            PathBuf::from(&cfg.root.environment.workdir)
        );

        // 2. With workdir override, should use the override
        let override_dir = PathBuf::from("my_override_dir_xyz_789");
        let spec_override = super::build_patch_capture_spec(
            github_pr.as_ref(),
            Some(&override_dir),
            &cfg,
            m.skip_patch_validation,
        )
        .unwrap();
        assert_eq!(spec_override.workdir, override_dir);
    }

    #[test]
    fn github_pr_arg_helpers_validate_required_inputs_and_modes() {
        assert_eq!(
            required_github_arg(Some(" owner/repo "), "--target-repo").unwrap(),
            "owner/repo"
        );
        assert!(required_github_arg(None, "--target-repo").is_err());
        assert!(required_github_arg(Some("   "), "--target-repo").is_err());

        assert!(swebench_github_pr_config(&swebench_github(false, false)).is_none());
        let dry_run = swebench_github_pr_config(&swebench_github(false, true)).unwrap();
        assert_eq!(dry_run.mode, PublishMode::DryRun);
        let open = swebench_github_pr_config(&swebench_github(true, false)).unwrap();
        assert_eq!(open.mode, PublishMode::Open);

        let mut missing_repo = swebench_github(true, false);
        missing_repo.target_repo = None;
        assert!(validate_swebench_github_pr_args(&missing_repo).is_err());
        let mut missing_branch = swebench_github(true, false);
        missing_branch.target_branch = None;
        assert!(validate_swebench_github_pr_args(&missing_branch).is_err());
    }

    #[test]
    fn mini_cli_parses_invocation_time_mcp_server() {
        let cli = Cli::parse_from([
            "max",
            "mini",
            "--task",
            "Fix it",
            "--mcp-server",
            "diagnostic-mcp",
        ]);
        let crate::cli::Command::Mini(cmd) = cli.command else {
            panic!("expected mini command");
        };
        let cmd = *cmd;

        assert_eq!(cmd.mcp_servers, vec!["diagnostic-mcp"]);
    }

    #[test]
    fn mini_cli_no_bell_flag_defaults_false_and_parses() {
        // Default: flag absent.
        let cli = Cli::parse_from(["max", "mini", "--task", "t"]);
        let crate::cli::Command::Mini(cmd) = cli.command else {
            panic!("expected mini command");
        };
        assert!(!cmd.no_bell, "no_bell defaults to false");

        // Present: --no-bell sets it true.
        let cli = Cli::parse_from(["max", "mini", "--task", "t", "--no-bell"]);
        let crate::cli::Command::Mini(cmd) = cli.command else {
            panic!("expected mini command");
        };
        assert!(cmd.no_bell, "--no-bell parses to true");
    }

    #[tokio::test]
    async fn mini_github_pr_publish_helper_respects_submission_state() {
        let work = tempfile::tempdir().unwrap();
        let submitted = work.path().join("submitted.traj.json");
        write_trajectory(&submitted, Some(outcome::SUBMITTED));
        let patch = work.path().join("submitted.patch");
        std::fs::write(&patch, sample_patch()).unwrap();

        maybe_publish_mini_github_pr(
            Some(crate::run::github_pr::GithubPrOptions {
                target_repo: "madmax983/maxwells-daemon".into(),
                target_branch: "trunk".into(),
                task_id: "submitted".into(),
                trajectory_ref: submitted.display().to_string(),
                patch_path: patch,
                branch_prefix: "max".into(),
                token_env: "GITHUB_TOKEN".into(),
                mode: PublishMode::DryRun,
                timeout_secs: 30,
                max_retries: 2,
                backoff_base_ms: 250,
                redaction: crate::config::RedactionCfg::default(),
            }),
            false,
        )
        .await
        .unwrap();

        let errored = work.path().join("errored.traj.json");
        write_trajectory(&errored, Some(outcome::ERROR));
        maybe_publish_mini_github_pr(
            Some(crate::run::github_pr::GithubPrOptions {
                trajectory_ref: errored.display().to_string(),
                patch_path: work.path().join("missing.patch"),
                mode: PublishMode::DryRun,
                ..github_options_for_cli_test()
            }),
            false,
        )
        .await
        .unwrap();
        maybe_publish_mini_github_pr(None, false).await.unwrap();
    }

    #[test]
    fn trajectory_submission_detection_reads_outcome() {
        let work = tempfile::tempdir().unwrap();
        let submitted = work.path().join("submitted.traj.json");
        let errored = work.path().join("errored.traj.json");
        write_trajectory(&submitted, Some(outcome::SUBMITTED));
        write_trajectory(&errored, Some(outcome::ERROR));

        assert!(trajectory_submitted(&submitted).unwrap());
        assert!(!trajectory_submitted(&errored).unwrap());
    }

    #[test]
    fn cancellation_exit_code_only_applies_to_cancelled_sweeps() {
        let mut results = empty_sweep_results();
        assert_eq!(cancellation_exit_code(&results), None);

        results.sweep_status = crate::run::swebench::SWEEP_STATUS_CANCELLED.into();
        assert_eq!(
            cancellation_exit_code(&results),
            Some(crate::run::swebench::CANCEL_EXIT_CODE_GRACEFUL)
        );

        results.cancel_exit_code = Some(CANCEL_EXIT_CODE_ESCALATED);
        assert_eq!(cancellation_exit_code(&results), Some(137));
    }

    #[test]
    fn swebench_cli_defers_os_signal_handler_installation_to_run_loop() {
        let cli = Cli::parse_from([
            "max",
            "bench",
            "swebench",
            "--dataset-path",
            "dataset.jsonl",
            "--output",
            "runs",
        ]);
        let crate::cli::Command::Bench { cmd } = cli.command else {
            panic!("expected bench swebench command");
        };
        let args::BenchCmd::Swebench(cmd) = *cmd else {
            panic!("expected bench swebench command");
        };

        let args =
            swebench_args_from_cmd(*cmd, crate::config::Config::defaults().unwrap(), "sweep")
                .unwrap();
        assert!(args.install_os_signal_handlers);
        assert!(
            args.cancellation_signals.is_none(),
            "CLI construction must not install process-level Ctrl-C handlers before preflight"
        );
    }

    fn mini_cmd(open_pr: bool, dry_run: bool) -> args::MiniCmd {
        args::MiniCmd {
            task: Some("Fix it".into()),
            task_file: None,
            driver: crate::run::mini::RunDriver::Builtin,
            driver_append_system_prompt: false,
            driver_isolated: false,
            from_issue: None,
            from_issue_file: None,
            resume_from: None,
            resume_allow_step_bump: false,
            continue_from: None,
            continue_allow_step_bump: false,
            extra_context: None,
            model: "deterministic".into(),
            step_limit: Some(1),
            observation_max_bytes: None,
            observation_head_ratio: None,
            task_timeout_secs: None,
            per_task_budget_usd: None,
            hide_budget_from_agent: false,
            detect_stagnation: None,
            stagnation_repeat_threshold: None,
            stagnation_window: None,
            history_max_input_tokens: None,
            history_keep_last_observations: None,
            mcp_servers: Vec::new(),
            read_only: false,
            allow_mcp_in_read_only: false,
            config: None,
            workdir: None,
            env: None,
            docker_image: None,
            network_mode: None,
            output: PathBuf::from("runs"),
            trajectory_name: None,
            stream: None,
            skip_patch_validation: false,
            event_log: None,
            verify: vec![],
            verify_timeout_secs: 60,
            github_pr: args::MiniGithubPrArgs {
                open_pr,
                target_repo: Some("madmax983/maxwells-daemon".into()),
                target_branch: Some("trunk".into()),
                github_token_env: "GITHUB_TOKEN".into(),
                github_pr_dry_run: dry_run,
                github_pr_timeout_secs: 30,
                github_pr_max_retries: 2,
                github_pr_backoff_base_ms: 250,
                github_pr_branch_prefix: "max".into(),
            },
            render_only: false,
            format: "text".into(),
            interactive: false,
            yolo: false,
            ui: args::UiKind::Stderr,
            no_bell: false,
            webhook_url: None,
            webhook_headers: vec![],
            no_step_persist: false,
            chaos_fail_every: 0,
            result_format: crate::run::mini::ResultFormat::Text,
            deterministic_responses: vec![],
        }
    }

    fn empty_sweep_results() -> SweepResults {
        SweepResults {
            total: 0,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 0,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: Default::default(),
            budget_halted: 0,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            actual_cost_usd: None,
            actual_cost_source: None,
            baseline_cost_usd: None,
            baseline_cost_model: None,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: Default::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: Vec::new(),
            rate_limit_events: None,

            total_fallbacks: 0,

            model_mix: std::collections::BTreeMap::new(),
            systemic_halt_category: None,
            retry_history: vec![],
            partial: 0,
            span_export_dropped: 0,

            max_peak_memory_bytes: None,

            median_peak_memory_bytes: None,

            total_cpu_seconds: None,
        }
    }

    fn swebench_github(open_prs: bool, dry_run: bool) -> args::SwebenchGithubPrArgs {
        args::SwebenchGithubPrArgs {
            open_prs,
            target_repo: Some("madmax983/maxwells-daemon".into()),
            target_branch: Some("trunk".into()),
            github_token_env: "GITHUB_TOKEN".into(),
            github_pr_dry_run: dry_run,
            github_pr_timeout_secs: 30,
            github_pr_max_retries: 2,
            github_pr_backoff_base_ms: 250,
            github_pr_branch_prefix: "max".into(),
        }
    }

    fn github_options_for_cli_test() -> crate::run::github_pr::GithubPrOptions {
        crate::run::github_pr::GithubPrOptions {
            target_repo: "madmax983/maxwells-daemon".into(),
            target_branch: "trunk".into(),
            task_id: "task".into(),
            trajectory_ref: "traj.json".into(),
            patch_path: PathBuf::from("patch.diff"),
            branch_prefix: "max".into(),
            token_env: "GITHUB_TOKEN".into(),
            mode: PublishMode::DryRun,
            timeout_secs: 30,
            max_retries: 2,
            backoff_base_ms: 250,
            redaction: crate::config::RedactionCfg::default(),
        }
    }

    fn write_trajectory(path: &Path, outcome: Option<&str>) {
        let mut trajectory = Trajectory::new();
        trajectory.info.outcome = outcome.map(str::to_owned);
        trajectory.save_pretty(path).unwrap();
    }

    fn sample_patch() -> &'static str {
        "diff --git a/file.txt b/file.txt\n\
         --- a/file.txt\n\
         +++ b/file.txt\n\
         @@ -1 +1 @@\n\
         -base\n\
         +patched\n"
    }

    #[test]
    fn resolve_interactive_mode_off_when_neither_flag_set() {
        let m = resolve_interactive_mode(false, false, args::UiKind::Stderr);
        assert_eq!(m, crate::run::mini::InteractiveMode::Off);
    }

    #[test]
    fn resolve_interactive_mode_yolo_alone_is_status_only() {
        let m = resolve_interactive_mode(false, true, args::UiKind::Stderr);
        assert_eq!(m, crate::run::mini::InteractiveMode::YoloStatusOnly);
    }

    #[test]
    fn resolve_interactive_mode_interactive_picks_ui() {
        assert_eq!(
            resolve_interactive_mode(true, false, args::UiKind::Stderr),
            crate::run::mini::InteractiveMode::StderrPrompt
        );
        assert_eq!(
            resolve_interactive_mode(true, false, args::UiKind::Ratatui),
            crate::run::mini::InteractiveMode::Ratatui
        );
    }

    #[test]
    fn resolve_interactive_mode_yolo_overrides_interactive() {
        // `--interactive --yolo` short-circuits to status-line mode for
        // operators who want live progress but no prompts.
        assert_eq!(
            resolve_interactive_mode(true, true, args::UiKind::Stderr),
            crate::run::mini::InteractiveMode::YoloStatusOnly
        );
        assert_eq!(
            resolve_interactive_mode(true, true, args::UiKind::Ratatui),
            crate::run::mini::InteractiveMode::RatatuiMonitor
        );
    }

    #[test]
    fn resolve_interactive_mode_yolo_with_ratatui_ui_resolves_to_monitor() {
        assert_eq!(
            resolve_interactive_mode(false, true, args::UiKind::Ratatui),
            crate::run::mini::InteractiveMode::RatatuiMonitor
        );
    }

    #[test]
    fn parse_verify_checks_parses_valid_specs() {
        let checks = parse_verify_checks(&["unit-tests:cargo test -q".into()]).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "unit-tests");
        assert_eq!(checks[0].command, "cargo test -q");
    }

    #[test]
    fn parse_verify_checks_trims_whitespace() {
        let checks = parse_verify_checks(&["  lint  :  cargo clippy  ".into()]).unwrap();
        assert_eq!(checks[0].name, "lint");
        assert_eq!(checks[0].command, "cargo clippy");
    }

    #[test]
    fn parse_verify_checks_rejects_missing_colon() {
        let err = parse_verify_checks(&["no-colon-here".into()]).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("NAME:COMMAND"), "{err}");
    }

    #[test]
    fn parse_verify_checks_rejects_empty_name_or_command() {
        let err = parse_verify_checks(&[":ls".into()]).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("non-empty"), "{err}");

        let err2 = parse_verify_checks(&["test:".into()]).unwrap_err();
        assert!(matches!(err2, Error::Config(_)));
        assert!(err2.to_string().contains("non-empty"), "{err2}");
    }

    #[test]
    fn bench_dataset_stats_rejects_invalid_format() {
        let cmd = args::DatasetStatsCmd {
            dataset_path: Some(PathBuf::from("dummy.jsonl")),
            dataset: None,
            split: None,
            dataset_cache_dir: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
            runs_dir: PathBuf::from("./runs"),
            format: "jsno".to_owned(),
            model: "gpt-4".to_owned(),
        };
        let res = super::bench::bench_dataset_stats(cmd);
        assert!(res.is_err());
        let err_str = res.unwrap_err().to_string();
        assert!(
            err_str.contains("dataset-stats: unknown --format `jsno`"),
            "unexpected error string: {err_str}"
        );
    }

    #[test]
    fn test_rehearsal_output_path_normalization() {
        let test_cases = vec![
            ("out", "out.rehearsal"),
            ("out/", "out.rehearsal"),
            ("a/b/c", "a/b/c.rehearsal"),
            ("a/b/c/", "a/b/c.rehearsal"),
        ];
        for (input, expected) in test_cases {
            let mut path = PathBuf::from(input);
            if let Some(file_name) = path.file_name() {
                let name_str = file_name.to_string_lossy();
                if !name_str.ends_with(".rehearsal") {
                    let mut new_name = file_name.to_owned();
                    new_name.push(".rehearsal");
                    path.set_file_name(new_name);
                }
            }
            assert_eq!(path, PathBuf::from(expected));
        }
    }

    #[tokio::test]
    async fn test_mini_cmd_mutual_exclusivity() {
        let mut cmd = mini_cmd(false, false);
        cmd.task = Some("Fix it".into());
        cmd.from_issue = Some("owner/repo#123".into());

        let res = super::mini::mini_cmd(cmd).await;
        assert!(res.is_err());
        let err_str = res.unwrap_err().to_string();
        assert!(err_str.contains("multiple task sources provided"));
    }

    #[test]
    fn test_issue_provenance_extraction_from_trajectory() {
        use crate::trajectory::{MiniProvenanceManifest, Trajectory};

        // Case 1: Trajectory with provenance
        let mut traj = Trajectory::default();
        let manifest = MiniProvenanceManifest {
            harness_git_sha: None,
            harness_binary_version: "0.1.0".to_string(),
            started_at_utc: "2026-06-01T00:00:00Z".to_string(),
            ended_at_utc: None,
            env_kind: "local".to_string(),
            working_dir: None,
            config_sha256: "dummy".to_string(),
            config_redacted: serde_json::Value::Null,
            cli_invocation: vec![],
            extra_context_present: false,
            task_timeout_secs: None,
            step_limit: 50,
            model_name: "claude-3-5-sonnet".to_string(),
            fallback_models: vec![],
            redaction_policy_id: "dummy".to_string(),
            deterministic_mode: false,
            chaos_fail_every: 0,
            parent_sweep_run_id: None,
            issue_repo: Some("owner/repo".to_string()),
            issue_number: Some(123),
            issue_fetched_at_utc: Some("2026-06-01T00:00:00Z".to_string()),
            issue_body_sha256: Some("abcdef".to_string()),
        };
        traj.info.manifest = Some(manifest);

        let issue_provenance = traj.info.manifest.as_ref().and_then(|man| {
            if man.issue_repo.is_some()
                || man.issue_number.is_some()
                || man.issue_fetched_at_utc.is_some()
                || man.issue_body_sha256.is_some()
            {
                Some(crate::run::github_issue::IssueProvenance {
                    issue_repo: man.issue_repo.clone(),
                    issue_number: man.issue_number,
                    issue_fetched_at_utc: man.issue_fetched_at_utc.clone(),
                    issue_body_sha256: man.issue_body_sha256.clone(),
                })
            } else {
                None
            }
        });

        let prov = issue_provenance.unwrap();
        assert_eq!(prov.issue_repo, Some("owner/repo".to_string()));
        assert_eq!(prov.issue_number, Some(123));
        assert_eq!(
            prov.issue_fetched_at_utc,
            Some("2026-06-01T00:00:00Z".to_string())
        );
        assert_eq!(prov.issue_body_sha256, Some("abcdef".to_string()));

        // Case 2: Trajectory without provenance
        let mut traj_empty = Trajectory::default();
        traj_empty.info.manifest = None;

        let issue_provenance_empty = traj_empty.info.manifest.as_ref().and_then(|man| {
            if man.issue_repo.is_some()
                || man.issue_number.is_some()
                || man.issue_fetched_at_utc.is_some()
                || man.issue_body_sha256.is_some()
            {
                Some(crate::run::github_issue::IssueProvenance {
                    issue_repo: man.issue_repo.clone(),
                    issue_number: man.issue_number,
                    issue_fetched_at_utc: man.issue_fetched_at_utc.clone(),
                    issue_body_sha256: man.issue_body_sha256.clone(),
                })
            } else {
                None
            }
        });
        assert!(issue_provenance_empty.is_none());
    }

    #[test]
    fn test_bench_dataset_verify_cli_missing_reference() {
        let temp = tempfile::tempdir().unwrap();
        let cmd = args::DatasetVerifyCmd {
            dataset_path: Some(temp.path().join("candidate.jsonl")),
            dataset: Some("lite".to_string()),
            split: Some("test".to_string()),
            dataset_cache_dir: Some(temp.path().to_path_buf()),
            canonical_dir: Some(temp.path().join("missing_canonical")),
            format: "text".to_string(),
        };

        let res = super::bench::bench_dataset_verify(cmd);
        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("canonical reference for dataset alias `lite` split `test` not found")
        );
    }

    #[test]
    fn test_bench_dataset_verify_cli_clean_success() {
        let temp = tempfile::tempdir().unwrap();
        let inst = crate::run::swebench::SweBenchInstance {
            instance_id: "inst-1".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix it".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };
        let line = serde_json::to_string(&inst).unwrap() + "\n";

        let candidate_path = temp.path().join("candidate.jsonl");
        std::fs::write(&candidate_path, &line).unwrap();

        let canonical_dir = temp.path().join("canonical").join("lite");
        std::fs::create_dir_all(&canonical_dir).unwrap();
        let reference_path = canonical_dir.join("test.jsonl");
        std::fs::write(&reference_path, &line).unwrap();

        let cmd = args::DatasetVerifyCmd {
            dataset_path: Some(candidate_path),
            dataset: Some("lite".to_string()),
            split: Some("test".to_string()),
            dataset_cache_dir: Some(temp.path().to_path_buf()),
            canonical_dir: Some(temp.path().join("canonical")),
            format: "json".to_string(),
        };

        let res = super::bench::bench_dataset_verify(cmd);
        assert!(res.is_ok());
    }

    // ── bench_subset CLI tests ────────────────────────────────────────────────

    fn make_subset_dataset(temp: &tempfile::TempDir, rows: usize) -> std::path::PathBuf {
        let path = temp.path().join("dataset.jsonl");
        let mut content = String::new();
        for i in 0..rows {
            let inst = crate::run::swebench::SweBenchInstance {
                instance_id: format!("repo__{i}"),
                repo: Some("owner/repo".to_string()),
                base_commit: None,
                problem_statement: Some(format!("Fix {i}")),
                image: None,
                other: serde_json::Map::new(),
            };
            content.push_str(&serde_json::to_string(&inst).unwrap());
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn bench_subset_requires_dataset_source() {
        let temp = tempfile::tempdir().unwrap();
        let cmd = args::SubsetCmd {
            dataset_path: None,
            dataset: None,
            split: Some("test".to_owned()),
            dataset_cache_dir: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
            output: temp.path().join("out.jsonl"),
        };
        let res = super::bench::bench_subset(cmd);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("one of --dataset-path or --dataset is required"),
            "{msg}"
        );
    }

    #[test]
    fn bench_subset_rejects_mutually_exclusive_sources() {
        let temp = tempfile::tempdir().unwrap();
        let dataset_path = make_subset_dataset(&temp, 3);
        let cmd = args::SubsetCmd {
            dataset_path: Some(dataset_path),
            dataset: Some("lite".to_owned()),
            split: Some("test".to_owned()),
            dataset_cache_dir: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
            output: temp.path().join("out.jsonl"),
        };
        let res = super::bench::bench_subset(cmd);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("mutually exclusive"), "{msg}");
    }

    #[test]
    fn bench_subset_writes_jsonl_and_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let dataset_path = make_subset_dataset(&temp, 5);
        let output = temp.path().join("slice.jsonl");
        let cmd = args::SubsetCmd {
            dataset_path: Some(dataset_path),
            dataset: None,
            split: None,
            dataset_cache_dir: None,
            instance_ids: None,
            limit: Some(3),
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
            output: output.clone(),
        };
        let res = super::bench::bench_subset(cmd);
        assert!(res.is_ok(), "expected ok, got: {:?}", res.unwrap_err());

        // JSONL written and parseable
        assert!(output.exists());
        let bytes = std::fs::read(&output).unwrap();
        let loaded = crate::run::swebench::load_dataset_from_bytes_pub(&bytes).unwrap();
        assert_eq!(loaded.len(), 3);

        // Sidecar manifest exists and round-trips
        let manifest_path = crate::run::subset::manifest_path_for(&output);
        assert!(
            manifest_path.exists(),
            "manifest not found at {}",
            manifest_path.display()
        );
        let raw = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest: crate::run::subset::SubsetManifest = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            manifest.schema_version,
            crate::run::subset::MANIFEST_SCHEMA_VERSION
        );
        assert_eq!(manifest.instance_count, 3);
    }

    #[test]
    fn bench_subset_sample_larger_than_available_is_error() {
        let temp = tempfile::tempdir().unwrap();
        let dataset_path = make_subset_dataset(&temp, 3);
        let cmd = args::SubsetCmd {
            dataset_path: Some(dataset_path),
            dataset: None,
            split: None,
            dataset_cache_dir: None,
            instance_ids: None,
            limit: None,
            sample: Some(10), // larger than the 3 available instances
            seed: Some(42),
            stratify_by: None,
            stratify_mode: None,
            output: temp.path().join("out.jsonl"),
        };
        let res = super::bench::bench_subset(cmd);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("larger than the available instance count"),
            "expected 'larger than' message, got: {msg}"
        );
    }

    #[test]
    fn bench_subset_zero_rows_is_error() {
        let temp = tempfile::tempdir().unwrap();
        let dataset_path = make_subset_dataset(&temp, 3);
        // Request an instance ID that doesn't exist → zero rows
        let cmd = args::SubsetCmd {
            dataset_path: Some(dataset_path),
            dataset: None,
            split: None,
            dataset_cache_dir: None,
            instance_ids: Some("nonexistent__999".to_owned()),
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
            output: temp.path().join("out.jsonl"),
        };
        let res = super::bench::bench_subset(cmd);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        // apply_subset rejects unknown IDs
        assert!(
            msg.contains("unknown id") || msg.contains("zero instances"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn bench_subset_rejects_output_aliasing_source() {
        let temp = tempfile::tempdir().unwrap();
        let dataset_path = make_subset_dataset(&temp, 3);
        // Output == source: should be rejected before any write occurs.
        let cmd = args::SubsetCmd {
            dataset_path: Some(dataset_path.clone()),
            dataset: None,
            split: None,
            dataset_cache_dir: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
            output: dataset_path.clone(),
        };
        let original_bytes = std::fs::read(&dataset_path).unwrap();
        let res = super::bench::bench_subset(cmd);
        assert!(res.is_err(), "expected error, got ok");
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("same file as the source dataset"),
            "unexpected error: {msg}"
        );
        // Source must be untouched.
        assert_eq!(
            std::fs::read(&dataset_path).unwrap(),
            original_bytes,
            "source dataset was modified"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TailFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriageFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandStatsFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrepOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventsOutputFormat {
    Table,
    Json,
    Jsonl,
}
