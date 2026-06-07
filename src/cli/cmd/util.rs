// use std::io::Write as _;
use crate::cli::args;
use crate::config::Config;
use crate::error::Error;
use crate::exit_code::ExitCode;

pub fn print_env_preview_text(preview: &crate::run::env_preview::EnvPreview) {
    print!("{}", crate::run::env_preview::format_preview_text(preview));
}

pub fn effective_log_level(cli_log: Option<&str>) -> String {
    cli_log
        .map(str::to_owned)
        .or_else(|| std::env::var("MAXWELL_LOG").ok())
        .or_else(|| std::env::var("RUST_SWE_AGENT_LOG").ok())
        .unwrap_or_else(|| "info".into())
}

pub fn init_logging(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

pub fn resolve_and_validate_workdir(
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
pub fn redact_json_strings(v: &mut serde_json::Value, redactor: &crate::redaction::Redactor) {
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
pub fn load_resume_traj(path: &std::path::Path) -> Result<crate::trajectory::Trajectory, Error> {
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
pub fn validate_resume_or_exit(traj: &crate::trajectory::Trajectory, path: &std::path::Path) {
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
pub fn reject_cap_bump_without_flag(m: &args::MiniCmd) {
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
pub fn validate_continue_or_exit(traj: &crate::trajectory::Trajectory, path: &std::path::Path) {
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
pub fn reject_cap_bump_without_flag_continue(m: &args::MiniCmd) {
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

pub fn apply_read_only_policy(m: &args::MiniCmd, cfg: &Config) -> Result<(), Error> {
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

/// Print a stable `outcome_class` label followed by the error detail, then exit.
///
/// Used for outcomes that are driven by explicit CLI logic (regression gate,
/// budget-halt forecast, tail abort) rather than propagated `Error` variants.
pub fn exit_with_outcome(code: ExitCode, detail: &str) -> ! {
    eprintln!("outcome_class: {}", code.outcome_class());
    eprintln!("error: {detail}");
    // Flush stdout so piped consumers receive any buffered report output
    // before the process terminates (process::exit bypasses Drop).
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(code.as_i32());
}

pub fn cancellation_exit_code(results: &crate::run::swebench::SweepResults) -> Option<i32> {
    (results.sweep_status == crate::run::swebench::SWEEP_STATUS_CANCELLED).then_some(
        results
            .cancel_exit_code
            .unwrap_or(crate::run::swebench::CANCEL_EXIT_CODE_GRACEFUL),
    )
}

pub fn exit_if_cancelled_sweep(results: &crate::run::swebench::SweepResults) {
    if let Some(code) = cancellation_exit_code(results) {
        let outcome = if code == crate::run::swebench::CANCEL_EXIT_CODE_GRACEFUL {
            ExitCode::Interrupted
        } else {
            ExitCode::Killed
        };
        exit_with_outcome(outcome, "sweep was cancelled");
    }
}

pub fn exit_if_systemic_halt_sweep(results: &crate::run::swebench::SweepResults) {
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

pub async fn run_forecast_from_cmd(
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

pub fn print_dry_run_summary(results: &crate::run::swebench::SweepResults, output_format: &str) {
    if output_format != "json" {
        print!("{}", results.summary_table());
    }
}

pub fn print_forecast_report(
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

pub fn swebench_config_from_cmd(s: &args::SwebenchCmd) -> Result<Config, Error> {
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

pub fn apply_mcp_server_overrides(cfg: &mut Config, commands: &[String]) -> Result<(), Error> {
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

pub fn validate_observation_head_ratio(value: f64) -> Result<(), Error> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "--observation-head-ratio must be a finite value in [0,1], got {value}"
        ))))
    }
}

#[allow(clippy::single_option_map)]
pub fn build_patch_capture_spec(
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

pub fn mini_github_pr_options(
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

pub fn swebench_github_pr_config(
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

pub fn validate_swebench_github_pr_args(github: &args::SwebenchGithubPrArgs) -> Result<(), Error> {
    if !github.open_prs && !github.github_pr_dry_run {
        return Ok(());
    }
    let _ = required_github_arg(github.target_repo.as_deref(), "--target-repo")?;
    let _ = required_github_arg(github.target_branch.as_deref(), "--target-branch")?;
    crate::run::github_pr::validate_branch_prefix(&github.github_pr_branch_prefix)
        .map_err(Error::Config)?;
    Ok(())
}

pub fn required_github_arg(value: Option<&str>, name: &str) -> Result<String, Error> {
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

pub fn trajectory_submitted(path: &std::path::Path) -> Result<bool, Error> {
    let text = std::fs::read_to_string(path)?;
    let trajectory: crate::trajectory::Trajectory = serde_json::from_str(&text)?;
    Ok(trajectory.info.outcome.as_deref() == Some(crate::trajectory::outcome::SUBMITTED))
}

pub async fn publish_github_pr(
    options: crate::run::github_pr::GithubPrOptions,
) -> Result<(), Error> {
    let result = crate::run::github_pr::publish(options).await?;
    if let Some(output) = result.dry_run_output {
        print!("{output}");
    } else if let Some(url) = result.url {
        println!("github_pr_url: {url}");
    }
    Ok(())
}

pub async fn maybe_publish_mini_github_pr(
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

pub fn github_pr_failure_count(results: &crate::run::swebench::SweepResults) -> usize {
    results.github_pr_failures
}

pub fn parse_dataset_source(
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

pub fn swebench_args_from_cmd(
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
pub fn resolve_interactive_mode(
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

pub fn parse_verify_checks(
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
pub fn print_contamination_adjusted_rate(
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

pub fn apply_significance_gates(
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

pub fn parse_compare_format(raw: &str) -> Result<crate::run::compare::CompareFormat, Error> {
    match raw {
        "text" => Ok(crate::run::compare::CompareFormat::Text),
        "json" => Ok(crate::run::compare::CompareFormat::Json),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "unknown --format `{other}` (expected `text` or `json`)"
        )))),
    }
}

pub fn parse_trajectory_diff_format(
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

pub fn print_trajectory_diff(
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

/// Compare annotations between original and replay sweep directories.
/// Best-effort — prints a warning when annotations differ; silent on errors.
pub fn render_reproduce_annotation_diff(
    from: &std::path::Path,
    output: &std::path::Path,
    replayed_ids: &std::collections::HashSet<&str, impl std::hash::BuildHasher>,
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
pub fn build_current_manifest_for_reproduce(
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

pub fn current_git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

pub fn current_rust_version() -> Option<String> {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

pub fn chrono_now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn load_sweep_results(
    sweep_dir: &std::path::Path,
) -> Result<crate::run::swebench::SweepResults, Error> {
    let path = sweep_dir.join("results.json");
    let file = std::fs::File::open(&path).map_err(Error::Io)?;
    serde_json::from_reader(std::io::BufReader::new(file)).map_err(Error::Json)
}

pub fn hash_manifest(manifest: &crate::run::swebench::ProvenanceManifest) -> String {
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
pub fn reproduce_swebench_args(
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

pub fn bundle_reproduce_dataset_path(
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

pub fn parse_breakdown_selection(
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

#[cfg(feature = "html-export")]
#[allow(clippy::unnecessary_wraps)]
pub fn inspect_export_html(traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    use crate::trajectory::export::{HtmlExporter, TrajectoryExporter};
    Ok(HtmlExporter::export(traj))
}

#[cfg(not(feature = "html-export"))]
pub fn inspect_export_html(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "format_unavailable: --format html requires the `html-export` Cargo feature; \
         rebuild with `--features html-export`"
            .into(),
    )))
}

#[cfg(feature = "csv-export")]
#[allow(clippy::unnecessary_wraps)]
pub fn inspect_export_csv(traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    use crate::trajectory::export::{CsvExporter, TrajectoryExporter};
    Ok(CsvExporter::export(traj))
}

#[cfg(not(feature = "csv-export"))]
pub fn inspect_export_csv(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "format_unavailable: --format csv requires the `csv-export` Cargo feature; \
         rebuild with `--features csv-export`"
            .into(),
    )))
}

#[cfg(feature = "mermaid-export")]
#[allow(clippy::unnecessary_wraps)]
pub fn inspect_export_mermaid(traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    use crate::trajectory::export::{MermaidExporter, TrajectoryExporter};
    Ok(MermaidExporter::export(traj))
}

#[cfg(not(feature = "mermaid-export"))]
pub fn inspect_export_mermaid(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "format_unavailable: --format mermaid requires the `mermaid-export` Cargo feature; \
         rebuild with `--features mermaid-export`"
            .into(),
    )))
}

#[allow(clippy::too_many_lines)]
pub fn retry_swebench_args(
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

pub fn bundle_error_to_error(err: crate::run::bundle::BundleError) -> Error {
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

pub fn parse_env_kind(kind: &str) -> Result<crate::config::EnvKind, Error> {
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
pub async fn doctor_probe_webhook(url: &str, headers: &[(String, String)]) -> String {
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

pub fn print_doctor_skills_preview(cfg: &crate::config::Config) {
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

pub fn parse_dataset_source_stats(
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
