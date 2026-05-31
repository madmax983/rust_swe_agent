//! Command-line interface. `clap` derive; subcommand dispatch.
// The dispatch functions in this module call large async subsystems.  The
// Box::pin calls on the hot paths heap-allocate the inner futures, but the
// outer dispatch state machines can still cross the 16 KiB threshold on some
// compiler builds.  The lint is informational here; the allocation behaviour
// is already correct.
#![allow(clippy::large_futures)]

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::error::Error;
use crate::exit_code::ExitCode;

pub mod args;

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
        Command::Mini(m) => mini_cmd(*m).await,
        Command::HelloWorld(h) => {
            crate::run::hello_world::main(h.output, h.config.as_deref()).await
        }
        Command::Replay(r) => replay_cmd(*r).await,
        Command::Bench { cmd } => match *cmd {
            args::BenchCmd::Swebench(s) => Box::pin(bench_swebench(*s)).await,
            args::BenchCmd::Rehearsal(mut s) => {
                s.rehearse = true;
                Box::pin(bench_swebench(*s)).await
            }
            args::BenchCmd::Forecast(s) => Box::pin(bench_forecast(*s)).await,
            args::BenchCmd::Calibrate(c) => bench_calibrate(c),
            args::BenchCmd::Doctor(s) => Box::pin(bench_doctor(*s)).await,
            args::BenchCmd::Compare(c) => bench_compare(c),
            args::BenchCmd::DiffConfig(c) => bench_diff_config(c),
            args::BenchCmd::Evaluate(e) => bench_evaluate(e),
            args::BenchCmd::Inspect(i) => bench_inspect(i),
            args::BenchCmd::Tail(t) => bench_tail(t).await,
            args::BenchCmd::Watch(w) => bench_watch(w).await,
            args::BenchCmd::Triage(t) => bench_triage(t),
            args::BenchCmd::TriageDiff(t) => bench_triage_diff(t),
            args::BenchCmd::CommandStats(c) => bench_command_stats(c),
            args::BenchCmd::Grep(g) => bench_grep(g),
            args::BenchCmd::Frontier(f) => bench_frontier(f),
            args::BenchCmd::Reproduce(r) => Box::pin(bench_reproduce(r)).await,
            args::BenchCmd::Bundle(b) => bench_bundle(b),
            args::BenchCmd::Matrix(m) => Box::pin(bench_matrix(m)).await,
            args::BenchCmd::EvaluatorSelftest(s) => bench_evaluator_selftest(s),
            args::BenchCmd::Report(r) => bench_report(r),
            args::BenchCmd::Retry(r) => Box::pin(bench_retry(r)).await,
            args::BenchCmd::Behavior(b) => bench_behavior(b),
            args::BenchCmd::ToolCoverage(t) => bench_tool_coverage(t),
            args::BenchCmd::PolicyImpact(p) => bench_policy_impact(p),
            args::BenchCmd::InstanceHistory(h) => bench_instance_history(h),
            args::BenchCmd::CacheStats(c) => bench_cache_stats(c),
            args::BenchCmd::BudgetFit(b) => bench_budget_fit(b),
            args::BenchCmd::ToolAblation(t) => Box::pin(bench_tool_ablation(t)).await,
            args::BenchCmd::Ladder(l) => bench_ladder(l),
            args::BenchCmd::Cascade(c) => Box::pin(bench_cascade(c)).await,
            args::BenchCmd::TestProgress(t) => bench_test_progress(t),
            args::BenchCmd::Fork(f) => Box::pin(crate::run::fork::run(f)).await,
            args::BenchCmd::Power(p) => bench_power(&p),
            args::BenchCmd::DatasetStats(s) => bench_dataset_stats(s),
            args::BenchCmd::Bisect(b) => Box::pin(bench_bisect(b)).await,
            args::BenchCmd::Audit(a) => bench_audit(a),
            args::BenchCmd::FailureDigest(f) => bench_failure_digest(f),
            args::BenchCmd::EvalFlake(f) => bench_eval_flake(f),
            args::BenchCmd::Annotate(a) => bench_annotate(a),
            args::BenchCmd::StagnationReport(s) => bench_stagnation_report(s),
            args::BenchCmd::SelfCheck(s) => bench_self_check(s),
            args::BenchCmd::Import(i) => bench_import(i),
            args::BenchCmd::ExportCi(c) => bench_export_ci(c),
            args::BenchCmd::ContaminationCheck(c) => bench_contamination_check(c),
            args::BenchCmd::ScriptabilityCheck(s) => Box::pin(bench_scriptability_check(s)).await,
            args::BenchCmd::NearMiss(n) => bench_near_miss(n),
            args::BenchCmd::Assert(a) => bench_assert(a),
        },
        Command::Agent { cmd } => match *cmd {
            args::AgentCmd::SkillsPreview(s) => agent_skills_preview_cmd(&s),
            args::AgentCmd::RedactCheck(r) => agent_redact_check_cmd(&r),
            args::AgentCmd::Env {
                cmd: args::AgentEnvCmd::Preview(ref p),
            } => agent_env_preview_cmd(p),
            args::AgentCmd::Suite(s) => Box::pin(agent_suite_cmd(*s)).await,
            args::AgentCmd::PolicyCheck(p) => agent_policy_check_cmd(&p),
            args::AgentCmd::Apply(a) => agent_apply_cmd(&a),
        },
        Command::Ui(u) => ui_cmd(u).await,
        #[cfg(feature = "docker")]
        Command::Cleanup => cleanup_cmd().await,
        #[cfg(not(feature = "docker"))]
        Command::Cleanup => cleanup_cmd(),
    }
}

fn agent_env_preview_cmd(p: &args::EnvPreviewCmd) -> Result<(), Error> {
    let cfg = match &p.config {
        Some(path) => Config::load(path)?,
        None => Config::defaults()?,
    };
    let opts = crate::run::env_preview::EnvPreviewOpts {
        env_type: p.env.as_str().to_owned(),
        task: p.task.clone(),
        config_path: p.config.clone(),
        show_values: p.show_values,
    };
    let preview = crate::run::env_preview::run_env_preview(&cfg, &opts);
    if p.format == args::PreviewFormatArg::Json {
        let wrapped = serde_json::json!({ "env_preview": &preview });
        println!(
            "{}",
            serde_json::to_string_pretty(&wrapped).map_err(|e| {
                Error::Config(crate::error::ConfigError::Invalid(e.to_string()))
            })?
        );
    } else {
        print_env_preview_text(&preview);
    }
    if crate::run::env_preview::is_risky(&preview) {
        exit_with_outcome(
            ExitCode::EnvPreviewWarning,
            "env preview has risky findings",
        );
    }
    Ok(())
}

fn agent_redact_check_cmd(r: &args::RedactCheckCmd) -> Result<(), Error> {
    use crate::run::redact_check::{
        RedactCheckFormat, RedactCheckOpts, RedactCheckSource, format_human, format_json,
        run_redact_check,
    };

    let cfg = match &r.config {
        Some(path) => Config::load(path)?,
        None => Config::defaults()?,
    };

    // Resolve input source; exactly one of --text/--file/--trajectory/stdin.
    let source = match (&r.text, &r.file, &r.trajectory) {
        (Some(text), None, None) => RedactCheckSource::Text(text.clone()),
        (None, Some(path), None) => RedactCheckSource::File(path.clone()),
        (None, None, Some(path)) => RedactCheckSource::Trajectory(path.clone()),
        (None, None, None) => {
            // Block on stdin only when it is actually a pipe/redirect; reject
            // interactive TTYs immediately so forgotten flags fail fast.
            if std::io::stdin().is_terminal() {
                return Err(Error::Config(crate::error::ConfigError::Usage(
                    "no input source provided; use --text, --file, --trajectory, or pipe to stdin"
                        .to_owned(),
                )));
            }
            RedactCheckSource::Stdin
        }
        _ => {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "supply exactly one of --text, --file, or --trajectory (or pipe to stdin)"
                    .to_owned(),
            )));
        }
    };

    // --json is a convenient shorthand for --format json.
    let format = if r.json {
        RedactCheckFormat::Json
    } else {
        match r.format.as_str() {
            "json" => RedactCheckFormat::Json,
            "human" | "" => RedactCheckFormat::Human,
            other => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "--format '{other}' is not valid; use 'human' or 'json'"
                ))));
            }
        }
    };

    let opts = RedactCheckOpts {
        source,
        format,
        strict: r.strict,
    };

    let output = run_redact_check(&cfg, &opts)?;
    let exit_code = output.exit_code();

    match format {
        RedactCheckFormat::Json => {
            let json_val = format_json(&output).map_err(Error::Json)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json_val).map_err(Error::Json)?
            );
        }
        RedactCheckFormat::Human => {
            print!("{}", format_human(&output));
        }
    }

    if exit_code != ExitCode::Success {
        exit_with_outcome(exit_code, exit_code.outcome_class());
    }
    Ok(())
}

fn agent_policy_check_cmd(p: &args::PolicyCheckCmd) -> Result<(), Error> {
    use crate::run::policy_check::{
        ExpectAssertion, PolicyCheckOpts, PolicyCheckSource, VerdictKind, format_json, format_text,
        run_policy_check,
    };

    let cfg = match &p.config {
        Some(path) => Config::load(path)?,
        None => Config::defaults()?,
    };

    // Resolve input source; exactly one of --commands-file / --stdin / --command (required).
    let source = match (&p.commands_file, p.stdin, p.command.is_empty()) {
        (Some(path), false, true) => PolicyCheckSource::CommandsFile(path.clone()),
        (None, true, true) => PolicyCheckSource::Stdin,
        (None, false, false) => PolicyCheckSource::Commands(p.command.clone()),
        (None, false, true) => {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "no input source; use --commands-file, --command, or --stdin".to_owned(),
            )));
        }
        _ => {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "supply exactly one of --commands-file, --stdin, or --command (repeatable)"
                    .to_owned(),
            )));
        }
    };

    // Parse --expect CMD:VERDICT assertions (split on last ':' to handle colons in commands).
    let expect: Vec<ExpectAssertion> = p
        .expect
        .iter()
        .map(|raw| {
            let last_colon = raw.rfind(':').ok_or_else(|| {
                Error::Config(crate::error::ConfigError::Usage(format!(
                    "--expect must be in CMD:VERDICT format (allow/ask/deny), got '{raw}'"
                )))
            })?;
            let cmd = raw[..last_colon].to_owned();
            let verdict_str = &raw[last_colon + 1..];
            let expected = VerdictKind::parse(verdict_str).ok_or_else(|| {
                Error::Config(crate::error::ConfigError::Usage(format!(
                    "--expect verdict must be 'allow', 'ask', or 'deny', got '{verdict_str}'"
                )))
            })?;
            Ok(ExpectAssertion {
                command: cmd,
                expected,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let opts = PolicyCheckOpts { source, expect };
    let output = run_policy_check(&cfg, &opts)?;

    let formatted = match p.format {
        args::PolicyCheckFormatArg::Json => {
            let json_val = format_json(&output).map_err(Error::Json)?;
            serde_json::to_string_pretty(&json_val).map_err(Error::Json)?
        }
        args::PolicyCheckFormatArg::Text => format_text(&output),
    };

    if let Some(out_path) = &p.output {
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(out_path, &formatted)?;
    } else {
        print!("{formatted}");
    }

    if output.has_mismatches() {
        exit_with_outcome(
            ExitCode::UsageError,
            "policy-check: --expect assertions failed",
        );
    }
    Ok(())
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

fn agent_apply_cmd(a: &args::AgentApplyCmd) -> Result<(), Error> {
    use crate::run::apply::{AgentApplyOpts, PatchSelector, exit_code_for, run_agent_apply};

    // Resolve selector
    let selector = match (&a.patch, &a.trajectory, &a.sweep, &a.instance) {
        (Some(p), None, None, None) => PatchSelector::PatchFile(p.clone()),
        (None, Some(t), None, None) => PatchSelector::TrajectoryFile(t.clone()),
        (None, None, Some(s), Some(inst)) => PatchSelector::SweepInstance {
            sweep: s.clone(),
            instance: inst.clone(),
        },
        (None, None, None, None) => {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "no patch selector; use --patch <PATH>, --trajectory <PATH>, \
                 or --sweep <DIR> --instance <ID>"
                    .to_owned(),
            )));
        }
        _ => {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "supply exactly one of: --patch <PATH>, --trajectory <PATH>, \
                 or --sweep <DIR> --instance <ID>"
                    .to_owned(),
            )));
        }
    };

    let target = a.target.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });

    let opts = AgentApplyOpts {
        selector,
        target,
        allow_redacted: a.allow_redacted,
        allow_dirty: a.allow_dirty,
        dry_run: a.dry_run,
        three_way: a.three_way,
        report_path: a.report.clone(),
    };

    match run_agent_apply(opts) {
        Ok(report) => {
            if report.check_result == "empty" {
                eprintln!("no changes to apply (empty patch)");
            } else if report.dry_run {
                eprintln!(
                    "dry-run: {} file(s) would change (+{} -{} lines)",
                    report.files_changed.len(),
                    report.lines_added,
                    report.lines_removed
                );
                for f in &report.files_changed {
                    eprintln!("  {f}");
                }
            } else {
                eprintln!(
                    "applied: {} file(s) changed (+{} -{} lines)",
                    report.files_changed.len(),
                    report.lines_added,
                    report.lines_removed
                );
            }
            Ok(())
        }
        Err(e) => {
            let ec = exit_code_for(&e);
            eprintln!("outcome_class: {}", ec.outcome_class());
            eprintln!("error: {e}");
            // Print rejected hunks on CheckFailed
            if let crate::run::apply::ApplyError::CheckFailed(ref hunks) = e {
                if !hunks.is_empty() {
                    eprintln!("--- rejected hunks ---");
                    eprintln!("{hunks}");
                }
            }
            // Print dirty paths on DirtyTree
            if let crate::run::apply::ApplyError::DirtyTree(ref paths) = e {
                eprintln!("--- dirty paths ---");
                for p in paths {
                    eprintln!("  {p}");
                }
            }
            std::process::exit(ec.as_i32());
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn mini_cmd(m: args::MiniCmd) -> Result<(), Error> {
    let task = if m.resume_from.is_some() {
        if m.task.is_some() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "both --task and --resume were provided".into(),
            )));
        }
        if m.task_file.is_some() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "both --task-file and --resume were provided".into(),
            )));
        }
        String::new()
    } else if m.continue_from.is_some() {
        // --continue REQUIRES --task (or --task-file) — the follow-up instruction.
        // Clap enforces that --task is present when --continue is set; if the task is
        // empty we catch it here the same way the normal path does below.
        match (&m.task, &m.task_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "both --task and --task-file were provided".into(),
                )));
            }
            (None, None) => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "--continue requires --task (or --task-file) to supply the follow-up instruction"
                        .into(),
                )));
            }
            (Some(t), None) => {
                if t.trim().is_empty() {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(
                        "empty --task source".into(),
                    )));
                }
                t.clone()
            }
            (None, Some(tf)) => {
                let mut raw_content = if tf == "-" {
                    let mut buffer = String::new();
                    std::io::stdin().read_to_string(&mut buffer).map_err(|e| {
                        Error::Config(crate::error::ConfigError::Invalid(format!(
                            "failed to read task from stdin: {e}"
                        )))
                    })?;
                    buffer
                } else {
                    let path = std::path::Path::new(tf);
                    if !path.exists() {
                        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                            "--task-file does not exist: {tf}"
                        ))));
                    }
                    std::fs::read_to_string(path).map_err(|e| {
                        Error::Config(crate::error::ConfigError::Invalid(format!(
                            "failed to read --task-file `{tf}`: {e}"
                        )))
                    })?
                };
                if raw_content.starts_with('\u{FEFF}') {
                    raw_content.remove(0);
                }
                if raw_content.trim().is_empty() {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "empty task source from `{tf}`"
                    ))));
                }
                raw_content
            }
        }
    } else {
        match (&m.task, &m.task_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "both --task and --task-file were provided".into(),
                )));
            }
            (None, None) => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "either --task or --task-file must be provided".into(),
                )));
            }
            (Some(t), None) => {
                if t.trim().is_empty() {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(
                        "empty --task source".into(),
                    )));
                }
                t.clone()
            }
            (None, Some(tf)) => {
                let mut raw_content = if tf == "-" {
                    let mut buffer = String::new();
                    std::io::stdin().read_to_string(&mut buffer).map_err(|e| {
                        Error::Config(crate::error::ConfigError::Invalid(format!(
                            "failed to read task from stdin: {e}"
                        )))
                    })?;
                    buffer
                } else {
                    let path = std::path::Path::new(tf);
                    if !path.exists() {
                        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                            "--task-file does not exist: {tf}"
                        ))));
                    }
                    std::fs::read_to_string(path).map_err(|e| {
                        Error::Config(crate::error::ConfigError::Invalid(format!(
                            "failed to read --task-file `{tf}`: {e}"
                        )))
                    })?
                };

                if raw_content.starts_with('\u{FEFF}') {
                    raw_content.remove(0);
                }

                if raw_content.trim().is_empty() {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "empty task source from `{tf}`"
                    ))));
                }

                raw_content
            }
        }
    };

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
    // step_limit: apply CLI value only when explicitly set; otherwise the
    // config-file default is preserved. For resume runs, this is intentionally
    // skipped here — mini_resume_cmd owns cap application for that path.
    if let Some(v) = m.step_limit {
        if m.resume_from.is_none() {
            cfg.root.agent.step_limit = v;
        }
    }
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
    if m.chaos_fail_every > 0 {
        cfg.root.environment.chaos_fail_every = m.chaos_fail_every;
    }
    apply_mcp_server_overrides(&mut cfg, &m.mcp_servers)?;
    apply_read_only_policy(&m, &cfg)?;
    let resolved_workdir = resolve_and_validate_workdir(m.workdir.as_ref(), &cfg)?;

    // ── Continue path ─────────────────────────────────────────────────────────
    if let Some(continue_path) = m.continue_from.clone() {
        return mini_continue_cmd(m, cfg, continue_path, task).await;
    }

    // ── Resume path ──────────────────────────────────────────────────────────
    if let Some(resume_path) = m.resume_from.clone() {
        return mini_resume_cmd(m, cfg, resume_path).await;
    }

    if m.render_only {
        return mini_render_only_cmd(m, task, cfg);
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
        .unwrap_or_else(|| crate::run::mini::slugify(&task));
    let github_pr = mini_github_pr_options(&m, &cfg, &trajectory_name)?;
    let patch_capture = build_patch_capture_spec(
        github_pr.as_ref(),
        resolved_workdir.as_ref(),
        &cfg,
        m.skip_patch_validation,
    );

    let stream_addr = match &m.stream {
        Some(s) => Some(s.parse().map_err(|e: std::net::AddrParseError| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid --stream address `{s}`: {e}"
            )))
        })?),
        None => None,
    };

    let verification_checks = parse_verify_checks(&m.verify)?;
    let interactive_mode = resolve_interactive_mode(m.interactive, m.yolo, m.ui);
    let args = crate::run::mini::MiniArgs {
        task,
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
        resume_from: None,
        interactive_mode,
        trace_id: None,
        webhook_url: m.webhook_url,
        webhook_headers: m.webhook_headers,
        event_log: m.event_log,
        event_log_instance_id: None,
        local_workdir: resolved_workdir,
        read_only: m.read_only,
        allow_mcp_in_read_only: m.allow_mcp_in_read_only,
        rehearsal_gold_patch: None,
        no_step_persist: m.no_step_persist,
        parent_sweep_run_id: None,
        continue_from: None,
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

fn agent_skills_preview_cmd(s: &args::SkillsPreviewCmd) -> Result<(), Error> {
    let cfg = match &s.config {
        Some(p) => crate::config::Config::load(p)?,
        None => crate::config::Config::defaults()?,
    };

    // Collect tasks: --task flags + optional --task-file
    let mut tasks = s.tasks.clone();
    if let Some(ref task_file) = s.task_file {
        let file_tasks = crate::run::skills_preview::read_task_file(task_file)?;
        tasks.extend(file_tasks);
    }

    if tasks.is_empty() {
        exit_with_outcome(
            ExitCode::UsageError,
            "agent skills-preview requires at least one --task or --task-file",
        );
    }

    let result =
        crate::run::skills_preview::preview(&crate::run::skills_preview::SkillsPreviewArgs {
            tasks,
            config: cfg.clone(),
        })?;

    let redactor = crate::redaction::Redactor::from_config_lossy(&cfg.root.redaction);

    match result {
        crate::run::skills_preview::PreviewResult::Disabled(msg) => {
            match s.format.as_str() {
                "json" => {
                    // Return a minimal schema-versioned JSON object so callers that
                    // unconditionally parse stdout as JSON still get valid output.
                    let disabled_json = serde_json::json!({
                        "artifact_kind": "skills_preview",
                        "schema_version": crate::artifact::ArtifactSchemaVersion::CURRENT,
                        "disabled": true,
                        "reason": msg,
                        "tasks": [],
                        "summary": {
                            "task_count": 0,
                            "unique_skills_activated": 0,
                            "p50_bytes_per_task": 0,
                            "p95_bytes_per_task": 0,
                            "tasks_hitting_max_active": 0
                        }
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&disabled_json).map_err(Error::Json)?
                    );
                }
                "text" | "" => {
                    println!("{msg}");
                }
                other => {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "--format '{other}' is not valid; use 'text' or 'json'"
                    ))));
                }
            }
        }
        crate::run::skills_preview::PreviewResult::Report(outcome) => {
            let (report, warnings) = match outcome {
                crate::run::skills_preview::PreviewOutcome::Clean(r) => (r, vec![]),
                crate::run::skills_preview::PreviewOutcome::Warning(r, w) => (r, w),
            };
            match s.format.as_str() {
                "json" => {
                    // Redact structurally (string values only) to avoid corrupting
                    // numeric/boolean fields or key names via text substitution.
                    let mut json_val = serde_json::to_value(&report).map_err(Error::Json)?;
                    redact_json_strings(&mut json_val, &redactor);
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json_val).map_err(Error::Json)?
                    );
                }
                "text" | "" => {
                    print!(
                        "{}",
                        crate::run::skills_preview::format_text(&report, &redactor)
                    );
                }
                other => {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "--format '{other}' is not valid; use 'text' or 'json'"
                    ))));
                }
            }
            if !warnings.is_empty() {
                for w in &warnings {
                    // Redact each warning string before printing to stderr so
                    // skill names containing secret literals are never logged verbatim.
                    let redacted_w = redactor
                        .redact_text(w, crate::redaction::surface::TRAJECTORY)
                        .text;
                    eprintln!("warning: {redacted_w}");
                }
                exit_with_outcome(
                    ExitCode::SkillsPreviewWarning,
                    &format!(
                        "skills-preview completed with {} warning(s)",
                        warnings.len()
                    ),
                );
            }
        }
    }
    Ok(())
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

fn mini_render_only_cmd(
    m: args::MiniCmd,
    task: String,
    cfg: crate::config::Config,
) -> Result<(), Error> {
    crate::run::render_only::reject_incompatible_flags(
        &crate::run::render_only::IncompatibleFlags {
            per_task_budget_usd: m.per_task_budget_usd,
            task_timeout_secs: m.task_timeout_secs,
            stream: m.stream.as_deref(),
            has_verify_checks: !m.verify.is_empty(),
            open_pr: m.github_pr.open_pr,
            pr_dry_run: m.github_pr.github_pr_dry_run,
            webhook_url: m.webhook_url.is_some(),
            webhook_headers: !m.webhook_headers.is_empty(),
        },
    )?;

    let resolved_workdir = resolve_and_validate_workdir(m.workdir.as_ref(), &cfg)?;

    let args = crate::run::render_only::RenderOnlyArgs {
        task,
        extra_context: m.extra_context,
        config: cfg,
        local_workdir: resolved_workdir,
        read_only: m.read_only,
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

/// Handle `mini --resume <path>`.
///
/// Validates the on-disk trajectory, extracts configuration from it (AC #2),
/// and invokes `mini::run()` with `resume_from` populated so the agent
/// continues from the last persisted step without replaying the prefix (AC #3).
#[allow(clippy::too_many_lines)]
async fn mini_resume_cmd(
    m: args::MiniCmd,
    mut cfg: Config,
    resume_path: std::path::PathBuf,
) -> Result<(), Error> {
    // Reject GitHub PR flags — the PR-opening path lives in the non-resume
    // branch. Silently ignoring them would mislead the operator.
    let pr_flags: &[(&str, bool)] = &[
        ("--open-pr", m.github_pr.open_pr),
        ("--github-pr-dry-run", m.github_pr.github_pr_dry_run),
        ("--target-repo", m.github_pr.target_repo.is_some()),
        ("--target-branch", m.github_pr.target_branch.is_some()),
    ];
    let set_pr_flags: Vec<&str> = pr_flags
        .iter()
        .filter_map(|&(name, set)| set.then_some(name))
        .collect();
    if !set_pr_flags.is_empty() {
        exit_with_outcome(
            ExitCode::UsageError,
            &format!(
                "--resume: GitHub PR flags are not supported on resume invocations: {}",
                set_pr_flags.join(", ")
            ),
        );
    }

    // Reject MCP server overrides: the trajectory doesn't store the original
    // MCP configuration, so we cannot validate compatibility. A different tool
    // registry on resume would cause tool-not-found failures or behavior drift.
    if !m.mcp_servers.is_empty() {
        exit_with_outcome(
            ExitCode::UsageError,
            "--resume: --mcp-server overrides are not supported on resume invocations; \
             the original tool registry cannot be restored from the trajectory",
        );
    }

    // Validate extension: the write path is reconstructed as
    // `parent/{stem}.traj.json`, so if the file doesn't end with `.traj.json`
    // the read and write targets would differ. Reject early with a clear error.
    let traj_stem = resume_path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".traj.json"))
        .unwrap_or_else(|| {
            exit_with_outcome(
                ExitCode::UsageError,
                &format!(
                    "--resume: `{}` must end with `.traj.json`; \
                     only trajectory files written by this harness are supported",
                    resume_path.display()
                ),
            )
        })
        .to_owned();

    let traj = load_resume_traj(&resume_path)?;
    validate_resume_or_exit(&traj, &resume_path);

    cfg.root.model.name = traj.info.model_name.clone().unwrap_or_default();
    let task = traj.info.task.clone().unwrap_or_default();
    apply_read_only_policy(&m, &cfg)?;

    if m.resume_allow_step_bump {
        // Apply only caps the operator explicitly set on the resume invocation.
        // Note: task_timeout_secs and per_task_budget_usd are not stored in the
        // trajectory schema today, so they cannot be auto-restored from the
        // checkpoint; the operator must re-supply them with --resume-allow-step-bump.
        if let Some(v) = m.step_limit {
            cfg.root.agent.step_limit = v;
        }
        if let Some(v) = m.per_task_budget_usd {
            cfg.root.agent.per_task_budget_usd = Some(v);
        }
    } else {
        reject_cap_bump_without_flag(&m);
    }

    let stream_addr = match &m.stream {
        Some(s) => Some(s.parse().map_err(|e: std::net::AddrParseError| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid --stream address `{s}`: {e}"
            )))
        })?),
        None => None,
    };

    let verification_checks = parse_verify_checks(&m.verify)?;
    let interactive_mode = resolve_interactive_mode(m.interactive, m.yolo, m.ui);

    let resolved_workdir = match m.workdir.as_ref() {
        Some(w) => resolve_and_validate_workdir(Some(w), &cfg)?,
        None => match &traj.info.local_workdir {
            Some(w) => resolve_and_validate_workdir(Some(&std::path::PathBuf::from(w)), &cfg)?,
            None => None,
        },
    };

    let traj_output_dir = resume_path.parent().map_or_else(
        || std::path::PathBuf::from("."),
        std::path::Path::to_path_buf,
    );

    let args = crate::run::mini::MiniArgs {
        task,
        extra_context: m.extra_context,
        config: cfg,
        output_dir: traj_output_dir,
        trajectory_name: traj_stem,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        task_timeout_secs: m.task_timeout_secs,
        cancellation: None,
        stream_addr,
        patch_capture: None,
        verification_checks,
        verification_timeout_secs: m.verify_timeout_secs,
        resume_from: Some(traj),
        interactive_mode,
        trace_id: None,
        webhook_url: m.webhook_url,
        webhook_headers: m.webhook_headers,
        event_log: m.event_log,
        event_log_instance_id: None,
        local_workdir: resolved_workdir,
        read_only: m.read_only,
        allow_mcp_in_read_only: m.allow_mcp_in_read_only,
        rehearsal_gold_patch: None,
        no_step_persist: m.no_step_persist,
        parent_sweep_run_id: None,
        continue_from: None,
    };
    crate::run::mini::run(args).await
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

/// Handle `mini --continue <path> --task "<follow-up>"`.
///
/// Loads the terminal parent trajectory, appends the follow-up instruction as
/// a new user turn, and runs the agent starting from that point. The result is
/// written to a new trajectory file that records the parent lineage.
#[allow(clippy::too_many_lines)]
async fn mini_continue_cmd(
    m: args::MiniCmd,
    mut cfg: Config,
    continue_path: std::path::PathBuf,
    follow_up_task: String,
) -> Result<(), Error> {
    // Reject GitHub PR flags — same rationale as --resume.
    let pr_flags: &[(&str, bool)] = &[
        ("--open-pr", m.github_pr.open_pr),
        ("--github-pr-dry-run", m.github_pr.github_pr_dry_run),
        ("--target-repo", m.github_pr.target_repo.is_some()),
        ("--target-branch", m.github_pr.target_branch.is_some()),
    ];
    let set_pr_flags: Vec<&str> = pr_flags
        .iter()
        .filter_map(|&(name, set)| set.then_some(name))
        .collect();
    if !set_pr_flags.is_empty() {
        exit_with_outcome(
            ExitCode::UsageError,
            &format!(
                "--continue: GitHub PR flags are not supported on continue invocations: {}",
                set_pr_flags.join(", ")
            ),
        );
    }

    // Reject --format — it requires --render-only, which is already mutually
    // exclusive with --continue; allowing it would silently ignore the format
    // setting after making a paid model call.
    if m.format != "text" {
        exit_with_outcome(
            ExitCode::UsageError,
            "--continue: --format requires --render-only, which cannot be combined with \
             --continue; run without --format or re-render the finished trajectory with \
             --render-only",
        );
    }

    // Reject MCP server overrides — same rationale as --resume.
    if !m.mcp_servers.is_empty() {
        exit_with_outcome(
            ExitCode::UsageError,
            "--continue: --mcp-server overrides are not supported on continue invocations; \
             the original tool registry cannot be restored from the trajectory",
        );
    }

    // Validate extension so read and write targets agree.
    let traj_stem = continue_path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".traj.json"))
        .unwrap_or_else(|| {
            exit_with_outcome(
                ExitCode::UsageError,
                &format!(
                    "--continue: `{}` must end with `.traj.json`; \
                     only trajectory files written by this harness are supported",
                    continue_path.display()
                ),
            )
        })
        .to_owned();

    let traj = load_resume_traj(&continue_path)?;
    validate_continue_or_exit(&traj, &continue_path);

    // Inherit model name, fallback list, step limit, and timeout from parent.
    cfg.root.model.name = traj.info.model_name.clone().unwrap_or_default();
    if let Some(manifest) = &traj.info.manifest {
        cfg.root.agent.step_limit = manifest.step_limit;
        cfg.root.model.fallback_models = manifest.fallback_models.clone();
        // Restore the parent's environment kind unless the operator explicitly
        // overrode it with --env. Prevents a Docker-parent from being silently
        // continued as a local run (or vice-versa) when the operator omits the flag.
        if m.env.is_none() {
            cfg.root.environment.kind = parse_env_kind(&manifest.env_kind)?;
        }
    }
    // task_timeout_secs lives on MiniArgs, not cfg; compute effective value here.
    let effective_task_timeout = m.task_timeout_secs.or_else(|| {
        traj.info
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.task_timeout_secs)
    });
    apply_read_only_policy(&m, &cfg)?;

    if m.hide_budget_from_agent {
        cfg.root.agent.hide_budget_from_agent = true;
    }

    if m.continue_allow_step_bump {
        if let Some(v) = m.step_limit {
            cfg.root.agent.step_limit = v;
        }
        if let Some(v) = m.per_task_budget_usd {
            cfg.root.agent.per_task_budget_usd = Some(v);
        }
    } else {
        reject_cap_bump_without_flag_continue(&m);
    }

    let stream_addr = match &m.stream {
        Some(s) => Some(s.parse().map_err(|e: std::net::AddrParseError| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "invalid --stream address `{s}`: {e}"
            )))
        })?),
        None => None,
    };

    let resolved_workdir = if let Some(w) = m.workdir.as_ref() {
        resolve_and_validate_workdir(Some(w), &cfg)?
    } else if let Some(w) = &traj.info.local_workdir {
        resolve_and_validate_workdir(Some(&std::path::PathBuf::from(w)), &cfg)?
    } else {
        // Neither --workdir nor a recorded workdir in the parent trajectory;
        // the continuation will use the current process directory. Warn so
        // operators know to run from the original checkout.
        tracing::warn!(
            "--continue: parent trajectory has no recorded working directory; \
             the continuation will run in the current process directory. \
             Pass --workdir or run from the original working directory."
        );
        None
    };

    // The child trajectory goes into the same directory as the parent.
    let output_dir = continue_path.parent().map_or_else(
        || std::path::PathBuf::from("."),
        std::path::Path::to_path_buf,
    );

    // Build a unique child trajectory name: {parent_stem}-continue-{task_slug}.
    // If the target already exists (same follow-up run more than once), append a
    // numeric suffix (-2, -3, …) so retries never silently overwrite earlier runs.
    let follow_up_slug = crate::run::mini::slugify(&follow_up_task);
    let base_name = format!("{traj_stem}-continue-{follow_up_slug}");
    let child_traj_name = {
        let candidate = output_dir.join(format!("{base_name}.traj.json"));
        if candidate.exists() {
            let mut n = 2u32;
            loop {
                let suffixed = format!("{base_name}-{n}");
                if !output_dir.join(format!("{suffixed}.traj.json")).exists() {
                    break suffixed;
                }
                n += 1;
            }
        } else {
            base_name
        }
    };

    // Record the parent path as a stable, canonicalized string.
    let parent_path_str = continue_path
        .canonicalize()
        .unwrap_or_else(|_| continue_path.clone())
        .to_string_lossy()
        .to_string();

    let continue_state = crate::run::mini::ContinueState {
        parent_trajectory: traj,
        parent_path: parent_path_str,
        parent_trajectory_id: traj_stem,
        follow_up_task: follow_up_task.clone(),
    };

    let verification_checks = parse_verify_checks(&m.verify)?;
    let interactive_mode = resolve_interactive_mode(m.interactive, m.yolo, m.ui);

    let args = crate::run::mini::MiniArgs {
        task: follow_up_task,
        extra_context: m.extra_context,
        config: cfg,
        output_dir,
        trajectory_name: child_traj_name,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        task_timeout_secs: effective_task_timeout,
        cancellation: None,
        stream_addr,
        patch_capture: None,
        verification_checks,
        verification_timeout_secs: m.verify_timeout_secs,
        resume_from: None,
        continue_from: Some(continue_state),
        interactive_mode,
        trace_id: None,
        webhook_url: m.webhook_url,
        webhook_headers: m.webhook_headers,
        event_log: m.event_log,
        event_log_instance_id: None,
        local_workdir: resolved_workdir,
        read_only: m.read_only,
        allow_mcp_in_read_only: m.allow_mcp_in_read_only,
        rehearsal_gold_patch: None,
        no_step_persist: m.no_step_persist,
        parent_sweep_run_id: None,
    };
    crate::run::mini::run(args).await
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

fn bench_swebench_render_only(s: &args::SwebenchCmd) -> Result<(), Error> {
    crate::run::render_only::reject_incompatible_flags(
        &crate::run::render_only::IncompatibleFlags {
            per_task_budget_usd: s.per_task_budget_usd,
            task_timeout_secs: s.task_timeout_secs,
            stream: None,
            has_verify_checks: false,
            open_pr: s.github_pr.open_prs,
            pr_dry_run: s.github_pr.github_pr_dry_run,
            webhook_url: false,
            webhook_headers: false,
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
        local_workdir: None,
        read_only: false,
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

#[allow(clippy::too_many_lines)]
pub async fn bench_swebench(s: args::SwebenchCmd) -> Result<(), Error> {
    let mut sweep_cmd = s;

    if sweep_cmd.rehearse {
        let file_name = sweep_cmd.output.file_name().ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(
                "rehearsal output path must contain a terminal path segment (cannot be '.', '..', or '/')"
                    .to_owned(),
            ))
        })?;
        let name_str = file_name.to_string_lossy();
        if !name_str.ends_with(".rehearsal") {
            let mut new_name = file_name.to_owned();
            new_name.push(".rehearsal");
            sweep_cmd.output.set_file_name(new_name);
        }
        sweep_cmd.github_pr.open_prs = false;
        sweep_cmd.github_pr.github_pr_dry_run = false;
        sweep_cmd.skip_model_probe = true;
        sweep_cmd.skip_preflight = true;
    }

    if sweep_cmd.diff.is_some() && !sweep_cmd.rehearse {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--diff can only be used in rehearsal mode (with --rehearse)".to_owned(),
        )));
    }

    if sweep_cmd.diff.is_some() && sweep_cmd.dry_run {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--diff cannot be used with --dry-run".to_owned(),
        )));
    }

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

    let is_rehearsal = sweep_cmd.rehearse;
    let skip_evaluator = sweep_cmd.skip_evaluator;
    let eval_backend_str = sweep_cmd.eval_backend.clone();
    let dataset_path_opt = sweep_cmd.dataset_path.clone();
    let dataset_alias_opt = sweep_cmd.dataset.clone();
    let split_str = sweep_cmd.split.clone();
    let parallel_num = sweep_cmd.parallel;
    let diff_path_opt = sweep_cmd.diff.clone();
    let output_dir = sweep_cmd.output.clone();
    let eval_timeout_secs_opt = sweep_cmd.eval_timeout_secs;
    let sb_subset_opt = sweep_cmd.sb_subset.clone();
    let sb_split_opt = sweep_cmd.sb_split.clone();
    let dataset_cache_dir_opt = sweep_cmd.dataset_cache_dir.clone();
    let dry_run = sweep_cmd.dry_run;

    let cfg = swebench_config_from_cmd(&sweep_cmd)?;
    let preflight_mode = if sweep_cmd.dry_run {
        "dry_run"
    } else {
        "sweep"
    };
    let results = Box::pin(crate::run::swebench::run(swebench_args_from_cmd(
        sweep_cmd,
        cfg,
        preflight_mode,
    )?))
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

    if is_rehearsal && !dry_run {
        if skip_evaluator {
            let stale_eval = output_dir.join("evaluation.json");
            if stale_eval.exists() {
                std::fs::remove_file(&stale_eval)?;
            }
        }
        if !skip_evaluator {
            let eval_backend = match eval_backend_str.to_lowercase().as_str() {
                "none" => crate::run::evaluate::EvaluateBackend::None,
                "sb-cli" | "sbcli" => crate::run::evaluate::EvaluateBackend::SbCli,
                "rehearsal" => crate::run::evaluate::EvaluateBackend::Rehearsal,
                other => {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "unknown --eval-backend {other} (expected sb-cli, none, or rehearsal)"
                    ))));
                }
            };

            let actual_dataset_path = if let Some(path) = dataset_path_opt {
                Some(path)
            } else if let Some(alias) = dataset_alias_opt {
                let source = crate::run::dataset::DatasetSource::Named {
                    alias: alias
                        .parse()
                        .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?,
                    split: split_str
                        .unwrap_or_else(|| "test".to_owned())
                        .parse()
                        .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?,
                };
                let cache_dir = dataset_cache_dir_opt
                    .clone()
                    .unwrap_or_else(crate::run::dataset::default_cache_dir);
                let (_, meta) = crate::run::dataset::resolve_dataset(&source, &cache_dir)?;
                Some(meta.path)
            } else {
                None
            };

            let eval_args = crate::run::evaluate::EvaluateArgs {
                sweep_dir: output_dir.clone(),
                dataset_path: actual_dataset_path,
                backend: eval_backend,
                timeout_per_instance_secs: eval_timeout_secs_opt.unwrap_or(1800),
                parallel: parallel_num,
                sb_subset: sb_subset_opt.unwrap_or_default(),
                sb_split: sb_split_opt.unwrap_or_else(|| "test".to_owned()),
                run_id: None,
                breakdown: crate::run::evaluate::BreakdownSelection::default_axes(),
                cost_attribution: true,
            };
            let eval = crate::run::evaluate::run(&eval_args)?;
            let loaded_sweep = crate::run::compare::load_sweep(&output_dir)?;
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
                "rehearsal evaluation complete"
            );
            print!("{}", crate::run::evaluate::render_summary_table(&summary));
        }

        // Run report generator stage
        let report_args = crate::run::report::ReportArgs {
            sweep_dir: output_dir.clone(),
            output: output_dir.join("report.md"),
            baseline: None,
            top_failures: 5,
            format: crate::run::report::ReportFormat::Markdown,
        };
        crate::run::report::run(&report_args)?;

        // If diff_path is Some, run the diff comparator
        if let Some(ref diff_path) = diff_path_opt {
            compare_rehearsals(diff_path, &output_dir)?;
        }
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
    // Non-fatal skills-preview informational section printed first so it
    // appears even when preflight checks subsequently fail. Suppressed in
    // JSON mode because it would corrupt the structured doctor output.
    if output_format != "json" && cfg.root.skills.enabled && !cfg.root.skills.paths.is_empty() {
        print_doctor_skills_preview(&cfg);
    }

    // Non-fatal informational env preview section (issue #313).
    {
        let env_type_label = match cfg.root.environment.kind {
            crate::config::EnvKind::Local => "local",
            crate::config::EnvKind::Docker => "docker",
        };
        let opts = crate::run::env_preview::EnvPreviewOpts {
            env_type: env_type_label.to_owned(),
            task: "(doctor preflight)".into(),
            config_path: s.config.clone(),
            show_values: false,
        };
        let preview = crate::run::env_preview::run_env_preview(&cfg, &opts);
        if output_format != "json" {
            println!("[bench doctor] env preview:");
            print_env_preview_text(&preview);
        }
    }

    // Non-fatal webhook reachability check (issue #315).  Gated on the flag
    // being present; the only paid network call doctor makes.
    #[cfg(feature = "webhook")]
    if let Some(ref url) = s.notify_webhook {
        if output_format != "json" {
            let display = match reqwest::Url::parse(url) {
                Ok(u) => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("<unknown>")),
                Err(_) => "<invalid url>".to_owned(),
            };
            let headers: Vec<(String, String)> = s
                .notify_webhook_headers
                .iter()
                .filter_map(|h| {
                    let mut parts = h.splitn(2, ':');
                    let name = parts.next()?.trim().to_owned();
                    let value = parts.next()?.trim().to_owned();
                    Some((name, value))
                })
                .collect();
            let status = doctor_probe_webhook(url, &headers).await;
            println!("[bench doctor] Webhook reachability: {display} — {status}");
        }
    }

    let results = Box::pin(crate::run::swebench::run(swebench_args_from_cmd(
        s, cfg, "doctor",
    )?))
    .await?;
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
        // `--interactive --yolo` short-circuits to status-line mode — the
        // operator wants live progress on stderr without prompts.
        (_, true) => InteractiveMode::YoloStatusOnly,
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

#[allow(clippy::too_many_lines)]
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
        flake_report: c.flake_report.clone(),
    })?;
    match format {
        crate::run::compare::CompareFormat::Text => {
            print!("{}", report.human_table());
            if let Some(diff) =
                crate::run::behavior::behavior_compare_section(&c.baseline, &c.candidate)
            {
                print!("{diff}");
            }
            if let Some(tp_diff) =
                crate::run::test_progress::test_progress_compare_section(&c.baseline, &c.candidate)
            {
                print!("{tp_diff}");
            }
        }
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
    if let Some(max_rate) = c.max_test_only_resolved_rate {
        if !(0.0..=1.0).contains(&max_rate) {
            exit_with_outcome(
                ExitCode::UsageError,
                "compare: --max-test-only-resolved-rate must be between 0.0 and 1.0",
            );
        }
        match report.candidate_test_only_resolved_rate {
            Some(cand_rate) => {
                if !cand_rate.is_finite() || !(0.0..=1.0).contains(&cand_rate) {
                    exit_with_outcome(
                        ExitCode::UsageError,
                        &format!(
                            "compare: candidate test-only resolved rate ({cand_rate}) is invalid (must be a finite float between 0.0 and 1.0)"
                        ),
                    );
                }
                if cand_rate > max_rate {
                    tracing::error!(
                        max_rate = max_rate,
                        candidate_rate = cand_rate,
                        "compare: candidate test-only resolved rate exceeds --max-test-only-resolved-rate threshold"
                    );
                    exit_with_outcome(
                        ExitCode::EvalGamingGateFailure,
                        &format!(
                            "compare: candidate test-only resolved rate ({:.2}%) exceeds --max-test-only-resolved-rate={:.2}%",
                            cand_rate * 100.0,
                            max_rate * 100.0
                        ),
                    );
                }
            }
            None => {
                exit_with_outcome(
                    ExitCode::UsageError,
                    "compare: candidate evaluation report is missing test-only resolved rate data, required for --max-test-only-resolved-rate gating",
                );
            }
        }
    }
    // Contamination-adjusted resolved rate (text format only — JSON stdout must stay clean)
    if let Some(ref contamination_path) = c.contamination {
        if format == crate::run::compare::CompareFormat::Text {
            print_contamination_adjusted_rate(contamination_path, &report, &c.candidate)?;
        } else {
            eprintln!(
                "compare: note: --contamination is ignored with --format json; \
                 omit --format json to see the contamination-adjusted rate"
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

#[allow(clippy::needless_pass_by_value)]
fn bench_diff_config(c: args::DiffConfigCmd) -> Result<(), Error> {
    let args = crate::run::diff_config::DiffConfigArgs {
        baseline: c.baseline,
        candidate: c.candidate,
        format: c.format,
        fail_on_change: c.fail_on_change,
        ignore: c.ignore,
    };
    crate::run::diff_config::run(&args)
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

fn bench_evaluate(e: args::EvaluateCmd) -> Result<(), Error> {
    let backend = match e.backend.as_str() {
        "sb-cli" => crate::run::evaluate::EvaluateBackend::SbCli,
        "none" => crate::run::evaluate::EvaluateBackend::None,
        "rehearsal" => crate::run::evaluate::EvaluateBackend::Rehearsal,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --backend `{other}` (expected `sb-cli`, `none`, or `rehearsal`)"
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
    let replay_results = Box::pin(crate::run::swebench::run(sweep_args)).await?;

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

    // Handle per-call sampling drift as soft (warn) or hard (abort) divergence.
    if let Some(sd) = &report.sampling_drift {
        if sd.steps_drifted > 0 {
            let drift_field = sd.as_drift_field(r.strict_sampling);
            if r.strict_sampling && !drift_field.is_whitelisted(&r.allow_drift) {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "reproduce: hard sampling drift (--strict-sampling): {}",
                    drift_field.message
                ))));
            }
            tracing::warn!(
                steps_drifted = sd.steps_drifted,
                instances_drifted = sd.instances_drifted,
                "reproduce: soft sampling drift — {}",
                drift_field.message
            );
        }
    }

    print!("{}", render_summary(&report));

    // Surface annotation diff when the original sweep has annotations.json.
    // Scope the diff to replayed instances so partial runs (--filter/--limit)
    // don't report skipped-instance annotations as false drift.
    render_reproduce_annotation_diff(&r.from, &r.output, &replayed_ids);

    Ok(())
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
        if i.output.is_some() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "inspect: --output is only supported with export formats (markdown/html/csv/mermaid)".into(),
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

    if matches!(i.format.as_str(), "markdown" | "html" | "csv" | "mermaid") {
        return bench_inspect_export(i);
    }

    if i.output.is_some() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "inspect: --output is only supported with export formats (markdown/html/csv/mermaid), not `{}`",
            i.format
        ))));
    }

    let format = match i.format.as_str() {
        "text" => crate::run::inspect::InspectFormat::Text,
        "json" => crate::run::inspect::InspectFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text`, `json`, `markdown`, `html`, `csv`, or `mermaid`)"
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
        flake_report: i.flake_report,
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

fn bench_inspect_export(i: args::InspectCmd) -> Result<(), Error> {
    if i.filter.is_some() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "inspect: --format {} cannot be combined with --filter; use --instance",
            i.format
        ))));
    }
    let sweep = i.sweep.ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "inspect: --sweep is required for export formats".into(),
        ))
    })?;
    let instance_id = i.instance.as_deref().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "inspect: --instance is required for export formats (markdown/html/csv/mermaid)".into(),
        ))
    })?;
    let traj_path =
        crate::run::inspect::resolve_trajectory_path(&sweep, instance_id).ok_or_else(|| {
            Error::Trajectory(format!(
                "inspect: trajectory not found for instance `{instance_id}` in {}",
                sweep.display()
            ))
        })?;
    let text = std::fs::read_to_string(&traj_path)?;
    let traj: crate::trajectory::Trajectory = serde_json::from_str(&text)
        .map_err(|e| Error::Trajectory(format!("inspect: failed to parse trajectory: {e}")))?;

    let content = match i.format.as_str() {
        "markdown" => {
            use crate::trajectory::export::{MarkdownExporter, TrajectoryExporter};
            MarkdownExporter::export(&traj)
        }
        "html" => inspect_export_html(&traj)?,
        "csv" => inspect_export_csv(&traj)?,
        "mermaid" => inspect_export_mermaid(&traj)?,
        _ => unreachable!("dispatch guarded by caller"),
    };

    if let Some(output_path) = i.output {
        if let Some(parent) = output_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let out_canon = std::fs::canonicalize(&output_path).unwrap_or_else(|_| output_path.clone());
        let traj_canon = std::fs::canonicalize(&traj_path).unwrap_or_else(|_| traj_path.clone());
        if out_canon == traj_canon {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "inspect: --output `{}` resolves to the source trajectory file; \
                 writing would corrupt the sweep artifact",
                output_path.display()
            ))));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let (Ok(out_meta), Ok(traj_meta)) = (
                std::fs::metadata(&output_path),
                std::fs::metadata(&traj_path),
            ) {
                if out_meta.dev() == traj_meta.dev() && out_meta.ino() == traj_meta.ino() {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "inspect: --output `{}` is a hard link to the source trajectory file; \
                         writing would corrupt the sweep artifact",
                        output_path.display()
                    ))));
                }
            }
        }
        std::fs::write(&output_path, &content)?;
    } else {
        print!("{content}");
    }
    Ok(())
}

#[cfg(feature = "html-export")]
#[allow(clippy::unnecessary_wraps)]
fn inspect_export_html(traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    use crate::trajectory::export::{HtmlExporter, TrajectoryExporter};
    Ok(HtmlExporter::export(traj))
}

#[cfg(not(feature = "html-export"))]
fn inspect_export_html(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "format_unavailable: --format html requires the `html-export` Cargo feature; \
         rebuild with `--features html-export`"
            .into(),
    )))
}

#[cfg(feature = "csv-export")]
#[allow(clippy::unnecessary_wraps)]
fn inspect_export_csv(traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    use crate::trajectory::export::{CsvExporter, TrajectoryExporter};
    Ok(CsvExporter::export(traj))
}

#[cfg(not(feature = "csv-export"))]
fn inspect_export_csv(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "format_unavailable: --format csv requires the `csv-export` Cargo feature; \
         rebuild with `--features csv-export`"
            .into(),
    )))
}

#[cfg(feature = "mermaid-export")]
#[allow(clippy::unnecessary_wraps)]
fn inspect_export_mermaid(traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    use crate::trajectory::export::{MermaidExporter, TrajectoryExporter};
    Ok(MermaidExporter::export(traj))
}

#[cfg(not(feature = "mermaid-export"))]
fn inspect_export_mermaid(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "format_unavailable: --format mermaid requires the `mermaid-export` Cargo feature; \
         rebuild with `--features mermaid-export`"
            .into(),
    )))
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

fn bench_test_progress(t: args::TestProgressCmd) -> Result<(), Error> {
    let is_json = match t.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "test-progress: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let report = crate::run::test_progress::run(&crate::run::test_progress::TestProgressArgs {
        sweep_dir: t.sweep,
        format: t.format,
        bucket: t.bucket.clone(),
        hot_tests_n: t.hot_tests_n,
        filter: t.filter,
        min_tests: t.min_tests,
    })?;
    if is_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            crate::run::test_progress::render_text(&report, t.bucket.as_deref())
        );
    }
    Ok(())
}

fn bench_power(p: &args::PowerCmd) -> Result<(), Error> {
    let is_json = match p.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "power: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let report = crate::run::power::run(p)?;
    if is_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", crate::run::power::render_text(&report));
    }
    Ok(())
}

fn bench_behavior(b: args::BehaviorCmd) -> Result<(), Error> {
    let is_json = match b.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "behavior: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let report = crate::run::behavior::run(&crate::run::behavior::BehaviorArgs {
        sweep_dir: b.sweep,
        bucket: b.bucket.clone(),
        min_share: b.min_share,
        filter: b.filter,
        per_instance: b.per_instance,
    })?;
    if is_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            crate::run::behavior::render_text(
                &report,
                b.bucket.as_deref(),
                b.min_share.unwrap_or(0.0),
            )
        );
    }
    Ok(())
}

fn bench_instance_history(h: args::InstanceHistoryCmd) -> Result<(), Error> {
    // Threshold must be in (0.5, 1.0] — values outside this range produce
    // nonsensical or misleading stability labels.
    if h.stable_threshold <= 0.5 || h.stable_threshold > 1.0 || !h.stable_threshold.is_finite() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "instance-history: --stable-threshold must be in (0.5, 1.0], got {}",
            h.stable_threshold
        ))));
    }
    if let Some(share) = h.max_partial_share {
        if !share.is_finite() || !(0.0..=1.0).contains(&share) {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "instance-history: --max-partial-share must be in [0.0, 1.0], got {share}"
            ))));
        }
    }

    let format = h
        .format
        .parse::<crate::run::instance_history::HistoryFormat>()
        .map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "instance-history: {e}"
            )))
        })?;

    let class_filter = h
        .class
        .as_deref()
        .map(|s| {
            use crate::run::instance_history::StabilityClass;
            match s {
                "stable_win" => Ok(StabilityClass::StableWin),
                "stable_loss" => Ok(StabilityClass::StableLoss),
                "flipper" => Ok(StabilityClass::Flipper),
                "unstable_minority_win" => Ok(StabilityClass::UnstableMinorityWin),
                "unstable_minority_loss" => Ok(StabilityClass::UnstableMinorityLoss),
                other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "instance-history: unknown --class `{other}`"
                )))),
            }
        })
        .transpose()?;

    let report = crate::run::instance_history::compute(
        &crate::run::instance_history::InstanceHistoryArgs {
            sweeps: h.sweeps,
            stable_threshold: h.stable_threshold,
            require_full_coverage: h.require_full_coverage,
            max_partial_share: h.max_partial_share,
            format,
            output: h.output.clone(),
            top: h.top,
            focus: h.focus,
            class_filter,
        },
    )?;

    crate::run::instance_history::write_output(&report, &h.output)?;

    // When --output - is combined with --format text the JSON has already been
    // written to stdout by write_output; skip the text render to avoid mixing.
    let output_is_stdout = h.output == std::path::Path::new("-");
    if format == crate::run::instance_history::HistoryFormat::Text && !output_is_stdout {
        print!(
            "{}",
            crate::run::instance_history::render_text(&report, h.top, h.focus, class_filter,)
        );
    }

    Ok(())
}

fn bench_cache_stats(c: args::CacheStatsCmd) -> Result<(), Error> {
    let is_json = match c.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "cache-stats: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let report = crate::run::cache_stats::run(&crate::run::cache_stats::CacheStatsArgs {
        sweep_dir: c.sweep,
        top: c.top,
        baseline: c.baseline,
    })?;
    if is_json {
        let json = crate::artifact::to_string_pretty(
            crate::artifact::ArtifactKind::CacheStatsReport,
            &report,
        )?;
        println!("{json}");
    } else {
        print!("{}", crate::run::cache_stats::render_text(&report, c.top));
    }
    Ok(())
}

fn bench_budget_fit(b: args::BudgetFitCmd) -> Result<(), Error> {
    if !(0.0..=0.5).contains(&b.at_cap_tolerance) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "budget-fit: --at-cap-tolerance must be in [0.0, 0.5], got {}",
            b.at_cap_tolerance
        ))));
    }
    if b.target_percentile < 50 || b.target_percentile > 99 {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "budget-fit: --target-percentile must be in [50, 99], got {}",
            b.target_percentile
        ))));
    }
    if let Some(ref ax) = b.axis {
        let valid = ["steps", "cost_usd", "wall_clock_s"];
        if !valid.contains(&ax.as_str()) {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: unknown --axis `{ax}` (expected one of: {})",
                valid.join(", ")
            ))));
        }
    }
    let is_json = match b.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "budget-fit: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let report = crate::run::budget_fit::run(&crate::run::budget_fit::BudgetFitArgs {
        sweep_dir: b.sweep,
        at_cap_tolerance: b.at_cap_tolerance,
        target_percentile: b.target_percentile,
        axis: b.axis,
        filter: b.filter,
    })?;
    if is_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", crate::run::budget_fit::render_text(&report));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn bench_tool_ablation(t: args::ToolAblationCmd) -> Result<(), Error> {
    let cache_dir = t
        .dataset_cache_dir
        .clone()
        .unwrap_or_else(crate::run::dataset::default_cache_dir);

    // Validate format before doing any work (needed by both render-only and run paths).
    let is_json_format = match t.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "tool-ablation: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };

    // --format json without --render-only would start a paid run and silently
    // ignore the format flag (the run output is always text).  Catch it early.
    if is_json_format && !t.render_only {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--format json is only valid with --render-only; \
             omit --format or add --render-only"
                .into(),
        )));
    }

    // render-only only needs the config; dataset is not required.
    if t.render_only {
        let cfg = crate::config::Config::load(&t.config).map_err(Error::Config)?;
        let all_tools = crate::run::tool_ablation::enumerate_tools(&cfg);

        // Validate --ablate names up front so render-only rejects unknown names
        // the same way a real run would, rather than silently dropping them.
        if !t.ablate.is_empty() {
            let unknown: Vec<&str> = t
                .ablate
                .iter()
                .map(String::as_str)
                .filter(|name| !all_tools.iter().any(|t| t == *name))
                .collect();
            if !unknown.is_empty() {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "tool-ablation: unknown tool name(s) in --ablate: {}",
                    unknown.join(", ")
                ))));
            }
        }

        let arm_plan = crate::run::tool_ablation::generate_arm_plan(
            &all_tools,
            &t.ablate,
            t.include_pair_ablation,
        );

        // Reject duplicate arm names now so --render-only previews the same
        // validity constraints as a real run (pair collisions such as
        // `(a, b__c)` and `(a__b, c)` both produce the name `pair_a__b__c`).
        {
            let mut seen = std::collections::HashSet::new();
            for arm in &arm_plan {
                if !seen.insert(arm.name.as_str()) {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "ambiguous arm name `{}`; rename conflicting tools to avoid collision",
                        arm.name
                    ))));
                }
            }
        }

        if t.include_pair_ablation {
            let pair_count = arm_plan.iter().filter(|a| a.ablated_pair.is_some()).count();
            eprintln!(
                "bench tool-ablation: --include-pair-ablation adds {pair_count} pair arm(s) \
                 (total {} arms)",
                arm_plan.len()
            );
        }

        let manifest = crate::run::tool_ablation::ArmManifest {
            schema_version: "tool-ablation-1.0".into(),
            config_path: t.config.display().to_string(),
            arms: arm_plan,
        };

        if is_json_format {
            println!(
                "{}",
                crate::run::tool_ablation::render_manifest_json(&manifest)?
            );
        } else {
            print!(
                "{}",
                crate::run::tool_ablation::render_manifest_text(&manifest)
            );
        }
        return Ok(());
    }

    // Dataset is required for the non-render-only sweep path.
    let dataset_source = match (&t.dataset_path, &t.dataset) {
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
            let split_str = t.split.as_deref().unwrap_or("test");
            let split = split_str
                .parse::<crate::run::dataset::SwebenchSplit>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            crate::run::dataset::DatasetSource::Named { alias, split }
        }
    };

    let ablation_args = crate::run::tool_ablation::ToolAblationArgs {
        config_path: t.config,
        dataset_source,
        dataset_cache_dir: cache_dir,
        output_dir: t.output,
        ablate: t.ablate,
        sweep_cost_limit_usd: t.sweep_cost_limit_usd,
        matrix_parallelism: t.matrix_parallelism,
        resume: t.resume,
        instance_ids: t.instance_ids,
        limit: t.limit,
        sample: t.sample,
        seed: t.seed,
        parallel: t.parallel,
        include_pair_ablation: t.include_pair_ablation,
        skip_preflight: t.skip_preflight,
        skip_model_probe: t.skip_model_probe,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        cancel_deadline_secs: t.cancel_deadline_secs,
        install_os_signal_handlers: true,
    };

    let report = Box::pin(crate::run::tool_ablation::run(ablation_args)).await?;
    print!(
        "{}",
        crate::run::tool_ablation::render_text_summary(&report)
    );
    if report.systemic_halt {
        exit_with_outcome(
            ExitCode::SystemicHalt,
            "an ablation arm hit the systemic-failure circuit breaker",
        );
    }
    if let Some(code) = report.cancel_exit_code {
        let outcome = if code == crate::run::swebench::CANCEL_EXIT_CODE_GRACEFUL {
            ExitCode::Interrupted
        } else {
            ExitCode::Killed
        };
        exit_with_outcome(outcome, "ablation was cancelled");
    }
    Ok(())
}

fn bench_ladder(l: args::LadderCmd) -> Result<(), Error> {
    let format = l
        .format
        .parse::<crate::run::ladder::LadderFormat>()
        .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(format!("ladder: {e}"))))?;
    let report = crate::run::ladder::run(&crate::run::ladder::LadderArgs {
        root: l.root,
        dataset: l.dataset,
        last: l.last,
        baseline: l.baseline,
        format,
    })?;
    match format {
        crate::run::ladder::LadderFormat::Text => {
            print!("{}", crate::run::ladder::render_text(&report));
        }
        crate::run::ladder::LadderFormat::Json => {
            let json = crate::run::ladder::render_json(&report)?;
            println!("{json}");
        }
        crate::run::ladder::LadderFormat::Markdown => {
            print!("{}", crate::run::ladder::render_markdown(&report));
        }
    }
    Ok(())
}

fn bench_stagnation_report(s: args::StagnationReportCmd) -> Result<(), Error> {
    let format = s
        .format
        .parse::<crate::run::stagnation_report::StagnationReportFormat>()
        .map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "stagnation-report: {e}"
            )))
        })?;
    let report =
        crate::run::stagnation_report::run(&crate::run::stagnation_report::StagnationReportArgs {
            sweep: s.sweep,
            format,
        })?;
    match format {
        crate::run::stagnation_report::StagnationReportFormat::Text => {
            print!("{}", crate::run::stagnation_report::render_text(&report));
        }
        crate::run::stagnation_report::StagnationReportFormat::Json => {
            let json = crate::run::stagnation_report::render_json(&report)?;
            println!("{json}");
        }
    }
    Ok(())
}

fn bench_self_check(s: args::SelfCheckCmd) -> Result<(), Error> {
    let args = crate::run::self_check::SelfCheckArgs {
        sweep_dir: s.sweep,
        format: s.format.clone(),
        list: s.list,
        by_repo: s.by_repo,
    };
    let report = crate::run::self_check::run(&args)?;
    match s.format.as_str() {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(Error::Json)?
            );
        }
        "text" => {
            print!("{}", crate::run::self_check::render_text(&report, s.list));
        }
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "bench self-check: --format '{other}' is not valid; use 'text' or 'json'"
            ))));
        }
    }
    Ok(())
}

fn bench_export_ci(c: args::ExportCiCmd) -> Result<(), Error> {
    use crate::run::export_ci::{ExportCiArgs, ExportCiFormat};

    let format = match c.format {
        args::ExportCiFormatArg::Junit => ExportCiFormat::Junit,
        args::ExportCiFormatArg::GithubAnnotations => ExportCiFormat::GithubAnnotations,
        args::ExportCiFormatArg::Both => ExportCiFormat::Both,
    };

    let result = crate::run::export_ci::run(&ExportCiArgs {
        sweep_dir: c.sweep,
        format,
        output: c.output,
    })?;

    if result.integrity_violation {
        exit_with_outcome(
            ExitCode::ArtifactIntegrityViolation,
            &format!(
                "bench export-ci: JUnit aggregate attributes do not match results.json counts \
                 (tests: xml={} json={}; failures: xml={} json={}; errors: xml={} json={})",
                result.xml_tests,
                result.json_total,
                result.xml_failures,
                result.json_failures,
                result.xml_errors,
                result.json_errors,
            ),
        );
    }

    Ok(())
}

fn bench_contamination_check(c: args::ContaminationCheckCmd) -> Result<(), Error> {
    use crate::run::contamination_check::{ContaminationCheckArgs, ContaminationReport};

    if let Some(threshold) = c.fail_on_high {
        if !(0.0..=1.0).contains(&threshold) {
            exit_with_outcome(
                ExitCode::UsageError,
                "contamination-check: --fail-on-high must be between 0.0 and 1.0",
            );
        }
    }

    let args = ContaminationCheckArgs {
        sweep_dir: c.sweep,
        output: c.output,
        config: c.config,
        fail_on_high: c.fail_on_high,
    };

    let report: ContaminationReport = crate::run::contamination_check::run(&args)?;

    let total = report.summary.total_resolved;
    let high = report.summary.high_count;
    let high_share = report.summary.high_risk_share;

    if total == 0 {
        eprintln!("contamination-check: no resolved instances found; contamination.json written");
    } else {
        eprintln!(
            "contamination-check: {total} resolved instance(s) scored \
             — low: {low}, medium: {med}, high: {high} ({pct:.1}%)",
            low = report.summary.low_count,
            med = report.summary.medium_count,
            pct = high_share * 100.0,
        );
    }

    if let Some(threshold) = c.fail_on_high {
        if high_share > threshold {
            exit_with_outcome(
                ExitCode::PreflightFailure,
                &format!(
                    "contamination-check: high-risk share {pct:.1}% exceeds \
                     --fail-on-high threshold {thr:.1}% ({high} of {total} resolved)",
                    pct = high_share * 100.0,
                    thr = threshold * 100.0,
                ),
            );
        }
    }

    Ok(())
}

async fn bench_scriptability_check(cmd: args::ScriptabilityCheckCmd) -> Result<(), Error> {
    use crate::run::scriptability_check::{ScriptabilityCheckArgs, render_text};

    // value_parser = ["text", "json"] on the arg ensures only valid values reach here.
    let is_json = cmd.format == "json";

    let args = ScriptabilityCheckArgs {
        config_path: cmd.config,
        output: cmd.output,
    };

    let report = crate::run::scriptability_check::run(&args).await?;

    if is_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }

    if !report.all_ok {
        let failed_servers = report.servers.iter().filter(|s| !s.ok).count();
        let failed_hooks = report.hooks.iter().filter(|h| !h.ok).count();
        exit_with_outcome(
            ExitCode::ScriptabilityCheckFailure,
            &format!(
                "scriptability check failed: {failed_servers} server(s) and \
                 {failed_hooks} hook(s) had failures"
            ),
        );
    }

    Ok(())
}

fn bench_near_miss(n: args::NearMissCmd) -> Result<(), Error> {
    use crate::run::near_miss::{NearMissArgs, NearMissFormat, render_json, render_text, run};

    let format: NearMissFormat = n.format.parse().map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "bench near-miss: {e}"
        )))
    })?;

    let args = NearMissArgs {
        sweep: n.sweep,
        top: n.top,
        format,
    };

    let report = run(&args).unwrap_or_else(|e| {
        // Missing evaluation.json → usage error (exit 2); parse failure → internal error (exit 1).
        let code = if matches!(e, Error::Trajectory(_)) {
            ExitCode::UsageError
        } else {
            ExitCode::InternalError
        };
        exit_with_outcome(code, &format!("bench near-miss: {e}"));
    });

    match format {
        NearMissFormat::Text => print!("{}", render_text(&report)),
        NearMissFormat::Json => println!("{}", render_json(&report)?),
    }

    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn bench_assert(a: args::AssertCmd) -> Result<(), Error> {
    use crate::run::assert::{AssertArgs, run_assert};

    let args = AssertArgs {
        sweep: a.sweep,
        rules_file: a.rules,
        inline_rules: a.rule,
        verbose: a.verbose,
        allow_missing_artifacts: a.allow_missing_artifacts,
    };

    let report = run_assert(&args).unwrap_or_else(|e| {
        let code = if matches!(e, Error::Config(_)) {
            ExitCode::UsageError
        } else {
            ExitCode::InternalError
        };
        exit_with_outcome(code, &format!("bench assert: {e}"));
    });

    print!("{}", report.stdout);
    if !report.all_passed {
        exit_with_outcome(
            ExitCode::SloRuleFailure,
            "bench assert: at least one SLO rule failed",
        );
    }
    Ok(())
}

fn bench_import(i: args::ImportCmd) -> Result<(), Error> {
    let format = match i.format.as_str() {
        "json" => crate::run::import::ImportFormat::Json,
        "text" => crate::run::import::ImportFormat::Text,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "bench import: --format '{other}' is not valid; use 'text' or 'json'"
            ))));
        }
    };
    let args = crate::run::import::ImportArgs {
        predictions: i.predictions,
        dataset_path: i.dataset_path,
        output: i.output,
        evaluate: i.evaluate,
        format,
    };
    let summary = crate::run::import::run(&args)?;
    match format {
        crate::run::import::ImportFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).map_err(Error::Json)?
            );
        }
        crate::run::import::ImportFormat::Text => {
            print!("{}", crate::run::import::format_summary_text(&summary));
        }
    }
    Ok(())
}

async fn bench_cascade(c: args::CascadeCmd) -> Result<(), Error> {
    let cache_dir = c
        .dataset_cache_dir
        .clone()
        .unwrap_or_else(crate::run::dataset::default_cache_dir);

    let dataset_source = match (&c.dataset_path, &c.dataset) {
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
            let split_str = c.split.as_deref().unwrap_or("test");
            let split = split_str
                .parse::<crate::run::dataset::SwebenchSplit>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            crate::run::dataset::DatasetSource::Named { alias, split }
        }
    };

    let eval_backend = match c.eval_backend.to_lowercase().as_str() {
        "sb-cli" | "sbcli" => crate::run::evaluate::EvaluateBackend::SbCli,
        "none" => crate::run::evaluate::EvaluateBackend::None,
        "rehearsal" => crate::run::evaluate::EvaluateBackend::Rehearsal,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --eval-backend `{other}`; expected `sb-cli` or `rehearsal`"
            ))));
        }
    };

    let cascade_args = crate::run::cascade::CascadeArgs {
        config_path: c.config,
        dataset_source,
        dataset_cache_dir: cache_dir,
        output_dir: c.output,
        instance_ids: c.instance_ids,
        limit: c.limit,
        sample: c.sample,
        seed: c.seed,
        stratify_by: c.stratify_by.map(|v| match v {
            args::StratifyByArg::Repo => crate::run::swebench::StratifyBy::Repo,
        }),
        stratify_mode: match c
            .stratify_mode
            .unwrap_or(args::StratifyModeArg::Proportional)
        {
            args::StratifyModeArg::Proportional => crate::run::swebench::StratifyMode::Proportional,
            args::StratifyModeArg::Balanced => crate::run::swebench::StratifyMode::Balanced,
        },
        sweep_cost_limit_usd: c.sweep_cost_limit_usd,
        resume: c.resume,
        parallel: c.parallel,
        skip_preflight: c.skip_preflight,
        skip_model_probe: c.skip_model_probe,
        eval_backend,
        sb_subset: c.sb_subset,
        sb_split: c.sb_split,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        eval_timeout_per_instance_secs: c.eval_timeout_per_instance_secs,
        cancel_deadline_secs: c.cancel_deadline_secs,
        install_os_signal_handlers: true,
        mock_eval_resolved_ids: None,
    };

    let _summary = Box::pin(crate::run::cascade::run(cascade_args)).await?;
    Ok(())
}

fn bench_tool_coverage(t: args::ToolCoverageCmd) -> Result<(), Error> {
    let is_json = match t.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "tool-coverage: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let bucket = t.bucket.clone();
    let report = crate::run::tool_coverage::run(&crate::run::tool_coverage::ToolCoverageArgs {
        sweep_dir: t.sweep,
        bucket: t.bucket,
        filter: t.filter,
        min_invocations: t.min_invocations,
        per_instance: t.per_instance,
    })?;
    if is_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            crate::run::tool_coverage::render_text(&report, bucket.as_deref(), t.min_invocations,)
        );
    }
    Ok(())
}

fn bench_policy_impact(t: args::PolicyImpactCmd) -> Result<(), Error> {
    let is_json = match t.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "policy-impact: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let report = crate::run::policy_impact::run(&crate::run::policy_impact::PolicyImpactArgs {
        sweep_dir: t.sweep,
    })?;
    if is_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", crate::run::policy_impact::render_text(&report));
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

fn bench_triage_diff(t: args::TriageDiffCmd) -> Result<(), Error> {
    let is_json = match t.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };

    let report = crate::run::triage_diff::run(&crate::run::triage_diff::TriageDiffArgs {
        baseline_dir: t.baseline,
        candidate_dir: t.candidate,
        auto_triage: t.auto_triage,
        min_cluster_size: t.min_cluster_size,
        top: t.top,
        output: t.output,
        format: t.format,
        fail_on_regression: t.fail_on_regression,
    })?;

    if is_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", crate::run::triage_diff::render_text(&report, t.top));
    }

    if t.fail_on_regression && !report.regression_instances.is_empty() {
        exit_with_outcome(
            ExitCode::RegressionGateFailure,
            &format!(
                "Triage diff contains {} regression cluster(s).",
                report.regression_instances.len()
            ),
        );
    }

    Ok(())
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

    let summary = Box::pin(crate::run::matrix::run(matrix_args)).await?;

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
    let retry_results = match Box::pin(crate::run::swebench::run(sweep_args)).await {
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

#[allow(clippy::needless_pass_by_value)]
fn bench_dataset_stats(s: args::DatasetStatsCmd) -> Result<(), Error> {
    if s.format != "text" && s.format != "json" {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "dataset-stats: unknown --format `{}`; valid values: text, json",
            s.format
        ))));
    }
    let (dataset_source, dataset_cache_dir) = parse_dataset_source_stats(&s)?;
    let (dataset_bytes, meta) =
        crate::run::dataset::resolve_dataset(&dataset_source, &dataset_cache_dir)?;
    let full_instances = crate::run::swebench::load_dataset_from_bytes_pub(&dataset_bytes)?;

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

    let params = crate::run::swebench::ApplySubsetParams {
        instance_ids_arg: s.instance_ids.as_deref(),
        limit: s.limit,
        sample: s.sample,
        seed: s.seed,
        stratify_by,
        stratify_mode,
    };

    let mut light_full = Vec::with_capacity(full_instances.len());
    for inst in &full_instances {
        light_full.push(crate::run::swebench::SweBenchInstance {
            instance_id: inst.instance_id.clone(),
            repo: inst.repo.clone(),
            base_commit: None,
            problem_statement: inst.problem_statement.clone(),
            image: None,
            other: serde_json::Map::new(),
        });
    }

    let (slice_instances, _filter_spec) =
        crate::run::swebench::apply_subset(full_instances, &params)?;

    // Compute stats
    let mut stats = crate::run::dataset_stats::compute_stats(
        &slice_instances,
        &light_full,
        &s.model,
        &s.runs_dir,
        &Some(meta.sha256.clone()),
    )?;

    // Populate dataset stats fields
    stats.dataset_path = meta.path.display().to_string();
    stats.subset_selector.limit = s.limit;
    stats.subset_selector.sample = s.sample;
    stats.subset_selector.seed = s.seed;
    stats
        .subset_selector
        .instance_ids
        .clone_from(&s.instance_ids);
    stats.subset_selector.stratify_by = s.stratify_by.map(|v| match v {
        args::StratifyByArg::Repo => "repo".to_owned(),
    });
    stats.subset_selector.stratify_mode = if s.stratify_by.is_some() {
        Some(
            match s
                .stratify_mode
                .unwrap_or(args::StratifyModeArg::Proportional)
            {
                args::StratifyModeArg::Proportional => "proportional".to_owned(),
                args::StratifyModeArg::Balanced => "balanced".to_owned(),
            },
        )
    } else {
        None
    };

    if s.format == "json" {
        let serialized = serde_json::to_string_pretty(&stats)?;
        println!("{serialized}");
    } else {
        let text = crate::run::dataset_stats::render_text(&stats);
        println!("{text}");
    }

    Ok(())
}

async fn bench_bisect(b: args::BisectCmd) -> Result<(), Error> {
    crate::run::bisect::run(&b).await
}

#[allow(clippy::needless_pass_by_value)]
fn bench_audit(a: args::AuditCmd) -> Result<(), Error> {
    crate::run::audit::run(&a)
}

fn bench_failure_digest(f: args::FailureDigestCmd) -> Result<(), Error> {
    let format = match f.format.as_str() {
        "markdown" => crate::run::failure_digest::DigestFormat::Markdown,
        "json" => crate::run::failure_digest::DigestFormat::Json,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --format `{other}` (expected `markdown` or `json`)"
            ))));
        }
    };
    let max_chars = f.max_chars;
    let digest = crate::run::failure_digest::run(&crate::run::failure_digest::FailureDigestArgs {
        sweep_dir: f.sweep,
        instance: f.instance,
        format,
        max_chars,
    })?;
    match format {
        crate::run::failure_digest::DigestFormat::Markdown => {
            print!(
                "{}",
                crate::run::failure_digest::render_markdown(&digest, max_chars)
            );
            Ok(())
        }
        crate::run::failure_digest::DigestFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&digest)?);
            Ok(())
        }
    }
}

fn bench_eval_flake(f: args::EvalFlakeCmd) -> Result<(), Error> {
    let args = crate::run::eval_flake::EvalFlakeArgs {
        sweep_dir: f.sweep,
        replays: f.replays,
        output: f.output,
        concurrency: f.concurrency,
    };
    let report = crate::run::eval_flake::run(&args)?;
    let summary = &report.summary;
    eprintln!(
        "eval-flake: {} instance(s) evaluated, {} flaky ({:.1}% flake rate), {} disagree with original sweep verdict",
        summary.instances_evaluated,
        summary.flaky_count,
        summary.flaky_rate * 100.0,
        summary.dominant_disagrees_with_sweep_count,
    );
    eprintln!("eval-flake: total_cost_usd=0.00 (evaluator wallclock only)");
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(Error::Json)?
    );
    Ok(())
}

fn bench_annotate(a: args::AnnotateCmd) -> Result<(), Error> {
    use crate::run::annotate::{
        AnnotateAddArgs, AnnotateListArgs, AnnotateRmArgs, render_add_text, render_list_text,
        render_rm_text, run_add, run_list, run_rm,
    };
    match a.cmd {
        args::AnnotateSubCmd::Add(cmd) => {
            let args = AnnotateAddArgs {
                instance_id: cmd.instance_id,
                tags: cmd.tag,
                note: cmd.note,
                store: cmd.store,
            };
            let report = run_add(&args)?;
            eprint!("{}", render_add_text(&report));
            Ok(())
        }
        args::AnnotateSubCmd::List(cmd) => {
            let args = AnnotateListArgs {
                instance: cmd.instance,
                tag: cmd.tag,
                store: cmd.store,
            };
            let report = run_list(&args)?;
            match cmd.format.as_str() {
                "json" => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).map_err(Error::Json)?
                    );
                }
                "text" => {
                    print!("{}", render_list_text(&report));
                }
                other => {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "--format '{other}' is not valid; use 'text' or 'json'"
                    ))));
                }
            }
            Ok(())
        }
        args::AnnotateSubCmd::Rm(cmd) => {
            let args = AnnotateRmArgs {
                instance_id: cmd.instance_id,
                tag: cmd.tag,
                store: cmd.store,
            };
            let report = run_rm(&args)?;
            eprint!("{}", render_rm_text(&report));
            Ok(())
        }
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

async fn agent_suite_cmd(s: args::SuiteCmd) -> Result<(), Error> {
    let mut cfg = match &s.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };

    cfg.root.model.name.clone_from(&s.model);

    if let Some(v) = s.step_limit {
        cfg.root.agent.step_limit = v;
    }
    if let Some(v) = s.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(v);
    }
    if let Some(kind) = &s.env {
        cfg.root.environment.kind = parse_env_kind(kind.as_str())?;
    }
    if let Some(img) = s.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }
    if let Some(v) = s.detect_stagnation {
        cfg.root.agent.detect_stagnation = v;
    }
    if let Some(v) = s.history_max_input_tokens {
        cfg.root.agent.history_max_input_tokens = Some(v);
    }
    if let Some(v) = s.history_keep_last_observations {
        cfg.root.agent.history_keep_last_observations = Some(v);
    }
    apply_mcp_server_overrides(&mut cfg, &s.mcp_servers)?;

    let suite_name = s.suite_name.clone().unwrap_or_else(|| {
        s.tasks_file
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("suite")
            .to_owned()
    });

    let suite_args = crate::run::suite::SuiteArgs {
        tasks_file: s.tasks_file,
        format_override: s.format,
        suite_name,
        config: cfg,
        output_dir: s.output,
        suite_cost_limit_usd: s.suite_cost_limit_usd,
        verify: s.verify,
        verify_timeout_secs: s.verify_timeout_secs,
        resume: s.resume,
        task_timeout_secs: s.task_timeout_secs,
        step_limit: s.step_limit,
        per_task_budget_usd: s.per_task_budget_usd,
    };

    let exit_code = crate::run::suite::run(suite_args).await?;
    if exit_code != ExitCode::Success {
        exit_with_outcome(exit_code, "suite completed with failures");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        Cli, args, cancellation_exit_code, maybe_publish_mini_github_pr, mini_github_pr_options,
        parse_verify_checks, required_github_arg, resolve_interactive_mode, swebench_args_from_cmd,
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

    #[tokio::test]
    async fn mini_github_pr_publish_helper_respects_submission_state() {
        let work = tempfile::tempdir().unwrap();
        let submitted = work.path().join("submitted.traj.json");
        write_trajectory(&submitted, Some(outcome::SUBMITTED));
        let patch = work.path().join("submitted.patch");
        std::fs::write(&patch, sample_patch()).unwrap();

        maybe_publish_mini_github_pr(Some(crate::run::github_pr::GithubPrOptions {
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
            webhook_url: None,
            webhook_headers: vec![],
            no_step_persist: false,
            chaos_fail_every: 0,
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
            crate::run::mini::InteractiveMode::YoloStatusOnly
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
        let res = super::bench_dataset_stats(cmd);
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
}
