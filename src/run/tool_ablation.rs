//! `bench tool-ablation`: systematic per-tool removal ablation experiment.
//!
//! Generates one sweep arm per tool in the base config: a `baseline` arm that
//! runs the full tool set, plus one `no_<tool>` arm per ablated tool. Results
//! are collected in `tool-ablation.json` (schema `tool-ablation-1.0`) and a
//! ranked text summary ordered by `|delta_resolved_vs_baseline|`.

use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{ConfigError, Error};
use crate::model::ModelUsage;
use crate::run::dataset::DatasetSource;
use crate::run::swebench::{
    ApplySubsetParams, SWEEP_STATUS_CANCELLED, StratifyMode, SwebenchArgs, SweepResults,
    apply_subset, load_dataset_from_bytes_pub,
};

// ── public argument struct ────────────────────────────────────────────────────

#[allow(clippy::struct_excessive_bools)]
pub struct ToolAblationArgs {
    /// Path to the base config TOML. Tools are read from `agent.tools`.
    pub config_path: PathBuf,
    /// Dataset source for all arms.
    pub dataset_source: DatasetSource,
    /// Directory for the named-dataset cache (unused for local paths).
    pub dataset_cache_dir: PathBuf,
    /// Root output directory. Arm results land in `{output_dir}/{arm_name}/`.
    pub output_dir: PathBuf,
    /// Explicit tool names to ablate (empty = all user tools from config).
    pub ablate: Vec<String>,
    /// Shared USD ceiling across all arms.
    pub sweep_cost_limit_usd: Option<f64>,
    /// Number of arms to run concurrently.
    pub matrix_parallelism: usize,
    /// When true, skip `complete` and `skipped_budget` arms if
    /// `tool-ablation.json` already exists.
    pub resume: bool,
    /// Optional comma-separated instance ID filter applied before sampling.
    pub instance_ids: Option<String>,
    /// Keep at most N instances after filtering and sampling.
    pub limit: Option<usize>,
    /// Reproducibly random-subset to N instances (requires `seed`).
    pub sample: Option<usize>,
    /// RNG seed used by `sample`.
    pub seed: Option<u64>,
    /// Worker parallelism passed to each arm's sweep.
    pub parallel: usize,
    /// When true, add one arm per *pair* of removed tools (O(N²)).
    pub include_pair_ablation: bool,
    pub skip_preflight: bool,
    pub skip_model_probe: bool,
    /// Seconds each arm sweep waits for in-flight tasks after a cancel signal.
    pub cancel_deadline_secs: u64,
    /// Scripted model responses injected into every arm (tests / smoke checks).
    pub deterministic_responses: Option<Vec<String>>,
    /// Fixed token usage reported by the scripted backend (tests only).
    pub deterministic_usage_per_call: Option<ModelUsage>,
    /// Install OS signal handlers (disable in tests to avoid handler conflicts).
    pub install_os_signal_handlers: bool,
}

// ── arm plan ──────────────────────────────────────────────────────────────────

/// One planned ablation arm (pre-run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedArm {
    pub name: String,
    /// The single tool removed in this arm. `None` for the baseline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ablated_tool: Option<String>,
    /// The pair of tools removed (pair ablation only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ablated_pair: Option<(String, String)>,
}

/// Planned manifest returned by `--render-only`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmManifest {
    pub schema_version: String,
    pub config_path: String,
    pub arms: Vec<PlannedArm>,
}

// ── output report ─────────────────────────────────────────────────────────────

/// Per-arm result stored in `tool-ablation.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmAblationResult {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ablated_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ablated_pair: Option<(String, String)>,
    /// `"complete"`, `"skipped_budget"`, `"cancelled"`, or `"not_started"`.
    pub status: String,
    pub resolved: usize,
    pub errored: usize,
    pub total: usize,
    pub cost_usd: f64,
    pub step_mean: f64,
    pub step_p95: f64,
    /// Resolved-rate difference from baseline (positive = better than baseline).
    pub delta_resolved_vs_baseline: f64,
    /// Cost-per-resolved difference from baseline (negative = cheaper).
    pub delta_cost_per_resolve_vs_baseline: f64,
}

/// Written to `{output}/tool-ablation.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAblationReport {
    pub schema_version: String,
    pub config_path: String,
    pub generated_at: String,
    /// Deterministic instance list used by all arms.
    pub instance_ids: Vec<String>,
    pub arms: Vec<ArmAblationResult>,
}

// ── public API ────────────────────────────────────────────────────────────────

/// Return the list of user-defined tool names from the config (`agent.tools`).
pub fn enumerate_tools(cfg: &Config) -> Vec<String> {
    cfg.root
        .agent
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect()
}

/// Generate the ordered arm plan.
///
/// - Always includes a `baseline` arm (no tool removed).
/// - Adds one `no_<tool>` arm per tool to ablate.
/// - When `ablate` is non-empty, restricts the set to those named tools;
///   tools not found in the list are silently skipped.
/// - When `include_pairs` is true, adds one arm per pair (O(N²)).
pub fn generate_arm_plan(
    tools: &[String],
    ablate: &[String],
    include_pairs: bool,
) -> Vec<PlannedArm> {
    let to_ablate: Vec<&String> = if ablate.is_empty() {
        tools.iter().collect()
    } else {
        tools.iter().filter(|t| ablate.contains(t)).collect()
    };

    let mut arms = vec![PlannedArm {
        name: "baseline".into(),
        ablated_tool: None,
        ablated_pair: None,
    }];

    for tool in &to_ablate {
        arms.push(PlannedArm {
            name: format!("no_{tool}"),
            ablated_tool: Some((*tool).clone()),
            ablated_pair: None,
        });
    }

    if include_pairs {
        for i in 0..to_ablate.len() {
            for j in (i + 1)..to_ablate.len() {
                let t1 = (*to_ablate[i]).clone();
                let t2 = (*to_ablate[j]).clone();
                arms.push(PlannedArm {
                    name: format!("no_{t1}_and_{t2}"),
                    ablated_tool: None,
                    ablated_pair: Some((t1, t2)),
                });
            }
        }
    }

    arms
}

/// Render the planned arm manifest as a human-readable text table.
pub fn render_manifest_text(manifest: &ArmManifest) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== bench tool-ablation arm manifest ===");
    let _ = writeln!(out, "config:     {}", manifest.config_path);
    let _ = writeln!(out, "total arms: {}", manifest.arms.len());
    let _ = writeln!(out);

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["Arm", "Ablated Tool(s)"]);

    for arm in &manifest.arms {
        let ablated = if let Some(tool) = &arm.ablated_tool {
            tool.clone()
        } else if let Some((a, b)) = &arm.ablated_pair {
            format!("{a} + {b}")
        } else {
            "(baseline)".to_string()
        };
        table.add_row(vec![arm.name.clone(), ablated]);
    }

    let _ = write!(out, "{table}");
    out
}

/// Render the planned arm manifest as schema-versioned JSON.
pub fn render_manifest_json(manifest: &ArmManifest) -> Result<String, Error> {
    Ok(serde_json::to_string_pretty(manifest)?)
}

/// Render the completed report as a ranked text summary.
pub fn render_text_summary(report: &ToolAblationReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== bench tool-ablation summary ===");
    let _ = writeln!(out, "config:     {}", report.config_path);
    let _ = writeln!(out, "instances:  {}", report.instance_ids.len());
    let _ = writeln!(out);

    let mut arms = report.arms.clone();
    arms.sort_by(|a, b| {
        b.delta_resolved_vs_baseline
            .abs()
            .partial_cmp(&a.delta_resolved_vs_baseline.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "Arm",
            "Ablated Tool",
            "Status",
            "Resolved",
            "Cost($)",
            "Δresolved",
            "Δcost/resolve",
        ]);

    for arm in &arms {
        let ablated = arm.ablated_tool.as_deref().unwrap_or("(baseline)");
        table.add_row(vec![
            arm.name.clone(),
            ablated.to_string(),
            arm.status.clone(),
            arm.resolved.to_string(),
            format!("{:.4}", arm.cost_usd),
            format!("{:+.4}", arm.delta_resolved_vs_baseline),
            format!("{:+.4}", arm.delta_cost_per_resolve_vs_baseline),
        ]);
    }

    let _ = write!(out, "{table}");
    out
}

/// Run the full tool-ablation experiment and return a ranked report.
#[allow(clippy::too_many_lines)]
pub async fn run(args: ToolAblationArgs) -> Result<ToolAblationReport, Error> {
    let base_cfg = Config::load(&args.config_path).map_err(Error::Config)?;
    let all_tools = enumerate_tools(&base_cfg);

    // Validate --ablate names against what the config actually contains.
    for name in &args.ablate {
        if !all_tools.contains(name) {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "--ablate `{name}` is not a tool in the config; available: {}",
                if all_tools.is_empty() {
                    "(none)".to_string()
                } else {
                    all_tools.join(", ")
                }
            ))));
        }
    }

    let arm_plan = generate_arm_plan(&all_tools, &args.ablate, args.include_pair_ablation);

    if args.include_pair_ablation {
        let pair_count = arm_plan.iter().filter(|a| a.ablated_pair.is_some()).count();
        eprintln!(
            "bench tool-ablation: --include-pair-ablation adds {pair_count} pair arm(s) \
             (total {} arms); projected cost is O(N²) — confirm before proceeding",
            arm_plan.len()
        );
    }

    if args.matrix_parallelism == 0 {
        return Err(Error::Config(ConfigError::Invalid(
            "--matrix-parallelism must be at least 1".into(),
        )));
    }

    // Resolve the shared instance list once.
    let (dataset_bytes, _) =
        crate::run::dataset::resolve_dataset(&args.dataset_source, &args.dataset_cache_dir)?;
    let all_instances = load_dataset_from_bytes_pub(&dataset_bytes)?;
    let (selected_instances, _filter_spec) = apply_subset(
        all_instances,
        &ApplySubsetParams {
            instance_ids_arg: args.instance_ids.as_deref(),
            limit: args.limit,
            sample: args.sample,
            seed: args.seed,
            stratify_by: None,
            stratify_mode: StratifyMode::Proportional,
        },
    )?;
    let instance_ids: Vec<String> = selected_instances
        .iter()
        .map(|i| i.instance_id.clone())
        .collect();
    let instance_ids_csv = instance_ids.join(",");

    std::fs::create_dir_all(&args.output_dir)?;
    let report_path = args.output_dir.join("tool-ablation.json");

    // Resume: populate already-complete arms from the prior report so they are
    // skipped during execution and their costs count toward the budget ceiling.
    let mut arm_results: Vec<Option<ArmAblationResult>> = vec![None; arm_plan.len()];
    let mut cumulative_cost = 0.0f64;
    if args.resume && report_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&report_path) {
            if let Ok(prior) = serde_json::from_str::<ToolAblationReport>(&text) {
                for (idx, planned) in arm_plan.iter().enumerate() {
                    if let Some(prior_arm) = prior.arms.iter().find(|a| a.name == planned.name) {
                        if prior_arm.status == "complete" || prior_arm.status == "skipped_budget" {
                            cumulative_cost += prior_arm.cost_usd;
                            arm_results[idx] = Some(prior_arm.clone());
                        }
                    }
                }
            }
        }
    }

    // Shared context cloned into each arm task.
    let arm_ctx = AblationRunCtx {
        dataset_source: args.dataset_source.clone(),
        dataset_cache_dir: args.dataset_cache_dir.clone(),
        parallel: args.parallel,
        resume: args.resume,
        skip_preflight: args.skip_preflight,
        skip_model_probe: args.skip_model_probe,
        cancel_deadline_secs: args.cancel_deadline_secs,
        install_os_signal_handlers: args.install_os_signal_handlers,
        deterministic_responses: args.deterministic_responses.clone(),
        deterministic_usage_per_call: args.deterministic_usage_per_call.clone(),
    };

    let mut cancelled = false;
    let mut next_to_launch = 0usize;
    let mut join_set: tokio::task::JoinSet<(usize, Result<SweepResults, Error>)> =
        tokio::task::JoinSet::new();

    loop {
        // With a budget cap, launch one new arm per cycle so `cumulative_cost`
        // is updated between launches and the ceiling cannot be overrun by more
        // than one arm's spend.
        let fill_limit = if args.sweep_cost_limit_usd.is_some() {
            join_set.len().saturating_add(1)
        } else {
            args.matrix_parallelism
        };

        while !cancelled && join_set.len() < fill_limit {
            // Advance past arms already in a terminal state (resume or prior
            // budget-skip from this run).
            while next_to_launch < arm_plan.len() && arm_results[next_to_launch].is_some() {
                next_to_launch += 1;
            }
            if next_to_launch >= arm_plan.len() {
                break;
            }
            let i = next_to_launch;
            next_to_launch += 1;

            // Budget guard: mark this and all remaining pending arms skipped.
            if let Some(limit) = args.sweep_cost_limit_usd {
                if cumulative_cost >= limit {
                    for j in i..arm_plan.len() {
                        if arm_results[j].is_none() {
                            arm_results[j] = Some(make_skipped_budget(&arm_plan[j]));
                        }
                    }
                    break;
                }
            }

            let planned = arm_plan[i].clone();
            let arm_cfg = build_arm_config(&base_cfg, &planned);
            let arm_dir = args.output_dir.join(&planned.name);
            std::fs::create_dir_all(&arm_dir)?;
            // Pass the *remaining* budget so the underlying sweep can also halt
            // at the shared ceiling rather than only being stopped between arms.
            let remaining = args
                .sweep_cost_limit_usd
                .map(|l| (l - cumulative_cost).max(0.0));
            let ids_csv = instance_ids_csv.clone();
            let ctx = arm_ctx.clone();

            join_set.spawn(async move {
                (
                    i,
                    run_arm_ablation(arm_cfg, arm_dir, ids_csv, remaining, ctx).await,
                )
            });
        }

        // Nothing running and nothing queued → done.
        if join_set.is_empty() {
            break;
        }

        match join_set.join_next().await {
            None => break,
            Some(Err(join_err)) => {
                return Err(Error::Io(std::io::Error::other(format!(
                    "arm task panicked: {join_err}"
                ))));
            }
            Some(Ok((_, Err(e)))) => return Err(e),
            Some(Ok((arm_idx, Ok(results)))) => {
                cumulative_cost += results.estimated_cost_usd;

                // When an arm signals cancellation, stop launching new arms and
                // drain the already-in-flight ones before exiting.
                if results.sweep_status == SWEEP_STATUS_CANCELLED {
                    cancelled = true;
                }

                let status = if results.sweep_status == SWEEP_STATUS_CANCELLED {
                    "cancelled"
                } else {
                    "complete"
                };
                let resolved: usize = results
                    .instances
                    .iter()
                    .map(|r| r.resolved_count as usize)
                    .sum();
                let steps: Vec<f64> = results
                    .instances
                    .iter()
                    .filter_map(|r| r.steps.map(f64::from))
                    .collect();

                arm_results[arm_idx] = Some(ArmAblationResult {
                    name: arm_plan[arm_idx].name.clone(),
                    ablated_tool: arm_plan[arm_idx].ablated_tool.clone(),
                    ablated_pair: arm_plan[arm_idx].ablated_pair.clone(),
                    status: status.into(),
                    resolved,
                    errored: results.errored,
                    total: results.total,
                    cost_usd: results.estimated_cost_usd,
                    step_mean: mean_f64(&steps),
                    step_p95: percentile_f64(&steps, 0.95),
                    delta_resolved_vs_baseline: 0.0,
                    delta_cost_per_resolve_vs_baseline: 0.0,
                });
            }
        }
    }

    // Fill arms that were never launched because the run was cancelled.
    for (idx, result) in arm_results.iter_mut().enumerate() {
        if result.is_none() {
            *result = Some(ArmAblationResult {
                name: arm_plan[idx].name.clone(),
                ablated_tool: arm_plan[idx].ablated_tool.clone(),
                ablated_pair: arm_plan[idx].ablated_pair.clone(),
                status: "not_started".into(),
                resolved: 0,
                errored: 0,
                total: 0,
                cost_usd: 0.0,
                step_mean: 0.0,
                step_p95: 0.0,
                delta_resolved_vs_baseline: 0.0,
                delta_cost_per_resolve_vs_baseline: 0.0,
            });
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let mut all_results: Vec<ArmAblationResult> = arm_results.into_iter().flatten().collect();

    // Compute deltas relative to the baseline arm.  Arms that never ran keep
    // their zero deltas so skipped-budget entries are not misleadingly ranked.
    #[allow(clippy::cast_precision_loss)]
    let (baseline_rate, baseline_cpr) =
        all_results
            .iter()
            .find(|a| a.name == "baseline")
            .map_or((0.0, 0.0), |a| {
                let rate = if a.total > 0 {
                    a.resolved as f64 / a.total as f64
                } else {
                    0.0
                };
                let cpr = if a.resolved > 0 {
                    a.cost_usd / a.resolved as f64
                } else {
                    0.0
                };
                (rate, cpr)
            });

    #[allow(clippy::cast_precision_loss)]
    for arm in &mut all_results {
        // Preserve zero deltas for arms that produced no data.
        if arm.status == "skipped_budget" || arm.status == "not_started" {
            continue;
        }
        let rate = if arm.total > 0 {
            arm.resolved as f64 / arm.total as f64
        } else {
            0.0
        };
        let cpr = if arm.resolved > 0 {
            arm.cost_usd / arm.resolved as f64
        } else {
            0.0
        };
        arm.delta_resolved_vs_baseline = rate - baseline_rate;
        arm.delta_cost_per_resolve_vs_baseline = cpr - baseline_cpr;
    }

    let report = ToolAblationReport {
        schema_version: "tool-ablation-1.0".into(),
        config_path: args.config_path.display().to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        instance_ids,
        arms: all_results,
    };

    let json = serde_json::to_string_pretty(&report)?;
    atomic_write(&args.output_dir.join("tool-ablation.json"), json.as_bytes())?;

    let summary_txt = render_text_summary(&report);
    atomic_write(
        &args.output_dir.join("tool-ablation-summary.txt"),
        summary_txt.as_bytes(),
    )?;

    Ok(report)
}

// ── private helpers ───────────────────────────────────────────────────────────

/// Shared context cloned into each concurrent arm task.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
struct AblationRunCtx {
    dataset_source: DatasetSource,
    dataset_cache_dir: PathBuf,
    parallel: usize,
    resume: bool,
    skip_preflight: bool,
    skip_model_probe: bool,
    cancel_deadline_secs: u64,
    install_os_signal_handlers: bool,
    deterministic_responses: Option<Vec<String>>,
    deterministic_usage_per_call: Option<ModelUsage>,
}

/// Run one arm sweep and return raw `SweepResults`.
async fn run_arm_ablation(
    arm_cfg: Config,
    arm_dir: PathBuf,
    instance_ids_csv: String,
    cost_limit_usd: Option<f64>,
    ctx: AblationRunCtx,
) -> Result<SweepResults, Error> {
    let sweep_args = SwebenchArgs {
        dataset_source: ctx.dataset_source,
        dataset_cache_dir: ctx.dataset_cache_dir,
        output_dir: arm_dir,
        parallel: ctx.parallel,
        config: arm_cfg,
        reruns: 1,
        resume: ctx.resume,
        cost_limit_usd,
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
    crate::run::swebench::run(sweep_args).await
}

/// Construct a zero-cost `skipped_budget` result for `planned`.
fn make_skipped_budget(planned: &PlannedArm) -> ArmAblationResult {
    ArmAblationResult {
        name: planned.name.clone(),
        ablated_tool: planned.ablated_tool.clone(),
        ablated_pair: planned.ablated_pair.clone(),
        status: "skipped_budget".into(),
        resolved: 0,
        errored: 0,
        total: 0,
        cost_usd: 0.0,
        step_mean: 0.0,
        step_p95: 0.0,
        delta_resolved_vs_baseline: 0.0,
        delta_cost_per_resolve_vs_baseline: 0.0,
    }
}

/// Clone `base_cfg` and remove any tools specified by `arm`.
fn build_arm_config(base_cfg: &Config, arm: &PlannedArm) -> Config {
    let mut cfg = base_cfg.clone();

    let remove: Vec<String> = match (&arm.ablated_tool, &arm.ablated_pair) {
        (Some(t), _) => vec![t.clone()],
        (None, Some((t1, t2))) => vec![t1.clone(), t2.clone()],
        (None, None) => vec![],
    };

    if remove.is_empty() {
        return cfg;
    }

    // Remove from typed struct.
    cfg.root.agent.tools.retain(|t| !remove.contains(&t.name));

    // Mirror the removal in the raw JSON so template rendering is consistent.
    if let Some(agent) = cfg.raw.get_mut("agent") {
        if let Some(tools) = agent.get_mut("tools") {
            if let Some(arr) = tools.as_array_mut() {
                arr.retain(|t| {
                    let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    !remove.iter().any(|r| r == name)
                });
            }
        }
    }

    cfg
}

#[allow(clippy::cast_precision_loss)]
fn mean_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
fn percentile_f64(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<(), Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
