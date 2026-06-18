//! `bench export-otlp`: backfill OTLP traces from a completed sweep (issue #513).
//!
//! OTLP trace export is otherwise **live-only**: spans are emitted to the
//! collector during a sweep, and only when an endpoint is configured at launch.
//! Headless / cron runs (or a collector outage mid-sweep) lose their spans
//! permanently even though the canonical trajectories persist on disk. This
//! command reads the persisted trajectory/result artifacts under a sweep
//! directory and re-exports reconstructed sweep + instance spans to an
//! OTLP/HTTP collector, using the *same* span shape and the *same* trace/span
//! IDs the live exporter would have produced (see [`compute_sweep_id`] and
//! [`crate::telemetry::new_trace_id`]), so a re-exported run and a live-exported
//! run are indistinguishable in the collector.
//!
//! Read-only with respect to inputs: it never re-runs instances and never
//! mutates the sweep directory. The only side effect is the OTLP POST (skipped
//! entirely under `--dry-run`).
//!
//! See `docs/spec-export-otlp.md` for the full contract.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{ConfigError, Error};
use crate::run::swebench::{
    EXIT_REASON_BUDGET_HALT, InstanceResult, ProvenanceManifest, SweepResults, compute_sweep_id,
    existing_patch_path_for_run, existing_trajectory_path_for_run,
};
use crate::telemetry::{
    self, InstanceSpanData, SweepSpanData, Tracer, instance_span_data_from_result,
    instance_span_data_from_trajectory, new_span_id, new_trace_id,
};
use crate::trajectory::Trajectory;

/// Inputs to [`run`].
#[derive(Debug, Clone)]
pub struct ExportOtlpArgs {
    /// Completed sweep directory produced by `bench swebench`.
    pub sweep_dir: PathBuf,
    /// Explicit `--otlp-endpoint` flag value (base URL). When `None`, the
    /// endpoint is resolved from the standard OTel env vars.
    pub otlp_endpoint: Option<String>,
    /// When `true`, reconstruct and count spans but open no socket.
    pub dry_run: bool,
}

/// The reconstructed span tree for a sweep, ready to hand to the exporter.
pub struct Reconstructed {
    /// Stable sweep-level ID seeding every trace/span ID.
    pub sweep_id: String,
    /// The sweep's recorded start timestamp (RFC3339), from the manifest.
    pub started_at_utc: String,
    /// Root sweep span data.
    pub sweep_span: SweepSpanData,
    /// Per-instance span data (each its own trace, linked to the sweep span).
    pub instance_spans: Vec<InstanceSpanData>,
}

/// Outcome of an export, returned for both real and dry-run exports.
#[derive(Debug, Clone)]
pub struct ExportOtlpSummary {
    /// Sweep directory path as supplied on the command line.
    pub sweep: String,
    /// Stable sweep-level ID used to seed the trace/span IDs.
    pub sweep_id: String,
    /// Fully-resolved OTLP traces endpoint URL. `None` only in a dry-run when
    /// no endpoint resolved (a real export requires one).
    pub endpoint: Option<String>,
    /// Whether this was a dry-run (no socket opened).
    pub dry_run: bool,
    /// Number of instances whose spans were reconstructed.
    pub instance_count: u64,
    /// Total spans (sweep root + per-instance trees).
    pub span_count: u64,
}

fn invalid(msg: String) -> Error {
    Error::Config(ConfigError::Invalid(msg))
}

/// Parse an RFC3339 timestamp into Unix nanoseconds, or `None` when absent /
/// unparseable / pre-epoch.
fn rfc3339_to_nanos(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().and_then(|dt| {
        let secs = dt.timestamp();
        if secs < 0 {
            return None;
        }
        u64::try_from(secs)
            .ok()
            .map(|s| s * 1_000_000_000 + u64::from(dt.timestamp_subsec_nanos()))
    })
}

fn now_unix_nanos() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

/// Total spans in an export: sweep root + per-instance (instance + children).
#[must_use]
pub fn count_spans(instances: &[InstanceSpanData]) -> u64 {
    1 + instances
        .iter()
        .map(|i| 1 + i.model_calls.len() as u64 + i.tool_calls.len() as u64)
        .sum::<u64>()
}

/// Recover the configured `--rerun`/`--samples` count from the manifest argv so
/// we read the same (terminal) run slot the live exporter exported. Defaults to
/// 1 when the flag is absent.
fn reruns_from_manifest(manifest: &ProvenanceManifest) -> u32 {
    let argv = &manifest.cli.argv;
    for (idx, arg) in argv.iter().enumerate() {
        for key in ["--rerun", "--samples"] {
            if let Some(value) = arg.strip_prefix(&format!("{key}=")) {
                if let Ok(n) = value.parse::<u32>() {
                    return n.max(1);
                }
            }
            if arg == key && idx + 1 < argv.len() {
                if let Ok(n) = argv[idx + 1].parse::<u32>() {
                    return n.max(1);
                }
            }
        }
    }
    1
}

/// Reconstruct the full span tree for a completed sweep directory.
///
/// Errors map to `usage_error` (exit 2): a missing directory, a missing /
/// corrupt `results.json`, or a results file with no provenance manifest (the
/// manifest carries the `started_at_utc` timestamp required to recompute the
/// deterministic sweep ID).
pub fn reconstruct(sweep_dir: &Path) -> Result<Reconstructed, Error> {
    if !sweep_dir.exists() {
        return Err(invalid(format!(
            "export-otlp: sweep directory does not exist: {}",
            sweep_dir.display()
        )));
    }
    let results_path = sweep_dir.join("results.json");
    if !results_path.exists() {
        return Err(invalid(format!(
            "export-otlp: no results.json under {}; this command requires a \
             completed sweep produced by `bench swebench`",
            sweep_dir.display()
        )));
    }
    let text = std::fs::read_to_string(&results_path).map_err(|e| {
        invalid(format!(
            "export-otlp: could not read {}: {e}",
            results_path.display()
        ))
    })?;
    let sweep: SweepResults = serde_json::from_str(&text).map_err(|e| {
        invalid(format!(
            "export-otlp: {} is not a valid sweep results file: {e}",
            results_path.display()
        ))
    })?;

    let manifest = sweep.manifest.as_ref().ok_or_else(|| {
        invalid(format!(
            "export-otlp: no provenance manifest in {}; cannot recompute the \
             deterministic sweep ID (run produced by an older harness?)",
            results_path.display()
        ))
    })?;

    let started_at_utc = manifest.runtime.started_at_utc.clone();
    let sweep_id = compute_sweep_id(sweep_dir, &started_at_utc);
    let sweep_trace_id = new_trace_id("sweep", &sweep_id);
    let sweep_span_id = new_span_id("sweep_span", &sweep_trace_id);

    let sweep_start_nanos = rfc3339_to_nanos(&started_at_utc).unwrap_or_else(now_unix_nanos);
    let sweep_end_nanos = manifest
        .runtime
        .finished_at_utc
        .as_deref()
        .and_then(rfc3339_to_nanos)
        .unwrap_or(sweep_start_nanos);

    let resolved_count = sweep
        .instances
        .iter()
        .filter(|r| r.resolved_count > 0)
        .count() as u64;

    let sweep_span = SweepSpanData {
        sweep_id: sweep_id.clone(),
        dataset: manifest.dataset.path.clone(),
        model: manifest.model.name.clone(),
        instance_count: sweep.total as u64,
        resolved_count,
        total_cost_usd: sweep.actual_cost_usd.unwrap_or(sweep.estimated_cost_usd),
        harness_version: manifest.harness.version.clone(),
        git_sha: manifest.harness.git_sha.clone(),
        start_nanos: sweep_start_nanos,
        end_nanos: sweep_end_nanos,
    };

    let last_run = reruns_from_manifest(manifest);
    let mut instance_spans: Vec<InstanceSpanData> = Vec::with_capacity(sweep.instances.len());
    for ir in &sweep.instances {
        let span = reconstruct_instance(
            sweep_dir,
            &sweep_id,
            &sweep_span_id,
            last_run,
            sweep_start_nanos,
            sweep_end_nanos,
            ir,
        );
        instance_spans.push(span);
    }

    Ok(Reconstructed {
        sweep_id,
        started_at_utc,
        sweep_span,
        instance_spans,
    })
}

/// Build the span data for a single instance, preferring the persisted
/// trajectory (full model/tool child spans) and falling back to a minimal
/// instance-only span when no trajectory is on disk.
fn reconstruct_instance(
    sweep_dir: &Path,
    sweep_id: &str,
    sweep_span_id: &crate::ids::SpanId,
    last_run: u32,
    sweep_start_nanos: u64,
    sweep_end_nanos: u64,
    ir: &InstanceResult,
) -> InstanceSpanData {
    // Reuse the persisted trace ID when present (it is authoritative for the
    // original run); otherwise recompute it exactly as the live exporter would.
    let trace_id = ir
        .trace_id
        .clone()
        .unwrap_or_else(|| new_trace_id(&ir.instance_id, sweep_id));

    let repo =
        crate::run::evaluate::parse_repo_from_instance_id(&ir.instance_id).unwrap_or_default();

    let traj_path = existing_trajectory_path_for_run(sweep_dir, &ir.instance_id, last_run);
    let final_patch_bytes = existing_patch_path_for_run(sweep_dir, &ir.instance_id, last_run)
        .metadata()
        .map_or(0, |m| m.len());

    let traj = std::fs::read_to_string(&traj_path)
        .ok()
        .and_then(|json| serde_json::from_str::<Trajectory>(&json).ok());

    if let Some(traj) = traj {
        let mut span = instance_span_data_from_trajectory(
            &trace_id,
            sweep_span_id,
            &ir.instance_id,
            &repo,
            &traj,
            final_patch_bytes,
            sweep_start_nanos,
        );
        // Prefer the authoritative result outcome (the final InstanceResult may
        // differ from the trajectory outcome after post-processing).
        if let Some(outcome) = ir.outcome.as_deref() {
            outcome.clone_into(&mut span.outcome);
        }
        span
    } else {
        // Mirror the live exporter's no-trajectory shape: budget-halted rows
        // never ran, so give them an instantaneous span (collapsed at the sweep
        // finish) instead of one spanning the whole sweep — otherwise they
        // appear as long-running failures and distort duration dashboards.
        let (inst_start, inst_end) = if ir.exit_reason == EXIT_REASON_BUDGET_HALT {
            (sweep_end_nanos, sweep_end_nanos)
        } else {
            (sweep_start_nanos, sweep_end_nanos)
        };
        instance_span_data_from_result(
            &trace_id,
            sweep_span_id,
            &ir.instance_id,
            &repo,
            ir,
            final_patch_bytes,
            inst_start,
            inst_end,
        )
    }
}

/// Reconstruct spans from `args.sweep_dir` and export them to the resolved OTLP
/// collector (unless `--dry-run`).
///
/// Exit-code contract (via [`crate::exit_code::ExitCode::from_error`]):
/// * `usage_error` (2) — missing/invalid sweep directory, or no endpoint
///   resolved for a real export.
/// * `preflight_failure` (3) — the collector was unreachable or rejected the
///   export.
/// * `success` (0) — the export (or dry-run) completed.
pub async fn run(args: &ExportOtlpArgs) -> Result<ExportOtlpSummary, Error> {
    let rec = reconstruct(&args.sweep_dir)?;
    let instance_count = rec.instance_spans.len() as u64;
    let span_count = count_spans(&rec.instance_spans);

    let resolved = telemetry::resolve_endpoint(args.otlp_endpoint.as_deref());

    if args.dry_run {
        return Ok(ExportOtlpSummary {
            sweep: args.sweep_dir.display().to_string(),
            sweep_id: rec.sweep_id,
            endpoint: resolved,
            dry_run: true,
            instance_count,
            span_count,
        });
    }

    let endpoint = resolved.ok_or_else(|| {
        invalid(
            "export-otlp: no OTLP endpoint resolved; pass --otlp-endpoint or set \
             OTEL_EXPORTER_OTLP_TRACES_ENDPOINT / OTEL_EXPORTER_OTLP_ENDPOINT"
                .to_owned(),
        )
    })?;

    let dropped = Arc::new(AtomicU64::new(0));
    let tracer = Tracer::new(endpoint.clone(), dropped.clone());
    tracer
        .export_sweep(&rec.sweep_span, &rec.instance_spans)
        .await;

    let dropped_n = dropped.load(Ordering::Relaxed);
    if dropped_n > 0 {
        return Err(Error::Preflight(format!(
            "export-otlp: collector at {endpoint} was unreachable or rejected the \
             export ({dropped_n} of {span_count} spans dropped)"
        )));
    }

    Ok(ExportOtlpSummary {
        sweep: args.sweep_dir.display().to_string(),
        sweep_id: rec.sweep_id,
        endpoint: Some(endpoint),
        dry_run: false,
        instance_count,
        span_count,
    })
}

/// Render a human-readable one-paragraph summary of an export.
#[must_use]
pub fn render_text(summary: &ExportOtlpSummary) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let verb = if summary.dry_run {
        "Would export (dry-run)"
    } else {
        "Exported"
    };
    let _ = writeln!(
        out,
        "{verb} {} spans across {} instances from sweep {}",
        summary.span_count, summary.instance_count, summary.sweep
    );
    let _ = writeln!(out, "  sweep_id: {}", summary.sweep_id);
    match &summary.endpoint {
        Some(ep) => {
            let _ = writeln!(out, "  endpoint: {ep}");
        }
        None => {
            let _ = writeln!(out, "  endpoint: (none resolved)");
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn count_spans_counts_root_plus_trees() {
        // No instances → just the sweep root span.
        assert_eq!(count_spans(&[]), 1);
    }

    #[test]
    fn rfc3339_to_nanos_parses_and_rejects() {
        assert!(rfc3339_to_nanos("2026-05-01T00:00:00Z").is_some());
        assert!(rfc3339_to_nanos("not-a-date").is_none());
    }

    #[test]
    fn reconstruct_missing_dir_is_config_error() {
        let Err(err) = reconstruct(Path::new("/no/such/sweep/dir/xyz")) else {
            panic!("expected an error for a missing sweep directory");
        };
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn render_text_marks_dry_run() {
        let s = ExportOtlpSummary {
            sweep: "/tmp/s".into(),
            sweep_id: "abc".into(),
            endpoint: None,
            dry_run: true,
            instance_count: 3,
            span_count: 7,
        };
        let text = render_text(&s);
        assert!(text.contains("dry-run"));
        assert!(text.contains('7'));
        assert!(text.contains('3'));
    }
}
