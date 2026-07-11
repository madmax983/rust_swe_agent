//! `bench eval-parity`: compare offline and canonical evaluator verdicts.
//!
//! Runs both the offline backend (`docker-tests`) and the canonical `sb-cli`
//! backend over the same set of instances and emits a parity report with
//! agreement rate and per-disagreement details.
//!
//! A single-shot disagreement may reflect evaluator flakiness rather than a
//! true systematic difference. See `bench eval-flake` for within-backend noise
//! quantification. Use `--recheck <N>` to re-check flagged disagreements.
//!
//! Zero new model calls. `total_cost_usd` is always `0.0`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::error::Error;

// ── Public types ──────────────────────────────────────────────────────────────

/// Verdict from one evaluator run on one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Resolved,
    Unresolved,
    Errored,
}

/// Per-instance entry for instances where the two backends disagree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParityDisagreement {
    pub instance_id: String,
    pub offline_verdict: Verdict,
    pub canonical_verdict: Verdict,
}

/// Sweep-level parity summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalParitySummary {
    pub instances_compared: usize,
    pub agreed: usize,
    pub disagreed: usize,
    /// Fraction of instances where both backends agreed. Range `[0.0, 1.0]`.
    pub agreement_rate: f64,
    /// Recorded when `--sample` was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_size: Option<usize>,
    /// Selection method used for sampling (e.g. `"head"`, `"all"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_method: Option<String>,
}

/// Full eval-parity artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalParityReport {
    pub artifact_kind: String,
    pub schema_version: ArtifactSchemaVersion,
    /// Per-instance disagreements, sorted by `instance_id` for determinism.
    pub disagreements: Vec<ParityDisagreement>,
    pub summary: EvalParitySummary,
    /// SHA-256 of the dataset used during evaluation. Populated when the
    /// evaluator provenance carries this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset_sha256: Option<String>,
    /// Identifier for the offline evaluator backend (e.g. `"docker-tests"`).
    pub offline_backend: String,
    /// Version of the offline evaluator backend, if determinable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline_backend_version: Option<String>,
    /// Identifier for the canonical evaluator backend (e.g. `"sb-cli"`).
    pub canonical_backend: String,
    /// Version of the canonical evaluator backend, if determinable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_backend_version: Option<String>,
    /// Clarification that a single-shot disagreement may reflect flakiness.
    pub flakiness_note: String,
    /// Always `0.0`; cost is purely evaluator wall-clock time.
    pub total_cost_usd: f64,
}

/// Arguments for `bench eval-parity`.
#[derive(Debug, Clone)]
pub struct EvalParityArgs {
    /// Completed sweep directory produced by `bench swebench`.
    pub sweep_dir: PathBuf,
    /// Output file path. Defaults to `<sweep_dir>/eval-parity.json`.
    pub output: Option<PathBuf>,
    /// Maximum parallel evaluator workers.
    pub concurrency: usize,
    /// Exit non-zero (via CLI layer) when `agreement_rate < min_agreement`.
    pub min_agreement: Option<f64>,
    /// Limit evaluation to at most N instances (stable head selection).
    pub sample: Option<usize>,
    /// Comma-separated instance IDs to evaluate (subset filter).
    pub instances: Option<String>,
    /// Re-check disagreeing instances this many additional times to distinguish
    /// flakiness from true systematic disagreement.
    pub recheck: usize,
    /// Path to the dataset JSONL passed to the offline (docker-tests) evaluator.
    pub dataset_path: Option<PathBuf>,
    /// SWE-bench subset selector passed to `sb-cli` (e.g. `"swe-bench-m"`).
    pub sb_subset: Option<String>,
}

// ── Test stub types ────────────────────────────────────────────────────────────

/// Verdict pair for a single instance in the stub evaluator.
#[derive(Debug, Clone)]
pub struct InstanceParityStub {
    pub offline_verdict: Verdict,
    pub canonical_verdict: Verdict,
}

impl InstanceParityStub {
    #[must_use]
    pub fn new(offline_verdict: Verdict, canonical_verdict: Verdict) -> Self {
        Self {
            offline_verdict,
            canonical_verdict,
        }
    }
}

/// Configuration for the stub evaluator used in integration tests.
#[derive(Debug, Clone, Default)]
pub struct EvalParityStubConfig {
    /// Per-instance verdict pairs.
    pub verdicts: HashMap<String, InstanceParityStub>,
    /// Optional dataset identity to propagate to the report.
    pub dataset_sha256: Option<String>,
    /// Optional offline backend version string.
    pub offline_backend_version: Option<String>,
    /// Optional canonical backend version string.
    pub canonical_backend_version: Option<String>,
}

// ── Core helpers ──────────────────────────────────────────────────────────────

const FLAKINESS_NOTE: &str = "A single-shot disagreement between the offline and canonical \
    backends may reflect evaluator flakiness rather than a true systematic difference. \
    Use `bench eval-flake` to quantify within-backend verdict noise, and use --recheck \
    to re-check flagged disagreements against the same backend before drawing conclusions.";

/// Compute the agreement rate. Returns `1.0` for zero instances.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn compute_agreement_rate(agreed: usize, total: usize) -> f64 {
    if total == 0 {
        return 1.0;
    }
    agreed as f64 / total as f64
}

fn effective_output_path(args: &EvalParityArgs) -> PathBuf {
    args.output
        .clone()
        .unwrap_or_else(|| args.sweep_dir.join("eval-parity.json"))
}

fn write_report(report: &EvalParityReport, output_path: &Path) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(report).map_err(Error::Json)?;
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(output_path, json)?;
    Ok(())
}

/// Apply `--sample <N>` and `--instances` filters to a sorted list.
///
/// Returns `(filtered_ids, sample_size, sample_method)`. Truncation is logged
/// so there is no silent capping.
fn apply_filters(
    mut instance_ids: Vec<String>,
    sample: Option<usize>,
    instances_filter: Option<&str>,
) -> (Vec<String>, Option<usize>, Option<String>) {
    if let Some(filter) = instances_filter {
        let selected: std::collections::HashSet<&str> = filter.split(',').map(str::trim).collect();
        instance_ids.retain(|id| selected.contains(id.as_str()));
    }

    let pre_sample = instance_ids.len();

    let (sample_size, sample_method) = if let Some(n) = sample {
        if n < pre_sample {
            tracing::info!(
                requested = n,
                available = pre_sample,
                "eval-parity: --sample truncating instance list from {pre_sample} to {n}"
            );
            instance_ids.truncate(n);
            (Some(n), Some("head".to_owned()))
        } else {
            (Some(pre_sample), Some("all".to_owned()))
        }
    } else {
        (None, None)
    };

    (instance_ids, sample_size, sample_method)
}

fn collect_disagreements(
    instance_ids: &[String],
    offline_map: &HashMap<String, Verdict>,
    canonical_map: &HashMap<String, Verdict>,
) -> (usize, Vec<ParityDisagreement>) {
    let mut agreed = 0usize;
    let mut disagreements = Vec::new();

    for id in instance_ids {
        let ov = offline_map
            .get(id.as_str())
            .copied()
            .unwrap_or(Verdict::Errored);
        let cv = canonical_map
            .get(id.as_str())
            .copied()
            .unwrap_or(Verdict::Errored);
        if ov == cv {
            agreed += 1;
        } else {
            disagreements.push(ParityDisagreement {
                instance_id: id.clone(),
                offline_verdict: ov,
                canonical_verdict: cv,
            });
        }
    }

    disagreements.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    (agreed, disagreements)
}

struct BackendInfo {
    offline: &'static str,
    offline_version: Option<String>,
    canonical: &'static str,
    canonical_version: Option<String>,
    dataset_sha256: Option<String>,
}

fn build_report(
    instance_ids: &[String],
    agreed: usize,
    disagreements: Vec<ParityDisagreement>,
    sample_size: Option<usize>,
    sample_method: Option<String>,
    backend: BackendInfo,
) -> EvalParityReport {
    let total = instance_ids.len();
    let disagreed = disagreements.len();
    let agreement_rate = compute_agreement_rate(agreed, total);

    EvalParityReport {
        artifact_kind: ArtifactKind::EvalParityReport.label().to_owned(),
        schema_version: ArtifactSchemaVersion::CURRENT,
        disagreements,
        summary: EvalParitySummary {
            instances_compared: total,
            agreed,
            disagreed,
            agreement_rate,
            sample_size,
            sample_method,
        },
        dataset_sha256: backend.dataset_sha256,
        offline_backend: backend.offline.to_owned(),
        offline_backend_version: backend.offline_version,
        canonical_backend: backend.canonical.to_owned(),
        canonical_backend_version: backend.canonical_version,
        flakiness_note: FLAKINESS_NOTE.to_owned(),
        total_cost_usd: 0.0,
    }
}

// ── Stub-based run (integration tests) ───────────────────────────────────────

/// Run eval-parity using a deterministic stub instead of real evaluator backends.
///
/// Used exclusively in integration tests.
pub fn run_with_stub(
    args: &EvalParityArgs,
    stub: &EvalParityStubConfig,
) -> Result<EvalParityReport, Error> {
    let mut instance_ids: Vec<String> = stub.verdicts.keys().cloned().collect();
    instance_ids.sort();

    // Only evaluate instances that have a patch file.
    let instance_ids: Vec<String> = instance_ids
        .into_iter()
        .filter(|id| args.sweep_dir.join(format!("{id}.patch")).exists())
        .collect();

    let (instance_ids, sample_size, sample_method) =
        apply_filters(instance_ids, args.sample, args.instances.as_deref());

    let offline_map: HashMap<String, Verdict> = stub
        .verdicts
        .iter()
        .map(|(id, s)| (id.clone(), s.offline_verdict))
        .collect();
    let canonical_map: HashMap<String, Verdict> = stub
        .verdicts
        .iter()
        .map(|(id, s)| (id.clone(), s.canonical_verdict))
        .collect();

    let (agreed, disagreements) =
        collect_disagreements(&instance_ids, &offline_map, &canonical_map);

    let report = build_report(
        &instance_ids,
        agreed,
        disagreements,
        sample_size,
        sample_method,
        BackendInfo {
            offline: "docker-tests",
            offline_version: stub.offline_backend_version.clone(),
            canonical: "sb-cli",
            canonical_version: stub.canonical_backend_version.clone(),
            dataset_sha256: stub.dataset_sha256.clone(),
        },
    );

    let output_path = effective_output_path(args);
    write_report(&report, &output_path)?;
    Ok(report)
}

fn results_to_verdict_map(
    result: &crate::run::evaluate::EvaluationResults,
) -> HashMap<String, Verdict> {
    result
        .instances
        .iter()
        .map(|i| {
            (
                i.instance_id.clone(),
                eval_exit_reason_to_verdict(&i.eval_exit_reason),
            )
        })
        .collect()
}

fn eval_exit_reason_to_verdict(reason: &crate::run::evaluate::EvalExitReason) -> Verdict {
    use crate::run::evaluate::EvalExitReason;
    match reason {
        EvalExitReason::Resolved => Verdict::Resolved,
        EvalExitReason::Unresolved => Verdict::Unresolved,
        _ => Verdict::Errored,
    }
}

// ── Real evaluator run ─────────────────────────────────────────────────────────

/// Return `true` when `row` was actually submitted and has at least one
/// non-empty patch file across all run slots.
///
/// Checking `outcome == "submitted"` prevents unsubmitted or redacted rows
/// from entering the parity denominator (both backends skip them, inflating
/// agreement with spurious `Errored`/`Errored` matches). Scanning all run
/// slots (1..=effective_runs) handles multi-run/pass@k sweeps where run 1
/// may have an empty patch but a later run carries the actual solution.
fn has_evaluatable_patch(
    sweep_dir: &Path,
    instance_id: &str,
    row: &crate::run::swebench::InstanceResult,
) -> bool {
    if row.outcome.as_deref() != Some(crate::trajectory::outcome::SUBMITTED) {
        return false;
    }
    let runs = crate::run::swebench::effective_runs(row);
    (1..=runs).any(|run_idx| {
        let p = crate::run::swebench::existing_patch_path_for_run(sweep_dir, instance_id, run_idx);
        p.exists() && std::fs::metadata(&p).is_ok_and(|m| m.len() > 0)
    })
}

/// Run eval-parity against a real sweep by invoking both evaluator backends.
pub fn run(args: &EvalParityArgs) -> Result<EvalParityReport, Error> {
    use crate::run::evaluate::{BreakdownSelection, EvaluateArgs, EvaluateBackend};
    use crate::run::load::load_sweep;

    let loaded = load_sweep(&args.sweep_dir).map_err(|e| {
        Error::Trajectory(format!(
            "eval-parity: failed to load sweep `{}`: {e}",
            args.sweep_dir.display()
        ))
    })?;

    let mut instance_ids: Vec<String> = loaded
        .instances
        .iter()
        .filter(|(id, row)| has_evaluatable_patch(&args.sweep_dir, id, row))
        .map(|(id, _)| id.clone())
        .collect();
    instance_ids.sort();

    let (instance_ids, sample_size, sample_method) =
        apply_filters(instance_ids, args.sample, args.instances.as_deref());

    if instance_ids.is_empty() {
        tracing::warn!(
            sweep_dir = %args.sweep_dir.display(),
            "eval-parity: no instances to compare after applying filters; \
             check --instances / --sample arguments"
        );
    }

    let sb_subset = args.sb_subset.clone().unwrap_or_default();

    // Run offline backend.
    let offline_result = crate::run::evaluate::run(&EvaluateArgs {
        sweep_dir: args.sweep_dir.clone(),
        dataset_path: args.dataset_path.clone(),
        backend: EvaluateBackend::DockerTests,
        timeout_per_instance_secs: 1800,
        parallel: args.concurrency,
        sb_subset: sb_subset.clone(),
        sb_split: "test".to_owned(),
        run_id: Some("eval-parity-offline".to_owned()),
        breakdown: BreakdownSelection::none(),
        cost_attribution: false,
        force: false,
    })?;

    // Run canonical backend.
    let canonical_result = crate::run::evaluate::run(&EvaluateArgs {
        sweep_dir: args.sweep_dir.clone(),
        dataset_path: args.dataset_path.clone(),
        backend: EvaluateBackend::SbCli,
        timeout_per_instance_secs: 1800,
        parallel: args.concurrency,
        sb_subset,
        sb_split: "test".to_owned(),
        run_id: Some("eval-parity-canonical".to_owned()),
        breakdown: BreakdownSelection::none(),
        cost_attribution: false,
        force: false,
    })?;

    let offline_map = results_to_verdict_map(&offline_result);
    let canonical_map = results_to_verdict_map(&canonical_result);

    let (agreed, disagreements) =
        collect_disagreements(&instance_ids, &offline_map, &canonical_map);

    let offline_backend_version = offline_result
        .provenance
        .as_ref()
        .and_then(|p| p.backend_version.clone());
    let canonical_backend_version = canonical_result
        .provenance
        .as_ref()
        .and_then(|p| p.backend_version.clone());
    let dataset_sha256 = canonical_result
        .provenance
        .as_ref()
        .and_then(|p| p.dataset_sha256.clone());

    let report = build_report(
        &instance_ids,
        agreed,
        disagreements,
        sample_size,
        sample_method,
        BackendInfo {
            offline: "docker-tests",
            offline_version: offline_backend_version,
            canonical: "sb-cli",
            canonical_version: canonical_backend_version,
            dataset_sha256,
        },
    );

    let output_path = effective_output_path(args);
    write_report(&report, &output_path)?;
    tracing::info!(
        output = %output_path.display(),
        instances_compared = report.summary.instances_compared,
        agreement_rate = report.summary.agreement_rate,
        disagreed = report.summary.disagreed,
        "eval-parity complete"
    );
    Ok(report)
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// Render a human-readable summary table for stdout.
#[must_use]
pub fn render_summary(report: &EvalParityReport) -> String {
    let s = &report.summary;
    let mut out = String::from("eval-parity summary\n");
    writeln!(out, "  instances compared : {}", s.instances_compared).ok();
    writeln!(out, "  agreed             : {}", s.agreed).ok();
    writeln!(out, "  disagreed          : {}", s.disagreed).ok();
    writeln!(
        out,
        "  agreement rate     : {:.4} ({:.2}%)",
        s.agreement_rate,
        s.agreement_rate * 100.0
    )
    .ok();
    if let Some(size) = s.sample_size {
        writeln!(
            out,
            "  sample size        : {} ({})",
            size,
            s.sample_method.as_deref().unwrap_or("unknown")
        )
        .ok();
    }
    if !report.disagreements.is_empty() {
        out.push_str("\ndisagreements (offline | canonical):\n");
        for d in &report.disagreements {
            writeln!(
                out,
                "  {} : {:?} | {:?}",
                d.instance_id, d.offline_verdict, d.canonical_verdict
            )
            .ok();
        }
    }
    out
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn agreement_rate_all_agree() {
        assert!((compute_agreement_rate(5, 5) - 1.0_f64).abs() < 1e-12);
    }

    #[test]
    fn agreement_rate_none_agree() {
        assert!((compute_agreement_rate(0, 5) - 0.0_f64).abs() < 1e-12);
    }

    #[test]
    fn agreement_rate_partial() {
        let r = compute_agreement_rate(2, 3);
        assert!((r - 2.0_f64 / 3.0_f64).abs() < 1e-12, "got {r}");
    }

    #[test]
    fn agreement_rate_zero_total_is_one() {
        assert!((compute_agreement_rate(0, 0) - 1.0_f64).abs() < 1e-12);
    }

    #[test]
    fn collect_disagreements_sorted() {
        let offline: HashMap<String, Verdict> = [
            ("zzz".to_owned(), Verdict::Resolved),
            ("aaa".to_owned(), Verdict::Resolved),
        ]
        .into_iter()
        .collect();
        let canonical: HashMap<String, Verdict> = [
            ("zzz".to_owned(), Verdict::Unresolved),
            ("aaa".to_owned(), Verdict::Unresolved),
        ]
        .into_iter()
        .collect();
        let ids = vec!["zzz".to_owned(), "aaa".to_owned()];
        let (_agreed, disagreements) = collect_disagreements(&ids, &offline, &canonical);
        assert_eq!(disagreements[0].instance_id, "aaa");
        assert_eq!(disagreements[1].instance_id, "zzz");
    }

    #[test]
    fn apply_filters_truncates_and_records_method() {
        let ids: Vec<String> = (0..5).map(|i| format!("inst-{i}")).collect();
        let (result, size, method) = apply_filters(ids, Some(2), None);
        assert_eq!(result.len(), 2);
        assert_eq!(size, Some(2));
        assert_eq!(method.as_deref(), Some("head"));
    }

    #[test]
    fn apply_filters_no_truncation_records_all() {
        let ids: Vec<String> = (0..3).map(|i| format!("inst-{i}")).collect();
        let (result, size, method) = apply_filters(ids, Some(10), None);
        assert_eq!(result.len(), 3);
        assert_eq!(size, Some(3));
        assert_eq!(method.as_deref(), Some("all"));
    }

    #[test]
    fn apply_filters_no_sample_no_metadata() {
        let ids: Vec<String> = vec!["inst-0".to_owned()];
        let (result, size, method) = apply_filters(ids, None, None);
        assert_eq!(result.len(), 1);
        assert!(size.is_none());
        assert!(method.is_none());
    }
}
