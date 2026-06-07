use crate::cli::args;
#[allow(clippy::wildcard_imports)]
use crate::cli::cmd::util::*;
use crate::error::Error;
use crate::exit_code::ExitCode;
use std::io::{IsTerminal as _, Write as _};
use std::time::Duration;

pub fn bench_swebench_render_only(s: &args::SwebenchCmd) -> Result<(), Error> {
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

pub fn bench_calibrate(c: args::CalibrateCmd) -> Result<(), Error> {
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

#[allow(clippy::too_many_lines)]
pub fn bench_compare(c: args::CompareCmd) -> Result<(), Error> {
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

#[allow(clippy::needless_pass_by_value)]
pub fn bench_diff_config(c: args::DiffConfigCmd) -> Result<(), Error> {
    let args = crate::run::diff_config::DiffConfigArgs {
        baseline: c.baseline,
        candidate: c.candidate,
        format: c.format,
        fail_on_change: c.fail_on_change,
        ignore: c.ignore,
    };
    crate::run::diff_config::run(&args)
}

#[allow(clippy::too_many_lines)]
pub fn bench_evaluate(e: args::EvaluateCmd) -> Result<(), Error> {
    let backend = match e.backend.as_str() {
        "sb-cli" => crate::run::evaluate::EvaluateBackend::SbCli,
        "none" => crate::run::evaluate::EvaluateBackend::None,
        "rehearsal" => crate::run::evaluate::EvaluateBackend::Rehearsal,
        "docker-tests" => crate::run::evaluate::EvaluateBackend::DockerTests,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "unknown --backend `{other}` (expected `sb-cli`, `none`, `rehearsal`, or `docker-tests`)"
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

pub async fn bench_reproduce(r: args::ReproduceCmd) -> Result<(), Error> {
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

pub fn bench_frontier(f: args::FrontierCmd) -> Result<(), Error> {
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

pub fn bench_inspect(i: args::InspectCmd) -> Result<(), Error> {
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

pub fn bench_inspect_export(i: args::InspectCmd) -> Result<(), Error> {
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

pub fn bench_command_stats(c: args::CommandStatsCmd) -> Result<(), Error> {
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

pub fn bench_test_progress(t: args::TestProgressCmd) -> Result<(), Error> {
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

pub fn bench_power(p: &args::PowerCmd) -> Result<(), Error> {
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

pub fn bench_behavior(b: args::BehaviorCmd) -> Result<(), Error> {
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

pub fn bench_instance_history(h: args::InstanceHistoryCmd) -> Result<(), Error> {
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

pub fn bench_cache_stats(c: args::CacheStatsCmd) -> Result<(), Error> {
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

pub fn bench_budget_fit(b: args::BudgetFitCmd) -> Result<(), Error> {
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
pub async fn bench_tool_ablation(t: args::ToolAblationCmd) -> Result<(), Error> {
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

pub fn bench_ladder(l: args::LadderCmd) -> Result<(), Error> {
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

pub fn bench_stagnation_report(s: args::StagnationReportCmd) -> Result<(), Error> {
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

pub fn bench_self_check(s: args::SelfCheckCmd) -> Result<(), Error> {
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

pub fn bench_export_ci(c: args::ExportCiCmd) -> Result<(), Error> {
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

pub fn bench_contamination_check(c: args::ContaminationCheckCmd) -> Result<(), Error> {
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

pub async fn bench_scriptability_check(cmd: args::ScriptabilityCheckCmd) -> Result<(), Error> {
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

pub fn bench_near_miss(n: args::NearMissCmd) -> Result<(), Error> {
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
pub fn bench_assert(a: args::AssertCmd) -> Result<(), Error> {
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

pub fn bench_import(i: args::ImportCmd) -> Result<(), Error> {
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

pub async fn bench_cascade(c: args::CascadeCmd) -> Result<(), Error> {
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

pub fn bench_tool_coverage(t: args::ToolCoverageCmd) -> Result<(), Error> {
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

pub fn bench_skill_coverage(t: args::SkillCoverageCmd) -> Result<(), Error> {
    let is_json = match t.format.as_str() {
        "text" => false,
        "json" => true,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "skill-coverage: unknown --format `{other}` (expected `text` or `json`)"
            ))));
        }
    };
    let bucket = t.bucket.clone();
    let report = crate::run::skill_coverage::run(&crate::run::skill_coverage::SkillCoverageArgs {
        sweep_dir: t.sweep,
        bucket: t.bucket,
        filter: t.filter,
        per_instance: t.per_instance,
    })?;
    if is_json {
        let json_str = crate::artifact::to_string_pretty(
            crate::artifact::ArtifactKind::SkillCoverage,
            &report,
        )?;
        println!("{json_str}");
    } else {
        print!(
            "{}",
            crate::run::skill_coverage::render_text(&report, bucket.as_deref())
        );
    }
    Ok(())
}

pub fn bench_policy_impact(t: args::PolicyImpactCmd) -> Result<(), Error> {
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

pub fn bench_grep(g: args::GrepCmd) -> Result<(), Error> {
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

pub fn bench_triage(t: args::TriageCmd) -> Result<(), Error> {
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

pub fn bench_triage_diff(t: args::TriageDiffCmd) -> Result<(), Error> {
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

pub fn bench_bundle(b: args::BundleCmd) -> Result<(), Error> {
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
pub async fn bench_matrix(m: args::MatrixCmd) -> Result<(), Error> {
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
pub fn bench_evaluator_selftest(s: args::EvaluatorSelftestCmd) -> Result<(), Error> {
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

pub fn bench_report(r: args::ReportCmd) -> Result<(), Error> {
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
pub async fn bench_retry(r: args::RetryCmd) -> Result<(), Error> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatsFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepOutputFormat {
    Text,
    Json,
}

pub async fn bench_tail(t: args::TailCmd) -> Result<(), Error> {
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

pub async fn bench_watch(w: args::WatchCmd) -> Result<(), Error> {
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

#[allow(clippy::needless_pass_by_value)]
pub fn bench_dataset_stats(s: args::DatasetStatsCmd) -> Result<(), Error> {
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

#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub fn bench_subset(s: args::SubsetCmd) -> Result<(), Error> {
    use crate::run::dataset::DatasetSource;
    use crate::run::subset::{SubsetArgs, manifest_path_for, run_subset};

    // Resolve dataset source (same mutual-exclusion logic as dataset-stats)
    let cache_dir = s
        .dataset_cache_dir
        .clone()
        .unwrap_or_else(crate::run::dataset::default_cache_dir);

    let dataset_source = match (&s.dataset_path, &s.dataset) {
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
        (Some(path), None) => DatasetSource::LocalPath(path.clone()),
        (None, Some(alias_str)) => {
            let alias = alias_str
                .parse::<crate::run::dataset::SwebenchAlias>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            let split_str = s.split.as_deref().unwrap_or("test");
            let split = split_str
                .parse::<crate::run::dataset::SwebenchSplit>()
                .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
            DatasetSource::Named { alias, split }
        }
    };

    let (dataset_bytes, meta) = crate::run::dataset::resolve_dataset(&dataset_source, &cache_dir)?;
    let all_instances = crate::run::swebench::load_dataset_from_bytes_pub(&dataset_bytes)?;

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

    // Validate --sample is not larger than the available post-filter count.
    // apply_subset silently keeps all instances when sample >= len; bench subset
    // treats this as a configuration error to surface unintentional over-sampling.
    if let Some(n) = s.sample {
        // Determine the pool size *after* any --instance-ids filter but before
        // sampling, so the error message reflects the true available count.
        let available = if let Some(ids_arg) = s.instance_ids.as_deref() {
            let no_sample_params = crate::run::swebench::ApplySubsetParams {
                instance_ids_arg: Some(ids_arg),
                limit: None,
                sample: None,
                seed: None,
                stratify_by: None,
                stratify_mode: crate::run::swebench::StratifyMode::default(),
            };
            match crate::run::swebench::apply_subset(all_instances.clone(), &no_sample_params) {
                Ok((filtered, _)) => filtered.len(),
                Err(_) => all_instances.len(),
            }
        } else {
            all_instances.len()
        };
        if n > available {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--sample {n} is larger than the available instance count ({available}); \
                 reduce --sample or omit it to use all instances"
            ))));
        }
    }

    let (instances, filter_spec) = crate::run::swebench::apply_subset(all_instances, &params)?;

    let alias_str = match &dataset_source {
        DatasetSource::Named { alias, .. } => Some(alias.to_string()),
        DatasetSource::LocalPath(_) => None,
    };
    let split_str = match &dataset_source {
        DatasetSource::Named { split, .. } => Some(split.to_string()),
        DatasetSource::LocalPath(_) => None,
    };

    // Refuse to overwrite the source dataset — writing the slice back to the
    // same file would corrupt the source while the manifest still records the
    // pre-write hash.  We canonicalize both paths; if the output does not
    // exist yet it cannot be the same file, so we skip the check.
    if let Ok(source_canon) = meta.path.canonicalize() {
        if let Ok(out_canon) = s.output.canonicalize() {
            if source_canon == out_canon {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "the output path resolves to the same file as the source dataset; \
                     writing would destroy the source — choose a different output path"
                        .into(),
                )));
            }
        }
    }

    let manifest = run_subset(SubsetArgs {
        instances,
        source_sha256: meta.sha256,
        alias: alias_str,
        split: split_str,
        filter_spec,
        output: &s.output,
    })?;

    eprintln!(
        "bench subset: wrote {} instances to {}",
        manifest.instance_count,
        s.output.display()
    );
    eprintln!(
        "bench subset: sidecar manifest → {}",
        manifest_path_for(&s.output).display()
    );

    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
pub fn bench_dataset_verify(s: args::DatasetVerifyCmd) -> Result<(), Error> {
    use crate::run::dataset::DatasetSource;

    if s.format != "text" && s.format != "json" {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "dataset-verify: unknown --format `{}`; valid values: text, json",
            s.format
        ))));
    }

    if s.dataset_path.is_none() && s.dataset.is_none() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "one of --dataset-path or --dataset is required".into(),
        )));
    }

    let cache_dir = s
        .dataset_cache_dir
        .clone()
        .unwrap_or_else(crate::run::dataset::default_cache_dir);

    // Determine target canonical alias/split
    let alias_str = s.dataset.as_ref().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "when using --dataset-path, --dataset must also be specified to identify the target canonical release".into()
        ))
    })?;
    let alias = alias_str
        .parse::<crate::run::dataset::SwebenchAlias>()
        .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;
    let split_str = s.split.as_deref().unwrap_or("test");
    let split = split_str
        .parse::<crate::run::dataset::SwebenchSplit>()
        .map_err(|e| Error::Config(crate::error::ConfigError::Invalid(e)))?;

    // Determine candidate source
    let candidate_source = if let Some(path) = &s.dataset_path {
        DatasetSource::LocalPath(path.clone())
    } else {
        DatasetSource::Named {
            alias: alias.clone(),
            split: split.clone(),
        }
    };

    // Determine canonical reference path
    let canonical_dir = s
        .canonical_dir
        .clone()
        .unwrap_or_else(|| cache_dir.join("canonical"));
    let reference_path = canonical_dir
        .join(alias.as_str())
        .join(format!("{}.jsonl", split.as_str()));

    if !reference_path.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "canonical reference for dataset alias `{alias}` split `{split}` not found: \
            expected file at `{path}`\n\
            \n\
            To populate the reference, download the official SWE-bench JSONL for the \
            `{alias}` dataset (`{split}` split) and place it at:\n\
            \n  {path}",
            path = reference_path.display()
        ))));
    }

    // Load candidate dataset
    let (cand_bytes, _meta) = crate::run::dataset::resolve_dataset(&candidate_source, &cache_dir)?;
    let candidate_instances = crate::run::swebench::load_dataset_from_bytes_pub(&cand_bytes)?;

    // Load reference dataset
    let ref_bytes = std::fs::read(&reference_path)?;
    let reference_instances = crate::run::swebench::load_dataset_from_bytes_pub(&ref_bytes)?;

    // Verify
    let report =
        crate::run::dataset_verify::verify_dataset(&candidate_instances, &reference_instances)?;

    if s.format == "json" {
        let serialized = serde_json::to_string_pretty(&report)?;
        println!("{serialized}");
    } else {
        let text = crate::run::dataset_verify::render_text(&report);
        print!("{text}");
    }

    if report.verdict == "mismatch" {
        exit_with_outcome(ExitCode::DatasetVerifyMismatch, "dataset mismatch detected");
    }

    Ok(())
}

pub async fn bench_bisect(b: args::BisectCmd) -> Result<(), Error> {
    crate::run::bisect::run(&b).await
}

#[allow(clippy::needless_pass_by_value)]
pub fn bench_audit(a: args::AuditCmd) -> Result<(), Error> {
    crate::run::audit::run(&a)
}

pub fn bench_failure_digest(f: args::FailureDigestCmd) -> Result<(), Error> {
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

pub fn bench_eval_flake(f: args::EvalFlakeCmd) -> Result<(), Error> {
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

pub fn bench_eval_parity(p: args::EvalParityCmd) -> Result<(), Error> {
    if let Some(min) = p.min_agreement {
        if min.is_nan() || !(0.0..=1.0).contains(&min) {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--min-agreement must be in [0.0, 1.0], got {min}"
            ))));
        }
    }
    let args = crate::run::eval_parity::EvalParityArgs {
        sweep_dir: p.sweep,
        output: p.output,
        concurrency: p.concurrency,
        min_agreement: p.min_agreement,
        sample: p.sample,
        instances: p.instances,
        recheck: p.recheck,
        dataset_path: p.dataset_path,
        sb_subset: p.sb_subset,
    };
    let report = crate::run::eval_parity::run(&args)?;
    let summary = &report.summary;
    eprint!("{}", crate::run::eval_parity::render_summary(&report));
    eprintln!("eval-parity: total_cost_usd=0.00 (evaluator wallclock only)");
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(Error::Json)?
    );
    if let Some(min) = args.min_agreement {
        if summary.agreement_rate < min {
            exit_with_outcome(
                ExitCode::EvalParityGateFailure,
                &format!(
                    "eval-parity: agreement_rate {:.4} is below --min-agreement {min:.4}",
                    summary.agreement_rate
                ),
            );
        }
    }
    Ok(())
}

pub fn bench_annotate(a: args::AnnotateCmd) -> Result<(), Error> {
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
