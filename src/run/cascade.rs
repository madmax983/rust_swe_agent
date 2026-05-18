//! `bench cascade`: cost-optimized model-tier routing per instance.
//!
//! Each instance is routed through an ordered list of model tiers and
//! short-circuited on the first tier that resolves it. Premium-tier dollars
//! are only spent on instances that demonstrably need premium capability.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{ConfigError, Error};
use crate::model::ModelUsage;
use crate::run::dataset::DatasetSource;
use crate::run::evaluate::{BreakdownSelection, EvaluateArgs, EvaluateBackend};
use crate::run::swebench::{
    ApplySubsetParams, FilterSpec, StratifyBy, StratifyMode, apply_subset,
    load_dataset_from_bytes_pub,
};

// ── TOML manifest ─────────────────────────────────────────────────────────────

/// Top-level cascade manifest parsed from the TOML config file.
#[derive(Debug, Clone, Deserialize)]
pub struct CascadeManifest {
    /// The `[[tier]]` array of tables from the TOML file.
    #[serde(rename = "tier", default)]
    pub tiers: Vec<TierDef>,
}

/// One tier definition in the cascade manifest.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TierDef {
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
pub struct CascadeArgs {
    /// Path to the TOML cascade manifest.
    pub config_path: PathBuf,
    /// Dataset source for all tiers.
    pub dataset_source: DatasetSource,
    /// Directory for the named-dataset cache (unused for local paths).
    pub dataset_cache_dir: PathBuf,
    /// Root output directory. Tier results land in `{output_dir}/tier-{name}/`.
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
    /// Shared USD ceiling across ALL tiers. Instances reached after the limit
    /// is exhausted are recorded as `skipped_budget`.
    pub sweep_cost_limit_usd: Option<f64>,
    /// When true, load existing `cascade.json` and skip completed instances.
    pub resume: bool,
    /// Worker parallelism passed to each tier's sweep.
    pub parallel: usize,
    pub skip_preflight: bool,
    pub skip_model_probe: bool,
    /// Evaluation backend that gates per-instance short-circuit.
    /// REQUIRED (must not be None in production) — fails preflight if None
    /// and `mock_eval_resolved_ids` is also None.
    pub eval_backend: EvaluateBackend,
    /// SWE-bench subset passed to the evaluator (e.g. `"swe-bench-m"`).
    pub sb_subset: String,
    /// SWE-bench split passed to the evaluator (e.g. `"test"`).
    pub sb_split: String,
    /// Scripted model responses injected into every tier (tests / smoke checks).
    pub deterministic_responses: Option<Vec<String>>,
    /// Fixed token usage reported by the scripted backend (tests only).
    pub deterministic_usage_per_call: Option<ModelUsage>,
    /// Per-instance evaluator timeout in seconds (passed to `bench evaluate`).
    pub eval_timeout_per_instance_secs: u64,
    /// Seconds each tier sweep waits for in-flight tasks after a cancel signal.
    pub cancel_deadline_secs: u64,
    /// Install OS signal handlers (disable in tests to avoid handler conflicts).
    pub install_os_signal_handlers: bool,
    /// Test override: resolved instance IDs per tier (index = tier index).
    /// When Some, bypasses the evaluation backend — for unit/integration tests only.
    pub mock_eval_resolved_ids: Option<Vec<HashSet<String>>>,
}

// ── Per-instance tier attempt ─────────────────────────────────────────────────

/// One tier's attempt record for a single instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierAttempt {
    pub tier_name: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_exit_reason: Option<String>,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub halted_reason: Option<String>,
}

/// Complete cascade record for a single instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceCascadeRecord {
    pub resolving_tier: Option<String>,
    pub total_cost_usd: f64,
    pub attempts: Vec<TierAttempt>,
}

// ── cascade.json (state + artifact) ──────────────────────────────────────────

/// Written to `{output}/cascade.json`; updated after each tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeState {
    pub artifact_kind: String,
    pub config_path: String,
    pub instance_ids: Vec<String>,
    pub filter_spec: FilterSpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_limit_usd: Option<f64>,
    pub instances: BTreeMap<String, InstanceCascadeRecord>,
}

// ── cascade-summary.json ──────────────────────────────────────────────────────

/// Per-tier row in the cascade summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierSummaryRow {
    pub name: String,
    pub model: String,
    pub instances_attempted: usize,
    pub resolved: usize,
    pub resolved_rate: f64,
    pub total_cost_usd: f64,
    pub mean_cost_per_attempt_usd: f64,
    pub cost_per_resolved_usd: f64,
}

/// Returned by `run`; also written to `{output}/cascade-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeSummary {
    pub artifact_kind: String,
    pub tiers: Vec<TierSummaryRow>,
    pub cascade_resolved: usize,
    pub cascade_resolved_rate: f64,
    pub total_instances: usize,
    pub total_cost_usd: f64,
    pub cost_per_resolved_cascade_usd: f64,
    pub savings_vs_top_tier_only_usd: f64,
}

// ── Validation ────────────────────────────────────────────────────────────────

/// Validate tier definitions before running.
///
/// Rules: at least one tier, all names non-empty, no path separators, unique.
pub fn validate_tiers(tiers: &[TierDef]) -> Result<(), Error> {
    if tiers.is_empty() {
        return Err(Error::Config(ConfigError::Invalid(
            "cascade manifest must have at least one tier".into(),
        )));
    }

    for tier in tiers {
        if tier.name.is_empty() {
            return Err(Error::Config(ConfigError::Invalid(
                "tier name cannot be empty".into(),
            )));
        }
        if tier.name.contains('/') || tier.name.contains('\\') || tier.name.contains("..") {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "tier name {:?} must not contain path separators or `..`",
                tier.name
            ))));
        }
    }

    let mut seen: HashSet<&str> = HashSet::new();
    for tier in tiers {
        if !seen.insert(tier.name.as_str()) {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "duplicate tier name: {:?}",
                tier.name
            ))));
        }
    }

    Ok(())
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Run the full cascade experiment and return a summary.
#[allow(clippy::too_many_lines)]
pub async fn run(args: CascadeArgs) -> Result<CascadeSummary, Error> {
    // Preflight: cascade requires an evaluation backend (or test mock).
    if args.eval_backend == EvaluateBackend::None && args.mock_eval_resolved_ids.is_none() {
        return Err(Error::Config(ConfigError::Invalid(
            "bench cascade requires an evaluation backend (--eval-backend sb-cli or compatible); \
             the evaluator is the gating signal for per-instance short-circuit. \
             Configure an evaluation backend before running cascade."
                .into(),
        )));
    }

    // Load and validate manifest.
    let manifest_text = std::fs::read_to_string(&args.config_path)?;
    let manifest: CascadeManifest = toml::from_str(&manifest_text)
        .map_err(|e| Error::Config(ConfigError::Invalid(format!("cascade manifest: {e}"))))?;
    validate_tiers(&manifest.tiers)?;

    // Resolve the shared instance list once (before any tier runs).
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

    std::fs::create_dir_all(&args.output_dir)?;
    let state_path = args.output_dir.join("cascade.json");

    // Load or create cascade state.
    let mut state = if args.resume && state_path.exists() {
        let text = std::fs::read_to_string(&state_path)?;
        let loaded: CascadeState = serde_json::from_str(&text)?;
        if loaded.instance_ids != instance_ids {
            tracing::warn!(
                persisted = loaded.instance_ids.len(),
                resolved = instance_ids.len(),
                "resume: resolved instance set differs from persisted cascade.json; \
                 using persisted list"
            );
        }
        loaded
    } else {
        CascadeState {
            artifact_kind: "cascade".into(),
            config_path: args.config_path.display().to_string(),
            instance_ids: instance_ids.clone(),
            filter_spec,
            cost_limit_usd: args.sweep_cost_limit_usd,
            instances: BTreeMap::new(),
        }
    };
    write_cascade_state(&state_path, &state)?;

    // Use the authoritative (possibly persisted) instance list.
    let all_ids = state.instance_ids.clone();

    // Cumulative cost starts from already-recorded attempts (resume scenario).
    let mut cumulative_cost: f64 = state.instances.values().map(|r| r.total_cost_usd).sum();

    // Track per-tier aggregate stats for summary.
    let mut tier_stats: Vec<TierStats> = manifest
        .tiers
        .iter()
        .map(|t| TierStats {
            name: t.name.clone(),
            model: t.model.clone(),
            instances_attempted: 0,
            resolved: 0,
            total_cost_usd: 0.0,
        })
        .collect();

    // Populate tier_stats from already-completed attempts (resume).
    for record in state.instances.values() {
        for attempt in &record.attempts {
            if let Some(pos) = tier_stats.iter().position(|t| t.name == attempt.tier_name) {
                tier_stats[pos].instances_attempted += 1;
                tier_stats[pos].total_cost_usd += attempt.cost_usd;
                if record.resolving_tier.as_deref() == Some(&attempt.tier_name) {
                    tier_stats[pos].resolved += 1;
                }
            }
        }
    }

    let ctx = TierRunCtx {
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

    // Run tiers sequentially.
    for (tier_idx, tier_def) in manifest.tiers.iter().enumerate() {
        // Determine which instances still need this tier (not yet resolved, not all-tiers-done).
        let pending_ids: Vec<String> = all_ids
            .iter()
            .filter(|id| {
                let record = state.instances.get(id.as_str());
                match record {
                    // Already resolved → skip (AC#9).
                    Some(r) if r.resolving_tier.is_some() => false,
                    // This tier already attempted for this instance.
                    Some(r) if r.attempts.iter().any(|a| a.tier_name == tier_def.name) => false,
                    // All tiers already attempted (exhausted).
                    Some(r) if r.attempts.len() >= manifest.tiers.len() => false,
                    _ => true,
                }
            })
            .cloned()
            .collect();

        if pending_ids.is_empty() {
            tracing::info!(
                tier = %tier_def.name,
                "cascade: no pending instances for this tier; skipping"
            );
            continue;
        }

        // Budget guard: mark remaining as skipped_budget if cap reached (AC#7).
        if let Some(limit) = args.sweep_cost_limit_usd {
            if cumulative_cost >= limit {
                for id in &pending_ids {
                    let record = state.instances.entry(id.clone()).or_insert_with(|| {
                        InstanceCascadeRecord {
                            resolving_tier: None,
                            total_cost_usd: 0.0,
                            attempts: vec![],
                        }
                    });
                    record.attempts.push(TierAttempt {
                        tier_name: tier_def.name.clone(),
                        model: tier_def.model.clone(),
                        outcome: None,
                        eval_exit_reason: None,
                        cost_usd: 0.0,
                        steps: None,
                        halted_reason: Some("skipped_budget".into()),
                    });
                }
                write_cascade_state(&state_path, &state)?;
                break;
            }
        }

        // Run this tier's sweep for pending instances.
        let tier_sweep_dir = args.output_dir.join(format!("tier-{}", tier_def.name));
        std::fs::create_dir_all(&tier_sweep_dir)?;

        let ids_csv = pending_ids.join(",");
        let sweep_results = run_tier(
            tier_def,
            tier_sweep_dir.clone(),
            ids_csv,
            ctx.clone(),
            args.sweep_cost_limit_usd.map(|lim| lim - cumulative_cost),
        )
        .await?;

        // Determine resolved instances for this tier.
        let resolved_ids: HashSet<String> = if let Some(ref mocks) = args.mock_eval_resolved_ids {
            mocks.get(tier_idx).cloned().unwrap_or_default()
        } else {
            // Run actual evaluator on tier's sweep dir.
            // Wrap the synchronous sb-cli subprocess in spawn_blocking so it
            // doesn't stall the Tokio executor thread for the minutes it may run.
            let eval_args = EvaluateArgs {
                sweep_dir: tier_sweep_dir.clone(),
                dataset_path: None,
                backend: args.eval_backend,
                timeout_per_instance_secs: args.eval_timeout_per_instance_secs,
                parallel: args.parallel,
                sb_subset: args.sb_subset.clone(),
                sb_split: args.sb_split.clone(),
                run_id: None,
                breakdown: BreakdownSelection::none(),
                cost_attribution: false,
            };
            let eval_results =
                tokio::task::spawn_blocking(move || crate::run::evaluate::run(&eval_args))
                    .await
                    .map_err(|e| {
                        Error::Io(std::io::Error::other(format!("eval task panicked: {e}")))
                    })??;
            eval_results
                .instances
                .iter()
                .filter(|e| e.resolved)
                .map(|e| e.instance_id.clone())
                .collect()
        };

        // Build a map of instance_id → sweep result for this tier.
        let results_map: std::collections::HashMap<String, _> = sweep_results
            .instances
            .iter()
            .map(|r| (r.instance_id.clone(), r))
            .collect();

        // Record tier attempts and update cumulative cost.
        let mut tier_cost = 0.0_f64;
        for id in &pending_ids {
            let sweep_result = results_map.get(id.as_str());
            // Use the same cost-accounting helper as the sweep runner so that
            // zero-recorded-cost rows are re-priced from token usage when available.
            let cost = sweep_result.map_or(0.0, |r| {
                crate::run::swebench::budget_accounting_cost_usd(r, &tier_def.model)
            });
            let steps = sweep_result.and_then(|r| r.steps);
            let outcome = sweep_result.and_then(|r| r.outcome.clone());
            // Map swebench's "budget_halt" exit_reason to cascade's "skipped_budget" label
            // so all budget-related non-starts appear uniformly in cascade.json (AC#7).
            let halted_reason = sweep_result
                .map(|r| r.exit_reason.as_str())
                .filter(|s| *s == crate::run::swebench::EXIT_REASON_BUDGET_HALT)
                .map(|_| "skipped_budget".to_string());

            let resolved = resolved_ids.contains(id.as_str());
            let is_budget_halt = sweep_result
                .is_some_and(|r| r.exit_reason == crate::run::swebench::EXIT_REASON_BUDGET_HALT);
            let eval_exit_reason = if is_budget_halt {
                None
            } else if resolved {
                Some("resolved".into())
            } else {
                Some("unresolved".into())
            };

            let record =
                state
                    .instances
                    .entry(id.clone())
                    .or_insert_with(|| InstanceCascadeRecord {
                        resolving_tier: None,
                        total_cost_usd: 0.0,
                        attempts: vec![],
                    });

            // Only record if not already resolved (idempotent on resume).
            if record.resolving_tier.is_none()
                && !record.attempts.iter().any(|a| a.tier_name == tier_def.name)
            {
                record.attempts.push(TierAttempt {
                    tier_name: tier_def.name.clone(),
                    model: tier_def.model.clone(),
                    outcome,
                    eval_exit_reason,
                    cost_usd: cost,
                    steps,
                    halted_reason,
                });
                record.total_cost_usd += cost;
                tier_cost += cost;

                if resolved {
                    record.resolving_tier = Some(tier_def.name.clone());
                }
            }

            // Update tier stats.
            tier_stats[tier_idx].instances_attempted += 1;
            tier_stats[tier_idx].total_cost_usd += cost;
            if resolved {
                tier_stats[tier_idx].resolved += 1;
            }
        }

        cumulative_cost += tier_cost;
        write_cascade_state(&state_path, &state)?;

        tracing::info!(
            tier = %tier_def.name,
            pending = pending_ids.len(),
            resolved_this_tier = resolved_ids.len(),
            tier_cost,
            cumulative_cost,
            "cascade: tier complete"
        );
    }

    // Build and persist summary.
    let summary = build_summary(&tier_stats, &state);

    let summary_json = serde_json::to_string_pretty(&summary)?;
    atomic_write(
        &args.output_dir.join("cascade-summary.json"),
        summary_json.as_bytes(),
    )?;

    let table_txt = render_summary_table(&summary);
    println!("{table_txt}");

    Ok(summary)
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Everything `run_tier` needs; cloneable so concurrent tasks can own their copy.
#[derive(Clone)]
struct TierRunCtx {
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

struct TierStats {
    name: String,
    model: String,
    instances_attempted: usize,
    resolved: usize,
    total_cost_usd: f64,
}

async fn run_tier(
    tier: &TierDef,
    tier_sweep_dir: PathBuf,
    instance_ids_csv: String,
    ctx: TierRunCtx,
    remaining_budget: Option<f64>,
) -> Result<crate::run::swebench::SweepResults, Error> {
    let mut cfg = if let Some(ref prompt_file) = tier.prompt_file {
        Config::load(prompt_file).map_err(Error::Config)?
    } else {
        Config::defaults().map_err(Error::Config)?
    };
    cfg.root.model.name.clone_from(&tier.model);
    if let Some(step_limit) = tier.step_limit {
        cfg.root.agent.step_limit = step_limit;
    }
    if let Some(budget) = tier.per_task_budget_usd {
        cfg.root.agent.per_task_budget_usd = Some(budget);
    }

    let tier_args = crate::run::swebench::SwebenchArgs {
        dataset_source: ctx.dataset_source,
        dataset_cache_dir: ctx.dataset_cache_dir,
        output_dir: tier_sweep_dir,
        parallel: ctx.parallel,
        config: cfg,
        reruns: 1,
        resume: false,
        cost_limit_usd: remaining_budget,
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

    crate::run::swebench::run(tier_args).await
}

#[allow(clippy::cast_precision_loss)]
fn build_summary(tier_stats: &[TierStats], state: &CascadeState) -> CascadeSummary {
    let total_instances = state.instance_ids.len();

    let tier_rows: Vec<TierSummaryRow> = tier_stats
        .iter()
        .map(|t| {
            let n = t.instances_attempted;
            let resolved_rate = if n > 0 {
                t.resolved as f64 / n as f64
            } else {
                0.0
            };
            let mean_cost = if n > 0 {
                t.total_cost_usd / n as f64
            } else {
                0.0
            };
            let cost_per_resolved = if t.resolved > 0 {
                t.total_cost_usd / t.resolved as f64
            } else {
                0.0
            };
            TierSummaryRow {
                name: t.name.clone(),
                model: t.model.clone(),
                instances_attempted: n,
                resolved: t.resolved,
                resolved_rate,
                total_cost_usd: t.total_cost_usd,
                mean_cost_per_attempt_usd: mean_cost,
                cost_per_resolved_usd: cost_per_resolved,
            }
        })
        .collect();

    // Cascade-wide resolved count and cost.
    let cascade_resolved = state
        .instances
        .values()
        .filter(|r| r.resolving_tier.is_some())
        .count();
    let total_cost_usd: f64 = state.instances.values().map(|r| r.total_cost_usd).sum();
    let cascade_resolved_rate = if total_instances > 0 {
        cascade_resolved as f64 / total_instances as f64
    } else {
        0.0
    };
    let cost_per_resolved_cascade_usd = if cascade_resolved > 0 {
        total_cost_usd / cascade_resolved as f64
    } else {
        0.0
    };

    // Counterfactual: run all instances on the top (last) tier.
    // savings = counterfactual_cost - actual_cost
    let savings_vs_top_tier_only_usd = if let Some(top_tier) = tier_stats.last() {
        let top_mean = if top_tier.instances_attempted > 0 {
            top_tier.total_cost_usd / top_tier.instances_attempted as f64
        } else {
            // No data for top tier; estimate using tier-wide cost / attempt ratio.
            tier_stats
                .iter()
                .rev()
                .find(|t| t.instances_attempted > 0)
                .map_or(0.0, |t| t.total_cost_usd / t.instances_attempted as f64)
        };
        let counterfactual = top_mean * total_instances as f64;
        (counterfactual - total_cost_usd).max(0.0)
    } else {
        0.0
    };

    CascadeSummary {
        artifact_kind: "cascade-summary".into(),
        tiers: tier_rows,
        cascade_resolved,
        cascade_resolved_rate,
        total_instances,
        total_cost_usd,
        cost_per_resolved_cascade_usd,
        savings_vs_top_tier_only_usd,
    }
}

fn render_summary_table(summary: &CascadeSummary) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "Tier",
            "Model",
            "Attempted",
            "Resolved",
            "Rate%",
            "Cost($)",
            "$/Resolved",
        ]);

    for row in &summary.tiers {
        let rate_pct = format!("{:.1}", row.resolved_rate * 100.0);
        table.add_row(vec![
            row.name.clone(),
            row.model.clone(),
            row.instances_attempted.to_string(),
            row.resolved.to_string(),
            rate_pct,
            format!("{:.4}", row.total_cost_usd),
            format!("{:.4}", row.cost_per_resolved_usd),
        ]);
    }

    format!(
        "=== bench cascade summary ===\n\n{table}\n\
         Cascade resolved: {}/{} ({:.1}%)\n\
         Total cost: ${:.4}\n\
         Cost/resolved (cascade): ${:.4}\n\
         Savings vs top-tier-only: ${:.4}\n",
        summary.cascade_resolved,
        summary.total_instances,
        summary.cascade_resolved_rate * 100.0,
        summary.total_cost_usd,
        summary.cost_per_resolved_cascade_usd,
        summary.savings_vs_top_tier_only_usd,
    )
}

fn write_cascade_state(path: &Path, state: &CascadeState) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(state)?;
    atomic_write(path, json.as_bytes())
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<(), Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
