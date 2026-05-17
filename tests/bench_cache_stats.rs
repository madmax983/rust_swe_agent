//! `bench cache-stats`: surface prompt-cache hit rate per sweep.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

// ── unit tests for pure calculation functions ─────────────────────────────────

use rust_swe_agent::run::cache_stats::{
    compute_cache_hit_rate, compute_estimated_savings_usd_vs_cold, compute_realized_cache_spend_usd,
};

#[test]
fn cache_hit_rate_zero_tokens_returns_zero() {
    assert_eq!(compute_cache_hit_rate(0, 0, 0), 0.0);
}

#[test]
fn cache_hit_rate_all_reads() {
    // 100 % of prompt tokens are cache reads
    let rate = compute_cache_hit_rate(0, 100_000, 0);
    assert!((rate - 1.0).abs() < 1e-9, "expected 1.0, got {rate}");
}

#[test]
fn cache_hit_rate_no_reads() {
    // no cache reads at all
    let rate = compute_cache_hit_rate(50_000, 0, 50_000);
    assert_eq!(rate, 0.0);
}

#[test]
fn cache_hit_rate_mixed() {
    // 80 000 reads / (10 000 input + 80 000 reads + 10 000 creation) = 80/100
    let rate = compute_cache_hit_rate(10_000, 80_000, 10_000);
    assert!((rate - 0.8).abs() < 1e-9, "expected 0.8, got {rate}");
}

#[test]
fn estimated_savings_zero_reads_is_zero() {
    assert_eq!(compute_estimated_savings_usd_vs_cold(0), 0.0);
}

#[test]
fn estimated_savings_one_million_reads() {
    // 1 M reads × $3/M × (1 − 0.10) = $2.70
    let savings = compute_estimated_savings_usd_vs_cold(1_000_000);
    assert!(
        (savings - 2.70).abs() < 1e-9,
        "expected 2.70, got {savings}"
    );
}

#[test]
fn realized_spend_zero_tokens_is_zero() {
    assert_eq!(compute_realized_cache_spend_usd(0, 0), 0.0);
}

#[test]
fn realized_spend_reads_only() {
    // 1 M reads × $3/M × 0.10 = $0.30
    let spend = compute_realized_cache_spend_usd(1_000_000, 0);
    assert!((spend - 0.30).abs() < 1e-9, "expected 0.30, got {spend}");
}

#[test]
fn realized_spend_creation_only() {
    // 1 M creation × $3/M × 1.25 = $3.75
    let spend = compute_realized_cache_spend_usd(0, 1_000_000);
    assert!((spend - 3.75).abs() < 1e-9, "expected 3.75, got {spend}");
}

#[test]
fn realized_spend_mixed() {
    // reads: 1M × $3/M × 0.10 = $0.30 + creation: 1M × $3/M × 1.25 = $3.75 = $4.05
    let spend = compute_realized_cache_spend_usd(1_000_000, 1_000_000);
    assert!((spend - 4.05).abs() < 1e-9, "expected 4.05, got {spend}");
}

// ── CLI integration tests ─────────────────────────────────────────────────────

fn cache_stats_sweep_fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cache_stats/sweep")
}

fn no_cache_fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cache_stats/no_cache")
}

fn baseline_fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cache_stats/baseline")
}

fn run_cache_stats(sweep: &Path, extra_args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.args(["--log", "error", "bench", "cache-stats", "--sweep"])
        .arg(sweep);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.output().unwrap()
}

fn run_cache_stats_json(sweep: &Path) -> serde_json::Value {
    let output = run_cache_stats(sweep, &["--format", "json"]);
    assert!(
        output.status.success(),
        "bench cache-stats --format json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_exits_zero_with_cache_data() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let output = run_cache_stats(sweep.path(), &[]);
    assert!(
        output.status.success(),
        "bench cache-stats failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_text_output_shows_bench_header() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let output = run_cache_stats(sweep.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("bench cache-stats"),
        "expected 'bench cache-stats' header in:\n{stdout}"
    );
}

#[test]
fn cli_text_output_shows_cache_hit_rate() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let output = run_cache_stats(sweep.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("cache_hit_rate") || stdout.contains("hit_rate"),
        "expected hit rate label in:\n{stdout}"
    );
}

#[test]
fn cli_json_emits_versioned_artifact_kind() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let report = run_cache_stats_json(sweep.path());
    assert_eq!(
        report["artifact_kind"],
        serde_json::json!("cache_stats_report"),
        "wrong artifact_kind"
    );
    assert!(
        report["schema_version"].is_object(),
        "schema_version must be present"
    );
    assert_eq!(report["schema_version"]["major"], 1);
}

#[test]
fn cli_json_has_required_sweep_total_fields() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let report = run_cache_stats_json(sweep.path());
    let totals = &report["sweep_totals"];
    assert!(totals.is_object(), "sweep_totals required");
    assert!(
        totals["total_input_tokens"].is_u64(),
        "total_input_tokens required"
    );
    assert!(
        totals["total_cache_read_tokens"].is_u64(),
        "total_cache_read_tokens required"
    );
    assert!(
        totals["total_cache_creation_tokens"].is_u64(),
        "total_cache_creation_tokens required"
    );
    assert!(
        totals["cache_hit_rate"].is_f64() || totals["cache_hit_rate"].is_u64(),
        "cache_hit_rate required"
    );
    assert!(
        totals["estimated_savings_usd_vs_cold"].is_f64()
            || totals["estimated_savings_usd_vs_cold"].is_u64(),
        "estimated_savings_usd_vs_cold required"
    );
    assert!(
        totals["realized_cache_spend_usd"].is_f64() || totals["realized_cache_spend_usd"].is_u64(),
        "realized_cache_spend_usd required"
    );
}

#[test]
fn cli_json_sweep_totals_correct_values() {
    // sweep fixture: input=80000, reads=120000, creation=80000
    // hit_rate = 120000 / 280000 ≈ 0.4286
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let report = run_cache_stats_json(sweep.path());
    let totals = &report["sweep_totals"];
    assert_eq!(totals["total_input_tokens"].as_u64().unwrap(), 80_000);
    assert_eq!(totals["total_cache_read_tokens"].as_u64().unwrap(), 120_000);
    assert_eq!(
        totals["total_cache_creation_tokens"].as_u64().unwrap(),
        80_000
    );
    let hit_rate = totals["cache_hit_rate"].as_f64().unwrap();
    let expected_rate = 120_000.0_f64 / 280_000.0;
    assert!(
        (hit_rate - expected_rate).abs() < 1e-6,
        "expected ~{expected_rate:.4}, got {hit_rate:.4}"
    );
}

#[test]
fn cli_json_instances_sorted_worst_first() {
    // cold-1 has 0% hit rate, partial-1 has 50%, warm-1 has 80%
    // sorted ascending → cold-1 first
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let report = run_cache_stats_json(sweep.path());
    let instances = report["instances"].as_array().unwrap();
    assert!(!instances.is_empty(), "instances must not be empty");
    // Verify ascending order by cache_hit_rate
    for i in 1..instances.len() {
        let prev = instances[i - 1]["cache_hit_rate"].as_f64().unwrap();
        let curr = instances[i]["cache_hit_rate"].as_f64().unwrap();
        assert!(
            prev <= curr,
            "instances should be sorted ascending by cache_hit_rate: {prev} before {curr}"
        );
    }
    // First entry should be cold-1 (0% hit rate)
    assert_eq!(
        instances[0]["instance_id"],
        serde_json::json!("cold-1"),
        "worst-cache instance should be first"
    );
}

#[test]
fn cli_json_instance_row_has_required_fields() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let report = run_cache_stats_json(sweep.path());
    let instances = report["instances"].as_array().unwrap();
    let row = &instances[0];
    assert!(row["instance_id"].is_string(), "instance_id required");
    assert!(
        row["total_input_tokens"].is_u64(),
        "total_input_tokens required"
    );
    assert!(
        row["total_cache_read_tokens"].is_u64(),
        "total_cache_read_tokens required"
    );
    assert!(
        row["total_cache_creation_tokens"].is_u64(),
        "total_cache_creation_tokens required"
    );
    assert!(
        row["cache_hit_rate"].is_f64() || row["cache_hit_rate"].is_u64(),
        "cache_hit_rate required"
    );
    assert!(
        row["estimated_savings_usd_vs_cold"].is_f64()
            || row["estimated_savings_usd_vs_cold"].is_u64(),
        "estimated_savings_usd_vs_cold required"
    );
    assert!(
        row["realized_cache_spend_usd"].is_f64() || row["realized_cache_spend_usd"].is_u64(),
        "realized_cache_spend_usd required"
    );
}

#[test]
fn cli_top_flag_limits_text_display() {
    // --top only clips the rendered text table; JSON artifact keeps all instances.
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    // Text output with --top 1: only the single worst instance row should appear.
    let output = run_cache_stats(sweep.path(), &["--top", "1"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("top 1"),
        "text output should reflect --top 1 limit:\n{stdout}"
    );

    // JSON artifact on disk should contain all 3 instances regardless of --top.
    let artifact: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(sweep.path().join("cache-stats.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        artifact["instances"].as_array().unwrap().len(),
        3,
        "JSON artifact must contain all instances regardless of --top"
    );
}

#[test]
fn cli_json_format_includes_all_instances_regardless_of_top() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let output = run_cache_stats(sweep.path(), &["--format", "json", "--top", "1"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // JSON output (stdout) always includes full instance list; --top is a text-rendering flag.
    assert_eq!(
        report["instances"].as_array().unwrap().len(),
        3,
        "JSON stdout should include all instances even with --top 1"
    );
}

#[test]
fn cli_writes_cache_stats_json_to_disk() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());

    let output = run_cache_stats(sweep.path(), &[]);
    assert!(output.status.success());

    let artifact_path = sweep.path().join("cache-stats.json");
    assert!(
        artifact_path.exists(),
        "cache-stats.json should be written to sweep dir"
    );

    let content = std::fs::read_to_string(&artifact_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(v["artifact_kind"], serde_json::json!("cache_stats_report"));
}

#[test]
fn cli_cache_disabled_exits_zero_and_prints_message() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&no_cache_fixture(), sweep.path());

    let output = run_cache_stats(sweep.path(), &[]);
    assert!(
        output.status.success(),
        "bench cache-stats on no-cache sweep should exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("cache disabled") || stdout.contains("unsupported"),
        "expected informational message for no-cache sweep, got:\n{stdout}"
    );
}

#[test]
fn cli_json_cache_disabled_sets_flag() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&no_cache_fixture(), sweep.path());

    let output = run_cache_stats(sweep.path(), &["--format", "json"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["cache_disabled"],
        serde_json::json!(true),
        "cache_disabled should be true when no cache tokens"
    );
}

#[test]
fn cli_baseline_json_includes_delta_fields() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());
    let baseline_dir = tempfile::tempdir().unwrap();
    copy_dir(&baseline_fixture(), baseline_dir.path());

    let output = run_cache_stats(
        sweep.path(),
        &[
            "--format",
            "json",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "bench cache-stats --baseline failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let baseline = &report["baseline"];
    assert!(baseline.is_object(), "baseline delta object required");
    assert!(
        baseline["delta_hit_rate"].is_f64() || baseline["delta_hit_rate"].is_u64(),
        "delta_hit_rate required"
    );
    assert!(
        baseline["delta_realized_spend_usd"].is_f64()
            || baseline["delta_realized_spend_usd"].is_u64(),
        "delta_realized_spend_usd required"
    );
}

#[test]
fn cli_baseline_text_shows_delta_column() {
    let sweep = tempfile::tempdir().unwrap();
    copy_dir(&cache_stats_sweep_fixture(), sweep.path());
    let baseline_dir = tempfile::tempdir().unwrap();
    copy_dir(&baseline_fixture(), baseline_dir.path());

    let output = run_cache_stats(
        sweep.path(),
        &["--baseline", baseline_dir.path().to_str().unwrap()],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("delta") || stdout.contains("Δ") || stdout.contains("baseline"),
        "expected delta info in text output with --baseline, got:\n{stdout}"
    );
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}
