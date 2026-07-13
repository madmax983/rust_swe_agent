#![allow(unused_imports)]
use super::args;
use super::{
    apply_mcp_server_overrides, resolve_interactive_mode, validate_observation_head_ratio,
};
use super::{
    doctor_probe_webhook, exit_with_outcome, parse_env_kind, print_doctor_skills_preview,
    print_doctor_text, print_env_preview_text, redact_json_strings, resolve_and_validate_workdir,
};
use crate::config::Config;
use crate::error::Error;
use crate::exit_code::ExitCode;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::time::Duration;

pub fn agent_env_preview_cmd(p: &args::EnvPreviewCmd) -> Result<(), Error> {
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

pub fn agent_doctor_cmd(d: &args::AgentDoctorCmd) -> Result<(), Error> {
    use crate::run::agent_doctor::{DoctorOpts, run_doctor};

    let cfg = match &d.config {
        Some(path) => Config::load(path)?,
        None => Config::defaults()?,
    };
    let model = d
        .model
        .clone()
        .unwrap_or_else(|| cfg.root.model.name.clone());
    // An explicit `--env` wins; otherwise fall back to the environment kind
    // resolved from config so a docker-backed config is not silently validated
    // as local (which would skip the Docker daemon check).
    let env_kind = match d.env {
        Some(args::EnvTypeArg::Local) => crate::config::schema::EnvKind::Local,
        Some(args::EnvTypeArg::Docker) => crate::config::schema::EnvKind::Docker,
        None => cfg.root.environment.kind,
    };
    let opts = DoctorOpts {
        env_kind,
        model,
        output_dir: d.output.clone(),
        // A `--docker-image` override wins; otherwise fall back to the configured
        // image, so `agent doctor` preflights the same image the live run would
        // use (mirrors `max mini --env docker --docker-image …`).
        docker_image: d.docker_image.clone().or(cfg.root.environment.docker_image),
    };

    let report = run_doctor(&opts);

    if d.format == args::PreviewFormatArg::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| {
                Error::Config(crate::error::ConfigError::Invalid(e.to_string()))
            })?
        );
    } else {
        print_doctor_text(&report);
    }

    if !report.ready {
        exit_with_outcome(
            ExitCode::HostNotReady,
            "host not ready: one or more readiness checks failed",
        );
    }
    Ok(())
}

pub fn agent_config_resolve_cmd(r: &args::ConfigResolveCmd) -> Result<(), Error> {
    use crate::run::config_resolve::{ConfigResolveArgs, format_text, run_config_resolve};

    if let Some(v) = r.observation_head_ratio {
        validate_observation_head_ratio(v)?;
    }
    if let Some(v) = r.per_task_budget_usd {
        if !v.is_finite() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "--per-task-budget-usd must be a finite value".into(),
            )));
        }
    }
    if let Some(ref env_val) = r.env {
        parse_env_kind(env_val.as_str())?;
    }
    let canonical_workdir = if let Some(ref wd) = r.workdir {
        // Build an ephemeral config with the --env flag applied so the docker+workdir
        // check mirrors runtime behaviour.
        let mut check_cfg = match &r.config {
            Some(p) => Config::load(p)?,
            None => Config::defaults()?,
        };
        if let Some(ref env_val) = r.env {
            check_cfg.root.environment.kind = parse_env_kind(env_val.as_str())?;
        }
        resolve_and_validate_workdir(Some(wd), &check_cfg)?
    } else {
        None
    };

    let resolve_args = ConfigResolveArgs {
        config: r.config.clone(),
        model_flag: r.model.clone(),
        step_limit_flag: r.step_limit,
        observation_max_bytes_flag: r.observation_max_bytes,
        observation_head_ratio_flag: r.observation_head_ratio,
        per_task_budget_usd_flag: r.per_task_budget_usd,
        hide_budget_from_agent_flag: r.hide_budget_from_agent,
        env_flag: r.env.clone(),
        workdir_flag: canonical_workdir,
        detect_stagnation_flag: r.detect_stagnation,
        stagnation_repeat_threshold_flag: r.stagnation_repeat_threshold,
        stagnation_window_flag: r.stagnation_window,
    };

    let report = run_config_resolve(&resolve_args).map_err(Error::Config)?;

    match r.format.as_str() {
        "json" => {
            let wrapped = serde_json::json!({ "config_resolve": &report });
            println!(
                "{}",
                serde_json::to_string_pretty(&wrapped).map_err(|e| Error::Config(
                    crate::error::ConfigError::Invalid(e.to_string())
                ))?
            );
        }
        "text" | "" => {
            print!("{}", format_text(&report));
        }
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--format '{other}' is not valid; use 'text' or 'json'"
            ))));
        }
    }

    if report.has_hazards {
        exit_with_outcome(
            ExitCode::ConfigOverrideWarning,
            "config resolve detected clap-default override hazard(s); \
             see output above for affected fields",
        );
    }
    Ok(())
}

pub fn agent_redact_check_cmd(r: &args::RedactCheckCmd) -> Result<(), Error> {
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

pub fn agent_redact_audit_cmd(a: &args::RedactAuditCmd) -> Result<(), Error> {
    use crate::run::redact_audit::{
        AuditFormat, AuditOpts, format_human, format_json, is_audited_file, mask_report_path,
        output_aliases_scanned_artifact, parse_format, run_redact_audit,
    };

    let cfg = match &a.config {
        Some(path) => Config::load(path)?,
        None => Config::defaults()?,
    };

    let format = parse_format(a.json, a.format.as_str())?;

    // Resolve the report path up front and refuse to overwrite a scanned source
    // artifact: `redact-audit` is detector-only and must never mutate the sweep
    // it audits. The default `redact_audit.json` is never itself audited, so it
    // is always allowed; any other path that resolves to an existing audited
    // artifact *inside the scanned directory* (e.g. `<dir>/results.json`, an
    // instance `trajectory.json`) is rejected before the scan runs so the
    // completed sweep cannot be corrupted. A path outside the scanned tree (e.g.
    // `--output /tmp/results.json`) is never a scanned source artifact and is
    // allowed even if its name looks audited. Two checks catch a write that
    // would mutate a scanned artifact: (1) the canonical target resolves inside
    // the scanned tree under an audited name (covers a symlink such as
    // `report -> <dir>/results.json`); (2) the output shares an on-disk inode
    // with a scanned artifact (covers a hard link whose own name is not
    // allowlisted), since `std::fs::write` would truncate the shared inode.
    let out_path = a
        .output
        .clone()
        .unwrap_or_else(|| a.dir.join("redact_audit.json"));
    let resolved_out = std::fs::canonicalize(&out_path).unwrap_or_else(|_| out_path.clone());
    let canonical_dir = std::fs::canonicalize(&a.dir).unwrap_or_else(|_| a.dir.clone());
    let inside_scan = resolved_out.starts_with(&canonical_dir);
    if out_path.exists()
        && ((inside_scan && (is_audited_file(&out_path) || is_audited_file(&resolved_out)))
            || output_aliases_scanned_artifact(&a.dir, &out_path))
    {
        return Err(Error::Config(crate::error::ConfigError::Usage(format!(
            "redact-audit: --output '{}' would overwrite an audited source artifact; \
             choose a different path (the report is detector-only and must not mutate the sweep)",
            // The rejected path may itself embed a secret; mask it like other
            // path-shaped report fields before it reaches stderr.
            mask_report_path(&cfg, &out_path.display().to_string())
        ))));
    }

    let opts = AuditOpts {
        dir: a.dir.clone(),
        detectors: a.detectors.clone(),
        disable_entropy: a.disable_entropy,
        baseline: a.baseline.clone(),
    };

    let report = run_redact_audit(&cfg, &opts)?;

    let json = format_json(&report).map_err(Error::Json)?;
    let exit_code = report.exit_code();

    // Persist the report. If any filesystem step (creating the output parent or
    // writing the file) fails *after* a scan that already found leaks or scan
    // errors, surface the audit's own exit code (32/33) rather than letting the
    // I/O error collapse to a generic internal_error (1) — CI gates route on the
    // documented outcome class, and the result is known.
    let write_result = (|| -> std::io::Result<()> {
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&out_path, &json)
    })();
    if let Err(e) = write_result {
        if exit_code == ExitCode::Success {
            return Err(Error::Io(e));
        }
        eprintln!(
            "warning: redact-audit could not write report to {}: {e}",
            mask_report_path(&cfg, &out_path.display().to_string())
        );
    }

    match format {
        AuditFormat::Json => println!("{json}"),
        AuditFormat::Human => print!("{}", format_human(&report)),
    }

    if exit_code != ExitCode::Success {
        exit_with_outcome(exit_code, exit_code.outcome_class());
    }
    Ok(())
}

pub fn agent_injection_audit_cmd(a: &args::InjectionAuditCmd) -> Result<(), Error> {
    use crate::run::injection_audit::{
        AuditFormat, AuditOpts, format_json, format_jsonl, format_text, parse_fail_on,
        parse_format, run_injection_audit,
    };

    let format = parse_format(a.format.as_str()).map_err(Error::Config)?;
    let fail_on = parse_fail_on(a.fail_on.as_str()).map_err(Error::Config)?;

    // Guard: --output must not overwrite a trajectory artifact.
    if let Some(ref out_path) = a.output {
        if out_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".traj.json"))
        {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "--output path must not be a .traj.json file".to_owned(),
            )));
        }
    }

    let opts = AuditOpts {
        sweep_dir: a.sweep.clone(),
        extra_signatures: a.signatures.clone(),
        format,
        fail_on,
    };

    let report = match run_injection_audit(&opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("injection-audit: {e}");
            // Config errors (bad --signatures file, invalid regex) → usage_error (2).
            // I/O errors (unreadable sweep dir, bad trajectory) → scan_error (35).
            let code = if matches!(e, Error::Config(_)) {
                ExitCode::UsageError
            } else {
                ExitCode::InjectionAuditScanError
            };
            exit_with_outcome(code, code.outcome_class());
        }
    };

    let exit_code = report.exit_code(fail_on);

    // Render the report content.
    let report_content: String = match format {
        AuditFormat::Json => {
            let json = format_json(&report).map_err(Error::Json)?;
            serde_json::to_string_pretty(&json).map_err(Error::Json)?
        }
        AuditFormat::Jsonl => format_jsonl(&report),
        AuditFormat::Text => format_text(&report),
    };

    // Write to --output if requested.  Write failures are logged but do NOT
    // override the audit exit code — a flaky output path must not hide the
    // scan result that CI gates on.
    if let Some(ref out_path) = a.output {
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("injection-audit: failed to create output directory: {e}");
                }
            }
        }
        if let Err(e) = std::fs::write(out_path, &report_content) {
            eprintln!("injection-audit: failed to write output file: {e}");
            // If the scan already found hits or scan errors, those exit codes
            // take priority.  But if the audit was otherwise clean, a write
            // failure means the requested artifact was not produced — surface
            // that as an error rather than silently exiting 0.
            if exit_code == ExitCode::Success {
                return Err(Error::Io(e));
            }
        }
    }

    // Print to stdout.
    match format {
        AuditFormat::Json | AuditFormat::Text => println!("{report_content}"),
        AuditFormat::Jsonl => print!("{report_content}"),
    }

    if exit_code != ExitCode::Success {
        exit_with_outcome(exit_code, exit_code.outcome_class());
    }
    Ok(())
}

pub fn agent_policy_check_cmd(p: &args::PolicyCheckCmd) -> Result<(), Error> {
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

pub fn agent_apply_cmd(a: &args::AgentApplyCmd) -> Result<(), Error> {
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

pub fn agent_skills_preview_cmd(s: &args::SkillsPreviewCmd) -> Result<(), Error> {
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

#[allow(clippy::too_many_lines)]
pub async fn agent_stability_cmd(s: args::StabilityCmd) -> Result<(), Error> {
    // ── Validate --runs ───────────────────────────────────────────────────────
    if let Err(msg) = crate::run::stability::validate_runs(s.runs) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(msg)));
    }

    // ── Resolve task text ─────────────────────────────────────────────────────
    let task = match (&s.task, &s.task_file) {
        (Some(t), _) => t.clone(),
        (None, Some(path)) => {
            if path == std::path::Path::new("-") {
                use std::io::Read as _;
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(Error::Io)?;
                buf.trim().to_owned()
            } else {
                std::fs::read_to_string(path)
                    .map_err(|e| {
                        Error::Config(crate::error::ConfigError::Invalid(format!(
                            "cannot read task file '{}': {e}",
                            path.display()
                        )))
                    })?
                    .trim()
                    .to_owned()
            }
        }
        (None, None) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "one of --task or --task-file is required".into(),
            )));
        }
    };

    if task.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "task must not be empty".into(),
        )));
    }

    // ── Load config and apply CLI overrides ───────────────────────────────────
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

    // ── Derive stability name from task slug when not supplied ────────────────
    let stability_name = s.stability_name.clone().unwrap_or_else(|| {
        let slug: String = task
            .chars()
            .take(40)
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_owned();
        if slug.is_empty() {
            "stability".to_owned()
        } else {
            slug
        }
    });

    let output_dir = s.output.clone();
    let stability_name_for_json = stability_name.clone();
    let stability_args = crate::run::stability::StabilityArgs {
        task,
        runs: s.runs,
        config: cfg,
        output_dir,
        stability_name,
        verify: s.verify,
        verify_timeout_secs: s.verify_timeout_secs,
        fail_under: s.fail_under,
        cost_limit_usd: s.cost_limit_usd,
        task_timeout_secs: s.task_timeout_secs,
        step_limit: s.step_limit,
        per_task_budget_usd: s.per_task_budget_usd,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        print_summary: s.format != Some(args::StabilityFormatArg::Json),
    };

    let exit_code = crate::run::stability::run(stability_args).await?;

    // ── --format json: print artifact to stdout ───────────────────────────────
    if s.format == Some(args::StabilityFormatArg::Json) {
        let result_dir = s.output.join(&stability_name_for_json);
        let result_path = result_dir.join("stability-results.json");
        if let Ok(json_text) = std::fs::read_to_string(&result_path) {
            println!("{json_text}");
        }
    }

    if exit_code != ExitCode::Success {
        exit_with_outcome(exit_code, exit_code.outcome_class());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn agent_best_of_cmd(b: args::BestOfCmd) -> Result<(), Error> {
    // ── Validate --runs ───────────────────────────────────────────────────────
    if let Err(msg) = crate::run::best_of::validate_runs(b.runs) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(msg)));
    }

    // ── Require --verify ──────────────────────────────────────────────────────
    if b.verify.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "--verify is required for `agent best-of`; it is the oracle that drives \
             selection. Without it there is no way to score runs against each other. \
             Use `mini` if you just want to run a single task."
                .into(),
        )));
    }

    // ── Resolve task text ─────────────────────────────────────────────────────
    let task = match (&b.task, &b.task_file) {
        (Some(t), _) => t.clone(),
        (None, Some(path)) => {
            if path == std::path::Path::new("-") {
                use std::io::Read as _;
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(Error::Io)?;
                buf.trim().to_owned()
            } else {
                std::fs::read_to_string(path)
                    .map_err(|e| {
                        Error::Config(crate::error::ConfigError::Invalid(format!(
                            "cannot read task file '{}': {e}",
                            path.display()
                        )))
                    })?
                    .trim()
                    .to_owned()
            }
        }
        (None, None) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "one of --task or --task-file is required".into(),
            )));
        }
    };

    if task.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "task must not be empty".into(),
        )));
    }

    // ── Load config and apply CLI overrides ───────────────────────────────────
    let mut cfg = match &b.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };

    cfg.root.model.name.clone_from(&b.model);

    if let Some(v) = b.step_limit {
        cfg.root.agent.step_limit = v;
    }
    if let Some(v) = b.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(v);
    }
    if let Some(kind) = &b.env {
        cfg.root.environment.kind = parse_env_kind(kind.as_str())?;
    }
    if let Some(img) = b.docker_image.clone() {
        cfg.root.environment.docker_image = Some(img);
    }
    if let Some(v) = b.detect_stagnation {
        cfg.root.agent.detect_stagnation = v;
    }
    if let Some(v) = b.history_max_input_tokens {
        cfg.root.agent.history_max_input_tokens = Some(v);
    }
    if let Some(v) = b.history_keep_last_observations {
        cfg.root.agent.history_keep_last_observations = Some(v);
    }
    apply_mcp_server_overrides(&mut cfg, &b.mcp_servers)?;

    // ── Derive best-of name from task slug when not supplied ──────────────────
    let best_of_name = b.best_of_name.clone().unwrap_or_else(|| {
        let slug: String = task
            .chars()
            .take(40)
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_owned();
        if slug.is_empty() {
            "best-of".to_owned()
        } else {
            slug
        }
    });

    let output_dir = b.output.clone();
    let best_of_name_for_json = best_of_name.clone();

    let best_of_args = crate::run::best_of::BestOfArgs {
        task,
        runs: b.runs,
        config: cfg,
        output_dir,
        best_of_name,
        verify: b.verify,
        verify_timeout_secs: b.verify_timeout_secs,
        cost_limit_usd: b.cost_limit_usd,
        task_timeout_secs: b.task_timeout_secs,
        step_limit: b.step_limit,
        per_task_budget_usd: b.per_task_budget_usd,
        output_patch: b.output_patch,
        allow_no_pass: b.allow_no_pass,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
        print_summary: b.format != Some(args::BestOfFormatArg::Json),
    };

    let exit_code = crate::run::best_of::run(best_of_args).await?;

    // ── --format json: print artifact to stdout ───────────────────────────────
    if b.format == Some(args::BestOfFormatArg::Json) {
        let result_dir = b.output.join(&best_of_name_for_json);
        let result_path = result_dir.join("best-of-results.json");
        if let Ok(json_text) = std::fs::read_to_string(&result_path) {
            println!("{json_text}");
        }
    }

    if exit_code != ExitCode::Success {
        exit_with_outcome(exit_code, exit_code.outcome_class());
    }
    Ok(())
}

pub fn agent_profile_cmd(p: &args::AgentProfileCmd) -> Result<(), Error> {
    use crate::run::agent_profile::{
        AgentProfileOpts, ProfileFormat, format_text, run_agent_profile,
    };

    let format = match p.format.as_str() {
        "json" => ProfileFormat::Json,
        "text" | "" => ProfileFormat::Text,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--format '{other}' is not valid; use 'text' or 'json'"
            ))));
        }
    };

    let opts = AgentProfileOpts {
        trajectory_path: p.trajectory.clone(),
        format,
    };

    let report = run_agent_profile(&opts)?;

    match format {
        ProfileFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(Error::Json)?
            );
        }
        ProfileFormat::Text => {
            print!("{}", format_text(&report));
        }
    }

    Ok(())
}

pub fn agent_annotate_cmd(a: &args::AgentAnnotateCmd) -> Result<(), Error> {
    use crate::run::agent_annotate::{
        AnnotateFormat, AnnotateOpts, ShowOpts, StepNoteInput, Verdict, instance_id_from_path,
        render_show_text, render_write_text, run_annotate, run_show, sidecar_path,
    };

    let format = match a.format.as_str() {
        "json" => AnnotateFormat::Json,
        "text" | "" => AnnotateFormat::Text,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--format '{other}' is not valid; use 'text' or 'json'"
            ))));
        }
    };

    if a.show {
        let opts = ShowOpts {
            trajectory_path: a.trajectory.clone(),
            format,
        };
        let ann = run_show(&opts)?;
        match format {
            AnnotateFormat::Json => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ann).map_err(Error::Json)?
                );
            }
            AnnotateFormat::Text => {
                print!("{}", render_show_text(&ann));
            }
        }
        return Ok(());
    }

    // Write mode: --verdict is required.
    let verdict_str = a.verdict.as_deref().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "agent annotate: --verdict is required in write mode \
             (use: correct, incorrect, partial, unsure)"
                .into(),
        ))
    })?;

    let verdict = Verdict::parse(verdict_str).ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "agent annotate: unknown --verdict `{verdict_str}`; \
             expected one of: correct, incorrect, partial, unsure"
        )))
    })?;

    // Parse --step-note <INDEX>=<TEXT> entries.
    let mut step_notes = Vec::new();
    for raw in &a.step_notes {
        let (idx_str, note_text) = raw.split_once('=').ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "agent annotate: --step-note must be in the form INDEX=TEXT, got `{raw}`"
            )))
        })?;
        let step: usize = idx_str.trim().parse().map_err(|_| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "agent annotate: --step-note index `{idx_str}` is not a valid integer"
            )))
        })?;
        step_notes.push(StepNoteInput {
            step,
            note: note_text.to_owned(),
        });
    }

    let opts = AnnotateOpts {
        trajectory_path: a.trajectory.clone(),
        verdict,
        failure_category: a.failure_category.clone(),
        note: a.note.clone(),
        step_notes,
        force: a.force,
    };

    run_annotate(&opts)?;

    let sidecar = sidecar_path(&a.trajectory);
    match format {
        AnnotateFormat::Json => {
            let msg = serde_json::json!({
                "status": "written",
                "verdict": verdict.as_str(),
                "instance_id": instance_id_from_path(&a.trajectory),
                "sidecar": sidecar.display().to_string(),
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&msg).map_err(Error::Json)?
            );
        }
        AnnotateFormat::Text => {
            print!("{}", render_write_text(&a.trajectory, verdict, &sidecar));
        }
    }

    Ok(())
}

pub fn agent_runs_cmd(r: &args::AgentRunsCmd) -> Result<(), Error> {
    use crate::run::agent_runs::{
        AgentRunsOpts, RunsFilter, RunsFormat, RunsSort, format_text, run_agent_runs,
    };

    let format = match r.format.as_str() {
        "json" => RunsFormat::Json,
        "text" | "" => RunsFormat::Text,
        other => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--format '{other}' is not valid; use 'text' or 'json'"
            ))));
        }
    };

    let mut filters = Vec::new();
    for raw in &r.filters {
        filters.push(RunsFilter::parse(raw)?);
    }

    let sort = RunsSort::parse(&r.sort)?;

    let opts = AgentRunsOpts {
        dir: r.dir.clone(),
        recursive: r.recursive,
        format,
        filters,
        sort,
    };

    let report = run_agent_runs(&opts)?;

    match format {
        RunsFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(Error::Json)?
            );
        }
        RunsFormat::Text => {
            print!("{}", format_text(&report));
        }
    }

    Ok(())
}

pub fn agent_fs_audit_cmd(a: &args::FsAuditCmd) -> Result<(), Error> {
    use crate::run::fs_audit::{
        FsAuditFormat, FsAuditOpts, FsAuditSource, format_json, format_text, parse_format,
        run_fs_audit,
    };

    // Exactly one of --trajectory / --sweep is required (clap group enforces this,
    // but guard here for a helpful error message).
    let source = match (&a.trajectory, &a.sweep) {
        (Some(p), None) => FsAuditSource::Trajectory(p.clone()),
        (None, Some(d)) => FsAuditSource::Sweep(d.clone()),
        (None, None) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "one of --trajectory or --sweep is required".to_owned(),
            )));
        }
        (Some(_), Some(_)) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "--trajectory and --sweep are mutually exclusive".to_owned(),
            )));
        }
    };

    let format = parse_format(a.format.as_str()).map_err(Error::Config)?;

    let opts = FsAuditOpts {
        source,
        workdir_override: a.workdir.clone(),
        allow: a.allow.clone(),
        format,
    };

    let report = match run_fs_audit(&opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fs-audit: {e}");
            let code = if matches!(e, Error::Config(_)) {
                ExitCode::UsageError
            } else {
                ExitCode::FsAuditScanError
            };
            exit_with_outcome(code, code.outcome_class());
        }
    };

    let exit_code = report.exit_code();

    let report_content = match format {
        FsAuditFormat::Json => {
            let json = format_json(&report).map_err(Error::Json)?;
            serde_json::to_string_pretty(&json).map_err(Error::Json)?
        }
        FsAuditFormat::Text => format_text(&report),
    };

    println!("{report_content}");

    if exit_code != ExitCode::Success {
        exit_with_outcome(exit_code, exit_code.outcome_class());
    }
    Ok(())
}

pub fn agent_artifact_check_cmd(a: &args::ArtifactCheckCmd) -> Result<(), Error> {
    use crate::error::ConfigError;
    use crate::run::artifact_check::{
        ArtifactCheckOpts, ArtifactCheckSource, format_json, format_text, run_artifact_check,
    };

    if a.format != "text" && a.format != "json" {
        return Err(Error::Config(ConfigError::Usage(format!(
            "unknown --format value {:?}; expected 'text' or 'json'",
            a.format
        ))));
    }

    let opts = ArtifactCheckOpts {
        source: ArtifactCheckSource::Paths(a.paths.clone()),
        strict: a.strict,
    };

    let output = run_artifact_check(&opts).map_err(|e| {
        eprintln!("artifact-check: {e}");
        e
    })?;

    let content = if a.format == "json" {
        let json = format_json(&output).map_err(Error::Json)?;
        serde_json::to_string_pretty(&json).map_err(Error::Json)?
    } else {
        format_text(&output)
    };

    println!("{content}");

    if output.has_failures() {
        exit_with_outcome(
            ExitCode::ArtifactCheckFailure,
            ExitCode::ArtifactCheckFailure.outcome_class(),
        );
    }

    Ok(())
}

pub async fn agent_suite_cmd(s: args::SuiteCmd) -> Result<(), Error> {
    let mut cfg = match &s.config {
        Some(p) => Config::load(p)?,
        None => Config::defaults()?,
    };

    // `--model` has no clap default (it's an `Option`) so `--check` can tell
    // "not passed" apart from "explicitly passed the default value"; the
    // live run path still always resolves to a concrete model name here.
    cfg.root.model.name = s
        .model
        .clone()
        .unwrap_or_else(|| crate::run::config_resolve::CLAP_DEFAULT_MODEL.to_owned());

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

    if s.check {
        let check_args = crate::run::suite_check::SuiteCheckArgs {
            tasks_file: s.tasks_file,
            format_override: s.format,
            suite_name,
            config: cfg,
            config_path: s.config,
            verify: s.verify,
            suite_cost_limit_usd: s.suite_cost_limit_usd,
            per_task_budget_usd: s.per_task_budget_usd,
            step_limit_flag: s.step_limit,
            model_flag: s.model.clone(),
            detect_stagnation_flag: s.detect_stagnation,
            strict: s.strict,
        };
        let report = crate::run::suite_check::run(&check_args).await?;
        match s.check_format.as_str() {
            "json" => {
                let wrapped = serde_json::json!({ "suite_check": &report });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&wrapped).map_err(Error::Json)?
                );
            }
            _ => print!("{}", crate::run::suite_check::render_text(&report)),
        }
        if !report.ok {
            exit_with_outcome(
                ExitCode::PreflightFailure,
                "agent suite --check found at least one fatal preflight failure",
            );
        }
        return Ok(());
    }

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
        rerun_failed: s.rerun_failed,
        deterministic_responses: None,
        deterministic_usage_per_call: None,
    };

    let exit_code = crate::run::suite::run(suite_args).await?;
    if exit_code != ExitCode::Success {
        exit_with_outcome(exit_code, "suite completed with failures");
    }
    Ok(())
}
