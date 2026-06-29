//! Command-line interface. `clap` derive; subcommand dispatch.
// The dispatch functions in this module call large async subsystems.  The
// Box::pin calls on the hot paths heap-allocate the inner futures, but the
// outer dispatch state machines can still cross the 16 KiB threshold on some
// compiler builds.  The lint is informational here; the allocation behaviour
// is already correct.

use std::io::{IsTerminal as _, Read as _, Write as _};

use clap::{Parser, Subcommand};

use crate::error::Error;
use crate::exit_code::ExitCode;

pub mod handlers;
pub mod args;
pub mod catalog;
pub mod explain;

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
    let log = handlers::effective_log_level(cli.log.as_deref());
    handlers::init_logging(&log);

    match cli.command {
        Command::Mini(m) => handlers::mini_cmd(*m).await,
        Command::HelloWorld(h) => {
            crate::run::hello_world::main(h.output, h.config.as_deref()).await
        }
        Command::Replay(r) => handlers::replay_cmd(*r).await,
        Command::Bench { cmd } => match *cmd {
            args::BenchCmd::Swebench(s) => Box::pin(handlers::bench_swebench(*s)).await,
            args::BenchCmd::Rehearsal(mut s) => {
                s.rehearse = true;
                Box::pin(handlers::bench_swebench(*s)).await
            }
            args::BenchCmd::Forecast(s) => Box::pin(handlers::bench_forecast(*s)).await,
            args::BenchCmd::Calibrate(c) => handlers::bench_calibrate(c),
            args::BenchCmd::Doctor(s) => Box::pin(handlers::bench_doctor(*s)).await,
            args::BenchCmd::Compare(c) => handlers::bench_compare(c),
            args::BenchCmd::DiffConfig(c) => handlers::bench_diff_config(c),
            args::BenchCmd::Evaluate(e) => handlers::bench_evaluate(e),
            args::BenchCmd::Inspect(i) => handlers::bench_inspect(i),
            args::BenchCmd::Tail(t) => handlers::bench_tail(t).await,
            args::BenchCmd::Watch(w) => handlers::bench_watch(w).await,
            args::BenchCmd::Triage(t) => handlers::bench_triage(t),
            args::BenchCmd::TriageDiff(t) => handlers::bench_triage_diff(t),
            args::BenchCmd::CommandStats(c) => handlers::bench_command_stats(c),
            args::BenchCmd::Grep(g) => handlers::bench_grep(g),
            args::BenchCmd::Events(e) => handlers::bench_events(e),
            args::BenchCmd::Frontier(f) => handlers::bench_frontier(f),
            args::BenchCmd::Reproduce(r) => Box::pin(handlers::bench_reproduce(r)).await,
            args::BenchCmd::Bundle(b) => handlers::bench_bundle(b),
            args::BenchCmd::Matrix(m) => Box::pin(handlers::bench_matrix(m)).await,
            args::BenchCmd::EvaluatorSelftest(s) => handlers::bench_evaluator_selftest(s),
            args::BenchCmd::Report(r) => handlers::bench_report(r),
            args::BenchCmd::Retry(r) => Box::pin(handlers::bench_retry(r)).await,
            args::BenchCmd::Behavior(b) => handlers::bench_behavior(b),
            args::BenchCmd::ToolCoverage(t) => handlers::bench_tool_coverage(t),
            args::BenchCmd::SkillCoverage(t) => handlers::bench_skill_coverage(t),
            args::BenchCmd::PolicyImpact(p) => handlers::bench_policy_impact(p),
            args::BenchCmd::InstanceHistory(h) => handlers::bench_instance_history(h),
            args::BenchCmd::CacheStats(c) => handlers::bench_cache_stats(c),
            args::BenchCmd::ContextPressure(c) => handlers::bench_context_pressure(c),
            args::BenchCmd::BudgetFit(b) => handlers::bench_budget_fit(b),
            args::BenchCmd::ToolAblation(t) => Box::pin(handlers::bench_tool_ablation(t)).await,
            args::BenchCmd::Ladder(l) => handlers::bench_ladder(l),
            args::BenchCmd::Cascade(c) => Box::pin(handlers::bench_cascade(c)).await,
            args::BenchCmd::TestProgress(t) => handlers::bench_test_progress(t),
            args::BenchCmd::Fork(f) => Box::pin(crate::run::fork::run(f)).await,
            args::BenchCmd::Power(p) => handlers::bench_power(&p),
            args::BenchCmd::DatasetStats(s) => handlers::bench_dataset_stats(s),
            args::BenchCmd::DatasetVerify(s) => handlers::bench_dataset_verify(s),
            args::BenchCmd::Bisect(b) => Box::pin(handlers::bench_bisect(b)).await,
            args::BenchCmd::Audit(a) => handlers::bench_audit(a),
            args::BenchCmd::FailureDigest(f) => handlers::bench_failure_digest(f),
            args::BenchCmd::EvalFlake(f) => handlers::bench_eval_flake(f),
            args::BenchCmd::Annotate(a) => handlers::bench_annotate(a),
            args::BenchCmd::StagnationReport(s) => handlers::bench_stagnation_report(s),
            args::BenchCmd::SelfCheck(s) => handlers::bench_self_check(s),
            args::BenchCmd::Import(i) => handlers::bench_import(i),
            args::BenchCmd::ExportCi(c) => handlers::bench_export_ci(c),
            args::BenchCmd::ContaminationCheck(c) => handlers::bench_contamination_check(c),
            args::BenchCmd::ScriptabilityCheck(s) => Box::pin(handlers::bench_scriptability_check(s)).await,
            args::BenchCmd::NearMiss(n) => handlers::bench_near_miss(n),
            args::BenchCmd::Assert(a) => handlers::bench_assert(a),
            args::BenchCmd::Subset(s) => handlers::bench_subset(s),
            args::BenchCmd::EvalParity(p) => handlers::bench_eval_parity(p),
            args::BenchCmd::Utilization(u) => handlers::bench_utilization(u),
            args::BenchCmd::ExportOtlp(c) => Box::pin(handlers::bench_export_otlp(c)).await,
            args::BenchCmd::Variance(v) => handlers::bench_variance(v),
            args::BenchCmd::Merge(m) => handlers::bench_merge(&m),
            args::BenchCmd::Shard(s) => handlers::bench_shard(s),
            args::BenchCmd::Ledger(l) => handlers::bench_ledger(l),
        },
        Command::Agent { cmd } => match *cmd {
            args::AgentCmd::SkillsPreview(s) => handlers::agent_skills_preview_cmd(&s),
            args::AgentCmd::RedactCheck(r) => handlers::agent_redact_check_cmd(&r),
            args::AgentCmd::RedactAudit(a) => handlers::agent_redact_audit_cmd(&a),
            args::AgentCmd::InjectionAudit(a) => handlers::agent_injection_audit_cmd(&a),
            args::AgentCmd::Env {
                cmd: args::AgentEnvCmd::Preview(ref p),
            } => handlers::agent_env_preview_cmd(p),
            args::AgentCmd::Config {
                cmd: args::AgentConfigCmd::Resolve(ref r),
            } => handlers::agent_config_resolve_cmd(r),
            args::AgentCmd::Stability(s) => Box::pin(handlers::agent_stability_cmd(*s)).await,
            args::AgentCmd::Suite(s) => Box::pin(handlers::agent_suite_cmd(*s)).await,
            args::AgentCmd::PolicyCheck(p) => handlers::agent_policy_check_cmd(&p),
            args::AgentCmd::Apply(a) => handlers::agent_apply_cmd(&a),
            args::AgentCmd::BestOf(b) => Box::pin(handlers::agent_best_of_cmd(*b)).await,
            args::AgentCmd::Profile(p) => handlers::agent_profile_cmd(&p),
            args::AgentCmd::Runs(r) => handlers::agent_runs_cmd(&r),
            args::AgentCmd::FsAudit(a) => handlers::agent_fs_audit_cmd(&a),
            args::AgentCmd::ArtifactCheck(a) => handlers::agent_artifact_check_cmd(&a),
            args::AgentCmd::Doctor(d) => handlers::agent_doctor_cmd(&d),
            args::AgentCmd::Annotate(a) => handlers::agent_annotate_cmd(&a),
        },
        Command::Catalog(c) => catalog::run_catalog(c),
        Command::Explain(c) => explain::run_explain(&c),
        Command::Ui(u) => handlers::ui_cmd(u).await,
        #[cfg(feature = "docker")]
        Command::Cleanup => handlers::cleanup_cmd().await,
        #[cfg(not(feature = "docker"))]
        Command::Cleanup => handlers::cleanup_cmd(),
    }
}
