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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::error::Error;
use crate::model::litellm::is_anthropic_model;
use crate::model::{Model, ModelUsage};
use crate::redaction::{Redactor, surface};
use crate::trajectory::{FailureCategory, TokenUsage, Trajectory, exit_reason, outcome};

/// Sentinel `exit_reason` for tasks that never started because the
/// sweep-level USD budget was exhausted. Distinct from `error` and
/// `submitted` so summary tooling can attribute the halt correctly.
pub const EXIT_REASON_BUDGET_HALT: &str = "budget_halt";
pub const SWEEP_STATUS_RUNNING: &str = "running";
pub const SWEEP_STATUS_COMPLETED: &str = "completed";
pub const SWEEP_STATUS_CANCELLING: &str = "cancelling";
pub const SWEEP_STATUS_CANCELLED: &str = "cancelled";
pub const CANCEL_EXIT_CODE_GRACEFUL: i32 = 130;
pub const CANCEL_EXIT_CODE_ESCALATED: i32 = 137;

/// Standard `claude-3-5-sonnet` USD pricing per 1M tokens. Used for the
/// summary's cost estimate; per-instance trajectories carry only token
/// counts so downstream tooling can re-price as needed.
pub const SONNET_INPUT_USD_PER_MTOK: f64 = 3.0;
pub const SONNET_OUTPUT_USD_PER_MTOK: f64 = 15.0;
pub const ANTHROPIC_CACHE_READ_MULTIPLIER: f64 = 0.10;
pub const ANTHROPIC_CACHE_CREATION_MULTIPLIER: f64 = 1.25;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenBreakdown {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub completion_tokens: u64,
}

impl TokenBreakdown {
    #[must_use]
    pub fn prompt_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    #[must_use]
    pub fn total_tokens(self) -> u64 {
        self.prompt_tokens().saturating_add(self.completion_tokens)
    }

    #[must_use]
    pub fn has_billable_tokens(self) -> bool {
        self.prompt_tokens() > 0 || self.completion_tokens > 0
    }

    #[must_use]
    pub fn cache_hit_rate(self) -> f64 {
        let total_prompt = self.prompt_tokens();
        if total_prompt == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.cache_read_tokens as f64 / total_prompt as f64
        }
    }
}

#[must_use]
pub fn estimate_cost_usd(
    prompt_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    completion_tokens: u64,
    model: &str,
) -> f64 {
    let (cache_read_multiplier, cache_creation_multiplier) = if is_anthropic_model(model) {
        (
            ANTHROPIC_CACHE_READ_MULTIPLIER,
            ANTHROPIC_CACHE_CREATION_MULTIPLIER,
        )
    } else {
        (1.0, 1.0)
    };
    #[allow(clippy::cast_precision_loss)]
    let p = prompt_tokens as f64;
    #[allow(clippy::cast_precision_loss)]
    let cr = cache_read_tokens as f64;
    #[allow(clippy::cast_precision_loss)]
    let cc = cache_creation_tokens as f64;
    #[allow(clippy::cast_precision_loss)]
    let c = completion_tokens as f64;
    let input_cost = p / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK;
    let cache_read_cost = cr / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK * cache_read_multiplier;
    let cache_creation_cost =
        cc / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK * cache_creation_multiplier;
    let completion_cost = c / 1_000_000.0 * SONNET_OUTPUT_USD_PER_MTOK;
    input_cost + cache_read_cost + cache_creation_cost + completion_cost
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepSignal {
    Interrupt,
    Terminate,
}

fn default_sweep_status() -> String {
    SWEEP_STATUS_COMPLETED.to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
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
    #[serde(default, rename = "total_input_tokens", alias = "prompt_tokens")]
    pub prompt_tokens: Option<u64>,
    #[serde(
        default,
        rename = "total_cache_read_tokens",
        alias = "cache_read_tokens"
    )]
    pub cache_read_tokens: Option<u64>,
    #[serde(
        default,
        rename = "total_cache_creation_tokens",
        alias = "cache_creation_tokens"
    )]
    pub cache_creation_tokens: Option<u64>,
    #[serde(
        default,
        rename = "total_completion_tokens",
        alias = "completion_tokens"
    )]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
    /// Failure from the GitHub PR publication side effect. This is separate
    /// from `error` so a submitted patch stays submitted even if publishing
    /// the PR fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_pr_error: Option<String>,
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
    /// Number of independent samples requested for this instance. Legacy
    /// summaries that predate reruns deserialize as `0`; consumers should use
    /// `effective_runs` to treat those rows as single-shot.
    #[serde(default)]
    pub runs: u32,
    /// Number of resolved samples out of `runs`.
    #[serde(default)]
    pub resolved_count: u32,
    /// Whether the first sample resolved. Equivalent to the historical
    /// single-shot pass/fail value when `runs == 1`.
    #[serde(default)]
    pub pass_at_1: bool,
    /// Whether the agent issued at least one recognized test command before
    /// submitting.
    #[serde(default)]
    pub tests_run_before_submit: bool,
    /// Pass/fail value of the most recent recognized pre-submit test command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tests_passed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepResults {
    pub total: usize,
    #[serde(default = "default_sweep_status")]
    pub sweep_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_deadline_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_exit_code: Option<i32>,
    /// Terminal run slots completed before the first cancellation signal.
    #[serde(default)]
    pub completed: usize,
    /// Run slots that were in flight when the first cancellation signal arrived.
    #[serde(default)]
    pub in_flight_at_cancel: usize,
    /// Run slots that had not started when the first cancellation signal arrived.
    #[serde(default)]
    pub not_started: usize,
    pub submitted: usize,
    #[serde(default)]
    pub submitted_with_tests: usize,
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
    /// Instances downgraded from `submitted` because the captured diff was
    /// empty at capture time.
    #[serde(default)]
    pub patch_empty: usize,
    /// Instances downgraded from `submitted` because `git apply --check`
    /// rejected the patch at capture time.
    #[serde(default)]
    pub patch_apply_invalid: usize,
    /// GitHub PR publication failures across every run slot, including reruns
    /// that are collapsed out of aggregate instance rows.
    #[serde(default)]
    pub github_pr_failures: usize,
    #[serde(default, rename = "total_input_tokens", alias = "total_prompt_tokens")]
    pub total_prompt_tokens: u64,
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    #[serde(default)]
    pub total_completion_tokens: u64,
    #[serde(default, rename = "total_cost_usd", alias = "estimated_cost_usd")]
    pub estimated_cost_usd: f64,
    #[serde(default)]
    pub cache_hit_rate: f64,
    /// Total retry attempts executed across all instances.
    #[serde(default)]
    pub retries: u64,
    /// Number of instances that retried at least once.
    #[serde(default)]
    pub retried_instances: usize,
    /// Mean any-of-k resolution rate across instances, where k is each row's
    /// `runs` count. For single-shot sweeps this is equivalent to pass@1.
    #[serde(default)]
    pub pass_at_k: f64,
    /// Resolved dataset subset spec used for this run.
    #[serde(default)]
    pub filter_spec: FilterSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<ProvenanceManifest>,
    /// The USD ceiling enforced for this sweep, echoed from
    /// `SwebenchArgs::cost_limit_usd`. `None` when no limit was set —
    /// distinguishes "ran without a budget" from "budget was infinite".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_limit_usd: Option<f64>,
    #[serde(default)]
    pub instances: Vec<InstanceResult>,
    /// Rate-limit telemetry emitted when `--max-rpm` or `--max-input-tpm` is
    /// set. `None` when neither flag was provided (opt-in, no behavior change).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_events: Option<crate::run::rate_limit::RateLimitEvents>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stratify_by: Option<StratifyBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stratify_mode: Option<StratifyMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StratifyBy {
    Repo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StratifyMode {
    #[default]
    Proportional,
    Balanced,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub harness: HarnessManifest,
    pub dataset: DatasetManifest,
    pub prompt_template: PromptTemplateManifest,
    pub config: ConfigManifest,
    pub model: ModelManifest,
    pub runtime: RuntimeManifest,
    pub cli: CliManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessManifest {
    pub name: String,
    pub version: String,
    pub git_sha: Option<String>,
    pub git_dirty: Option<bool>,
    pub git_resolution: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetManifest {
    pub path: String,
    pub sha256: String,
    pub instance_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_spec: Option<FilterSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplateManifest {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigManifest {
    pub resolved: String,
    #[serde(default)]
    pub overlay_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    pub name: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[cfg(test)]
struct PanicAfterInitialManifestHook {
    output_dir: PathBuf,
}

#[cfg(test)]
static PANIC_AFTER_INITIAL_MANIFEST_WRITE: std::sync::Mutex<Option<PanicAfterInitialManifestHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn panic_after_initial_manifest_write_if_requested(output_dir: &Path) {
    let should_panic = {
        let mut hook = PANIC_AFTER_INITIAL_MANIFEST_WRITE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if hook
            .as_ref()
            .is_some_and(|hook| hook.output_dir == output_dir)
        {
            let _ = hook.take();
            true
        } else {
            false
        }
    };
    assert!(!should_panic, "test panic after initial manifest write");
}

#[cfg(test)]
struct SignalBeforeDispatchHook {
    output_dir: PathBuf,
    sender: mpsc::UnboundedSender<SweepSignal>,
}

#[cfg(test)]
static SIGNAL_BEFORE_NEXT_DISPATCH: std::sync::Mutex<Option<SignalBeforeDispatchHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn send_signal_before_next_dispatch_if_requested(output_dir: &Path) {
    let mut hook = SIGNAL_BEFORE_NEXT_DISPATCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if hook
        .as_ref()
        .is_some_and(|hook| hook.output_dir == output_dir)
    {
        if let Some(hook) = hook.take() {
            let _ = hook.sender.send(SweepSignal::Interrupt);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeManifest {
    pub started_at_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_utc: Option<String>,
    pub host_os: String,
    #[serde(default)]
    pub resume_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliManifest {
    pub argv: Vec<String>,
}

impl InstanceResult {
    #[must_use]
    pub fn token_breakdown(&self) -> TokenBreakdown {
        TokenBreakdown {
            input_tokens: self.prompt_tokens.unwrap_or(0),
            cache_read_tokens: self.cache_read_tokens.unwrap_or(0),
            cache_creation_tokens: self.cache_creation_tokens.unwrap_or(0),
            completion_tokens: self.completion_tokens.unwrap_or(0),
        }
    }

    #[must_use]
    pub fn effective_cost_usd(&self, model: Option<&str>) -> Option<f64> {
        let tokens = self.token_breakdown();
        if let Some(cost) = self.cost_usd {
            if cost != 0.0 || !tokens.has_billable_tokens() {
                return Some(cost);
            }
        }
        tokens.has_billable_tokens().then(|| {
            estimate_cost_usd(
                tokens.input_tokens,
                tokens.cache_read_tokens,
                tokens.cache_creation_tokens,
                tokens.completion_tokens,
                model.unwrap_or(""),
            )
        })
    }
}

impl SweepResults {
    #[must_use]
    pub fn token_breakdown(&self) -> TokenBreakdown {
        TokenBreakdown {
            input_tokens: self.total_prompt_tokens,
            cache_read_tokens: self.total_cache_read_tokens,
            cache_creation_tokens: self.total_cache_creation_tokens,
            completion_tokens: self.total_completion_tokens,
        }
    }

    /// Render the post-sweep summary table. A flat plain-text block so it
    /// reads cleanly in CI logs and from a tail of stdout.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn summary_table(&self) -> String {
        let submit_rate_pct = self.submit_rate_pct();
        let tokens = self.token_breakdown();
        let total_tokens = tokens.total_tokens();
        let effective_tasks = self.effective_task_count();
        let uniform_runs = self.uniform_runs_per_instance();
        let pass_at_k_label = uniform_runs.map_or_else(|| "k".to_owned(), |k| k.to_string());
        let mut s = String::new();
        s.push_str("\n=== SWE-bench sweep summary ===\n");
        let _ = writeln!(s, "Total tasks:        {}", self.total);
        if self.sweep_status != SWEEP_STATUS_COMPLETED {
            let _ = writeln!(s, "Sweep status:       {}", self.sweep_status);
            if self.sweep_status == SWEEP_STATUS_CANCELLED {
                let _ = writeln!(
                    s,
                    "Cancelled:          completed {}, in-flight {}, not-started {}",
                    self.completed, self.in_flight_at_cancel, self.not_started
                );
            }
        }
        write_effective_task_line(&mut s, self.total, effective_tasks, uniform_runs);
        write_submission_lines(
            &mut s,
            self.submitted,
            self.submitted_with_tests,
            &self.instances,
        );
        let _ = writeln!(
            s,
            "With patch:         {} — non-empty diff against base_commit",
            self.with_patch
        );
        if self.patch_empty > 0 || self.patch_apply_invalid > 0 {
            let _ = writeln!(
                s,
                "Patch-empty:        {} — empty diff downgraded from submitted",
                self.patch_empty
            );
            let _ = writeln!(
                s,
                "Patch-invalid:      {} — git apply --check failed at capture",
                self.patch_apply_invalid
            );
        }
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
        let _ = writeln!(
            s,
            "Pass@{pass_at_k_label}:            {:.2}%",
            self.pass_at_k * 100.0
        );
        let _ = writeln!(s, "Input tokens:       {}", self.total_prompt_tokens);
        let _ = writeln!(s, "Cache read tokens:  {}", self.total_cache_read_tokens);
        let _ = writeln!(
            s,
            "Cache create toks:  {}",
            self.total_cache_creation_tokens
        );
        let _ = writeln!(s, "Completion tokens:  {}", self.total_completion_tokens);
        let _ = writeln!(
            s,
            "Cache hit rate:     {:.2}%",
            tokens.cache_hit_rate() * 100.0
        );
        let _ = writeln!(s, "Total tokens:       {total_tokens}");
        let _ = writeln!(
            s,
            "Total cost:         ${:.4} (claude-3-5-sonnet @ ${SONNET_INPUT_USD_PER_MTOK}/MTok in, ${SONNET_OUTPUT_USD_PER_MTOK}/MTok out)",
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
        let model_name = self.manifest.as_ref().map(|m| m.model.name.as_str());
        write_spend_stats_by_resolution(&mut s, &self.instances, model_name);
        write_rate_limit_summary(&mut s, self.rate_limit_events.as_ref());
        s
    }

    #[allow(clippy::cast_precision_loss)]
    fn submit_rate_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.submitted as f64 / self.total as f64) * 100.0
        }
    }

    fn effective_task_count(&self) -> usize {
        self.instances
            .iter()
            .map(|row| usize::try_from(effective_runs(row)).unwrap_or(usize::MAX))
            .sum()
    }

    fn uniform_runs_per_instance(&self) -> Option<u32> {
        let mut iter = self.instances.iter().map(effective_runs);
        let first = iter.next()?;
        iter.all(|runs| runs == first).then_some(first)
    }
}

fn write_submission_lines(
    s: &mut String,
    submitted: usize,
    submitted_with_tests: usize,
    instances: &[InstanceResult],
) {
    let _ = writeln!(s, "Submitted:          {submitted}");
    let _ = writeln!(s, "Submitted w/tests:  {submitted_with_tests}/{submitted}");
    write_test_resolution_line(s, instances);
}

fn write_test_resolution_line(s: &mut String, instances: &[InstanceResult]) {
    let counts = test_behavior_resolution_counts(instances);
    let _ = writeln!(
        s,
        "Resolved by tests:  tests_run=true {}/{}, tests_run=false {}/{}",
        counts.resolved_with_tests,
        counts.with_tests,
        counts.resolved_without_tests,
        counts.without_tests
    );
}

#[derive(Debug, Clone, Copy, Default)]
struct TestBehaviorResolutionCounts {
    with_tests: usize,
    resolved_with_tests: usize,
    without_tests: usize,
    resolved_without_tests: usize,
}

fn test_behavior_resolution_counts(instances: &[InstanceResult]) -> TestBehaviorResolutionCounts {
    let mut counts = TestBehaviorResolutionCounts::default();
    for row in instances.iter().filter(|row| has_submitted_sample(row)) {
        if row.tests_run_before_submit {
            counts.with_tests += 1;
            if resolved_count(row) > 0 {
                counts.resolved_with_tests += 1;
            }
        } else {
            counts.without_tests += 1;
            if resolved_count(row) > 0 {
                counts.resolved_without_tests += 1;
            }
        }
    }
    counts
}

fn has_submitted_sample(row: &InstanceResult) -> bool {
    // Aggregate rerun rows keep run-1 outcome for pass@1 compatibility, so
    // later submitted/resolved samples are represented by `resolved_count`.
    row.outcome.as_deref() == Some(outcome::SUBMITTED) || resolved_count(row) > 0
}

fn write_spend_stats_by_resolution(
    s: &mut String,
    instances: &[InstanceResult],
    model: Option<&str>,
) {
    let mut resolved: Vec<f64> = instances
        .iter()
        .filter(|r| r.resolved_count > 0)
        .filter_map(|r| r.effective_cost_usd(model))
        .collect();
    let mut unresolved: Vec<f64> = instances
        .iter()
        .filter(|r| r.resolved_count == 0)
        .filter_map(|r| r.effective_cost_usd(model))
        .collect();
    write_spend_stat_line(s, "Spend/resolved  ", &mut resolved);
    write_spend_stat_line(s, "Spend/unresolved", &mut unresolved);
}

#[allow(clippy::cast_precision_loss)]
fn write_spend_stat_line(s: &mut String, label: &str, costs: &mut [f64]) {
    if costs.is_empty() {
        let _ = writeln!(s, "{label}:  n=0");
        return;
    }
    costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean = costs.iter().sum::<f64>() / costs.len() as f64;
    let n = costs.len();
    let median = if n % 2 == 0 {
        f64::midpoint(costs[n / 2 - 1], costs[n / 2])
    } else {
        costs[n / 2]
    };
    let _ = writeln!(s, "{label}:  mean=${mean:.4} median=${median:.4} n={n}");
}

fn write_rate_limit_summary(s: &mut String, rl: Option<&crate::run::rate_limit::RateLimitEvents>) {
    let Some(rl) = rl else { return };
    let _ = writeln!(s, "Rate-limit events:");
    let _ = writeln!(s, "  Throttled calls:    {}", rl.throttled_calls);
    let _ = writeln!(s, "  Throttled secs:     {:.1}", rl.total_throttled_seconds);
    let _ = writeln!(s, "  Peak concurrent:    {}", rl.peak_concurrent);
    if let Some(rpm) = rl.configured_max_rpm {
        let _ = writeln!(s, "  Configured max-rpm: {rpm}");
    }
    if let Some(tpm) = rl.configured_max_input_tpm {
        let _ = writeln!(s, "  Configured max-tpm: {tpm}");
    }
}

fn write_effective_task_line(
    s: &mut String,
    total_tasks: usize,
    effective_tasks: usize,
    uniform_runs: Option<u32>,
) {
    if effective_tasks == total_tasks {
        return;
    }
    if let Some(runs) = uniform_runs {
        let _ = writeln!(
            s,
            "Effective tasks:    {effective_tasks} ({total_tasks} instances * {runs} runs)"
        );
    } else {
        let _ = writeln!(
            s,
            "Effective tasks:    {effective_tasks} (sum of per-instance runs)"
        );
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct SwebenchArgs {
    pub dataset_path: PathBuf,
    pub output_dir: PathBuf,
    pub parallel: usize,
    pub config: Config,
    /// Independent samples per selected SWE-bench instance.
    pub reruns: u32,
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
    /// Optional wallclock budget applied independently to each task's agent
    /// loop. Orthogonal to step and cost limits; whichever fires first wins.
    pub task_timeout_secs: Option<u64>,
    /// Dataset subset selector. Either comma-separated ids or
    /// `@path/to/file.txt` (one id per line).
    pub instance_ids: Option<String>,
    /// Keep at most N instances after filtering + sampling.
    pub limit: Option<usize>,
    /// Reproducibly random-subset to N instances. Requires `seed`.
    pub sample: Option<usize>,
    /// RNG seed used by `sample`.
    pub seed: Option<u64>,
    /// Stratification key used while sampling.
    pub stratify_by: Option<StratifyBy>,
    /// Allocation mode used while stratifying.
    pub stratify_mode: StratifyMode,
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
    /// CLI-provided config overlay paths used to construct `config`.
    pub config_overlay_paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub skip_preflight: bool,
    pub preflight_format: String,
    pub skip_model_probe: bool,
    pub preflight_check_timeout_s: u64,
    pub preflight_total_timeout_s: u64,
    pub preflight_mode: String,
    /// When `true`, skip `git apply --check` and empty-diff validation after
    /// patch capture. Escape hatch for non-git environments; default is off.
    pub skip_patch_validation: bool,
    /// Optional aggregate request-rate ceiling across all workers (requests/min).
    /// When `None`, no RPM cap is enforced (opt-in, no behavior change).
    pub max_rpm: Option<u32>,
    /// Optional aggregate input-token-rate ceiling across all workers (tokens/min).
    /// When `None`, no TPM cap is enforced (opt-in, no behavior change).
    pub max_input_tpm: Option<u64>,
    /// Seconds to let in-flight tasks finish after the first cancellation
    /// signal before forcing them to persist `exit_reason = "cancelled"`.
    pub cancel_deadline_secs: u64,
    /// Install process OS signal handlers after preflight/dry-run handling,
    /// immediately before the worker loop starts listening for cancellation.
    pub install_os_signal_handlers: bool,
    /// Optional injected cancellation signal stream. Tests use this for
    /// deterministic cancellation; `None` disables injected signals.
    #[doc(hidden)]
    pub cancellation_signals: Option<mpsc::UnboundedReceiver<SweepSignal>>,
    /// Optional GitHub PR publisher for submitted patch artifacts.
    pub github_pr: Option<crate::run::github_pr::GithubPrSweepConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckStatus {
    Ok,
    Warn,
}

#[derive(Debug, Clone)]
struct CheckResult {
    status: CheckStatus,
    name: &'static str,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct PreflightReport {
    mode: String,
    checks: Vec<CheckResultOut>,
}

#[derive(Debug, Clone, Serialize)]
struct CheckResultOut {
    status: String,
    name: String,
    message: String,
}

fn default_attempts() -> u32 {
    1
}

/// Path where `run_one` writes the trajectory for an instance. Centralized so
/// the resume-skip check stays in lockstep with the writer.
#[must_use]
pub fn trajectory_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    trajectory_path_for_run(output_dir, instance_id, 1)
}

/// Path where the SWE-bench-style unified diff is written for an instance.
/// File presence is the resume-mode signal that the patch artifact was
/// captured for a previously-submitted run.
#[must_use]
pub fn patch_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    patch_path_for_run(output_dir, instance_id, 1)
}

#[must_use]
pub fn trajectory_path_for_run(
    output_dir: &std::path::Path,
    instance_id: &str,
    run_index: u32,
) -> PathBuf {
    output_dir
        .join(instance_id)
        .join(format!("run-{run_index}.traj.json"))
}

#[must_use]
pub fn patch_path_for_run(
    output_dir: &std::path::Path,
    instance_id: &str,
    run_index: u32,
) -> PathBuf {
    output_dir
        .join(instance_id)
        .join(format!("run-{run_index}.patch"))
}

fn legacy_trajectory_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    output_dir.join(format!("{instance_id}.traj.json"))
}

fn legacy_patch_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    output_dir.join(format!("{instance_id}.patch"))
}

fn existing_trajectory_path_for_run(
    output_dir: &std::path::Path,
    instance_id: &str,
    run_index: u32,
) -> PathBuf {
    let nested = trajectory_path_for_run(output_dir, instance_id, run_index);
    if nested.exists() || run_index != 1 {
        return nested;
    }
    legacy_trajectory_path_for(output_dir, instance_id)
}

fn existing_patch_path_for_run(
    output_dir: &std::path::Path,
    instance_id: &str,
    run_index: u32,
) -> PathBuf {
    let nested = patch_path_for_run(output_dir, instance_id, run_index);
    if nested.exists() || run_index != 1 {
        return nested;
    }
    legacy_patch_path_for(output_dir, instance_id)
}

/// Path of the aggregated SWE-bench predictions file written at the end of
/// a sweep. One JSONL line per submitted instance, in the schema sb-cli
/// expects (`instance_id`, `model_patch`, `model_name_or_path`).
#[must_use]
pub fn predictions_path(output_dir: &std::path::Path) -> PathBuf {
    output_dir.join("all_preds.jsonl")
}

#[must_use]
pub fn predictions_path_for_run(output_dir: &std::path::Path, run_index: u32) -> PathBuf {
    output_dir.join(format!("all_preds.run-{run_index}.jsonl"))
}

/// Inspect a trajectory path on disk. Returns `Some(info)` only when the file
/// exists *and* parses as valid trajectory JSON; truncated or corrupt files
/// (e.g. a mid-write crash) yield `None` so the task re-runs.
#[must_use]
pub fn existing_trajectory_info(
    output_dir: &std::path::Path,
    instance_id: &str,
) -> Option<crate::trajectory::TrajectoryInfo> {
    existing_trajectory_info_for_run(output_dir, instance_id, 1)
}

#[must_use]
pub fn existing_trajectory_info_for_run(
    output_dir: &std::path::Path,
    instance_id: &str,
    run_index: u32,
) -> Option<crate::trajectory::TrajectoryInfo> {
    read_trajectory_info(&existing_trajectory_path_for_run(
        output_dir,
        instance_id,
        run_index,
    ))
}

pub fn load_dataset(path: &std::path::Path) -> Result<Vec<SweBenchInstance>, Error> {
    let text = std::fs::read_to_string(path)?;
    parse_dataset_lines(&text)
}

fn load_dataset_from_bytes(bytes: &[u8]) -> Result<Vec<SweBenchInstance>, Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Error::Trajectory(format!("dataset utf8 decode: {e}")))?;
    parse_dataset_lines(text)
}

fn parse_dataset_lines(text: &str) -> Result<Vec<SweBenchInstance>, Error> {
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
pub async fn run(mut args: SwebenchArgs) -> Result<SweepResults, Error> {
    if args.reruns == 0 {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "bench swebench: --rerun must be at least 1".into(),
        )));
    }
    if !args.skip_preflight {
        let report = run_preflight(&args).await?;
        print_preflight_report(&report, &args.preflight_format, &args.preflight_mode)?;
    }
    if args.dry_run {
        if !args.skip_preflight && args.preflight_format != "json" {
            println!("preflight checks passed");
        } else if args.skip_preflight && args.preflight_format != "json" {
            println!("preflight skipped");
        }
        return Ok(SweepResults {
            total: 0,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 0,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: args.cost_limit_usd,
            instances: Vec::new(),
            rate_limit_events: None,
        });
    }
    std::fs::create_dir_all(&args.output_dir)?;
    let started_at_utc = chrono::Utc::now().to_rfc3339();
    let prior_results = if args.resume {
        load_prior_results_by_instance(&args.output_dir)
    } else {
        PriorResults::default()
    };
    let retry_policy = RetryPolicy::from_args(
        args.max_retries,
        args.retry_on.as_deref(),
        args.retry_backoff_base_ms,
        args.retry_backoff_cap_s,
    )?;

    let dataset_bytes = std::fs::read(&args.dataset_path)?;
    let dataset_sha = sha256_hex(&dataset_bytes);
    let instances = load_dataset_from_bytes(&dataset_bytes)?;
    let (instances, filter_spec) = apply_subset(
        instances,
        &ApplySubsetParams {
            instance_ids_arg: args.instance_ids.as_deref(),
            limit: args.limit,
            sample: args.sample,
            seed: args.seed,
            stratify_by: args.stratify_by,
            stratify_mode: args.stratify_mode,
        },
    )?;
    let total = instances.len();
    let summary_path = args.output_dir.join("results.json");
    let initial_manifest = build_manifest(
        &args,
        &dataset_sha,
        total,
        &filter_spec,
        &started_at_utc,
        None,
    );
    let initial = SweepResults {
        total,
        sweep_status: SWEEP_STATUS_RUNNING.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 0,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 0,
        estimated_cost_usd: 0.0,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: 0.0,
        filter_spec: filter_spec.clone(),
        manifest: Some(initial_manifest),
        cost_limit_usd: args.cost_limit_usd,
        instances: Vec::new(),
        rate_limit_events: None,
    };
    write_sweep_results_atomic(&summary_path, &initial)?;
    #[cfg(test)]
    panic_after_initial_manifest_write_if_requested(&args.output_dir);
    let mut set = tokio::task::JoinSet::new();
    let mut skipped_results: Vec<RunSlotResult> = Vec::new();
    let mut pending: std::collections::VecDeque<SweepRun> = std::collections::VecDeque::new();

    // Sweep-level cumulative USD spend. Prefer the provider-recorded
    // per-instance `cost_usd`; fall back to token-based re-pricing for
    // older artifacts that only carried token counts.
    let mut cumulative_cost = 0.0f64;
    let mut halted = false;
    let limit = args.cost_limit_usd;
    let model_name = args.config.root.model.name.clone();
    let bump_cost = |cost: f64, cumulative: &mut f64, halted: &mut bool| {
        *cumulative += cost;
        if let Some(l) = limit {
            if *cumulative >= l {
                *halted = true;
            }
        }
    };

    // Create the rate-limit governor only when at least one flag is set.
    // Wrapping in Arc lets each spawned task share it without cloning.
    let governor_arc: Option<std::sync::Arc<crate::run::rate_limit::RateLimitGovernor>> =
        crate::run::rate_limit::RateLimitGovernor::new(
            args.max_rpm,
            args.max_input_tpm,
            u32::try_from(args.parallel.max(1)).unwrap_or(u32::MAX),
        )
        .map(std::sync::Arc::new);

    let mut in_flight: usize = 0;
    let (force_cancel_tx, force_cancel_rx) = watch::channel(false);
    let mut cancellation: Option<CancellationSnapshot> = None;
    let mut force_cancel_sent = false;

    for inst in instances {
        // Resume short-circuit: a valid on-disk trajectory + (when the run
        // was submitted) a patch file mean this task is fully archived
        // from a prior sweep. Skip it before we even consider dispatch —
        // no worker slot, no Docker container, no model API call.
        for run_index in 1..=args.reruns {
            if args.resume {
                if let Some(info) =
                    existing_trajectory_info_for_run(&args.output_dir, &inst.instance_id, run_index)
                {
                    let patch_path =
                        existing_patch_path_for_run(&args.output_dir, &inst.instance_id, run_index);
                    let needs_patch = info.outcome.as_deref() == Some(outcome::SUBMITTED);
                    let cancelled_resume =
                        info.exit_reason.as_deref() == Some(exit_reason::CANCELLED);
                    if cancelled_resume {
                        tracing::info!(
                            instance = %inst.instance_id,
                            run_index,
                            "resume: cancelled trajectory found — re-running"
                        );
                        pending.push_back(SweepRun {
                            inst: inst.clone(),
                            run_index,
                        });
                        continue;
                    }
                    let retryable_resume = args.retry_on_resume
                        && info
                            .failure_category
                            .is_some_and(|cat| retry_policy.is_retry_category(cat));
                    if retryable_resume {
                        let prior = resume_snapshot_for_run(
                            &args.output_dir,
                            &inst.instance_id,
                            run_index,
                            &info,
                            &patch_path,
                            &prior_results,
                        );
                        bump_cost(
                            prior.effective_cost_usd(Some(&model_name)).unwrap_or(0.0),
                            &mut cumulative_cost,
                            &mut halted,
                        );
                        if halted {
                            skipped_results.push(RunSlotResult::new(
                                run_index,
                                budget_halt_result(&inst.instance_id),
                            ));
                            continue;
                        }
                    }
                    if !retryable_resume && (!needs_patch || patch_path.exists()) {
                        let mut r = resume_snapshot_for_run(
                            &args.output_dir,
                            &inst.instance_id,
                            run_index,
                            &info,
                            &patch_path,
                            &prior_results,
                        );
                        let traj_path = existing_trajectory_path_for_run(
                            &args.output_dir,
                            &inst.instance_id,
                            run_index,
                        );
                        r = publish_github_pr_for_result(
                            r,
                            GithubPrPublication {
                                config: args.github_pr.as_ref(),
                                instance_id: &inst.instance_id,
                                run_index,
                                trajectory_path: &traj_path,
                                patch_path: &patch_path,
                            },
                        )
                        .await;
                        bump_cost(
                            r.effective_cost_usd(Some(&model_name)).unwrap_or(0.0),
                            &mut cumulative_cost,
                            &mut halted,
                        );
                        skipped_results.push(RunSlotResult::new(run_index, r));
                        continue;
                    }
                    tracing::info!(
                        instance = %inst.instance_id,
                        run_index,
                        "resume: trajectory present but patch missing — re-running"
                    );
                }
            }
            pending.push_back(SweepRun {
                inst: inst.clone(),
                run_index,
            });
        }
    }

    let mut results = skipped_results;
    let mut skipped = results
        .iter()
        .filter(|r| r.result.exit_reason == "skipped_resume")
        .count();
    let mut submitted = 0;
    let mut errored = 0;
    let mut budget_halted = results
        .iter()
        .filter(|r| r.result.exit_reason == EXIT_REASON_BUDGET_HALT)
        .count();
    let mut accounting = SweepAccounting::default();
    let parallelism = args.parallel.max(1);

    // Consumer-driven dispatch: spawn at most `parallelism` tasks at a
    // time, and only launch a fresh task once the consumer has processed
    // the previous result and (re-)checked the halt flag. A semaphore
    // would close the same race only after the new permit was claimed —
    // by which time another agent has already started an API call.
    for r in &results {
        accounting.add_result(&r.result);
    }

    let spawn_one =
        |run: SweepRun,
         set: &mut tokio::task::JoinSet<RunSlotResult>,
         governor: Option<std::sync::Arc<crate::run::rate_limit::RateLimitGovernor>>| {
            let params = RunOneParams {
                output_dir: args.output_dir.clone(),
                cfg: args.config.clone(),
                deterministic_responses: args.deterministic_responses.clone(),
                deterministic_usage_per_call: args.deterministic_usage_per_call.clone(),
                retry_policy: retry_policy.clone(),
                task_timeout_secs: args.task_timeout_secs,
                skip_patch_validation: args.skip_patch_validation,
                governor,
                cancellation: crate::run::mini::MiniCancellation::new(force_cancel_rx.clone()),
                github_pr: args.github_pr.clone(),
            };
            set.spawn(async move {
                RunSlotResult::new(
                    run.run_index,
                    run_one(run.inst, run.run_index, params).await,
                )
            });
        };

    if halted {
        // Resume already exhausted the budget; everything that was queued
        // never starts.
        while let Some(inst) = pending.pop_front() {
            results.push(RunSlotResult::new(
                inst.run_index,
                budget_halt_result(&inst.inst.instance_id),
            ));
            budget_halted += 1;
        }
    } else {
        // Initial fill.
        for _ in 0..parallelism {
            if let Some(inst) = pending.pop_front() {
                spawn_one(inst, &mut set, governor_arc.clone());
                in_flight += 1;
                if let Some(g) = &governor_arc {
                    g.update_peak_concurrent(u32::try_from(in_flight).unwrap_or(u32::MAX))
                        .await;
                }
            } else {
                break;
            }
        }
    }

    let mut signal_rx = if in_flight > 0 {
        args.cancellation_signals.take().or_else(|| {
            args.install_os_signal_handlers
                .then(os_cancellation_signals)
        })
    } else {
        None
    };
    let mut signal_rx_closed = signal_rx.is_none();

    while in_flight > 0 {
        enum SweepEvent {
            Joined(Box<Option<Result<RunSlotResult, tokio::task::JoinError>>>),
            Signal(Option<SweepSignal>),
            DeadlineElapsed,
        }
        let deadline = cancellation
            .as_ref()
            .filter(|_| !force_cancel_sent)
            .map(|cancel| cancel.deadline);
        let event = tokio::select! {
            joined = set.join_next() => SweepEvent::Joined(Box::new(joined)),
            signal = async {
                if let Some(rx) = signal_rx.as_mut() {
                    rx.recv().await
                } else {
                    std::future::pending::<Option<SweepSignal>>().await
                }
            }, if !signal_rx_closed => SweepEvent::Signal(signal),
            () = async {
                if let Some(deadline) = deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if deadline.is_some() => SweepEvent::DeadlineElapsed,
        };

        match event {
            SweepEvent::Joined(joined) => {
                let Some(j) = *joined else {
                    break;
                };
                in_flight = in_flight.saturating_sub(1);
                match j {
                    Ok(r) => {
                        match r.result.outcome.as_deref() {
                            Some(outcome::SUBMITTED) => submitted += 1,
                            Some(outcome::ERROR) => errored += 1,
                            _ => {}
                        }
                        accounting.add_result(&r.result);
                        // Sweep-level budget bookkeeping. Tasks that completed
                        // (whether submitted or errored) consumed real API budget
                        // and count toward the cap.
                        let cost = r
                            .result
                            .effective_cost_usd(Some(&model_name))
                            .unwrap_or(0.0);
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
                        results.push(RunSlotResult::new(
                            1,
                            InstanceResult {
                                instance_id: "<join_error>".into(),
                                exit_reason: "error".into(),
                                outcome: Some(outcome::ERROR.into()),
                                failure_category: Some(FailureCategory::AgentInternal),
                                steps: None,
                                cost_usd: None,
                                prompt_tokens: None,
                                cache_read_tokens: None,
                                cache_creation_tokens: None,
                                completion_tokens: None,
                                duration_secs: None,
                                error: Some(e.to_string()),
                                github_pr_error: None,
                                patch_present: false,
                                non_empty_patch: false,
                                attempts: 1,
                                retry_reasons: Vec::new(),
                                runs: 1,
                                resolved_count: 0,
                                pass_at_1: false,
                                tests_run_before_submit: false,
                                last_tests_passed: None,
                            },
                        ));
                    }
                }
            }
            SweepEvent::Signal(Some(signal)) => {
                apply_sweep_signal(
                    signal,
                    CancellationSignalContext {
                        initial: &initial,
                        summary_path: &summary_path,
                        results: &results,
                        reruns: args.reruns,
                        cancel_deadline_secs: args.cancel_deadline_secs,
                        in_flight,
                        pending_len: pending.len(),
                        submitted,
                        skipped,
                        errored,
                        budget_halted,
                    },
                    &mut cancellation,
                    &mut force_cancel_sent,
                    &force_cancel_tx,
                )?;
            }
            SweepEvent::Signal(None) => {
                signal_rx_closed = true;
            }
            SweepEvent::DeadlineElapsed => {
                if !force_cancel_sent {
                    force_cancel_sent = true;
                    let _ = force_cancel_tx.send(true);
                    tracing::warn!("cancel deadline elapsed — forcing in-flight cancellation");
                }
            }
        }

        // Decide what to do with the next pending task. Once cancellation
        // starts, pending work remains unstarted for resume.
        if cancellation.is_none() {
            if let Some(g) = &governor_arc {
                if g.tick_aimd().await {
                    tracing::info!(
                        structured_event = "aimd_restore",
                        "rate-limit: AIMD restored 1 worker slot"
                    );
                }
            }
            let aimd_suppressed_count = match &governor_arc {
                Some(g) => g.suppressed_slots_count().await,
                None => 0,
            };
            let effective_parallelism = parallelism.saturating_sub(aimd_suppressed_count as usize);
            #[cfg(test)]
            send_signal_before_next_dispatch_if_requested(&args.output_dir);
            if let Some(signal) =
                take_pending_cancellation_signal(&mut signal_rx, &mut signal_rx_closed)
            {
                apply_sweep_signal(
                    signal,
                    CancellationSignalContext {
                        initial: &initial,
                        summary_path: &summary_path,
                        results: &results,
                        reruns: args.reruns,
                        cancel_deadline_secs: args.cancel_deadline_secs,
                        in_flight,
                        pending_len: pending.len(),
                        submitted,
                        skipped,
                        errored,
                        budget_halted,
                    },
                    &mut cancellation,
                    &mut force_cancel_sent,
                    &force_cancel_tx,
                )?;
            }
            if cancellation.is_none() {
                if halted {
                    while let Some(inst) = pending.pop_front() {
                        results.push(RunSlotResult::new(
                            inst.run_index,
                            budget_halt_result(&inst.inst.instance_id),
                        ));
                        budget_halted += 1;
                    }
                } else if in_flight < effective_parallelism {
                    if let Some(inst) = pending.pop_front() {
                        spawn_one(inst, &mut set, governor_arc.clone());
                        in_flight += 1;
                        if let Some(g) = &governor_arc {
                            g.update_peak_concurrent(u32::try_from(in_flight).unwrap_or(u32::MAX))
                                .await;
                        }
                    }
                }
            }
        }
    }

    write_predictions_file(
        &args.output_dir,
        &mut results,
        &args.config.root.model.name,
        &args.config.root.redaction,
    )?;

    skipped = results
        .iter()
        .filter(|r| r.result.exit_reason == "skipped_resume")
        .count();
    submitted = results
        .iter()
        .filter(|r| {
            r.result.exit_reason != "skipped_resume"
                && r.result.outcome.as_deref() == Some(outcome::SUBMITTED)
        })
        .count();
    errored = results
        .iter()
        .filter(|r| r.result.outcome.as_deref() == Some(outcome::ERROR))
        .count();
    budget_halted = results
        .iter()
        .filter(|r| r.result.exit_reason == EXIT_REASON_BUDGET_HALT)
        .count();
    accounting = SweepAccounting::default();
    for r in &results {
        accounting.add_result(&r.result);
    }

    let instance_results = aggregate_run_results(&results, args.reruns);
    let pass_at_k = pass_at_k(&instance_results);
    let mut failures_by_category: BTreeMap<FailureCategory, usize> = BTreeMap::new();
    for r in &results {
        if let Some(cat) = r.result.failure_category {
            *failures_by_category.entry(cat).or_insert(0) += 1;
        }
    }

    let token_breakdown = accounting.tokens;
    let total_cost_usd = sum_f64(
        instance_results
            .iter()
            .filter_map(|result| result.effective_cost_usd(Some(&model_name))),
    )
    .unwrap_or_else(|| {
        estimate_cost_usd(
            token_breakdown.input_tokens,
            token_breakdown.cache_read_tokens,
            token_breakdown.cache_creation_tokens,
            token_breakdown.completion_tokens,
            &model_name,
        )
    });

    let patch_empty = failures_by_category
        .get(&FailureCategory::PatchEmpty)
        .copied()
        .unwrap_or(0);
    let patch_apply_invalid = failures_by_category
        .get(&FailureCategory::PatchApplyInvalid)
        .copied()
        .unwrap_or(0);

    let mut sweep = SweepResults {
        total,
        sweep_status: SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted,
        submitted_with_tests: submitted_with_tests_for_fresh_submissions(&results),
        skipped,
        errored,
        failures_by_category,
        budget_halted,
        with_patch: accounting.with_patch,
        patch_empty,
        patch_apply_invalid,
        github_pr_failures: github_pr_failure_count_for_run_slots(&results),
        total_prompt_tokens: token_breakdown.input_tokens,
        total_cache_read_tokens: token_breakdown.cache_read_tokens,
        total_cache_creation_tokens: token_breakdown.cache_creation_tokens,
        total_completion_tokens: token_breakdown.completion_tokens,
        estimated_cost_usd: total_cost_usd,
        cache_hit_rate: token_breakdown.cache_hit_rate(),
        retries: accounting.total_retries,
        retried_instances: accounting.retried_instances,
        pass_at_k,
        filter_spec,
        manifest: Some(build_manifest(
            &args,
            &dataset_sha,
            total,
            &initial.filter_spec,
            &started_at_utc,
            Some(chrono::Utc::now().to_rfc3339()),
        )),
        cost_limit_usd: args.cost_limit_usd,
        instances: instance_results,
        rate_limit_events: match governor_arc.as_ref() {
            Some(g) => Some(g.events().await),
            None => None,
        },
    };
    if let Some(cancel) = cancellation.as_ref() {
        sweep.sweep_status = SWEEP_STATUS_CANCELLED.into();
        sweep.cancelled_at = Some(cancel.cancelled_at.clone());
        sweep.cancel_deadline_at = Some(cancel.deadline_at.clone());
        sweep.cancel_exit_code = Some(cancel.exit_code);
        sweep.completed = cancel.completed;
        sweep.in_flight_at_cancel = cancel.in_flight_at_cancel;
        sweep.not_started = cancel.not_started;
    }
    write_sweep_results_atomic(&summary_path, &sweep)?;

    Ok(sweep)
}

fn write_sweep_results_atomic(path: &Path, results: &SweepResults) -> Result<(), Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(temp.as_file_mut(), results)?;
    writeln!(temp.as_file_mut())?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

pub fn os_cancellation_signals() -> mpsc::UnboundedReceiver<SweepSignal> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(forward_os_cancellation_signals(tx));
    rx
}

#[cfg(unix)]
async fn forward_os_cancellation_signals(tx: mpsc::UnboundedSender<SweepSignal>) {
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(stream) => stream,
        Err(err) => {
            tracing::warn!(error = %err, "failed to install SIGTERM handler");
            return;
        }
    };
    loop {
        let signal = tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(err) = result {
                    tracing::warn!(error = %err, "failed waiting for Ctrl-C");
                    return;
                }
                SweepSignal::Interrupt
            }
            _ = sigterm.recv() => SweepSignal::Terminate,
        };
        if tx.send(signal).is_err() {
            return;
        }
    }
}

#[cfg(not(unix))]
async fn forward_os_cancellation_signals(tx: mpsc::UnboundedSender<SweepSignal>) {
    loop {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %err, "failed waiting for Ctrl-C");
            return;
        }
        if tx.send(SweepSignal::Interrupt).is_err() {
            return;
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_preflight(args: &SwebenchArgs) -> Result<Vec<CheckResult>, Error> {
    let deadline = Instant::now() + Duration::from_secs(args.preflight_total_timeout_s);
    let mut checks = Vec::new();
    let dataset_path = args.dataset_path.clone();
    let dataset_bytes = timed_sync(
        "dataset.read",
        args.preflight_check_timeout_s,
        deadline,
        move || std::fs::read(&dataset_path),
    )
    .await?;
    checks.push(CheckResult {
        status: CheckStatus::Ok,
        name: "dataset.read",
        message: format!("readable: {}", args.dataset_path.display()),
    });
    let instances = timed_sync(
        "dataset.parse",
        args.preflight_check_timeout_s,
        deadline,
        move || load_dataset_from_bytes(&dataset_bytes),
    )
    .await?;
    let instance_ids = args.instance_ids.clone();
    let limit = args.limit;
    let sample = args.sample;
    let seed = args.seed;
    let stratify_by = args.stratify_by;
    let stratify_mode = args.stratify_mode;
    let (subset, _) = timed_sync(
        "dataset.subset",
        args.preflight_check_timeout_s,
        deadline,
        move || {
            apply_subset(
                instances,
                &ApplySubsetParams {
                    instance_ids_arg: instance_ids.as_deref(),
                    limit,
                    sample,
                    seed,
                    stratify_by,
                    stratify_mode,
                },
            )
        },
    )
    .await?;
    checks.push(CheckResult {
        status: CheckStatus::Ok,
        name: "dataset.parse",
        message: format!("valid jsonl, selected {} instances", subset.len()),
    });
    checks.push(CheckResult {
        status: CheckStatus::Ok,
        name: "output.parent",
        message: "will be created if missing".into(),
    });
    if args.output_dir.join("results.json").exists() && !args.resume {
        checks.push(CheckResult {
            status: CheckStatus::Warn,
            name: "output.results_json",
            message: "results.json exists and --resume is false".into(),
        });
    }
    ensure_total_deadline(deadline)?;
    match args.config.root.environment.kind {
        crate::config::EnvKind::Local => {
            for bin in local_preflight_tools() {
                ensure_total_deadline(deadline)?;
                let ok = timed_sync(
                    "env.local_tools",
                    args.preflight_check_timeout_s,
                    deadline,
                    move || local_tool_probe(bin).output(),
                )
                .await?
                .status
                .success();
                if !ok {
                    return Err(Error::Trajectory(format!("required binary missing: {bin}")));
                }
            }
            checks.push(CheckResult {
                status: CheckStatus::Ok,
                name: "env.local_tools",
                message: "bash/git/patch are on PATH".into(),
            });
        }
        crate::config::EnvKind::Docker => {
            #[cfg(feature = "docker")]
            {
                ensure_total_deadline(deadline)?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                let per_check = Duration::from_secs(args.preflight_check_timeout_s);
                let budget = remaining.min(per_check);
                tokio::time::timeout(budget, crate::env::docker::preflight())
                    .await
                    .map_err(|_| Error::Trajectory("docker preflight timed out".into()))??;
                checks.push(CheckResult {
                    status: CheckStatus::Ok,
                    name: "env.docker",
                    message: "docker daemon reachable".into(),
                });
            }
            #[cfg(not(feature = "docker"))]
            {
                return Err(Error::Trajectory(
                    "environment.kind=docker requires binary built with `docker` feature".into(),
                ));
            }
        }
    }
    if !args.skip_model_probe {
        ensure_total_deadline(deadline)?;
        let backend = crate::model::LitellmBackend::new(args.config.root.model.name.clone());
        let msgs = vec![crate::model::Message::user("Reply with exactly: ok")];
        let opts = crate::model::QueryOpts {
            max_tokens: Some(1),
            ..crate::model::QueryOpts::default()
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let per_check = Duration::from_secs(args.preflight_check_timeout_s);
        let budget = remaining.min(per_check);
        let _ = tokio::time::timeout(budget, backend.query(&msgs, &opts))
            .await
            .map_err(|_| Error::Trajectory("model probe timed out".into()))?
            .map_err(|e| Error::Trajectory(format!("model probe failed: {e}")))?;
        checks.push(CheckResult {
            status: CheckStatus::Ok,
            name: "model.probe",
            message: "litellm 1-token probe succeeded".into(),
        });
    }
    let renderer = crate::template::Renderer::new();
    let fixture =
        serde_json::json!({"task":"demo","instance_id":"x","repo":"r","base_commit":"abc"});
    let _ = renderer.render_str(&args.config.root.prompts.system, &fixture)?;
    let _ = renderer.render_str(&args.config.root.prompts.instance, &fixture)?;
    checks.push(CheckResult {
        status: CheckStatus::Ok,
        name: "prompt.templates",
        message: "system/instance templates parse + render".into(),
    });
    ensure_total_deadline(deadline)?;
    if let Some(obj) = args.config.raw.as_object() {
        let known: std::collections::HashSet<&str> =
            ["agent", "model", "environment", "prompts", "extends"]
                .into_iter()
                .collect();
        for k in obj.keys() {
            if !known.contains(k.as_str()) {
                checks.push(CheckResult {
                    status: CheckStatus::Warn,
                    name: "config.unknown_top_level",
                    message: format!("unknown top-level key: {k}"),
                });
            }
        }
    }
    Ok(checks)
}

fn render_preflight_report(
    checks: &[CheckResult],
    format: &str,
    mode: &str,
) -> Result<String, Error> {
    if format == "json" {
        let payload = PreflightReport {
            mode: mode.into(),
            checks: checks
                .iter()
                .map(|c| CheckResultOut {
                    status: match c.status {
                        CheckStatus::Ok => "ok",
                        CheckStatus::Warn => "warn",
                    }
                    .into(),
                    name: c.name.into(),
                    message: c.message.clone(),
                })
                .collect(),
        };
        return serde_json::to_string_pretty(&payload)
            .map_err(|e| Error::Trajectory(format!("preflight json encode: {e}")));
    }
    let mut out = String::new();
    for c in checks {
        let s = match c.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
        };
        let _ = writeln!(out, "[{s}] {} — {}", c.name, c.message);
    }
    Ok(out)
}

fn print_preflight_report(checks: &[CheckResult], format: &str, mode: &str) -> Result<(), Error> {
    if format == "silent" {
        return Ok(());
    }
    print!("{}", render_preflight_report(checks, format, mode)?);
    Ok(())
}

#[cfg(windows)]
fn local_preflight_tools() -> &'static [&'static str] {
    &["cmd.exe", "git"]
}

#[cfg(not(windows))]
fn local_preflight_tools() -> &'static [&'static str] {
    &["bash", "git", "patch"]
}

#[cfg(windows)]
fn local_tool_probe(bin: &str) -> Command {
    let mut cmd = Command::new("where.exe");
    cmd.arg(bin);
    cmd
}

#[cfg(not(windows))]
fn local_tool_probe(bin: &str) -> Command {
    let mut cmd = Command::new("which");
    cmd.arg(bin);
    cmd
}

async fn timed_sync<T, E, F>(
    name: &str,
    timeout_s: u64,
    deadline: Instant,
    f: F,
) -> Result<T, Error>
where
    T: Send + 'static,
    E: Send + 'static + std::fmt::Display,
    F: Send + 'static + FnOnce() -> Result<T, E>,
{
    ensure_total_deadline(deadline)?;
    let rem = deadline.saturating_duration_since(Instant::now());
    let per = Duration::from_secs(timeout_s);
    let budget = rem.min(per);
    let h = tokio::task::spawn_blocking(f);
    let out = tokio::time::timeout(budget, h)
        .await
        .map_err(|_| Error::Trajectory(format!("{name}: timeout exceeded")))?
        .map_err(|e| Error::Trajectory(format!("{name}: join error: {e}")))?
        .map_err(|e| Error::Trajectory(format!("{name}: {e}")))?;
    ensure_total_deadline(deadline)?;
    Ok(out)
}

fn ensure_total_deadline(deadline: Instant) -> Result<(), Error> {
    if Instant::now() > deadline {
        return Err(Error::Trajectory("preflight total timeout exceeded".into()));
    }
    Ok(())
}

fn build_manifest(
    args: &SwebenchArgs,
    dataset_sha: &str,
    dataset_instance_count: usize,
    filter_spec: &FilterSpec,
    started_at_utc: &str,
    finished_at_utc: Option<String>,
) -> ProvenanceManifest {
    let prompt_source = format!(
        "{}\n---\n{}",
        args.config.root.prompts.system, args.config.root.prompts.instance
    );
    let mut config_raw = effective_runtime_config(&args.config);
    let redactor = Redactor::from_config_lossy(&args.config.root.redaction);
    redactor.redact_json_value(&mut config_raw, surface::TRAJECTORY);
    let resolved = toml::to_string(&strip_json_nulls(config_raw)).unwrap_or_default();
    ProvenanceManifest {
        purpose: None,
        harness: resolve_harness_manifest(),
        dataset: DatasetManifest {
            path: args.dataset_path.display().to_string(),
            sha256: dataset_sha.to_owned(),
            instance_count: dataset_instance_count,
            filter_spec: Some(filter_spec.clone()),
        },
        prompt_template: PromptTemplateManifest {
            source: "builtin".into(),
            path: None,
            sha256: sha256_hex(prompt_source.as_bytes()),
        },
        config: ConfigManifest {
            resolved,
            overlay_paths: args
                .config_overlay_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        },
        model: ModelManifest {
            name: args.config.root.model.name.clone(),
            backend: if args.deterministic_responses.is_some() {
                "deterministic".into()
            } else {
                "litellm".into()
            },
            backend_version: if args.deterministic_responses.is_some() {
                None
            } else {
                litellm_rs_version()
            },
            base_url: model_base_url(),
        },
        runtime: RuntimeManifest {
            started_at_utc: started_at_utc.to_owned(),
            finished_at_utc,
            host_os: std::env::consts::OS.into(),
            resume_mode: args.resume,
            rust_version: rust_version(),
        },
        cli: CliManifest {
            argv: redact_argv(std::env::args().collect(), &args.config.root.redaction),
        },
    }
}

fn effective_runtime_config(cfg: &Config) -> serde_json::Value {
    let mut raw = cfg.raw.clone();
    if let serde_json::Value::Object(ref mut m) = raw {
        if let Ok(agent) = serde_json::to_value(&cfg.root.agent) {
            m.insert("agent".into(), agent);
        }
        if let Ok(model) = serde_json::to_value(&cfg.root.model) {
            m.insert("model".into(), model);
        }
        if let Ok(environment) = serde_json::to_value(&cfg.root.environment) {
            m.insert("environment".into(), environment);
        }
        if let Ok(prompts) = serde_json::to_value(&cfg.root.prompts) {
            m.insert("prompts".into(), prompts);
        }
        if let Ok(redaction) = serde_json::to_value(&cfg.root.redaction) {
            m.insert("redaction".into(), redaction);
        }
    }
    raw
}

fn resolve_harness_manifest() -> HarnessManifest {
    resolve_harness_manifest_for_dir(None)
}

fn resolve_harness_manifest_for_dir(cwd: Option<&Path>) -> HarnessManifest {
    let name = env!("CARGO_PKG_NAME").to_owned();
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let sha_res = run_git(cwd, &["rev-parse", "HEAD"], false);
    let status_res = run_git(cwd, &["status", "--porcelain"], true);
    let sha = sha_res.as_ref().ok().cloned();
    let dirty = status_res.as_ref().ok().map(|s| !s.trim().is_empty());
    let git_resolution = match (&sha_res, &status_res) {
        (Ok(_), Ok(_)) => "ok".to_owned(),
        (Err(a), Err(b)) => format!("rev_parse_failed:{a};status_failed:{b}"),
        (Err(a), Ok(_)) => format!("rev_parse_failed:{a}"),
        (Ok(_), Err(b)) => format!("status_failed:{b}"),
    };
    HarnessManifest {
        name,
        version,
        git_sha: sha,
        git_dirty: dirty,
        git_resolution,
    }
}

fn run_git(cwd: Option<&Path>, args: &[&str], allow_empty: bool) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8(out.stderr).unwrap_or_else(|_| "non-utf8 stderr".into());
        return Err(stderr.trim().to_owned());
    }
    let text = String::from_utf8(out.stdout).map_err(|e| e.to_string())?;
    let trimmed = text.trim().to_owned();
    if trimmed.is_empty() && !allow_empty {
        return Err("empty output".into());
    }
    Ok(trimmed)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn rust_version() -> Option<String> {
    Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| o.status.success().then_some(o.stdout))
        .and_then(|s| String::from_utf8(s).ok())
        .map(|s| s.trim().to_owned())
}

fn model_base_url() -> Option<String> {
    let base = std::env::var("LITELLM_BASE_URL")
        .ok()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok());
    base.and_then(|b| {
        let lb = b.trim().to_ascii_lowercase();
        (lb != "https://api.openai.com/v1").then_some(b)
    })
}

fn redact_argv(argv: Vec<String>, redaction_cfg: &crate::config::RedactionCfg) -> Vec<String> {
    let redactor = Redactor::from_config_lossy(redaction_cfg);
    let mut out = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for arg in argv {
        let lower = arg.to_ascii_lowercase();
        if redact_next {
            out.push("<redacted>".into());
            redact_next = false;
            continue;
        }
        if lower.starts_with("--") && flag_name_is_sensitive(&arg) {
            if let Some((k, _)) = arg.split_once('=') {
                out.push(format!("{k}=<redacted>"));
            } else {
                out.push(arg);
                redact_next = true;
            }
            continue;
        }
        let redacted_arg = redactor.redact_text(&arg, surface::TRAJECTORY);
        if redacted_arg.redacted {
            out.push(redacted_arg.text);
            continue;
        }
        out.push(arg);
    }
    out
}

fn strip_json_nulls(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => serde_json::Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_json_nulls(v)))
                .collect(),
        ),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(strip_json_nulls).collect())
        }
        other => other,
    }
}

fn flag_name_is_sensitive(flag: &str) -> bool {
    let body = flag
        .trim_start_matches('-')
        .split('=')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        body.as_str(),
        "api-key"
            | "apikey"
            | "key"
            | "token"
            | "secret"
            | "password"
            | "access-token"
            | "auth-token"
            | "bearer-token"
    ) || body.ends_with("-key")
        || body.ends_with("-token")
        || body.ends_with("-secret")
        || body.ends_with("-password")
}

fn litellm_rs_version() -> Option<String> {
    let lock = include_str!("../../Cargo.lock");
    let mut lines = lock.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() == "name = \"litellm-rs\"" {
            while let Some(next) = lines.peek() {
                let n = next.trim();
                if n.starts_with("version = \"") {
                    return n
                        .strip_prefix("version = \"")
                        .and_then(|s| s.strip_suffix('"'))
                        .map(ToOwned::to_owned);
                }
                if n.starts_with("name = ") || n == "[[package]]" {
                    break;
                }
                lines.next();
            }
        }
    }
    None
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
        cache_read_tokens: None,
        cache_creation_tokens: None,
        completion_tokens: None,
        duration_secs: None,
        error: None,
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: 0,
        pass_at_1: false,
        tests_run_before_submit: false,
        last_tests_passed: None,
    }
}

struct CancelledWaitContext<'a> {
    output_dir: &'a Path,
    instance_id: &'a str,
    run_index: u32,
    task: &'a str,
    model_name: &'a str,
    attempts: u32,
    retry_reasons: &'a [FailureCategory],
    current: Option<InstanceResult>,
}

fn cancelled_wait_result(ctx: CancelledWaitContext<'_>) -> InstanceResult {
    persist_cancelled_wait_trajectory(
        ctx.output_dir,
        ctx.instance_id,
        ctx.run_index,
        ctx.task,
        ctx.model_name,
    );
    let mut result = ctx.current.unwrap_or_else(|| InstanceResult {
        instance_id: ctx.instance_id.to_owned(),
        exit_reason: exit_reason::CANCELLED.into(),
        outcome: Some(outcome::ERROR.into()),
        failure_category: None,
        steps: Some(0),
        cost_usd: Some(0.0),
        prompt_tokens: Some(0),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(0),
        duration_secs: Some(0.0),
        error: None,
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: ctx.attempts.max(1),
        retry_reasons: ctx.retry_reasons.to_vec(),
        runs: 1,
        resolved_count: 0,
        pass_at_1: false,
        tests_run_before_submit: false,
        last_tests_passed: None,
    });
    ctx.instance_id.clone_into(&mut result.instance_id);
    result.exit_reason = exit_reason::CANCELLED.into();
    result.outcome = Some(outcome::ERROR.into());
    result.failure_category = None;
    result.steps.get_or_insert(0);
    result.cost_usd.get_or_insert(0.0);
    result.prompt_tokens.get_or_insert(0);
    result.cache_read_tokens.get_or_insert(0);
    result.cache_creation_tokens.get_or_insert(0);
    result.completion_tokens.get_or_insert(0);
    result.duration_secs.get_or_insert(0.0);
    result.error = None;
    result.attempts = ctx.attempts.max(1);
    result.retry_reasons = ctx.retry_reasons.to_vec();
    result.resolved_count = 0;
    result.pass_at_1 = false;
    result
}

fn persist_cancelled_wait_trajectory(
    output_dir: &Path,
    instance_id: &str,
    run_index: u32,
    task: &str,
    model_name: &str,
) {
    let traj_path = trajectory_path_for_run(output_dir, instance_id, run_index);
    let mut trajectory = std::fs::read_to_string(&traj_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Trajectory>(&text).ok())
        .unwrap_or_default();
    let now = chrono::Utc::now().to_rfc3339();
    trajectory.info.task.get_or_insert_with(|| task.to_owned());
    trajectory
        .info
        .model_name
        .get_or_insert_with(|| model_name.to_owned());
    trajectory
        .info
        .started_at
        .get_or_insert_with(|| now.clone());
    trajectory.info.ended_at = Some(now);
    trajectory.info.exit_reason = Some(exit_reason::CANCELLED.into());
    trajectory.info.outcome = Some(outcome::ERROR.into());
    trajectory.info.failure_category = None;
    trajectory.info.total_cost_usd.get_or_insert(0.0);
    trajectory
        .info
        .token_usage
        .get_or_insert_with(TokenUsage::default);
    trajectory.info.duration_secs.get_or_insert(0.0);
    trajectory.info.steps.get_or_insert(0);

    if let Some(parent) = traj_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                path = %traj_path.display(),
                error = %err,
                "failed to create cancelled trajectory directory"
            );
            return;
        }
    }
    if let Err(err) = trajectory.save_pretty(&traj_path) {
        tracing::warn!(
            path = %traj_path.display(),
            error = %err,
            "failed to persist cancelled trajectory"
        );
    }
}

#[derive(Debug, Clone)]
struct SweepRun {
    inst: SweBenchInstance,
    run_index: u32,
}

#[derive(Debug, Clone)]
struct CancellationSnapshot {
    cancelled_at: String,
    deadline_at: String,
    deadline: tokio::time::Instant,
    completed: usize,
    in_flight_at_cancel: usize,
    not_started: usize,
    exit_code: i32,
}

#[derive(Clone, Copy)]
struct CancellationSignalContext<'a> {
    initial: &'a SweepResults,
    summary_path: &'a Path,
    results: &'a [RunSlotResult],
    reruns: u32,
    cancel_deadline_secs: u64,
    in_flight: usize,
    pending_len: usize,
    submitted: usize,
    skipped: usize,
    errored: usize,
    budget_halted: usize,
}

fn take_pending_cancellation_signal(
    signal_rx: &mut Option<mpsc::UnboundedReceiver<SweepSignal>>,
    signal_rx_closed: &mut bool,
) -> Option<SweepSignal> {
    if *signal_rx_closed {
        return None;
    }
    let Some(rx) = signal_rx.as_mut() else {
        *signal_rx_closed = true;
        return None;
    };
    match rx.try_recv() {
        Ok(signal) => Some(signal),
        Err(mpsc::error::TryRecvError::Empty) => None,
        Err(mpsc::error::TryRecvError::Disconnected) => {
            *signal_rx_closed = true;
            None
        }
    }
}

fn begin_sweep_cancellation(
    ctx: CancellationSignalContext<'_>,
) -> Result<CancellationSnapshot, Error> {
    let cancel_deadline = Duration::from_secs(ctx.cancel_deadline_secs);
    let cancelled_at = chrono::Utc::now();
    let deadline_at =
        cancelled_at + chrono::Duration::from_std(cancel_deadline).unwrap_or_default();
    let snapshot = CancellationSnapshot {
        cancelled_at: cancelled_at.to_rfc3339(),
        deadline_at: deadline_at.to_rfc3339(),
        deadline: tokio::time::Instant::now() + cancel_deadline,
        completed: ctx.results.len(),
        in_flight_at_cancel: ctx.in_flight,
        not_started: ctx.pending_len,
        exit_code: CANCEL_EXIT_CODE_GRACEFUL,
    };
    tracing::warn!(
        in_flight = snapshot.in_flight_at_cancel,
        not_started = snapshot.not_started,
        "cancellation requested — stopping new task launches"
    );
    let mut partial = ctx.initial.clone();
    partial.sweep_status = SWEEP_STATUS_CANCELLING.into();
    partial.cancelled_at = Some(snapshot.cancelled_at.clone());
    partial.cancel_deadline_at = Some(snapshot.deadline_at.clone());
    partial.cancel_exit_code = Some(snapshot.exit_code);
    partial.completed = snapshot.completed;
    partial.in_flight_at_cancel = snapshot.in_flight_at_cancel;
    partial.not_started = snapshot.not_started;
    partial.submitted = ctx.submitted;
    partial.skipped = ctx.skipped;
    partial.errored = ctx.errored;
    partial.budget_halted = ctx.budget_halted;
    partial.instances = aggregate_run_results(ctx.results, ctx.reruns);
    write_sweep_results_atomic(ctx.summary_path, &partial)?;
    Ok(snapshot)
}

fn apply_sweep_signal(
    signal: SweepSignal,
    ctx: CancellationSignalContext<'_>,
    cancellation: &mut Option<CancellationSnapshot>,
    force_cancel_sent: &mut bool,
    force_cancel_tx: &watch::Sender<bool>,
) -> Result<(), Error> {
    if cancellation.is_none() {
        let cancel_deadline_secs = ctx.cancel_deadline_secs;
        *cancellation = Some(begin_sweep_cancellation(ctx)?);
        if cancel_deadline_secs == 0 {
            *force_cancel_sent = true;
            let _ = force_cancel_tx.send(true);
        }
    } else if matches!(signal, SweepSignal::Interrupt | SweepSignal::Terminate)
        && !*force_cancel_sent
    {
        if let Some(cancel) = cancellation.as_mut() {
            cancel.exit_code = CANCEL_EXIT_CODE_ESCALATED;
        }
        *force_cancel_sent = true;
        let _ = force_cancel_tx.send(true);
        tracing::warn!("second interrupt received — forcing in-flight cancellation");
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct RunSlotResult {
    run_index: u32,
    result: InstanceResult,
}

impl RunSlotResult {
    fn new(run_index: u32, result: InstanceResult) -> Self {
        Self { run_index, result }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct SweepAccounting {
    with_patch: usize,
    tokens: TokenBreakdown,
    total_retries: u64,
    retried_instances: usize,
}

impl SweepAccounting {
    fn add_result(&mut self, result: &InstanceResult) {
        if result.non_empty_patch && result.outcome.as_deref() == Some(outcome::SUBMITTED) {
            self.with_patch += 1;
        }
        self.total_retries = self
            .total_retries
            .saturating_add(u64::from(result.attempts.saturating_sub(1)));
        if result.attempts > 1 {
            self.retried_instances += 1;
        }
        if let Some(prompt_tokens) = result.prompt_tokens {
            self.tokens.input_tokens = self.tokens.input_tokens.saturating_add(prompt_tokens);
        }
        if let Some(cache_read_tokens) = result.cache_read_tokens {
            self.tokens.cache_read_tokens = self
                .tokens
                .cache_read_tokens
                .saturating_add(cache_read_tokens);
        }
        if let Some(cache_creation_tokens) = result.cache_creation_tokens {
            self.tokens.cache_creation_tokens = self
                .tokens
                .cache_creation_tokens
                .saturating_add(cache_creation_tokens);
        }
        if let Some(completion_tokens) = result.completion_tokens {
            self.tokens.completion_tokens = self
                .tokens
                .completion_tokens
                .saturating_add(completion_tokens);
        }
    }
}

fn submitted_with_tests_for_fresh_submissions(results: &[RunSlotResult]) -> usize {
    results
        .iter()
        .filter(|r| {
            r.result.exit_reason != "skipped_resume"
                && r.result.outcome.as_deref() == Some(outcome::SUBMITTED)
                && r.result.tests_run_before_submit
        })
        .count()
}

fn github_pr_failure_count_for_run_slots(results: &[RunSlotResult]) -> usize {
    results
        .iter()
        .filter(|r| r.result.github_pr_error.is_some())
        .count()
}

fn aggregate_run_results(results: &[RunSlotResult], requested_runs: u32) -> Vec<InstanceResult> {
    let mut grouped: BTreeMap<&str, Vec<&RunSlotResult>> = BTreeMap::new();
    for r in results {
        grouped
            .entry(r.result.instance_id.as_str())
            .or_default()
            .push(r);
    }

    let mut out = Vec::with_capacity(grouped.len());
    for (instance_id, mut rows) in grouped {
        rows.sort_by_key(|r| r.run_index);
        let Some(first) = rows.first().map(|r| &r.result) else {
            continue;
        };
        let mut aggregate = first.clone();
        instance_id.clone_into(&mut aggregate.instance_id);
        aggregate.exit_reason.clone_from(&first.exit_reason);
        aggregate.outcome.clone_from(&first.outcome);
        aggregate.failure_category = first.failure_category;
        aggregate.steps = first.steps;
        aggregate.cost_usd = sum_f64(rows.iter().filter_map(|r| r.result.cost_usd));
        aggregate.prompt_tokens = Some(
            rows.iter()
                .filter_map(|r| r.result.prompt_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.cache_read_tokens = Some(
            rows.iter()
                .filter_map(|r| r.result.cache_read_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.cache_creation_tokens = Some(
            rows.iter()
                .filter_map(|r| r.result.cache_creation_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.completion_tokens = Some(
            rows.iter()
                .filter_map(|r| r.result.completion_tokens)
                .fold(0u64, u64::saturating_add),
        );
        aggregate.duration_secs = sum_f64(rows.iter().filter_map(|r| r.result.duration_secs));
        aggregate.error.clone_from(&first.error);
        aggregate.github_pr_error = rows.iter().find_map(|r| r.result.github_pr_error.clone());
        aggregate.patch_present = rows.iter().any(|r| r.result.patch_present);
        aggregate.non_empty_patch = rows.iter().any(|r| r.result.non_empty_patch);
        aggregate.attempts = rows
            .iter()
            .map(|r| r.result.attempts)
            .fold(0u32, u32::saturating_add);
        aggregate.retry_reasons = rows
            .iter()
            .flat_map(|r| r.result.retry_reasons.iter().copied())
            .collect();
        aggregate.runs = requested_runs;
        aggregate.resolved_count = rows
            .iter()
            .filter(|r| is_resolved_instance_result(&r.result))
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        aggregate.pass_at_1 = rows
            .iter()
            .find(|r| r.run_index == 1)
            .is_some_and(|r| is_resolved_instance_result(&r.result));
        aggregate.tests_run_before_submit = rows.iter().any(|r| r.result.tests_run_before_submit);
        aggregate.last_tests_passed = rows.iter().rev().find_map(|r| r.result.last_tests_passed);
        out.push(aggregate);
    }
    out
}

fn sum_f64(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut seen = false;
    let mut total = 0.0;
    for value in values {
        seen = true;
        total += value;
    }
    seen.then_some(total)
}

fn pass_at_k(instances: &[InstanceResult]) -> f64 {
    if instances.is_empty() {
        return 0.0;
    }
    let passed = instances
        .iter()
        .filter(|row| resolved_count(row) > 0)
        .count();
    #[allow(clippy::cast_precision_loss)]
    {
        passed as f64 / instances.len() as f64
    }
}

#[must_use]
pub fn effective_runs(row: &InstanceResult) -> u32 {
    if row.runs == 0 { 1 } else { row.runs }
}

#[must_use]
pub fn resolved_count(row: &InstanceResult) -> u32 {
    if row.runs == 0 {
        u32::from(is_resolved_instance_result(row))
    } else {
        row.resolved_count.min(row.runs)
    }
}

#[must_use]
pub fn pass_at_1(row: &InstanceResult) -> bool {
    if row.runs == 0 {
        is_resolved_instance_result(row)
    } else {
        row.pass_at_1
    }
}

#[must_use]
pub fn is_resolved_instance_result(row: &InstanceResult) -> bool {
    row.outcome.as_deref() == Some(outcome::SUBMITTED) && row.failure_category.is_none()
}

/// Write aggregate and per-run prediction files. `all_preds.jsonl` is a
/// complete artifact log with unique prediction IDs across reruns; each
/// `all_preds.run-k.jsonl` keeps original SWE-bench IDs and is safe to hand
/// to sb-cli, which rejects duplicate `instance_id` rows.
fn write_predictions_file(
    output_dir: &std::path::Path,
    results: &mut [RunSlotResult],
    model_name: &str,
    redaction_cfg: &crate::config::RedactionCfg,
) -> Result<(), Error> {
    let redactor = Redactor::from_config_lossy(redaction_cfg);
    let max_run = results.iter().map(|r| r.run_index).max().unwrap_or(1);
    let use_unique_prediction_ids = max_run > 1;
    let mut aggregate = String::new();
    let mut per_run: BTreeMap<u32, String> = BTreeMap::new();
    for r in results {
        if r.result.outcome.as_deref() != Some(outcome::SUBMITTED) {
            continue;
        }
        if !r.result.patch_present {
            // A submitted-but-patch-missing instance only happens on a
            // resume that found the trajectory but no `.patch`; we already
            // re-queued it above so this branch is defensive.
            continue;
        }
        let patch_path =
            existing_patch_path_for_run(output_dir, &r.result.instance_id, r.run_index);
        let raw_model_patch = std::fs::read_to_string(&patch_path).unwrap_or_default();
        let redacted_patch = redactor.redact_text(&raw_model_patch, surface::PATCH_SUBMISSION);
        if (redactor.configured_literal_leak(&raw_model_patch).is_some() || redacted_patch.redacted)
            && !redactor.unsafe_allow_secret_leaks()
        {
            std::fs::write(&patch_path, &redacted_patch.text)?;
            downgrade_prediction_secret_leak(&mut r.result);
            continue;
        }
        let model_patch = if redactor.unsafe_allow_secret_leaks() {
            raw_model_patch
        } else {
            redacted_patch.text
        };
        let run_line = serde_json::json!({
            "instance_id": r.result.instance_id,
            "model_patch": model_patch,
            "model_name_or_path": model_name,
            "run_index": r.run_index,
        });
        let _ = writeln!(
            per_run.entry(r.run_index).or_default(),
            "{}",
            serde_json::to_string(&run_line)?
        );

        let aggregate_instance_id = if use_unique_prediction_ids {
            format!("{}::run-{}", r.result.instance_id, r.run_index)
        } else {
            r.result.instance_id.clone()
        };
        let aggregate_line = if use_unique_prediction_ids {
            serde_json::json!({
                "instance_id": aggregate_instance_id,
                "original_instance_id": r.result.instance_id,
                "run_index": r.run_index,
                "model_patch": model_patch,
                "model_name_or_path": model_name,
            })
        } else {
            serde_json::json!({
                "instance_id": aggregate_instance_id,
                "model_patch": model_patch,
                "model_name_or_path": model_name,
            })
        };
        aggregate.push_str(&serde_json::to_string(&aggregate_line)?);
        aggregate.push('\n');
    }
    std::fs::write(predictions_path(output_dir), aggregate)?;
    for (run_index, text) in per_run {
        std::fs::write(predictions_path_for_run(output_dir, run_index), text)?;
    }
    Ok(())
}

fn downgrade_prediction_secret_leak(result: &mut InstanceResult) {
    result.exit_reason = "error".into();
    result.outcome = Some(outcome::ERROR.into());
    result.failure_category = Some(FailureCategory::SecretLeakDetected);
    result.error = Some("secret_leak_detected in prediction artifact".into());
    result.resolved_count = 0;
    result.pass_at_1 = false;
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
    let (prompt_tokens, cache_read_tokens, cache_creation_tokens, completion_tokens) = info
        .token_usage
        .as_ref()
        .map_or((None, None, None, None), |t| {
            (
                Some(t.prompt_tokens),
                Some(t.cache_read_tokens),
                Some(t.cache_creation_tokens),
                Some(t.completion_tokens),
            )
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
        cache_read_tokens,
        cache_creation_tokens,
        completion_tokens,
        duration_secs: info.duration_secs,
        error: None,
        github_pr_error: None,
        patch_present,
        non_empty_patch,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: u32::from(
            info.outcome.as_deref() == Some(outcome::SUBMITTED) && info.failure_category.is_none(),
        ),
        pass_at_1: info.outcome.as_deref() == Some(outcome::SUBMITTED)
            && info.failure_category.is_none(),
        tests_run_before_submit: info.tests_run_before_submit,
        last_tests_passed: info.last_tests_passed,
    }
}

fn skipped_result_from_prior_result(
    r: &InstanceResult,
    traj_based: &InstanceResult,
) -> InstanceResult {
    let mut out = r.clone();
    out.exit_reason = "skipped_resume".into();
    out.error = None;
    out.github_pr_error = None;
    out.patch_present = traj_based.patch_present;
    out.non_empty_patch = traj_based.non_empty_patch;
    out.tests_run_before_submit = traj_based.tests_run_before_submit;
    out.last_tests_passed = traj_based.last_tests_passed;
    out.runs = 1;
    out.resolved_count = u32::from(is_resolved_instance_result(&out));
    out.pass_at_1 = is_resolved_instance_result(&out);
    out
}

#[derive(Default)]
struct PriorResults {
    by_id: HashMap<String, InstanceResult>,
    results_mtime: Option<SystemTime>,
}

fn load_prior_results_by_instance(output_dir: &std::path::Path) -> PriorResults {
    let path = output_dir.join("results.json");
    if !path.exists() {
        return PriorResults::default();
    }
    let results_mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(path=%path.display(), error=%err, "resume: failed reading prior results.json; ignoring");
            return PriorResults::default();
        }
    };
    let parsed: SweepResults = match serde_json::from_str(&text) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(path=%path.display(), error=%err, "resume: malformed prior results.json; ignoring");
            return PriorResults::default();
        }
    };
    PriorResults {
        by_id: parsed
            .instances
            .into_iter()
            .map(|r| (r.instance_id.clone(), r))
            .collect(),
        results_mtime,
    }
}

fn resume_snapshot_for_run(
    output_dir: &std::path::Path,
    instance_id: &str,
    run_index: u32,
    info: &crate::trajectory::TrajectoryInfo,
    patch_path: &std::path::Path,
    prior: &PriorResults,
) -> InstanceResult {
    let traj_based = skipped_result_from_info(instance_id, info, patch_path);
    let Some(prior_result) = prior.by_id.get(instance_id) else {
        return traj_based;
    };
    if !prior_result_matches_trajectory(
        output_dir,
        instance_id,
        run_index,
        prior_result,
        &traj_based,
        prior,
    ) {
        return traj_based;
    }
    skipped_result_from_prior_result(prior_result, &traj_based)
}

fn prior_result_matches_trajectory(
    output_dir: &std::path::Path,
    instance_id: &str,
    run_index: u32,
    prior_result: &InstanceResult,
    traj_result: &InstanceResult,
    prior: &PriorResults,
) -> bool {
    if effective_runs(prior_result) != 1 {
        return false;
    }
    if prior_result.outcome != traj_result.outcome
        || prior_result.failure_category != traj_result.failure_category
        || prior_result.steps != traj_result.steps
    {
        return false;
    }
    let Some(results_mtime) = prior.results_mtime else {
        return false;
    };
    let traj_mtime = std::fs::metadata(existing_trajectory_path_for_run(
        output_dir,
        instance_id,
        run_index,
    ))
    .ok()
    .and_then(|m| m.modified().ok());
    let Some(traj_mtime) = traj_mtime else {
        return false;
    };
    results_mtime >= traj_mtime
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
        self.max_retries > 0 && self.is_retry_category(cat)
    }

    fn is_retry_category(&self, cat: FailureCategory) -> bool {
        self.retry_on.contains(&cat)
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
        let bounded_ms = if cap_ms == 0 { 0 } else { exp_ms.min(cap_ms) };
        let jitter_seed = simple_hash(instance_id) ^ u64::from(attempt);
        let jitter_pct = jitter_seed % 251; // 0..250 => up to +25.0%
        let jittered = bounded_ms.saturating_mul(1000 + jitter_pct) / 1000;
        std::time::Duration::from_millis(if cap_ms == 0 { 0 } else { jittered.min(cap_ms) })
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
        "budget_exhausted" => Ok(FailureCategory::BudgetExhausted),
        "wallclock_timeout" => Ok(FailureCategory::WallclockTimeout),
        "agent_internal" => Ok(FailureCategory::AgentInternal),
        "patch_apply_invalid" => Ok(FailureCategory::PatchApplyInvalid),
        "patch_empty" => Ok(FailureCategory::PatchEmpty),
        "secret_leak_detected" => Ok(FailureCategory::SecretLeakDetected),
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

#[derive(Clone)]
struct RunOneParams {
    output_dir: PathBuf,
    cfg: Config,
    deterministic_responses: Option<Vec<String>>,
    deterministic_usage_per_call: Option<ModelUsage>,
    retry_policy: RetryPolicy,
    task_timeout_secs: Option<u64>,
    skip_patch_validation: bool,
    governor: Option<std::sync::Arc<crate::run::rate_limit::RateLimitGovernor>>,
    cancellation: crate::run::mini::MiniCancellation,
    github_pr: Option<crate::run::github_pr::GithubPrSweepConfig>,
}

#[allow(clippy::too_many_lines)]
async fn run_one(inst: SweBenchInstance, run_index: u32, params: RunOneParams) -> InstanceResult {
    let RunOneParams {
        output_dir,
        mut cfg,
        deterministic_responses,
        deterministic_usage_per_call,
        retry_policy,
        task_timeout_secs,
        skip_patch_validation,
        governor,
        cancellation,
        github_pr,
    } = params;
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
    let patch_path = patch_path_for_run(&output_dir, &id, run_index);
    let run_output_dir = output_dir.join(&id);
    let trajectory_name = format!("run-{run_index}");
    let base_commit = inst.base_commit.clone();
    let mut attempts = 0u32;
    let mut retry_reasons = Vec::new();
    let mut total_prompt_tokens = 0u64;
    let mut total_cache_read_tokens = 0u64;
    let mut total_cache_creation_tokens = 0u64;
    let mut total_completion_tokens = 0u64;
    let model_name = cfg.root.model.name.clone();
    let mut total_recorded_cost_usd = 0.0f64;
    let mut terminal: Option<InstanceResult> = None;

    while attempts <= retry_policy.max_retries {
        attempts += 1;

        // Rate-limit gate: block (not spin) until the governor permits the
        // next attempt. Does NOT count against task_timeout_secs because
        // the timeout is applied inside mini::run, not here.
        if let Some(g) = &governor {
            if !g.acquire_until_cancelled(0, cancellation.clone()).await {
                return cancelled_wait_result(CancelledWaitContext {
                    output_dir: &output_dir,
                    instance_id: &id,
                    run_index,
                    task: &task,
                    model_name: &model_name,
                    attempts,
                    retry_reasons: &retry_reasons,
                    current: None,
                });
            }
        }

        let det_for_attempt = deterministic_responses
            .as_ref()
            .map(|v| deterministic_for_attempt(v, attempts, retry_policy.max_retries > 0));
        let traj_path = trajectory_path_for_run(&output_dir, &id, run_index);
        let before_fp = trajectory_fingerprint(&traj_path);
        let args = crate::run::mini::MiniArgs {
            task: task.clone(),
            extra_context: None,
            config: cfg.clone(),
            output_dir: run_output_dir.clone(),
            trajectory_name: trajectory_name.clone(),
            deterministic_responses: det_for_attempt,
            deterministic_usage_per_call: deterministic_usage_per_call.clone(),
            task_timeout_secs,
            cancellation: Some(cancellation.clone()),
            stream_addr: None,
            patch_capture: Some(crate::run::mini::PatchCaptureSpec {
                base_commit: base_commit.clone(),
                workdir: workdir.clone(),
                patch_path: patch_path.clone(),
                skip_patch_validation,
            }),
        };
        let run_err = crate::run::mini::run(args).await.err();

        // If the attempt hit a rate-limit error, report it to the governor
        // so the global Retry-After floor is set for all workers.
        if let (
            Some(g),
            Some(crate::error::Error::Model(crate::error::ModelError::RateLimited(msg))),
        ) = (&governor, &run_err)
        {
            let retry_after =
                crate::run::rate_limit::RateLimitGovernor::parse_retry_after_from_error(msg);
            g.report_429(retry_after).await;
        }
        let info = load_fresh_trajectory_info(&traj_path, before_fp);
        let outcome_str = info
            .as_ref()
            .and_then(|i| i.outcome.clone())
            .or_else(|| run_err.as_ref().map(|_| outcome::ERROR.to_owned()))
            .unwrap_or_else(|| outcome::ERROR.to_owned());
        let exit_reason = info
            .as_ref()
            .and_then(|i| i.exit_reason.clone())
            .unwrap_or_else(|| outcome_str.clone());
        let (prompt_tokens, cache_read_tokens, cache_creation_tokens, completion_tokens) = info
            .as_ref()
            .and_then(|i| i.token_usage.as_ref())
            .map_or((0, 0, 0, 0), |t| {
                (
                    t.prompt_tokens,
                    t.cache_read_tokens,
                    t.cache_creation_tokens,
                    t.completion_tokens,
                )
            });
        total_prompt_tokens = total_prompt_tokens.saturating_add(prompt_tokens);
        total_cache_read_tokens = total_cache_read_tokens.saturating_add(cache_read_tokens);
        total_cache_creation_tokens =
            total_cache_creation_tokens.saturating_add(cache_creation_tokens);
        total_completion_tokens = total_completion_tokens.saturating_add(completion_tokens);
        let attempt_recorded_cost = info.as_ref().and_then(|i| i.total_cost_usd);
        let has_token_usage = prompt_tokens > 0
            || cache_read_tokens > 0
            || cache_creation_tokens > 0
            || completion_tokens > 0;
        let attempt_effective_cost = match attempt_recorded_cost {
            Some(cost) if cost != 0.0 || !has_token_usage => cost,
            Some(_) | None => estimate_cost_usd(
                prompt_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                completion_tokens,
                &model_name,
            ),
        };
        total_recorded_cost_usd += attempt_effective_cost;

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
                    "budget_exhausted" => Some(FailureCategory::BudgetExhausted),
                    exit_reason::CANCELLED => None,
                    exit_reason::WALLCLOCK_TIMEOUT => Some(FailureCategory::WallclockTimeout),
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
            cost_usd: Some(total_recorded_cost_usd),
            prompt_tokens: Some(total_prompt_tokens),
            cache_read_tokens: Some(total_cache_read_tokens),
            cache_creation_tokens: Some(total_cache_creation_tokens),
            completion_tokens: Some(total_completion_tokens),
            duration_secs: info.as_ref().and_then(|i| i.duration_secs),
            error: run_err.map(|e| e.to_string()),
            github_pr_error: None,
            patch_present,
            non_empty_patch,
            attempts,
            retry_reasons: retry_reasons.clone(),
            runs: 1,
            resolved_count: u32::from(
                outcome_str == outcome::SUBMITTED && failure_category.is_none(),
            ),
            pass_at_1: outcome_str == outcome::SUBMITTED && failure_category.is_none(),
            tests_run_before_submit: info.as_ref().is_some_and(|i| i.tests_run_before_submit),
            last_tests_passed: info.as_ref().and_then(|i| i.last_tests_passed),
        };
        let current = publish_github_pr_for_result(
            current,
            GithubPrPublication {
                config: github_pr.as_ref(),
                instance_id: &id,
                run_index,
                trajectory_path: &traj_path,
                patch_path: &patch_path,
            },
        )
        .await;

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
        if !sleep_or_cancelled(
            retry_policy.backoff_for(&id, attempts),
            cancellation.clone(),
        )
        .await
        {
            return cancelled_wait_result(CancelledWaitContext {
                output_dir: &output_dir,
                instance_id: &id,
                run_index,
                task: &task,
                model_name: &model_name,
                attempts,
                retry_reasons: &retry_reasons,
                current: Some(current),
            });
        }
        terminal = Some(current);
    }

    terminal.unwrap_or_else(|| budget_halt_result(&id))
}

async fn sleep_or_cancelled(
    duration: Duration,
    mut cancellation: crate::run::mini::MiniCancellation,
) -> bool {
    if cancellation.is_cancelled() {
        return false;
    }
    if duration.is_zero() {
        return true;
    }
    tokio::select! {
        () = tokio::time::sleep(duration) => !cancellation.is_cancelled(),
        () = cancellation.cancelled() => false,
    }
}

struct GithubPrPublication<'a> {
    config: Option<&'a crate::run::github_pr::GithubPrSweepConfig>,
    instance_id: &'a str,
    run_index: u32,
    trajectory_path: &'a Path,
    patch_path: &'a Path,
}

async fn publish_github_pr_for_result(
    mut current: InstanceResult,
    publication: GithubPrPublication<'_>,
) -> InstanceResult {
    if current.outcome.as_deref() != Some(outcome::SUBMITTED) || !current.patch_present {
        return current;
    }
    let Some(config) = publication.config else {
        return current;
    };

    let pr_options = config.options_for_run(
        publication.instance_id,
        publication.run_index,
        publication.trajectory_path,
        publication.patch_path,
    );
    match crate::run::github_pr::publish(pr_options).await {
        Ok(result) => {
            if let Some(output) = result.dry_run_output {
                print!("{output}");
            } else if let Some(url) = result.url {
                println!(
                    "github_pr_url[{}#run-{}]: {url}",
                    publication.instance_id, publication.run_index
                );
            }
        }
        Err(err) => {
            current.github_pr_error = Some(err.to_string());
        }
    }
    current
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
            Some(
                FailureCategory::StepLimit
                    | FailureCategory::CostLimit
                    | FailureCategory::BudgetExhausted
                    | FailureCategory::WallclockTimeout
                    | FailureCategory::SecretLeakDetected
            )
        )
}

fn failure_category_label(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::BudgetExhausted => "budget_exhausted",
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::Unknown => "unknown",
    }
}

fn read_trajectory_info(path: &std::path::Path) -> Option<crate::trajectory::TrajectoryInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let traj: Trajectory = serde_json::from_str(&text).ok()?;
    Some(traj.info)
}

fn trajectory_fingerprint(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some((mtime, meta.len()))
}

fn load_fresh_trajectory_info(
    path: &std::path::Path,
    before: Option<(SystemTime, u64)>,
) -> Option<crate::trajectory::TrajectoryInfo> {
    let after = trajectory_fingerprint(path);
    (after != before).then_some(())?;
    read_trajectory_info(path)
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ApplySubsetParams<'a> {
    pub instance_ids_arg: Option<&'a str>,
    pub limit: Option<usize>,
    pub sample: Option<usize>,
    pub seed: Option<u64>,
    pub stratify_by: Option<StratifyBy>,
    pub stratify_mode: StratifyMode,
}

fn validate_subset_params(
    params: &ApplySubsetParams<'_>,
    requested_ids: Option<&Vec<String>>,
) -> Result<(), Error> {
    if params.seed.is_some() && params.sample.is_none() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "`--seed` requires `--sample`".into(),
        )));
    }
    if params.stratify_by.is_some() && params.sample.is_none() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "`--stratify-by` requires `--sample`".into(),
        )));
    }
    if params.stratify_by.is_none() && params.stratify_mode != StratifyMode::Proportional {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "`--stratify-mode` requires `--stratify-by`".into(),
        )));
    }
    if params.stratify_by.is_some() && requested_ids.is_some() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "`--stratify-by` cannot be combined with `--instance-ids`".into(),
        )));
    }
    Ok(())
}

pub(crate) fn apply_subset(
    mut instances: Vec<SweBenchInstance>,
    params: &ApplySubsetParams<'_>,
) -> Result<(Vec<SweBenchInstance>, FilterSpec), Error> {
    let original_count = instances.len();
    let requested_ids = parse_instance_ids_arg(params.instance_ids_arg)?;
    validate_subset_params(params, requested_ids.as_ref())?;

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

    if let Some(n) = params.sample {
        let seed_value = params.seed.ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(
                "`--sample` requires `--seed`".into(),
            ))
        })?;
        if n < instances.len() {
            if params.stratify_by == Some(StratifyBy::Repo) {
                instances =
                    stratified_sample_by_repo(instances, n, seed_value, params.stratify_mode);
            } else {
                let mut rng = XorShift64::new(seed_value);
                for i in (1..instances.len()).rev() {
                    let j = rng.next_usize() % (i + 1);
                    instances.swap(i, j);
                }
                instances.truncate(n);
            }
        }
    }

    if let Some(n) = params.limit {
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
        limit: params.limit,
        sample: params.sample,
        seed: params.seed,
        stratify_by: params.stratify_by,
        stratify_mode: params.stratify_by.map(|_| params.stratify_mode),
    };
    Ok((instances, spec))
}

fn stratified_sample_by_repo(
    instances: Vec<SweBenchInstance>,
    n: usize,
    seed: u64,
    mode: StratifyMode,
) -> Vec<SweBenchInstance> {
    let mut map: BTreeMap<String, Vec<SweBenchInstance>> = BTreeMap::new();
    for inst in instances {
        let key = inst.repo.clone().unwrap_or_else(|| "<unknown>".to_owned());
        map.entry(key).or_default().push(inst);
    }
    let groups: Vec<(String, Vec<SweBenchInstance>)> = map.into_iter().collect();
    let total = groups.iter().map(|(_, v)| v.len()).sum::<usize>();
    let mut targets = vec![0usize; groups.len()];
    match mode {
        StratifyMode::Balanced => {
            if !groups.is_empty() {
                let groups_len_u64 = u64::try_from(groups.len()).unwrap_or(u64::MAX);
                let start_u64 = seed % groups_len_u64;
                let start = usize::try_from(start_u64).unwrap_or(0);
                let mut cursor = 0usize;
                for _ in 0..n {
                    for _ in 0..groups.len() {
                        let i = (start + cursor) % groups.len();
                        cursor = cursor.wrapping_add(1);
                        if targets[i] < groups[i].1.len() {
                            targets[i] += 1;
                            break;
                        }
                    }
                }
            }
        }
        StratifyMode::Proportional => {
            let mut rem: Vec<(usize, usize, usize)> = Vec::new();
            for (i, (_, g)) in groups.iter().enumerate() {
                let numer = n * g.len();
                targets[i] = numer / total;
                rem.push((numer % total, g.len(), i));
            }
            let mut left = n.saturating_sub(targets.iter().sum::<usize>());
            // Deterministic tie-breaker: larger remainder, then more capacity,
            // then seeded pseudo-random order by group index.
            rem.sort_by(|a, b| {
                b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)).then_with(|| {
                    let ah = simple_hash(&format!("{seed}:{}", a.2));
                    let bh = simple_hash(&format!("{seed}:{}", b.2));
                    ah.cmp(&bh)
                })
            });
            for (_, cap, i) in rem {
                if left == 0 {
                    break;
                }
                if targets[i] < cap {
                    targets[i] += 1;
                    left -= 1;
                }
            }
        }
    }

    let mut out = Vec::with_capacity(n);
    for (i, (_, mut g)) in groups.into_iter().enumerate() {
        let mut rng = XorShift64::new(seed ^ simple_hash(&format!("{i}")));
        for j in (1..g.len()).rev() {
            let k = rng.next_usize() % (j + 1);
            g.swap(j, k);
        }
        out.extend(g.into_iter().take(targets[i]));
    }
    // Preserve stratified composition while preventing repo-clustered output
    // order, since a later `--limit` truncation should not always bias toward
    // lexicographically early repos.
    let mut rng = XorShift64::new(seed ^ simple_hash("stratified-final-shuffle"));
    for i in (1..out.len()).rev() {
        let j = rng.next_usize() % (i + 1);
        out.swap(i, j);
    }
    out
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
    use futures::FutureExt;

    fn test_instance_result(id: &str, submitted: bool, tests_run: bool) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: if submitted { "submitted" } else { "error" }.into(),
            outcome: Some(
                if submitted {
                    outcome::SUBMITTED
                } else {
                    outcome::ERROR
                }
                .into(),
            ),
            failure_category: if submitted {
                None
            } else {
                Some(FailureCategory::Unknown)
            },
            steps: Some(1),
            cost_usd: Some(0.0),
            prompt_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(0),
            duration_secs: Some(0.0),
            error: None,
            github_pr_error: None,
            patch_present: submitted,
            non_empty_patch: submitted,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 1,
            resolved_count: u32::from(submitted),
            pass_at_1: submitted,
            tests_run_before_submit: tests_run,
            last_tests_passed: tests_run.then_some(true),
        }
    }

    fn init_test_repo(dir: &Path) {
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@test"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "commit.gpgSign", "false"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
    }

    fn test_config_with_workdir(dir: &Path) -> Config {
        let workdir = dir
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        Config::from_toml_str(&format!("[environment]\nworkdir = \"{workdir}\"\n")).unwrap()
    }

    fn write_test_dataset(path: &Path, ids: &[&str]) {
        let mut dataset = String::new();
        for id in ids {
            let _ = writeln!(
                dataset,
                "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\"}}"
            );
        }
        std::fs::write(path, dataset).unwrap();
    }

    #[tokio::test]
    async fn queued_cancel_signal_stops_dispatch_after_joined_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_test_repo(&repo);
        let dataset = tmp.path().join("dataset.jsonl");
        write_test_dataset(&dataset, &["first", "should-not-start"]);
        let output = tmp.path().join("out");
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        {
            let mut hook = SIGNAL_BEFORE_NEXT_DISPATCH
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *hook = Some(SignalBeforeDispatchHook {
                output_dir: output.clone(),
                sender: signal_tx,
            });
        }

        let args = SwebenchArgs {
            dataset_path: dataset,
            output_dir: output.clone(),
            parallel: 1,
            reruns: 1,
            config: test_config_with_workdir(&repo),
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: StratifyMode::Proportional,
            max_retries: 0,
            retry_on: None,
            retry_backoff_base_ms: 0,
            retry_backoff_cap_s: 0,
            retry_on_resume: false,
            deterministic_responses: Some(vec![
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfresh-run\n```".into(),
            ]),
            deterministic_usage_per_call: None,
            config_overlay_paths: Vec::new(),
            dry_run: false,
            skip_preflight: true,
            preflight_format: "text".into(),
            skip_model_probe: true,
            preflight_check_timeout_s: 10,
            preflight_total_timeout_s: 60,
            preflight_mode: "test".into(),
            skip_patch_validation: true,
            max_rpm: Some(1),
            max_input_tpm: None,
            cancel_deadline_secs: 0,
            install_os_signal_handlers: false,
            cancellation_signals: Some(signal_rx),
            github_pr: None,
        };

        let results = tokio::time::timeout(Duration::from_secs(8), run(args))
            .await
            .unwrap()
            .unwrap();
        {
            let mut hook = SIGNAL_BEFORE_NEXT_DISPATCH
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *hook = None;
        }

        assert_eq!(results.sweep_status, SWEEP_STATUS_CANCELLED);
        assert_eq!(results.cancel_exit_code, Some(CANCEL_EXIT_CODE_GRACEFUL));
        assert_eq!(results.submitted, 1);
        assert_eq!(results.completed, 1);
        assert_eq!(results.in_flight_at_cancel, 0);
        assert_eq!(results.not_started, 1);
        assert!(trajectory_path_for_run(&output, "first", 1).exists());
        assert!(
            !trajectory_path_for_run(&output, "should-not-start", 1).exists(),
            "queued cancellation must stop the next pending task before it starts"
        );
    }

    #[tokio::test]
    async fn github_pr_publication_is_noop_when_disabled() {
        let result = test_instance_result("inst", true, false);

        let actual = publish_github_pr_for_result(
            result.clone(),
            GithubPrPublication {
                config: None,
                instance_id: "inst",
                run_index: 1,
                trajectory_path: std::path::Path::new("inst/run-1.traj.json"),
                patch_path: std::path::Path::new("inst/run-1.patch"),
            },
        )
        .await;

        assert_eq!(actual.instance_id, result.instance_id);
        assert_eq!(actual.exit_reason, result.exit_reason);
        assert_eq!(actual.outcome, result.outcome);
        assert_eq!(actual.failure_category, result.failure_category);
        assert_eq!(actual.error, result.error);
    }

    #[tokio::test]
    async fn github_pr_publication_failure_preserves_submission_outcome() {
        let result = test_instance_result("inst", true, true);
        let config = crate::run::github_pr::GithubPrSweepConfig {
            target_repo: "not-a-valid-owner-repo".into(),
            target_branch: "trunk".into(),
            token_env: "GITHUB_TOKEN".into(),
            mode: crate::run::github_pr::PublishMode::DryRun,
            timeout_secs: 1,
            max_retries: 0,
            backoff_base_ms: 1,
            branch_prefix: "rust-swe-agent".into(),
        };

        let actual = publish_github_pr_for_result(
            result.clone(),
            GithubPrPublication {
                config: Some(&config),
                instance_id: "inst",
                run_index: 1,
                trajectory_path: std::path::Path::new("inst/run-1.traj.json"),
                patch_path: std::path::Path::new("inst/run-1.patch"),
            },
        )
        .await;

        assert_eq!(actual.outcome, result.outcome);
        assert_eq!(actual.failure_category, result.failure_category);
        assert_eq!(actual.error, None);
        assert!(actual.github_pr_error.is_some());
        assert_eq!(
            github_pr_failure_count_for_run_slots(&[RunSlotResult::new(1, actual)]),
            1
        );
    }

    #[test]
    fn github_pr_failure_count_includes_later_rerun_slots() {
        let first = RunSlotResult::new(1, test_instance_result("rerun-task", true, true));
        let mut later = test_instance_result("rerun-task", true, true);
        later.github_pr_error = Some("tempdir creation failed".into());

        let results = vec![first, RunSlotResult::new(2, later)];
        let aggregate = aggregate_run_results(&results, 2);

        assert_eq!(aggregate.len(), 1);
        assert_eq!(aggregate[0].error, None);
        assert!(aggregate[0].github_pr_error.is_some());
        assert_eq!(github_pr_failure_count_for_run_slots(&results), 1);
    }

    #[test]
    fn cost_estimate_uses_sonnet_pricing() {
        // 1M cold input + 1M completion = $3 + $15 = $18.
        let c = estimate_cost_usd(1_000_000, 0, 0, 1_000_000, "claude-3-5-sonnet");
        assert!((c - 18.0).abs() < 1e-9, "got {c}");
        // Zero in, zero out.
        assert!(estimate_cost_usd(0, 0, 0, 0, "claude-3-5-sonnet").abs() < 1e-9);
    }

    #[test]
    fn cost_estimate_applies_cache_multipliers_for_anthropic() {
        let cold = estimate_cost_usd(1_000_000, 0, 0, 0, "claude-3-5-sonnet");
        let cache_read = estimate_cost_usd(0, 1_000_000, 0, 0, "claude-3-5-sonnet");
        let cache_creation = estimate_cost_usd(0, 0, 1_000_000, 0, "claude-3-5-sonnet");
        assert!((cold - 3.0).abs() < 1e-9, "got {cold}");
        assert!((cache_read - 0.3).abs() < 1e-9, "got {cache_read}");
        assert!((cache_creation - 3.75).abs() < 1e-9, "got {cache_creation}");
        assert!(cache_read < cold);
        assert!(cache_creation > cold);
    }

    #[test]
    fn cost_estimate_applies_cache_multipliers_for_routed_anthropic_models() {
        let cache_read =
            estimate_cost_usd(0, 1_000_000, 0, 0, "openrouter/anthropic/claude-sonnet-4-6");
        let cache_creation =
            estimate_cost_usd(0, 0, 1_000_000, 0, "openrouter/anthropic/claude-sonnet-4-6");
        assert!((cache_read - 0.3).abs() < 1e-9, "got {cache_read}");
        assert!((cache_creation - 3.75).abs() < 1e-9, "got {cache_creation}");
    }

    #[test]
    fn cost_estimate_without_model_name_does_not_apply_cache_multipliers() {
        let cache_read = estimate_cost_usd(0, 1_000_000, 0, 0, "");
        let cache_creation = estimate_cost_usd(0, 0, 1_000_000, 0, "");
        assert!((cache_read - 3.0).abs() < 1e-9, "got {cache_read}");
        assert!((cache_creation - 3.0).abs() < 1e-9, "got {cache_creation}");
    }

    #[test]
    fn legacy_prompt_token_alias_keeps_full_prompt_pricing() {
        let legacy: InstanceResult = serde_json::from_value(serde_json::json!({
            "instance_id": "legacy",
            "exit_reason": "submitted",
            "prompt_tokens": 1_000_000,
            "completion_tokens": 0
        }))
        .unwrap();
        assert_eq!(legacy.prompt_tokens, Some(1_000_000));
        assert_eq!(legacy.cache_read_tokens, None);
        assert_eq!(legacy.cache_creation_tokens, None);
        assert!(
            (legacy
                .effective_cost_usd(Some("claude-3-5-sonnet"))
                .unwrap_or_default()
                - 3.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn zero_stored_cost_falls_back_to_token_pricing() {
        let row = InstanceResult {
            instance_id: "zero-cost".into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: None,
            cost_usd: Some(0.0),
            prompt_tokens: Some(100_000),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(100_000),
            duration_secs: None,
            error: None,
            github_pr_error: None,
            patch_present: false,
            non_empty_patch: false,
            attempts: 1,
            retry_reasons: Vec::new(),
            runs: 1,
            resolved_count: 1,
            pass_at_1: true,
            tests_run_before_submit: false,
            last_tests_passed: None,
        };
        let expected = estimate_cost_usd(100_000, 0, 0, 100_000, "openai/gpt-4o-mini");
        assert!(
            (row.effective_cost_usd(Some("openai/gpt-4o-mini"))
                .unwrap_or_default()
                - expected)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn summary_table_includes_required_fields() {
        let s = SweepResults {
            total: 10,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 4,
            submitted_with_tests: 0,
            skipped: 3,
            errored: 1,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 3,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 250_000,
            total_cache_read_tokens: 2_000_000,
            total_cache_creation_tokens: 250_000,
            total_completion_tokens: 50_000,
            estimated_cost_usd: estimate_cost_usd(
                250_000,
                2_000_000,
                250_000,
                50_000,
                "claude-3-5-sonnet",
            ),
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec {
                original_count: 10,
                selected_count: 10,
                ..FilterSpec::default()
            },
            manifest: None,
            cost_limit_usd: None,
            cache_hit_rate: 0.8,
            instances: vec![],
            rate_limit_events: None,
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
        assert!(t.contains("Input tokens:       250000"));
        assert!(t.contains("Cache read tokens:  2000000"));
        assert!(t.contains("Cache create toks:  250000"));
        assert!(t.contains("Completion tokens:  50000"));
        assert!(t.contains("Cache hit rate:     80.00%"));
        assert!(t.contains("Total tokens:       2550000"));
        assert!(t.contains("Total cost:         $3.0375"));
        // Without a configured limit, the summary should not advertise one.
        assert!(
            !t.contains("Sweep cost limit:"),
            "limit row leaked in unconstrained sweep: {t}"
        );
        assert!(!t.contains("BUDGET HALT"));
    }

    #[test]
    fn summary_table_includes_test_behavior_telemetry() {
        let mut with_tests = test_instance_result("with-tests", true, true);
        with_tests.last_tests_passed = Some(true);
        let skipped_tests = test_instance_result("without-tests", true, false);
        let error_without_submit = test_instance_result("errored", false, false);
        let s = SweepResults {
            total: 3,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 2,
            submitted_with_tests: 1,
            skipped: 0,
            errored: 1,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 2,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            cache_hit_rate: 0.0,
            instances: vec![with_tests, skipped_tests, error_without_submit],
            rate_limit_events: None,
        };

        let t = s.summary_table();
        assert!(t.contains("Submitted w/tests:  1/2"), "{t}");
        assert!(
            t.contains("Resolved by tests:  tests_run=true 1/1, tests_run=false 1/1"),
            "{t}"
        );
    }

    #[test]
    fn test_behavior_resolution_counts_include_later_rerun_submission() {
        let first_error = test_instance_result("rerun-task", false, false);
        let later_submitted_with_tests = test_instance_result("rerun-task", true, true);
        let instances = aggregate_run_results(
            &[
                RunSlotResult::new(1, first_error),
                RunSlotResult::new(2, later_submitted_with_tests),
            ],
            2,
        );
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].resolved_count, 1);
        assert!(instances[0].tests_run_before_submit);

        let counts = test_behavior_resolution_counts(&instances);
        assert_eq!(counts.with_tests, 1);
        assert_eq!(counts.resolved_with_tests, 1);
        assert_eq!(counts.without_tests, 0);
        assert_eq!(counts.resolved_without_tests, 0);
    }

    #[test]
    fn summary_table_includes_per_task_budget_kills_when_present() {
        use crate::trajectory::FailureCategory;
        let mut failures = BTreeMap::new();
        failures.insert(FailureCategory::BudgetExhausted, 3usize);
        let s = SweepResults {
            total: 5,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 2,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 3,
            failures_by_category: failures,
            budget_halted: 0,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            cache_hit_rate: 0.0,
            instances: vec![],
            rate_limit_events: None,
        };
        let t = s.summary_table();
        assert!(
            t.contains("budget_exhausted") || t.contains("Budget-exhausted"),
            "summary should mention budget_exhausted failures; got:\n{t}"
        );
    }

    #[test]
    fn summary_table_spend_stats_by_resolution_shows_mean_and_median() {
        let mut resolved_cheap = test_instance_result("resolved-a", true, false);
        resolved_cheap.cost_usd = Some(0.10);
        let mut resolved_expensive = test_instance_result("resolved-b", true, false);
        resolved_expensive.cost_usd = Some(0.30);
        let mut unresolved_1 = test_instance_result("unresolved-a", false, false);
        unresolved_1.cost_usd = Some(0.20);
        let mut unresolved_2 = test_instance_result("unresolved-b", false, false);
        unresolved_2.cost_usd = Some(0.40);

        let s = SweepResults {
            total: 4,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 2,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 2,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 2,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 1.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.5,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            cache_hit_rate: 0.0,
            instances: vec![
                resolved_cheap,
                resolved_expensive,
                unresolved_1,
                unresolved_2,
            ],
            rate_limit_events: None,
        };

        let t = s.summary_table();
        // Resolved: $0.10 and $0.30 → mean=$0.20 median=$0.20
        assert!(
            t.contains("Spend/resolved"),
            "missing Spend/resolved line: {t}"
        );
        assert!(t.contains("mean=$0.2000"), "wrong resolved mean: {t}");
        // median of [$0.10, $0.30] with even n=2 is ($0.10 + $0.30)/2 = $0.20
        assert!(t.contains("median=$0.2000"), "wrong resolved median: {t}");
        assert!(t.contains("n=2"), "wrong resolved n: {t}");
        // Unresolved: $0.20 and $0.40 → mean=$0.30 median=$0.30
        assert!(
            t.contains("Spend/unresolved"),
            "missing Spend/unresolved line: {t}"
        );
        assert!(t.contains("mean=$0.3000"), "wrong unresolved mean: {t}");
        assert!(t.contains("median=$0.3000"), "wrong unresolved median: {t}");
    }

    #[test]
    fn summary_table_spend_stats_shows_n0_when_no_cost_data() {
        // Instances with no cost data (all None) should produce n=0 lines.
        let mut no_cost = test_instance_result("no-cost", false, false);
        no_cost.cost_usd = None;
        no_cost.prompt_tokens = None;
        no_cost.cache_read_tokens = None;
        no_cost.cache_creation_tokens = None;
        no_cost.completion_tokens = None;

        let s = SweepResults {
            total: 1,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 0,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 1,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            cache_hit_rate: 0.0,
            instances: vec![no_cost],
            rate_limit_events: None,
        };

        let t = s.summary_table();
        assert!(
            t.contains("Spend/resolved  :  n=0"),
            "missing n=0 for resolved: {t}"
        );
        assert!(
            t.contains("Spend/unresolved:  n=0") || t.contains("n=1"),
            "unexpected unresolved output: {t}"
        );
    }

    #[test]
    fn summary_table_includes_budget_halt_line_when_triggered() {
        let s = SweepResults {
            total: 5,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 3,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 2,
            with_patch: 0,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 100_000,
            estimated_cost_usd: 1.5,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec {
                original_count: 5,
                selected_count: 5,
                ..FilterSpec::default()
            },
            manifest: None,
            cost_limit_usd: Some(1.0),
            cache_hit_rate: 0.0,
            instances: vec![],
            rate_limit_events: None,
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
    fn redact_argv_masks_secret_flags() {
        let redacted = redact_argv(
            vec![
                "rust-swe-agent".into(),
                "--anthropic-api-key".into(),
                "sk-test".into(),
                "--github-token=ghp_123".into(),
            ],
            &crate::config::RedactionCfg::default(),
        );
        assert_eq!(redacted[2], "<redacted>");
        assert_eq!(redacted[3], "--github-token=<redacted>");
    }

    #[test]
    fn redact_argv_does_not_mask_max_tokens_flag() {
        let redacted = redact_argv(
            vec![
                "rust-swe-agent".into(),
                "--max-tokens".into(),
                "4096".into(),
            ],
            &crate::config::RedactionCfg::default(),
        );
        assert_eq!(redacted[1], "--max-tokens");
        assert_eq!(redacted[2], "4096");
    }

    #[test]
    fn redact_argv_uses_redactor_for_embedded_literals() {
        let cfg = crate::config::RedactionCfg {
            secret_literals: vec!["review-secret-value".into()],
            ..crate::config::RedactionCfg::default()
        };
        let redacted = redact_argv(
            vec![
                "rust-swe-agent".into(),
                "--note=prefix-review-secret-value-suffix".into(),
            ],
            &cfg,
        );

        assert!(!redacted[1].contains("review-secret-value"));
        assert!(redacted[1].contains("[REDACTED:configured_literal:"));
    }

    #[test]
    fn dataset_sha256_is_stable_for_identical_bytes() {
        let a = sha256_hex(br#"{"instance_id":"x"}"#);
        let b = sha256_hex(br#"{"instance_id":"x"}"#);
        assert_eq!(a, b);
    }

    #[test]
    fn json_redaction_preserves_max_tokens_knob() {
        let mut cfg = serde_json::json!({
            "model": { "max_tokens": 4096, "api_key": "sk-test" }
        });
        let redactor = Redactor::default_enabled();
        redactor.redact_json_value(&mut cfg, surface::TRAJECTORY);
        assert_eq!(cfg["model"]["max_tokens"], 4096);
        assert!(
            cfg["model"]["api_key"]
                .as_str()
                .is_some_and(|value| value.starts_with("[REDACTED:env_key:")),
            "{cfg}"
        );
    }

    #[test]
    fn strip_json_nulls_removes_nulls_and_traverses_arrays() {
        let v = serde_json::json!({
            "a": null,
            "b": 1,
            "c": [{"x": null, "y": 2}],
        });
        let stripped = strip_json_nulls(v);
        // null object keys are stripped; array elements are recursed into
        assert_eq!(stripped, serde_json::json!({"b": 1, "c": [{"y": 2}]}));
    }

    #[test]
    fn manifest_config_uses_effective_runtime_overrides() {
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 7;
        cfg.root.model.name = "override-model".into();
        let args = SwebenchArgs {
            dataset_path: PathBuf::from("dataset.jsonl"),
            output_dir: PathBuf::from("out"),
            parallel: 1,
            reruns: 1,
            config: cfg,
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: StratifyMode::Proportional,
            max_retries: 0,
            retry_on: None,
            retry_backoff_base_ms: 1,
            retry_backoff_cap_s: 1,
            retry_on_resume: false,
            deterministic_responses: None,
            deterministic_usage_per_call: None,
            config_overlay_paths: Vec::new(),
            dry_run: false,
            skip_preflight: true,
            preflight_format: "text".into(),
            skip_model_probe: true,
            preflight_check_timeout_s: 10,
            preflight_total_timeout_s: 60,
            preflight_mode: "test".into(),
            skip_patch_validation: true,
            max_rpm: None,
            max_input_tpm: None,
            cancel_deadline_secs: 30,
            install_os_signal_handlers: false,
            cancellation_signals: None,
            github_pr: None,
        };
        let manifest = build_manifest(
            &args,
            "dataset",
            1,
            &FilterSpec::default(),
            "2026-01-01T00:00:00Z",
            None,
        );
        assert!(manifest.config.resolved.contains("step_limit = 7"));
        assert!(
            manifest
                .config
                .resolved
                .contains(r#"name = "override-model""#)
        );
    }

    #[test]
    fn manifest_records_resume_mode_from_args() {
        let args = SwebenchArgs {
            dataset_path: PathBuf::from("dataset.jsonl"),
            output_dir: PathBuf::from("out"),
            parallel: 1,
            reruns: 1,
            config: Config::defaults().unwrap(),
            resume: true,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: StratifyMode::Proportional,
            max_retries: 0,
            retry_on: None,
            retry_backoff_base_ms: 1,
            retry_backoff_cap_s: 1,
            retry_on_resume: false,
            deterministic_responses: None,
            deterministic_usage_per_call: None,
            config_overlay_paths: Vec::new(),
            dry_run: false,
            skip_preflight: true,
            preflight_format: "text".into(),
            skip_model_probe: true,
            preflight_check_timeout_s: 10,
            preflight_total_timeout_s: 60,
            preflight_mode: "test".into(),
            skip_patch_validation: true,
            max_rpm: None,
            max_input_tpm: None,
            cancel_deadline_secs: 30,
            install_os_signal_handlers: false,
            cancellation_signals: None,
            github_pr: None,
        };
        let manifest = build_manifest(
            &args,
            "dataset",
            1,
            &FilterSpec::default(),
            "2026-01-01T00:00:00Z",
            None,
        );
        assert!(manifest.runtime.resume_mode);
    }

    #[test]
    fn prompt_template_hash_changes_when_overlay_edits_prompt() {
        let cfg_a = Config::from_toml_str(
            r#"
[prompts]
system = "sys-a"
instance = "inst"
"#,
        )
        .unwrap();
        let cfg_b = Config::from_toml_str(
            r#"
[prompts]
system = "sys-b"
instance = "inst"
"#,
        )
        .unwrap();
        let args_a = SwebenchArgs {
            dataset_path: PathBuf::from("dataset.jsonl"),
            output_dir: PathBuf::from("out"),
            parallel: 1,
            reruns: 1,
            config: cfg_a,
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: StratifyMode::Proportional,
            max_retries: 0,
            retry_on: None,
            retry_backoff_base_ms: 1,
            retry_backoff_cap_s: 1,
            retry_on_resume: false,
            deterministic_responses: None,
            deterministic_usage_per_call: None,
            config_overlay_paths: Vec::new(),
            dry_run: false,
            skip_preflight: true,
            preflight_format: "text".into(),
            skip_model_probe: true,
            preflight_check_timeout_s: 10,
            preflight_total_timeout_s: 60,
            preflight_mode: "test".into(),
            skip_patch_validation: true,
            max_rpm: None,
            max_input_tpm: None,
            cancel_deadline_secs: 30,
            install_os_signal_handlers: false,
            cancellation_signals: None,
            github_pr: None,
        };
        let filter = FilterSpec::default();
        let m_a = build_manifest(&args_a, "dataset", 1, &filter, "2026-01-01T00:00:00Z", None);
        let args_b = SwebenchArgs {
            config: cfg_b,
            ..args_a
        };
        let m_b = build_manifest(&args_b, "dataset", 1, &filter, "2026-01-01T00:00:00Z", None);
        assert_ne!(m_a.prompt_template.sha256, m_b.prompt_template.sha256);
    }

    #[test]
    fn harness_dirty_flag_detects_uncommitted_changes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("a.txt"), "v1\n").unwrap();
        Command::new("git")
            .arg("init")
            .current_dir(p)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "tester"])
            .current_dir(p)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "tester@example.com"])
            .current_dir(p)
            .output()
            .unwrap();
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(p)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(p)
            .output()
            .unwrap();
        std::fs::write(p.join("a.txt"), "dirty\n").unwrap();
        let m = resolve_harness_manifest_for_dir(Some(p));
        assert_eq!(m.git_resolution, "ok");
        assert_eq!(m.git_dirty, Some(true));
        assert!(m.git_sha.is_some());
    }

    #[tokio::test]
    async fn panic_after_initial_manifest_still_leaves_manifest_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dataset = tmp.path().join("d.jsonl");
        std::fs::write(&dataset, r#"{"instance_id":"x"}"#).unwrap();
        let out = tmp.path().join("out");
        let args = SwebenchArgs {
            dataset_path: dataset,
            output_dir: out.clone(),
            parallel: 1,
            reruns: 1,
            config: Config::defaults().unwrap(),
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: StratifyMode::Proportional,
            max_retries: 0,
            retry_on: None,
            retry_backoff_base_ms: 1,
            retry_backoff_cap_s: 1,
            retry_on_resume: false,
            deterministic_responses: None,
            deterministic_usage_per_call: None,
            config_overlay_paths: Vec::new(),
            dry_run: false,
            skip_preflight: true,
            preflight_format: "text".into(),
            skip_model_probe: true,
            preflight_check_timeout_s: 10,
            preflight_total_timeout_s: 60,
            preflight_mode: "test".into(),
            skip_patch_validation: true,
            max_rpm: None,
            max_input_tpm: None,
            cancel_deadline_secs: 30,
            install_os_signal_handlers: false,
            cancellation_signals: None,
            github_pr: None,
        };
        {
            let mut hook = PANIC_AFTER_INITIAL_MANIFEST_WRITE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *hook = Some(PanicAfterInitialManifestHook {
                output_dir: out.clone(),
            });
        }
        let panicked = std::panic::AssertUnwindSafe(run(args))
            .catch_unwind()
            .await
            .is_err();
        {
            let mut hook = PANIC_AFTER_INITIAL_MANIFEST_WRITE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *hook = None;
        }
        assert!(panicked);
        let text = std::fs::read_to_string(out.join("results.json")).unwrap();
        let parsed: SweepResults = serde_json::from_str(&text).unwrap();
        assert!(parsed.manifest.is_some());
        assert!(
            parsed
                .manifest
                .as_ref()
                .unwrap()
                .runtime
                .finished_at_utc
                .is_none()
        );
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
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&traj).unwrap()).unwrap();

        let info = existing_trajectory_info(dir.path(), "inst-1").unwrap();
        assert_eq!(info.outcome.as_deref(), Some(outcome::SUBMITTED));
    }

    #[test]
    fn existing_trajectory_info_returns_none_for_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = trajectory_path_for(dir.path(), "inst-2");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
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
    fn backoff_cap_can_shrink_below_base_or_disable_waits() {
        let policy = RetryPolicy {
            max_retries: 2,
            retry_on: BTreeSet::from([FailureCategory::ModelApi]),
            backoff_base_ms: 2_000,
            backoff_cap_s: 1,
        };
        assert!(
            policy.backoff_for("inst", 1) <= std::time::Duration::from_secs(1),
            "cap below base should be honored"
        );

        let disabled = RetryPolicy {
            max_retries: 2,
            retry_on: BTreeSet::from([FailureCategory::ModelApi]),
            backoff_base_ms: 1_000,
            backoff_cap_s: 0,
        };
        assert_eq!(
            disabled.backoff_for("inst", 3),
            std::time::Duration::from_millis(0)
        );
    }

    #[test]
    fn cancelled_wait_trajectory_clears_prior_failure_category() {
        let output = tempfile::tempdir().unwrap();
        let traj_path = trajectory_path_for_run(output.path(), "retry-waiter", 1);
        std::fs::create_dir_all(traj_path.parent().unwrap()).unwrap();

        let mut trajectory = Trajectory::default();
        trajectory.info.task = Some("old task".into());
        trajectory.info.model_name = Some("old model".into());
        trajectory.info.exit_reason = Some("error".into());
        trajectory.info.outcome = Some(outcome::ERROR.into());
        trajectory.info.failure_category = Some(FailureCategory::ModelApi);
        trajectory.save_pretty(&traj_path).unwrap();

        persist_cancelled_wait_trajectory(
            output.path(),
            "retry-waiter",
            1,
            "cancelled task",
            "cancelled model",
        );

        let reread: Trajectory =
            serde_json::from_str(&std::fs::read_to_string(&traj_path).unwrap()).unwrap();
        assert_eq!(
            reread.info.exit_reason.as_deref(),
            Some(exit_reason::CANCELLED)
        );
        assert_eq!(reread.info.outcome.as_deref(), Some(outcome::ERROR));
        assert_eq!(reread.info.failure_category, None);
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
        let (filtered, spec) = apply_subset(
            instances.clone(),
            &ApplySubsetParams {
                instance_ids_arg: Some("b"),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].instance_id, "b");
        assert_eq!(spec.original_count, 2);
        assert_eq!(spec.selected_count, 1);

        let err = apply_subset(
            instances,
            &ApplySubsetParams {
                instance_ids_arg: Some("missing"),
                ..Default::default()
            },
        )
        .unwrap_err();
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
        let (a, _) = apply_subset(
            instances.clone(),
            &ApplySubsetParams {
                sample: Some(3),
                seed: Some(42),
                ..Default::default()
            },
        )
        .unwrap();
        let (b, _) = apply_subset(
            instances,
            &ApplySubsetParams {
                sample: Some(3),
                seed: Some(42),
                ..Default::default()
            },
        )
        .unwrap();
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
        let (filtered, spec) = apply_subset(
            instances,
            &ApplySubsetParams {
                instance_ids_arg: Some("a,b,c,d,e"),
                limit: Some(2),
                sample: Some(4),
                seed: Some(7),
                ..Default::default()
            },
        )
        .unwrap();
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
    fn stratified_sample_balanced_spreads_across_repos() {
        let mk = |id: &str, repo: &str| SweBenchInstance {
            instance_id: id.into(),
            repo: Some(repo.into()),
            base_commit: None,
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        };
        let instances = vec![
            mk("a1", "a"),
            mk("a2", "a"),
            mk("b1", "b"),
            mk("b2", "b"),
            mk("c1", "c"),
            mk("c2", "c"),
        ];
        let (filtered, _) = apply_subset(
            instances,
            &ApplySubsetParams {
                sample: Some(3),
                seed: Some(123),
                stratify_by: Some(StratifyBy::Repo),
                stratify_mode: StratifyMode::Balanced,
                ..Default::default()
            },
        )
        .unwrap();
        let repos: HashSet<_> = filtered
            .into_iter()
            .map(|inst| inst.repo.unwrap_or_default())
            .collect();
        assert_eq!(repos.len(), 3);
    }

    #[test]
    fn stratified_sample_requires_sample_and_excludes_instance_ids() {
        let mk = |id: &str, repo: &str| SweBenchInstance {
            instance_id: id.into(),
            repo: Some(repo.into()),
            base_commit: None,
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        };
        let instances = vec![mk("a1", "a"), mk("b1", "b")];

        let err = apply_subset(
            instances.clone(),
            &ApplySubsetParams {
                stratify_by: Some(StratifyBy::Repo),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("`--stratify-by` requires `--sample`")
        );

        let err = apply_subset(
            instances,
            &ApplySubsetParams {
                instance_ids_arg: Some("a1"),
                sample: Some(1),
                seed: Some(1),
                stratify_by: Some(StratifyBy::Repo),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("`--stratify-by` cannot be combined with `--instance-ids`")
        );
    }

    #[test]
    fn stratify_mode_requires_stratify_by() {
        let inst = SweBenchInstance {
            instance_id: "a1".into(),
            repo: Some("a".into()),
            base_commit: None,
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        };
        let err = apply_subset(
            vec![inst],
            &ApplySubsetParams {
                sample: Some(1),
                seed: Some(1),
                stratify_mode: StratifyMode::Balanced,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("`--stratify-mode` requires `--stratify-by`")
        );
    }

    #[test]
    fn stratified_sample_then_limit_is_not_repo_clustered() {
        let mk = |id: &str, repo: &str| SweBenchInstance {
            instance_id: id.into(),
            repo: Some(repo.into()),
            base_commit: None,
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        };
        let instances = vec![
            mk("a1", "a"),
            mk("a2", "a"),
            mk("b1", "b"),
            mk("b2", "b"),
            mk("c1", "c"),
            mk("c2", "c"),
        ];

        let (filtered, _) = apply_subset(
            instances,
            &ApplySubsetParams {
                limit: Some(3),
                sample: Some(6),
                seed: Some(5),
                stratify_by: Some(StratifyBy::Repo),
                stratify_mode: StratifyMode::Balanced,
                ..Default::default()
            },
        )
        .unwrap();

        let repos: HashSet<_> = filtered
            .iter()
            .map(|inst| inst.repo.clone().unwrap_or_default())
            .collect();
        assert!(
            repos.len() > 1,
            "expected mixed repos after limit; got {:?}",
            filtered
                .iter()
                .map(|inst| (&inst.instance_id, &inst.repo))
                .collect::<Vec<_>>()
        );
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
        let err = apply_subset(
            instances,
            &ApplySubsetParams {
                instance_ids_arg: Some("a"),
                limit: Some(0),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("produced zero instances"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn doctor_and_dry_run_report_render_identical_for_same_checks() {
        let checks = vec![
            CheckResult {
                status: CheckStatus::Ok,
                name: "dataset.read",
                message: "readable".into(),
            },
            CheckResult {
                status: CheckStatus::Warn,
                name: "config.unknown_top_level",
                message: "unknown key".into(),
            },
        ];
        let doctor = render_preflight_report(&checks, "text", "doctor").unwrap();
        let dry = render_preflight_report(&checks, "text", "dry_run").unwrap();
        assert_eq!(doctor, dry);
    }

    #[test]
    fn preflight_json_schema_has_stable_top_level_fields() {
        let checks = vec![CheckResult {
            status: CheckStatus::Ok,
            name: "dataset.read",
            message: "readable".into(),
        }];
        let payload = render_preflight_report(&checks, "json", "doctor").unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(v.get("mode").is_some());
        assert!(v.get("checks").is_some());
        let c0 = &v["checks"][0];
        assert!(c0.get("status").is_some());
        assert!(c0.get("name").is_some());
        assert!(c0.get("message").is_some());
    }

    #[tokio::test]
    async fn timed_sync_succeeds_within_budget() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let out = timed_sync("ok", 1, deadline, || -> Result<u32, std::io::Error> {
            Ok(7)
        })
        .await
        .unwrap();
        assert_eq!(out, 7);
    }

    #[tokio::test]
    async fn timed_sync_times_out() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let err = timed_sync("slow", 0, deadline, || -> Result<(), std::io::Error> {
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timeout exceeded"));
    }

    #[test]
    fn ensure_total_deadline_errors_when_expired() {
        let deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        let err = ensure_total_deadline(deadline).unwrap_err();
        assert!(err.to_string().contains("total timeout exceeded"));
    }

    #[test]
    fn render_preflight_text_mode_is_line_oriented() {
        let checks = vec![
            CheckResult {
                status: CheckStatus::Ok,
                name: "a",
                message: "x".into(),
            },
            CheckResult {
                status: CheckStatus::Warn,
                name: "b",
                message: "y".into(),
            },
        ];
        let out = render_preflight_report(&checks, "text", "doctor").unwrap();
        assert!(out.contains("[ok] a"));
        assert!(out.contains("[warn] b"));
    }

    // ── Issue #44: rate-limit governor ────────────────────────────────────────

    #[test]
    fn rate_limit_events_serializes_to_json() {
        use crate::run::rate_limit::RateLimitEvents;
        let events = RateLimitEvents {
            throttled_calls: 7,
            total_throttled_seconds: 2.5,
            peak_concurrent: 8,
            configured_max_rpm: Some(4000),
            configured_max_input_tpm: Some(400_000),
        };
        let json = serde_json::to_string(&events).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["throttled_calls"], 7);
        assert_eq!(v["total_throttled_seconds"], 2.5);
        assert_eq!(v["peak_concurrent"], 8);
        assert_eq!(v["configured_max_rpm"], 4000);
        assert_eq!(v["configured_max_input_tpm"], 400_000u64);
    }

    #[test]
    fn sweep_results_has_rate_limit_events_field() {
        // rate_limit_events: None should be omitted from JSON (skip_serializing_if)
        let s = SweepResults {
            total: 1,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 1,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![],
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            rate_limit_events: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            v.get("rate_limit_events").is_none(),
            "None rate_limit_events should be omitted from JSON"
        );
    }

    #[test]
    fn swebench_args_has_max_rpm_and_max_input_tpm_fields() {
        let args = SwebenchArgs {
            dataset_path: PathBuf::from("d.jsonl"),
            output_dir: PathBuf::from("out"),
            parallel: 4,
            reruns: 1,
            config: Config::defaults().unwrap(),
            resume: false,
            cost_limit_usd: None,
            task_timeout_secs: None,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: StratifyMode::Proportional,
            max_retries: 0,
            retry_on: None,
            retry_backoff_base_ms: 0,
            retry_backoff_cap_s: 0,
            retry_on_resume: false,
            deterministic_responses: None,
            deterministic_usage_per_call: None,
            config_overlay_paths: vec![],
            dry_run: false,
            skip_preflight: true,
            preflight_format: "text".into(),
            skip_model_probe: true,
            preflight_check_timeout_s: 10,
            preflight_total_timeout_s: 60,
            preflight_mode: "test".into(),
            skip_patch_validation: false,
            max_rpm: Some(4000),
            max_input_tpm: Some(400_000),
            cancel_deadline_secs: 30,
            install_os_signal_handlers: false,
            cancellation_signals: None,
            github_pr: None,
        };
        assert_eq!(args.max_rpm, Some(4000));
        assert_eq!(args.max_input_tpm, Some(400_000));
    }

    #[test]
    fn governor_is_none_when_no_rate_limit_flags() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(None, None, 4);
        assert!(g.is_none(), "governor must be None when no flags are set");
    }

    #[test]
    fn governor_is_some_when_max_rpm_set() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(Some(600), None, 4);
        assert!(g.is_some());
    }

    #[test]
    fn governor_is_some_when_max_input_tpm_set() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(None, Some(100_000), 4);
        assert!(g.is_some());
    }

    #[tokio::test]
    async fn governor_acquire_is_immediate_when_bucket_has_capacity() {
        use crate::run::rate_limit::RateLimitGovernor;
        // 6000 RPM = 100 req/sec bucket; first call should be instant
        let g = RateLimitGovernor::new(Some(6000), None, 1).unwrap();
        let start = std::time::Instant::now();
        g.acquire(0).await;
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "first acquire should be immediate"
        );
    }

    #[tokio::test]
    async fn governor_acquire_blocks_when_rpm_bucket_empty() {
        use crate::run::rate_limit::RateLimitGovernor;
        // 60 RPM = 1 req/sec; drain the bucket then acquire should block ~1s
        let g = RateLimitGovernor::new(Some(60), None, 1).unwrap();
        // Drain the initial token
        g.acquire(0).await;
        // Second acquire must wait for refill
        let start = std::time::Instant::now();
        g.acquire(0).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(800),
            "should block ~1s for 60 RPM bucket, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn governor_global_retry_after_blocks_subsequent_acquire() {
        use crate::run::rate_limit::RateLimitGovernor;
        // High RPM so bucket is not the bottleneck
        let g = RateLimitGovernor::new(Some(6000), None, 4).unwrap();
        // Simulate a 429 with Retry-After: 1s
        g.report_429(Some(1)).await;
        // acquire should now block ~1s
        let start = std::time::Instant::now();
        g.acquire(0).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(800),
            "acquire should wait for global retry-after, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn governor_retry_after_applies_globally_to_concurrent_workers() {
        use crate::run::rate_limit::RateLimitGovernor;
        use std::sync::Arc;
        let g = Arc::new(RateLimitGovernor::new(Some(6000), None, 4).unwrap());
        // Worker 1 reports a 429 with Retry-After: 1
        g.report_429(Some(1)).await;
        // Worker 2 (concurrent) should also respect the global floor
        let g2 = g.clone();
        let start = std::time::Instant::now();
        g2.acquire(0).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(800),
            "global retry-after should block all workers, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn governor_aimd_not_triggered_before_three_429s() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(Some(6000), None, 4).unwrap();
        g.report_429(None).await;
        g.report_429(None).await;
        // Only 2 consecutive 429s without Retry-After — AIMD should NOT trigger
        assert_eq!(g.suppressed_slots_count().await, 0);
    }

    #[tokio::test]
    async fn governor_aimd_triggers_after_three_consecutive_429s_without_retry_after() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(Some(6000), None, 4).unwrap();
        g.report_429(None).await;
        g.report_429(None).await;
        g.report_429(None).await;
        assert!(
            g.suppressed_slots_count().await > 0,
            "AIMD should suppress after 3 consecutive 429s without Retry-After"
        );
    }

    #[tokio::test]
    async fn governor_retry_after_resets_aimd_counter() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(Some(6000), None, 4).unwrap();
        g.report_429(None).await;
        g.report_429(None).await;
        // Retry-After resets the counter
        g.report_429(Some(1)).await;
        g.report_429(None).await;
        g.report_429(None).await;
        // Only 2 consecutive no-retry-after 429s after the reset — no AIMD
        assert_eq!(g.suppressed_slots_count().await, 0);
    }

    #[tokio::test]
    async fn governor_events_tracks_throttled_calls() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(Some(6000), None, 4).unwrap();
        // Report a 429 (counts as throttled)
        g.report_429(Some(1)).await;
        let events = g.events().await;
        assert!(
            events.throttled_calls > 0,
            "throttled_calls should be incremented"
        );
        assert_eq!(events.configured_max_rpm, Some(6000));
    }

    #[test]
    fn parse_retry_after_from_error_extracts_seconds() {
        use crate::run::rate_limit::RateLimitGovernor;
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error("rate limited: retry-after: 30"),
            Some(30)
        );
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error("429 Too Many Requests"),
            None
        );
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error("retry-after: 5 seconds"),
            Some(5)
        );
    }

    #[test]
    fn parse_retry_after_handles_http_date_in_future() {
        use crate::run::rate_limit::{RateLimitGovernor, civil_to_unix};
        // Build a date 60 s in the future and verify the parser returns ~60.
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Use a fixed past/future date rather than decomposing a computed timestamp.
        let past_msg = "retry-after: Thu, 01 Jan 1970 00:00:00 GMT";
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error(past_msg),
            Some(0),
            "past HTTP-date should return 0 (saturating sub)"
        );
        // Verify the civil_to_unix epoch anchor.
        assert_eq!(
            civil_to_unix(1970, 1, 1, 0, 0, 0),
            Some(0),
            "unix epoch should be 0"
        );
        assert_eq!(
            civil_to_unix(2026, 10, 21, 12, 0, 0),
            Some(1_792_584_000),
            "known timestamp"
        );
        // Future HTTP-date yields Some(non-zero).
        let future_msg = "retry-after: Wed, 21 Oct 2026 12:00:00 GMT";
        let parsed = RateLimitGovernor::parse_retry_after_from_error(future_msg).unwrap();
        let expected = 1_792_584_000u64.saturating_sub(now_unix);
        assert_eq!(parsed, expected, "future HTTP-date seconds mismatch");
        // Numeric still works alongside HTTP-date support.
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error("retry-after: 42"),
            Some(42)
        );
    }

    #[test]
    fn config_sweep_keys_round_trip_toml() {
        use crate::config::Config;
        let cfg = Config::from_toml_str("[sweep]\nmax_rpm = 4000\nmax_input_tpm = 400000").unwrap();
        assert_eq!(cfg.root.sweep.max_rpm, Some(4000));
        assert_eq!(cfg.root.sweep.max_input_tpm, Some(400_000));
    }

    #[test]
    fn config_sweep_defaults_are_none() {
        use crate::config::Config;
        let cfg = Config::defaults().unwrap();
        assert_eq!(cfg.root.sweep.max_rpm, None);
        assert_eq!(cfg.root.sweep.max_input_tpm, None);
    }

    #[tokio::test]
    async fn deterministic_model_rate_limit_sentinel_returns_error() {
        use crate::model::{DeterministicModel, Model, QueryOpts};
        // Sentinel prefix "__rate_limited__:2" should cause ModelError::RateLimited
        let model = DeterministicModel::new(vec!["__rate_limited__:2".to_owned()]);
        let result = model.query(&[], &QueryOpts::default()).await;
        match result {
            Err(crate::error::ModelError::RateLimited(msg)) => {
                assert!(
                    msg.contains("retry-after: 2"),
                    "error should include retry-after seconds, got: {msg}"
                );
            }
            other => panic!("expected RateLimited error, got: {other:?}"),
        }
    }

    // ── Coverage: write_rate_limit_summary ───────────────────────────────────

    #[test]
    fn summary_table_includes_rate_limit_section_when_events_present() {
        use crate::run::rate_limit::RateLimitEvents;
        let s = SweepResults {
            total: 2,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 2,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 2,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![],
            rate_limit_events: Some(RateLimitEvents {
                throttled_calls: 12,
                total_throttled_seconds: 4.5,
                peak_concurrent: 6,
                configured_max_rpm: Some(4000),
                configured_max_input_tpm: Some(400_000),
            }),
        };
        let t = s.summary_table();
        assert!(
            t.contains("Rate-limit events:"),
            "missing section header: {t}"
        );
        assert!(
            t.contains("Throttled calls:    12"),
            "missing throttled_calls: {t}"
        );
        assert!(
            t.contains("Throttled secs:     4.5"),
            "missing throttled_secs: {t}"
        );
        assert!(
            t.contains("Peak concurrent:    6"),
            "missing peak_concurrent: {t}"
        );
        assert!(
            t.contains("Configured max-rpm: 4000"),
            "missing max-rpm: {t}"
        );
        assert!(
            t.contains("Configured max-tpm: 400000"),
            "missing max-tpm: {t}"
        );
    }

    #[test]
    fn summary_table_no_rate_limit_section_when_events_absent() {
        let s = SweepResults {
            total: 1,
            sweep_status: crate::run::swebench::SWEEP_STATUS_COMPLETED.into(),
            cancelled_at: None,
            cancel_deadline_at: None,
            cancel_exit_code: None,
            completed: 0,
            in_flight_at_cancel: 0,
            not_started: 0,
            submitted: 1,
            submitted_with_tests: 0,
            skipped: 0,
            errored: 0,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 1,
            patch_empty: 0,
            patch_apply_invalid: 0,
            github_pr_failures: 0,
            total_prompt_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cache_hit_rate: 0.0,
            retries: 0,
            retried_instances: 0,
            pass_at_k: 0.0,
            filter_spec: FilterSpec::default(),
            manifest: None,
            cost_limit_usd: None,
            instances: vec![],
            rate_limit_events: None,
        };
        let t = s.summary_table();
        assert!(
            !t.contains("Rate-limit events:"),
            "rate-limit section should be absent when events is None: {t}"
        );
    }

    // ── Coverage: governor TPM gating and tick_aimd ──────────────────────────

    #[tokio::test]
    async fn governor_tpm_gates_when_estimate_exceeds_bucket() {
        use crate::run::rate_limit::RateLimitGovernor;
        // 60 TPM = 1 token/sec. Start with 1 token (initial bucket).
        // First acquire with estimate=1 drains it; second should block.
        let g = RateLimitGovernor::new(None, Some(60), 1).unwrap();
        g.acquire(1).await; // drains the initial 1-token bucket
        let start = std::time::Instant::now();
        g.acquire(1).await; // must wait ~1 s for 1 TPM token to refill
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(800),
            "TPM gate should block ~1s when bucket is empty, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn governor_tick_aimd_restores_suppressed_slot() {
        use crate::run::rate_limit::RateLimitGovernor;
        // Trigger AIMD with 3 consecutive no-retry-after 429s.
        let g = RateLimitGovernor::new(Some(6000), None, 4).unwrap();
        g.report_429(None).await;
        g.report_429(None).await;
        g.report_429(None).await;
        let suppressed = g.suppressed_slots_count().await;
        assert!(suppressed > 0, "AIMD should have suppressed slots");
        // tick_aimd before the restoration window returns false
        let restored = g.tick_aimd().await;
        assert!(!restored, "tick_aimd should not restore before hold period");
    }

    #[tokio::test]
    async fn governor_tick_aimd_no_op_when_no_suppression() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(Some(6000), None, 4).unwrap();
        // No 429s reported; suppressed_slots == 0 — tick_aimd must be a no-op.
        assert!(!g.tick_aimd().await);
        assert_eq!(g.suppressed_slots_count().await, 0);
    }

    #[tokio::test]
    async fn governor_update_peak_concurrent_tracks_maximum() {
        use crate::run::rate_limit::RateLimitGovernor;
        let g = RateLimitGovernor::new(Some(6000), None, 4).unwrap();
        g.update_peak_concurrent(3).await;
        g.update_peak_concurrent(7).await;
        g.update_peak_concurrent(2).await;
        let events = g.events().await;
        assert_eq!(events.peak_concurrent, 7, "peak should be the maximum seen");
    }

    // ── Coverage: HTTP-date parse failure paths ──────────────────────────────

    #[test]
    fn parse_retry_after_returns_none_for_malformed_http_date() {
        use crate::run::rate_limit::RateLimitGovernor;
        // Too few parts.
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error("retry-after: Wed 21 Oct 2026"),
            None
        );
        // Non-GMT timezone.
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error(
                "retry-after: Wed, 21 Oct 2026 12:00:00 UTC"
            ),
            None
        );
        // Invalid month name.
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error(
                "retry-after: Wed, 21 Xyz 2026 12:00:00 GMT"
            ),
            None
        );
        // Malformed time field (not HH:MM:SS).
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error(
                "retry-after: Wed, 21 Oct 2026 12:00 GMT"
            ),
            None
        );
        // Non-numeric day.
        assert_eq!(
            RateLimitGovernor::parse_retry_after_from_error(
                "retry-after: Wed, XX Oct 2026 12:00:00 GMT"
            ),
            None
        );
    }

    #[test]
    fn civil_to_unix_returns_none_for_pre_epoch_date() {
        use crate::run::rate_limit::civil_to_unix;
        assert_eq!(
            civil_to_unix(1969, 12, 31, 23, 59, 59),
            None,
            "pre-epoch date should return None"
        );
        assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0), Some(0));
    }

    // ── Coverage: zero-limit treated as no-op ────────────────────────────────

    #[test]
    fn governor_new_treats_zero_rpm_as_none() {
        use crate::run::rate_limit::RateLimitGovernor;
        // max_rpm=0 with no TPM → governor should be None (no rate limiting).
        assert!(
            RateLimitGovernor::new(Some(0), None, 4).is_none(),
            "zero RPM should be treated as unset"
        );
        // max_input_tpm=0 with no RPM → also None.
        assert!(
            RateLimitGovernor::new(None, Some(0), 4).is_none(),
            "zero TPM should be treated as unset"
        );
        // Both zero → None.
        assert!(
            RateLimitGovernor::new(Some(0), Some(0), 4).is_none(),
            "both zero should be treated as unset"
        );
        // Non-zero RPM alongside zero TPM → governor active on RPM only.
        assert!(
            RateLimitGovernor::new(Some(600), Some(0), 4).is_some(),
            "non-zero RPM with zero TPM should still create a governor"
        );
    }
}
