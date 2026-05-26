//! `bench eval-flake`: quantify evaluator-side verdict noise on a completed sweep.
//!
//! Re-runs the evaluator against every instance's already-captured `.patch`
//! artifact N times and identifies which instances' verdicts are
//! non-deterministic. Writes `eval-flake.json` with per-instance verdicts,
//! flake flags, and a sweep-level summary.
//!
//! Zero new model calls. `total_cost_usd` is always `0.0`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::error::Error;
use crate::run::compare::load_sweep;
use crate::run::evaluate::{BreakdownSelection, EvalExitReason, EvaluateArgs, EvaluateBackend};

// ── Public types ──────────────────────────────────────────────────────────────

/// Per-replay verdict for a single instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Resolved,
    Unresolved,
    Errored,
}

/// Per-instance flake results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceFlakeResult {
    pub instance_id: String,
    /// One verdict per replay. Empty for instances without a patch (skipped).
    pub verdicts: Vec<Verdict>,
    /// True iff any two verdicts in `verdicts` disagree.
    pub is_flaky: bool,
    /// Fraction of verdict-pairs that disagree. Range `[0.0, 1.0]`.
    pub flake_rate: f32,
    /// Most common verdict across all replays. `None` when `verdicts` is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dominant_verdict: Option<Verdict>,
    /// Verdict recorded in the sweep's original evaluation (or results.json).
    /// `None` when not determinable (errored instance without a patch was skipped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_sweep_verdict: Option<Verdict>,
}

/// Sweep-level flake summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalFlakeSummary {
    pub replays: usize,
    pub instances_evaluated: usize,
    pub flaky_count: usize,
    pub flaky_rate: f32,
    /// Count of instances where the dominant verdict across replays differs from
    /// the sweep's originally recorded verdict — signals lucky/unlucky original runs.
    pub dominant_disagrees_with_sweep_count: usize,
}

/// Full eval-flake artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalFlakeReport {
    pub artifact_kind: String,
    pub schema_version: ArtifactSchemaVersion,
    pub instances: Vec<InstanceFlakeResult>,
    pub summary: EvalFlakeSummary,
    /// Always `0.0`; cost is purely evaluator (sb-cli) wall-clock time.
    pub total_cost_usd: f64,
}

impl EvalFlakeReport {
    /// Return the set of `instance_id`s that are flagged as flaky.
    #[must_use]
    pub fn flaky_ids(&self) -> std::collections::HashSet<String> {
        self.instances
            .iter()
            .filter(|i| i.is_flaky)
            .map(|i| i.instance_id.clone())
            .collect()
    }

    /// Load an `EvalFlakeReport` from a JSON file path.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::Trajectory(format!(
                "eval-flake: cannot read flake report `{}`: {e}",
                path.display()
            ))
        })?;
        serde_json::from_str(&text).map_err(|e| {
            Error::Trajectory(format!(
                "eval-flake: flake report `{}` is not valid JSON: {e}",
                path.display()
            ))
        })
    }
}

/// Arguments for the eval-flake command.
#[derive(Debug, Clone)]
pub struct EvalFlakeArgs {
    /// Completed sweep directory produced by `bench swebench`.
    pub sweep_dir: PathBuf,
    /// Number of times to replay the evaluator per instance. Default: 3.
    pub replays: usize,
    /// Output file path. Defaults to `<sweep_dir>/eval-flake.json`.
    pub output: Option<PathBuf>,
    /// Maximum parallel evaluator workers (mirrors `bench evaluate --concurrency`).
    pub concurrency: usize,
}

// ── Test stub types ────────────────────────────────────────────────────────────

/// One verdict entry for the stub evaluator.
#[derive(Debug, Clone)]
pub struct InstanceVerdictStub {
    pub instance_id: String,
    pub verdict: Verdict,
}

impl InstanceVerdictStub {
    #[must_use]
    pub fn new(instance_id: impl Into<String>, verdict: Verdict) -> Self {
        Self {
            instance_id: instance_id.into(),
            verdict,
        }
    }
}

/// Configuration for the stub evaluator used in integration tests.
///
/// Maps each `instance_id` to a sequence of verdicts, one per replay.
/// The `run_with_stub` function consumes entries in order.
#[derive(Debug, Clone)]
pub struct EvalFlakeStubConfig {
    /// Per-instance verdict sequences. Index 0 = replay 1, etc.
    pub verdicts: HashMap<String, Vec<InstanceVerdictStub>>,
}

// ── Core aggregation logic ─────────────────────────────────────────────────────

/// Compute `flake_rate` as the fraction of distinct index pairs (i,j) with i<j
/// where `verdicts[i] != verdicts[j]`.
///
/// Returns `0.0` for fewer than 2 verdicts.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn compute_flake_rate(verdicts: &[Verdict]) -> f32 {
    let n = verdicts.len();
    if n < 2 {
        return 0.0;
    }
    let total_pairs = n * (n - 1) / 2;
    let mut disagreeing = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            if verdicts[i] != verdicts[j] {
                disagreeing += 1;
            }
        }
    }
    disagreeing as f32 / total_pairs as f32
}

/// Compute the majority verdict (most frequent). Ties broken by Resolved > Unresolved > Errored.
#[must_use]
pub fn dominant_verdict(verdicts: &[Verdict]) -> Option<Verdict> {
    if verdicts.is_empty() {
        return None;
    }
    let mut counts = [0usize; 3]; // [Resolved, Unresolved, Errored]
    for v in verdicts {
        match v {
            Verdict::Resolved => counts[0] += 1,
            Verdict::Unresolved => counts[1] += 1,
            Verdict::Errored => counts[2] += 1,
        }
    }
    let max = counts[0].max(counts[1]).max(counts[2]);
    if counts[0] == max {
        Some(Verdict::Resolved)
    } else if counts[1] == max {
        Some(Verdict::Unresolved)
    } else {
        Some(Verdict::Errored)
    }
}

fn build_instance_result(
    instance_id: String,
    verdicts: Vec<Verdict>,
    original_sweep_verdict: Option<Verdict>,
) -> InstanceFlakeResult {
    let flake_rate = compute_flake_rate(&verdicts);
    let is_flaky = flake_rate > 0.0;
    let dom = dominant_verdict(&verdicts);
    InstanceFlakeResult {
        instance_id,
        verdicts,
        is_flaky,
        flake_rate,
        dominant_verdict: dom,
        original_sweep_verdict,
    }
}

#[allow(clippy::cast_precision_loss)]
fn build_summary(instances: &[InstanceFlakeResult], replays: usize) -> EvalFlakeSummary {
    let instances_evaluated = instances.len();
    let flaky_count = instances.iter().filter(|i| i.is_flaky).count();
    let flaky_rate = if instances_evaluated == 0 {
        0.0_f32
    } else {
        flaky_count as f32 / instances_evaluated as f32
    };
    let dominant_disagrees_with_sweep_count = instances
        .iter()
        .filter(|i| {
            // Only count when both dominant and original are known
            match (i.dominant_verdict, i.original_sweep_verdict) {
                (Some(d), Some(o)) => d != o,
                _ => false,
            }
        })
        .count();
    EvalFlakeSummary {
        replays,
        instances_evaluated,
        flaky_count,
        flaky_rate,
        dominant_disagrees_with_sweep_count,
    }
}

fn write_report(report: &EvalFlakeReport, output_path: &Path) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(report).map_err(Error::Json)?;
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(output_path, json)?;
    Ok(())
}

fn effective_output_path(args: &EvalFlakeArgs) -> PathBuf {
    args.output
        .clone()
        .unwrap_or_else(|| args.sweep_dir.join("eval-flake.json"))
}

// ── Load original sweep verdicts ───────────────────────────────────────────────

/// Read per-instance verdicts from the sweep's evaluation.json (preferred)
/// or results.json (fallback).
fn load_original_verdicts(sweep_dir: &Path) -> HashMap<String, Verdict> {
    // Try evaluation.json first
    let eval_path = crate::run::evaluate::evaluation_path(sweep_dir);
    if eval_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&eval_path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                let mut map = HashMap::new();
                if let Some(instances) = val["instances"].as_array() {
                    for inst in instances {
                        let id = inst["instance_id"].as_str().unwrap_or("").to_owned();
                        let resolved = inst["resolved"].as_bool().unwrap_or(false);
                        if !id.is_empty() {
                            let v = if resolved {
                                Verdict::Resolved
                            } else {
                                Verdict::Unresolved
                            };
                            map.insert(id, v);
                        }
                    }
                }
                if !map.is_empty() {
                    return map;
                }
            }
        }
    }

    // Fallback: results.json
    let results_path = sweep_dir.join("results.json");
    if let Ok(text) = std::fs::read_to_string(&results_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            let mut map = HashMap::new();
            if let Some(instances) = val["instances"].as_array() {
                for inst in instances {
                    let id = inst["instance_id"].as_str().unwrap_or("").to_owned();
                    let resolved_count = inst["resolved_count"].as_u64().unwrap_or(0);
                    if !id.is_empty() {
                        let v = if resolved_count > 0 {
                            Verdict::Resolved
                        } else {
                            Verdict::Unresolved
                        };
                        map.insert(id, v);
                    }
                }
            }
            return map;
        }
    }

    HashMap::new()
}

// ── Stub-based run (for integration tests) ────────────────────────────────────

/// Run eval-flake using a deterministic stub evaluator instead of sb-cli.
///
/// `stub.verdicts[instance_id][replay_index]` gives the verdict for that replay.
/// Used exclusively in integration tests.
pub fn run_with_stub(
    args: &EvalFlakeArgs,
    stub: &EvalFlakeStubConfig,
) -> Result<EvalFlakeReport, Error> {
    if args.replays < 1 {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "eval-flake: --replays must be at least 1".into(),
        )));
    }

    let original = load_original_verdicts(&args.sweep_dir);

    // Determine which instances have patches. An instance without a patch file
    // is skipped (verdicts: [], excluded from flaky_count).
    let mut instance_ids: Vec<String> = stub.verdicts.keys().cloned().collect();
    instance_ids.sort();

    // Filter: only evaluate instances that have a patch file in the sweep dir.
    let instance_ids: Vec<String> = instance_ids
        .into_iter()
        .filter(|id| {
            let patch_path = args.sweep_dir.join(format!("{id}.patch"));
            patch_path.exists()
        })
        .collect();

    let mut results: Vec<InstanceFlakeResult> = Vec::new();
    for id in &instance_ids {
        let stub_verdicts = stub.verdicts.get(id.as_str());
        let verdicts: Vec<Verdict> = (0..args.replays)
            .map(|i| {
                stub_verdicts
                    .and_then(|sv| sv.get(i))
                    .map_or(Verdict::Errored, |sv| sv.verdict)
            })
            .collect();
        let orig = original.get(id.as_str()).copied();
        results.push(build_instance_result(id.clone(), verdicts, orig));
    }

    // Sort by instance_id for deterministic output.
    results.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let summary = build_summary(&results, args.replays);
    let report = EvalFlakeReport {
        artifact_kind: ArtifactKind::EvalFlakeReport.label().to_owned(),
        schema_version: ArtifactSchemaVersion::CURRENT,
        instances: results,
        summary,
        total_cost_usd: 0.0,
    };

    let output_path = effective_output_path(args);
    write_report(&report, &output_path)?;

    Ok(report)
}

// ── Real evaluator run ─────────────────────────────────────────────────────────

/// Run eval-flake against a real sweep by re-invoking `bench evaluate` N times.
///
/// Each replay builds a `all_preds.jsonl` from the sweep's existing patch files,
/// calls the evaluator backend, and collects per-instance verdicts.
pub fn run(args: &EvalFlakeArgs) -> Result<EvalFlakeReport, Error> {
    if args.replays < 1 {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "eval-flake: --replays must be at least 1".into(),
        )));
    }

    let loaded = load_sweep(&args.sweep_dir).map_err(|e| {
        Error::Trajectory(format!(
            "eval-flake: failed to load sweep `{}`: {e}",
            args.sweep_dir.display()
        ))
    })?;

    let original = load_original_verdicts(&args.sweep_dir);

    // Collect all instances that have a patch file.
    let patchable: Vec<String> = {
        let mut ids: Vec<String> = loaded
            .instances
            .keys()
            .filter(|id| {
                let p = crate::run::swebench::existing_patch_path_for_run(&args.sweep_dir, id, 1);
                p.exists() && std::fs::metadata(&p).is_ok_and(|m| m.len() > 0)
            })
            .cloned()
            .collect();
        ids.sort();
        ids
    };

    if patchable.is_empty() {
        tracing::warn!(
            sweep_dir = %args.sweep_dir.display(),
            "eval-flake: no patch files found; nothing to evaluate"
        );
    }

    // Collect verdicts per instance across replays.
    let mut all_verdicts: HashMap<String, Vec<Verdict>> = patchable
        .iter()
        .map(|id| (id.clone(), Vec::new()))
        .collect();

    for replay_idx in 0..args.replays {
        tracing::info!(
            replay = replay_idx + 1,
            total = args.replays,
            "eval-flake: running evaluator replay"
        );

        let eval_args = EvaluateArgs {
            sweep_dir: args.sweep_dir.clone(),
            dataset_path: None,
            backend: EvaluateBackend::Rehearsal,
            timeout_per_instance_secs: 1800,
            parallel: args.concurrency,
            sb_subset: String::new(),
            sb_split: "test".to_owned(),
            run_id: Some(format!("eval-flake-replay-{}", replay_idx + 1)),
            breakdown: BreakdownSelection::none(),
            cost_attribution: false,
        };

        let eval_result = crate::run::evaluate::run(&eval_args)?;

        for inst_eval in &eval_result.instances {
            if let Some(verdicts) = all_verdicts.get_mut(&inst_eval.instance_id) {
                let v = match inst_eval.eval_exit_reason {
                    EvalExitReason::Resolved => Verdict::Resolved,
                    EvalExitReason::Unresolved => Verdict::Unresolved,
                    _ => Verdict::Errored,
                };
                verdicts.push(v);
            }
        }
    }

    let mut results: Vec<InstanceFlakeResult> = patchable
        .iter()
        .map(|id| {
            let verdicts = all_verdicts.remove(id).unwrap_or_default();
            let orig = original.get(id.as_str()).copied();
            build_instance_result(id.clone(), verdicts, orig)
        })
        .collect();

    results.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let summary = build_summary(&results, args.replays);
    let report = EvalFlakeReport {
        artifact_kind: ArtifactKind::EvalFlakeReport.label().to_owned(),
        schema_version: ArtifactSchemaVersion::CURRENT,
        instances: results,
        summary,
        total_cost_usd: 0.0,
    };

    let output_path = effective_output_path(args);
    write_report(&report, &output_path)?;
    tracing::info!(
        output = %output_path.display(),
        flaky_count = report.summary.flaky_count,
        instances_evaluated = report.summary.instances_evaluated,
        "eval-flake complete"
    );
    Ok(report)
}

// ── Unit tests for aggregation logic ──────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn flake_rate_alternating_three() {
        let verdicts = [Verdict::Resolved, Verdict::Unresolved, Verdict::Resolved];
        let rate = compute_flake_rate(&verdicts);
        // 2 disagreeing pairs out of 3 total
        assert!((rate - 2.0_f32 / 3.0_f32).abs() < 1e-5, "got {rate}");
    }

    #[test]
    fn flake_rate_all_same() {
        let verdicts = [Verdict::Resolved, Verdict::Resolved, Verdict::Resolved];
        assert_eq!(compute_flake_rate(&verdicts), 0.0_f32);
    }

    #[test]
    fn flake_rate_single() {
        assert_eq!(compute_flake_rate(&[Verdict::Resolved]), 0.0_f32);
    }

    #[test]
    fn flake_rate_empty() {
        assert_eq!(compute_flake_rate(&[]), 0.0_f32);
    }

    #[test]
    fn dominant_verdict_majority() {
        let v = [Verdict::Resolved, Verdict::Unresolved, Verdict::Resolved];
        assert_eq!(dominant_verdict(&v), Some(Verdict::Resolved));
    }

    #[test]
    fn dominant_verdict_tie_prefers_resolved() {
        let v = [Verdict::Resolved, Verdict::Unresolved];
        assert_eq!(dominant_verdict(&v), Some(Verdict::Resolved));
    }

    #[test]
    fn dominant_verdict_empty() {
        assert_eq!(dominant_verdict(&[]), None);
    }

    #[test]
    fn is_flaky_true_when_rate_positive() {
        let v = [Verdict::Resolved, Verdict::Unresolved, Verdict::Resolved];
        let rate = compute_flake_rate(&v);
        assert!(rate > 0.0);
    }

    #[test]
    fn build_summary_counts() {
        let instances = vec![
            build_instance_result(
                "a".into(),
                vec![Verdict::Resolved, Verdict::Unresolved],
                None,
            ),
            build_instance_result("b".into(), vec![Verdict::Resolved, Verdict::Resolved], None),
        ];
        let summary = build_summary(&instances, 2);
        assert_eq!(summary.flaky_count, 1);
        assert_eq!(summary.instances_evaluated, 2);
        assert!((summary.flaky_rate - 0.5_f32).abs() < 1e-5);
    }
}
