use crate::error::Error;
use crate::run::compare::load_sweep;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPressureReport {
    pub sweep: PathBuf,
    pub generated_at: DateTime<Utc>,
    pub total_runs: usize,
    pub runs_with_elision: usize,
    pub pct_runs_with_elision: f64,
    pub compaction_failures: usize,
    pub pct_compaction_failures: f64,

    // Percentile summary of bytes elided across all runs
    pub bytes_elided_p50: u64,
    pub bytes_elided_p90: u64,
    pub bytes_elided_p95: u64,
    pub bytes_elided_p99: u64,

    pub instances: Vec<InstancePressureRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstancePressureRow {
    pub instance_id: String,
    pub exit_reason: String,
    pub elision_trigger_count: u32,
    pub observations_elided: u32,
    pub bytes_elided: u64,
    pub peak_projected_tokens: u64,
    pub token_ceiling: u64,
    pub compaction_failed: bool,
}

pub struct ContextPressureArgs {
    pub sweep_dir: PathBuf,
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn calculate_percentile(sorted_values: &[u64], percentile: f64) -> u64 {
    if sorted_values.is_empty() {
        return 0;
    }
    let len = sorted_values.len();
    if len == 1 {
        return sorted_values[0];
    }
    // Linear interpolation
    let idx = (percentile / 100.0) * (len - 1) as f64;
    let low = idx.floor() as usize;
    let high = idx.ceil() as usize;
    if low == high {
        sorted_values[low]
    } else {
        let val_low = sorted_values[low] as f64;
        let val_high = sorted_values[high] as f64;
        let fract = idx - low as f64;
        (val_low + fract * (val_high - val_low)).round() as u64
    }
}

#[allow(clippy::cast_precision_loss)]
pub fn run(args: &ContextPressureArgs) -> Result<ContextPressureReport, Error> {
    let sweep = load_sweep(&args.sweep_dir)?;

    let mut total_runs = 0;
    let mut runs_with_elision = 0;
    let mut compaction_failures = 0;
    let mut bytes_elided_list = Vec::new();
    let mut instances = Vec::new();

    for (id, result) in &sweep.instances {
        total_runs += 1;
        let cp = &result.context_pressure;
        if cp.elision_trigger_count > 0 {
            runs_with_elision += 1;
        }
        if cp.compaction_failed {
            compaction_failures += 1;
        }
        bytes_elided_list.push(cp.bytes_elided);

        instances.push(InstancePressureRow {
            instance_id: id.clone(),
            exit_reason: result.exit_reason.clone(),
            elision_trigger_count: cp.elision_trigger_count,
            observations_elided: cp.observations_elided,
            bytes_elided: cp.bytes_elided,
            peak_projected_tokens: cp.peak_projected_tokens,
            token_ceiling: cp.token_ceiling,
            compaction_failed: cp.compaction_failed,
        });
    }

    // Sort instances by id for deterministic order
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    // Sort bytes list for percentiles
    bytes_elided_list.sort_unstable();

    let bytes_elided_p50 = calculate_percentile(&bytes_elided_list, 50.0);
    let bytes_elided_p90 = calculate_percentile(&bytes_elided_list, 90.0);
    let bytes_elided_p95 = calculate_percentile(&bytes_elided_list, 95.0);
    let bytes_elided_p99 = calculate_percentile(&bytes_elided_list, 99.0);

    let pct_runs_with_elision = if total_runs > 0 {
        (runs_with_elision as f64 / total_runs as f64) * 100.0
    } else {
        0.0
    };

    let pct_compaction_failures = if total_runs > 0 {
        (compaction_failures as f64 / total_runs as f64) * 100.0
    } else {
        0.0
    };

    let report = ContextPressureReport {
        sweep: args.sweep_dir.clone(),
        generated_at: Utc::now(),
        total_runs,
        runs_with_elision,
        pct_runs_with_elision,
        compaction_failures,
        pct_compaction_failures,
        bytes_elided_p50,
        bytes_elided_p90,
        bytes_elided_p95,
        bytes_elided_p99,
        instances,
    };

    // Write context-pressure.json to the sweep directory
    let report_path = args.sweep_dir.join("context-pressure.json");
    let file = std::fs::File::create(&report_path)?;
    crate::artifact::to_writer_pretty(
        file,
        crate::artifact::ArtifactKind::ContextPressureReport,
        &report,
    )
    .map_err(|e| Error::Trajectory(e.to_string()))?;

    Ok(report)
}

pub fn render_text(report: &ContextPressureReport) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "\n=== bench context-pressure ===");
    let _ = writeln!(out, "Sweep: {}", report.sweep.display());
    let _ = writeln!(out, "Generated at: {}", report.generated_at.to_rfc3339());
    let _ = writeln!(out);
    let _ = writeln!(out, "Total Runs:          {}", report.total_runs);
    let _ = writeln!(
        out,
        "Runs with Elision:   {} ({:.2}%)",
        report.runs_with_elision, report.pct_runs_with_elision
    );
    let _ = writeln!(
        out,
        "Compaction Failures: {} ({:.2}%)",
        report.compaction_failures, report.pct_compaction_failures
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Bytes Elided Percentiles:");
    let _ = writeln!(out, "  p50: {} bytes", report.bytes_elided_p50);
    let _ = writeln!(out, "  p90: {} bytes", report.bytes_elided_p90);
    let _ = writeln!(out, "  p95: {} bytes", report.bytes_elided_p95);
    let _ = writeln!(out, "  p99: {} bytes", report.bytes_elided_p99);
    let _ = writeln!(out);
    let _ = writeln!(out, "Instance Details:");

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "Instance ID",
            "Exit Reason",
            "Triggers",
            "Obs Elided",
            "Bytes Elided",
            "Peak Projected",
            "Ceiling",
            "Failed",
        ]);

    for row in &report.instances {
        table.add_row(vec![
            row.instance_id.as_str(),
            row.exit_reason.as_str(),
            &row.elision_trigger_count.to_string(),
            &row.observations_elided.to_string(),
            &row.bytes_elided.to_string(),
            &row.peak_projected_tokens.to_string(),
            &row.token_ceiling.to_string(),
            if row.compaction_failed { "YES" } else { "no" },
        ]);
    }

    let _ = writeln!(out, "{table}");
    out
}
