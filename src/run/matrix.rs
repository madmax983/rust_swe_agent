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
    ApplySubsetParams, FilterSpec, StratifyBy, StratifyMode, apply_subset,
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
    let instance_ids_csv = instance_ids.join(",");

    std::fs::create_dir_all(&args.output_dir)?;
    let state_path = args.output_dir.join("matrix.json");

    // Load or create matrix state.
    let mut state = if args.resume && state_path.exists() {
        let text = std::fs::read_to_string(&state_path)?;
        serde_json::from_str::<MatrixState>(&text)?
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

    // Cumulative cost starts from arms already complete (resume scenario).
    let mut cumulative_cost: f64 = state
        .arms
        .iter()
        .filter(|a| a.state == ArmState::Complete)
        .map(|a| a.total_cost_usd)
        .sum();

    for i in 0..manifest.arms.len() {
        // Skip arms that are already in a terminal state.
        match state.arms[i].state {
            ArmState::Complete | ArmState::SkippedBudget => continue,
            _ => {}
        }

        // Budget guard: mark remaining arms skipped when limit is exhausted.
        if let Some(limit) = args.sweep_cost_limit_usd {
            if cumulative_cost >= limit {
                state.arms[i].state = ArmState::SkippedBudget;
                write_matrix_state(&state_path, &state)?;
                continue;
            }
        }

        let arm_def = &manifest.arms[i];
        let arm_sweep_dir = args.output_dir.join(&arm_def.name);
        std::fs::create_dir_all(&arm_sweep_dir)?;

        state.arms[i].state = ArmState::Running;
        write_matrix_state(&state_path, &state)?;

        let arm_results = run_arm(arm_def, &arm_sweep_dir, &instance_ids_csv, &args).await?;

        let resolved: usize = arm_results
            .instances
            .iter()
            .map(|r| r.resolved_count as usize)
            .sum();

        cumulative_cost += arm_results.estimated_cost_usd;
        state.arms[i].state = ArmState::Complete;
        state.arms[i].total_cost_usd = arm_results.estimated_cost_usd;
        state.arms[i].submitted = arm_results.submitted;
        state.arms[i].resolved = resolved;

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
    arm: &ArmDef,
    arm_sweep_dir: &Path,
    instance_ids_csv: &str,
    matrix_args: &MatrixArgs,
) -> Result<crate::run::swebench::SweepResults, Error> {
    let mut cfg = Config::defaults().map_err(Error::Config)?;
    cfg.root.model.name.clone_from(&arm.model);
    cfg.root.agent.step_limit = arm.step_limit.unwrap_or(cfg.root.agent.step_limit);
    if let Some(budget) = arm.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(budget);
    }
    if let Some(ref prompt_file) = arm.prompt_file {
        let overlay = Config::load(prompt_file).map_err(Error::Config)?;
        cfg.root.agent = overlay.root.agent;
        cfg.root.model = overlay.root.model;
    }

    let arm_args = crate::run::swebench::SwebenchArgs {
        dataset_source: matrix_args.dataset_source.clone(),
        dataset_cache_dir: matrix_args.dataset_cache_dir.clone(),
        output_dir: arm_sweep_dir.to_path_buf(),
        parallel: matrix_args.parallel,
        config: cfg,
        reruns: 1,
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: Some(instance_ids_csv.to_owned()),
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
        deterministic_responses: matrix_args.deterministic_responses.clone(),
        deterministic_usage_per_call: matrix_args.deterministic_usage_per_call.clone(),
        config_overlay_paths: vec![],
        dry_run: false,
        skip_preflight: matrix_args.skip_preflight,
        preflight_format: "text".into(),
        skip_model_probe: matrix_args.skip_model_probe,
        preflight_check_timeout_s: 30,
        preflight_total_timeout_s: 120,
        preflight_mode: "sweep".into(),
        skip_patch_validation: false,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: matrix_args.cancel_deadline_secs,
        install_os_signal_handlers: matrix_args.install_os_signal_handlers,
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
