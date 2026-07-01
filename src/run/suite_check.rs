//! `agent suite --check` — zero-spend preflight for a personal-eval task pack
//! (issue #821).
//!
//! Validates everything `agent suite` would need before launching a single
//! paid agent loop: the pack parses, every task has a non-empty `id`/`task`
//! and unique id, every `--verify`/per-task `verify` entry is well-formed and
//! its command is statically launchable, configured MCP servers and hooks
//! start (reusing the `scriptability-check` probe), and the resolved suite
//! config carries no silent clap-default override hazard (reusing the
//! `agent config resolve` hazard detector). No model calls and no agent loop
//! are ever started; no files are written (this module takes no output
//! directory).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::config::schema::EnvKind;
use crate::error::Error;
use crate::run::agent_doctor::{is_executable, resolve_on_path};
use crate::run::config_resolve::{ConfigResolveArgs, OverrideHazard, run_config_resolve};
use crate::run::scriptability_check;
use crate::run::suite::{
    SuiteTaskSpec, TaskFileFormat, collect_task_validation_issues, parse_task_file,
    parse_verify_checks, validate_suite_name,
};

/// Schema version for [`SuiteCheckReport`]'s machine-readable form.
pub const SUITE_CHECK_SCHEMA_VERSION: u32 = 1;

// ── report types ──────────────────────────────────────────────────────────────

/// Outcome of a single preflight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Fail,
    /// Non-fatal unless `--strict` was requested.
    Warn,
}

/// One row of the preflight checklist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckItem {
    /// Stable check id, e.g. `pack_parse`, `unique_task_ids`,
    /// `verify:lint`, `mcp_server`, `hook:pre_tool_use`, `hazard:model.name`.
    pub check: String,
    /// Task id / MCP server name / hook name / hazard field this check is
    /// scoped to. `None` for suite-wide checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub status: CheckStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl CheckItem {
    fn pass(check: impl Into<String>, target: Option<String>) -> Self {
        Self {
            check: check.into(),
            target,
            status: CheckStatus::Pass,
            message: None,
        }
    }

    fn fail(check: impl Into<String>, target: Option<String>, message: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            target,
            status: CheckStatus::Fail,
            message: Some(message.into()),
        }
    }

    fn warn(check: impl Into<String>, target: Option<String>, message: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            target,
            status: CheckStatus::Warn,
            message: Some(message.into()),
        }
    }
}

/// Full report returned by [`run`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteCheckReport {
    pub schema_version: u32,
    pub tasks_file: String,
    pub suite_name: String,
    /// `0` when the pack failed to parse.
    pub task_count: usize,
    /// Suite-level `--verify` entries plus every per-task `verify` entry.
    pub verify_check_count: usize,
    pub mcp_server_count: usize,
    pub hook_count: usize,
    /// `--per-task-budget-usd * task_count`; `None` when no per-task budget
    /// was given (worst-case cost cannot be bounded arithmetically).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_worst_case_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite_cost_limit_usd: Option<f64>,
    /// Every check performed, in execution order, including passes.
    pub checks: Vec<CheckItem>,
    /// Config-provenance hazards (also folded into `checks` as `hazard:<field>`).
    pub hazards: Vec<OverrideHazard>,
    /// Whether hazards were escalated to fatal (`--strict`).
    pub strict: bool,
    pub duration_ms: u64,
    /// `true` iff no check has `status: fail`.
    pub ok: bool,
}

// ── arguments ──────────────────────────────────────────────────────────────────

/// Arguments for [`run`]. Deliberately has no output-directory field: a
/// preflight check makes no writes (AC: read-only except transient MCP/hook
/// probe processes).
pub struct SuiteCheckArgs {
    pub tasks_file: PathBuf,
    pub format_override: Option<String>,
    pub suite_name: String,
    /// Fully-resolved suite config (after `--mcp-server`, `--step-limit`,
    /// etc. overrides have been applied) — used for the MCP/hook probe.
    pub config: Config,
    /// The raw `--config` path, used only for the hazard detector (mirrors
    /// what a standalone `agent config resolve --config <path>` would see).
    pub config_path: Option<PathBuf>,
    /// Suite-level `--verify NAME:COMMAND` entries.
    pub verify: Vec<String>,
    pub suite_cost_limit_usd: Option<f64>,
    pub per_task_budget_usd: Option<f64>,
    pub step_limit_flag: Option<u32>,
    /// The `--model` value, `Some(_)` iff the caller explicitly passed the
    /// flag (regardless of whether the value matches the clap default).
    /// Forwarded to the hazard detector so a deliberately-acknowledged
    /// `--model` override doesn't get flagged as a silent config-file
    /// override hazard.
    pub model_flag: Option<String>,
    /// Mirrors `agent suite --detect-stagnation`. Forwarded to the hazard
    /// detector's stagnation-bounds validation so `--check` sees the same
    /// effective `detect_stagnation` the live run resolves — otherwise a
    /// config file that disables stagnation (masking invalid thresholds)
    /// could pass preflight while an explicit `--detect-stagnation` on the
    /// live invocation re-enables it and `DefaultAgent` rejects the task.
    pub detect_stagnation_flag: Option<bool>,
    pub strict: bool,
}

// ── main entry point ──────────────────────────────────────────────────────────

/// Run the preflight and return a [`SuiteCheckReport`]. Performs zero model
/// calls and starts no agent loop. For `environment.kind = local`, the only
/// subprocesses spawned are the transient MCP-server/hook probes already
/// used by `bench scriptability-check`; for `environment.kind = docker`
/// those probes are skipped entirely (nothing configured is executed on
/// the host — see the `is_docker` branch below).
#[allow(clippy::too_many_lines)]
pub async fn run(args: &SuiteCheckArgs) -> Result<SuiteCheckReport, Error> {
    let started = std::time::Instant::now();
    let mut checks: Vec<CheckItem> = Vec::new();

    // ── Format detection + parse ────────────────────────────────────────
    let format = match resolve_format(&args.tasks_file, args.format_override.as_deref()) {
        Ok(f) => {
            checks.push(CheckItem::pass("format_detect", None));
            Some(f)
        }
        Err(msg) => {
            checks.push(CheckItem::fail("format_detect", None, msg));
            None
        }
    };

    let content = match format {
        Some(_) => match std::fs::read_to_string(&args.tasks_file) {
            Ok(c) => {
                checks.push(CheckItem::pass("pack_readable", None));
                Some(c)
            }
            Err(e) => {
                checks.push(CheckItem::fail(
                    "pack_readable",
                    None,
                    format!(
                        "cannot read tasks file '{}': {e}",
                        args.tasks_file.display()
                    ),
                ));
                None
            }
        },
        None => None,
    };

    let tasks: Option<Vec<SuiteTaskSpec>> = match (format, content) {
        (Some(fmt), Some(content)) => match parse_task_file(&content, fmt) {
            Ok(t) => {
                checks.push(CheckItem::pass("pack_parse", None));
                Some(t)
            }
            Err(e) => {
                checks.push(CheckItem::fail("pack_parse", None, e.to_string()));
                None
            }
        },
        _ => None,
    };

    // ── Suite name (output-directory path segment) ─────────────────────
    // The live run rejects an unsafe suite name before creating the output
    // directory; --check must catch the same problem so it can't report
    // PASS for a pack whose real run would immediately fail on invocation.
    match validate_suite_name(&args.suite_name) {
        Ok(()) => checks.push(CheckItem::pass("suite_name_safe", None)),
        Err(msg) => checks.push(CheckItem::fail("suite_name_safe", None, msg)),
    }

    // With `--env docker`, verify commands, MCP servers, and hooks all run
    // inside the container image in a live run — not on this host. A
    // host-side probe/PATH lookup can't authoritatively validate them (a
    // docker-only binary would false-fail; a host-only same-named binary
    // would false-pass). Verify-command launchability is skipped entirely
    // in that case (format is still validated); MCP/hook probe failures are
    // downgraded to warnings instead of fatal (see below).
    let is_docker = matches!(args.config.root.environment.kind, EnvKind::Docker);

    // `mini::build_docker_env` unconditionally requires `docker_image`, and
    // a binary built without the (non-default) `docker` cargo feature
    // refuses `--env docker` entirely — both fail before any task runs.
    // Catch both statically here rather than reporting PASS for a suite
    // that cannot start. This does not probe the Docker daemon itself or
    // pull the image — run `agent doctor` for full host readiness.
    if is_docker {
        if cfg!(feature = "docker") {
            checks.push(CheckItem::pass("docker_feature_compiled", None));
        } else {
            checks.push(CheckItem::fail(
                "docker_feature_compiled",
                None,
                "docker support not compiled in — rebuild with --features docker",
            ));
        }
        // `Some("")`/whitespace-only is accepted by the `Option` check but
        // still passes an unusable image straight to `docker run`, which
        // fails with a confusing error instead of the clear one above.
        match args.config.root.environment.docker_image.as_deref() {
            Some(image) if !image.trim().is_empty() => {
                checks.push(CheckItem::pass("docker_image_configured", None));
            }
            _ => checks.push(CheckItem::fail(
                "docker_image_configured",
                None,
                "environment.kind=docker requires a non-empty environment.docker_image; run \
                 `agent doctor` to also verify the Docker daemon is reachable",
            )),
        }
    }
    let skip_launchability = is_docker;

    // ── Non-empty pack + per-task field/id validation ──────────────────
    let mut task_count = 0usize;
    let mut per_task_verify_count = 0usize;
    if let Some(tasks) = &tasks {
        if tasks.is_empty() {
            checks.push(CheckItem::fail(
                "pack_non_empty",
                None,
                "task pack contains no tasks",
            ));
        } else {
            checks.push(CheckItem::pass("pack_non_empty", None));
        }
        task_count = tasks.len();

        let issues = collect_task_validation_issues(tasks);
        if issues.is_empty() {
            checks.push(CheckItem::pass("task_fields_and_ids", None));
        } else {
            for issue in issues {
                checks.push(CheckItem::fail(
                    "task_fields_and_ids",
                    issue.task_id,
                    issue.message,
                ));
            }
        }

        for task in tasks {
            per_task_verify_count += task.verify.len();
            for spec in &task.verify {
                checks.push(check_verify_spec(
                    Some(task.id.clone()),
                    spec,
                    skip_launchability,
                ));
            }
        }
    }

    // ── Suite-level verify entries ───────────────────────────────────────
    for spec in &args.verify {
        checks.push(check_verify_spec(None, spec, skip_launchability));
    }
    let verify_check_count = args.verify.len() + per_task_verify_count;

    // ── MCP servers + hooks (reuse scriptability-check, no artifact write) ─
    // `scriptability_check` probes via a `LocalEnvironment` — it actually
    // spawns each configured MCP server and executes each configured hook
    // command. Under `--env docker` a live run never does that on the host
    // (MCP servers/hooks launch inside the container), so doing it here
    // would both misrepresent the container and — for a destructive or
    // container-only command — cause real side effects on the host outside
    // the sandbox the operator chose. Skip the probe entirely for docker;
    // report each configured server/hook as unprobed instead.
    let mcp_server_count = args.config.root.agent.mcp_servers.len();
    let hook_count = args.config.root.agent.hooks.pre_tool_use.len()
        + args.config.root.agent.hooks.post_tool_use.len();
    if is_docker {
        for (i, _) in args.config.root.agent.mcp_servers.iter().enumerate() {
            checks.push(CheckItem::warn(
                "mcp_server",
                Some(format!("mcp-{i}")),
                "not probed: --env docker is configured; running configured commands on the \
                 host would bypass the sandbox the operator chose",
            ));
        }
        for hook_cfg in args
            .config
            .root
            .agent
            .hooks
            .pre_tool_use
            .iter()
            .map(|h| (h, "pre_tool_use"))
            .chain(
                args.config
                    .root
                    .agent
                    .hooks
                    .post_tool_use
                    .iter()
                    .map(|h| (h, "post_tool_use")),
            )
        {
            let (hook, phase) = hook_cfg;
            checks.push(CheckItem::warn(
                format!("hook:{phase}"),
                Some(hook.name.clone()),
                "not probed: --env docker is configured; running configured commands on the \
                 host would bypass the sandbox the operator chose",
            ));
        }
    } else {
        let scriptability_report = scriptability_check::run_with_config(&args.config, None).await?;
        for server in &scriptability_report.servers {
            checks.push(scriptability_check_item(
                "mcp_server".to_owned(),
                server.name.clone(),
                server.ok,
                server.error.clone(),
            ));
        }
        for hook in &scriptability_report.hooks {
            checks.push(scriptability_check_item(
                format!("hook:{}", hook.phase),
                hook.name.clone(),
                hook.ok,
                hook.error.clone(),
            ));
        }
    }

    // ── Config-provenance hazards (reuse `agent config resolve`) ───────────
    let hazard_args = ConfigResolveArgs {
        config: args.config_path.clone(),
        model_flag: args.model_flag.clone(),
        step_limit_flag: args.step_limit_flag,
        observation_max_bytes_flag: None,
        observation_head_ratio_flag: None,
        per_task_budget_usd_flag: None,
        hide_budget_from_agent_flag: false,
        env_flag: None,
        workdir_flag: None,
        // `agent suite` has no `--stagnation-repeat-threshold`/`--stagnation-window`
        // flags of its own (only `--detect-stagnation`), so those two mirror
        // that same gap in the live run path — not something --check can
        // close on its own.
        detect_stagnation_flag: args.detect_stagnation_flag,
        stagnation_repeat_threshold_flag: None,
        stagnation_window_flag: None,
    };
    // `agent config resolve` reports hazards for several commands (e.g. the
    // `agent.step_limit` hazard only affects `bench swebench`/`rehearsal`/
    // `forecast`/`doctor`, not `agent suite`, which honors a config-file
    // step_limit when `--step-limit` is absent). Only surface hazards that
    // actually affect `agent suite`.
    //
    // `run_config_resolve` can itself return `Err` (e.g. invalid stagnation
    // bounds when detection is effectively enabled) — report that as a
    // fatal check rather than aborting `run` with an unstructured error, so
    // `--check` always produces a full report with the documented exit code.
    let hazards: Vec<OverrideHazard> = match run_config_resolve(&hazard_args) {
        Ok(resolved) => resolved
            .hazards
            .into_iter()
            .filter(|h| h.commands_affected.iter().any(|c| c == "agent suite"))
            .collect(),
        Err(e) => {
            checks.push(CheckItem::fail("config_resolve", None, e.to_string()));
            Vec::new()
        }
    };
    for hazard in &hazards {
        let target = Some(hazard.field.clone());
        if args.strict {
            checks.push(CheckItem::fail(
                format!("hazard:{}", hazard.field),
                target,
                hazard.message.clone(),
            ));
        } else {
            checks.push(CheckItem::warn(
                format!("hazard:{}", hazard.field),
                target,
                hazard.message.clone(),
            ));
        }
    }

    // ── Worst-case cost (arithmetic only — no model call) ──────────────────
    // The `--per-task-budget-usd` flag always wins when passed; otherwise
    // fall back to a config-file `[agent] per_task_budget_usd`, which the
    // live suite run also honors per-task when no flag override is given.
    let effective_per_task_budget_usd =
        args.per_task_budget_usd
            .or(args.config.root.agent.per_task_budget_usd);
    #[allow(clippy::cast_precision_loss)]
    let estimated_worst_case_cost_usd =
        effective_per_task_budget_usd.map(|budget| budget * task_count as f64);
    if let (Some(cost), Some(limit)) = (estimated_worst_case_cost_usd, args.suite_cost_limit_usd) {
        if cost > limit {
            checks.push(CheckItem::warn(
                "cost_ceiling",
                None,
                format!(
                    "worst-case cost ${cost:.4} (per-task-budget-usd x task_count) exceeds \
                     --suite-cost-limit-usd ${limit:.4}"
                ),
            ));
        } else {
            checks.push(CheckItem::pass("cost_ceiling", None));
        }
    }

    let ok = !checks.iter().any(|c| c.status == CheckStatus::Fail);

    Ok(SuiteCheckReport {
        schema_version: SUITE_CHECK_SCHEMA_VERSION,
        tasks_file: args.tasks_file.display().to_string(),
        suite_name: args.suite_name.clone(),
        task_count,
        verify_check_count,
        mcp_server_count,
        hook_count,
        estimated_worst_case_cost_usd,
        suite_cost_limit_usd: args.suite_cost_limit_usd,
        checks,
        hazards,
        strict: args.strict,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ok,
    })
}

// ── format resolution ─────────────────────────────────────────────────────────

fn resolve_format(path: &Path, format_override: Option<&str>) -> Result<TaskFileFormat, String> {
    if let Some(s) = format_override {
        return TaskFileFormat::parse(s)
            .ok_or_else(|| format!("--format '{s}' is not valid; use yaml, jsonl, or toml"));
    }
    TaskFileFormat::from_extension(path).ok_or_else(|| {
        format!(
            "cannot detect format from '{}'; use --format yaml|jsonl|toml",
            path.display()
        )
    })
}

// ── MCP/hook probe result → CheckItem ────────────────────────────────────────

/// Build a [`CheckItem`] from a `scriptability_check` server/hook result.
/// A failure is downgraded from `Fail` to `Warn` under `--env docker`,
/// since the probe ran on the host, not inside the configured container.
/// Build a [`CheckItem`] from a `scriptability_check` server/hook result.
/// Only ever called for `environment.kind = local` — under `--env docker`
/// the probe itself is skipped rather than run and downgraded (see the
/// `is_docker` branch in [`run`]).
fn scriptability_check_item(
    check: String,
    target: String,
    ok: bool,
    error: Option<String>,
) -> CheckItem {
    if ok {
        CheckItem::pass(check, Some(target))
    } else {
        let message = error.unwrap_or_else(|| "probe failed".to_owned());
        CheckItem::fail(check, Some(target), message)
    }
}

// ── verify-check format + launchability ─────────────────────────────────────────

fn check_verify_spec(target: Option<String>, spec: &str, skip_launchability: bool) -> CheckItem {
    let specs = [spec.to_owned()];
    match parse_verify_checks(&specs) {
        Ok(parsed) => {
            let Some(check) = parsed.first() else {
                return CheckItem::fail("verify", target, "empty --verify entry");
            };
            let check_id = format!("verify:{}", check.name);
            if skip_launchability {
                // Inconclusive, not a pass: a typo'd or missing binary
                // inside the container image would still be undetectable
                // from here, so asserting Pass would hide it from `ok` and
                // from a `--format text` reader skimming for failures.
                return CheckItem::warn(
                    check_id,
                    target,
                    "launchability not checked: --env docker runs verify commands inside \
                     the container image, not on this host",
                );
            }
            if has_leading_path_assignment(&check.command) {
                // A command that sets its own PATH (e.g. `PATH=./venv/bin:$PATH
                // pytest`) is resolved by the shell against that assigned
                // value, not this process's PATH — resolving the program
                // token against our own PATH could produce either a
                // false-fail (only reachable via the assignment) or a
                // false-pass (the assignment would actually hide it).
                // Emulating shell variable expansion is out of scope; be
                // honest that this can't be statically determined.
                return CheckItem::warn(
                    check_id,
                    target,
                    "launchability not checked: verify command sets PATH itself, which this \
                     static check does not emulate",
                );
            }
            match check_command_launchable(&check.command) {
                Ok(()) => CheckItem::pass(check_id, target),
                Err(reason) => CheckItem::fail(check_id, target, reason),
            }
        }
        Err(e) => CheckItem::fail("verify", target, e.to_string()),
    }
}

/// Best-effort *static* launchability check for a verify command: does the
/// first program token resolve to something runnable? Never executes the
/// command (see issue #821 Out of Scope — launchability only, not
/// correctness). Complex shell constructs (subshells, command substitution)
/// are not specially parsed; operators should keep verify commands to a
/// simple `program args...` or `VAR=val program args...` shape.
fn check_command_launchable(command: &str) -> Result<(), String> {
    let Some(token) = extract_program_token(command) else {
        return Err("verify command has no resolvable program token".to_owned());
    };
    if is_shell_builtin_or_keyword(&token) {
        return Ok(());
    }
    if token.contains('/') || token.contains(std::path::MAIN_SEPARATOR) {
        let path = Path::new(&token);
        return if path.is_file() && is_executable(path) {
            Ok(())
        } else {
            Err(format!("'{token}' does not resolve to an executable file"))
        };
    }
    if resolve_on_path(&token).is_some() {
        Ok(())
    } else {
        Err(format!("'{token}' was not found on PATH"))
    }
}

/// Extract the first non-`VAR=value` token from a shell command string.
fn extract_program_token(command: &str) -> Option<String> {
    let mut rest = command;
    loop {
        let (word, remainder) = next_shell_word(rest)?;
        rest = remainder;
        if is_env_assignment(&word) {
            continue;
        }
        return Some(word);
    }
}

/// Pop the next whitespace-delimited word off `s`, honouring single/double
/// quoting (quote characters themselves are stripped). Returns the word and
/// the unconsumed remainder.
fn next_shell_word(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut split_at = s.len();
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c.is_whitespace() {
            split_at = i;
            break;
        } else {
            word.push(c);
        }
    }
    if word.is_empty() {
        return None;
    }
    Some((word, &s[split_at..]))
}

fn is_env_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word[..eq];
    !name.is_empty()
        && name.chars().enumerate().all(|(i, c)| {
            if i == 0 {
                c.is_ascii_alphabetic() || c == '_'
            } else {
                c.is_ascii_alphanumeric() || c == '_'
            }
        })
}

/// Whether `command` begins with one or more `VAR=value` assignments that
/// include a `PATH=` override. `resolve_on_path` only ever consults this
/// process's own `PATH`, so a command that assigns its own can't be
/// statically resolved against it without emulating shell expansion.
fn has_leading_path_assignment(command: &str) -> bool {
    let mut rest = command;
    while let Some((word, remainder)) = next_shell_word(rest) {
        if !is_env_assignment(&word) {
            return false;
        }
        if word.split('=').next() == Some("PATH") {
            return true;
        }
        rest = remainder;
    }
    false
}

const SHELL_BUILTINS_AND_KEYWORDS: &[&str] = &[
    "cd", "pushd", "popd", "echo", "printf", "export", "unset", "set", "source", ".", ":", "true",
    "false", "test", "[", "[[", "exit", "return", "eval", "exec", "read", "type", "command",
    "builtin", "pwd", "alias", "unalias", "local", "declare", "typeset", "readonly", "shift",
    "trap", "wait", "jobs", "ulimit", "umask", "hash", "let", "time", "if", "then", "elif", "else",
    "fi", "for", "while", "until", "do", "done", "case", "esac", "function", "select", "in", "not",
    "{", "}",
];

fn is_shell_builtin_or_keyword(token: &str) -> bool {
    SHELL_BUILTINS_AND_KEYWORDS.contains(&token)
}

// ── text renderer ─────────────────────────────────────────────────────────────

#[must_use]
pub fn render_text(report: &SuiteCheckReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let status = if report.ok { "PASS" } else { "FAIL" };
    let _ = writeln!(
        out,
        "agent suite --check [{status}]  tasks-file={}  ({} ms)",
        report.tasks_file, report.duration_ms
    );
    let _ = write!(
        out,
        "{} task(s), {} verify check(s), {} MCP server(s), {} hook(s)",
        report.task_count, report.verify_check_count, report.mcp_server_count, report.hook_count
    );
    match (
        report.estimated_worst_case_cost_usd,
        report.suite_cost_limit_usd,
    ) {
        (Some(cost), Some(limit)) => {
            let _ = write!(out, ", worst-case ${cost:.4} (limit ${limit:.4})");
        }
        (Some(cost), None) => {
            let _ = write!(
                out,
                ", worst-case ${cost:.4} (no --suite-cost-limit-usd set)"
            );
        }
        (None, _) => {
            let _ = write!(
                out,
                ", worst-case cost unknown (no --per-task-budget-usd set)"
            );
        }
    }
    let _ = writeln!(out);

    let failures: Vec<&CheckItem> = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .collect();
    if !failures.is_empty() {
        let _ = writeln!(out, "\nFailures:");
        for c in &failures {
            let target = c.target.as_deref().unwrap_or("-");
            let msg = c.message.as_deref().unwrap_or("");
            let _ = writeln!(out, "  [FAIL] {:<24} id={:<20} {}", c.check, target, msg);
        }
    }

    let warnings: Vec<&CheckItem> = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Warn)
        .collect();
    if !warnings.is_empty() {
        let _ = writeln!(
            out,
            "\nWarnings (non-fatal{}):",
            if report.strict {
                ", escalated by --strict"
            } else {
                ""
            }
        );
        for c in &warnings {
            let target = c.target.as_deref().unwrap_or("-");
            let msg = c.message.as_deref().unwrap_or("");
            let _ = writeln!(out, "  [WARN] {:<24} id={:<20} {}", c.check, target, msg);
        }
    }

    out
}

// ── Tests (RED → GREEN) ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn base_args(tasks_file: PathBuf) -> SuiteCheckArgs {
        SuiteCheckArgs {
            tasks_file,
            format_override: None,
            suite_name: "test-suite".into(),
            config: Config::defaults().unwrap(),
            config_path: None,
            verify: vec![],
            suite_cost_limit_usd: None,
            per_task_budget_usd: None,
            step_limit_flag: None,
            model_flag: None,
            detect_stagnation_flag: None,
            strict: false,
        }
    }

    fn write_tasks(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    // ── RED: happy path ──────────────────────────────────────────────────

    #[tokio::test]
    async fn valid_pack_with_no_mcp_or_hooks_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: fix the bug\n- id: t2\n  task: add a feature\n",
        );
        let report = run(&base_args(path)).await.unwrap();
        assert!(report.ok, "checks: {:?}", report.checks);
        assert_eq!(report.task_count, 2);
        assert_eq!(report.mcp_server_count, 0);
        assert_eq!(report.hook_count, 0);
    }

    #[tokio::test]
    async fn valid_pack_makes_no_filesystem_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix the bug\n");
        let before: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        let _ = run(&base_args(path)).await.unwrap();
        let after: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(
            before.len(),
            after.len(),
            "--check must not write any files"
        );
        assert_eq!(after.len(), 1, "only the input tasks file should exist");
    }

    // ── RED: parse / structural failures ────────────────────────────────

    #[tokio::test]
    async fn malformed_yaml_fails_pack_parse_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "not: [valid, task, schema");
        let report = run(&base_args(path)).await.unwrap();
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "pack_parse" && c.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn undetectable_format_fails_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.txt", "irrelevant");
        let report = run(&base_args(path)).await.unwrap();
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "format_detect" && c.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn unsafe_suite_name_fails_preflight() {
        // The live `agent suite` run rejects a suite name containing path
        // separators or '..' before creating the output directory; --check
        // must catch this too instead of reporting PASS for a pack whose
        // real run would fail on invocation (regression test for a gap
        // found in review, issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let mut args = base_args(path);
        args.suite_name = "../escape".to_owned();
        let report = run(&args).await.unwrap();
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "suite_name_safe" && c.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn safe_suite_name_passes_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let report = run(&base_args(path)).await.unwrap();
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "suite_name_safe" && c.status == CheckStatus::Pass)
        );
    }

    #[tokio::test]
    async fn docker_env_without_image_fails_preflight() {
        // `mini::build_docker_env` unconditionally requires
        // `environment.docker_image` and errors before any task can run;
        // --check must catch this instead of reporting PASS for a suite
        // that cannot start. Regression test for a gap found in review
        // (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let mut config = Config::defaults().unwrap();
        config.root.environment.kind = EnvKind::Docker;
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        assert!(!report.ok, "checks: {:?}", report.checks);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "docker_image_configured" && c.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn docker_env_with_blank_image_fails_preflight() {
        // `docker_image = Some("")` (or whitespace) passes the `Option`
        // check but is still an unusable image string passed straight to
        // `docker run`, which fails with a confusing error rather than the
        // clear one above. Regression test for a gap found in review
        // (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let mut config = Config::defaults().unwrap();
        config.root.environment.kind = EnvKind::Docker;
        config.root.environment.docker_image = Some("   ".to_owned());
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        assert!(!report.ok, "checks: {:?}", report.checks);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "docker_image_configured" && c.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn docker_env_with_image_passes_that_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let mut config = Config::defaults().unwrap();
        config.root.environment.kind = EnvKind::Docker;
        config.root.environment.docker_image = Some("my-image:latest".to_owned());
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "docker_image_configured" && c.status == CheckStatus::Pass)
        );
    }

    #[tokio::test]
    async fn local_env_has_no_docker_image_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let report = run(&base_args(path)).await.unwrap();
        assert!(
            !report
                .checks
                .iter()
                .any(|c| c.check == "docker_image_configured")
        );
        assert!(
            !report
                .checks
                .iter()
                .any(|c| c.check == "docker_feature_compiled")
        );
    }

    #[tokio::test]
    async fn docker_feature_gate_matches_this_build() {
        // A binary built without --features docker refuses --env docker
        // entirely at `mini::build_docker_env`, even with a valid
        // docker_image configured — --check must catch that too instead of
        // reporting PASS for a build that cannot start a docker suite.
        // Self-adapts to whether *this* test binary was built with the
        // feature, rather than assuming either way. Regression test for a
        // gap found in review (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let mut config = Config::defaults().unwrap();
        config.root.environment.kind = EnvKind::Docker;
        config.root.environment.docker_image = Some("my-image:latest".to_owned());
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        let feature_check = report
            .checks
            .iter()
            .find(|c| c.check == "docker_feature_compiled")
            .unwrap();
        if cfg!(feature = "docker") {
            assert_eq!(feature_check.status, CheckStatus::Pass);
        } else {
            assert_eq!(feature_check.status, CheckStatus::Fail);
            assert!(!report.ok, "checks: {:?}", report.checks);
        }
    }

    #[tokio::test]
    async fn empty_pack_fails_non_empty_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.jsonl", "");
        let report = run(&base_args(path)).await.unwrap();
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "pack_non_empty" && c.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn duplicate_task_ids_reported_with_offending_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: dup\n  task: first\n- id: dup\n  task: second\n",
        );
        let report = run(&base_args(path)).await.unwrap();
        assert!(!report.ok);
        let dup = report
            .checks
            .iter()
            .find(|c| c.status == CheckStatus::Fail && c.target.as_deref() == Some("dup"))
            .unwrap();
        assert!(dup.message.as_deref().unwrap().contains("duplicate"));
    }

    #[tokio::test]
    async fn multiple_bad_tasks_all_surface_not_just_first() {
        // Mirrors the "task 47's verify command" motivating scenario: every
        // violation must surface in one pass, not just the first.
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: ok\n  verify:\n    - bad:not-a-format-without-colon-wait-it-has-one\n- id: t1\n  task: dup id\n",
        );
        let report = run(&base_args(path)).await.unwrap();
        assert!(!report.ok);
        let fail_count = report
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .count();
        assert!(fail_count >= 2, "checks: {:?}", report.checks);
    }

    // ── RED: verify-check launchability ─────────────────────────────────

    #[tokio::test]
    async fn launchable_verify_command_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: fix it\n  verify:\n    - smoke:true\n",
        );
        let report = run(&base_args(path)).await.unwrap();
        assert!(report.ok, "checks: {:?}", report.checks);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "verify:smoke" && c.status == CheckStatus::Pass)
        );
    }

    #[tokio::test]
    async fn unlaunchable_verify_command_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: fix it\n  verify:\n    - tests:__no_such_binary_xyz_suite_check__ -q\n",
        );
        let report = run(&base_args(path)).await.unwrap();
        assert!(!report.ok);
        let failed = report
            .checks
            .iter()
            .find(|c| c.check == "verify:tests")
            .unwrap();
        assert_eq!(failed.status, CheckStatus::Fail);
        assert_eq!(failed.target.as_deref(), Some("t1"));
    }

    #[tokio::test]
    async fn docker_env_marks_verify_launchability_inconclusive_not_fatal() {
        // A binary that only exists inside the configured Docker image (not
        // on this host) must not fail preflight — the real verify command
        // runs inside the container, not on the host PATH. It also must not
        // be asserted as a definite Pass: a typo'd/missing binary inside
        // the image would be just as undetectable from here. Warn is the
        // honest status. Regression test for a false-fail (and later a
        // false-pass) found in review (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: fix it\n  verify:\n    - tests:__no_such_binary_xyz_suite_check__ -q\n",
        );
        let mut config = Config::defaults().unwrap();
        config.root.environment.kind = EnvKind::Docker;
        config.root.environment.docker_image = Some("my-image:latest".to_owned());
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        // Not asserting report.ok: this test binary may or may not be built
        // with --features docker, which independently gates `ok` (see
        // docker_feature_gate_matches_this_build). Only the
        // launchability-skip behavior under test is asserted here.
        let checked = report
            .checks
            .iter()
            .find(|c| c.check == "verify:tests")
            .unwrap();
        assert_eq!(checked.status, CheckStatus::Warn);
    }

    #[tokio::test]
    async fn docker_env_does_not_execute_mcp_servers_on_host() {
        // The live run never launches MCP servers on the host under --env
        // docker (they run inside the container), and running a configured
        // command here anyway could have real side effects outside the
        // sandbox the operator chose. Prove no execution happens at all —
        // not just that a failure is downgraded — by using a command whose
        // side effect (writing a marker file) would be observable if run.
        // Regression test for a host-execution safety gap found in review
        // (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let marker = dir.path().join("marker.txt");
        let mut config = Config::defaults().unwrap();
        config.root.environment.kind = EnvKind::Docker;
        config.root.environment.docker_image = Some("my-image:latest".to_owned());
        config
            .root
            .agent
            .mcp_servers
            .push(crate::config::McpServerCfg {
                command: format!("touch {}", marker.display()),
                timeout_secs: Some(1),
            });
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        assert!(
            !marker.exists(),
            "MCP server command must not execute on the host under --env docker"
        );
        let checked = report
            .checks
            .iter()
            .find(|c| c.check == "mcp_server")
            .unwrap();
        assert_eq!(checked.status, CheckStatus::Warn);
        assert!(checked.message.as_deref().unwrap().contains("not probed"));
    }

    #[tokio::test]
    async fn docker_env_does_not_execute_hooks_on_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let marker = dir.path().join("marker.txt");
        let mut config = Config::defaults().unwrap();
        config.root.environment.kind = EnvKind::Docker;
        config.root.environment.docker_image = Some("my-image:latest".to_owned());
        config
            .root
            .agent
            .hooks
            .pre_tool_use
            .push(crate::config::ToolHookCfg {
                name: "guard".to_owned(),
                command: format!("touch {}", marker.display()),
                timeout_secs: Some(1),
            });
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        assert!(
            !marker.exists(),
            "hook command must not execute on the host under --env docker"
        );
        let checked = report
            .checks
            .iter()
            .find(|c| c.check == "hook:pre_tool_use")
            .unwrap();
        assert_eq!(checked.status, CheckStatus::Warn);
        assert_eq!(checked.target.as_deref(), Some("guard"));
    }

    #[tokio::test]
    async fn docker_env_mcp_and_hook_counts_still_reflect_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let mut config = Config::defaults().unwrap();
        config.root.environment.kind = EnvKind::Docker;
        config.root.environment.docker_image = Some("my-image:latest".to_owned());
        config
            .root
            .agent
            .mcp_servers
            .push(crate::config::McpServerCfg {
                command: "true".to_owned(),
                timeout_secs: Some(1),
            });
        config
            .root
            .agent
            .hooks
            .post_tool_use
            .push(crate::config::ToolHookCfg {
                name: "note".to_owned(),
                command: "true".to_owned(),
                timeout_secs: Some(1),
            });
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        assert_eq!(report.mcp_server_count, 1);
        assert_eq!(report.hook_count, 1);
    }

    #[test]
    fn scriptability_check_item_reflects_probe_result() {
        let ok = scriptability_check_item("mcp_server".into(), "s".into(), true, None);
        assert_eq!(ok.status, CheckStatus::Pass);

        let fail =
            scriptability_check_item("mcp_server".into(), "s".into(), false, Some("boom".into()));
        assert_eq!(fail.status, CheckStatus::Fail);
        assert_eq!(fail.message.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn local_env_mcp_failure_stays_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let mut config = Config::defaults().unwrap();
        config
            .root
            .agent
            .mcp_servers
            .push(crate::config::McpServerCfg {
                command: "__no_such_binary_xyz_suite_check_mcp__".to_owned(),
                timeout_secs: Some(1),
            });
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        assert!(!report.ok);
        let checked = report
            .checks
            .iter()
            .find(|c| c.check == "mcp_server")
            .unwrap();
        assert_eq!(checked.status, CheckStatus::Fail);
    }

    #[tokio::test]
    async fn malformed_verify_spec_without_colon_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: fix it\n  verify:\n    - \"no-colon-here\"\n",
        );
        let report = run(&base_args(path)).await.unwrap();
        assert!(!report.ok);
    }

    #[tokio::test]
    async fn suite_level_verify_checked_even_when_no_per_task_verify() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let mut args = base_args(path);
        args.verify = vec!["lint:__no_such_binary_xyz_suite_check__".to_owned()];
        let report = run(&args).await.unwrap();
        assert!(!report.ok);
        assert_eq!(report.verify_check_count, 1);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "verify:lint" && c.target.is_none())
        );
    }

    #[tokio::test]
    async fn verify_command_with_path_override_is_inconclusive_not_pass_or_fail() {
        // `resolve_on_path` only ever consults this process's own PATH, so
        // a verify command that assigns its own PATH (e.g. a venv prefix)
        // can't be statically resolved against it — asserting Pass could
        // hide a binary that only the assigned PATH would find, and
        // asserting Fail could reject one that's genuinely reachable
        // there. Regression test for a gap found in review (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: fix it\n  verify:\n    - tests:PATH=./venv/bin:$PATH pytest -q\n",
        );
        let report = run(&base_args(path)).await.unwrap();
        assert!(report.ok, "checks: {:?}", report.checks);
        let checked = report
            .checks
            .iter()
            .find(|c| c.check == "verify:tests")
            .unwrap();
        assert_eq!(checked.status, CheckStatus::Warn);
    }

    // ── RED: hazards ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn config_hazard_is_warning_not_fatal_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let mut args = base_args(path);
        args.config_path = Some(config_path);
        let report = run(&args).await.unwrap();
        assert!(
            report.ok,
            "hazards must be non-fatal by default: {:?}",
            report.checks
        );
        assert!(!report.hazards.is_empty());
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "hazard:model.name" && c.status == CheckStatus::Warn)
        );
    }

    #[tokio::test]
    async fn config_hazard_is_fatal_under_strict() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let mut args = base_args(path);
        args.config_path = Some(config_path);
        args.strict = true;
        let report = run(&args).await.unwrap();
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "hazard:model.name" && c.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn explicit_model_flag_suppresses_model_hazard_even_under_strict() {
        // The config file's model.name matches the operator's explicit
        // --model, so this is not a *silent* override and must not be
        // flagged as a hazard — regression test for a false-positive found
        // in review (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let mut args = base_args(path);
        args.config_path = Some(config_path);
        args.model_flag = Some("claude-sonnet-4-6".to_owned());
        args.strict = true;
        let report = run(&args).await.unwrap();
        assert!(report.ok, "checks: {:?}", report.checks);
        assert!(
            !report.checks.iter().any(|c| c.check == "hazard:model.name"),
            "checks: {:?}",
            report.checks
        );
    }

    #[tokio::test]
    async fn explicit_model_flag_matching_clap_default_still_suppresses_hazard() {
        // The operator explicitly chose the clap-default model name on
        // purpose (e.g. `--model claude-opus-4-7`) to override a config
        // file that sets a different model.name. Because `model_flag` is
        // `Some(_)` regardless of which value was passed, this must count
        // as an acknowledged override, not a silent one — regression test
        // for the "explicitness vs. value" gap found in review (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let mut args = base_args(path);
        args.config_path = Some(config_path);
        args.model_flag = Some(crate::run::config_resolve::CLAP_DEFAULT_MODEL.to_owned());
        args.strict = true;
        let report = run(&args).await.unwrap();
        assert!(report.ok, "checks: {:?}", report.checks);
        assert!(
            !report.checks.iter().any(|c| c.check == "hazard:model.name"),
            "checks: {:?}",
            report.checks
        );
    }

    #[tokio::test]
    async fn step_limit_hazard_does_not_affect_agent_suite_even_under_strict() {
        // `agent.step_limit`'s hazard only applies to `bench swebench` /
        // `rehearsal` / `forecast` / `doctor` — `agent suite` honors a
        // config-file step_limit when `--step-limit` is absent, so this
        // hazard must never surface for `agent suite --check`, even under
        // --strict. Regression test for a false-positive found in review
        // (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[agent]\nstep_limit = 100\n").unwrap();

        let mut args = base_args(path);
        args.config_path = Some(config_path);
        args.strict = true;
        let report = run(&args).await.unwrap();
        assert!(report.ok, "checks: {:?}", report.checks);
        assert!(report.hazards.is_empty(), "hazards: {:?}", report.hazards);
        assert!(
            !report.checks.iter().any(|c| c.check.starts_with("hazard:")),
            "checks: {:?}",
            report.checks
        );
    }

    #[tokio::test]
    async fn explicit_detect_stagnation_flag_surfaces_invalid_config_bounds() {
        // The config file disables stagnation detection, which masks an
        // otherwise-invalid `stagnation_window < stagnation_repeat_threshold`.
        // An explicit `--detect-stagnation` on the live `agent suite`
        // invocation re-enables it, and `DefaultAgent` would reject the
        // task before running. --check must forward the flag and surface
        // this instead of reporting PASS — regression test for a gap found
        // in review (issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[agent]\ndetect_stagnation = false\nstagnation_repeat_threshold = 5\nstagnation_window = 1\n",
        )
        .unwrap();

        let mut args = base_args(path);
        args.config_path = Some(config_path);
        args.detect_stagnation_flag = Some(true);
        let report = run(&args).await.unwrap();
        assert!(!report.ok, "checks: {:?}", report.checks);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "config_resolve" && c.status == CheckStatus::Fail)
        );
    }

    #[tokio::test]
    async fn invalid_stagnation_bounds_do_not_block_when_detection_stays_disabled() {
        // Same broken bounds as above, but nothing re-enables detection
        // (neither the config file nor a --detect-stagnation flag), so the
        // live run never hits the bounds check either — --check must not
        // false-fail here.
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[agent]\ndetect_stagnation = false\nstagnation_repeat_threshold = 5\nstagnation_window = 1\n",
        )
        .unwrap();

        let mut args = base_args(path);
        args.config_path = Some(config_path);
        let report = run(&args).await.unwrap();
        assert!(report.ok, "checks: {:?}", report.checks);
    }

    // ── RED: worst-case cost summary ────────────────────────────────────

    #[tokio::test]
    async fn worst_case_cost_computed_arithmetically() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: a\n- id: t2\n  task: b\n- id: t3\n  task: c\n",
        );
        let mut args = base_args(path);
        args.per_task_budget_usd = Some(0.10);
        let report = run(&args).await.unwrap();
        assert!((report.estimated_worst_case_cost_usd.unwrap() - 0.30).abs() < 1e-9);
    }

    #[tokio::test]
    async fn worst_case_cost_none_when_no_per_task_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: a\n");
        let report = run(&base_args(path)).await.unwrap();
        assert!(report.estimated_worst_case_cost_usd.is_none());
    }

    #[tokio::test]
    async fn worst_case_cost_uses_config_file_budget_when_no_cli_flag() {
        // The live `agent suite` run path honors a config-file
        // `[agent] per_task_budget_usd` per task when `--per-task-budget-usd`
        // isn't passed on the CLI — `--check` must estimate cost the same
        // way instead of reporting "unknown" (regression test for a gap
        // found in review, issue #821).
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: a\n- id: t2\n  task: b\n",
        );
        let mut config = Config::defaults().unwrap();
        config.root.agent.per_task_budget_usd = Some(0.25);
        let mut args = base_args(path);
        args.config = config;
        let report = run(&args).await.unwrap();
        assert!(
            (report.estimated_worst_case_cost_usd.unwrap() - 0.50).abs() < 1e-9,
            "report: {report:?}"
        );
    }

    #[tokio::test]
    async fn worst_case_cost_cli_flag_wins_over_config_file_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: a\n");
        let mut config = Config::defaults().unwrap();
        config.root.agent.per_task_budget_usd = Some(0.25);
        let mut args = base_args(path);
        args.config = config;
        args.per_task_budget_usd = Some(1.0);
        let report = run(&args).await.unwrap();
        assert!(
            (report.estimated_worst_case_cost_usd.unwrap() - 1.0).abs() < 1e-9,
            "report: {report:?}"
        );
    }

    #[tokio::test]
    async fn worst_case_cost_over_limit_is_warning_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: t1\n  task: a\n- id: t2\n  task: b\n",
        );
        let mut args = base_args(path);
        args.per_task_budget_usd = Some(1.0);
        args.suite_cost_limit_usd = Some(0.5);
        let report = run(&args).await.unwrap();
        assert!(report.ok, "cost ceiling must be a warning, not fatal");
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.check == "cost_ceiling" && c.status == CheckStatus::Warn)
        );
    }

    // ── RED: performance (success metric: <1s for 50 tasks, no MCP) ────────

    #[tokio::test]
    async fn fifty_task_pack_with_no_mcp_completes_under_one_second() {
        use std::fmt::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for i in 0..50 {
            let _ = writeln!(content, "- id: t{i}\n  task: do thing {i}");
        }
        let path = write_tasks(&dir, "tasks.yaml", &content);
        let start = std::time::Instant::now();
        let report = run(&base_args(path)).await.unwrap();
        assert!(report.ok);
        assert_eq!(report.task_count, 50);
        assert!(
            start.elapsed().as_secs_f64() < 1.0,
            "preflight took {:?}, expected < 1s",
            start.elapsed()
        );
    }

    // ── RED: text renderer ───────────────────────────────────────────────

    #[tokio::test]
    async fn render_text_shows_pass_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: a\n");
        let report = run(&base_args(path)).await.unwrap();
        let text = render_text(&report);
        assert!(text.contains("PASS"));
        assert!(text.contains("1 task(s)"));
    }

    #[tokio::test]
    async fn render_text_lists_failures_with_check_and_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tasks(
            &dir,
            "tasks.yaml",
            "- id: dup\n  task: a\n- id: dup\n  task: b\n",
        );
        let report = run(&base_args(path)).await.unwrap();
        let text = render_text(&report);
        assert!(text.contains("FAIL"));
        assert!(text.contains("dup"));
    }

    // ── extract_program_token / launchability unit tests ────────────────

    #[test]
    fn extract_program_token_skips_env_assignments() {
        assert_eq!(
            extract_program_token("FOO=bar pytest -q"),
            Some("pytest".to_owned())
        );
    }

    #[test]
    fn extract_program_token_handles_simple_command() {
        assert_eq!(
            extract_program_token("cargo test"),
            Some("cargo".to_owned())
        );
    }

    #[test]
    fn extract_program_token_none_for_empty() {
        assert_eq!(extract_program_token(""), None);
        assert_eq!(extract_program_token("   "), None);
    }

    #[test]
    fn has_leading_path_assignment_detects_path_override() {
        assert!(has_leading_path_assignment("PATH=./venv/bin:$PATH pytest"));
        assert!(has_leading_path_assignment("PATH=/custom/bin pytest -q"));
    }

    #[test]
    fn has_leading_path_assignment_ignores_other_assignments() {
        assert!(!has_leading_path_assignment("FOO=bar pytest -q"));
        assert!(!has_leading_path_assignment("cargo test"));
        // A non-PATH assignment before a PATH assignment doesn't matter for
        // this check — only that the command as a whole doesn't set PATH.
        assert!(has_leading_path_assignment("FOO=bar PATH=/x pytest"));
    }

    #[test]
    fn shell_builtin_true_is_launchable() {
        assert!(check_command_launchable("true").is_ok());
    }

    #[test]
    fn null_command_builtin_is_launchable() {
        // `:` is the shell no-op builtin, commonly used as a dummy
        // always-pass verify command (`verify: ":"`).
        assert!(check_command_launchable(":").is_ok());
    }

    #[test]
    fn unknown_binary_is_not_launchable() {
        assert!(check_command_launchable("__definitely_not_a_real_binary_xyz__").is_err());
    }

    #[test]
    fn path_resolvable_binary_is_launchable() {
        // `cargo` must exist on PATH for the test harness itself to have run.
        assert!(check_command_launchable("cargo --version").is_ok());
    }
}
