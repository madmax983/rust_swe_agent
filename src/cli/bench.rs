#![allow(clippy::large_futures)]
use super::{
    args, bench_swebench_render_only, compare_rehearsals, doctor_probe_webhook,
    exit_if_cancelled_sweep, exit_if_systemic_halt_sweep, exit_with_outcome,
    github_pr_failure_count, print_doctor_skills_preview, print_dry_run_summary,
    print_env_preview_text, print_forecast_report, run_forecast_from_cmd, swebench_args_from_cmd,
    swebench_config_from_cmd, validate_swebench_github_pr_args,
};
use crate::error::Error;
use crate::exit_code::ExitCode;

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

pub async fn bench_doctor(mut s: args::SwebenchCmd) -> Result<(), Error> {
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

pub async fn bench_forecast(s: args::SwebenchCmd) -> Result<(), Error> {
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
