//! `bench ladder`: integration tests.
//!
//! Covers the acceptance criteria from issue #270.
//! Tests are written first (TDD red phase) against the binary.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::map_unwrap_or,
    clippy::unnecessary_map_or,
    clippy::redundant_closure_for_method_calls
)]

use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

// ── fixture helpers ───────────────────────────────────────────────────────────

fn fixture_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ladder")
}

fn run_ladder(root: &Path, extra_args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.args(["--log", "error", "bench", "ladder", "--root"])
        .arg(root);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.output().unwrap()
}

fn run_ladder_json(root: &Path, extra_args: &[&str]) -> serde_json::Value {
    let mut args = vec!["--format", "json"];
    args.extend_from_slice(extra_args);
    let output = run_ladder(root, &args);
    assert!(
        output.status.success(),
        "bench ladder --format json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

// ── basic CLI smoke tests ─────────────────────────────────────────────────────

#[test]
fn ladder_subcommand_appears_in_bench_help() {
    let output = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("ladder") || stderr.contains("ladder"),
        "bench --help should list 'ladder'\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn ladder_exits_zero_with_valid_root() {
    let output = run_ladder(&fixture_root(), &[]);
    assert!(
        output.status.success(),
        "bench ladder should exit 0 for valid root\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn ladder_exits_zero_with_zero_matching_sweeps() {
    // Create a temp root with no valid sweeps — exit 0 per AC
    let tmp = tempfile::tempdir().unwrap();
    let output = run_ladder(tmp.path(), &[]);
    assert!(
        output.status.success(),
        "bench ladder should exit 0 even when no sweeps match\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── text format ───────────────────────────────────────────────────────────────

#[test]
fn ladder_text_shows_bench_header() {
    let output = run_ladder(&fixture_root(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("bench ladder"),
        "text output should contain 'bench ladder' header:\n{stdout}"
    );
}

#[test]
fn ladder_text_shows_three_sweep_rows() {
    let output = run_ladder(&fixture_root(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("sweep1"),
        "expected sweep1 row in text output:\n{stdout}"
    );
    assert!(stdout.contains("sweep2"), "expected sweep2 row:\n{stdout}");
    assert!(stdout.contains("sweep3"), "expected sweep3 row:\n{stdout}");
}

#[test]
fn ladder_text_shows_skipped_section() {
    let output = run_ladder(&fixture_root(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("sweep_bad"),
        "skipped section must list sweep_bad:\n{stdout}"
    );
    assert!(
        stdout.contains("Skipped") || stdout.contains("skipped"),
        "output must include 'Skipped' section:\n{stdout}"
    );
}

#[test]
fn ladder_text_shows_resolved_percentages() {
    let output = run_ladder(&fixture_root(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // sweep1 & sweep3: 50%, sweep2: 75%
    assert!(
        stdout.contains("50.00") || stdout.contains("50%"),
        "expected 50% resolved in output:\n{stdout}"
    );
    assert!(
        stdout.contains("75.00") || stdout.contains("75%"),
        "expected 75% resolved in output:\n{stdout}"
    );
}

#[test]
fn ladder_text_shows_delta_column() {
    let output = run_ladder(&fixture_root(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // sweep2 delta vs sweep1 should be +25.00pp
    assert!(
        stdout.contains("+25") || stdout.contains("25.00"),
        "expected positive delta in output:\n{stdout}"
    );
}

#[test]
fn ladder_text_rows_sorted_by_start_timestamp() {
    let output = run_ladder(&fixture_root(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // sweep1 (2026-04-28) must appear before sweep2 (2026-04-29)
    let pos1 = stdout.find("sweep1").expect("sweep1 not found");
    let pos2 = stdout.find("sweep2").expect("sweep2 not found");
    let pos3 = stdout.find("sweep3").expect("sweep3 not found");
    assert!(
        pos1 < pos2 && pos2 < pos3,
        "rows must be sorted by start_timestamp ascending: pos1={pos1} pos2={pos2} pos3={pos3}"
    );
}

// ── JSON format ───────────────────────────────────────────────────────────────

#[test]
fn ladder_json_has_artifact_kind() {
    let report = run_ladder_json(&fixture_root(), &[]);
    assert_eq!(
        report["artifact_kind"],
        serde_json::json!("ladder_report"),
        "artifact_kind must be 'ladder_report'"
    );
}

#[test]
fn ladder_json_has_schema_version() {
    let report = run_ladder_json(&fixture_root(), &[]);
    assert!(
        report["schema_version"].is_object(),
        "schema_version must be present"
    );
    assert_eq!(
        report["schema_version"]["major"],
        serde_json::json!(1),
        "schema_version.major must be 1"
    );
}

#[test]
fn ladder_json_has_three_rows() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "expected 3 valid sweep rows");
}

#[test]
fn ladder_json_rows_sorted_chronologically() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let rows = report["rows"].as_array().unwrap();
    let ids: Vec<&str> = rows
        .iter()
        .map(|r| r["sweep_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sweep1", "sweep2", "sweep3"]);
}

#[test]
fn ladder_json_row_has_required_fields() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let row = &report["rows"][0];
    assert!(row["sweep_id"].is_string(), "sweep_id required");
    assert!(row["date"].is_string(), "date required");
    assert!(row["model"].is_string(), "model required");
    assert!(row["prompt_sha"].is_string(), "prompt_sha required");
    assert!(row["n"].is_u64(), "n required");
    assert!(
        row["resolved_pct"].is_f64() || row["resolved_pct"].is_u64(),
        "resolved_pct required"
    );
}

#[test]
fn ladder_json_resolved_pct_values_correct() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let rows = report["rows"].as_array().unwrap();
    let pct0 = rows[0]["resolved_pct"].as_f64().unwrap();
    let pct1 = rows[1]["resolved_pct"].as_f64().unwrap();
    let pct2 = rows[2]["resolved_pct"].as_f64().unwrap();
    assert!(
        (pct0 - 50.0).abs() < 0.01,
        "sweep1 resolved_pct should be ~50%, got {pct0}"
    );
    assert!(
        (pct1 - 75.0).abs() < 0.01,
        "sweep2 resolved_pct should be ~75%, got {pct1}"
    );
    assert!(
        (pct2 - 50.0).abs() < 0.01,
        "sweep3 resolved_pct should be ~50%, got {pct2}"
    );
}

#[test]
fn ladder_json_delta_resolved_pct_first_row_is_null() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let rows = report["rows"].as_array().unwrap();
    assert!(
        rows[0]
            .get("delta_resolved_pct")
            .map_or(true, |v| v.is_null()),
        "first row delta_resolved_pct must be absent or null"
    );
}

#[test]
fn ladder_json_delta_resolved_pct_second_row_is_positive() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let rows = report["rows"].as_array().unwrap();
    let delta = rows[1]["delta_resolved_pct"].as_f64().unwrap();
    assert!(
        (delta - 25.0).abs() < 0.01,
        "sweep2 delta_resolved_pct should be ~+25.0pp, got {delta}"
    );
}

#[test]
fn ladder_json_has_skipped_section() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "expected 1 skipped sweep");
    let s = &skipped[0];
    assert!(s["dir"].is_string(), "skipped entry must have dir");
    assert!(s["reason"].is_string(), "skipped entry must have reason");
    assert_eq!(
        s["dir"].as_str().unwrap(),
        "sweep_bad",
        "skipped dir should be sweep_bad"
    );
}

#[test]
fn ladder_json_skipped_reason_mentions_mismatch() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let skipped = report["skipped"].as_array().unwrap();
    let reason = skipped[0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("mismatch") || reason.contains("trajectory") || reason.contains("expected"),
        "skipped reason should mention schema mismatch: {reason}"
    );
}

#[test]
fn ladder_json_usd_per_resolved_correct() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let rows = report["rows"].as_array().unwrap();
    // sweep1: $0.10 / 2 resolved = $0.05
    let upr0 = rows[0]["usd_per_resolved"].as_f64().unwrap();
    assert!(
        (upr0 - 0.05).abs() < 0.001,
        "sweep1 usd_per_resolved should be ~$0.05, got {upr0}"
    );
    // sweep2: $0.10 / 3 resolved ≈ $0.0333
    let upr1 = rows[1]["usd_per_resolved"].as_f64().unwrap();
    assert!(
        (upr1 - 0.03333).abs() < 0.001,
        "sweep2 usd_per_resolved should be ~$0.0333, got {upr1}"
    );
}

#[test]
fn ladder_json_mean_steps_correct() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let rows = report["rows"].as_array().unwrap();
    let steps = rows[0]["mean_steps"].as_f64().unwrap();
    assert!(
        (steps - 4.0).abs() < 0.01,
        "mean_steps should be 4.0, got {steps}"
    );
}

#[test]
fn ladder_json_model_and_prompt_sha_present() {
    let report = run_ladder_json(&fixture_root(), &[]);
    let row0 = &report["rows"][0];
    assert_eq!(row0["model"].as_str().unwrap(), "model-a");
    // prompt_sha should be 8 chars (first 8 of aaaa123400000000...)
    assert_eq!(row0["prompt_sha"].as_str().unwrap(), "aaaa1234");
}

// ── --last flag ───────────────────────────────────────────────────────────────

#[test]
fn ladder_last_flag_truncates_to_n_most_recent() {
    let report = run_ladder_json(&fixture_root(), &["--last", "2"]);
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "expected 2 rows with --last 2");
    // Should be the 2 most recent: sweep2 and sweep3
    assert_eq!(rows[0]["sweep_id"].as_str().unwrap(), "sweep2");
    assert_eq!(rows[1]["sweep_id"].as_str().unwrap(), "sweep3");
}

#[test]
fn ladder_last_zero_returns_empty_rows() {
    let report = run_ladder_json(&fixture_root(), &["--last", "0"]);
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 0, "expected 0 rows with --last 0");
}

// ── --dataset filter ──────────────────────────────────────────────────────────

#[test]
fn ladder_dataset_filter_by_alias_keeps_matching_sweeps() {
    let report = run_ladder_json(&fixture_root(), &["--dataset", "lite"]);
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "all 3 valid sweeps use dataset alias 'lite'");
}

#[test]
fn ladder_dataset_filter_by_nonexistent_returns_zero_rows() {
    let output = run_ladder(&fixture_root(), &["--dataset", "nonexistent"]);
    assert!(
        output.status.success(),
        "exit 0 even with 0 matching sweeps"
    );
    let report: serde_json::Value = serde_json::from_slice(
        &run_ladder(
            &fixture_root(),
            &["--dataset", "nonexistent", "--format", "json"],
        )
        .stdout,
    )
    .unwrap();
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 0, "no rows should match a nonexistent dataset");
}

// ── --baseline flag ───────────────────────────────────────────────────────────

#[test]
fn ladder_baseline_adds_delta_vs_baseline_column() {
    let report = run_ladder_json(&fixture_root(), &["--baseline", "sweep1"]);
    let rows = report["rows"].as_array().unwrap();
    // All rows should have delta_vs_baseline set
    for row in rows {
        assert!(
            row.get("delta_vs_baseline")
                .map_or(false, |v| v.is_f64() || v.is_u64()),
            "delta_vs_baseline should be present when --baseline is set: {row}"
        );
    }
}

#[test]
fn ladder_baseline_row_has_delta_zero() {
    let report = run_ladder_json(&fixture_root(), &["--baseline", "sweep1"]);
    let rows = report["rows"].as_array().unwrap();
    // sweep1 is the baseline row — its delta_vs_baseline should be 0.0
    let baseline_row = rows.iter().find(|r| r["sweep_id"] == "sweep1").unwrap();
    let delta = baseline_row["delta_vs_baseline"].as_f64().unwrap();
    assert!(
        delta.abs() < 0.001,
        "baseline row's delta_vs_baseline should be 0.0, got {delta}"
    );
}

#[test]
fn ladder_baseline_delta_computed_correctly() {
    let report = run_ladder_json(&fixture_root(), &["--baseline", "sweep1"]);
    let rows = report["rows"].as_array().unwrap();
    // sweep2 vs sweep1: 75% - 50% = +25.0pp
    let sweep2_row = rows.iter().find(|r| r["sweep_id"] == "sweep2").unwrap();
    let delta = sweep2_row["delta_vs_baseline"].as_f64().unwrap();
    assert!(
        (delta - 25.0).abs() < 0.01,
        "sweep2 delta_vs_baseline should be ~+25pp, got {delta}"
    );
    // sweep3 vs sweep1: 50% - 50% = 0.0pp
    let sweep3_row = rows.iter().find(|r| r["sweep_id"] == "sweep3").unwrap();
    let delta3 = sweep3_row["delta_vs_baseline"].as_f64().unwrap();
    assert!(
        delta3.abs() < 0.01,
        "sweep3 delta_vs_baseline should be ~0pp, got {delta3}"
    );
}

// ── markdown format ───────────────────────────────────────────────────────────

#[test]
fn ladder_markdown_has_header() {
    let output = run_ladder(&fixture_root(), &["--format", "markdown"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("bench ladder"),
        "markdown should contain 'bench ladder' header:\n{stdout}"
    );
}

#[test]
fn ladder_markdown_has_table_rows() {
    let output = run_ladder(&fixture_root(), &["--format", "markdown"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains('|'),
        "markdown should contain table pipe characters:\n{stdout}"
    );
    assert!(
        stdout.contains("sweep1") && stdout.contains("sweep2") && stdout.contains("sweep3"),
        "markdown must contain all 3 sweep rows:\n{stdout}"
    );
}

#[test]
fn ladder_markdown_has_skipped_section() {
    let output = run_ladder(&fixture_root(), &["--format", "markdown"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("sweep_bad"),
        "markdown skipped section must list sweep_bad:\n{stdout}"
    );
}

// ── determinism ───────────────────────────────────────────────────────────────

#[test]
fn ladder_output_is_deterministic() {
    let out1 = run_ladder(&fixture_root(), &["--format", "json"]);
    let out2 = run_ladder(&fixture_root(), &["--format", "json"]);
    assert_eq!(
        out1.stdout, out2.stdout,
        "JSON output should be byte-for-byte identical across two runs"
    );
}

// ── insta snapshot tests ──────────────────────────────────────────────────────

fn normalize_root(s: &str) -> String {
    // Replace the absolute fixture path with a stable placeholder so snapshots
    // are machine-independent and byte-for-byte identical in CI.
    let display = fixture_root().display().to_string();
    let escaped = display.replace('\\', "\\\\");
    s.replace(&escaped, "[LADDER_FIXTURES]")
        .replace(&display, "[LADDER_FIXTURES]")
}

#[test]
fn ladder_snapshot_text_format() {
    let output = run_ladder(&fixture_root(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    insta::assert_snapshot!("ladder_text", normalize_root(&stdout));
}

#[test]
fn ladder_snapshot_json_format() {
    let output = run_ladder(&fixture_root(), &["--format", "json"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    insta::assert_snapshot!("ladder_json", normalize_root(&stdout));
}

#[test]
fn ladder_snapshot_markdown_format() {
    let output = run_ladder(&fixture_root(), &["--format", "markdown"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    insta::assert_snapshot!("ladder_markdown", normalize_root(&stdout));
}
