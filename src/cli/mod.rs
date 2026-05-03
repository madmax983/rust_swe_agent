//! Command-line interface. `clap` derive; subcommand dispatch.

use std::io::{IsTerminal as _, Write as _};
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::error::Error;

pub mod args;

#[derive(Debug, Parser)]
#[command(
    name = "rust-swe-agent",
    version,
    about = "Rust port of mini-swe-agent"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Global log level.
    #[arg(long, default_value = "info", env = "RUST_SWE_AGENT_LOG")]
    pub log: String,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one task end-to-end and write a trajectory.
    Mini(args::MiniCmd),
    /// Smoke-test: scripted model + local env writes a trajectory.
    HelloWorld(args::HelloWorldCmd),
    /// Replay an existing trajectory using a deterministic model.
    Replay(args::ReplayCmd),
    /// SWE-bench parallel sweep.
    Bench {
        #[command(subcommand)]
        cmd: args::BenchCmd,
    },
    /// Reap any leftover `rust-swe-agent=1` labeled containers.
    Cleanup,
}

pub async fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    init_logging(&cli.log);

    match cli.command {
        Command::Mini(m) => mini_cmd(m).await,
        Command::HelloWorld(h) => crate::run::hello_world::main(h.output).await,
        Command::Replay(r) => replay_cmd(r).await,
        Command::Bench {
            cmd: args::BenchCmd::Swebench(s),
        } => bench_swebench(s).await,
        Command::Bench {
            cmd: args::BenchCmd::Forecast(s),
        } => bench_forecast(s).await,
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

async fn mini_cmd(m: args::MiniCmd) -> Result<(), Error> {
    let mut cfg = match &m.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };
    cfg.root.model.name.clone_from(&m.model);
    cfg.root.agent.step_limit = m.step_limit;
    if let Some(v) = m.observation_max_bytes {
        cfg.root.agent.observation_max_bytes = v;
    }
    if let Some(v) = m.observation_head_ratio {
        validate_observation_head_ratio(v)?;
        cfg.root.agent.observation_head_ratio = v;
    }
    if let Some(kind) = &m.env {
        cfg.root.environment.kind = match kind.as_str() {
            "local" => crate::config::EnvKind::Local,
            "docker" => crate::config::EnvKind::Docker,
            other => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "unknown --env `{other}` (expected `local` or `docker`)"
                ))));
            }
        };
    }
    if let Some(img) = m.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }

    let trajectory_name = m
        .trajectory_name
        .clone()
        .unwrap_or_else(|| crate::run::mini::slugify(&m.task));

    let stream_addr = match &m.stream {
        Some(s) => Some(s.parse().map_err(|e: std::net::AddrParseError| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid --stream address `{s}`: {e}"
            )))
        })?),
        None => None,
    };

    let args = crate::run::mini::MiniArgs {
        task: m.task,
        extra_context: m.extra_context,
        config: cfg,
        output_dir: m.output,
        trajectory_name,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        task_timeout_secs: m.task_timeout_secs,
        stream_addr,
        patch_capture: None,
    };
    crate::run::mini::run(args).await
}

async fn replay_cmd(r: args::ReplayCmd) -> Result<(), Error> {
    let mut cfg = match &r.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };
    if let Some(kind) = &r.env {
        cfg.root.environment.kind = match kind.as_str() {
            "local" => crate::config::EnvKind::Local,
            "docker" => crate::config::EnvKind::Docker,
            other => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "unknown --env `{other}` (expected `local` or `docker`)"
                ))));
            }
        };
    }
    if let Some(img) = r.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }

    let args = crate::run::replay::ReplayArgs {
        trajectory_path: r.trajectory_path,
        config: cfg,
        output_dir: r.output,
        trajectory_name: r.trajectory_name,
    };
    crate::run::replay::run(args).await
}

async fn bench_swebench(s: args::SwebenchCmd) -> Result<(), Error> {
    let mut sweep_cmd = s;
    if sweep_cmd.forecast_first {
        match run_forecast_from_cmd(sweep_cmd.clone()).await? {
            crate::run::forecast::ForecastOutcome::Report(report) => {
                print_forecast_report(&report, &sweep_cmd.format)?;
                crate::run::forecast::validate_fail_over_cap(&report, sweep_cmd.fail_over_cap)?;
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
            crate::run::forecast::ForecastOutcome::DryRun(results) => {
                print_dry_run_summary(&results, &sweep_cmd.format);
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
        crate::run::swebench::run(swebench_args_from_cmd(sweep_cmd, cfg, preflight_mode)).await?;

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
    Ok(())
}

async fn bench_doctor(mut s: args::SwebenchCmd) -> Result<(), Error> {
    s.dry_run = true;
    let output_format = s.format.clone();
    let cfg = swebench_config_from_cmd(&s)?;
    let results = crate::run::swebench::run(swebench_args_from_cmd(s, cfg, "doctor")).await?;
    if output_format != "json" {
        print!("{}", results.summary_table());
    }
    Ok(())
}

async fn bench_forecast(s: args::SwebenchCmd) -> Result<(), Error> {
    let output_format = s.format.clone();
    let fail_over_cap = s.fail_over_cap;
    match run_forecast_from_cmd(s).await? {
        crate::run::forecast::ForecastOutcome::Report(report) => {
            print_forecast_report(&report, &output_format)?;
            crate::run::forecast::validate_fail_over_cap(&report, fail_over_cap)
        }
        crate::run::forecast::ForecastOutcome::DryRun(results) => {
            print_dry_run_summary(&results, &output_format);
            Ok(())
        }
    }
}

async fn run_forecast_from_cmd(
    mut s: args::SwebenchCmd,
) -> Result<crate::run::forecast::ForecastOutcome, Error> {
    let calibration_n = s.calibration_n;
    let seed = s.seed.unwrap_or(42);
    let target_n = s.target_n;
    let confidence_pct = s.confidence;
    if s.sample.is_none() {
        s.seed = None;
    }
    let cfg = swebench_config_from_cmd(&s)?;
    let sweep = swebench_args_from_cmd(s, cfg, "forecast");
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
        cfg.root.environment.kind = match kind.as_str() {
            "local" => crate::config::EnvKind::Local,
            "docker" => crate::config::EnvKind::Docker,
            other => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "unknown --env `{other}` (expected `local` or `docker`)"
                ))));
            }
        };
    }
    if let Some(img) = s.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }
    Ok(cfg)
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

fn swebench_args_from_cmd(
    s: args::SwebenchCmd,
    cfg: Config,
    preflight_mode: &str,
) -> crate::run::swebench::SwebenchArgs {
    let cfg_max_rpm = cfg.root.sweep.max_rpm;
    let cfg_max_input_tpm = cfg.root.sweep.max_input_tpm;
    crate::run::swebench::SwebenchArgs {
        dataset_path: s.dataset_path,
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
    }
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
        breakdown,
        min_delta_pp: c.breakdown_min_delta_pp / 100.0,
        cost_attribution: matches!(c.cost_attribution, args::OnOffArg::On),
        cost_attribution_min_delta_usd: c.cost_attribution_min_delta_usd,
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
            std::process::exit(1);
        }
    }
    Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailFormat {
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
            eprintln!("bench tail: {reason}");
            std::process::exit(1);
        }
        if t.once || snapshot.is_complete {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(t.interval_ms)).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::validate_observation_head_ratio;

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
}

#[test]
fn test_compare_diff_mutex() {
    let cmd = crate::cli::args::CompareCmd {
        baseline: std::path::PathBuf::from("a"),
        candidate: std::path::PathBuf::from("b"),
        format: "text".into(),
        show_noise: false,
        inspect_diff: Some("foo".into()),
        emit_diff_script: Some(std::path::PathBuf::from("bar")),
        max_regressions: None,
        breakdown: String::new(),
        breakdown_min_delta_pp: 0.0,
        cost_attribution: crate::cli::args::OnOffArg::Off,
        cost_attribution_min_delta_usd: 0.0,
    };
    let err = bench_compare(cmd);
    assert!(err.is_err());
    let Err(err_val) = err else {
        panic!("Expected error but got Ok")
    };
    assert!(err_val.to_string().contains("pass only one"));
}

#[test]
fn test_bench_evaluate_backend_error() {
    let cmd = crate::cli::args::EvaluateCmd {
        sweep: std::path::PathBuf::from("foo"),
        dataset: None,
        backend: "unknown".into(),
        breakdown: String::new(),
        cost_attribution: crate::cli::args::OnOffArg::Off,
        parallel: 4,
        run_id: None,
        sb_subset: String::new(),
        sb_split: String::new(),
        timeout_per_instance: 0,
    };
    let err = bench_evaluate(cmd);
    assert!(err.is_err());
    let Err(err_val) = err else {
        panic!("Expected error but got Ok")
    };
    assert!(err_val.to_string().contains("unknown --backend"));
}

#[test]
fn test_inspect_missing_sweep() {
    let cmd = crate::cli::args::InspectCmd {
        diff: vec![],
        sweep: None,
        instance: None,
        filter: None,
        full: false,
        format: "unknown".into(),
        show_noise: false,
    };
    // Expect an error because sweep is missing
    let err = bench_inspect(cmd);
    assert!(err.is_err());
}

#[test]

fn test_bench_compare_format_error() {
    let cmd = crate::cli::args::CompareCmd {
        baseline: std::path::PathBuf::from("a"),
        candidate: std::path::PathBuf::from("b"),
        format: "unknown".into(),
        show_noise: false,
        inspect_diff: Some("foo".into()),
        emit_diff_script: None,
        max_regressions: None,
        breakdown: String::new(),
        breakdown_min_delta_pp: 0.0,
        cost_attribution: crate::cli::args::OnOffArg::Off,
        cost_attribution_min_delta_usd: 0.0,
    };
    let err = bench_compare(cmd);
    assert!(err.is_err());
    let Err(err_val) = err else {
        panic!("Expected error but got Ok")
    };
    assert!(err_val.to_string().contains("unknown --format"));
}
