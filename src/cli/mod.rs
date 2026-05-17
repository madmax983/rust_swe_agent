//! Command-line interface. `clap` derive; subcommand dispatch.

use std::io::{IsTerminal as _, Write as _};
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::error::Error;
use crate::exit_code::ExitCode;

pub mod args;

#[derive(Debug, Parser)]
#[command(
    name = "rust-swe-agent",
    version,
    about = "Measure-first SWE agent harness"
)]
/// The top-level struct parsing command line arguments.
pub struct Cli {
    /// The specific subcommand to execute.
    #[command(subcommand)]
    pub command: Command,

    /// Global log level.
    #[arg(long, default_value = "info", env = "RUST_SWE_AGENT_LOG")]
    pub log: String,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
/// The available subcommands for the application.
pub enum Command {
    /// Run one task end-to-end and write a trajectory.
    Mini(args::MiniCmd),
    /// Smoke-test: scripted model + local env writes a trajectory.
    HelloWorld(args::HelloWorldCmd),
    /// Replay an existing trajectory using a deterministic model.
    Replay(args::ReplayCmd),
    /// SWE-bench parallel sweep.
    Bench {
        /// The specific benchmarking subcommand.
        #[command(subcommand)]
        cmd: args::BenchCmd,
    },
    /// Reap any leftover `rust-swe-agent=1` labeled containers.
    Cleanup,
}

/// The main entry point for the CLI application.
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
    init_logging(&cli.log);

    match cli.command {
        Command::Mini(m) => mini_cmd(m).await,
        Command::HelloWorld(h) => {
            crate::run::hello_world::main(h.output, h.config.as_deref()).await
        }
        Command::Replay(r) => replay_cmd(r).await,
        Command::Bench {
            cmd: args::BenchCmd::Swebench(s),
        } => Box::pin(bench_swebench(s)).await,
        Command::Bench {
            cmd: args::BenchCmd::Forecast(s),
        } => Box::pin(bench_forecast(s)).await,
        Command::Bench {
            cmd: args::BenchCmd::Calibrate(c),
        } => bench_calibrate(c),
        Command::Bench {
            cmd: args::BenchCmd::Doctor(s),
        } => bench_doctor(s).await,
        Command::Bench {
            cmd: args::BenchCmd::Compare(c),
        } => bench_compare(c),
        Command::Bench {
            cmd: args::BenchCmd::Evaluate(e),
        } => bench_evaluate(e),
        Command::Bench {
            cmd: args::BenchCmd::Inspect(i),
        } => bench_inspect(i),
        Command::Bench {
            cmd: args::BenchCmd::Tail(t),
        } => bench_tail(t).await,
        Command::Bench {
            cmd: args::BenchCmd::Watch(w),
        } => bench_watch(w).await,
        Command::Bench {
            cmd: args::BenchCmd::Triage(t),
        } => bench_triage(t),
        Command::Bench {
            cmd: args::BenchCmd::CommandStats(c),
        } => bench_command_stats(c),
        Command::Bench {
            cmd: args::BenchCmd::Grep(g),
        } => bench_grep(g),
        Command::Bench {
            cmd: args::BenchCmd::Frontier(f),
        } => bench_frontier(f),
        Command::Bench {
            cmd: args::BenchCmd::Reproduce(r),
        } => bench_reproduce(r).await,
        Command::Bench {
            cmd: args::BenchCmd::Bundle(b),
        } => bench_bundle(b),
        Command::Bench {
            cmd: args::BenchCmd::Matrix(m),
        } => Box::pin(bench_matrix(m)).await,
        Command::Bench {
            cmd: args::BenchCmd::EvaluatorSelftest(s),
        } => bench_evaluator_selftest(s),
        Command::Bench {
            cmd: args::BenchCmd::Report(r),
        } => bench_report(r),
        Command::Bench {
            cmd: args::BenchCmd::Retry(r),
        } => Box::pin(bench_retry(r)).await,
        #[cfg(feature = "docker")]
        Command::Cleanup => cleanup_cmd().await,
        #[cfg(not(feature = "docker"))]
        Command::Cleanup => cleanup_cmd(),
    }
}

fn init_logging(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[allow(clippy::too_many_lines)]
async fn mini_cmd(m: args::MiniCmd) -> Result<(), Error> {
    let mut cfg = match &m.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };

    // Apply and validate all prompt-shaping overrides shared by both the
    // render-only preview path and the normal execution path. This ensures
    // that an invalid combination (e.g. --observation-head-ratio 2.0) is
    // caught even when --render-only is set, rather than blessing a config
    // that would fail on a real run.
    cfg.root.model.name.clone_from(&m.model);
    cfg.root.agent.step_limit = m.step_limit;
    if let Some(v) = m.observation_max_bytes {
        cfg.root.agent.observation_max_bytes = v;
    }
    if let Some(v) = m.observation_head_ratio {
        validate_observation_head_ratio(v)?;
        cfg.root.agent.observation_head_ratio = v;
    }
    if let Some(v) = m.detect_stagnation {
        cfg.root.agent.detect_stagnation = v;
    }
    if let Some(v) = m.stagnation_repeat_threshold {
        cfg.root.agent.stagnation_repeat_threshold = v;
    }
    if let Some(v) = m.stagnation_window {
        cfg.root.agent.stagnation_window = v;
    }
    if let Some(v) = m.history_max_input_tokens {
        cfg.root.agent.history_max_input_tokens = Some(v);
    }
    if let Some(v) = m.history_keep_last_observations {
        cfg.root.agent.history_keep_last_observations = Some(v);
    }
    if let Some(kind) = &m.env {
        cfg.root.environment.kind = parse_env_kind(kind.as_str())?;
    }
    if let Some(img) = m.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }
    apply_mcp_server_overrides(&mut cfg, &m.mcp_servers)?;

    if m.render_only {
        return mini_render_only_cmd(m, cfg);
    }

    if m.format != "text" {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--format requires --render-only; without it the agent runs normally and \
             ignoring your format setting could result in an unexpected paid model call"
                .into(),
        )));
    }

    if let Some(v) = m.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(v);
    }
    if m.hide_budget_from_agent {
        cfg.root.agent.hide_budget_from_agent = true;
    }

    let trajectory_name = m
        .trajectory_name
        .clone()
        .unwrap_or_else(|| crate::run::mini::slugify(&m.task));
    let github_pr = mini_github_pr_options(&m, &cfg, &trajectory_name)?;
    let patch_capture = github_pr
        .as_ref()
        .map(|options| crate::run::mini::PatchCaptureSpec {
            base_commit: Some(options.target_branch.clone()),
            workdir: std::path::PathBuf::from(cfg.root.environment.workdir.clone()),
            patch_path: options.patch_path.clone(),
            skip_patch_validation: m.skip_patch_validation,
        });

    let stream_addr = match &m.stream {
        Some(s) => Some(s.parse().map_err(|e: std::net::AddrParseError| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid --stream address `{s}`: {e}"
            )))
        })?),
        None => None,
    };

    let verification_checks = parse_verify_checks(&m.verify)?;
    let args = crate::run::mini::MiniArgs {
        task: m.task,
        extra_context: m.extra_context,
        config: cfg,
        output_dir: m.output,
        trajectory_name,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        task_timeout_secs: m.task_timeout_secs,
        cancellation: None,
        stream_addr,
        patch_capture,
        verification_checks,
        verification_timeout_secs: m.verify_timeout_secs,
    };
    let run_result = crate::run::mini::run(args).await;
    // Only publish when the run succeeded or failed at verification — those are
    // the two cases where the trajectory and patch are guaranteed on disk.
    // For other errors (env setup, model API, pre-trajectory I/O) propagate
    // immediately so the real failure isn't masked by a trajectory-read error.
    let is_verification_failure =
        matches!(run_result, Err(crate::error::Error::VerificationFailed(..)));
    if run_result.is_ok() || is_verification_failure {
        maybe_publish_mini_github_pr(github_pr).await?;
    }
    run_result?;
    Ok(())
}

fn mini_render_only_cmd(m: args::MiniCmd, cfg: crate::config::Config) -> Result<(), Error> {
    crate::run::render_only::reject_incompatible_flags(
        &crate::run::render_only::IncompatibleFlags {
            per_task_budget_usd: m.per_task_budget_usd,
            task_timeout_secs: m.task_timeout_secs,
            stream: m.stream.as_deref(),
            has_verify_checks: !m.verify.is_empty(),
            open_pr: m.github_pr.open_pr,
            pr_dry_run: m.github_pr.github_pr_dry_run,
        },
    )?;

    let args = crate::run::render_only::RenderOnlyArgs {
        task: m.task,
        extra_context: m.extra_context,
        config: cfg,
    };
    let report = crate::run::render_only::render(args)?;

    match m.format.as_str() {
        "json" => {
            let json = serde_json::to_string_pretty(&report).map_err(Error::Json)?;
            println!("{json}");
        }
        "text" => {
            print!("{}", crate::run::render_only::format_text(&report));
        }
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--format '{other}' is not valid for --render-only; use 'text' or 'json'"
            ))));
        }
    }
    Ok(())
}

fn bench_swebench_render_only(s: &args::SwebenchCmd) -> Result<(), Error> {
    crate::run::render_only::reject_incompatible_flags(
        &crate::run::render_only::IncompatibleFlags {
            per_task_budget_usd: s.per_task_budget_usd,
            task_timeout_secs: s.task_timeout_secs,
            stream: None,
            has_verify_checks: false,
            open_pr: s.github_pr.open_prs,
            pr_dry_run: s.github_pr.github_pr_dry_run,
        },
    )?;
    let format = s.format.clone();
    let cfg = swebench_config_from_cmd(s)?;
    let (dataset_source, dataset_cache_dir) = parse_dataset_source(s)?;
    let (dataset_bytes, _meta) =
        crate::run::dataset::resolve_dataset(&dataset_source, &dataset_cache_dir)?;
    let instances = crate::run::swebench::load_dataset_from_bytes_pub(&dataset_bytes)?;

    let stratify_by = s.stratify_by.map(|v| match v {
        args::StratifyByArg::Repo => crate::run::swebench::StratifyBy::Repo,
    });
    let stratify_mode = match s
        .stratify_mode
        .unwrap_or(args::StratifyModeArg::Proportional)
    {
        args::StratifyModeArg::Proportional => crate::run::swebench::StratifyMode::Proportional,
        args::StratifyModeArg::Balanced => crate::run::swebench::StratifyMode::Balanced,
    };

    let (instances, _filter_spec) = crate::run::swebench::apply_subset(
        instances,
        &crate::run::swebench::ApplySubsetParams {
            instance_ids_arg: s.instance_ids.as_deref(),
            limit: s.limit.or(Some(1)),
            sample: s.sample,
            seed: s.seed,
            stratify_by,
            stratify_mode,
        },
    )?;

    let instance = instances.into_iter().next().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "--render-only: dataset produced zero instances after filtering".into(),
        ))
    })?;

    let task = instance.problem_statement.unwrap_or_default();
    let render_args = crate::run::render_only::RenderOnlyArgs {
        task,
        extra_context: None,
        config: cfg,
    };
    let report = crate::run::render_only::render(render_args)?;

    match format.as_str() {
        "json" => {
            let json = serde_json::to_string_pretty(&report).map_err(Error::Json)?;
            println!("{json}");
        }
        "text" => {
            print!("{}", crate::run::render_only::format_text(&report));
        }
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--format '{other}' is not valid for --render-only; use 'text' or 'json'"
            ))));
        }
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

async fn bench_swebench(s: args::SwebenchCmd) -> Result<(), Error> {
    let mut sweep_cmd = s;

    if sweep_cmd.render_only {
        return bench_swebench_render_only(&sweep_cmd);
    }

    validate_swebench_github_pr_args(&sweep_cmd.github_pr)?;
    if sweep_cmd.forecast_first {
        match Box::pin(run_forecast_from_cmd(sweep_cmd.clone())).await? {
            crate::run::forecast::ForecastOutcome::Report(report) => {
                print_forecast_report(&report, &sweep_cmd.format)?;
                if let Err(e) =
                    crate::run::forecast::validate_fail_over_cap(&report, sweep_cmd.fail_over_cap)
                {
                    exit_with_outcome(ExitCode::BudgetHalt, &e.to_string());
                }
                if !crate::run::forecast::forecast_gate_allows_sweep(
                    &report,
                    crate::run::forecast::ForecastGate { yes: sweep_cmd.yes },
                )? {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "forecast-first blocked sweep: {} (pass --yes to proceed anyway)",
                        report.threshold.message
                    ))));
                }
            }
            crate::run::forecast::ForecastOutcome::DryRun(results)
            | crate::run::forecast::ForecastOutcome::Cancelled(results) => {
                print_dry_run_summary(&results, &sweep_cmd.format);
                exit_if_cancelled_sweep(&results);
                return Ok(());
            }
        }
        if sweep_cmd.sample.is_none() {
            sweep_cmd.seed = None;
        }
    }

    let cfg = swebench_config_from_cmd(&sweep_cmd)?;
    let preflight_mode = if sweep_cmd.dry_run {
        "dry_run"
    } else {
        "sweep"
    };
    let results =
        crate::run::swebench::run(swebench_args_from_cmd(sweep_cmd, cfg, preflight_mode)?).await?;

    tracing::info!(
        total = results.total,
        submitted = results.submitted,
        skipped = results.skipped,
        errored = results.errored,
        budget_halted = results.budget_halted,
        retries = results.retries,
        retried_instances = results.retried_instances,
        prompt_tokens = results.total_prompt_tokens,
        completion_tokens = results.total_completion_tokens,
        estimated_cost_usd = results.estimated_cost_usd,
        cost_limit_usd = ?results.cost_limit_usd,
        "sweep complete"
    );
    print!("{}", results.summary_table());
    exit_if_cancelled_sweep(&results);
    exit_if_systemic_halt_sweep(&results);
    if results.budget_halted > 0 && results.cost_limit_usd.is_some() {
        exit_with_outcome(
            ExitCode::BudgetHalt,
            &format!(
                "sweep stopped early: {} task(s) were not dispatched because the sweep cost limit was reached",
                results.budget_halted
            ),
        );
    }
    let github_pr_failures = github_pr_failure_count(&results);
    if github_pr_failures > 0 {
        return Err(Error::Github(format!(
            "{github_pr_failures} GitHub PR publication(s) failed; see results.json for instance errors"
        )));
    }
    Ok(())
}

async fn bench_doctor(mut s: args::SwebenchCmd) -> Result<(), Error> {
    if s.render_only {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--render-only is not supported for `bench doctor`; \
             it only applies to `bench swebench`"
                .into(),
        )));
    }
    s.dry_run = true;
    let output_format = s.format.clone();
    let cfg = swebench_config_from_cmd(&s)?;
    let results = crate::run::swebench::run(swebench_args_from_cmd(s, cfg, "doctor")?).await?;
    if output_format != "json" {
        print!("{}", results.summary_table());
    }
    exit_if_cancelled_sweep(&results);
    Ok(())
}

async fn bench_forecast(s: args::SwebenchCmd) -> Result<(), Error> {
    if s.render_only {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--render-only is not supported for `bench forecast`; \
             it only applies to `bench swebench`"
                .into(),
        )));
    }
    let output_format = s.format.clone();
    let fail_over_cap = s.fail_over_cap;
    match Box::pin(run_forecast_from_cmd(s)).await? {
        crate::run::forecast::ForecastOutcome::Report(report) => {
            print_forecast_report(&report, &output_format)?;
            if let Err(e) = crate::run::forecast::validate_fail_over_cap(&report, fail_over_cap) {
                exit_with_outcome(ExitCode::BudgetHalt, &e.to_string());
            }
            Ok(())
        }
        crate::run::forecast::ForecastOutcome::DryRun(results)
        | crate::run::forecast::ForecastOutcome::Cancelled(results) => {
            print_dry_run_summary(&results, &output_format);
            exit_if_cancelled_sweep(&results);
            Ok(())
        }
    }
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

fn bench_calibrate(c: args::CalibrateCmd) -> Result<(), Error> {
    let report = crate::run::calibrate::compute(&crate::run::calibrate::CalibrationArgs {
        forecast_path: c.forecast.clone(),
        results_path: c.results.clone(),
    })?;
    let json = crate::run::calibrate::to_json(&report)?;
    let output_path = c.output.unwrap_or_else(|| {
        c.results.parent().map_or_else(
            || std::path::PathBuf::from("calibration.json"),
            |parent| parent.join("calibration.json"),
        )
    });
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&output_path, &json)?;

    match c.format.as_str() {
        "text" => print!("{}", crate::run::calibrate::render_text(&report)),
        "json" => println!("{json}"),
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    }

    if c.fail_on_optimistic
        && report.verdict == crate::run::calibrate::CalibrationVerdict::Optimistic
    {
        exit_with_outcome(
            ExitCode::CalibrationOptimistic,
            "calibration verdict is optimistic",
        );
    }
    Ok(())
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

fn mini_github_pr_options(
    m: &args::MiniCmd,
    cfg: &Config,
    trajectory_name: &str,
) -> Result<Option<crate::run::github_pr::GithubPrOptions>, Error> {
    if !m.github_pr.open_pr && !m.github_pr.github_pr_dry_run {
        return Ok(None);
    }
    let target_repo = required_github_arg(m.github_pr.target_repo.as_deref(), "--target-repo")?;
    let target_branch =
        required_github_arg(m.github_pr.target_branch.as_deref(), "--target-branch")?;
    crate::run::github_pr::validate_branch_prefix(&m.github_pr.github_pr_branch_prefix)
        .map_err(Error::Config)?;
    let patch_path = m.output.join(format!("{trajectory_name}.patch"));
    let trajectory_path = m.output.join(format!("{trajectory_name}.traj.json"));
    Ok(Some(crate::run::github_pr::GithubPrOptions {
        target_repo,
        target_branch,
        task_id: trajectory_name.to_owned(),
        trajectory_ref: trajectory_path.display().to_string(),
        patch_path,
        branch_prefix: m.github_pr.github_pr_branch_prefix.clone(),
        token_env: m.github_pr.github_token_env.clone(),
        mode: if m.github_pr.github_pr_dry_run {
            crate::run::github_pr::PublishMode::DryRun
        } else {
            crate::run::github_pr::PublishMode::Open
        },
        timeout_secs: m.github_pr.github_pr_timeout_secs,
        max_retries: m.github_pr.github_pr_max_retries,
        backoff_base_ms: m.github_pr.github_pr_backoff_base_ms,
        redaction: cfg.root.redaction.clone(),
    }))
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

async fn publish_github_pr(options: crate::run::github_pr::GithubPrOptions) -> Result<(), Error> {
    let result = crate::run::github_pr::publish(options).await?;
    if let Some(output) = result.dry_run_output {
        print!("{output}");
    } else if let Some(url) = result.url {
        println!("github_pr_url: {url}");
    }
    Ok(())
}

async fn maybe_publish_mini_github_pr(
    github_pr: Option<crate::run::github_pr::GithubPrOptions>,
) -> Result<(), Error> {
    if let Some(options) = github_pr {
        let traj_path = options.trajectory_ref.clone();
        if trajectory_submitted(std::path::Path::new(&traj_path))? {
            publish_github_pr(options).await?;
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
    })
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

fn bench_compare(c: args::CompareCmd) -> Result<(), Error> {
    if c.inspect_diff.is_some() && c.emit_diff_script.is_some() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "compare: pass only one of --inspect-diff or --emit-diff-script".into(),
        )));
    }

    if let Some(instance_id) = c.inspect_diff {
        let format = parse_trajectory_diff_format(&c.format)?;
        let report = crate::run::trajectory_diff::diff_sweep_instance(
            &c.baseline,
            &c.candidate,
            &instance_id,
            c.show_noise,
        )?;
        print_trajectory_diff(&report, format)?;
        return Ok(());
    }

    let format = parse_compare_format(&c.format)?;
    let breakdown = parse_breakdown_selection(&c.breakdown, false)?;
    let report = crate::run::compare::compute(&crate::run::compare::CompareArgs {
        baseline: c.baseline.clone(),
        candidate: c.candidate.clone(),
        format,
        max_regressions: c.max_regressions,
        max_patch_size_regression_pct: c.max_patch_size_regression,
        breakdown,
        min_delta_pp: c.breakdown_min_delta_pp / 100.0,
        cost_attribution: matches!(c.cost_attribution, args::OnOffArg::On),
        cost_attribution_min_delta_usd: c.cost_attribution_min_delta_usd,
        min_significance: c.min_significance,
        regression_significance: c.regression_significance,
        allow_underpowered: c.allow_underpowered,
    })?;
    match format {
        crate::run::compare::CompareFormat::Text => print!("{}", report.human_table()),
        crate::run::compare::CompareFormat::Json => {
            println!("{}", report.to_json_pretty()?);
        }
    }
    if let Some(path) = c.emit_diff_script {
        crate::run::compare::write_diff_script(&report, &path)?;
    }
    if let Some(max) = c.max_regressions {
        if report.regression_count() > max
            && report.verdict == crate::run::compare::CompareVerdict::Regression
        {
            tracing::error!(
                regressions = report.regression_count(),
                max = max,
                ci_lower = report.resolved_delta_ci95.lower,
                ci_upper = report.resolved_delta_ci95.upper,
                "compare: regression count exceeds --max-regressions threshold"
            );
            exit_with_outcome(
                ExitCode::RegressionGateFailure,
                &format!(
                    "compare: {} regression(s) exceed --max-regressions={}",
                    report.regression_count(),
                    max
                ),
            );
        }
    }
    if let Some(max) = c.max_patch_size_regression {
        if report.patch_size_regression_exceeds(max) {
            tracing::error!(
                max_pct = max,
                baseline_mean_lines_changed = report.baseline_mean_lines_changed,
                candidate_mean_lines_changed = report.candidate_mean_lines_changed,
                "compare: patch size regression exceeds --max-patch-size-regression threshold"
            );
            exit_with_outcome(
                ExitCode::RegressionGateFailure,
                &format!(
                    "compare: patch size regression exceeds --max-patch-size-regression={max}%"
                ),
            );
        }
    }
    apply_significance_gates(
        &report,
        c.min_significance,
        c.regression_significance,
        c.allow_underpowered,
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

fn bench_evaluate(e: args::EvaluateCmd) -> Result<(), Error> {
    let backend = match e.backend.as_str() {
        "sb-cli" => crate::run::evaluate::EvaluateBackend::SbCli,
        "none" => crate::run::evaluate::EvaluateBackend::None,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --backend `{other}` (expected `sb-cli` or `none`)"
            ))));
        }
    };

    let breakdown = parse_breakdown_selection(&e.breakdown, true)?;
    let args = crate::run::evaluate::EvaluateArgs {
        sweep_dir: e.sweep.clone(),
        dataset_path: e.dataset,
        backend,
        timeout_per_instance_secs: e.timeout_per_instance,
        parallel: e.parallel,
        sb_subset: e.sb_subset,
        sb_split: e.sb_split,
        run_id: e.run_id,
        breakdown,
        cost_attribution: matches!(e.cost_attribution, args::OnOffArg::On),
    };
    let eval = crate::run::evaluate::run(&args)?;
    let loaded_sweep = crate::run::compare::load_sweep(&e.sweep)?;
    let summary = crate::run::evaluate::summarize_with_model(
        &eval,
        &loaded_sweep.instances,
        loaded_sweep
            .manifest
            .as_ref()
            .map(|m| m.model.name.as_str()),
    );

    tracing::info!(
        instances = summary.instances,
        resolved = summary.resolved,
        resolved_rate = summary.resolved_rate,
        pass_at_1 = summary.pass_at_1,
        pass_at_k = summary.pass_at_k,
        total_input_tokens = summary.total_input_tokens,
        total_cache_read_tokens = summary.total_cache_read_tokens,
        total_cache_creation_tokens = summary.total_cache_creation_tokens,
        total_completion_tokens = summary.total_completion_tokens,
        total_cost_usd = summary.total_cost_usd,
        cache_hit_rate = summary.cache_hit_rate,
        evaluation_path = %crate::run::evaluate::evaluation_path(&e.sweep).display(),
        "evaluation complete"
    );
    print!("{}", crate::run::evaluate::render_summary_table(&summary));
    if let Some(latency) = &eval.latency_summary {
        print!("{}", crate::run::evaluate::render_latency_summary(latency));
    }
    let elision_text = crate::run::evaluate::render_elision_stats(&eval.behavioral);
    if !elision_text.is_empty() {
        print!("{elision_text}");
    }
    if let Some(prov) = &eval.provenance {
        println!(
            "evaluator_provenance: backend={} subset={} split={}",
            prov.backend,
            prov.dataset_subset.as_deref().unwrap_or("?"),
            prov.dataset_split.as_deref().unwrap_or("?"),
        );
    }
    if let Some(rl) = &loaded_sweep.rate_limit_events {
        println!("rate_limit_throttled_calls: {}", rl.throttled_calls);
        println!(
            "rate_limit_throttled_secs: {:.1}",
            rl.total_throttled_seconds
        );
        println!("rate_limit_peak_concurrent: {}", rl.peak_concurrent);
        if let Some(rpm) = rl.configured_max_rpm {
            println!("rate_limit_configured_max_rpm: {rpm}");
        }
        if let Some(tpm) = rl.configured_max_input_tpm {
            println!("rate_limit_configured_max_input_tpm: {tpm}");
        }
    }
    if !eval.breakdown.is_empty() {
        print!(
            "{}",
            crate::run::evaluate::render_breakdown_table(&eval.breakdown)
        );
    }
    if !eval.cost_attribution.is_empty() {
        let missing_cost_count = crate::run::evaluate::cost_missing_count_for_run_slots(
            &e.sweep,
            &loaded_sweep.instances,
        )?;
        if missing_cost_count > 0 {
            println!(
                "warning: cost attribution missing usd_cost for {missing_cost_count} trajectories; treating as $0.00"
            );
        }
        print!(
            "{}",
            crate::run::evaluate::render_cost_attribution_table(&eval.cost_attribution)
        );
    }
    Ok(())
}

async fn bench_reproduce(r: args::ReproduceCmd) -> Result<(), Error> {
    use crate::run::reproduce::{
        compare_manifests, filter_hard_drifts, load_manifest_from_sweep, render_summary,
        write_report,
    };

    // Load the source manifest.
    let source_manifest = load_manifest_from_sweep(&r.from)?;

    // Build a "current" manifest from the CLI environment to detect drift.
    let current_manifest = build_current_manifest_for_reproduce(&source_manifest);

    let all_drifts = compare_manifests(&source_manifest, &current_manifest);

    // Report soft drifts as warnings.
    for d in all_drifts
        .iter()
        .filter(|d| d.severity == crate::run::reproduce::DriftSeverity::Soft)
    {
        tracing::warn!(field = %d.field, "reproduce: soft drift — {}", d.message);
    }

    // Abort on unwhitelisted hard drifts.
    let hard_blocking = filter_hard_drifts(&all_drifts, &r.allow_drift);
    if !hard_blocking.is_empty() {
        let reasons: Vec<String> = hard_blocking.iter().map(|d| d.message.clone()).collect();
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "reproduce: hard drift detected (use --allow-drift to whitelist):\n  {}",
            reasons.join("\n  ")
        ))));
    }

    // Reject output that aliases the source sweep — overwriting it would corrupt
    // the original artifacts and make patch comparison compare files against
    // themselves.
    let from_canon = std::fs::canonicalize(&r.from).unwrap_or_else(|_| r.from.clone());
    let out_canon = std::fs::canonicalize(&r.output).unwrap_or_else(|_| r.output.clone());
    if from_canon == out_canon {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "reproduce: --output must differ from --from; \
             writing replay results into the source sweep directory would overwrite the original artifacts"
                .into(),
        )));
    }

    // Load original results to replay.
    let source_results = load_sweep_results(&r.from)?;
    // Snapshot all original instances; will be narrowed to the replayed subset
    // after the sweep runs so partial replays (--filter / --limit) don't count
    // un-requested instances as errors.
    let all_original_instances = source_results.instances.clone();

    // Compute source manifest hash for provenance.
    let source_manifest_hash = hash_manifest(&source_manifest);

    if r.limit == Some(0) {
        let report = crate::run::reproduce::build_reproducibility_report(
            &r.from,
            source_manifest_hash,
            &[],
            &[],
            &r.output,
        );
        std::fs::create_dir_all(&r.output).map_err(Error::Io)?;
        write_report(&report, &r.output)?;
        print!("{}", render_summary(&report));
        return Ok(());
    }

    // Build the swebench args from the source manifest, applying any overrides.
    let sweep_args =
        reproduce_swebench_args(&r, &source_manifest, &source_results, &source_manifest_hash)?;

    // Run the replay sweep.
    let replay_results = crate::run::swebench::run(sweep_args).await?;

    // For partial replays (--filter / --limit), restrict original instances to
    // those actually present in the replay so skipped instances aren't counted
    // as errors in the report.
    let replayed_ids: std::collections::HashSet<&str> = replay_results
        .instances
        .iter()
        .map(|i| i.instance_id.as_str())
        .collect();
    let original_instances: Vec<_> = all_original_instances
        .iter()
        .filter(|i| replayed_ids.contains(i.instance_id.as_str()))
        .cloned()
        .collect();

    // Build and write the reproducibility report.
    let report = crate::run::reproduce::build_reproducibility_report(
        &r.from,
        source_manifest_hash,
        &original_instances,
        &replay_results.instances,
        &r.output,
    );

    std::fs::create_dir_all(&r.output).map_err(Error::Io)?;
    write_report(&report, &r.output)?;

    print!("{}", render_summary(&report));

    Ok(())
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

fn reproduce_swebench_args(
    r: &args::ReproduceCmd,
    manifest: &crate::run::swebench::ProvenanceManifest,
    source_results: &crate::run::swebench::SweepResults,
    source_manifest_hash: &str,
) -> Result<crate::run::swebench::SwebenchArgs, Error> {
    use crate::run::dataset::DatasetSource;

    let mut cfg = Config::defaults()?;
    cfg.root.model.name.clone_from(&manifest.model.name);

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

fn bench_frontier(f: args::FrontierCmd) -> Result<(), Error> {
    let format = f.format;
    let report =
        crate::run::frontier::compute(&crate::run::frontier::FrontierArgs { dirs: f.dirs })?;
    match format {
        crate::run::frontier::FrontierFormat::Text => {
            print!("{}", crate::run::frontier::render_text(&report));
        }
        crate::run::frontier::FrontierFormat::Json => {
            println!("{}", crate::run::frontier::render_json(&report));
        }
    }
    Ok(())
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

fn bench_inspect(i: args::InspectCmd) -> Result<(), Error> {
    if !i.diff.is_empty() {
        if i.instance.is_some() || i.filter.is_some() || i.sweep.is_some() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "inspect: --diff cannot be combined with --sweep, --instance, or --filter".into(),
            )));
        }
        if i.diff.len() != 2 {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "inspect: --diff expects exactly two trajectory paths".into(),
            )));
        }
        let format = parse_trajectory_diff_format(&i.format)?;
        let report = crate::run::trajectory_diff::diff_paths(
            &crate::run::trajectory_diff::TrajectoryDiffArgs {
                baseline: i.diff[0].clone(),
                candidate: i.diff[1].clone(),
                show_noise: i.show_noise,
            },
        )?;
        print_trajectory_diff(&report, format)?;
        return Ok(());
    }

    let format = match i.format.as_str() {
        "text" => crate::run::inspect::InspectFormat::Text,
        "json" => crate::run::inspect::InspectFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let sweep = i.sweep.ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "inspect: --sweep is required unless --diff is used".into(),
        ))
    })?;
    let out = crate::run::inspect::run(&crate::run::inspect::InspectArgs {
        sweep,
        instance: i.instance,
        filter: i.filter,
        full: i.full,
        show_expected: i.show_expected,
    })?;
    match format {
        crate::run::inspect::InspectFormat::Text => {
            print!("{}", crate::run::inspect::render_text(&out));
        }
        crate::run::inspect::InspectFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
    }
    Ok(())
}

fn bench_command_stats(c: args::CommandStatsCmd) -> Result<(), Error> {
    let format = match c.format.as_str() {
        "text" => CommandStatsFormat::Text,
        "json" => CommandStatsFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let report = crate::run::command_stats::run(&crate::run::command_stats::CommandStatsArgs {
        sweep_dir: c.sweep,
        bucket: c.bucket,
        min_invocations: c.min_invocations,
        top: c.top,
        compare: c.compare,
        filter: c.filter,
    })?;
    match format {
        CommandStatsFormat::Text => {
            print!("{}", crate::run::command_stats::render_text(&report, c.top));
        }
        CommandStatsFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

fn bench_grep(g: args::GrepCmd) -> Result<(), Error> {
    let format = match g.format.as_str() {
        "text" => GrepOutputFormat::Text,
        "json" => GrepOutputFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let instance_ids = g.instance_ids.as_deref().map(|s| {
        s.split(',')
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>()
    });
    let exclude_instance_ids = g.exclude_instance_ids.as_deref().map(|s| {
        s.split(',')
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>()
    });
    let report = crate::run::grep::run(&crate::run::grep::GrepArgs {
        sweep_dir: g.sweep,
        pattern: g.pattern,
        roles: g.roles,
        field: g.field,
        instance_ids,
        exclude_instance_ids,
        outcomes: g.outcomes,
        context_chars: g.context,
        max_matches_per_instance: g.max_matches_per_instance,
    })?;
    let has_matches = !report.matches.is_empty();
    match format {
        GrepOutputFormat::Text => {
            print!("{}", crate::run::grep::render_text(&report));
        }
        GrepOutputFormat::Json => {
            let lines = crate::run::grep::render_json_lines(&report)?;
            if !lines.is_empty() {
                println!("{lines}");
            }
        }
    }
    if !has_matches {
        // Exit 1 = no matches found (grep convention; AC requires this specific code).
        std::process::exit(1);
    }
    Ok(())
}

fn bench_triage(t: args::TriageCmd) -> Result<(), Error> {
    let format = match t.format.as_str() {
        "text" => TriageFormat::Text,
        "json" => TriageFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let report = crate::run::triage::run(&crate::run::triage::TriageArgs {
        sweep_dir: t.sweep,
        bucket: t.bucket,
        min_cluster_size: t.min_cluster_size,
        top: t.top,
    })?;
    match format {
        TriageFormat::Text => {
            print!("{}", crate::run::triage::render_text(&report, t.top));
            Ok(())
        }
        TriageFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

fn bench_bundle(b: args::BundleCmd) -> Result<(), Error> {
    if let Some(archive) = b.verify {
        let report = crate::run::bundle::verify_bundle(&archive).map_err(bundle_error_to_error)?;
        if report.problems.is_empty() {
            println!("bundle:ok");
            return Ok(());
        }
        for problem in report.problems {
            println!("{problem}");
        }
        exit_with_outcome(ExitCode::VerificationFailure, "bundle verification failed");
    }

    let sweep = b.sweep.ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "bundle: --sweep is required unless --verify is used".into(),
        ))
    })?;
    let output = b.output.ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "bundle: --output is required when --sweep is used".into(),
        ))
    })?;
    match crate::run::bundle::create_bundle(&crate::run::bundle::BundleCreateArgs {
        sweep_dir: sweep,
        output_path: output,
        instance: b.instance,
    }) {
        Ok(report) => {
            println!(
                "bundle:{} files={}",
                report.output_path.display(),
                report.files.len()
            );
            Ok(())
        }
        Err(crate::run::bundle::BundleError::RedactionRetrigger { path }) => {
            println!("redaction:retrigger:{path}");
            exit_with_outcome(
                ExitCode::VerificationFailure,
                "bundle redaction retriggered",
            );
        }
        Err(err) => Err(bundle_error_to_error(err)),
    }
}

#[allow(clippy::too_many_lines)]
async fn bench_matrix(m: args::MatrixCmd) -> Result<(), Error> {
    let cache_dir = m
        .dataset_cache_dir
        .clone()
        .unwrap_or_else(crate::run::dataset::default_cache_dir);

    let dataset_source = match (&m.dataset_path, &m.dataset) {
        (Some(_), Some(_)) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "--dataset-path and --dataset are mutually exclusive; provide only one".into(),
            )));
        }
        (None, None) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "one of --dataset-path or --dataset is required".into(),
            )));
        }
        (Some(path), None) => crate::run::dataset::DatasetSource::LocalPath(path.clone()),
        (None, Some(alias_str)) => {
            let alias = alias_str
                .parse::<crate::run::dataset::SwebenchAlias>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            let split_str = m.split.as_deref().unwrap_or("test");
            let split = split_str
                .parse::<crate::run::dataset::SwebenchSplit>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            crate::run::dataset::DatasetSource::Named { alias, split }
        }
    };

    let matrix_args = crate::run::matrix::MatrixArgs {
        config_path: m.config,
        dataset_source,
        dataset_cache_dir: cache_dir,
        output_dir: m.output,
        instance_ids: m.instance_ids,
        limit: m.limit,
        sample: m.sample,
        seed: m.seed,
        stratify_by: m.stratify_by.map(|v| match v {
            args::StratifyByArg::Repo => crate::run::swebench::StratifyBy::Repo,
        }),
        stratify_mode: match m
            .stratify_mode
            .unwrap_or(args::StratifyModeArg::Proportional)
        {
            args::StratifyModeArg::Proportional => crate::run::swebench::StratifyMode::Proportional,
            args::StratifyModeArg::Balanced => crate::run::swebench::StratifyMode::Balanced,
        },
        sweep_cost_limit_usd: m.sweep_cost_limit_usd,
        matrix_parallelism: m.matrix_parallelism,
        resume: m.resume,
        parallel: m.parallel,
        skip_preflight: m.skip_preflight,
        skip_model_probe: m.skip_model_probe,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        cancel_deadline_secs: m.cancel_deadline_secs,
        install_os_signal_handlers: true,
    };

    let summary = crate::run::matrix::run(matrix_args).await?;

    let mut table = comfy_table::Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_header(vec![
            "Rank", "Name", "Model", "State", "Resolved", "Cost($)",
        ]);
    for arm in &summary.arms {
        table.add_row(vec![
            arm.rank.to_string(),
            arm.name.clone(),
            arm.model.clone(),
            arm.state.clone(),
            arm.resolved.to_string(),
            format!("{:.4}", arm.total_cost_usd),
        ]);
    }
    println!("=== bench matrix ===\n{table}");
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn bench_evaluator_selftest(s: args::EvaluatorSelftestCmd) -> Result<(), Error> {
    let selftest_args = crate::run::evaluator_selftest::SelftestArgs {
        dataset_path: s.dataset_path,
        output_dir: s.output,
        instance_ids: s.instance_ids,
        limit: s.limit,
        sample: s.sample,
        seed: s.seed,
        format: s.format,
        backend: s.backend,
        sb_subset: s.sb_subset,
        sb_split: s.sb_split,
        timeout_per_instance: s.timeout_per_instance,
        parallel: s.parallel,
    };
    let result = crate::run::evaluator_selftest::run(selftest_args);
    print!("{}", result.stdout);
    let code = result.exit_status.as_exit_code();
    if code != 0 {
        exit_with_outcome(
            match result.exit_status {
                crate::run::evaluator_selftest::SelftestExitStatus::AllResolved => {
                    ExitCode::Success
                }
                crate::run::evaluator_selftest::SelftestExitStatus::HasUnresolved => {
                    ExitCode::TaskUnsuccessful
                }
                crate::run::evaluator_selftest::SelftestExitStatus::HasErrored => {
                    ExitCode::PreflightFailure
                }
            },
            "evaluator self-test: not all instances resolved",
        );
    }
    Ok(())
}

fn bench_report(r: args::ReportCmd) -> Result<(), Error> {
    let format = match r.format.as_str() {
        "markdown" | "md" => crate::run::report::ReportFormat::Markdown,
        "html" => crate::run::report::ReportFormat::Html,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `markdown` or `html`)"
            ))));
        }
    };
    crate::run::report::run(&crate::run::report::ReportArgs {
        sweep_dir: r.sweep,
        output: r.output,
        baseline: r.baseline,
        top_failures: r.top_failures,
        format,
    })
}

#[allow(clippy::too_many_lines)]
async fn bench_retry(r: args::RetryCmd) -> Result<(), Error> {
    use crate::run::retry::{
        archive_trajectories, build_history_entry, detect_harness_mismatch, generate_retry_id,
        load_sweep_results, merge_retry_results, resolve_selection, restore_archived_trajectories,
        restore_missing_trajectories, save_pre_retry_backup,
    };
    use crate::run::swebench::{
        OverrideDelta, RetrySelection, SWEEP_STATUS_COMPLETED, write_sweep_results_atomic,
    };
    use crate::trajectory::FailureCategory;
    use std::collections::HashSet;

    let original = load_sweep_results(&r.sweep)?;
    if original.sweep_status != SWEEP_STATUS_COMPLETED {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "bench retry: sweep status is '{}', not 'completed'; only completed sweeps can be retried",
            original.sweep_status
        ))));
    }

    // Parse comma-separated selection flags.
    let failure_categories: Option<Vec<FailureCategory>> = r
        .failure_category
        .as_deref()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(crate::run::swebench::parse_failure_category_label)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    let outcomes: Option<Vec<String>> = r.outcome.as_deref().map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            // Normalize the user-facing alias "errored" → "error" so it matches
            // the stored outcome value in trajectory files.
            .map(|s| if s == "errored" { "error" } else { s }.to_owned())
            .collect()
    });

    let instance_ids: Option<Vec<String>> = r.instance_ids.as_deref().map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    });

    let selected = resolve_selection(
        &original.instances,
        failure_categories.as_deref(),
        outcomes.as_deref(),
        instance_ids.as_deref(),
        r.limit,
        r.allow_resolved_retry,
    )?;

    // Harness mismatch gate.
    let harness_mismatch = detect_harness_mismatch(&original);
    if harness_mismatch && !r.allow_harness_mismatch {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "bench retry: harness git SHA mismatch; pass --allow-harness-mismatch to bypass".into(),
        )));
    }

    // Dry-run preview: ask for confirmation when --yes is not set.
    if !r.yes {
        let ids: Vec<&str> = selected.iter().map(|i| i.instance_id.as_str()).collect();
        eprintln!("bench retry: {} instance(s) selected:", selected.len());
        for id in &ids {
            eprintln!("  {id}");
        }
        if std::io::stdin().is_terminal() {
            eprint!("Proceed? [y/N] ");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer)
                .map_err(Error::Io)?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "bench retry: cancelled by user".into(),
                )));
            }
        } else {
            eprintln!("(pass --yes to proceed non-interactively)");
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "bench retry: pass --yes to proceed non-interactively".into(),
            )));
        }
    }

    let retry_id = generate_retry_id();
    save_pre_retry_backup(&r.sweep, &original, &retry_id)?;
    archive_trajectories(&r.sweep, &selected, &retry_id)?;

    let selected_ids: HashSet<String> = selected.iter().map(|i| i.instance_id.clone()).collect();
    let ids_csv = {
        let mut v: Vec<&str> = selected_ids.iter().map(String::as_str).collect();
        v.sort_unstable();
        v.join(",")
    };

    let sweep_args = retry_swebench_args(&r, &original, &ids_csv)?;
    let retry_results = match crate::run::swebench::run(sweep_args).await {
        Ok(results) => results,
        Err(e) => {
            // Unconditionally restore all archived trajectories/patches so that
            // any completed instances that already overwrote their live files are
            // rolled back to match the pre-retry results.json we are restoring.
            if let Err(restore_err) = restore_archived_trajectories(&r.sweep, &selected, &retry_id)
            {
                tracing::warn!(err = %restore_err, "could not restore archived trajectories");
            }
            if let Err(restore_err) =
                crate::run::retry::restore_pre_retry_backup(&r.sweep, &retry_id)
            {
                tracing::warn!(err = %restore_err, "could not restore pre-retry backup");
            }
            return Err(e);
        }
    };
    let retry_cancelled = retry_results.sweep_status != SWEEP_STATUS_COMPLETED;
    restore_missing_trajectories(&r.sweep, &selected, &retry_id)?;

    let override_delta = OverrideDelta {
        model: r.model.clone(),
        step_limit: r.step_limit,
        task_timeout_secs: r.task_timeout_secs,
        per_task_budget_usd: r.per_task_budget_usd,
        sweep_cost_limit_usd: r.sweep_cost_limit_usd,
    };
    let selection = RetrySelection {
        failure_categories: failure_categories
            .as_ref()
            .map(|v| v.iter().map(|c| format!("{c:?}").to_lowercase()).collect()),
        outcomes: outcomes.clone(),
        instance_ids: instance_ids.clone(),
        limit: r.limit,
    };
    // Build a placeholder entry (post-counts will be fixed after merge).
    let entry = build_history_entry(
        &retry_id,
        &selected,
        selection,
        override_delta,
        harness_mismatch,
        &original,
        &retry_results,
    );

    let mut merged = merge_retry_results(&original, &retry_results, entry, &selected_ids);
    // Overwrite post-counts with values from the fully merged sweep so that
    // the history entry reflects the whole sweep, not just the retry subset.
    if let Some(last) = merged.retry_history.last_mut() {
        let post_resolved: u32 = merged.instances.iter().map(|r| r.resolved_count).sum();
        last.post_submitted = merged.submitted;
        last.post_errored = merged.errored;
        last.post_resolved_count = post_resolved as usize;
    }

    let results_path = r.sweep.join("results.json");
    write_sweep_results_atomic(&results_path, &merged)?;

    if retry_cancelled {
        // Merge and write succeeded so partial results are preserved, but exit
        // non-zero so automation can detect the incomplete retry.
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "bench retry: retry was cancelled (status: {}) — partial results have been merged",
            retry_results.sweep_status
        ))));
    }

    tracing::info!(
        retry_id = %retry_id,
        count = selected.len(),
        "bench retry complete"
    );
    Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriageFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandStatsFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrepOutputFormat {
    Text,
    Json,
}

async fn bench_tail(t: args::TailCmd) -> Result<(), Error> {
    if t.interval_ms == 0 {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "tail: --interval-ms must be greater than 0".into(),
        )));
    }
    let format = match t.format.as_str() {
        "text" => TailFormat::Text,
        "json" => TailFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let mut stdout = std::io::stdout();
    let clear_tty = format == TailFormat::Text && !t.once && stdout.is_terminal();
    loop {
        let options = crate::run::tail::SnapshotOptions::default();
        let snapshot = crate::run::tail::snapshot(&t.sweep, &options)?;
        if clear_tty {
            write!(stdout, "\x1b[2J\x1b[H")?;
        }
        match format {
            TailFormat::Text => {
                write!(stdout, "{}", crate::run::tail::render_text(&snapshot))?;
            }
            TailFormat::Json => {
                writeln!(stdout, "{}", serde_json::to_string(&snapshot)?)?;
            }
        }
        stdout.flush()?;

        if let Some(reason) = snapshot.abort_reason {
            exit_with_outcome(ExitCode::InternalError, &format!("bench tail: {reason}"));
        }
        if t.once || snapshot.is_complete {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(t.interval_ms)).await;
    }
}

async fn bench_watch(w: args::WatchCmd) -> Result<(), Error> {
    crate::run::watch::run(&crate::run::watch::WatchArgs {
        sweep: w.sweep,
        instance: w.instance,
        run_index: w.run_index,
        wait_secs: w.wait_secs,
        stall_secs: w.stall_secs,
        full: w.full,
        max_bytes: w.max_bytes,
        ndjson: w.ndjson,
    })
    .await
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        Cli, args, cancellation_exit_code, maybe_publish_mini_github_pr, mini_github_pr_options,
        parse_verify_checks, required_github_arg, swebench_args_from_cmd,
        swebench_github_pr_config, trajectory_submitted, validate_observation_head_ratio,
        validate_swebench_github_pr_args,
    };
    use crate::error::Error;
    use crate::run::github_pr::PublishMode;
    use crate::run::swebench::{CANCEL_EXIT_CODE_ESCALATED, SweepResults};
    use crate::trajectory::{Trajectory, outcome};
    use clap::Parser as _;
    use std::path::{Path, PathBuf};

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
            target_repo: Some("madmax983/rust_swe_agent".into()),
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
        assert_eq!(options.target_repo, "madmax983/rust_swe_agent");
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
            "rust-swe-agent",
            "mini",
            "--task",
            "Fix it",
            "--mcp-server",
            "diagnostic-mcp",
        ]);
        let crate::cli::Command::Mini(cmd) = cli.command else {
            panic!("expected mini command");
        };

        assert_eq!(cmd.mcp_servers, vec!["diagnostic-mcp"]);
    }

    #[tokio::test]
    async fn mini_github_pr_publish_helper_respects_submission_state() {
        let work = tempfile::tempdir().unwrap();
        let submitted = work.path().join("submitted.traj.json");
        write_trajectory(&submitted, Some(outcome::SUBMITTED));
        let patch = work.path().join("submitted.patch");
        std::fs::write(&patch, sample_patch()).unwrap();

        maybe_publish_mini_github_pr(Some(crate::run::github_pr::GithubPrOptions {
            target_repo: "madmax983/rust_swe_agent".into(),
            target_branch: "trunk".into(),
            task_id: "submitted".into(),
            trajectory_ref: submitted.display().to_string(),
            patch_path: patch,
            branch_prefix: "rust-swe-agent".into(),
            token_env: "GITHUB_TOKEN".into(),
            mode: PublishMode::DryRun,
            timeout_secs: 30,
            max_retries: 2,
            backoff_base_ms: 250,
            redaction: crate::config::RedactionCfg::default(),
        }))
        .await
        .unwrap();

        let errored = work.path().join("errored.traj.json");
        write_trajectory(&errored, Some(outcome::ERROR));
        maybe_publish_mini_github_pr(Some(crate::run::github_pr::GithubPrOptions {
            trajectory_ref: errored.display().to_string(),
            patch_path: work.path().join("missing.patch"),
            mode: PublishMode::DryRun,
            ..github_options_for_cli_test()
        }))
        .await
        .unwrap();
        maybe_publish_mini_github_pr(None).await.unwrap();
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
            "rust-swe-agent",
            "bench",
            "swebench",
            "--dataset-path",
            "dataset.jsonl",
            "--output",
            "runs",
        ]);
        let crate::cli::Command::Bench {
            cmd: args::BenchCmd::Swebench(cmd),
        } = cli.command
        else {
            panic!("expected bench swebench command");
        };

        let args = swebench_args_from_cmd(cmd, crate::config::Config::defaults().unwrap(), "sweep")
            .unwrap();
        assert!(args.install_os_signal_handlers);
        assert!(
            args.cancellation_signals.is_none(),
            "CLI construction must not install process-level Ctrl-C handlers before preflight"
        );
    }

    fn mini_cmd(open_pr: bool, dry_run: bool) -> args::MiniCmd {
        args::MiniCmd {
            task: "Fix it".into(),
            extra_context: None,
            model: "deterministic".into(),
            step_limit: 1,
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
            config: None,
            env: None,
            docker_image: None,
            output: PathBuf::from("runs"),
            trajectory_name: None,
            stream: None,
            skip_patch_validation: false,
            verify: vec![],
            verify_timeout_secs: 60,
            github_pr: args::MiniGithubPrArgs {
                open_pr,
                target_repo: Some("madmax983/rust_swe_agent".into()),
                target_branch: Some("trunk".into()),
                github_token_env: "GITHUB_TOKEN".into(),
                github_pr_dry_run: dry_run,
                github_pr_timeout_secs: 30,
                github_pr_max_retries: 2,
                github_pr_backoff_base_ms: 250,
                github_pr_branch_prefix: "rust-swe-agent".into(),
            },
            render_only: false,
            format: "text".into(),
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
        }
    }

    fn swebench_github(open_prs: bool, dry_run: bool) -> args::SwebenchGithubPrArgs {
        args::SwebenchGithubPrArgs {
            open_prs,
            target_repo: Some("madmax983/rust_swe_agent".into()),
            target_branch: Some("trunk".into()),
            github_token_env: "GITHUB_TOKEN".into(),
            github_pr_dry_run: dry_run,
            github_pr_timeout_secs: 30,
            github_pr_max_retries: 2,
            github_pr_backoff_base_ms: 250,
            github_pr_branch_prefix: "rust-swe-agent".into(),
        }
    }

    fn github_options_for_cli_test() -> crate::run::github_pr::GithubPrOptions {
        crate::run::github_pr::GithubPrOptions {
            target_repo: "madmax983/rust_swe_agent".into(),
            target_branch: "trunk".into(),
            task_id: "task".into(),
            trajectory_ref: "traj.json".into(),
            patch_path: PathBuf::from("patch.diff"),
            branch_prefix: "rust-swe-agent".into(),
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
}
