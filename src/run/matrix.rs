//! `bench matrix`: run multiple sweep arms against the same instance set.
//!
//! Each arm in the TOML manifest is a distinct (model, config) pair. All arms
//! run against the same deterministically-selected instances so results are
//! directly comparable. A shared budget ceiling is enforced: once cumulative
//! cost across completed arms meets the limit, remaining arms are recorded as
//! `skipped_budget`.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{ConfigError, Error};
use crate::model::ModelUsage;
use crate::run::dataset::DatasetSource;
use crate::run::swebench::{
    ApplySubsetParams, FilterSpec, SWEEP_STATUS_CANCELLED, StratifyBy, StratifyMode, apply_subset,
    load_dataset_from_bytes_pub,
};

// ── TOML manifest ─────────────────────────────────────────────────────────────

/// Top-level matrix manifest parsed from the TOML config file.
#[derive(Debug, Clone, Deserialize)]
pub struct MatrixManifest {
    /// The `[[arm]]` array of tables from the TOML file.
    #[serde(rename = "arm", default)]
    pub arms: Vec<ArmDef>,
}

/// One arm definition in the matrix manifest.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ArmDef {
    pub name: String,
    pub model: String,
    pub step_limit: Option<u32>,
    pub per_task_budget_usd: Option<f64>,
    pub prompt_file: Option<PathBuf>,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

// ── Runtime args ──────────────────────────────────────────────────────────────

#[allow(clippy::struct_excessive_bools)]
pub struct MatrixArgs {
    /// Path to the TOML matrix manifest.
    pub config_path: PathBuf,
    /// Dataset source for all arms.
    pub dataset_source: DatasetSource,
    /// Directory for the named-dataset cache (unused for local paths).
    pub dataset_cache_dir: PathBuf,
    /// Root output directory. Arm results land in `{output_dir}/{arm_name}/`.
    pub output_dir: PathBuf,
    /// Optional comma-separated instance ID filter applied before sampling.
    pub instance_ids: Option<String>,
    /// Keep at most N instances after filtering and sampling.
    pub limit: Option<usize>,
    /// Reproducibly random-subset to N instances (requires `seed`).
    pub sample: Option<usize>,
    /// RNG seed used by `sample`.
    pub seed: Option<u64>,
    /// Optional stratification key used during sampling.
    pub stratify_by: Option<StratifyBy>,
    /// Allocation mode used with stratification.
    pub stratify_mode: StratifyMode,
    /// Shared USD ceiling across all arms. Arms that would start after the
    /// limit is reached are recorded as `skipped_budget`.
    pub sweep_cost_limit_usd: Option<f64>,
    /// Number of arms to run concurrently (default: 1 = sequential).
    pub matrix_parallelism: usize,
    /// When true, load existing `matrix.json` and skip `complete` arms.
    pub resume: bool,
    /// Worker parallelism passed to each arm's sweep.
    pub parallel: usize,
    pub skip_preflight: bool,
    pub skip_model_probe: bool,
    /// Scripted model responses injected into every arm (tests / smoke checks).
    pub deterministic_responses: Option<Vec<String>>,
    /// Fixed token usage reported by the scripted backend (tests only).
    pub deterministic_usage_per_call: Option<ModelUsage>,
    /// Seconds each arm sweep waits for in-flight tasks after a cancel signal.
    pub cancel_deadline_secs: u64,
    /// Install OS signal handlers (disable in tests to avoid handler conflicts).
    pub install_os_signal_handlers: bool,
}

// ── Internal state (matrix.json) ─────────────────────────────────────────────

/// Persisted state of a single arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmState {
    Pending,
    Running,
    Complete,
    SkippedBudget,
    NotStarted,
    Cancelled,
}

impl ArmState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Complete => "complete",
            Self::SkippedBudget => "skipped_budget",
            Self::NotStarted => "not_started",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArmStatus {
    name: String,
    model: String,
    state: ArmState,
    sweep_dir: String,
    total_cost_usd: f64,
    resolved: usize,
    submitted: usize,
}

/// Written to `{output}/matrix.json` before any arm runs, updated after each.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MatrixState {
    artifact_kind: String,
    config_path: String,
    instance_ids: Vec<String>,
    filter_spec: FilterSpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_limit_usd: Option<f64>,
    arms: Vec<ArmStatus>,
}

// ── Output summary ────────────────────────────────────────────────────────────

/// One row in the ranked matrix summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmSummaryRow {
    pub rank: usize,
    pub name: String,
    pub model: String,
    pub state: String,
    pub resolved: usize,
    pub resolved_rate: f64,
    pub total_cost_usd: f64,
    pub cost_per_resolved_usd: f64,
    /// Resolved-rate difference from rank-1 arm, in percentage points.
    pub delta_resolved_rate_pp: f64,
    pub delta_cost_per_resolved_usd: f64,
}

/// Returned by `run`; also written to `{output}/matrix-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixSummary {
    pub arms: Vec<ArmSummaryRow>,
}

// ── Private arm-run context ───────────────────────────────────────────────────

/// Everything `run_arm` needs; cloneable so concurrent tasks can own their copy.
#[derive(Clone)]
struct ArmRunCtx {
    dataset_source: DatasetSource,
    dataset_cache_dir: PathBuf,
    parallel: usize,
    skip_preflight: bool,
    skip_model_probe: bool,
    cancel_deadline_secs: u64,
    install_os_signal_handlers: bool,
    deterministic_responses: Option<Vec<String>>,
    deterministic_usage_per_call: Option<ModelUsage>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Validate arm definitions before running.
///
/// Rules: at least one arm, all names non-empty, no path separators, unique.
pub fn validate_arms(arms: &[ArmDef]) -> Result<(), Error> {
    if arms.is_empty() {
        return Err(Error::Config(ConfigError::Invalid(
            "matrix manifest must have at least one arm".into(),
        )));
    }

    for arm in arms {
        if arm.name.is_empty() {
            return Err(Error::Config(ConfigError::Invalid(
                "arm name cannot be empty".into(),
            )));
        }
        if arm.name.contains('/') || arm.name.contains('\\') || arm.name.contains("..") {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "arm name {:?} must not contain path separators or `..`",
                arm.name
            ))));
        }
    }

    let mut seen: HashSet<&str> = HashSet::new();
    for arm in arms {
        if !seen.insert(arm.name.as_str()) {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "duplicate arm name: {:?}",
                arm.name
            ))));
        }
    }

    Ok(())
}

/// Run the full matrix experiment and return a ranked summary.
#[allow(clippy::too_many_lines)]
pub async fn run(args: MatrixArgs) -> Result<MatrixSummary, Error> {
    // Load and validate manifest.
    let manifest_text = std::fs::read_to_string(&args.config_path)?;
    let manifest: MatrixManifest = toml::from_str(&manifest_text)
        .map_err(|e| Error::Config(ConfigError::Invalid(format!("matrix manifest: {e}"))))?;
    validate_arms(&manifest.arms)?;

    // Resolve the shared instance list once (before any arm runs).
    let (dataset_bytes, _dataset_meta) =
        crate::run::dataset::resolve_dataset(&args.dataset_source, &args.dataset_cache_dir)?;
    let all_instances = load_dataset_from_bytes_pub(&dataset_bytes)?;
    let (selected_instances, filter_spec) = apply_subset(
        all_instances,
        &ApplySubsetParams {
            instance_ids_arg: args.instance_ids.as_deref(),
            limit: args.limit,
            sample: args.sample,
            seed: args.seed,
            stratify_by: args.stratify_by,
            stratify_mode: args.stratify_mode,
        },
    )?;

    let instance_ids: Vec<String> = selected_instances
        .iter()
        .map(|i| i.instance_id.clone())
        .collect();

    if args.matrix_parallelism == 0 {
        return Err(Error::Config(ConfigError::Invalid(
            "--matrix-parallelism must be at least 1".into(),
        )));
    }

    // With parallelism > 1 and a budget cap, up to `matrix_parallelism` arms
    // can be in-flight simultaneously against the same pre-completion
    // `cumulative_cost`, so the ceiling may be overrun by that many arms before
    // any completion updates the running total.  The effective overshoot is
    // bounded to one arm's cost per slot, not per sweep.
    if args.matrix_parallelism > 1 && args.sweep_cost_limit_usd.is_some() {
        tracing::warn!(
            matrix_parallelism = args.matrix_parallelism,
            "bench matrix: --sweep-cost-limit-usd with --matrix-parallelism > 1 \
             enforces the shared budget against completed-arm costs only; \
             up to {} arms may be in flight simultaneously before the ceiling \
             is re-checked",
            args.matrix_parallelism,
        );
    }

    std::fs::create_dir_all(&args.output_dir)?;
    let state_path = args.output_dir.join("matrix.json");

    // Load or create matrix state.
    let mut state = if args.resume && state_path.exists() {
        let text = std::fs::read_to_string(&state_path)?;
        let loaded: MatrixState = serde_json::from_str(&text)?;
        // Warn when the resolved instance set differs from the persisted one.
        if loaded.instance_ids != instance_ids {
            tracing::warn!(
                persisted = loaded.instance_ids.len(),
                resolved = instance_ids.len(),
                "resume: resolved instance set differs from persisted matrix.json; \
                 using persisted list to maintain arm consistency"
            );
        }
        loaded
    } else {
        MatrixState {
            artifact_kind: "matrix".into(),
            config_path: args.config_path.display().to_string(),
            instance_ids: instance_ids.clone(),
            filter_spec,
            cost_limit_usd: args.sweep_cost_limit_usd,
            arms: manifest
                .arms
                .iter()
                .map(|arm| ArmStatus {
                    name: arm.name.clone(),
                    model: arm.model.clone(),
                    state: ArmState::Pending,
                    sweep_dir: args.output_dir.join(&arm.name).display().to_string(),
                    total_cost_usd: 0.0,
                    resolved: 0,
                    submitted: 0,
                })
                .collect(),
        }
    };
    write_matrix_state(&state_path, &state)?;

    // Derive the arm instance list from the authoritative state (persisted on
    // resume, freshly resolved on a new run) so all arms see a consistent workload.
    let instance_ids_csv = state.instance_ids.join(",");

    // Cumulative cost starts from arms already complete (resume scenario).
    let mut cumulative_cost: f64 = state
        .arms
        .iter()
        .filter(|a| a.state == ArmState::Complete)
        .map(|a| a.total_cost_usd)
        .sum();

    let ctx = ArmRunCtx {
        dataset_source: args.dataset_source.clone(),
        dataset_cache_dir: args.dataset_cache_dir.clone(),
        parallel: args.parallel,
        skip_preflight: args.skip_preflight,
        skip_model_probe: args.skip_model_probe,
        cancel_deadline_secs: args.cancel_deadline_secs,
        install_os_signal_handlers: args.install_os_signal_handlers,
        deterministic_responses: args.deterministic_responses.clone(),
        deterministic_usage_per_call: args.deterministic_usage_per_call.clone(),
    };

    // Run arms with up to `matrix_parallelism` concurrent arm sweeps.
    //
    // Each arm task owns its data (cloned from the manifest), so the tasks are
    // `'static` and safe to spawn into a `JoinSet`.  For N=1 the loop is
    // effectively sequential; N>1 fills `N` slots concurrently then refills as
    // completions arrive.  Ctrl-C is handled by each arm's own OS-signal
    // machinery: when an arm returns with `sweep_status == "cancelled"` the
    // matrix stops launching new arms, drains any already-in-flight ones, and
    // marks the remaining pending arms `not_started`.
    let mut next_to_launch = 0usize;
    let mut cancelled = false;
    let mut join_set: tokio::task::JoinSet<(
        usize,
        Result<crate::run::swebench::SweepResults, Error>,
    )> = tokio::task::JoinSet::new();

    loop {
        // Fill available parallelism slots with new arms.
        // When a budget cap is active, only launch one new arm per cycle so
        // that `cumulative_cost` is updated between launches and the ceiling
        // is not overrun by more than one arm's cost.
        let fill_limit = if args.sweep_cost_limit_usd.is_some() {
            join_set.len().saturating_add(1)
        } else {
            args.matrix_parallelism
        };
        while !cancelled && join_set.len() < fill_limit {
            // Advance past arms already in a terminal state (resume or prior iteration).
            while next_to_launch < manifest.arms.len()
                && matches!(
                    state.arms[next_to_launch].state,
                    ArmState::Complete | ArmState::SkippedBudget
                )
            {
                next_to_launch += 1;
            }
            if next_to_launch >= manifest.arms.len() {
                break;
            }
            let i = next_to_launch;
            next_to_launch += 1;

            // Budget guard: mark this arm and all remaining pending arms skipped.
            if let Some(limit) = args.sweep_cost_limit_usd {
                if cumulative_cost >= limit {
                    for j in i..manifest.arms.len() {
                        if state.arms[j].state == ArmState::Pending {
                            state.arms[j].state = ArmState::SkippedBudget;
                        }
                    }
                    write_matrix_state(&state_path, &state)?;
                    break;
                }
            }

            let arm_def = manifest.arms[i].clone();
            let arm_sweep_dir = args.output_dir.join(&arm_def.name);
            std::fs::create_dir_all(&arm_sweep_dir)?;

            state.arms[i].state = ArmState::Running;
            write_matrix_state(&state_path, &state)?;

            let ids_csv = instance_ids_csv.clone();
            let ctx_clone = ctx.clone();
            join_set.spawn(async move {
                (i, run_arm(arm_def, arm_sweep_dir, ids_csv, ctx_clone).await)
            });
        }

        // Nothing running and nothing queued → done.
        if join_set.is_empty() {
            break;
        }

        // Wait for the next arm to finish.
        match join_set.join_next().await {
            None => break,
            Some(Err(join_err)) => {
                return Err(Error::Io(std::io::Error::other(format!(
                    "arm task panicked: {join_err}"
                ))));
            }
            Some(Ok((_arm_idx, Err(e)))) => return Err(e),
            Some(Ok((arm_idx, Ok(results)))) => {
                let resolved: usize = results
                    .instances
                    .iter()
                    .map(|r| r.resolved_count as usize)
                    .sum();
                cumulative_cost += results.estimated_cost_usd;

                // An arm reports "cancelled" when it received a Ctrl-C / SIGTERM.
                // Stop launching new arms; let already-in-flight ones drain.
                if cancelled || results.sweep_status == SWEEP_STATUS_CANCELLED {
                    state.arms[arm_idx].state = ArmState::Cancelled;
                    if results.sweep_status == SWEEP_STATUS_CANCELLED {
                        cancelled = true;
                    }
                } else {
                    state.arms[arm_idx].state = ArmState::Complete;
                }
                state.arms[arm_idx].total_cost_usd = results.estimated_cost_usd;
                state.arms[arm_idx].submitted = results.submitted;
                state.arms[arm_idx].resolved = resolved;
                write_matrix_state(&state_path, &state)?;
            }
        }
    }

    // Mark arms that were queued but never launched as not_started.
    if cancelled {
        for arm_status in &mut state.arms {
            if arm_status.state == ArmState::Pending {
                arm_status.state = ArmState::NotStarted;
            }
        }
        write_matrix_state(&state_path, &state)?;
    }

    // Build and persist summary.
    let summary = build_summary(&state);

    let summary_json = serde_json::to_string_pretty(&summary)?;
    atomic_write(
        &args.output_dir.join("matrix-summary.json"),
        summary_json.as_bytes(),
    )?;

    let summary_txt = render_summary_text(&summary, &state);
    atomic_write(
        &args.output_dir.join("matrix-summary.txt"),
        summary_txt.as_bytes(),
    )?;

    Ok(summary)
}

// ── Private helpers ───────────────────────────────────────────────────────────

async fn run_arm(
    arm: ArmDef,
    arm_sweep_dir: PathBuf,
    instance_ids_csv: String,
    ctx: ArmRunCtx,
) -> Result<crate::run::swebench::SweepResults, Error> {
    // Load prompt_file first so explicit arm-manifest fields override it.
    let mut cfg = if let Some(ref prompt_file) = arm.prompt_file {
        Config::load(prompt_file).map_err(Error::Config)?
    } else {
        Config::defaults().map_err(Error::Config)?
    };
    // Arm manifest values take precedence over anything in prompt_file.
    cfg.root.model.name.clone_from(&arm.model);
    if let Some(step_limit) = arm.step_limit {
        cfg.root.agent.step_limit = step_limit;
    }
    if let Some(budget) = arm.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(budget);
    }

    let arm_args = crate::run::swebench::SwebenchArgs {
        dataset_source: ctx.dataset_source,
        dataset_cache_dir: ctx.dataset_cache_dir,
        output_dir: arm_sweep_dir,
        parallel: ctx.parallel,
        config: cfg,
        reruns: 1,
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: Some(instance_ids_csv),
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 1000,
        retry_backoff_cap_s: 60,
        retry_on_resume: false,
        deterministic_responses: ctx.deterministic_responses,
        deterministic_usage_per_call: ctx.deterministic_usage_per_call,
        config_overlay_paths: vec![],
        dry_run: false,
        skip_preflight: ctx.skip_preflight,
        preflight_format: "text".into(),
        skip_model_probe: ctx.skip_model_probe,
        preflight_check_timeout_s: 30,
        preflight_total_timeout_s: 120,
        preflight_mode: "sweep".into(),
        skip_patch_validation: false,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: ctx.cancel_deadline_secs,
        install_os_signal_handlers: ctx.install_os_signal_handlers,
        cancellation_signals: None,
        github_pr: None,
        reproduced_from: None,
        abort_on_systemic_failure: false,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
    };

    crate::run::swebench::run(arm_args).await
}

#[allow(clippy::cast_precision_loss)]
fn build_summary(state: &MatrixState) -> MatrixSummary {
    let n = state.instance_ids.len();

    let mut rows: Vec<ArmSummaryRow> = state
        .arms
        .iter()
        .map(|arm| {
            let resolved_rate = if n > 0 {
                arm.resolved as f64 / n as f64
            } else {
                0.0
            };
            let cost_per_resolved = if arm.resolved > 0 {
                arm.total_cost_usd / arm.resolved as f64
            } else {
                0.0
            };
            ArmSummaryRow {
                rank: 0, // assigned after sort
                name: arm.name.clone(),
                model: arm.model.clone(),
                state: arm.state.as_str().to_owned(),
                resolved: arm.resolved,
                resolved_rate,
                total_cost_usd: arm.total_cost_usd,
                cost_per_resolved_usd: cost_per_resolved,
                delta_resolved_rate_pp: 0.0,
                delta_cost_per_resolved_usd: 0.0,
            }
        })
        .collect();

    // Rank: complete arms by resolved_rate descending, then by lower cost.
    // Non-complete arms go after all complete arms.
    rows.sort_by(|a, b| {
        let a_complete = a.state == "complete";
        let b_complete = b.state == "complete";
        match (a_complete, b_complete) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b
                .resolved_rate
                .partial_cmp(&a.resolved_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    a.total_cost_usd
                        .partial_cmp(&b.total_cost_usd)
                        .unwrap_or(std::cmp::Ordering::Equal),
                ),
        }
    });

    // Assign contiguous ranks starting at 1.
    for (i, row) in rows.iter_mut().enumerate() {
        row.rank = i + 1;
    }

    // Compute deltas relative to rank-1 arm.
    if let Some(best) = rows.first().cloned() {
        for row in rows.iter_mut().skip(1) {
            row.delta_resolved_rate_pp = (row.resolved_rate - best.resolved_rate) * 100.0;
            row.delta_cost_per_resolved_usd =
                row.cost_per_resolved_usd - best.cost_per_resolved_usd;
        }
    }

    MatrixSummary { arms: rows }
}

fn render_summary_text(summary: &MatrixSummary, state: &MatrixState) -> String {
    let n = state.instance_ids.len();
    let mut s = String::from("=== bench matrix summary ===\n\n");
    let _ = writeln!(
        s,
        "{:<4} {:<24} {:<24} {:<14} {:>8} {:>10} {:>10}",
        "Rank", "Name", "Model", "State", "Resolved", "Rate%", "Cost($)"
    );
    s.push_str(&"-".repeat(100));
    s.push('\n');

    for row in &summary.arms {
        let rate_pct = if n > 0 {
            format!("{:.1}", row.resolved_rate * 100.0)
        } else {
            "N/A".into()
        };
        let _ = writeln!(
            s,
            "{:<4} {:<24} {:<24} {:<14} {:>8} {:>10} {:>10.4}",
            row.rank, row.name, row.model, row.state, row.resolved, rate_pct, row.total_cost_usd
        );
    }
    s
}

fn write_matrix_state(path: &Path, state: &MatrixState) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(state)?;
    atomic_write(path, json.as_bytes())
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<(), Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
