//! SWE-bench sweep runner. Minimum-viable full parity: load JSONL, dispatch
//! tasks across `parallel` workers via a consumer-driven `JoinSet`, emit
//! per-instance trajectory + patch files, summarize in `results.json`.
//!
//! Dispatch is consumer-driven (rather than a `Semaphore` + spawn-all
//! pattern) so the sweep-level cost cap can be checked synchronously
//! against the just-finished task before a new task is launched. With
//! a semaphore, a fresh task could acquire its permit after the
//! previous one frees it but before the consumer has updated the
//! cumulative cost — which would silently overshoot the cap by one
//! task's worth of API spend per worker.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::Error;
use crate::model::ModelUsage;
use crate::trajectory::{FailureCategory, Trajectory, outcome};

/// Sentinel `exit_reason` for tasks that never started because the
/// sweep-level USD budget was exhausted. Distinct from `error` and
/// `submitted` so summary tooling can attribute the halt correctly.
pub const EXIT_REASON_BUDGET_HALT: &str = "budget_halt";

/// Standard `claude-3-5-sonnet` USD pricing per 1M tokens. Used for the
/// summary's cost estimate; per-instance trajectories carry only token
/// counts so downstream tooling can re-price as needed.
pub const SONNET_INPUT_USD_PER_MTOK: f64 = 3.0;
pub const SONNET_OUTPUT_USD_PER_MTOK: f64 = 15.0;

#[must_use]
pub fn estimate_cost_usd(prompt_tokens: u64, completion_tokens: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let p = prompt_tokens as f64;
    #[allow(clippy::cast_precision_loss)]
    let c = completion_tokens as f64;
    (c / 1_000_000.0).mul_add(
        SONNET_OUTPUT_USD_PER_MTOK,
        p / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK,
    )
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SweBenchInstance {
    pub instance_id: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub problem_statement: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceResult {
    pub instance_id: String,
    pub exit_reason: String,
    /// Coarse outcome from the trajectory: `submitted` | `step_limit_reached`
    /// | `error`. `None` when the trajectory file could not be read.
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
    /// `true` once the runner persisted a `.patch` artifact for this
    /// instance — even an empty diff. Submitted instances missing a
    /// patch indicate a capture failure (which downgrades `outcome` to
    /// `error`); non-submitted outcomes never write a patch.
    #[serde(default)]
    pub patch_present: bool,
    /// `true` if the captured patch had any content. Distinct from
    /// `patch_present`: a submitted instance always sets `patch_present`
    /// after a successful capture, but `non_empty_patch` only when the
    /// agent's working tree actually diverged from `base_commit`.
    #[serde(default)]
    pub non_empty_patch: bool,
    /// Total attempts executed for this instance (first run + retries).
    #[serde(default = "default_attempts")]
    pub attempts: u32,
    /// Failure categories that triggered retries before the terminal attempt.
    #[serde(default)]
    pub retry_reasons: Vec<FailureCategory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepResults {
    pub total: usize,
    pub submitted: usize,
    pub skipped: usize,
    pub errored: usize,
    #[serde(default)]
    pub failures_by_category: BTreeMap<FailureCategory, usize>,
    /// Tasks that never started because the sweep-level USD budget was
    /// exhausted before they could acquire a worker permit. Counted in
    /// `total` but excluded from `submitted` and `errored`.
    #[serde(default)]
    pub budget_halted: usize,
    /// Submitted instances whose captured patch had non-zero length.
    /// Equal to `submitted` minus the count of empty-diff submissions.
    #[serde(default)]
    pub with_patch: usize,
    #[serde(default)]
    pub total_prompt_tokens: u64,
    #[serde(default)]
    pub total_completion_tokens: u64,
    #[serde(default)]
    pub estimated_cost_usd: f64,
    /// Total retry attempts executed across all instances.
    #[serde(default)]
    pub retries: u64,
    /// Number of instances that retried at least once.
    #[serde(default)]
    pub retried_instances: usize,
    /// Resolved dataset subset spec used for this run.
    #[serde(default)]
    pub filter_spec: FilterSpec,
    /// The USD ceiling enforced for this sweep, echoed from
    /// `SwebenchArgs::cost_limit_usd`. `None` when no limit was set —
    /// distinguishes "ran without a budget" from "budget was infinite".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_limit_usd: Option<f64>,
    #[serde(default)]
    pub instances: Vec<InstanceResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilterSpec {
    #[serde(default)]
    pub original_count: usize,
    #[serde(default)]
    pub selected_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

impl SweepResults {
    /// Render the post-sweep summary table. A flat plain-text block so it
    /// reads cleanly in CI logs and from a tail of stdout.
    #[must_use]
    pub fn summary_table(&self) -> String {
        #[allow(clippy::cast_precision_loss)]
        let submit_rate_pct = if self.total == 0 {
            0.0
        } else {
            (self.submitted as f64 / self.total as f64) * 100.0
        };
        let total_tokens = self
            .total_prompt_tokens
            .saturating_add(self.total_completion_tokens);
        let mut s = String::new();
        s.push_str("\n=== SWE-bench sweep summary ===\n");
        let _ = writeln!(s, "Total tasks:        {}", self.total);
        let _ = writeln!(s, "Submitted:          {}", self.submitted);
        let _ = writeln!(
            s,
            "With patch:         {} — non-empty diff against base_commit",
            self.with_patch
        );
        let _ = writeln!(
            s,
            "Skipped:            {} — trajectory already on disk",
            self.skipped
        );
        let _ = writeln!(
            s,
            "Budget-halted:      {} — never started; sweep-level USD limit reached",
            self.budget_halted
        );
        let _ = writeln!(
            s,
            "Retries:            {} over {} instances",
            self.retries, self.retried_instances
        );
        let _ = writeln!(s, "Submit rate:        {submit_rate_pct:.2}%");
        let _ = writeln!(s, "Prompt tokens:      {}", self.total_prompt_tokens);
        let _ = writeln!(s, "Completion tokens:  {}", self.total_completion_tokens);
        let _ = writeln!(s, "Total tokens:       {total_tokens}");
        let _ = writeln!(
            s,
            "Estimated cost:     ${:.4} (claude-3-5-sonnet @ ${SONNET_INPUT_USD_PER_MTOK}/MTok in, ${SONNET_OUTPUT_USD_PER_MTOK}/MTok out)",
            self.estimated_cost_usd
        );
        if let Some(limit) = self.cost_limit_usd {
            let _ = writeln!(s, "Sweep cost limit:   ${limit:.4}");
            if self.budget_halted > 0 {
                let _ = writeln!(
                    s,
                    "BUDGET HALT at ${:.4} of ${:.4} — {} task(s) never started",
                    self.estimated_cost_usd, limit, self.budget_halted
                );
            }
        }
        let mut nonzero: Vec<(FailureCategory, usize)> = self
            .failures_by_category
            .iter()
            .filter_map(|(k, v)| (*v > 0).then_some((*k, *v)))
            .collect();
        nonzero.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if !nonzero.is_empty() {
            s.push_str("Failures by category:\n");
            for (k, v) in nonzero {
                let _ = writeln!(s, "  - {}: {}", failure_category_label(k), v);
            }
        }
        let unclassified_legacy = self
            .instances
            .iter()
            .filter(|r| is_failed_instance(r) && r.failure_category.is_none())
            .count();
        if unclassified_legacy > 0 {
            let _ = writeln!(s, "  - unclassified (legacy): {unclassified_legacy}");
        }
        s
    }
}

pub struct SwebenchArgs {
    pub dataset_path: PathBuf,
    pub output_dir: PathBuf,
    pub parallel: usize,
    pub config: Config,
    /// When true, tasks whose trajectory file already exists and parses as
    /// valid JSON are skipped before any agent (or Docker container, or
    /// model API call) is launched for them.
    pub resume: bool,
    /// Optional sweep-level USD spend ceiling. When `Some(limit)`, the
    /// runner stops dequeuing new tasks once cumulative cost (summed
    /// from each finished task's `estimate_cost_usd`) reaches `limit`.
    /// In-flight tasks are allowed to finish; tasks that never started
    /// are recorded with `exit_reason: "budget_halt"`. Resume-skipped
    /// tasks contribute to the running total at their stored cost so a
    /// resumed sweep cannot blow past the limit.
    pub cost_limit_usd: Option<f64>,
    /// Dataset subset selector. Either comma-separated ids or
    /// `@path/to/file.txt` (one id per line).
    pub instance_ids: Option<String>,
    /// Keep at most N instances after filtering + sampling.
    pub limit: Option<usize>,
    /// Reproducibly random-subset to N instances. Requires `seed`.
    pub sample: Option<usize>,
    /// RNG seed used by `sample`.
    pub seed: Option<u64>,
    /// Retry transiently-failed instances up to N additional attempts.
    /// `0` preserves the historical no-retry behavior.
    pub max_retries: u32,
    /// Comma-separated failure-category labels to retry.
    /// When `None`, the default transient set is used.
    pub retry_on: Option<String>,
    /// Exponential backoff base in milliseconds.
    pub retry_backoff_base_ms: u64,
    /// Exponential backoff max cap in seconds.
    pub retry_backoff_cap_s: u64,
    /// If true, `--resume` re-runs previously completed instances whose
    /// stored `failure_category` is retryable.
    pub retry_on_resume: bool,
    /// Per-task deterministic responses, cloned into each spawned `MiniArgs`.
    /// Lets sweeps run end-to-end against a scripted model without network
    /// I/O — mainly useful for tests and local smoke checks.
    pub deterministic_responses: Option<Vec<String>>,
    /// Optional fixed `ModelUsage` reported by the scripted backend on
    /// every call. Lets sweep-budget tests trigger budget halts
    /// deterministically. Only meaningful when `deterministic_responses`
    /// is `Some`.
    pub deterministic_usage_per_call: Option<ModelUsage>,
}

fn default_attempts() -> u32 {
    1
}

/// Path where `run_one` writes the trajectory for an instance. Centralized so
/// the resume-skip check stays in lockstep with the writer.
#[must_use]
pub fn trajectory_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    output_dir.join(format!("{instance_id}.traj.json"))
}

/// Path where the SWE-bench-style unified diff is written for an instance.
/// File presence is the resume-mode signal that the patch artifact was
/// captured for a previously-submitted run.
#[must_use]
pub fn patch_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    output_dir.join(format!("{instance_id}.patch"))
}

/// Path of the aggregated SWE-bench predictions file written at the end of
/// a sweep. One JSONL line per submitted instance, in the schema sb-cli
/// expects (`instance_id`, `model_patch`, `model_name_or_path`).
#[must_use]
pub fn predictions_path(output_dir: &std::path::Path) -> PathBuf {
    output_dir.join("all_preds.jsonl")
}

/// Inspect a trajectory path on disk. Returns `Some(info)` only when the file
/// exists *and* parses as valid trajectory JSON; truncated or corrupt files
/// (e.g. a mid-write crash) yield `None` so the task re-runs.
#[must_use]
pub fn existing_trajectory_info(
    output_dir: &std::path::Path,
    instance_id: &str,
) -> Option<crate::trajectory::TrajectoryInfo> {
    read_trajectory_info(&trajectory_path_for(output_dir, instance_id))
}

pub fn load_dataset(path: &std::path::Path) -> Result<Vec<SweBenchInstance>, Error> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let instance: SweBenchInstance = serde_json::from_str(line)
            .map_err(|e| Error::Trajectory(format!("dataset line {}: {e}", i + 1)))?;
        out.push(instance);
    }
    Ok(out)
}

/// Run the sweep. This scaffolds the parallelism + trajectory emission; the
/// per-instance body calls through to `run::mini::run` using a Docker env
/// (when the `docker` feature is enabled).
// The body is a single sequential pipeline (load → resume-skip → spawn →
// join → aggregate → emit). Splitting it would obscure the linear flow
// without yielding reusable pieces.
#[allow(clippy::too_many_lines)]
pub async fn run(args: SwebenchArgs) -> Result<SweepResults, Error> {
    std::fs::create_dir_all(&args.output_dir)?;
    let retry_policy = RetryPolicy::from_args(
        args.max_retries,
        args.retry_on.as_deref(),
        args.retry_backoff_base_ms,
        args.retry_backoff_cap_s,
    )?;

    let instances = load_dataset(&args.dataset_path)?;
    let (instances, filter_spec) = apply_subset(
        instances,
        args.instance_ids.as_deref(),
        args.limit,
        args.sample,
        args.seed,
    )?;
    let total = instances.len();
    let mut set = tokio::task::JoinSet::new();
    let mut skipped_results: Vec<InstanceResult> = Vec::new();
    let mut pending: std::collections::VecDeque<SweBenchInstance> =
        std::collections::VecDeque::new();

    // Sweep-level cumulative USD spend, computed via `estimate_cost_usd`
    // from each task's prompt/completion tokens. Resume-skipped tasks
    // contribute their stored cost up front so a resumed sweep cannot
    // blow past the limit by re-summing only freshly-run tasks.
    let mut cumulative_cost = 0.0f64;
    let mut halted = false;
    let limit = args.cost_limit_usd;
    let bump_cost = |cost: f64, cumulative: &mut f64, halted: &mut bool| {
        *cumulative += cost;
        if let Some(l) = limit {
            if *cumulative >= l {
                *halted = true;
            }
        }
    };

    for inst in instances {
        // Resume short-circuit: a valid on-disk trajectory + (when the run
        // was submitted) a patch file mean this task is fully archived
        // from a prior sweep. Skip it before we even consider dispatch —
        // no worker slot, no Docker container, no model API call.
        if args.resume {
            if let Some(info) = existing_trajectory_info(&args.output_dir, &inst.instance_id) {
                let patch_path = patch_path_for(&args.output_dir, &inst.instance_id);
                let needs_patch = info.outcome.as_deref() == Some(outcome::SUBMITTED);
                let retryable_resume = args.retry_on_resume
                    && info
                        .failure_category
                        .is_some_and(|cat| retry_policy.should_retry(cat));
                if !retryable_resume && (!needs_patch || patch_path.exists()) {
                    let r = skipped_result_from_info(&inst.instance_id, &info, &patch_path);
                    bump_cost(
                        estimate_cost_usd(
                            r.prompt_tokens.unwrap_or(0),
                            r.completion_tokens.unwrap_or(0),
                        ),
                        &mut cumulative_cost,
                        &mut halted,
                    );
                    skipped_results.push(r);
                    continue;
                }
                tracing::info!(
                    instance = %inst.instance_id,
                    "resume: trajectory present but patch missing — re-running"
                );
            }
        }
        pending.push_back(inst);
    }

    let mut results = skipped_results;
    let skipped = results
        .iter()
        .filter(|r| r.exit_reason == "skipped_resume")
        .count();
    let mut submitted = 0;
    let mut errored = 0;
    let mut budget_halted = 0;
    let mut with_patch = 0;
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;
    let mut total_retries = 0u64;
    let mut retried_instances = 0usize;
    let parallelism = args.parallel.max(1);

    // Consumer-driven dispatch: spawn at most `parallelism` tasks at a
    // time, and only launch a fresh task once the consumer has processed
    // the previous result and (re-)checked the halt flag. A semaphore
    // would close the same race only after the new permit was claimed —
    // by which time another agent has already started an API call.
    let spawn_one = |inst: SweBenchInstance, set: &mut tokio::task::JoinSet<InstanceResult>| {
        let output_dir = args.output_dir.clone();
        let cfg = args.config.clone();
        let deterministic = args.deterministic_responses.clone();
        let det_usage = args.deterministic_usage_per_call.clone();
        let retry_policy = retry_policy.clone();
        set.spawn(async move {
            run_one(
                inst,
                output_dir,
                cfg,
                deterministic,
                det_usage,
                retry_policy,
            )
            .await
        });
    };

    if halted {
        // Resume already exhausted the budget; everything that was queued
        // never starts.
        while let Some(inst) = pending.pop_front() {
            results.push(budget_halt_result(&inst.instance_id));
            budget_halted += 1;
        }
    } else {
        // Initial fill.
        for _ in 0..parallelism {
            if let Some(inst) = pending.pop_front() {
                spawn_one(inst, &mut set);
            } else {
                break;
            }
        }
    }

    while let Some(j) = set.join_next().await {
        match j {
            Ok(r) => {
                match r.outcome.as_deref() {
                    Some(outcome::SUBMITTED) => submitted += 1,
                    Some(outcome::ERROR) => errored += 1,
                    _ => {}
                }
                if r.non_empty_patch {
                    with_patch += 1;
                }
                total_retries =
                    total_retries.saturating_add(u64::from(r.attempts.saturating_sub(1)));
                if r.attempts > 1 {
                    retried_instances += 1;
                }
                if let Some(p) = r.prompt_tokens {
                    total_prompt = total_prompt.saturating_add(p);
                }
                if let Some(c) = r.completion_tokens {
                    total_completion = total_completion.saturating_add(c);
                }
                // Sweep-level budget bookkeeping. Tasks that completed
                // (whether submitted or errored) consumed real API budget
                // and count toward the cap.
                let cost = estimate_cost_usd(
                    r.prompt_tokens.unwrap_or(0),
                    r.completion_tokens.unwrap_or(0),
                );
                let was_halted = halted;
                bump_cost(cost, &mut cumulative_cost, &mut halted);
                if halted && !was_halted {
                    if let Some(l) = limit {
                        tracing::warn!(
                            cumulative_usd = cumulative_cost,
                            limit_usd = l,
                            "sweep cost limit reached — halting new task launches"
                        );
                    }
                }
                results.push(r);
            }
            Err(e) => {
                errored += 1;
                results.push(InstanceResult {
                    instance_id: "<join_error>".into(),
                    exit_reason: "error".into(),
                    outcome: Some(outcome::ERROR.into()),
                    failure_category: Some(FailureCategory::AgentInternal),
                    steps: None,
                    cost_usd: None,
                    prompt_tokens: None,
                    completion_tokens: None,
                    duration_secs: None,
                    error: Some(e.to_string()),
                    patch_present: false,
                    non_empty_patch: false,
                    attempts: 1,
                    retry_reasons: Vec::new(),
                });
            }
        }

        // Decide what to do with the next pending task. If the budget is
        // exhausted, drain the queue into `budget_halt` results without
        // spawning. Otherwise, dispatch one — keeping the in-flight
        // count at `parallelism` until the queue drains.
        if halted {
            while let Some(inst) = pending.pop_front() {
                results.push(budget_halt_result(&inst.instance_id));
                budget_halted += 1;
            }
        } else if let Some(inst) = pending.pop_front() {
            spawn_one(inst, &mut set);
        }
    }

    // Skipped instances loaded from disk also contribute to the patch
    // counter so resumed sweeps report cumulative `with_patch` correctly.
    for r in &results {
        if r.exit_reason == "skipped_resume" && r.non_empty_patch {
            with_patch += 1;
        }
    }
    let mut failures_by_category: BTreeMap<FailureCategory, usize> = BTreeMap::new();
    for r in &results {
        if let Some(cat) = r.failure_category {
            *failures_by_category.entry(cat).or_insert(0) += 1;
        }
    }

    write_predictions_file(&args.output_dir, &results, &args.config.root.model.name)?;

    let sweep = SweepResults {
        total,
        submitted,
        skipped,
        errored,
        failures_by_category,
        budget_halted,
        with_patch,
        total_prompt_tokens: total_prompt,
        total_completion_tokens: total_completion,
        estimated_cost_usd: estimate_cost_usd(total_prompt, total_completion),
        retries: total_retries,
        retried_instances,
        filter_spec,
        cost_limit_usd: args.cost_limit_usd,
        instances: results,
    };
    let summary_path = args.output_dir.join("results.json");
    std::fs::write(&summary_path, serde_json::to_string_pretty(&sweep)?)?;

    Ok(sweep)
}

/// Build the `InstanceResult` returned for a task that never started
/// because the sweep-level budget was already exhausted by the time
/// its permit became available.
fn budget_halt_result(instance_id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: instance_id.to_owned(),
        exit_reason: EXIT_REASON_BUDGET_HALT.into(),
        outcome: None,
        failure_category: None,
        steps: None,
        cost_usd: None,
        prompt_tokens: None,
        completion_tokens: None,
        duration_secs: None,
        error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
    }
}

/// Write `all_preds.jsonl` containing one line per *submitted* instance
/// with a patch artifact on disk. Schema: `{instance_id, model_patch,
/// model_name_or_path}` — the minimum sb-cli accepts. Non-submitted and
/// patch-capture-failed instances are excluded by design so sb-cli
/// reports them as unresolved rather than misattributes a stale diff.
fn write_predictions_file(
    output_dir: &std::path::Path,
    results: &[InstanceResult],
    model_name: &str,
) -> Result<(), Error> {
    let path = predictions_path(output_dir);
    let mut text = String::new();
    for r in results {
        if r.outcome.as_deref() != Some(outcome::SUBMITTED) {
            continue;
        }
        if !r.patch_present {
            // A submitted-but-patch-missing instance only happens on a
            // resume that found the trajectory but no `.patch`; we already
            // re-queued it above so this branch is defensive.
            continue;
        }
        let patch_path = patch_path_for(output_dir, &r.instance_id);
        let model_patch = std::fs::read_to_string(&patch_path).unwrap_or_default();
        let line = serde_json::json!({
            "instance_id": r.instance_id,
            "model_patch": model_patch,
            "model_name_or_path": model_name,
        });
        text.push_str(&serde_json::to_string(&line)?);
        text.push('\n');
    }
    std::fs::write(&path, text)?;
    Ok(())
}

/// Build an `InstanceResult` for a task skipped via `--resume`. Mirrors what
/// `run_one` would have produced from the on-disk trajectory, with
/// `exit_reason = "skipped_resume"` so summaries can distinguish a fresh run
/// from a resumed one.
fn skipped_result_from_info(
    instance_id: &str,
    info: &crate::trajectory::TrajectoryInfo,
    patch_path: &std::path::Path,
) -> InstanceResult {
    let (prompt_tokens, completion_tokens) = info.token_usage.as_ref().map_or((None, None), |t| {
        (Some(t.prompt_tokens), Some(t.completion_tokens))
    });
    let (patch_present, non_empty_patch) = match std::fs::metadata(patch_path) {
        Ok(m) => (true, m.len() > 0),
        Err(_) => (false, false),
    };
    InstanceResult {
        instance_id: instance_id.to_owned(),
        exit_reason: "skipped_resume".into(),
        outcome: info.outcome.clone(),
        failure_category: info.failure_category,
        steps: info.steps,
        cost_usd: info.total_cost_usd,
        prompt_tokens,
        completion_tokens,
        duration_secs: info.duration_secs,
        error: None,
        patch_present,
        non_empty_patch,
        attempts: 1,
        retry_reasons: Vec::new(),
    }
}

#[derive(Debug, Clone)]
struct RetryPolicy {
    max_retries: u32,
    retry_on: BTreeSet<FailureCategory>,
    backoff_base_ms: u64,
    backoff_cap_s: u64,
}

impl RetryPolicy {
    fn from_args(
        max_retries: u32,
        retry_on: Option<&str>,
        backoff_base_ms: u64,
        backoff_cap_s: u64,
    ) -> Result<Self, Error> {
        let retry_on = parse_retry_on(retry_on)?;
        Ok(Self {
            max_retries,
            retry_on,
            backoff_base_ms,
            backoff_cap_s,
        })
    }

    fn should_retry(&self, cat: FailureCategory) -> bool {
        self.max_retries > 0 && self.retry_on.contains(&cat)
    }

    fn backoff_for(&self, instance_id: &str, attempt: u32) -> std::time::Duration {
        if self.backoff_base_ms == 0 {
            return std::time::Duration::from_millis(0);
        }
        let factor = 1u64
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u64::MAX);
        let exp_ms = self.backoff_base_ms.saturating_mul(factor);
        let cap_ms = self.backoff_cap_s.saturating_mul(1000);
        let bounded_ms = exp_ms.min(cap_ms.max(self.backoff_base_ms));
        let jitter_seed = simple_hash(instance_id) ^ u64::from(attempt);
        let jitter_pct = jitter_seed % 251; // 0..250 => up to +25.0%
        let jittered = bounded_ms.saturating_mul(1000 + jitter_pct) / 1000;
        std::time::Duration::from_millis(jittered.min(cap_ms.max(bounded_ms)))
    }
}

fn parse_retry_on(retry_on: Option<&str>) -> Result<BTreeSet<FailureCategory>, Error> {
    let raw = retry_on.map(str::trim).filter(|s| !s.is_empty());
    if let Some(list) = raw {
        let mut set = BTreeSet::new();
        for token in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let cat = parse_failure_category_label(token)?;
            set.insert(cat);
        }
        return Ok(set);
    }
    Ok(BTreeSet::from([FailureCategory::ModelApi]))
}

fn parse_failure_category_label(s: &str) -> Result<FailureCategory, Error> {
    match s {
        "env_setup" => Ok(FailureCategory::EnvSetup),
        "model_api" => Ok(FailureCategory::ModelApi),
        "model_parse" => Ok(FailureCategory::ModelParse),
        "step_limit" => Ok(FailureCategory::StepLimit),
        "cost_limit" => Ok(FailureCategory::CostLimit),
        "agent_internal" => Ok(FailureCategory::AgentInternal),
        "unknown" => Ok(FailureCategory::Unknown),
        _ => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "unknown retry category `{s}`"
        )))),
    }
}

fn simple_hash(s: &str) -> u64 {
    let mut x = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        x ^= u64::from(b);
        x = x.wrapping_mul(0x1000_0000_01b3);
    }
    x
}

fn deterministic_for_attempt(all: &[String], attempt: u32, retry_mode: bool) -> Vec<String> {
    if !retry_mode {
        return all.to_vec();
    }
    let idx = usize::try_from(attempt.saturating_sub(1)).unwrap_or(usize::MAX);
    if idx < all.len() {
        return vec![all[idx].clone()];
    }
    all.last().cloned().into_iter().collect()
}

async fn run_one(
    inst: SweBenchInstance,
    output_dir: PathBuf,
    mut cfg: Config,
    deterministic_responses: Option<Vec<String>>,
    deterministic_usage_per_call: Option<ModelUsage>,
    retry_policy: RetryPolicy,
) -> InstanceResult {
    let id = inst.instance_id.clone();
    let task = inst.problem_statement.clone().unwrap_or_default();
    // A scripted model implies a local-only sweep — `image` from the dataset
    // would otherwise force the Docker env, which is wrong for tests.
    if deterministic_responses.is_none() {
        if let Some(img) = &inst.image {
            cfg.root.environment.docker_image = Some(img.clone());
            cfg.root.environment.kind = crate::config::EnvKind::Docker;
        }
    }
    let workdir = PathBuf::from(cfg.root.environment.workdir.clone());
    let patch_path = patch_path_for(&output_dir, &id);
    let base_commit = inst.base_commit.clone();
    let mut attempts = 0u32;
    let mut retry_reasons = Vec::new();
    let mut total_prompt_tokens = 0u64;
    let mut total_completion_tokens = 0u64;
    let mut terminal: Option<InstanceResult> = None;

    while attempts <= retry_policy.max_retries {
        attempts += 1;
        let det_for_attempt = deterministic_responses
            .as_ref()
            .map(|v| deterministic_for_attempt(v, attempts, retry_policy.max_retries > 0));
        let args = crate::run::mini::MiniArgs {
            task: task.clone(),
            extra_context: None,
            config: cfg.clone(),
            output_dir: output_dir.clone(),
            trajectory_name: id.clone(),
            deterministic_responses: det_for_attempt,
            deterministic_usage_per_call: deterministic_usage_per_call.clone(),
            stream_addr: None,
            patch_capture: Some(crate::run::mini::PatchCaptureSpec {
                base_commit: base_commit.clone(),
                workdir: workdir.clone(),
                patch_path: patch_path.clone(),
            }),
        };
        let run_err = crate::run::mini::run(args).await.err();
        let traj_path = output_dir.join(format!("{id}.traj.json"));
        let info = read_trajectory_info(&traj_path);
        let outcome_str = info
            .as_ref()
            .and_then(|i| i.outcome.clone())
            .or_else(|| run_err.as_ref().map(|_| outcome::ERROR.to_owned()))
            .unwrap_or_else(|| outcome::ERROR.to_owned());
        let exit_reason = info
            .as_ref()
            .and_then(|i| i.exit_reason.clone())
            .unwrap_or_else(|| outcome_str.clone());
        let (prompt_tokens, completion_tokens) = info
            .as_ref()
            .and_then(|i| i.token_usage.as_ref())
            .map_or((0, 0), |t| (t.prompt_tokens, t.completion_tokens));
        total_prompt_tokens = total_prompt_tokens.saturating_add(prompt_tokens);
        total_completion_tokens = total_completion_tokens.saturating_add(completion_tokens);

        let (patch_present, non_empty_patch) = match std::fs::metadata(&patch_path) {
            Ok(m) => (true, m.len() > 0),
            Err(_) => (false, false),
        };
        let failure_category = if outcome_str == outcome::SUBMITTED {
            None
        } else {
            info.as_ref()
                .and_then(|i| i.failure_category)
                .or_else(|| match exit_reason.as_str() {
                    "step_limit" => Some(FailureCategory::StepLimit),
                    "cost_limit" => Some(FailureCategory::CostLimit),
                    _ => run_err
                        .as_ref()
                        .map(classify_error)
                        .or(Some(FailureCategory::Unknown)),
                })
        };
        let current = InstanceResult {
            instance_id: id.clone(),
            exit_reason,
            outcome: Some(outcome_str.clone()),
            failure_category,
            steps: info.as_ref().and_then(|i| i.steps),
            cost_usd: Some(estimate_cost_usd(
                total_prompt_tokens,
                total_completion_tokens,
            )),
            prompt_tokens: Some(total_prompt_tokens),
            completion_tokens: Some(total_completion_tokens),
            duration_secs: info.as_ref().and_then(|i| i.duration_secs),
            error: run_err.map(|e| e.to_string()),
            patch_present,
            non_empty_patch,
            attempts,
            retry_reasons: retry_reasons.clone(),
        };
        let retryable = current
            .failure_category
            .is_some_and(|cat| retry_policy.should_retry(cat))
            && attempts <= retry_policy.max_retries;
        if !retryable {
            terminal = Some(current);
            break;
        }
        if let Some(cat) = current.failure_category {
            retry_reasons.push(cat);
            tracing::warn!(instance=%id, attempt=attempts, failure_category=%failure_category_label(cat), "retrying transient failure");
        }
        tokio::time::sleep(retry_policy.backoff_for(&id, attempts)).await;
        terminal = Some(current);
    }

    terminal.unwrap_or_else(|| budget_halt_result(&id))
}

fn classify_error(err: &Error) -> FailureCategory {
    match err {
        // Docker build/start/exec or local command execution plumbing.
        Error::Env(_) => FailureCategory::EnvSetup,
        // Model transport/auth/rate-limit/5xx style failures.
        Error::Model(crate::error::ModelError::Malformed(_)) => FailureCategory::ModelParse,
        Error::Model(_) => FailureCategory::ModelApi,
        // Any remaining typed error in the runner/agent.
        _ => FailureCategory::AgentInternal,
    }
}

fn is_failed_instance(r: &InstanceResult) -> bool {
    r.outcome.as_deref() == Some(outcome::ERROR)
        || matches!(
            r.failure_category,
            Some(FailureCategory::StepLimit | FailureCategory::CostLimit)
        )
}

fn failure_category_label(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::Unknown => "unknown",
    }
}

fn read_trajectory_info(path: &std::path::Path) -> Option<crate::trajectory::TrajectoryInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let traj: Trajectory = serde_json::from_str(&text).ok()?;
    Some(traj.info)
}

fn apply_subset(
    mut instances: Vec<SweBenchInstance>,
    instance_ids_arg: Option<&str>,
    limit: Option<usize>,
    sample: Option<usize>,
    seed: Option<u64>,
) -> Result<(Vec<SweBenchInstance>, FilterSpec), Error> {
    if seed.is_some() && sample.is_none() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "`--seed` requires `--sample`".into(),
        )));
    }

    let original_count = instances.len();
    let requested_ids = parse_instance_ids_arg(instance_ids_arg)?;

    if let Some(ids) = requested_ids.as_ref() {
        let dataset_ids: HashSet<&str> = instances.iter().map(|i| i.instance_id.as_str()).collect();
        let unknown: BTreeSet<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|id| !dataset_ids.contains(id))
            .collect();
        if !unknown.is_empty() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "--instance-ids references unknown id(s): {}",
                unknown.into_iter().collect::<Vec<_>>().join(", ")
            ))));
        }
        let include: HashSet<&str> = ids.iter().map(String::as_str).collect();
        instances.retain(|i| include.contains(i.instance_id.as_str()));
    }

    if let Some(n) = sample {
        let seed_value = seed.ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(
                "`--sample` requires `--seed`".into(),
            ))
        })?;
        if n < instances.len() {
            let mut rng = XorShift64::new(seed_value);
            for i in (1..instances.len()).rev() {
                let j = rng.next_usize() % (i + 1);
                instances.swap(i, j);
            }
            instances.truncate(n);
        }
    }

    if let Some(n) = limit {
        if n < instances.len() {
            instances.truncate(n);
        }
    }

    if instances.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "dataset subset produced zero instances; refusing to run".into(),
        )));
    }

    let spec = FilterSpec {
        original_count,
        selected_count: instances.len(),
        instance_ids: requested_ids,
        limit,
        sample,
        seed,
    };
    Ok((instances, spec))
}

fn parse_instance_ids_arg(instance_ids_arg: Option<&str>) -> Result<Option<Vec<String>>, Error> {
    let Some(raw) = instance_ids_arg.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let text = if let Some(path) = raw.strip_prefix('@') {
        std::fs::read_to_string(path).map_err(|e| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "failed to read --instance-ids file `{path}`: {e}"
            )))
        })?
    } else {
        raw.to_owned()
    };
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for part in text.split([',', '\n']) {
        let id = part.trim();
        if id.is_empty() {
            continue;
        }
        if seen.insert(id.to_owned()) {
            ids.push(id.to_owned());
        }
    }
    if ids.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--instance-ids resolved to an empty list".into(),
        )));
    }
    Ok(Some(ids))
}

/// Tiny deterministic RNG for dataset shuffling. This is intentionally local
/// so we can keep sampling reproducible without adding a new dependency.
#[derive(Clone, Copy)]
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero absorbing state.
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_usize(&mut self) -> usize {
        #[cfg(target_pointer_width = "64")]
        {
            usize::from_le_bytes(self.next_u64().to_le_bytes())
        }
        #[cfg(target_pointer_width = "32")]
        {
            let bytes = self.next_u64().to_le_bytes();
            usize::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn cost_estimate_uses_sonnet_pricing() {
        // 1M prompt + 1M completion = $3 + $15 = $18.
        let c = estimate_cost_usd(1_000_000, 1_000_000);
        assert!((c - 18.0).abs() < 1e-9, "got {c}");
        // Zero in, zero out.
        assert!(estimate_cost_usd(0, 0).abs() < 1e-9);
    }

    #[test]
    fn summary_table_includes_required_fields() {
        let s = SweepResults {
            total: 10,
            submitted: 4,
            skipped: 3,
            errored: 1,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 3,
            total_prompt_tokens: 250_000,
            total_completion_tokens: 50_000,
            estimated_cost_usd: estimate_cost_usd(250_000, 50_000),
            retries: 0,
            retried_instances: 0,
            filter_spec: FilterSpec {
                original_count: 10,
                selected_count: 10,
                ..FilterSpec::default()
            },
            cost_limit_usd: None,
            instances: vec![],
        };
        let t = s.summary_table();
        assert!(t.contains("Total tasks:        10"));
        assert!(t.contains("Submitted:          4"));
        assert!(
            t.contains("With patch:         3 — non-empty diff against base_commit"),
            "missing with_patch row in: {t}"
        );
        assert!(
            t.contains("Skipped:            3 — trajectory already on disk"),
            "missing skipped row in: {t}"
        );
        assert!(
            t.contains("Budget-halted:      0 — never started; sweep-level USD limit reached"),
            "missing budget_halted row in: {t}"
        );
        assert!(t.contains("Submit rate:        40.00%"));
        assert!(t.contains("Total tokens:       300000"));
        assert!(t.contains("Estimated cost:     $1.5000"));
        // Without a configured limit, the summary should not advertise one.
        assert!(
            !t.contains("Sweep cost limit:"),
            "limit row leaked in unconstrained sweep: {t}"
        );
        assert!(!t.contains("BUDGET HALT"));
    }

    #[test]
    fn summary_table_includes_budget_halt_line_when_triggered() {
        let s = SweepResults {
            total: 5,
            submitted: 3,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 2,
            with_patch: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 100_000,
            estimated_cost_usd: 1.5,
            retries: 0,
            retried_instances: 0,
            filter_spec: FilterSpec {
                original_count: 5,
                selected_count: 5,
                ..FilterSpec::default()
            },
            cost_limit_usd: Some(1.0),
            instances: vec![],
        };
        let t = s.summary_table();
        assert!(t.contains("Sweep cost limit:   $1.0000"), "got: {t}");
        assert!(
            t.contains("BUDGET HALT at $1.5000 of $1.0000"),
            "missing budget halt line: {t}"
        );
        assert!(t.contains("2 task(s) never started"));
    }

    #[test]
    fn existing_trajectory_info_returns_some_for_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let traj = Trajectory {
            trajectory_format: crate::trajectory::FORMAT_VERSION.into(),
            info: crate::trajectory::TrajectoryInfo {
                outcome: Some(outcome::SUBMITTED.into()),
                exit_reason: Some("submitted".into()),
                steps: Some(2),
                ..Default::default()
            },
            messages: vec![],
        };
        let path = trajectory_path_for(dir.path(), "inst-1");
        std::fs::write(&path, serde_json::to_string(&traj).unwrap()).unwrap();

        let info = existing_trajectory_info(dir.path(), "inst-1").unwrap();
        assert_eq!(info.outcome.as_deref(), Some(outcome::SUBMITTED));
    }

    #[test]
    fn existing_trajectory_info_returns_none_for_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = trajectory_path_for(dir.path(), "inst-2");
        std::fs::write(&path, "{\"trajectory_format\": \"mini-swe-agent-1.").unwrap();
        assert!(existing_trajectory_info(dir.path(), "inst-2").is_none());
    }

    #[test]
    fn existing_trajectory_info_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(existing_trajectory_info(dir.path(), "no-such-id").is_none());
    }

    #[test]
    fn retry_on_parser_accepts_known_labels() {
        let parsed = parse_retry_on(Some("model_api,step_limit")).unwrap();
        assert!(parsed.contains(&FailureCategory::ModelApi));
        assert!(parsed.contains(&FailureCategory::StepLimit));
    }

    #[test]
    fn retry_on_parser_rejects_unknown_label() {
        let err = parse_retry_on(Some("not_a_category")).err();
        assert!(err.is_some());
    }

    #[test]
    fn deterministic_attempt_script_shifts_after_first_attempt() {
        let seq = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(deterministic_for_attempt(&seq, 1, false), seq);
        assert_eq!(
            deterministic_for_attempt(&seq, 1, true),
            vec!["a".to_owned()]
        );
        assert_eq!(
            deterministic_for_attempt(&seq, 2, true),
            vec!["b".to_owned()]
        );
        assert_eq!(
            deterministic_for_attempt(&seq, 99, true),
            vec!["c".to_owned()]
        );
    }

    #[test]
    fn loads_jsonl() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "{\"instance_id\":\"a\"}\n{\"instance_id\":\"b\",\"image\":\"ubuntu:22.04\"}\n",
        )
        .unwrap();
        let got = load_dataset(tmp.path()).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].instance_id, "a");
        assert_eq!(got[1].image.as_deref(), Some("ubuntu:22.04"));
    }

    #[test]
    fn subset_instance_ids_filters_and_rejects_unknowns() {
        let instances = vec![
            SweBenchInstance {
                instance_id: "a".into(),
                repo: None,
                base_commit: None,
                problem_statement: None,
                image: None,
                other: serde_json::Map::new(),
            },
            SweBenchInstance {
                instance_id: "b".into(),
                repo: None,
                base_commit: None,
                problem_statement: None,
                image: None,
                other: serde_json::Map::new(),
            },
        ];
        let (filtered, spec) =
            apply_subset(instances.clone(), Some("b"), None, None, None).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].instance_id, "b");
        assert_eq!(spec.original_count, 2);
        assert_eq!(spec.selected_count, 1);

        let err = apply_subset(instances, Some("missing"), None, None, None).unwrap_err();
        assert!(err.to_string().contains("unknown id(s): missing"), "{err}");
    }

    #[test]
    fn subset_sample_is_reproducible_with_fixed_seed() {
        let mk = |id: &str| SweBenchInstance {
            instance_id: id.into(),
            repo: None,
            base_commit: None,
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        };
        let instances = vec![mk("a"), mk("b"), mk("c"), mk("d"), mk("e"), mk("f")];
        let (a, _) = apply_subset(instances.clone(), None, None, Some(3), Some(42)).unwrap();
        let (b, _) = apply_subset(instances, None, None, Some(3), Some(42)).unwrap();
        let a_ids: Vec<_> = a.into_iter().map(|i| i.instance_id).collect();
        let b_ids: Vec<_> = b.into_iter().map(|i| i.instance_id).collect();
        assert_eq!(a_ids, b_ids);
    }

    #[test]
    fn subset_composition_order_is_ids_then_sample_then_limit() {
        let mk = |id: &str| SweBenchInstance {
            instance_id: id.into(),
            repo: None,
            base_commit: None,
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        };
        let instances = vec![mk("a"), mk("b"), mk("c"), mk("d"), mk("e"), mk("f")];
        let (filtered, spec) =
            apply_subset(instances, Some("a,b,c,d,e"), Some(2), Some(4), Some(7)).unwrap();
        assert_eq!(spec.original_count, 6);
        assert_eq!(spec.selected_count, 2);
        assert_eq!(spec.limit, Some(2));
        assert_eq!(spec.sample, Some(4));
        assert_eq!(spec.seed, Some(7));
        assert_eq!(filtered.len(), 2);
        for inst in filtered {
            assert!(["a", "b", "c", "d", "e"].contains(&inst.instance_id.as_str()));
        }
    }

    #[test]
    fn subset_empty_set_errors() {
        let instances = vec![SweBenchInstance {
            instance_id: "a".into(),
            repo: None,
            base_commit: None,
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        }];
        let err = apply_subset(instances, Some("a"), Some(0), None, None).unwrap_err();
        assert!(
            err.to_string().contains("produced zero instances"),
            "unexpected err: {err}"
        );
    }
}
