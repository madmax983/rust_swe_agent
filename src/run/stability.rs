//! `agent stability` — single-task run-to-run variance measurement (issue #475).
//!
//! Runs one task N times through the existing `mini` path and emits a
//! schema-versioned `stability-results.json` artifact with pass@k, cost/step
//! statistics, and patch-identity rate — giving operators a principled way to
//! tell whether a behaviour change is signal or LLM noise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::error::Error;
use crate::exit_code::ExitCode;

// ── Public helper types ───────────────────────────────────────────────────────

/// Statistics for a numeric series (min / max / mean / population stddev).
#[derive(Debug, Clone, Copy)]
pub struct StatsResult {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
}

// ── Result schema ─────────────────────────────────────────────────────────────

/// Per-run record stored in `runs_detail`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StabilityRunDetail {
    /// 1-based run index.
    pub run_number: u32,
    /// Terminal outcome string from the trajectory (or `skipped_budget_exhausted`).
    pub outcome: String,
    /// Whether this run passed the success predicate.
    pub passed: bool,
    /// Billed cost for this run in USD. `None` when run was skipped or cost unknown.
    pub cost_usd: Option<f64>,
    /// Agent steps executed. `None` when run was skipped.
    pub step_count: Option<u32>,
    /// Whether this run was skipped (cost cap exceeded before it started).
    pub skipped: bool,
    /// Human-readable reason for skipping, when `skipped == true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// Top-level `stability-results.json` artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StabilityResults {
    pub schema_version: ArtifactSchemaVersion,
    pub artifact_kind: ArtifactKind,
    /// The task description that was run.
    pub task: String,
    /// Total number of runs attempted (including skipped).
    pub runs: u32,
    /// Number of non-skipped runs that passed the success predicate.
    pub pass_count: u32,
    /// Fraction of non-skipped runs that passed (0.0–1.0).
    pub pass_at_k: f64,
    /// Fraction of submitted runs whose captured patch byte-matches the modal
    /// patch. 0.0 when no patches were captured.
    pub patch_identical_rate: f64,
    /// Which predicate was used: `"verify"` (all `--verify` checks passed) or
    /// `"outcome_submitted"` (fallback when no `--verify` supplied).
    pub pass_predicate: String,
    pub cost_usd_min: f64,
    pub cost_usd_max: f64,
    pub cost_usd_mean: f64,
    pub cost_usd_stddev: f64,
    pub step_count_min: f64,
    pub step_count_max: f64,
    pub step_count_mean: f64,
    pub step_count_stddev: f64,
    pub started_at: String,
    pub finished_at: String,
    pub runs_detail: Vec<StabilityRunDetail>,
}

impl StabilityResults {
    /// Render a human-readable multi-line summary.
    pub fn summary_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();

        let _ = writeln!(
            &mut out,
            "agent stability: \"{}\" — {}/{} runs passed (pass_at_k={:.3})",
            truncate(&self.task, 60),
            self.pass_count,
            self.runs - self.runs_detail.iter().filter(|d| d.skipped).count() as u32,
            self.pass_at_k,
        );
        let _ = writeln!(&mut out, "  pass_predicate    : {}", self.pass_predicate);
        let _ = writeln!(
            &mut out,
            "  patch_identical   : {:.3}",
            self.patch_identical_rate
        );
        let _ = writeln!(
            &mut out,
            "  cost_usd          : min={:.4}  max={:.4}  mean={:.4}  stddev={:.4}",
            self.cost_usd_min, self.cost_usd_max, self.cost_usd_mean, self.cost_usd_stddev
        );
        let _ = writeln!(
            &mut out,
            "  step_count        : min={:.1}    max={:.1}    mean={:.1}    stddev={:.1}",
            self.step_count_min,
            self.step_count_max,
            self.step_count_mean,
            self.step_count_stddev
        );
        let _ = writeln!(&mut out);
        let _ = writeln!(
            &mut out,
            "  {:<4}  {:<22}  {:<6}  {:>8}  {:>6}",
            "RUN", "OUTCOME", "PASS", "COST($)", "STEPS"
        );
        let _ = writeln!(&mut out, "  {}", "-".repeat(54));
        for d in &self.runs_detail {
            let cost = d
                .cost_usd
                .map_or_else(|| "-".to_owned(), |c| format!("{c:.4}"));
            let steps = d
                .step_count
                .map_or_else(|| "-".to_owned(), |s| s.to_string());
            let pass_label = if d.skipped {
                "skip"
            } else if d.passed {
                "yes"
            } else {
                "no"
            };
            let _ = writeln!(
                &mut out,
                "  {:<4}  {:<22}  {:<6}  {:>8}  {:>6}",
                d.run_number,
                truncate(&d.outcome, 22),
                pass_label,
                cost,
                steps,
            );
        }
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

// ── Runner arguments ──────────────────────────────────────────────────────────

/// Arguments for `agent stability`.
pub struct StabilityArgs {
    /// The task to run repeatedly.
    pub task: String,
    /// How many times to run the task (validated to 1..=10 before calling `run`).
    pub runs: u32,
    pub config: crate::config::Config,
    /// Root output directory; artifacts land in `<output_dir>/<stability_name>/`.
    pub output_dir: PathBuf,
    /// Subdirectory name for this stability run.
    pub stability_name: String,
    /// Verify checks in `NAME:COMMAND` format. Empty → fallback predicate.
    pub verify: Vec<String>,
    pub verify_timeout_secs: u64,
    /// Fail with `StabilityGateFailure` when `pass_at_k < fail_under`.
    pub fail_under: Option<f64>,
    /// Total USD ceiling. Remaining runs are recorded as skipped when exceeded.
    pub cost_limit_usd: Option<f64>,
    pub task_timeout_secs: Option<u64>,
    pub step_limit: Option<u32>,
    /// Per-run USD ceiling enforced inside the agent loop.
    pub per_task_budget_usd: Option<f64>,
    /// Scripted model responses (test-only). Each run receives an identical
    /// clone of this vec, enabling deterministic integration tests.
    pub deterministic_responses: Option<Vec<String>>,
    /// Fixed per-call usage reported by the scripted backend (test-only).
    pub deterministic_usage_per_call: Option<crate::model::ModelUsage>,
}

// ── Public computation helpers ────────────────────────────────────────────────

/// Validate that `n` is in the range 1..=10. Returns an error message on failure.
pub fn validate_runs(n: u32) -> Result<(), String> {
    if n < 1 || n > 10 {
        Err(format!(
            "--runs must be between 1 and 10 (inclusive), got {n}"
        ))
    } else {
        Ok(())
    }
}

/// Return `true` when `pass_at_k < fail_under` (i.e. the gate should fire).
pub fn should_fail_under(pass_at_k: f64, fail_under: Option<f64>) -> bool {
    fail_under.is_some_and(|threshold| pass_at_k < threshold)
}

/// Label for the success predicate used in `pass_predicate` field.
pub fn pass_predicate_label(verify: &[String]) -> &'static str {
    if verify.is_empty() {
        "outcome_submitted"
    } else {
        "verify"
    }
}

/// Compute `(pass_count, pass_at_k)` from run details.
///
/// Skipped runs are excluded from both numerator and denominator.
pub fn compute_pass_at_k(details: &[StabilityRunDetail]) -> (u32, f64) {
    let non_skipped: Vec<&StabilityRunDetail> = details.iter().filter(|d| !d.skipped).collect();
    let total = non_skipped.len() as f64;
    let pass_count = non_skipped.iter().filter(|d| d.passed).count() as u32;
    let pass_at_k = if total > 0.0 {
        f64::from(pass_count) / total
    } else {
        0.0
    };
    (pass_count, pass_at_k)
}

/// Compute the fraction of submitted runs whose patch byte-matches the modal patch.
///
/// `patches` is a vec where `Some(text)` means the run submitted a patch and
/// `None` means the run did not submit. Returns 0.0 when no runs submitted.
pub fn compute_patch_identical_rate(patches: &[Option<String>]) -> f64 {
    let submitted: Vec<&str> = patches.iter().filter_map(|p| p.as_deref()).collect();
    if submitted.is_empty() {
        return 0.0;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for p in &submitted {
        *counts.entry(p).or_insert(0) += 1;
    }
    let max_count = counts.values().max().copied().unwrap_or(0);
    max_count as f64 / submitted.len() as f64
}

/// Compute min / max / mean / population stddev for a slice of values.
/// Returns all-zero stats for an empty slice.
pub fn compute_stats(values: &[f64]) -> StatsResult {
    if values.is_empty() {
        return StatsResult {
            min: 0.0,
            max: 0.0,
            mean: 0.0,
            stddev: 0.0,
        };
    }
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    StatsResult {
        min,
        max,
        mean,
        stddev: variance.sqrt(),
    }
}

// ── Main runner ───────────────────────────────────────────────────────────────

/// Run stability measurement and return the final exit code.
///
/// Writes `<output_dir>/<stability_name>/stability-results.json` and prints a
/// human-readable summary. Returns `StabilityGateFailure` when `--fail-under`
/// is set and `pass_at_k < fail_under`.
pub async fn run(args: StabilityArgs) -> Result<ExitCode, Error> {
    // ── Prepare output directory ──────────────────────────────────────────────
    let stability_dir = args.output_dir.join(&args.stability_name);
    std::fs::create_dir_all(&stability_dir).map_err(Error::Io)?;

    // ── Parse verify checks ───────────────────────────────────────────────────
    let verification_checks = parse_verify_checks(&args.verify)?;

    let pass_predicate = pass_predicate_label(&args.verify).to_owned();
    let started_at = Utc::now().to_rfc3339();

    let mut runs_detail: Vec<StabilityRunDetail> = Vec::with_capacity(args.runs as usize);
    let mut cumulative_cost = 0.0_f64;
    let mut patches: Vec<Option<String>> = Vec::new();

    for run_number in 1..=args.runs {
        // ── Cost-cap check: skip remaining runs ───────────────────────────────
        if let Some(limit) = args.cost_limit_usd {
            if cumulative_cost >= limit {
                runs_detail.push(StabilityRunDetail {
                    run_number,
                    outcome: "skipped_budget_exhausted".into(),
                    passed: false,
                    cost_usd: None,
                    step_count: None,
                    skipped: true,
                    skip_reason: Some("cost_limit_usd".into()),
                });
                patches.push(None);
                continue;
            }
        }

        // ── Build per-run trajectory name and output path ─────────────────────
        let trajectory_name = format!("run_{run_number:02}");
        let traj_path = stability_dir.join(format!("{trajectory_name}.traj.json"));

        // ── Build MiniArgs and execute ────────────────────────────────────────
        let mut run_cfg = args.config.clone();
        if let Some(v) = args.step_limit {
            run_cfg.root.agent.step_limit = v;
        }
        if let Some(v) = args.per_task_budget_usd {
            run_cfg.root.agent.per_task_budget_usd = Some(v);
        }

        let mini_args = crate::run::mini::MiniArgs {
            task: args.task.clone(),
            extra_context: None,
            config: run_cfg,
            driver: crate::run::mini::RunDriver::Builtin,
            driver_append_system_prompt: false,
            driver_isolated: false,
            output_dir: stability_dir.clone(),
            trajectory_name: trajectory_name.clone(),
            deterministic_responses: args.deterministic_responses.clone(),
            deterministic_usage_per_call: args.deterministic_usage_per_call.clone(),
            task_timeout_secs: args.task_timeout_secs,
            cancellation: None,
            stream_addr: None,
            patch_capture: None,
            verification_checks: verification_checks.clone(),
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
            issue_provenance: None,
        };

        let _run_outcome = crate::run::mini::run(mini_args).await;

        // ── Read trajectory from disk ─────────────────────────────────────────
        let detail = if let Some(traj) = try_load_trajectory(&traj_path) {
            let outcome = traj
                .info
                .outcome
                .clone()
                .unwrap_or_else(|| "error".to_owned());
            let cost = traj.info.total_cost_usd;
            let steps = traj.info.steps;

            let passed = determine_pass(&traj, &verification_checks);

            cumulative_cost += cost.unwrap_or(0.0);

            // Capture patch text if available (sibling .patch file)
            let patch_text = read_sibling_patch(&stability_dir, &trajectory_name);
            patches.push(if outcome == crate::trajectory::outcome::SUBMITTED {
                patch_text
            } else {
                None
            });

            StabilityRunDetail {
                run_number,
                outcome,
                passed,
                cost_usd: cost,
                step_count: steps,
                skipped: false,
                skip_reason: None,
            }
        } else {
            // Mini errored before writing a trajectory
            patches.push(None);
            StabilityRunDetail {
                run_number,
                outcome: "error".into(),
                passed: false,
                cost_usd: None,
                step_count: None,
                skipped: false,
                skip_reason: None,
            }
        };

        runs_detail.push(detail);
    }

    let finished_at = Utc::now().to_rfc3339();

    // ── Compute aggregate statistics ──────────────────────────────────────────
    let (pass_count, pass_at_k) = compute_pass_at_k(&runs_detail);
    let patch_identical_rate = compute_patch_identical_rate(&patches);

    let cost_values: Vec<f64> = runs_detail
        .iter()
        .filter(|d| !d.skipped)
        .filter_map(|d| d.cost_usd)
        .collect();
    let step_values: Vec<f64> = runs_detail
        .iter()
        .filter(|d| !d.skipped)
        .filter_map(|d| d.step_count.map(f64::from))
        .collect();

    let cost_stats = compute_stats(&cost_values);
    let step_stats = compute_stats(&step_values);

    let results = StabilityResults {
        schema_version: ArtifactSchemaVersion::CURRENT,
        artifact_kind: ArtifactKind::StabilityResults,
        task: args.task.clone(),
        runs: args.runs,
        pass_count,
        pass_at_k,
        patch_identical_rate,
        pass_predicate,
        cost_usd_min: cost_stats.min,
        cost_usd_max: cost_stats.max,
        cost_usd_mean: cost_stats.mean,
        cost_usd_stddev: cost_stats.stddev,
        step_count_min: step_stats.min,
        step_count_max: step_stats.max,
        step_count_mean: step_stats.mean,
        step_count_stddev: step_stats.stddev,
        started_at,
        finished_at,
        runs_detail,
    };

    // ── Write stability-results.json ──────────────────────────────────────────
    let results_path = stability_dir.join("stability-results.json");
    write_results(&results, &results_path)?;

    // ── Print summary ─────────────────────────────────────────────────────────
    print!("{}", results.summary_text());

    // ── --fail-under gate ─────────────────────────────────────────────────────
    if should_fail_under(pass_at_k, args.fail_under) {
        return Ok(ExitCode::StabilityGateFailure);
    }

    Ok(ExitCode::Success)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn parse_verify_checks(
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

fn try_load_trajectory(path: &Path) -> Option<crate::trajectory::Trajectory> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn read_sibling_patch(dir: &Path, trajectory_name: &str) -> Option<String> {
    let patch_path = dir.join(format!("{trajectory_name}.patch"));
    std::fs::read_to_string(patch_path).ok()
}

/// Determine whether a completed trajectory passes the success predicate.
///
/// With verify checks: all must pass. Without: outcome must be `"submitted"`.
fn determine_pass(
    traj: &crate::trajectory::Trajectory,
    checks: &[crate::trajectory::VerificationCheck],
) -> bool {
    if checks.is_empty() {
        // Fallback predicate: submitted
        traj.info.outcome.as_deref() == Some(crate::trajectory::outcome::SUBMITTED)
    } else {
        // All verify checks must have passed
        traj.info.verification_status.as_deref()
            == Some(crate::trajectory::verification_status::VERIFIED)
    }
}

fn write_results(results: &StabilityResults, path: &Path) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(results)
        .map_err(|e| Error::Json(e))?;
    std::fs::write(path, json).map_err(Error::Io)
}
