//! Command-line interface. `clap` derive; subcommand dispatch.

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
            cmd: args::BenchCmd::Compare(c),
        } => bench_compare(c),
        Command::Bench {
            cmd: args::BenchCmd::Evaluate(e),
        } => bench_evaluate(e),
        Command::Bench {
            cmd: args::BenchCmd::Inspect(i),
        } => bench_inspect(i),
        #[cfg(feature = "docker")]
        Command::Cleanup => cleanup_cmd().await,
        #[cfg(not(feature = "docker"))]
        Command::Cleanup => cleanup_cmd(),
    }
}

fn init_logging(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

async fn mini_cmd(m: args::MiniCmd) -> Result<(), Error> {
    let mut cfg = match &m.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };
    cfg.root.model.name.clone_from(&m.model);
    cfg.root.agent.step_limit = m.step_limit;
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
    let mut cfg = match &s.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };
    cfg.root.model.name.clone_from(&s.model);
    cfg.root.agent.step_limit = s.step_limit;

    let results = crate::run::swebench::run(crate::run::swebench::SwebenchArgs {
        dataset_path: s.dataset_path,
        output_dir: s.output,
        parallel: s.parallel,
        config: cfg,
        resume: s.resume,
        cost_limit_usd: s.sweep_cost_limit_usd,
        instance_ids: s.instance_ids,
        limit: s.limit,
        sample: s.sample,
        seed: s.seed,
        max_retries: s.max_retries,
        retry_on: s.retry_on,
        retry_backoff_base_ms: s.retry_backoff_base_ms,
        retry_backoff_cap_s: s.retry_backoff_cap_s,
        retry_on_resume: s.retry_on_resume,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        config_overlay_paths: s.config.into_iter().collect(),
    })
    .await?;

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

fn bench_compare(c: args::CompareCmd) -> Result<(), Error> {
    let format = match c.format.as_str() {
        "text" => crate::run::compare::CompareFormat::Text,
        "json" => crate::run::compare::CompareFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let breakdown = parse_breakdown_selection(&c.breakdown, false)?;
    let report = crate::run::compare::compute(&crate::run::compare::CompareArgs {
        baseline: c.baseline,
        candidate: c.candidate,
        format,
        max_regressions: c.max_regressions,
        breakdown,
        min_delta_pp: c.breakdown_min_delta_pp / 100.0,
    })?;
    match format {
        crate::run::compare::CompareFormat::Text => print!("{}", report.human_table()),
        crate::run::compare::CompareFormat::Json => {
            println!("{}", report.to_json_pretty()?);
        }
    }
    if let Some(max) = c.max_regressions {
        if report.regression_count() > max {
            tracing::error!(
                regressions = report.regression_count(),
                max = max,
                "compare: regression count exceeds --max-regressions threshold"
            );
            std::process::exit(1);
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
    };
    let eval = crate::run::evaluate::run(&args)?;

    let resolved = eval.instances.iter().filter(|x| x.resolved).count();
    tracing::info!(
        instances = eval.instances.len(),
        resolved,
        evaluation_path = %crate::run::evaluate::evaluation_path(&e.sweep).display(),
        "evaluation complete"
    );
    println!("resolved: {resolved}");
    if !eval.breakdown.is_empty() {
        print!(
            "{}",
            crate::run::evaluate::render_breakdown_table(&eval.breakdown)
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
        let axis = match tok {
            "repo" => crate::run::evaluate::BreakdownAxis::Repo,
            "failure_category" => crate::run::evaluate::BreakdownAxis::FailureCategory,
            other => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "unknown --breakdown axis `{other}`"
                ))));
            }
        };
        if !axes.contains(&axis) {
            axes.push(axis);
        }
    }
    if axes.is_empty() && allow_default {
        return Ok(crate::run::evaluate::BreakdownSelection::default_axes());
    }
    Ok(crate::run::evaluate::BreakdownSelection { axes })
}

fn bench_inspect(i: args::InspectCmd) -> Result<(), Error> {
    let format = match i.format.as_str() {
        "text" => crate::run::inspect::InspectFormat::Text,
        "json" => crate::run::inspect::InspectFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let out = crate::run::inspect::run(&crate::run::inspect::InspectArgs {
        sweep: i.sweep,
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
