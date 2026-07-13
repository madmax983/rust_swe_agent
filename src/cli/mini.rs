#![allow(unused_imports)]
use super::args;
use super::{
    apply_mcp_server_overrides, parse_env_kind, parse_network_mode, reject_json_with_ratatui,
    required_github_arg, resolve_interactive_mode, validate_observation_head_ratio,
};
use super::{
    apply_read_only_policy, build_patch_capture_spec, emit_mini_result, exit_with_outcome,
    load_resume_traj, maybe_publish_mini_github_pr, parse_verify_checks, publish_github_pr,
    reject_cap_bump_without_flag, reject_cap_bump_without_flag_continue,
    resolve_and_validate_workdir, trajectory_submitted, validate_continue_or_exit,
    validate_resume_or_exit,
};
use crate::config::Config;
use crate::error::Error;
use crate::exit_code::ExitCode;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::time::Duration;

#[allow(clippy::too_many_lines)]
pub async fn mini_cmd(m: args::MiniCmd) -> Result<(), Error> {
    let mut issue_provenance = None;
    let sources_count = [
        m.task.is_some(),
        m.task_file.is_some(),
        m.resume_from.is_some(),
        m.from_issue.is_some(),
        m.from_issue_file.is_some(),
    ]
    .iter()
    .filter(|&&b| b)
    .count();

    if sources_count > 1 {
        let mut provided = Vec::new();
        if m.task.is_some() {
            provided.push("--task");
        }
        if m.task_file.is_some() {
            provided.push("--task-file");
        }
        if m.resume_from.is_some() {
            provided.push("--resume");
        }
        if m.from_issue.is_some() {
            provided.push("--from-issue");
        }
        if m.from_issue_file.is_some() {
            provided.push("--from-issue-file");
        }
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "multiple task sources provided ({}); only one may be used",
            provided.join(", ")
        ))));
    }

    let task = if m.resume_from.is_some() {
        String::new()
    } else if m.continue_from.is_some() {
        if m.from_issue.is_some() || m.from_issue_file.is_some() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "cannot use --from-issue or --from-issue-file with --continue".into(),
            )));
        }
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
    } else if m.from_issue.is_some() || m.from_issue_file.is_some() {
        let (t, prov) = crate::run::github_issue::resolve_issue_task_async(
            m.from_issue.clone(),
            m.from_issue_file.clone(),
            &m.github_pr.github_token_env,
        )
        .await?;
        issue_provenance = Some(prov);
        t
    } else {
        match (&m.task, &m.task_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "both --task and --task-file were provided".into(),
                )));
            }
            (None, None) => {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "either --task, --task-file, --from-issue, or --from-issue-file must be provided"
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
    if let Some(nm) = m.network_mode {
        cfg.root.environment.network_mode = parse_network_mode(nm.as_str())?;
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

    // Capture state needed for --result-format json before MiniArgs consumes fields.
    let result_format = m.result_format;
    reject_json_with_ratatui(result_format, interactive_mode)?;
    let redactor = crate::redaction::Redactor::from_config_lossy(&cfg.root.redaction);
    let traj_path = m.output.join(format!("{trajectory_name}.traj.json"));
    // patch_path is only set when github-pr flags are active (same gating as patch_capture).
    let patch_path = github_pr.as_ref().map(|o| o.patch_path.clone());
    // The scripted-model hook only affects the builtin loop; external drivers
    // shell out to a real agent and ignore it, which would silently make a paid
    // call despite the documented "bypasses the model API" guarantee.
    if !m.deterministic_responses.is_empty() && m.driver != crate::run::mini::RunDriver::Builtin {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--deterministic-responses is only honored by the builtin driver; \
             external drivers (--driver codex|claude-code) shell out to a real agent \
             and ignore scripted responses"
                .into(),
        )));
    }
    let scripted = if m.deterministic_responses.is_empty() {
        None
    } else {
        Some(m.deterministic_responses.clone())
    };

    let args = crate::run::mini::MiniArgs {
        task,
        extra_context: m.extra_context,
        config: cfg,
        driver: m.driver,
        driver_append_system_prompt: m.driver_append_system_prompt,
        driver_isolated: m.driver_isolated,
        output_dir: m.output,
        trajectory_name,
        deterministic_responses: scripted,
        deterministic_usage_per_call: None,
        task_timeout_secs: m.task_timeout_secs,
        cancellation: None,
        stream_addr,
        patch_capture,
        verification_checks,
        verification_timeout_secs: m.verify_timeout_secs,
        resume_from: None,
        interactive_mode,
        no_bell: m.no_bell,
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
        issue_provenance,
    };
    let run_result = crate::run::mini::run(args).await;
    // Attempt the GitHub PR publish only when the run succeeded or failed at
    // verification — those are the two cases where the trajectory and patch are
    // guaranteed on disk. Capture the result instead of early-returning so the
    // JSON result object can both always emit AND fold the publish failure into
    // its effective exit code.
    let is_verification_failure =
        matches!(run_result, Err(crate::error::Error::VerificationFailed(..)));
    let publish_result = if run_result.is_ok() || is_verification_failure {
        maybe_publish_mini_github_pr(
            github_pr,
            result_format == crate::run::mini::ResultFormat::Json,
        )
        .await
    } else {
        Ok(())
    };
    emit_mini_result(
        result_format,
        &run_result,
        &publish_result,
        &traj_path,
        patch_path.as_deref(),
        &redactor,
    )?;
    // Propagate the run error first (it determines the JSON exit code), then any
    // publish error. This ordering matches emit_mini_result's exit-code precedence.
    run_result?;
    publish_result?;
    Ok(())
}

pub fn mini_render_only_cmd(
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

/// Handle `mini --resume <path>`.
///
/// Validates the on-disk trajectory, extracts configuration from it (AC #2),
/// and invokes `mini::run()` with `resume_from` populated so the agent
/// continues from the last persisted step without replaying the prefix (AC #3).
#[allow(clippy::too_many_lines)]
pub async fn mini_resume_cmd(
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

    // Capture --result-format state before MiniArgs consumes cfg/output fields.
    let result_format = m.result_format;
    reject_json_with_ratatui(result_format, interactive_mode)?;
    let redactor = crate::redaction::Redactor::from_config_lossy(&cfg.root.redaction);
    let result_traj_path = traj_output_dir.join(format!("{traj_stem}.traj.json"));
    let scripted = if m.deterministic_responses.is_empty() {
        None
    } else {
        Some(m.deterministic_responses.clone())
    };

    let args = crate::run::mini::MiniArgs {
        task,
        extra_context: m.extra_context,
        config: cfg,
        driver: crate::run::mini::RunDriver::Builtin,
        driver_append_system_prompt: false,
        driver_isolated: false,
        output_dir: traj_output_dir,
        trajectory_name: traj_stem,
        deterministic_responses: scripted,
        deterministic_usage_per_call: None,
        task_timeout_secs: m.task_timeout_secs,
        cancellation: None,
        stream_addr,
        patch_capture: None,
        verification_checks,
        verification_timeout_secs: m.verify_timeout_secs,
        resume_from: Some(traj),
        interactive_mode,
        no_bell: m.no_bell,
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
        issue_provenance,
    };
    // Resume never captures a patch (patch_capture: None) and never publishes a PR.
    let run_result = crate::run::mini::run(args).await;
    let no_publish: Result<(), Error> = Ok(());
    emit_mini_result(
        result_format,
        &run_result,
        &no_publish,
        &result_traj_path,
        None,
        &redactor,
    )?;
    run_result
}

/// Handle `mini --continue <path> --task "<follow-up>"`.
///
/// Loads the terminal parent trajectory, appends the follow-up instruction as
/// a new user turn, and runs the agent starting from that point. The result is
/// written to a new trajectory file that records the parent lineage.
#[allow(clippy::too_many_lines)]
pub async fn mini_continue_cmd(
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

    // Capture --result-format state before MiniArgs consumes cfg/output fields.
    let result_format = m.result_format;
    reject_json_with_ratatui(result_format, interactive_mode)?;
    let redactor = crate::redaction::Redactor::from_config_lossy(&cfg.root.redaction);
    let result_traj_path = output_dir.join(format!("{child_traj_name}.traj.json"));
    let scripted = if m.deterministic_responses.is_empty() {
        None
    } else {
        Some(m.deterministic_responses.clone())
    };

    let args = crate::run::mini::MiniArgs {
        task: follow_up_task,
        extra_context: m.extra_context,
        config: cfg,
        driver: crate::run::mini::RunDriver::Builtin,
        driver_append_system_prompt: false,
        driver_isolated: false,
        output_dir,
        trajectory_name: child_traj_name,
        deterministic_responses: scripted,
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
        no_bell: m.no_bell,
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
        issue_provenance: None,
    };
    // Continue never captures a patch (patch_capture: None) and never publishes a PR.
    let run_result = crate::run::mini::run(args).await;
    let no_publish: Result<(), Error> = Ok(());
    emit_mini_result(
        result_format,
        &run_result,
        &no_publish,
        &result_traj_path,
        None,
        &redactor,
    )?;
    run_result
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
