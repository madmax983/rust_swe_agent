//! Command-line interface. `clap` derive; subcommand dispatch.
// The dispatch functions in this module call large async subsystems.  The
// Box::pin calls on the hot paths heap-allocate the inner futures, but the
// outer dispatch state machines can still cross the 16 KiB threshold on some
// compiler builds.  The lint is informational here; the allocation behaviour
// is already correct.
#![allow(clippy::large_futures)]

use clap::{Parser, Subcommand};

use crate::error::Error;
use crate::exit_code::ExitCode;

pub mod args;
pub mod catalog;
pub mod cmd;

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
    let log = crate::cli::cmd::util::effective_log_level(cli.log.as_deref());
    crate::cli::cmd::util::init_logging(&log);

    match cli.command {
        Command::Mini(m) => crate::cli::cmd::mini::mini_cmd(*m).await,
        Command::HelloWorld(h) => {
            crate::run::hello_world::main(h.output, h.config.as_deref()).await
        }
        Command::Replay(r) => crate::cli::cmd::mini::replay_cmd(*r).await,
        Command::Bench { cmd } => match *cmd {
            args::BenchCmd::Swebench(s) => {
                Box::pin(crate::cli::cmd::bench::bench_swebench(*s)).await
            }
            args::BenchCmd::Rehearsal(mut s) => {
                s.rehearse = true;
                Box::pin(crate::cli::cmd::bench::bench_swebench(*s)).await
            }
            args::BenchCmd::Forecast(s) => {
                Box::pin(crate::cli::cmd::bench::bench_forecast(*s)).await
            }
            args::BenchCmd::Calibrate(c) => crate::cli::cmd::bench::bench_calibrate(c),
            args::BenchCmd::Doctor(s) => Box::pin(crate::cli::cmd::bench::bench_doctor(*s)).await,
            args::BenchCmd::Compare(c) => crate::cli::cmd::bench::bench_compare(c),
            args::BenchCmd::DiffConfig(c) => crate::cli::cmd::bench::bench_diff_config(c),
            args::BenchCmd::Evaluate(e) => crate::cli::cmd::bench::bench_evaluate(e),
            args::BenchCmd::Inspect(i) => crate::cli::cmd::bench::bench_inspect(i),
            args::BenchCmd::Tail(t) => crate::cli::cmd::bench::bench_tail(t).await,
            args::BenchCmd::Watch(w) => crate::cli::cmd::bench::bench_watch(w).await,
            args::BenchCmd::Triage(t) => crate::cli::cmd::bench::bench_triage(t),
            args::BenchCmd::TriageDiff(t) => crate::cli::cmd::bench::bench_triage_diff(t),
            args::BenchCmd::CommandStats(c) => crate::cli::cmd::bench::bench_command_stats(c),
            args::BenchCmd::Grep(g) => crate::cli::cmd::bench::bench_grep(g),
            args::BenchCmd::Frontier(f) => crate::cli::cmd::bench::bench_frontier(f),
            args::BenchCmd::Reproduce(r) => {
                Box::pin(crate::cli::cmd::bench::bench_reproduce(r)).await
            }
            args::BenchCmd::Bundle(b) => crate::cli::cmd::bench::bench_bundle(b),
            args::BenchCmd::Matrix(m) => Box::pin(crate::cli::cmd::bench::bench_matrix(m)).await,
            args::BenchCmd::EvaluatorSelftest(s) => {
                crate::cli::cmd::bench::bench_evaluator_selftest(s)
            }
            args::BenchCmd::Report(r) => crate::cli::cmd::bench::bench_report(r),
            args::BenchCmd::Retry(r) => Box::pin(crate::cli::cmd::bench::bench_retry(r)).await,
            args::BenchCmd::Behavior(b) => crate::cli::cmd::bench::bench_behavior(b),
            args::BenchCmd::ToolCoverage(t) => crate::cli::cmd::bench::bench_tool_coverage(t),
            args::BenchCmd::SkillCoverage(t) => crate::cli::cmd::bench::bench_skill_coverage(t),
            args::BenchCmd::PolicyImpact(p) => crate::cli::cmd::bench::bench_policy_impact(p),
            args::BenchCmd::InstanceHistory(h) => crate::cli::cmd::bench::bench_instance_history(h),
            args::BenchCmd::CacheStats(c) => crate::cli::cmd::bench::bench_cache_stats(c),
            args::BenchCmd::BudgetFit(b) => crate::cli::cmd::bench::bench_budget_fit(b),
            args::BenchCmd::ToolAblation(t) => {
                Box::pin(crate::cli::cmd::bench::bench_tool_ablation(t)).await
            }
            args::BenchCmd::Ladder(l) => crate::cli::cmd::bench::bench_ladder(l),
            args::BenchCmd::Cascade(c) => Box::pin(crate::cli::cmd::bench::bench_cascade(c)).await,
            args::BenchCmd::TestProgress(t) => crate::cli::cmd::bench::bench_test_progress(t),
            args::BenchCmd::Fork(f) => Box::pin(crate::run::fork::run(f)).await,
            args::BenchCmd::Power(p) => crate::cli::cmd::bench::bench_power(&p),
            args::BenchCmd::DatasetStats(s) => crate::cli::cmd::bench::bench_dataset_stats(s),
            args::BenchCmd::DatasetVerify(s) => crate::cli::cmd::bench::bench_dataset_verify(s),
            args::BenchCmd::Bisect(b) => Box::pin(crate::cli::cmd::bench::bench_bisect(b)).await,
            args::BenchCmd::Audit(a) => crate::cli::cmd::bench::bench_audit(a),
            args::BenchCmd::FailureDigest(f) => crate::cli::cmd::bench::bench_failure_digest(f),
            args::BenchCmd::EvalFlake(f) => crate::cli::cmd::bench::bench_eval_flake(f),
            args::BenchCmd::Annotate(a) => crate::cli::cmd::bench::bench_annotate(a),
            args::BenchCmd::StagnationReport(s) => {
                crate::cli::cmd::bench::bench_stagnation_report(s)
            }
            args::BenchCmd::SelfCheck(s) => crate::cli::cmd::bench::bench_self_check(s),
            args::BenchCmd::Import(i) => crate::cli::cmd::bench::bench_import(i),
            args::BenchCmd::ExportCi(c) => crate::cli::cmd::bench::bench_export_ci(c),
            args::BenchCmd::ContaminationCheck(c) => {
                crate::cli::cmd::bench::bench_contamination_check(c)
            }
            args::BenchCmd::ScriptabilityCheck(s) => {
                Box::pin(crate::cli::cmd::bench::bench_scriptability_check(s)).await
            }
            args::BenchCmd::NearMiss(n) => crate::cli::cmd::bench::bench_near_miss(n),
            args::BenchCmd::Assert(a) => crate::cli::cmd::bench::bench_assert(a),
            args::BenchCmd::Subset(s) => crate::cli::cmd::bench::bench_subset(s),
            args::BenchCmd::EvalParity(p) => crate::cli::cmd::bench::bench_eval_parity(p),
        },
        Command::Agent { cmd } => match *cmd {
            args::AgentCmd::SkillsPreview(s) => {
                crate::cli::cmd::agent::agent_skills_preview_cmd(&s)
            }
            args::AgentCmd::RedactCheck(r) => crate::cli::cmd::agent::agent_redact_check_cmd(&r),
            args::AgentCmd::RedactAudit(a) => crate::cli::cmd::agent::agent_redact_audit_cmd(&a),
            args::AgentCmd::InjectionAudit(a) => {
                crate::cli::cmd::agent::agent_injection_audit_cmd(&a)
            }
            args::AgentCmd::Env {
                cmd: args::AgentEnvCmd::Preview(ref p),
            } => crate::cli::cmd::agent::agent_env_preview_cmd(p),
            args::AgentCmd::Config {
                cmd: args::AgentConfigCmd::Resolve(ref r),
            } => crate::cli::cmd::agent::agent_config_resolve_cmd(r),
            args::AgentCmd::Stability(s) => {
                Box::pin(crate::cli::cmd::agent::agent_stability_cmd(*s)).await
            }
            args::AgentCmd::Suite(s) => Box::pin(crate::cli::cmd::agent::agent_suite_cmd(*s)).await,
            args::AgentCmd::PolicyCheck(p) => crate::cli::cmd::agent::agent_policy_check_cmd(&p),
            args::AgentCmd::Apply(a) => crate::cli::cmd::agent::agent_apply_cmd(&a),
            args::AgentCmd::BestOf(b) => {
                Box::pin(crate::cli::cmd::agent::agent_best_of_cmd(*b)).await
            }
            args::AgentCmd::Profile(p) => crate::cli::cmd::agent::agent_profile_cmd(&p),
        },
        Command::Catalog(c) => catalog::run_catalog(c),
        Command::Ui(u) => crate::cli::cmd::ui::ui_cmd(u).await,
        #[cfg(feature = "docker")]
        Command::Cleanup => crate::cli::cmd::ui::cleanup_cmd().await,
        #[cfg(not(feature = "docker"))]
        Command::Cleanup => crate::cli::cmd::ui::cleanup_cmd(),
    }
}

// ── agent suite ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{Cli, args};
    use crate::cli::cmd::bench::*;
    #[allow(clippy::wildcard_imports)]
    use crate::cli::cmd::util::*;
    use crate::error::Error;
    use crate::run::github_pr::PublishMode;
    use crate::run::swebench::{CANCEL_EXIT_CODE_ESCALATED, SweepResults};
    use crate::trajectory::{Trajectory, outcome};
    use clap::Parser as _;
    use std::path::{Path, PathBuf};

    // ── doctor_probe_webhook tests (feature = "webhook") ──────────────────

    #[cfg(feature = "webhook")]
    mod webhook_doctor {
        use crate::cli::cmd::util::*;

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
        let spec_no_override =
            build_patch_capture_spec(github_pr.as_ref(), None, &cfg, m.skip_patch_validation)
                .unwrap();
        assert_eq!(
            spec_no_override.workdir,
            PathBuf::from(&cfg.root.environment.workdir)
        );

        // 2. With workdir override, should use the override
        let override_dir = PathBuf::from("my_override_dir_xyz_789");
        let spec_override = build_patch_capture_spec(
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
        let res = bench_dataset_stats(cmd);
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

        let res = crate::cli::cmd::mini::mini_cmd(cmd).await;
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

        let res = bench_dataset_verify(cmd);
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

        let res = bench_dataset_verify(cmd);
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
        let res = bench_subset(cmd);
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
        let res = bench_subset(cmd);
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
        let res = bench_subset(cmd);
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
        let res = bench_subset(cmd);
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
        let res = bench_subset(cmd);
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
        let res = bench_subset(cmd);
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
