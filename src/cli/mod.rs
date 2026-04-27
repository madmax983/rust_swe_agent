//! Command-line interface. `clap` derive; subcommand dispatch.

use clap::Parser;

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
    pub command: args::Command,

    /// Global log level.
    #[arg(long, default_value = "info", env = "RUST_SWE_AGENT_LOG")]
    pub log: String,
}

pub async fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    init_logging(&cli.log);

    match cli.command {
        args::Command::Mini(m) => mini_cmd(m).await,
        args::Command::HelloWorld(h) => crate::run::hello_world::main(h.output).await,
        args::Command::Replay(r) => replay_cmd(r).await,
        args::Command::Bench {
            cmd: args::BenchCmd::Swebench(s),
        } => bench_swebench(s).await,
        args::Command::Bench {
            cmd: args::BenchCmd::Compare(c),
        } => bench_compare(c),
        args::Command::Bench {
            cmd: args::BenchCmd::Evaluate(e),
        } => bench_evaluate(e),
        args::Command::Bench {
            cmd: args::BenchCmd::Inspect(i),
        } => bench_inspect(i),
        #[cfg(feature = "docker")]
        args::Command::Cleanup => cleanup_cmd().await,
        #[cfg(not(feature = "docker"))]
        args::Command::Cleanup => cleanup_cmd(),
        #[cfg(feature = "exporter")]
        args::Command::Export(e) => export_cmd(&e),
    }
}

#[cfg(feature = "exporter")]
fn export_cmd(e: &args::ExportCmd) -> Result<(), Error> {
    let json = std::fs::read_to_string(&e.trajectory_path)?;
    let trajectory: crate::trajectory::Trajectory = serde_json::from_str(&json)?;
    let script = crate::trajectory::export::to_bash_script(&trajectory);
    print!("{script}");
    Ok(())
}

#[cfg(all(test, feature = "exporter"))]
mod export_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Write;

    #[test]
    fn test_export_cmd() {
        let mut t = crate::trajectory::Trajectory::new();
        let mut msg = crate::model::Message::assistant("Hello");
        msg.extra.actions = Some(vec!["echo hi".into()]);
        t.record_with_extra(&msg, msg.extra.clone());

        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        let json = serde_json::to_string(&t).unwrap();
        temp_file.write_all(json.as_bytes()).unwrap();

        let e = args::ExportCmd {
            trajectory_path: temp_file.path().to_path_buf(),
        };

        let res = export_cmd(&e);
        assert!(res.is_ok());
    }

    #[test]
    fn test_export_cmd_invalid_json() {
        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        temp_file.write_all(b"not json").unwrap();

        let e = args::ExportCmd {
            trajectory_path: temp_file.path().to_path_buf(),
        };

        let res = export_cmd(&e);
        assert!(res.is_err());
    }

    #[test]
    fn test_export_cmd_missing_file() {
        let e = args::ExportCmd {
            trajectory_path: std::path::PathBuf::from("does_not_exist.json"),
        };

        let res = export_cmd(&e);
        assert!(res.is_err());
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
        deterministic_responses: None,
        deterministic_usage_per_call: None,
    })
    .await?;

    tracing::info!(
        total = results.total,
        submitted = results.submitted,
        skipped = results.skipped,
        errored = results.errored,
        budget_halted = results.budget_halted,
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
    let report = crate::run::compare::compute(&crate::run::compare::CompareArgs {
        baseline: c.baseline,
        candidate: c.candidate,
        format,
        max_regressions: c.max_regressions,
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

    let args = crate::run::evaluate::EvaluateArgs {
        sweep_dir: e.sweep.clone(),
        dataset_path: e.dataset,
        backend,
        timeout_per_instance_secs: e.timeout_per_instance,
        parallel: e.parallel,
        sb_subset: e.sb_subset,
        sb_split: e.sb_split,
        run_id: e.run_id,
    };
    let eval = crate::run::evaluate::run(&args)?;

    let resolved = eval.instances.iter().filter(|x| x.resolved).count();
    tracing::info!(
        instances = eval.instances.len(),
        resolved,
        evaluation_path = %crate::run::evaluate::evaluation_path(&e.sweep).display(),
        "evaluation complete"
    );
    Ok(())
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
