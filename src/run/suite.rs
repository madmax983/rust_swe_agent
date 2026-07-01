//! `agent suite` — operator-defined personal eval task pack runner (issue #322).
//!
//! Accepts YAML, TOML, or JSONL task-pack files, runs each task through the
//! existing `mini` code path, and writes both per-task trajectories and an
//! aggregated `suite-results.json` artifact. Redaction, resume, and
//! suite-level cost cap are all honoured.
//!
//! `--rerun-failed` (issue #825) re-runs only the tasks whose last recorded
//! result was non-passing, carrying every already-passing result forward
//! unchanged (zero model calls, zero fresh cost) into a freshly merged
//! `suite-results.json`. See [`tasks_needing_rerun`] and
//! [`load_prior_suite_state`].

use std::collections::HashMap;
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
    // ── `--rerun-failed` provenance (issue #825) ───────────────────────────
    /// `true` when this row was carried forward unchanged from a prior run
    /// (a `--rerun-failed` invocation found it already passing) rather than
    /// freshly executed in this invocation. Always `false` for a plain
    /// `agent suite` run. Absent on artifacts written before issue #825;
    /// defaults to `false` on read, matching the historical behavior (every
    /// row was freshly run).
    #[serde(default)]
    pub carried_over: bool,
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
        let carried = self.tasks.iter().filter(|t| t.carried_over).count();
        if carried > 0 {
            let _ = writeln!(
                &mut out,
                "{carried}/{} carried forward from a prior run (--rerun-failed), 0 fresh cost",
                self.task_count,
            );
        }
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
        carried_over: false,
    }
}

// ── `--rerun-failed` selection & carry-forward (issue #825) ──────────────────

/// A task result counts as "passing" — and is therefore eligible to be
/// carried forward unchanged by `--rerun-failed` — only when it submitted
/// *and* did not fail verification. Everything else (`error`,
/// `step_limit_reached`, `budget_exhausted`, `skipped_budget_exhausted`, a
/// submitted-but-`verification_failed` row, …) is "failed" for selection
/// purposes.
fn is_passing_result(result: &SuiteTaskResult) -> bool {
    result.outcome == crate::trajectory::outcome::SUBMITTED
        && result.verification_status != crate::trajectory::verification_status::VERIFICATION_FAILED
}

/// Determine which task ids in the current pack need to be (re)run under
/// `--rerun-failed`: every id absent from `prior` (never recorded, e.g. a
/// task newly added to the pack) or present but not [`is_passing_result`].
/// Ids present in `prior` and passing are excluded — the whole point of the
/// mode is to skip work already confirmed to pass.
pub(crate) fn tasks_needing_rerun(
    tasks: &[SuiteTaskSpec],
    prior: &HashMap<String, SuiteTaskResult>,
) -> std::collections::HashSet<String> {
    tasks
        .iter()
        .filter(|t| !prior.get(&t.id).is_some_and(is_passing_result))
        .map(|t| t.id.clone())
        .collect()
}

/// Load the suite's prior state for `--rerun-failed` selection.
///
/// Prefers `suite-results.json` — written at the end of every prior `agent
/// suite` invocation, even one halted early — since it is the authoritative,
/// already-aggregated source of truth. Falls back to reconstructing per-task
/// results from individual `<task-id>.traj.json` files (for the tasks in the
/// current pack) when that artifact is missing entirely, e.g. the previous
/// process was killed before it could write the aggregate.
///
/// Returns `None` when neither source has anything to report for this pack —
/// the caller should treat that as "never run; run without `--rerun-failed`
/// first".
fn load_prior_suite_state(
    suite_dir: &Path,
    tasks: &[SuiteTaskSpec],
) -> Option<HashMap<String, SuiteTaskResult>> {
    let results_path = suite_dir.join("suite-results.json");
    if let Ok(text) = std::fs::read_to_string(&results_path) {
        if let Ok(prior) = serde_json::from_str::<SuiteResults>(&text) {
            return Some(prior.tasks.into_iter().map(|t| (t.id.clone(), t)).collect());
        }
    }

    let mut map = HashMap::new();
    for task in tasks {
        let traj_path = suite_dir.join(format!("{}.traj.json", task.id));
        if let Some(traj) = try_load_terminal_trajectory(&traj_path) {
            map.insert(
                task.id.clone(),
                task_result_from_trajectory(&task.id, &traj, &traj_path),
            );
        }
    }
    if map.is_empty() { None } else { Some(map) }
}

/// Returns the prior passing result for `task_id`, marked `carried_over:
/// true`, when `--rerun-failed` has excluded it from the re-run subset.
/// `None` when the task must run (or re-run) instead — either this isn't a
/// `--rerun-failed` invocation, or the task is in the re-run subset, or (a
/// defensive fallback that should not occur in practice) it has no prior
/// result to carry.
///
/// Used both by the main per-task loop and by the early-halt padding loop so
/// a passing task later in the pack is never demoted to a `skipped_*` row
/// just because an earlier re-run task hit a hard error.
fn try_carry_forward(
    task_id: &str,
    run_ids: Option<&std::collections::HashSet<String>>,
    prior_by_id: &HashMap<String, SuiteTaskResult>,
) -> Option<SuiteTaskResult> {
    let ids = run_ids?;
    if ids.contains(task_id) {
        return None;
    }
    let mut carried = prior_by_id.get(task_id).cloned()?;
    carried.carried_over = true;
    Some(carried)
}

/// Fold a carried-forward result's cost/resolved/verified contribution into
/// the running suite totals. Shared by the main loop and the halt-padding
/// loop so both count carried rows identically.
fn accumulate_carried(
    carried: &SuiteTaskResult,
    total_cost: &mut f64,
    resolved_count: &mut usize,
    verified_count: &mut usize,
) {
    *total_cost += carried.cost_usd.unwrap_or(0.0);
    if carried.outcome == crate::trajectory::outcome::SUBMITTED {
        *resolved_count += 1;
    }
    if carried.verification_status == crate::trajectory::verification_status::VERIFIED {
        *verified_count += 1;
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
    /// Re-run only tasks whose last recorded result was non-passing (issue
    /// #825). Mutually exclusive with `resume`.
    pub rerun_failed: bool,
    /// Test-only hook mirroring `MiniArgs::deterministic_responses`: scripted
    /// model responses forwarded to every task's `mini::run`, bypassing the
    /// real model API. Always `None` from the CLI entry point.
    pub deterministic_responses: Option<Vec<String>>,
    /// Test-only hook mirroring `MiniArgs::deterministic_usage_per_call`.
    /// Always `None` from the CLI entry point.
    pub deterministic_usage_per_call: Option<crate::model::ModelUsage>,
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
    if let Some(issue) = collect_task_validation_issues(&tasks).into_iter().next() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            issue.message,
        )));
    }

    // ── Prepare output directory ──────────────────────────────────────────
    let suite_dir = args.output_dir.join(&args.suite_name);
    if let Err(msg) = validate_suite_name(&args.suite_name) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(msg)));
    }
    std::fs::create_dir_all(&suite_dir).map_err(Error::Io)?;

    // ── `--rerun-failed` selection (issue #825) ────────────────────────────
    // Distinct from `--resume` (which continues an interrupted run and skips
    // every terminal task, pass or fail): this mode re-runs only the subset
    // that previously did not pass. The CLI also rejects this combination via
    // `conflicts_with`; this guard keeps `suite::run` itself safe for direct
    // (non-CLI) callers.
    if args.rerun_failed && args.resume {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--rerun-failed cannot be combined with --resume: --resume continues an \
             interrupted run (skips every terminal task, pass or fail); --rerun-failed \
             re-runs only the previously-failed subset. Choose one."
                .into(),
        )));
    }
    let prior_by_id: HashMap<String, SuiteTaskResult> = if args.rerun_failed {
        load_prior_suite_state(&suite_dir, &tasks).ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "--rerun-failed: no suite-results.json or task trajectories found in '{}'; \
                 run `agent suite` once without --rerun-failed first",
                suite_dir.display()
            )))
        })?
    } else {
        HashMap::new()
    };
    let run_ids: Option<std::collections::HashSet<String>> = if args.rerun_failed {
        let ids = tasks_needing_rerun(&tasks, &prior_by_id);
        eprintln!(
            "agent suite --rerun-failed: {} of {} task(s) need a re-run in '{}'",
            ids.len(),
            tasks.len(),
            suite_dir.display()
        );
        Some(ids)
    } else {
        None
    };

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

        // ── `--rerun-failed`: carry forward already-passing results untouched,
        //    zero model calls and zero fresh cost for them ──────────────────
        if let Some(carried) = try_carry_forward(&task.id, run_ids.as_ref(), &prior_by_id) {
            accumulate_carried(
                &carried,
                &mut total_cost,
                &mut resolved_count,
                &mut verified_count,
            );
            task_results.push(carried);
            continue;
        }

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
                    carried_over: false,
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
            driver_append_system_prompt: false,
            driver_isolated: false,
            output_dir: suite_dir.clone(),
            trajectory_name: trajectory_name.clone(),
            deterministic_responses: args.deterministic_responses.clone(),
            deterministic_usage_per_call: args.deterministic_usage_per_call.clone(),
            task_timeout_secs: args.task_timeout_secs,
            cancellation: None,
            stream_addr: None,
            patch_capture: None,
            verification_checks: task_verify_checks,
            verification_timeout_secs: args.verify_timeout_secs,
            resume_from: None,
            interactive_mode: crate::run::mini::InteractiveMode::Off,
            no_bell: false,
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
            issue_provenance: None,
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
                carried_over: false,
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
        // A passing task later in the pack must still be carried forward
        // unchanged even when an earlier re-run task caused this halt —
        // otherwise it would be wrongly demoted to a `skipped_*` row here.
        if let Some(carried) = try_carry_forward(&task.id, run_ids.as_ref(), &prior_by_id) {
            accumulate_carried(
                &carried,
                &mut total_cost,
                &mut resolved_count,
                &mut verified_count,
            );
            task_results.push(carried);
            continue;
        }
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
            carried_over: false,
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

/// One task-pack validation problem found by [`collect_task_validation_issues`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskIssue {
    /// The offending task id, when known (absent for an empty-id violation).
    pub task_id: Option<String>,
    /// Human-readable description, matching the historical `agent suite`
    /// error-message wording for each violation kind.
    pub message: String,
}

/// Validate every task's `id`/`task` fields and cross-task id uniqueness,
/// returning *all* violations found rather than stopping at the first
/// (unlike the `agent suite` run path, which bails on the first issue via
/// [`run`]). Used by both `agent suite` (which reports only the first issue)
/// and `agent suite --check` (which reports every issue it can find).
pub(crate) fn collect_task_validation_issues(tasks: &[SuiteTaskSpec]) -> Vec<TaskIssue> {
    let mut issues = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for task in tasks {
        if task.id.is_empty() {
            issues.push(TaskIssue {
                task_id: None,
                message: "task id must not be empty".to_owned(),
            });
        }
        if task.task.trim().is_empty() {
            issues.push(TaskIssue {
                task_id: Some(task.id.clone()),
                message: format!("task '{}': task description must not be empty", task.id),
            });
        }
        if task.id.contains('/') || task.id.contains('\\') || task.id.contains("..") {
            issues.push(TaskIssue {
                task_id: Some(task.id.clone()),
                message: format!(
                    "task id '{}' must not contain path separators or '..'",
                    task.id
                ),
            });
        }
        if !task.id.is_empty() && !seen.insert(task.id.as_str()) {
            issues.push(TaskIssue {
                task_id: Some(task.id.clone()),
                message: format!("duplicate task id '{}'", task.id),
            });
        }
    }
    issues
}

/// Validate a suite name for safe use as an `--output` subdirectory path
/// segment. Shared by `agent suite` (fails fast on the first violation via
/// [`run`]) and `agent suite --check` (reports it as a preflight check).
pub(crate) fn validate_suite_name(name: &str) -> Result<(), String> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        Err(format!(
            "suite name '{name}' must not contain path separators or '..'"
        ))
    } else {
        Ok(())
    }
}

/// Parse `NAME:COMMAND` verify check specs.
pub(crate) fn parse_verify_checks(specs: &[String]) -> Result<Vec<VerificationCheck>, Error> {
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

    // ── RED: collect_task_validation_issues (issue #821) ───────────────────

    fn spec(id: &str, task: &str) -> SuiteTaskSpec {
        SuiteTaskSpec {
            id: id.to_owned(),
            task: task.to_owned(),
            extra_context: None,
            verify: vec![],
        }
    }

    #[test]
    fn collect_task_validation_issues_empty_for_valid_pack() {
        let tasks = vec![spec("t1", "do a thing"), spec("t2", "do another")];
        assert!(collect_task_validation_issues(&tasks).is_empty());
    }

    #[test]
    fn collect_task_validation_issues_flags_empty_id() {
        let tasks = vec![spec("", "do a thing")];
        let issues = collect_task_validation_issues(&tasks);
        assert!(
            issues
                .iter()
                .any(|i| i.message == "task id must not be empty")
        );
    }

    #[test]
    fn collect_task_validation_issues_flags_empty_task_description() {
        let tasks = vec![spec("t1", "   ")];
        let issues = collect_task_validation_issues(&tasks);
        assert!(issues.iter().any(|i| i.task_id.as_deref() == Some("t1")
            && i.message.contains("task description must not be empty")));
    }

    #[test]
    fn collect_task_validation_issues_flags_unsafe_id() {
        let tasks = vec![spec("../escape", "do a thing")];
        let issues = collect_task_validation_issues(&tasks);
        assert!(issues.iter().any(|i| i.message.contains("path separators")));
    }

    #[test]
    fn collect_task_validation_issues_flags_duplicate_id() {
        let tasks = vec![spec("dup", "first"), spec("dup", "second")];
        let issues = collect_task_validation_issues(&tasks);
        assert!(
            issues
                .iter()
                .any(|i| i.message == "duplicate task id 'dup'")
        );
    }

    #[test]
    fn collect_task_validation_issues_reports_every_violation_not_just_first() {
        // Three independently-broken tasks: all issues should surface, not
        // just the first one encountered (the whole point of --check).
        let tasks = vec![
            spec("", "ok task"),
            spec("t2", "  "),
            spec("dup", "ok"),
            spec("dup", "ok again"),
        ];
        let issues = collect_task_validation_issues(&tasks);
        assert!(
            issues.len() >= 3,
            "expected multiple issues, got {issues:?}"
        );
    }

    // ── RED: validate_suite_name ────────────────────────────────────────

    #[test]
    fn validate_suite_name_accepts_plain_name() {
        assert!(validate_suite_name("my-suite").is_ok());
    }

    #[test]
    fn validate_suite_name_rejects_path_separators_and_dotdot() {
        assert!(validate_suite_name("../escape").is_err());
        assert!(validate_suite_name("a/b").is_err());
        assert!(validate_suite_name("a\\b").is_err());
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
            carried_over: false,
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
                    carried_over: false,
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
                    carried_over: false,
                },
            ],
        }
    }

    fn passing_result(id: &str) -> SuiteTaskResult {
        SuiteTaskResult {
            id: id.to_owned(),
            outcome: crate::trajectory::outcome::SUBMITTED.to_owned(),
            verification_status: crate::trajectory::verification_status::VERIFIED.to_owned(),
            steps: Some(3),
            cost_usd: Some(0.02),
            duration_secs: Some(1.0),
            failure_category: None,
            trajectory_path: format!("runs/s/{id}.traj.json"),
            attempt_count: 3,
            unchanged_failure_count: 0,
            verifier_delta: Some(1),
            stop_reason: Some("submitted".into()),
            carried_over: false,
        }
    }

    fn failing_result(id: &str) -> SuiteTaskResult {
        SuiteTaskResult {
            verification_status: crate::trajectory::verification_status::VERIFICATION_FAILED
                .to_owned(),
            ..passing_result(id)
        }
    }

    // ── RED: `tasks_needing_rerun` (issue #825) ────────────────────────────

    #[test]
    fn tasks_needing_rerun_excludes_passing_includes_failing_and_unknown() {
        let tasks = vec![spec("t1", "do a"), spec("t2", "do b"), spec("t3", "do c")];
        let mut prior = HashMap::new();
        prior.insert("t1".to_owned(), passing_result("t1"));
        prior.insert("t2".to_owned(), failing_result("t2"));
        // t3 has no prior entry at all (e.g. added to the pack after the last run).
        let ids = tasks_needing_rerun(&tasks, &prior);
        assert!(
            !ids.contains("t1"),
            "passing task must be excluded: {ids:?}"
        );
        assert!(ids.contains("t2"), "failing task must be selected: {ids:?}");
        assert!(
            ids.contains("t3"),
            "never-run task must be selected: {ids:?}"
        );
    }

    #[test]
    fn tasks_needing_rerun_treats_submitted_but_verification_failed_as_failing() {
        let tasks = vec![spec("t1", "do a")];
        let mut prior = HashMap::new();
        let mut r = passing_result("t1");
        r.verification_status =
            crate::trajectory::verification_status::VERIFICATION_FAILED.to_owned();
        prior.insert("t1".to_owned(), r);
        assert!(tasks_needing_rerun(&tasks, &prior).contains("t1"));
    }

    #[test]
    fn tasks_needing_rerun_empty_when_all_passing() {
        let tasks = vec![spec("t1", "a"), spec("t2", "b")];
        let mut prior = HashMap::new();
        prior.insert("t1".to_owned(), passing_result("t1"));
        prior.insert("t2".to_owned(), passing_result("t2"));
        assert!(tasks_needing_rerun(&tasks, &prior).is_empty());
    }

    // ── RED: `load_prior_suite_state` (issue #825) ─────────────────────────

    #[test]
    fn load_prior_suite_state_prefers_suite_results_json() {
        let dir = tempfile::tempdir().unwrap();
        let results = SuiteResults {
            suite_name: "s".into(),
            task_count: 1,
            resolved_count: 1,
            verified_count: 1,
            total_cost_usd: 0.01,
            total_duration_secs: 1.0,
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:00:01Z".into(),
            tasks: vec![passing_result("t1")],
        };
        let json = crate::artifact::to_string_pretty(ArtifactKind::SuiteResults, &results).unwrap();
        std::fs::write(dir.path().join("suite-results.json"), json).unwrap();

        let tasks = vec![spec("t1", "do a")];
        let prior = load_prior_suite_state(dir.path(), &tasks).unwrap();
        assert!(prior.contains_key("t1"));
        assert_eq!(prior["t1"].verification_status, "verified");
    }

    #[test]
    fn load_prior_suite_state_falls_back_to_trajectories_when_results_json_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut traj = Trajectory::new();
        traj.info.outcome = Some(crate::trajectory::outcome::SUBMITTED.to_owned());
        traj.info.verification_status =
            Some(crate::trajectory::verification_status::VERIFIED.to_owned());
        traj.info.partial = false;
        let text = serde_json::to_string(&traj).unwrap();
        std::fs::write(dir.path().join("t1.traj.json"), text).unwrap();

        let tasks = vec![spec("t1", "do a")];
        let prior = load_prior_suite_state(dir.path(), &tasks).unwrap();
        assert!(prior.contains_key("t1"));
    }

    #[test]
    fn load_prior_suite_state_none_when_nothing_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = vec![spec("t1", "do a")];
        assert!(load_prior_suite_state(dir.path(), &tasks).is_none());
    }

    // ── RED: `summary_table` carried-forward note (issue #825) ─────────────

    #[test]
    fn summary_table_shows_carried_forward_note_when_present() {
        let mut results = make_sample_suite_results();
        results.tasks[0].carried_over = true;
        let table = results.summary_table();
        assert!(table.contains("carried forward"), "table: {table}");
    }

    #[test]
    fn summary_table_omits_carried_forward_note_when_absent() {
        let results = make_sample_suite_results();
        let table = results.summary_table();
        assert!(!table.contains("carried forward"), "table: {table}");
    }

    // ── RED/GREEN: `--rerun-failed` guards (issue #825) ────────────────────

    #[tokio::test]
    async fn rerun_failed_and_resume_together_is_rejected() {
        let work = tempfile::tempdir().unwrap();
        let pack = work.path().join("tasks.yaml");
        std::fs::write(&pack, "- id: t1\n  task: do a\n").unwrap();
        let args = SuiteArgs {
            tasks_file: pack,
            format_override: None,
            suite_name: "s".into(),
            config: crate::config::Config::defaults().unwrap(),
            output_dir: work.path().join("runs"),
            suite_cost_limit_usd: None,
            verify: vec![],
            verify_timeout_secs: 60,
            resume: true,
            task_timeout_secs: None,
            step_limit: None,
            per_task_budget_usd: None,
            rerun_failed: true,
            deterministic_responses: None,
            deterministic_usage_per_call: None,
        };
        let err = run(args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--resume"), "message: {msg}");
    }

    #[tokio::test]
    async fn rerun_failed_without_prior_results_is_rejected() {
        let work = tempfile::tempdir().unwrap();
        let pack = work.path().join("tasks.yaml");
        std::fs::write(&pack, "- id: t1\n  task: do a\n").unwrap();
        let args = SuiteArgs {
            tasks_file: pack,
            format_override: None,
            suite_name: "never-run".into(),
            config: crate::config::Config::defaults().unwrap(),
            output_dir: work.path().join("runs"),
            suite_cost_limit_usd: None,
            verify: vec![],
            verify_timeout_secs: 60,
            resume: false,
            task_timeout_secs: None,
            step_limit: None,
            per_task_budget_usd: None,
            rerun_failed: true,
            deterministic_responses: None,
            deterministic_usage_per_call: None,
        };
        let err = run(args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no suite-results.json"), "message: {msg}");
    }

    // ── GREEN: `--rerun-failed` end-to-end (issue #825) ────────────────────

    #[tokio::test]
    async fn rerun_failed_reruns_only_failing_tasks_and_carries_the_rest_forward() {
        let work = tempfile::tempdir().unwrap();
        let output_dir = work.path().join("runs");

        let pack_v1 = work.path().join("tasks.yaml");
        std::fs::write(
            &pack_v1,
            "- id: task-a\n  task: do a\n  verify:\n    - ok:true\n\
             - id: task-b\n  task: do b\n  verify:\n    - bad:false\n",
        )
        .unwrap();

        let mut cfg = crate::config::Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let submit = "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfix\n```".to_owned();

        let base_args = |tasks_file: PathBuf, rerun_failed: bool| SuiteArgs {
            tasks_file,
            format_override: None,
            suite_name: "my-suite".to_owned(),
            config: cfg.clone(),
            output_dir: output_dir.clone(),
            suite_cost_limit_usd: None,
            verify: vec![],
            verify_timeout_secs: 60,
            resume: false,
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            rerun_failed,
            deterministic_responses: Some(vec![submit.clone()]),
            deterministic_usage_per_call: None,
        };

        let exit1 = run(base_args(pack_v1, false)).await.unwrap();
        assert_eq!(exit1, ExitCode::VerificationFailure);

        let suite_dir = output_dir.join("my-suite");
        let task_a_traj_before =
            std::fs::read_to_string(suite_dir.join("task-a.traj.json")).unwrap();

        // Operator fixes task-b's verify command and re-runs only the failed subset.
        let pack_v2 = work.path().join("tasks-fixed.yaml");
        std::fs::write(
            &pack_v2,
            "- id: task-a\n  task: do a\n  verify:\n    - ok:true\n\
             - id: task-b\n  task: do b\n  verify:\n    - ok2:true\n",
        )
        .unwrap();

        let exit2 = run(base_args(pack_v2, true)).await.unwrap();
        assert_eq!(exit2, ExitCode::Success);

        let task_a_traj_after =
            std::fs::read_to_string(suite_dir.join("task-a.traj.json")).unwrap();
        assert_eq!(
            task_a_traj_before, task_a_traj_after,
            "carried-forward task's trajectory must not be touched (zero cost, zero model calls)"
        );

        let merged_text = std::fs::read_to_string(suite_dir.join("suite-results.json")).unwrap();
        let merged: serde_json::Value = serde_json::from_str(&merged_text).unwrap();
        let tasks_arr = merged["tasks"].as_array().unwrap();
        let a = tasks_arr.iter().find(|t| t["id"] == "task-a").unwrap();
        let b = tasks_arr.iter().find(|t| t["id"] == "task-b").unwrap();
        assert_eq!(a["carried_over"], true, "a: {a}");
        assert_eq!(b["carried_over"], false, "b: {b}");
        assert_eq!(b["verification_status"], "verified", "b: {b}");
        assert_eq!(merged["task_count"], 2);
    }

    #[tokio::test]
    async fn rerun_failed_suite_cost_limit_applies_only_to_rerun_subset() {
        let work = tempfile::tempdir().unwrap();
        let output_dir = work.path().join("runs");
        let suite_name = "budget-suite";

        // Seed prior state directly: two failing tasks and one already-passing.
        let suite_dir = output_dir.join(suite_name);
        std::fs::create_dir_all(&suite_dir).unwrap();
        let prior = SuiteResults {
            suite_name: suite_name.to_owned(),
            task_count: 3,
            resolved_count: 1,
            verified_count: 1,
            total_cost_usd: 0.02,
            total_duration_secs: 3.0,
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:00:03Z".into(),
            tasks: vec![
                failing_result("task-x"),
                failing_result("task-y"),
                passing_result("task-z"),
            ],
        };
        let json = crate::artifact::to_string_pretty(ArtifactKind::SuiteResults, &prior).unwrap();
        std::fs::write(suite_dir.join("suite-results.json"), json).unwrap();

        let pack = work.path().join("tasks.yaml");
        std::fs::write(
            &pack,
            "- id: task-x\n  task: do x\n- id: task-y\n  task: do y\n- id: task-z\n  task: do z\n",
        )
        .unwrap();

        let mut cfg = crate::config::Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let submit = "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfix\n```".to_owned();

        let args = SuiteArgs {
            tasks_file: pack,
            format_override: None,
            suite_name: suite_name.to_owned(),
            config: cfg,
            output_dir,
            suite_cost_limit_usd: Some(0.5),
            verify: vec![],
            verify_timeout_secs: 60,
            resume: false,
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            rerun_failed: true,
            deterministic_responses: Some(vec![submit]),
            deterministic_usage_per_call: Some(crate::model::ModelUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_usd: Some(1.0),
            }),
        };

        let exit = run(args).await.unwrap();
        assert_eq!(exit, ExitCode::BudgetHalt);

        let merged_text = std::fs::read_to_string(suite_dir.join("suite-results.json")).unwrap();
        let merged: serde_json::Value = serde_json::from_str(&merged_text).unwrap();
        let tasks_arr = merged["tasks"].as_array().unwrap();
        let z = tasks_arr.iter().find(|t| t["id"] == "task-z").unwrap();
        assert_eq!(
            z["carried_over"], true,
            "already-passing task must stay carried forward regardless of the rerun \
             subset's cost cap: {z}"
        );
        let outcomes: Vec<&str> = tasks_arr
            .iter()
            .map(|t| t["outcome"].as_str().unwrap())
            .collect();
        assert!(
            outcomes.contains(&"skipped_budget_exhausted"),
            "one rerun task should be skipped by the cost cap: {outcomes:?}"
        );
    }

    /// Regression for a review finding: the early-halt padding loop used to
    /// mark *every* not-yet-reached task as `skipped_*`, even ones that
    /// `--rerun-failed` had already decided to carry forward unchanged. If
    /// an earlier re-run task hard-fails (env/preflight error) before a
    /// later, already-passing task is reached, that later task must still
    /// be carried forward — not demoted to a skipped row.
    #[tokio::test]
    async fn rerun_failed_carries_forward_later_passing_task_despite_earlier_hard_halt() {
        let work = tempfile::tempdir().unwrap();
        let output_dir = work.path().join("runs");
        let suite_name = "halt-suite";

        let suite_dir = output_dir.join(suite_name);
        std::fs::create_dir_all(&suite_dir).unwrap();

        // Prior state: task-x needs a re-run; task-y already passes.
        let prior = SuiteResults {
            suite_name: suite_name.to_owned(),
            task_count: 2,
            resolved_count: 1,
            verified_count: 1,
            total_cost_usd: 0.02,
            total_duration_secs: 2.0,
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:00:02Z".into(),
            tasks: vec![failing_result("task-x"), passing_result("task-y")],
        };
        let json = crate::artifact::to_string_pretty(ArtifactKind::SuiteResults, &prior).unwrap();
        std::fs::write(suite_dir.join("suite-results.json"), json).unwrap();

        // Force task-x's mini run to hard-fail with an I/O error before any
        // trajectory is written, by pre-occupying its trajectory path with a
        // directory instead of a file.
        std::fs::create_dir_all(suite_dir.join("task-x.traj.json")).unwrap();

        let pack = work.path().join("tasks.yaml");
        std::fs::write(
            &pack,
            "- id: task-x\n  task: do x\n- id: task-y\n  task: do y\n",
        )
        .unwrap();

        let mut cfg = crate::config::Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;
        let submit = "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfix\n```".to_owned();

        let args = SuiteArgs {
            tasks_file: pack,
            format_override: None,
            suite_name: suite_name.to_owned(),
            config: cfg,
            output_dir,
            suite_cost_limit_usd: None,
            verify: vec![],
            verify_timeout_secs: 60,
            resume: false,
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            rerun_failed: true,
            deterministic_responses: Some(vec![submit]),
            deterministic_usage_per_call: None,
        };

        // The suite still completes (returns Ok) — task-x's hard failure is
        // recorded as a task-scoped error, not a propagated Err.
        let _ = run(args).await.unwrap();

        let merged_text = std::fs::read_to_string(suite_dir.join("suite-results.json")).unwrap();
        let merged: serde_json::Value = serde_json::from_str(&merged_text).unwrap();
        let tasks_arr = merged["tasks"].as_array().unwrap();
        // task-y must still be present in the merged results.
        let y = tasks_arr.iter().find(|t| t["id"] == "task-y").unwrap();
        assert_eq!(
            y["carried_over"], true,
            "a passing task after an earlier hard-halted re-run task must still be carried \
             forward, not demoted to skipped_*: {y}"
        );
        assert_eq!(y["outcome"], "submitted", "task-y: {y}");
        assert_eq!(y["verification_status"], "verified", "task-y: {y}");
    }
}
