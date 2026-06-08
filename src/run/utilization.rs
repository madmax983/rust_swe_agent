//! `bench utilization`: report how efficiently a sweep used its configured concurrency.
//!
//! Read-only and zero-cost: reads only on-disk artifacts (the sweep manifest +
//! per-instance results), performs no model calls, starts no containers, and
//! never modifies the input sweep.
//!
//! The report joins three values the harness already persists:
//!   * Σ per-instance `duration_secs` — the total CPU-seconds of agent work,
//!   * the sweep wallclock (`finished_at_utc` − `started_at_utc`),
//!   * the configured worker count (`--parallel`, recovered from the manifest).
//!
//! From these it derives **effective parallelism** (work ÷ wallclock), the
//! **utilization %** against the configured workers, and the **idle waste** —
//! the wallclock gap between what was observed and the theoretical minimum at
//! full utilization.
//!
//! See `docs/spec-utilization.md` for the schema and algorithm specification.

#![allow(clippy::cast_precision_loss)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error};
use crate::run::compare::load_sweep;
use crate::run::swebench::{DEFAULT_PARALLEL, InstanceResult, ProvenanceManifest};

/// Stable schema version for the JSON artifact. Bump only on breaking changes.
pub const SCHEMA_VERSION: u32 = 1;

/// Inputs to [`compute`].
#[derive(Debug, Clone)]
pub struct UtilizationArgs {
    /// Completed sweep directory produced by `bench swebench`.
    pub sweep_dir: PathBuf,
    /// Optional CI gate: minimum acceptable utilization percentage (0–100).
    /// When `Some`, the report records whether the floor was met; the CLI maps
    /// an unmet floor to a non-zero exit code.
    pub min_utilization: Option<f64>,
}

/// The full utilization report. Field order is part of the stable JSON schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtilizationReport {
    /// Schema version of this artifact (see [`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Sweep directory path as supplied on the command line.
    pub sweep: String,
    /// RFC3339 timestamp at which this report was generated.
    pub generated_at: String,

    /// Configured worker count recovered from the manifest.
    pub configured_workers: usize,
    /// Provenance of `configured_workers` (which manifest field sourced it).
    pub configured_workers_source: String,

    /// Total instances counted in the sum of durations.
    pub total_instances: usize,
    /// Instances whose `duration_secs` was present and contributed to the sum.
    pub instances_with_duration: usize,
    /// Instances missing `duration_secs` (excluded from the sum).
    pub instances_missing_duration: usize,

    /// Σ per-instance `duration_secs`, in seconds.
    pub sum_instance_duration_secs: f64,
    /// Observed sweep wallclock (`finished_at_utc` − `started_at_utc`), seconds.
    pub wallclock_secs: f64,

    /// Σ duration ÷ wallclock. "How many workers' worth of work actually ran."
    pub effective_parallelism: f64,
    /// `effective_parallelism` ÷ `configured_workers`, as a percentage.
    pub utilization_pct: f64,

    /// Theoretical-minimum wallclock at full utilization (Σ duration ÷ workers).
    pub theoretical_min_wallclock_secs: f64,
    /// Wallclock-seconds gap between observed and theoretical-minimum wallclock.
    pub idle_waste_secs: f64,
    /// `idle_waste_secs` as a percentage of observed wallclock.
    pub idle_waste_pct: f64,

    /// `true` when the sweep contains retried / multi-sample instances, whose
    /// `duration_secs` reflects only the terminal attempt. The utilization
    /// figure is then an approximation; see the note field. Exact
    /// reconstruction is intentionally out of scope.
    pub retry_merged: bool,
    /// Human-readable caveat present only when `retry_merged` is `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_merged_note: Option<String>,

    /// The `--min-utilization` floor, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_utilization: Option<f64>,
    /// Whether `utilization_pct` met `min_utilization`. `None` when no floor set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_utilization_met: Option<bool>,
}

/// Compute the utilization report for a completed sweep directory.
///
/// Errors (each mapped to a non-zero CLI exit) when the sweep lacks the inputs
/// required for an honest measurement: a missing manifest, absent or
/// unparseable start/finish timestamps, a non-positive wallclock, or no
/// instance carrying a `duration_secs`.
pub fn compute(args: &UtilizationArgs) -> Result<UtilizationReport, Error> {
    let loaded = load_sweep(&args.sweep_dir)?;

    let manifest = loaded.manifest.as_ref().ok_or_else(|| {
        invalid(format!(
            "utilization: no manifest found in {}; this command requires a \
             completed sweep produced by `bench swebench`",
            args.sweep_dir.display()
        ))
    })?;

    let wallclock_secs = sweep_wallclock_secs(manifest)?;

    let configured_workers = parallel_from_manifest(manifest).max(1);
    let configured_workers_source = parallel_source(manifest);

    let mut instances: Vec<&InstanceResult> = loaded.instances.values().collect();
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    let total_instances = instances.len();

    if total_instances == 0 {
        return Err(invalid(
            "utilization: sweep contains no instances to measure".to_owned(),
        ));
    }

    let mut sum_instance_duration_secs = 0.0_f64;
    let mut instances_with_duration = 0usize;
    for inst in &instances {
        if let Some(d) = inst.duration_secs {
            // Negative or NaN durations are corrupt; treat as missing so they
            // neither inflate nor poison the sum.
            if d.is_finite() && d >= 0.0 {
                sum_instance_duration_secs += d;
                instances_with_duration += 1;
            }
        }
    }
    let instances_missing_duration = total_instances - instances_with_duration;

    if instances_with_duration == 0 {
        return Err(invalid(
            "utilization: no instance carries a duration_secs value; cannot \
             compute effective parallelism (legacy sweep or capture failure)"
                .to_owned(),
        ));
    }

    let workers_f = configured_workers as f64;
    let effective_parallelism = sum_instance_duration_secs / wallclock_secs;
    let utilization_pct = (effective_parallelism / workers_f) * 100.0;
    let theoretical_min_wallclock_secs = sum_instance_duration_secs / workers_f;
    let idle_waste_secs = (wallclock_secs - theoretical_min_wallclock_secs).max(0.0);
    let idle_waste_pct = (idle_waste_secs / wallclock_secs) * 100.0;

    let retry_merged = instances.iter().any(|i| {
        i.attempts > 1 || i.runs > 1 || !i.retry_reasons.is_empty()
    });
    let retry_merged_note = retry_merged.then(|| {
        "one or more instances retried or ran multiple samples; duration_secs \
         reflects only the terminal attempt, so effective parallelism is an \
         approximation (exact reconstruction is out of scope)"
            .to_owned()
    });

    let min_utilization_met = args
        .min_utilization
        .map(|floor| utilization_pct >= floor);

    Ok(UtilizationReport {
        schema_version: SCHEMA_VERSION,
        sweep: args.sweep_dir.display().to_string(),
        generated_at: now_rfc3339(),
        configured_workers,
        configured_workers_source,
        total_instances,
        instances_with_duration,
        instances_missing_duration,
        sum_instance_duration_secs,
        wallclock_secs,
        effective_parallelism,
        utilization_pct,
        theoretical_min_wallclock_secs,
        idle_waste_secs,
        idle_waste_pct,
        retry_merged,
        retry_merged_note,
        min_utilization: args.min_utilization,
        min_utilization_met,
    })
}

/// Render a human-readable text report.
#[must_use]
pub fn render_text(report: &UtilizationReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(out, "Sweep utilization: {}", report.sweep);
    let _ = writeln!(
        out,
        "  Configured workers:    {} (from {})",
        report.configured_workers, report.configured_workers_source
    );
    let _ = writeln!(out, "  Instances:             {}", report.total_instances);
    if report.instances_missing_duration > 0 {
        let _ = writeln!(
            out,
            "    (missing duration:   {} — excluded from the sum)",
            report.instances_missing_duration
        );
    }
    let _ = writeln!(
        out,
        "  Σ instance duration:   {:.1}s",
        report.sum_instance_duration_secs
    );
    let _ = writeln!(out, "  Sweep wallclock:       {:.1}s", report.wallclock_secs);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  Effective parallelism: {:.2} of {} configured",
        report.effective_parallelism, report.configured_workers
    );
    let _ = writeln!(
        out,
        "  Utilization:           {:.1}%",
        report.utilization_pct
    );
    let _ = writeln!(
        out,
        "  Theoretical-min wall:  {:.1}s (at full utilization)",
        report.theoretical_min_wallclock_secs
    );
    let _ = writeln!(
        out,
        "  Idle waste:            {:.1}s ({:.1}% of wallclock)",
        report.idle_waste_secs, report.idle_waste_pct
    );

    if report.retry_merged {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  ⚠ retry-merged sweep: {}",
            report
                .retry_merged_note
                .as_deref()
                .unwrap_or("durations reflect only terminal attempts")
        );
    }

    if let Some(floor) = report.min_utilization {
        let verdict = if report.min_utilization_met == Some(true) {
            "PASS"
        } else {
            "FAIL"
        };
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  Gate (--min-utilization {floor:.1}%): {verdict}"
        );
    }

    out
}

// ── helpers ────────────────────────────────────────────────────────────────

fn invalid(msg: String) -> Error {
    Error::Config(ConfigError::Invalid(msg))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Sweep wallclock in seconds, or an error when the timestamps are absent or
/// unparseable (in-progress or legacy manifest), or non-positive.
fn sweep_wallclock_secs(manifest: &ProvenanceManifest) -> Result<f64, Error> {
    let started_raw = &manifest.runtime.started_at_utc;
    let finished_raw = manifest.runtime.finished_at_utc.as_ref().ok_or_else(|| {
        invalid(
            "utilization: manifest is missing runtime.finished_at_utc — the sweep \
             is in progress or was interrupted; cannot measure wallclock"
                .to_owned(),
        )
    })?;

    let started = chrono::DateTime::parse_from_rfc3339(started_raw).map_err(|_| {
        invalid(format!(
            "utilization: manifest runtime.started_at_utc is missing or not a valid \
             RFC3339 timestamp (got {started_raw:?})"
        ))
    })?;
    let finished = chrono::DateTime::parse_from_rfc3339(finished_raw).map_err(|_| {
        invalid(format!(
            "utilization: manifest runtime.finished_at_utc is not a valid RFC3339 \
             timestamp (got {finished_raw:?})"
        ))
    })?;

    let millis = finished.signed_duration_since(started).num_milliseconds();
    if millis <= 0 {
        return Err(invalid(format!(
            "utilization: sweep wallclock is non-positive (started {started_raw}, \
             finished {finished_raw}); cannot compute utilization"
        )));
    }
    Ok(millis as f64 / 1000.0)
}

/// Recover the configured worker count from `manifest.cli.argv`, falling back to
/// the harness default when `--parallel` was not passed explicitly.
fn parallel_from_manifest(manifest: &ProvenanceManifest) -> usize {
    let argv = &manifest.cli.argv;
    for (idx, arg) in argv.iter().enumerate() {
        if let Some(value) = arg.strip_prefix("--parallel=") {
            if let Ok(parallel) = value.parse() {
                return parallel;
            }
        }
        if (arg == "--parallel" || arg == "-p") && idx + 1 < argv.len() {
            if let Ok(parallel) = argv[idx + 1].parse() {
                return parallel;
            }
        }
    }
    DEFAULT_PARALLEL
}

fn parallel_source(manifest: &ProvenanceManifest) -> String {
    let argv = &manifest.cli.argv;
    let explicit = argv.iter().enumerate().any(|(idx, arg)| {
        arg.starts_with("--parallel=")
            || ((arg == "--parallel" || arg == "-p") && idx + 1 < argv.len())
    });
    if explicit {
        "manifest.cli.argv[--parallel]".to_owned()
    } else {
        format!("default ({DEFAULT_PARALLEL})")
    }
}
