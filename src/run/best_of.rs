//! `agent best-of` — sample N runs and emit the best patch (issue #485).
//!
//! Runs one task N times through the existing `mini` path with identical config,
//! scores each candidate patch by the operator's `--verify` oracle, and emits the
//! single best patch using a deterministic selection policy.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::error::Error;
use crate::exit_code::ExitCode;

// ── Result schema ─────────────────────────────────────────────────────────────

/// Per-run record stored in `best-of-results.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BestOfRunDetail {
    /// 0-based run index.
    pub run_index: u32,
    /// Terminal outcome string from the trajectory (or `skipped_budget_exhausted`).
    pub outcome: String,
    /// Number of `--verify` checks that passed for this run.
    pub verify_checks_passed: u32,
    /// Total number of `--verify` checks configured.
    pub verify_checks_total: u32,
    /// True iff all verify checks passed.
    pub passed: bool,
    /// Billed cost for this run in USD.
    pub total_cost_usd: Option<f64>,
    /// Agent steps executed.
    pub step_count: Option<u32>,
    /// Byte length of the captured patch. `None` when no patch was captured.
    pub patch_byte_len: Option<u64>,
    /// SHA-256 hex digest of the captured patch bytes. `None` when no patch was captured.
    pub patch_sha256: Option<String>,
    /// Whether this run was skipped (cost cap exceeded before it started).
    pub skipped: bool,
    /// Human-readable reason for skipping, when `skipped == true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// Top-level `best-of-results.json` artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BestOfResults {
    pub schema_version: ArtifactSchemaVersion,
    pub artifact_kind: ArtifactKind,
    /// The task description that was run.
    pub task: String,
    /// Total number of runs configured (including skipped).
    pub runs: u32,
    /// 0-based index of the selected winner run. `None` if all runs were skipped.
    pub winner_run_index: Option<u32>,
    /// Number of non-skipped runs that passed all verify checks.
    pub passing_run_count: u32,
    /// True when no run passed all verify checks.
    pub all_failed: bool,
    /// True when the tie-break chain beyond "most checks passed" was used.
    pub tie_break_applied: bool,
    /// Human-readable rationale for the winner selection.
    pub selection_rationale: String,
    pub started_at: String,
    pub finished_at: String,
    pub runs_detail: Vec<BestOfRunDetail>,
}

impl BestOfResults {
    /// Render a human-readable multi-line summary.
    pub fn summary_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();

        let non_skipped = self.runs_detail.iter().filter(|d| !d.skipped).count();

        let _ = writeln!(
            &mut out,
            "agent best-of: \"{}\" — {}/{} runs passed all verify checks",
            truncate(&self.task, 60),
            self.passing_run_count,
            non_skipped,
        );

        if self.all_failed {
            let _ = writeln!(
                &mut out,
                "  WARNING: all runs failed verify checks — best-scoring run selected"
            );
        }

        if let Some(winner) = self.winner_run_index {
            let _ = writeln!(&mut out, "  WINNER: run {winner} (0-based index)");
        }
        if self.tie_break_applied {
            let _ = writeln!(&mut out, "  tie_break_applied: true");
        }
        let _ = writeln!(
            &mut out,
            "  selection_rationale: {}",
            self.selection_rationale
        );
        let _ = writeln!(&mut out);
        let _ = writeln!(
            &mut out,
            "  {:<4}  {:<22}  {:<5}  {:>6}/{:<6}  {:>8}  {:>6}",
            "RUN", "OUTCOME", "PASS", "PASS", "TOTAL", "COST($)", "STEPS"
        );
        let _ = writeln!(&mut out, "  {}", "-".repeat(62));
        for d in &self.runs_detail {
            let cost = d
                .total_cost_usd
                .map_or_else(|| "-".to_owned(), |c| format!("{c:.4}"));
            let steps = d
                .step_count
                .map_or_else(|| "-".to_owned(), |s| s.to_string());
            let winner_marker = self.winner_run_index.is_some_and(|w| w == d.run_index);
            let pass_label = if d.skipped {
                "skip"
            } else if d.passed {
                "yes"
            } else {
                "no"
            };
            let winner_str = if winner_marker { "*" } else { " " };
            let _ = writeln!(
                &mut out,
                "{} {:<4}  {:<22}  {:<5}  {:>6}/{:<6}  {:>8}  {:>6}",
                winner_str,
                d.run_index,
                truncate(&d.outcome, 22),
                pass_label,
                d.verify_checks_passed,
                d.verify_checks_total,
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

// ── Selection policy ──────────────────────────────────────────────────────────

/// Deterministically select the best run from the completed runs.
///
/// Selection policy (all tie-breaks are deterministic):
/// 1. Most `verify_checks_passed` (skipped runs excluded).
/// 2. Lowest `total_cost_usd`.
/// 3. Fewest `step_count`.
/// 4. Smallest patch byte length (approximated by SHA-256 presence and string
///    length of the stored patch — callers store the patch content, not SHA).
/// 5. Lexicographically smallest `patch_sha256`.
///
/// Returns `(winner_run_index, rationale_string, tie_break_applied)`.
pub fn select_winner(runs: &[BestOfRunDetail]) -> (u32, String, bool) {
    let candidates: Vec<&BestOfRunDetail> = runs.iter().filter(|d| !d.skipped).collect();

    if candidates.is_empty() {
        // All skipped; pick run index 0 as a fallback (should not happen in practice)
        return (0, "all_skipped_fallback".into(), false);
    }

    // Primary sort: most checks passed (descending)
    let max_checks = candidates
        .iter()
        .map(|d| d.verify_checks_passed)
        .max()
        .unwrap_or(0);

    let tier1: Vec<&BestOfRunDetail> = candidates
        .iter()
        .copied()
        .filter(|d| d.verify_checks_passed == max_checks)
        .collect();

    if tier1.len() == 1 {
        return (
            tier1[0].run_index,
            "most_verify_checks_passed".into(),
            false,
        );
    }

    // Tie-break 1: lowest cost
    let min_cost = tier1
        .iter()
        .map(|d| d.total_cost_usd.unwrap_or(f64::MAX))
        .min_by(f64::total_cmp)
        .unwrap_or(f64::MAX);

    let tier2: Vec<&BestOfRunDetail> = tier1
        .iter()
        .copied()
        .filter(|d| {
            d.total_cost_usd.unwrap_or(f64::MAX).total_cmp(&min_cost) == std::cmp::Ordering::Equal
        })
        .collect();

    if tier2.len() == 1 {
        return (tier2[0].run_index, "tie_break:lowest_cost_usd".into(), true);
    }

    // Tie-break 2: fewest steps
    let min_steps = tier2
        .iter()
        .map(|d| d.step_count.unwrap_or(u32::MAX))
        .min()
        .unwrap_or(0);

    let tier3: Vec<&BestOfRunDetail> = tier2
        .iter()
        .copied()
        .filter(|d| d.step_count.unwrap_or(u32::MAX) == min_steps)
        .collect();

    if tier3.len() == 1 {
        return (tier3[0].run_index, "tie_break:fewest_steps".into(), true);
    }

    // Tie-break 3: smallest patch byte length
    let min_patch_len = tier3
        .iter()
        .map(|d| d.patch_byte_len.unwrap_or(u64::MAX))
        .min()
        .unwrap_or(0);

    let tier4: Vec<&BestOfRunDetail> = tier3
        .iter()
        .copied()
        .filter(|d| d.patch_byte_len.unwrap_or(u64::MAX) == min_patch_len)
        .collect();

    if tier4.len() == 1 {
        return (
            tier4[0].run_index,
            "tie_break:smallest_patch_bytes".into(),
            true,
        );
    }

    // Tie-break 4: lexicographically smallest patch SHA-256
    let best = tier4
        .iter()
        .copied()
        .min_by(|a, b| {
            // No sha (no patch) is treated as lexicographically largest
            let sha_a = a.patch_sha256.as_deref().unwrap_or("\u{FF}");
            let sha_b = b.patch_sha256.as_deref().unwrap_or("\u{FF}");
            sha_a.cmp(sha_b)
        })
        .unwrap_or(tier4[0]);

    (best.run_index, "tie_break:patch_sha256_lex".into(), true)
}

// ── Runner arguments ──────────────────────────────────────────────────────────

/// Arguments for `agent best-of`.
pub struct BestOfArgs {
    /// The task to run N times.
    pub task: String,
    /// How many times to run the task (validated 2..=10 before calling `run`).
    pub runs: u32,
    pub config: crate::config::Config,
    /// Root output directory; artifacts land in `<output_dir>/<best_of_name>/`.
    pub output_dir: PathBuf,
    /// Subdirectory name for this best-of run.
    pub best_of_name: String,
    /// Verify checks in `NAME:COMMAND` format. Empty → fallback to outcome_submitted.
    pub verify: Vec<String>,
    pub verify_timeout_secs: u64,
    /// Total USD ceiling. Remaining runs are recorded as skipped when exceeded.
    pub cost_limit_usd: Option<f64>,
    pub task_timeout_secs: Option<u64>,
    pub step_limit: Option<u32>,
    /// Per-run USD ceiling enforced inside the agent loop.
    pub per_task_budget_usd: Option<f64>,
    /// Where to write the winner's patch file. Defaults to `<best_of_dir>/best.patch`.
    pub output_patch: Option<PathBuf>,
    /// When true, exit 0 even when all runs fail verify checks.
    pub allow_no_pass: bool,
    /// Scripted model responses (test-only).
    pub deterministic_responses: Option<Vec<String>>,
    /// Fixed per-call usage for scripted backend (test-only).
    pub deterministic_usage_per_call: Option<crate::model::ModelUsage>,
    /// When false, suppress the human-readable summary (--format json).
    pub print_summary: bool,
}

// ── Validation ────────────────────────────────────────────────────────────────

/// Validate that `n` is in the range 2..=10. Returns an error message on failure.
pub fn validate_runs(n: u32) -> Result<(), String> {
    match n {
        0 | 1 => Err(format!(
            "--runs must be between 2 and 10 (inclusive), got {n}; \
             best-of-1 is equivalent to `mini` — use that command instead"
        )),
        2..=10 => Ok(()),
        _ => Err(format!(
            "--runs must be between 2 and 10 (inclusive), got {n}"
        )),
    }
}

// ── Main runner ───────────────────────────────────────────────────────────────

/// Run best-of selection and return the final exit code.
///
/// Writes `<output_dir>/<best_of_name>/best-of-results.json` and, when a
/// winner has a captured patch, writes the patch to `--output` (or
/// `<best_of_dir>/best.patch`).
#[allow(clippy::too_many_lines)]
pub async fn run(args: BestOfArgs) -> Result<ExitCode, Error> {
    // ── Validate name (no path traversal) ────────────────────────────────────
    if args.best_of_name.is_empty()
        || args.best_of_name.contains('/')
        || args.best_of_name.contains('\\')
        || args.best_of_name.contains("..")
    {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "best-of name '{}' must not contain path separators or '..'",
            args.best_of_name
        ))));
    }

    // ── Prepare output directory ──────────────────────────────────────────────
    let best_of_dir = args.output_dir.join(&args.best_of_name);
    std::fs::create_dir_all(&best_of_dir).map_err(Error::Io)?;

    // ── Parse verify checks ───────────────────────────────────────────────────
    let verification_checks = parse_verify_checks(&args.verify)?;

    let started_at = Utc::now().to_rfc3339();
    let mut runs_detail: Vec<BestOfRunDetail> = Vec::with_capacity(args.runs as usize);
    let mut cumulative_cost = 0.0_f64;
    let mut run_patch_texts: Vec<Option<String>> = Vec::new();

    for run_index in 0..args.runs {
        // ── Cost-cap check: skip remaining runs ───────────────────────────────
        if let Some(limit) = args.cost_limit_usd {
            if cumulative_cost >= limit {
                runs_detail.push(BestOfRunDetail {
                    run_index,
                    outcome: "skipped_budget_exhausted".into(),
                    verify_checks_passed: 0,
                    verify_checks_total: u32::try_from(verification_checks.len()).unwrap_or(0),
                    passed: false,
                    total_cost_usd: None,
                    step_count: None,
                    patch_byte_len: None,
                    patch_sha256: None,
                    skipped: true,
                    skip_reason: Some("cost_limit_usd".into()),
                });
                run_patch_texts.push(None);
                continue;
            }
        }

        let trajectory_name = format!("run_{run_index:02}");
        let traj_path = best_of_dir.join(format!("{trajectory_name}.traj.json"));
        let run_patch_path = best_of_dir.join(format!("{trajectory_name}.patch"));
        let _ = std::fs::remove_file(&traj_path);
        let _ = std::fs::remove_file(&run_patch_path);

        // ── Build and execute mini run ────────────────────────────────────────
        let mut run_cfg = args.config.clone();
        if let Some(v) = args.step_limit {
            run_cfg.root.agent.step_limit = v;
        }
        if let Some(v) = args.per_task_budget_usd {
            run_cfg.root.agent.per_task_budget_usd = Some(v);
        }
        // Clamp per-run budget to the remaining global budget so the last
        // allowed run cannot overrun the total ceiling by a full candidate.
        if let Some(limit) = args.cost_limit_usd {
            let remaining = (limit - cumulative_cost).max(0.0);
            let clamped = run_cfg
                .root
                .agent
                .per_task_budget_usd
                .map_or(remaining, |p| p.min(remaining));
            run_cfg.root.agent.per_task_budget_usd = Some(clamped);
        }

        let is_docker = matches!(
            run_cfg.root.environment.kind,
            crate::config::EnvKind::Docker
        );

        // For Docker, `git diff` runs inside the container so the workdir must
        // be the container path. For local, use the host CWD.
        let patch_workdir = if is_docker {
            PathBuf::from(&run_cfg.root.environment.workdir)
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        };

        // Snapshot HEAD before the run so agents that commit don't produce an
        // empty diff (local env only; Docker HEAD is snapshotted inside the
        // container where we can't reach it cheaply here).
        let base_commit = if is_docker {
            None
        } else {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&patch_workdir)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        };

        let mini_args = crate::run::mini::MiniArgs {
            task: args.task.clone(),
            extra_context: None,
            config: run_cfg,
            driver: crate::run::mini::RunDriver::Builtin,
            driver_append_system_prompt: false,
            driver_isolated: false,
            output_dir: best_of_dir.clone(),
            trajectory_name: trajectory_name.clone(),
            deterministic_responses: args.deterministic_responses.clone(),
            deterministic_usage_per_call: args.deterministic_usage_per_call.clone(),
            task_timeout_secs: args.task_timeout_secs,
            cancellation: None,
            stream_addr: None,
            patch_capture: Some(crate::run::mini::PatchCaptureSpec {
                base_commit,
                workdir: patch_workdir,
                patch_path: run_patch_path,
                // best-of's oracle is --verify, not patch non-emptiness; skipping
                // the SWE-bench "empty diff = error" rule prevents the outcome from
                // being downgraded when the scripted model or a no-op agent submits.
                skip_patch_validation: true,
            }),
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

        if let Err(e) = crate::run::mini::run(mini_args).await {
            if !traj_path.exists() {
                return Err(e);
            }
        }

        // ── Read trajectory and score run ─────────────────────────────────────
        let patch_text = read_sibling_patch(&best_of_dir, &trajectory_name);

        let detail = if let Some(traj) = try_load_trajectory(&traj_path) {
            let outcome = traj
                .info
                .outcome
                .clone()
                .unwrap_or_else(|| "error".to_owned());
            let cost = traj.info.total_cost_usd;
            let steps = traj.info.steps;

            cumulative_cost += cost.unwrap_or(0.0);

            let (checks_passed, checks_total) = count_verify_results(&traj, &verification_checks);

            let passed = checks_total > 0 && checks_passed == checks_total;
            let patch_byte_len = patch_text.as_deref().map(|p| p.len() as u64);
            let patch_sha = patch_text.as_deref().map(sha256_hex);

            BestOfRunDetail {
                run_index,
                outcome,
                verify_checks_passed: checks_passed,
                verify_checks_total: checks_total,
                passed,
                total_cost_usd: cost,
                step_count: steps,
                patch_byte_len,
                patch_sha256: patch_sha,
                skipped: false,
                skip_reason: None,
            }
        } else {
            BestOfRunDetail {
                run_index,
                outcome: "error".into(),
                verify_checks_passed: 0,
                verify_checks_total: u32::try_from(verification_checks.len()).unwrap_or(0),
                passed: false,
                total_cost_usd: None,
                step_count: None,
                patch_byte_len: None,
                patch_sha256: None,
                skipped: false,
                skip_reason: None,
            }
        };

        run_patch_texts.push(patch_text);
        runs_detail.push(detail);
    }

    let finished_at = Utc::now().to_rfc3339();

    // ── Select winner ─────────────────────────────────────────────────────────
    let passing_run_count =
        u32::try_from(runs_detail.iter().filter(|d| d.passed).count()).unwrap_or(u32::MAX);

    let all_failed = passing_run_count == 0 && runs_detail.iter().any(|d| !d.skipped);

    let (winner_run_index, selection_rationale, tie_break_applied) =
        if runs_detail.iter().any(|d| !d.skipped) {
            let (idx, rat, tb) = select_winner(&runs_detail);
            (Some(idx), rat, tb)
        } else {
            (None, "all_skipped".into(), false)
        };

    // ── Write winner patch ────────────────────────────────────────────────────
    let patch_out_path = args
        .output_patch
        .clone()
        .unwrap_or_else(|| best_of_dir.join("best.patch"));

    if let Some(winner_idx) = winner_run_index {
        let winner_patch = run_patch_texts
            .get(winner_idx as usize)
            .and_then(|p| p.as_deref());

        let patch_bytes = winner_patch.unwrap_or("").as_bytes();
        if let Some(parent) = patch_out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(Error::Io)?;
            }
        }
        std::fs::write(&patch_out_path, patch_bytes).map_err(Error::Io)?;
    }

    // ── Build results artifact ────────────────────────────────────────────────
    let results = BestOfResults {
        schema_version: ArtifactSchemaVersion::CURRENT,
        artifact_kind: ArtifactKind::BestOfResults,
        task: args.task.clone(),
        runs: args.runs,
        winner_run_index,
        passing_run_count,
        all_failed,
        tie_break_applied,
        selection_rationale,
        started_at,
        finished_at,
        runs_detail,
    };

    // ── Write best-of-results.json ────────────────────────────────────────────
    let results_path = best_of_dir.join("best-of-results.json");
    write_results(&results, &results_path)?;

    // ── Print summary ─────────────────────────────────────────────────────────
    if args.print_summary {
        print!("{}", results.summary_text());
        if winner_run_index.is_some() {
            eprintln!("best patch written to: {}", patch_out_path.display());
        }
    }

    // ── Exit code resolution ──────────────────────────────────────────────────
    // Budget halt if any runs were skipped
    if results.runs_detail.iter().any(|d| d.skipped) {
        return Ok(ExitCode::BudgetHalt);
    }

    if all_failed {
        if args.allow_no_pass {
            return Ok(ExitCode::Success);
        }
        return Ok(ExitCode::BestOfAllFailed);
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

/// Count the number of verify checks that passed vs total.
///
/// With verify checks present: counts individual check results from the
/// trajectory's `verification_results`. Without checks: falls back to
/// outcome-based scoring (submitted = 1/1).
fn count_verify_results(
    traj: &crate::trajectory::Trajectory,
    checks: &[crate::trajectory::VerificationCheck],
) -> (u32, u32) {
    if checks.is_empty() {
        // Fallback: treat outcome==submitted as 1/1
        let passed = traj.info.outcome.as_deref() == Some(crate::trajectory::outcome::SUBMITTED);
        return (u32::from(passed), 1);
    }

    let total = u32::try_from(checks.len()).unwrap_or(0);

    // Count passed checks from verification_results if available
    if !traj.info.verification_results.is_empty() {
        let passed = u32::try_from(
            traj.info
                .verification_results
                .iter()
                .filter(|r| r.passed)
                .count(),
        )
        .unwrap_or(0);
        return (passed, total);
    }

    // Fall back to binary: all passed or none
    let all_passed = traj.info.verification_status.as_deref()
        == Some(crate::trajectory::verification_status::VERIFIED);
    (if all_passed { total } else { 0 }, total)
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn write_results(results: &BestOfResults, path: &Path) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(results).map_err(Error::Json)?;
    std::fs::write(path, json).map_err(Error::Io)
}
