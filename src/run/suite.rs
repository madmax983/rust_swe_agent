//! `agent suite` — operator-defined personal eval task pack runner (issue #322).
//!
//! Accepts YAML, TOML, or JSONL task-pack files, runs each task through the
//! existing `mini` code path, and writes both per-task trajectories and an
//! aggregated `suite-results.json` artifact. Redaction, resume, and
//! suite-level cost cap are all honoured.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactKind;
use crate::error::Error;
use crate::exit_code::ExitCode;
use crate::redaction::Redactor;
use crate::trajectory::{
    FailureCategory, TestInvocation, Trajectory, VerificationCheck, VerificationResult,
};

// ── Task file schema ──────────────────────────────────────────────────────────

/// A single task entry in the operator-defined task pack.
#[derive(Debug, Clone, Deserialize)]
pub struct SuiteTaskSpec {
    /// Unique identifier for this task within the suite.
    pub id: String,
    /// Natural-language task description passed to the agent.
    pub task: String,
    /// Optional extra context appended to the task prompt.
    pub extra_context: Option<String>,
    /// Per-task verify checks in `NAME:COMMAND` format. Merged with
    /// suite-level `--verify` entries.
    #[serde(default)]
    pub verify: Vec<String>,
}

/// Supported task-file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFileFormat {
    Yaml,
    Jsonl,
    Toml,
}

impl TaskFileFormat {
    /// Detect format from file extension. Returns `None` when unrecognised.
    pub fn from_extension(path: &Path) -> Option<Self> {
        match path.extension().and_then(|s| s.to_str()) {
            Some("yaml" | "yml") => Some(Self::Yaml),
            Some("jsonl" | "ndjson") => Some(Self::Jsonl),
            Some("toml") => Some(Self::Toml),
            _ => None,
        }
    }

    /// Parse from the CLI `--format` string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "yaml" | "yml" => Some(Self::Yaml),
            "jsonl" | "ndjson" => Some(Self::Jsonl),
            "toml" => Some(Self::Toml),
            _ => None,
        }
    }
}

/// Parse a task-pack file into an ordered list of task specs.
///
/// JSONL: one JSON object per line.
/// YAML: a YAML sequence of task objects.
/// TOML: `[[tasks]]` array of tables.
pub fn parse_task_file(content: &str, format: TaskFileFormat) -> Result<Vec<SuiteTaskSpec>, Error> {
    match format {
        TaskFileFormat::Jsonl => {
            let mut tasks = Vec::new();
            for (i, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let spec: SuiteTaskSpec = serde_json::from_str(trimmed).map_err(|e| {
                    Error::Config(crate::error::ConfigError::Invalid(format!(
                        "task file JSONL line {}: {e}",
                        i + 1
                    )))
                })?;
                tasks.push(spec);
            }
            Ok(tasks)
        }
        TaskFileFormat::Yaml => {
            let specs: Vec<SuiteTaskSpec> = serde_yml::from_str(content).map_err(|e| {
                Error::Config(crate::error::ConfigError::Invalid(format!(
                    "task file YAML parse error: {e}"
                )))
            })?;
            Ok(specs)
        }
        TaskFileFormat::Toml => {
            #[derive(Deserialize)]
            struct TomlWrapper {
                tasks: Vec<SuiteTaskSpec>,
            }
            let wrapper: TomlWrapper = toml::from_str(content).map_err(|e| {
                Error::Config(crate::error::ConfigError::Invalid(format!(
                    "task file TOML parse error: {e}"
                )))
            })?;
            Ok(wrapper.tasks)
        }
    }
}

// ── Suite result schema ───────────────────────────────────────────────────────

/// Per-task result recorded in `suite-results.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteTaskResult {
    /// Task identifier from the pack.
    pub id: String,
    /// Terminal outcome: `"submitted"` | `"step_limit_reached"` | `"error"` |
    /// `"budget_exhausted"` | `"skipped_budget_exhausted"`.
    pub outcome: String,
    /// Aggregated verification verdict: `"verified"` | `"unverified"` |
    /// `"verification_failed"`.
    pub verification_status: String,
    /// Agent steps executed (from trajectory).
    pub steps: Option<u32>,
    /// Cost in USD for this task (from trajectory).
    pub cost_usd: Option<f64>,
    /// Wall-clock duration in seconds (from trajectory).
    pub duration_secs: Option<f64>,
    /// Root-cause failure bucket when task did not submit.
    pub failure_category: Option<FailureCategory>,
    /// Path to the per-task trajectory file.
    pub trajectory_path: String,
    // ── Loop behaviour fields (issue #322 feedback) ────────────────────────
    /// Total number of agent steps / loop iterations executed.
    pub attempt_count: u32,
    /// Number of consecutive test-loop iterations that produced the same
    /// non-zero exit code as the immediately preceding run — signals that the
    /// agent is retrying without making progress.
    pub unchanged_failure_count: u32,
    /// Net verifier score: (checks passed) − (checks failed). `None` when no
    /// verify checks were configured for this task.
    pub verifier_delta: Option<i32>,
    /// Why the agent loop stopped (e.g. `"submitted"`, `"step_limit"`,
    /// `"agent_stagnation"`, `"budget_exhausted"`, `"wallclock_timeout"`).
    pub stop_reason: Option<String>,
}

/// Top-level `suite-results.json` artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteResults {
    pub suite_name: String,
    pub task_count: usize,
    pub resolved_count: usize,
    pub verified_count: usize,
    pub total_cost_usd: f64,
    pub total_duration_secs: f64,
    pub started_at: String,
    pub finished_at: String,
    pub tasks: Vec<SuiteTaskResult>,
}

impl SuiteResults {
    /// Render a human-readable summary table: one row per task + summary line.
    pub fn summary_table(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            &mut out,
            "{:<30} {:<22} {:<12} {:>8} {:>7}",
            "ID", "OUTCOME", "VERIFY", "COST($)", "STEPS"
        );
        let _ = writeln!(&mut out, "{}", "-".repeat(84));
        for t in &self.tasks {
            let cost_str = t
                .cost_usd
                .map_or_else(|| "-".to_owned(), |c| format!("{c:.4}"));
            let steps_str = t.steps.map_or_else(|| "-".to_owned(), |s| s.to_string());
            let _ = writeln!(
                &mut out,
                "{:<30} {:<22} {:<12} {:>8} {:>7}",
                truncate_id(&t.id, 30),
                truncate_id(&t.outcome, 22),
                truncate_id(&t.verification_status, 12),
                cost_str,
                steps_str,
            );
        }
        let _ = writeln!(&mut out, "{}", "-".repeat(84));
        let _ = writeln!(
            &mut out,
            "{}/{} resolved, {}/{} verified, ${:.4}, {:.1}s elapsed",
            self.resolved_count,
            self.task_count,
            self.verified_count,
            self.task_count,
            self.total_cost_usd,
            self.total_duration_secs,
        );
        out
    }
}

fn truncate_id(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

// ── Exit-code severity ────────────────────────────────────────────────────────

/// Severity ordering for multi-task exit codes.
/// Higher severity replaces lower severity as the suite accumulates results.
fn exit_code_severity(code: ExitCode) -> u8 {
    match code {
        ExitCode::Interrupted | ExitCode::Killed => 5,
        ExitCode::VerificationFailure => 4,
        ExitCode::BudgetHalt => 3,
        ExitCode::TaskUnsuccessful | ExitCode::PreflightFailure => 2,
        ExitCode::Success => 0,
        _ => 1,
    }
}

/// Merge two exit codes; the higher-severity code wins.
pub fn merge_exit_code(current: ExitCode, new_code: ExitCode) -> ExitCode {
    if exit_code_severity(new_code) > exit_code_severity(current) {
        new_code
    } else {
        current
    }
}

// ── Loop-behaviour derivation ─────────────────────────────────────────────────

/// Count consecutive tail-end test invocations that share the same non-zero
/// exit code as the last test run — the "same miss, more spend" signal.
///
/// A passing run (exit_code == 0) resets the chain: only failures *after* the
/// last pass are eligible.
fn count_unchanged_failures(invocations: &[TestInvocation]) -> u32 {
    let last_pass_idx = invocations.iter().rposition(|t| t.exit_code == 0);
    let tail = match last_pass_idx {
        Some(i) => &invocations[i + 1..],
        None => invocations,
    };
    if tail.is_empty() {
        return 0;
    }
    let last_exit = tail.last().map_or(0, |t| t.exit_code);
    let n = tail
        .iter()
        .rev()
        .take_while(|t| t.exit_code == last_exit)
        .count();
    // Only signal "stuck" when the same failure repeats 2+ times consecutively.
    if n >= 2 {
        u32::try_from(n).unwrap_or(u32::MAX)
    } else {
        0
    }
}

/// Net verifier score: passed minus failed. `None` when no checks ran.
fn compute_verifier_delta(results: &[VerificationResult]) -> Option<i32> {
    if results.is_empty() {
        return None;
    }
    let passed = i32::try_from(results.iter().filter(|v| v.passed).count()).unwrap_or(i32::MAX);
    let total = i32::try_from(results.len()).unwrap_or(i32::MAX);
    let failed = total - passed;
    Some(passed - failed)
}

/// Derive a stable stop-reason string from the trajectory.
fn derive_stop_reason(traj: &Trajectory) -> Option<String> {
    if let Some(fc) = traj.info.failure_category {
        // Use the serde snake_case label for the failure category.
        let val = serde_json::to_value(fc).ok()?;
        return val.as_str().map(str::to_owned);
    }
    traj.info.outcome.clone()
}

/// Derive a `SuiteTaskResult` from a completed trajectory.
fn task_result_from_trajectory(id: &str, traj: &Trajectory, traj_path: &Path) -> SuiteTaskResult {
    let outcome = traj
        .info
        .outcome
        .clone()
        .unwrap_or_else(|| "error".to_owned());
    let verification_status = traj
        .info
        .verification_status
        .clone()
        .unwrap_or_else(|| crate::trajectory::verification_status::UNVERIFIED.to_owned());

    SuiteTaskResult {
        id: id.to_owned(),
        outcome,
        verification_status,
        steps: traj.info.steps,
        cost_usd: traj.info.total_cost_usd,
        duration_secs: traj.info.duration_secs,
        failure_category: traj.info.failure_category,
        trajectory_path: traj_path.display().to_string(),
        attempt_count: traj.info.steps.unwrap_or(0),
        unchanged_failure_count: count_unchanged_failures(&traj.info.test_invocations),
        verifier_delta: compute_verifier_delta(&traj.info.verification_results),
        stop_reason: derive_stop_reason(traj),
    }
}

// ── Suite runner ──────────────────────────────────────────────────────────────

/// Arguments for `agent suite`.
pub struct SuiteArgs {
    pub tasks_file: PathBuf,
    pub format_override: Option<String>,
    pub suite_name: String,
    pub config: crate::config::Config,
    pub output_dir: PathBuf,
    pub suite_cost_limit_usd: Option<f64>,
    pub verify: Vec<String>,
    pub verify_timeout_secs: u64,
    pub resume: bool,
    pub task_timeout_secs: Option<u64>,
    pub step_limit: Option<u32>,
    pub per_task_budget_usd: Option<f64>,
}

/// Run the suite and return the final exit code.
#[allow(clippy::too_many_lines)]
pub async fn run(args: SuiteArgs) -> Result<ExitCode, Error> {
    // ── Resolve task file format ──────────────────────────────────────────
    let format = if let Some(ref s) = args.format_override {
        TaskFileFormat::parse(s).ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "--format '{s}' is not valid; use yaml, jsonl, or toml"
            )))
        })?
    } else {
        TaskFileFormat::from_extension(&args.tasks_file).ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "cannot detect format from '{}'; use --format yaml|jsonl|toml",
                args.tasks_file.display()
            )))
        })?
    };

    // ── Read and parse task file ──────────────────────────────────────────
    let content = std::fs::read_to_string(&args.tasks_file).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "cannot read tasks file '{}': {e}",
            args.tasks_file.display()
        )))
    })?;
    let tasks = parse_task_file(&content, format)?;
    if tasks.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "task pack contains no tasks".into(),
        )));
    }

    // ── Validate task IDs ─────────────────────────────────────────────────
    {
        let mut seen = std::collections::HashSet::new();
        for task in &tasks {
            if task.id.is_empty() {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "task id must not be empty".into(),
                )));
            }
            if task.task.trim().is_empty() {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "task '{}': task description must not be empty",
                    task.id
                ))));
            }
            if task.id.contains('/') || task.id.contains('\\') || task.id.contains("..") {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "task id '{}' must not contain path separators or '..'",
                    task.id
                ))));
            }
            if !seen.insert(task.id.as_str()) {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "duplicate task id '{}'",
                    task.id
                ))));
            }
        }
    }

    // ── Prepare output directory ──────────────────────────────────────────
    let suite_dir = args.output_dir.join(&args.suite_name);
    if args.suite_name.contains('/')
        || args.suite_name.contains('\\')
        || args.suite_name.contains("..")
    {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "suite name '{}' must not contain path separators or '..'",
            args.suite_name
        ))));
    }
    std::fs::create_dir_all(&suite_dir).map_err(Error::Io)?;

    // ── Parse suite-level verify checks ──────────────────────────────────
    let suite_verify = parse_verify_checks(&args.verify)?;

    // ── Pre-validate all per-task verify entries ──────────────────────────
    for task in &tasks {
        parse_verify_checks(&task.verify).map_err(|e| match e {
            Error::Config(crate::error::ConfigError::Invalid(msg)) => Error::Config(
                crate::error::ConfigError::Invalid(format!("task '{}': {msg}", task.id)),
            ),
            other => other,
        })?;
    }

    let redactor = crate::redaction::Redactor::from_config_lossy(&args.config.root.redaction);

    let started_at = Utc::now().to_rfc3339();
    let wall_start = std::time::Instant::now();

    let task_count = tasks.len();
    let mut task_results: Vec<SuiteTaskResult> = Vec::with_capacity(task_count);
    let mut total_cost = 0.0_f64;
    let mut resolved_count = 0_usize;
    let mut verified_count = 0_usize;
    let mut suite_exit = ExitCode::Success;
    let mut cumulative_cost = 0.0_f64;
    let mut early_halt_reason: Option<&'static str> = None;

    for task in &tasks {
        let traj_path = suite_dir.join(format!("{}.traj.json", task.id));

        // ── Resume: skip tasks with terminal outcomes ─────────────────────
        if args.resume && traj_path.exists() {
            if let Some(existing) = try_load_terminal_trajectory(&traj_path) {
                let result = task_result_from_trajectory(&task.id, &existing, &traj_path);
                let is_resolved = result.outcome == crate::trajectory::outcome::SUBMITTED;
                let is_verified =
                    result.verification_status == crate::trajectory::verification_status::VERIFIED;
                total_cost += result.cost_usd.unwrap_or(0.0);
                cumulative_cost += result.cost_usd.unwrap_or(0.0);
                if is_resolved {
                    resolved_count += 1;
                }
                if is_verified {
                    verified_count += 1;
                }
                // Resumed tasks must still contribute to suite exit code.
                let task_exit = classify_task_exit(&result, &Ok(()), false);
                suite_exit = merge_exit_code(suite_exit, task_exit);
                // Also check if resumed cost pushed us over the suite cap.
                if let Some(limit) = args.suite_cost_limit_usd {
                    if cumulative_cost >= limit {
                        suite_exit = merge_exit_code(suite_exit, ExitCode::BudgetHalt);
                    }
                }
                task_results.push(result);
                continue;
            }
        }

        // ── Suite cost-cap: skip remaining tasks ──────────────────────────
        if let Some(limit) = args.suite_cost_limit_usd {
            if cumulative_cost >= limit {
                task_results.push(SuiteTaskResult {
                    id: task.id.clone(),
                    outcome: "skipped_budget_exhausted".to_owned(),
                    verification_status: crate::trajectory::verification_status::UNVERIFIED
                        .to_owned(),
                    steps: None,
                    cost_usd: None,
                    duration_secs: None,
                    failure_category: None,
                    trajectory_path: traj_path.display().to_string(),
                    attempt_count: 0,
                    unchanged_failure_count: 0,
                    verifier_delta: None,
                    stop_reason: Some("suite_budget_exhausted".to_owned()),
                });
                suite_exit = merge_exit_code(suite_exit, ExitCode::BudgetHalt);
                continue;
            }
        }

        // ── Merge per-task verify with suite-level verify ─────────────────
        let mut task_verify_checks = suite_verify.clone();
        let per_task_checks = parse_verify_checks(&task.verify)?;
        task_verify_checks.extend(per_task_checks);

        // ── Build MiniArgs and run the task ───────────────────────────────
        let mut task_cfg = args.config.clone();
        if let Some(v) = args.step_limit {
            task_cfg.root.agent.step_limit = v;
        }
        if let Some(v) = args.per_task_budget_usd {
            task_cfg.root.agent.per_task_budget_usd = Some(v);
        }

        let trajectory_name = task.id.clone();
        let mini_args = crate::run::mini::MiniArgs {
            task: task.task.clone(),
            extra_context: task.extra_context.clone(),
            config: task_cfg,
            driver: crate::run::mini::RunDriver::Builtin,
            output_dir: suite_dir.clone(),
            trajectory_name: trajectory_name.clone(),
            deterministic_responses: None,
            deterministic_usage_per_call: None,
            task_timeout_secs: args.task_timeout_secs,
            cancellation: None,
            stream_addr: None,
            patch_capture: None,
            verification_checks: task_verify_checks,
            verification_timeout_secs: args.verify_timeout_secs,
            resume_from: None,
            interactive_mode: crate::run::mini::InteractiveMode::Off,
            trace_id: None,
            webhook_url: None,
            webhook_headers: vec![],
            event_log: None,
            event_log_instance_id: None,
            local_workdir: std::env::current_dir().ok(),
            read_only: false,
            allow_mcp_in_read_only: false,
            rehearsal_gold_patch: None,
            no_step_persist: false,
            parent_sweep_run_id: None,
            continue_from: None,
        };

        let run_outcome = crate::run::mini::run(mini_args).await;
        let is_verification_error = matches!(
            run_outcome,
            Err(crate::error::Error::VerificationFailed(..))
        );

        // ── Load trajectory from disk ─────────────────────────────────────
        let result = if let Some(traj) = try_load_terminal_trajectory(&traj_path) {
            let mut r = task_result_from_trajectory(&task.id, &traj, &traj_path);
            r.trajectory_path = redactor
                .redact_text(&r.trajectory_path, crate::redaction::surface::EXPORT)
                .text;
            r
        } else {
            // Mini errored before writing a trajectory (env/preflight failure).
            let (failure_category, stop_reason) = match &run_outcome {
                Err(e) => {
                    let code = ExitCode::from_error(e);
                    let cat = match code {
                        ExitCode::PreflightFailure => FailureCategory::EnvSetup,
                        _ => FailureCategory::AgentInternal,
                    };
                    (Some(cat), Some(code.outcome_class().to_owned()))
                }
                Ok(()) => (None, Some("error".to_owned())),
            };
            SuiteTaskResult {
                id: task.id.clone(),
                outcome: "error".to_owned(),
                verification_status: crate::trajectory::verification_status::UNVERIFIED.to_owned(),
                steps: None,
                cost_usd: None,
                duration_secs: None,
                failure_category,
                trajectory_path: traj_path.display().to_string(),
                attempt_count: 0,
                unchanged_failure_count: 0,
                verifier_delta: None,
                stop_reason,
            }
        };

        // ── Accumulate totals ─────────────────────────────────────────────
        let task_cost = result.cost_usd.unwrap_or(0.0);
        total_cost += task_cost;
        cumulative_cost += task_cost;

        if result.outcome == crate::trajectory::outcome::SUBMITTED {
            resolved_count += 1;
        }
        if result.verification_status == crate::trajectory::verification_status::VERIFIED {
            verified_count += 1;
        }

        // ── Post-task budget check ────────────────────────────────────────
        if let Some(limit) = args.suite_cost_limit_usd {
            if cumulative_cost >= limit {
                suite_exit = merge_exit_code(suite_exit, ExitCode::BudgetHalt);
            }
        }

        // ── Determine per-task exit code contribution ─────────────────────
        let task_exit = classify_task_exit(&result, &run_outcome, is_verification_error);
        suite_exit = merge_exit_code(suite_exit, task_exit);

        // Remember whether mini failed before writing any trajectory — used to
        // distinguish suite-level setup errors from ordinary task failures.
        let is_pre_task_error = result.outcome == "error";
        task_results.push(result);

        // ── Propagate hard errors (env/preflight) that should stop the suite
        match &run_outcome {
            Err(e) if !is_verification_error => {
                let code = ExitCode::from_error(e);
                // Only halt on InternalError when no trajectory was written; if
                // mini returned InternalError *after* writing a trajectory (e.g.
                // a wallclock-timeout wrapper), it is a task-scoped failure and
                // should not abort the remaining suite tasks.
                if matches!(code, ExitCode::PreflightFailure | ExitCode::UsageError)
                    || (is_pre_task_error && matches!(code, ExitCode::InternalError))
                {
                    early_halt_reason = Some("suite_preflight_halt");
                    break;
                } else if matches!(code, ExitCode::Interrupted | ExitCode::Killed) {
                    early_halt_reason = Some("suite_interrupted");
                    break;
                }
            }
            _ => {}
        }
    }

    // ── Pad skipped tasks not yet reached due to early halt ───────────────
    let halt_reason = early_halt_reason.unwrap_or("suite_halted");
    let halt_outcome = if halt_reason == "suite_preflight_halt" {
        "skipped_preflight_halt"
    } else {
        "skipped_interrupted"
    };
    let reached = task_results.len();
    for task in tasks.iter().skip(reached) {
        let traj_path = suite_dir.join(format!("{}.traj.json", task.id));
        task_results.push(SuiteTaskResult {
            id: task.id.clone(),
            outcome: halt_outcome.to_owned(),
            verification_status: crate::trajectory::verification_status::UNVERIFIED.to_owned(),
            steps: None,
            cost_usd: None,
            duration_secs: None,
            failure_category: None,
            trajectory_path: traj_path.display().to_string(),
            attempt_count: 0,
            unchanged_failure_count: 0,
            verifier_delta: None,
            stop_reason: Some(halt_reason.to_owned()),
        });
    }

    let total_duration = wall_start.elapsed().as_secs_f64();
    let finished_at = Utc::now().to_rfc3339();

    let suite_results = SuiteResults {
        suite_name: args.suite_name.clone(),
        task_count,
        resolved_count,
        verified_count,
        total_cost_usd: total_cost,
        total_duration_secs: total_duration,
        started_at,
        finished_at,
        tasks: task_results,
    };

    // ── Write suite-results.json ──────────────────────────────────────────
    let results_path = suite_dir.join("suite-results.json");
    write_suite_results(&suite_results, &results_path, &redactor)?;

    // ── Print summary table ───────────────────────────────────────────────
    print!("{}", suite_results.summary_table());

    Ok(suite_exit)
}

/// Classify the exit-code contribution for one task result.
fn classify_task_exit(
    result: &SuiteTaskResult,
    run_outcome: &Result<(), Error>,
    is_verification_error: bool,
) -> ExitCode {
    if result.outcome == "skipped_budget_exhausted" {
        return ExitCode::BudgetHalt;
    }
    if is_verification_error
        || result.verification_status == crate::trajectory::verification_status::VERIFICATION_FAILED
    {
        return ExitCode::VerificationFailure;
    }
    if result.outcome == crate::trajectory::outcome::SUBMITTED {
        // If mini returned a non-verification error even after writing a submitted
        // trajectory (e.g. post-save I/O failure), propagate that exit code.
        return match run_outcome {
            Ok(()) => ExitCode::Success,
            Err(e) => ExitCode::from_error(e),
        };
    }
    match run_outcome {
        Ok(()) => ExitCode::TaskUnsuccessful,
        Err(e) => {
            let code = ExitCode::from_error(e);
            // Agent-loop terminal conditions (stagnation) and Trajectory-wrapper
            // errors (e.g. wallclock timeout maps to InternalError) are
            // task-level failures in the suite exit matrix (only 0/4/5/7 are
            // documented). Remap them when a trajectory was written.
            if result.outcome != "error"
                && matches!(code, ExitCode::AgentStagnation | ExitCode::InternalError)
            {
                ExitCode::TaskUnsuccessful
            } else {
                code
            }
        }
    }
}

/// Load a trajectory from disk only if it has a terminal outcome.
fn try_load_terminal_trajectory(path: &Path) -> Option<Trajectory> {
    let text = std::fs::read_to_string(path).ok()?;
    let traj: Trajectory = serde_json::from_str(&text).ok()?;
    // Non-terminal (partial) trajectories should be re-run.
    if traj.info.partial {
        return None;
    }
    traj.info.outcome.as_ref()?;
    Some(traj)
}

/// Serialize `suite-results.json` with artifact header, applying redaction.
fn write_suite_results(
    results: &SuiteResults,
    path: &Path,
    redactor: &Redactor,
) -> Result<(), Error> {
    let mut json_val = serde_json::to_value(results).map_err(Error::Json)?;
    redact_json_strings(&mut json_val, redactor);
    let json = crate::artifact::to_string_pretty(ArtifactKind::SuiteResults, &json_val)
        .map_err(Error::Json)?;
    atomic_write(path, json.as_bytes())?;
    Ok(())
}

/// Parse `NAME:COMMAND` verify check specs.
fn parse_verify_checks(specs: &[String]) -> Result<Vec<VerificationCheck>, Error> {
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
            Ok(VerificationCheck {
                name: name.to_owned(),
                command: command.to_owned(),
            })
        })
        .collect()
}

/// Recursively redact string values in a JSON tree.
fn redact_json_strings(v: &mut serde_json::Value, redactor: &Redactor) {
    match v {
        serde_json::Value::String(s) => {
            *s = redactor
                .redact_text(s, crate::redaction::surface::EXPORT)
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

/// Atomically write bytes to `path` via a sibling temp file.
fn atomic_write(path: &Path, data: &[u8]) -> Result<(), Error> {
    use std::io::Write as _;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(Error::Io)?;
    tmp.write_all(data).map_err(Error::Io)?;
    tmp.flush().map_err(Error::Io)?;
    tmp.persist(path).map_err(|e| Error::Io(e.error))?;
    Ok(())
}

// ── Tests (RED → GREEN) ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // ── RED: parse_task_file ───────────────────────────────────────────────

    #[test]
    fn parse_jsonl_task_file_parses_tasks() {
        let input = r#"{"id":"t1","task":"fix the bug"}
{"id":"t2","task":"add a feature","extra_context":"hints"}
{"id":"t3","task":"write tests","verify":["lint:cargo clippy"]}
"#;
        let tasks = parse_task_file(input, TaskFileFormat::Jsonl).unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].id, "t1");
        assert_eq!(tasks[0].task, "fix the bug");
        assert!(tasks[0].extra_context.is_none());
        assert!(tasks[0].verify.is_empty());
        assert_eq!(tasks[1].extra_context.as_deref(), Some("hints"));
        assert_eq!(tasks[2].verify, ["lint:cargo clippy"]);
    }

    #[test]
    fn parse_yaml_task_file_parses_tasks() {
        let input = "- id: t1\n  task: fix the bug\n- id: t2\n  task: add tests\n  extra_context: some hints\n";
        let tasks = parse_task_file(input, TaskFileFormat::Yaml).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "t1");
        assert_eq!(tasks[1].extra_context.as_deref(), Some("some hints"));
    }

    #[test]
    fn parse_toml_task_file_parses_tasks() {
        let input = "[[tasks]]\nid = \"t1\"\ntask = \"fix the bug\"\n\n[[tasks]]\nid = \"t2\"\ntask = \"write docs\"\n";
        let tasks = parse_task_file(input, TaskFileFormat::Toml).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "t1");
        assert_eq!(tasks[1].task, "write docs");
    }

    #[test]
    fn jsonl_skips_blank_lines() {
        let input = "\n{\"id\":\"t1\",\"task\":\"go\"}\n\n";
        let tasks = parse_task_file(input, TaskFileFormat::Jsonl).unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn parse_bad_jsonl_returns_error() {
        let input = "not json\n";
        assert!(parse_task_file(input, TaskFileFormat::Jsonl).is_err());
    }

    // ── RED: TaskFileFormat detection ─────────────────────────────────────

    #[test]
    fn format_detection_from_extension() {
        assert_eq!(
            TaskFileFormat::from_extension(Path::new("tasks.yaml")),
            Some(TaskFileFormat::Yaml)
        );
        assert_eq!(
            TaskFileFormat::from_extension(Path::new("tasks.yml")),
            Some(TaskFileFormat::Yaml)
        );
        assert_eq!(
            TaskFileFormat::from_extension(Path::new("tasks.jsonl")),
            Some(TaskFileFormat::Jsonl)
        );
        assert_eq!(
            TaskFileFormat::from_extension(Path::new("tasks.toml")),
            Some(TaskFileFormat::Toml)
        );
        assert_eq!(TaskFileFormat::from_extension(Path::new("tasks.txt")), None);
    }

    #[test]
    fn format_detection_from_str() {
        assert_eq!(TaskFileFormat::parse("yaml"), Some(TaskFileFormat::Yaml));
        assert_eq!(TaskFileFormat::parse("jsonl"), Some(TaskFileFormat::Jsonl));
        assert_eq!(TaskFileFormat::parse("toml"), Some(TaskFileFormat::Toml));
        assert_eq!(TaskFileFormat::parse("csv"), None);
    }

    // ── RED: loop behaviour fields ────────────────────────────────────────

    #[test]
    fn unchanged_failure_count_zero_when_no_failures() {
        let invocations = vec![make_invocation(0, 0), make_invocation(1, 0)];
        assert_eq!(count_unchanged_failures(&invocations), 0);
    }

    #[test]
    fn unchanged_failure_count_counts_consecutive_tail_failures() {
        let invocations = vec![
            make_invocation(0, 1),
            make_invocation(1, 0),
            make_invocation(2, 1),
            make_invocation(3, 1),
        ];
        // Last two are the same exit code (1), previous was 0.
        assert_eq!(count_unchanged_failures(&invocations), 2);
    }

    #[test]
    fn unchanged_failure_count_breaks_on_different_exit_code() {
        let invocations = vec![
            make_invocation(0, 1),
            make_invocation(1, 2),
            make_invocation(2, 1),
        ];
        // Last is 1, but the one before is 2 — chain breaks.
        assert_eq!(count_unchanged_failures(&invocations), 0);
    }

    #[test]
    fn unchanged_failure_count_single_tail_failure_is_zero() {
        let invocations = vec![
            make_invocation(0, 0), // pass
            make_invocation(1, 1), // single fail — not a "stuck" streak
        ];
        assert_eq!(count_unchanged_failures(&invocations), 0);
    }

    #[test]
    fn verifier_delta_none_when_empty() {
        assert_eq!(compute_verifier_delta(&[]), None);
    }

    #[test]
    fn verifier_delta_positive_when_all_pass() {
        let results = vec![make_verification(true), make_verification(true)];
        assert_eq!(compute_verifier_delta(&results), Some(2)); // 2 passed - 0 failed = 2
    }

    #[test]
    fn verifier_delta_zero_when_half_pass() {
        let results = vec![make_verification(true), make_verification(false)];
        assert_eq!(compute_verifier_delta(&results), Some(0)); // 1 - 1 = 0
    }

    #[test]
    fn verifier_delta_negative_when_all_fail() {
        let results = vec![make_verification(false), make_verification(false)];
        assert_eq!(compute_verifier_delta(&results), Some(-2)); // 0 - 2 = -2
    }

    // ── RED: exit-code severity ordering ─────────────────────────────────

    #[test]
    fn merge_exit_code_highest_severity_wins() {
        assert_eq!(
            merge_exit_code(ExitCode::Success, ExitCode::TaskUnsuccessful),
            ExitCode::TaskUnsuccessful
        );
        assert_eq!(
            merge_exit_code(ExitCode::TaskUnsuccessful, ExitCode::VerificationFailure),
            ExitCode::VerificationFailure
        );
        assert_eq!(
            merge_exit_code(ExitCode::VerificationFailure, ExitCode::BudgetHalt),
            ExitCode::VerificationFailure
        );
        assert_eq!(
            merge_exit_code(ExitCode::BudgetHalt, ExitCode::TaskUnsuccessful),
            ExitCode::BudgetHalt
        );
    }

    #[test]
    fn merge_exit_code_idempotent_when_same() {
        assert_eq!(
            merge_exit_code(ExitCode::TaskUnsuccessful, ExitCode::TaskUnsuccessful),
            ExitCode::TaskUnsuccessful
        );
    }

    // ── RED: summary table format ─────────────────────────────────────────

    #[test]
    fn summary_table_contains_task_id_and_summary_line() {
        let results = make_sample_suite_results();
        let table = results.summary_table();
        assert!(table.contains("task-alpha"), "table must show task id");
        assert!(
            table.contains("1/2 resolved"),
            "table must show resolved count"
        );
        assert!(
            table.contains("1/2 verified"),
            "table must show verified count"
        );
    }

    #[test]
    fn summary_table_has_header_row() {
        let results = make_sample_suite_results();
        let table = results.summary_table();
        assert!(table.contains("ID"), "table must have ID column");
        assert!(table.contains("OUTCOME"), "table must have OUTCOME column");
        assert!(table.contains("VERIFY"), "table must have VERIFY column");
    }

    // ── RED: SuiteResults serialization ──────────────────────────────────

    #[test]
    fn suite_results_serializes_with_artifact_header() {
        let results = make_sample_suite_results();
        let json_val = serde_json::to_value(&results).unwrap();
        // Simulate what write_suite_results does: wrap with artifact header.
        let wrapped = crate::artifact::to_string_pretty(
            crate::artifact::ArtifactKind::SuiteResults,
            &json_val,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(parsed["artifact_kind"], "suite_results");
        assert!(parsed["schema_version"].is_object());
        assert!(parsed["tasks"].is_array());
        let task0 = &parsed["tasks"][0];
        assert!(task0["attempt_count"].is_number());
        assert!(task0["unchanged_failure_count"].is_number());
        assert!(task0["stop_reason"].is_string());
    }

    #[test]
    fn suite_task_result_records_loop_behaviour_fields() {
        let result = SuiteTaskResult {
            id: "t1".into(),
            outcome: "submitted".into(),
            verification_status: "verified".into(),
            steps: Some(7),
            cost_usd: Some(0.01),
            duration_secs: Some(5.0),
            failure_category: None,
            trajectory_path: "runs/s/t1.traj.json".into(),
            attempt_count: 7,
            unchanged_failure_count: 2,
            verifier_delta: Some(1),
            stop_reason: Some("submitted".into()),
        };
        let val = serde_json::to_value(&result).unwrap();
        assert_eq!(val["attempt_count"], 7);
        assert_eq!(val["unchanged_failure_count"], 2);
        assert_eq!(val["verifier_delta"], 1);
        assert_eq!(val["stop_reason"], "submitted");
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    fn make_invocation(step_index: u32, exit_code: i32) -> TestInvocation {
        TestInvocation {
            step_index,
            command: "cargo test".to_owned(),
            exit_code,
            matched_pattern: String::new(),
        }
    }

    fn make_verification(passed: bool) -> VerificationResult {
        VerificationResult {
            name: "check".to_owned(),
            command: "echo ok".to_owned(),
            exit_code: i32::from(!passed),
            duration_ms: 10,
            passed,
            stdout_preview: String::new(),
            stderr_preview: String::new(),
            timed_out: false,
        }
    }

    fn make_sample_suite_results() -> SuiteResults {
        SuiteResults {
            suite_name: "my-suite".into(),
            task_count: 2,
            resolved_count: 1,
            verified_count: 1,
            total_cost_usd: 0.05,
            total_duration_secs: 30.0,
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:00:30Z".into(),
            tasks: vec![
                SuiteTaskResult {
                    id: "task-alpha".into(),
                    outcome: "submitted".into(),
                    verification_status: "verified".into(),
                    steps: Some(5),
                    cost_usd: Some(0.03),
                    duration_secs: Some(15.0),
                    failure_category: None,
                    trajectory_path: "runs/my-suite/task-alpha.traj.json".into(),
                    attempt_count: 5,
                    unchanged_failure_count: 0,
                    verifier_delta: Some(1),
                    stop_reason: Some("submitted".into()),
                },
                SuiteTaskResult {
                    id: "task-beta".into(),
                    outcome: "step_limit_reached".into(),
                    verification_status: "unverified".into(),
                    steps: Some(50),
                    cost_usd: Some(0.02),
                    duration_secs: Some(15.0),
                    failure_category: Some(FailureCategory::StepLimit),
                    trajectory_path: "runs/my-suite/task-beta.traj.json".into(),
                    attempt_count: 50,
                    unchanged_failure_count: 3,
                    verifier_delta: None,
                    stop_reason: Some("step_limit".into()),
                },
            ],
        }
    }
}
