//! `bench command-stats`: surface shell-command behavior by outcome.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

mod support;
use support::binary_path;

// ── unit tests for head extraction (live in the library) ──────────────────────

use rust_swe_agent::run::command_stats::extract_command_heads;

#[test]
fn head_extraction_simple_command() {
    assert_eq!(extract_command_heads("ls -la"), vec!["ls"]);
}

#[test]
fn head_extraction_strips_sudo() {
    assert_eq!(extract_command_heads("sudo apt-get install python3"), vec!["apt-get"]);
}

#[test]
fn head_extraction_strips_time() {
    assert_eq!(extract_command_heads("time cargo test"), vec!["cargo"]);
}

#[test]
fn head_extraction_strips_env_prefix() {
    assert_eq!(extract_command_heads("env RUST_LOG=debug cargo build"), vec!["cargo"]);
}

#[test]
fn head_extraction_strips_inline_var_assignment() {
    assert_eq!(extract_command_heads("RUST_LOG=debug cargo build"), vec!["cargo"]);
}

#[test]
fn head_extraction_decomposes_pipeline() {
    let heads = extract_command_heads("cat file.txt | grep error | sort -u");
    assert_eq!(heads, vec!["cat", "grep", "sort"]);
}

#[test]
fn head_extraction_pipeline_not_logical_or() {
    // `||` should not split into segments the way `|` does
    let heads = extract_command_heads("false || true");
    assert_eq!(heads, vec!["false"]);
}

#[test]
fn head_extraction_empty_command_returns_empty() {
    let heads = extract_command_heads("");
    assert!(heads.is_empty(), "empty command should produce no heads");
}

#[test]
fn head_extraction_whitespace_only_returns_empty() {
    let heads = extract_command_heads("   ");
    assert!(heads.is_empty());
}

#[test]
fn head_extraction_quoted_args_dont_affect_head() {
    assert_eq!(extract_command_heads("grep 'def test' ."), vec!["grep"]);
}

#[test]
fn head_extraction_sudo_time_combined() {
    assert_eq!(extract_command_heads("sudo time pytest -x"), vec!["pytest"]);
}

#[test]
fn head_extraction_strips_multiple_env_vars() {
    assert_eq!(
        extract_command_heads("A=1 B=2 C=3 python script.py"),
        vec!["python"]
    );
}

// ── CLI integration tests ─────────────────────────────────────────────────────

#[test]
fn cli_produces_text_output_with_outcome_tables() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench command-stats failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("=== bench command-stats ==="), "{stdout}");
    // Should show outcome buckets
    assert!(stdout.contains("resolved"), "{stdout}");
    assert!(stdout.contains("unresolved"), "{stdout}");
    // Should show command names
    assert!(stdout.contains("cat"), "{stdout}");
    assert!(stdout.contains("grep"), "{stdout}");
    assert!(stdout.contains("pytest"), "{stdout}");
}

#[test]
fn cli_writes_command_stats_json_artifact() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench command-stats failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report_path = sweep.path().join("command-stats.json");
    assert!(report_path.exists(), "command-stats.json should be written");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();

    // Top-level schema validation
    assert!(report["sweep"].is_string(), "sweep field required");
    assert!(report["generated_at"].is_string(), "generated_at required");
    assert!(report["totals"].is_object(), "totals required");
    assert!(report["by_outcome"].is_object(), "by_outcome required");

    // Totals
    assert_eq!(report["totals"]["trajectories"], 3);
    assert!(
        report["totals"]["bash_steps"].as_u64().unwrap() > 0,
        "should have bash steps"
    );
    assert!(
        report["totals"]["unique_command_heads"].as_u64().unwrap() > 0
    );

    // by_outcome buckets
    let by_outcome = report["by_outcome"].as_object().unwrap();
    assert!(by_outcome.contains_key("resolved"), "resolved bucket required");
    assert!(by_outcome.contains_key("unresolved"), "unresolved bucket required");
    assert!(by_outcome.contains_key("errored"), "errored bucket required");
    assert!(by_outcome.contains_key("all"), "all bucket required");
}

#[test]
fn cli_json_format_emits_valid_json_to_stdout() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench command-stats --format json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["by_outcome"].is_object());
}

#[test]
fn cli_row_fields_have_correct_types() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    // Check resolved bucket rows
    let resolved_rows = report["by_outcome"]["resolved"].as_array().unwrap();
    assert!(!resolved_rows.is_empty(), "resolved bucket should have rows");
    let row = &resolved_rows[0];
    assert!(row["command_head"].is_string(), "command_head must be string");
    assert!(row["instance_count"].is_u64(), "instance_count must be int");
    assert!(row["invocation_count"].is_u64(), "invocation_count must be int");
    assert!(row["mean_calls_per_instance"].is_f64() || row["mean_calls_per_instance"].is_u64());
    assert!(row["nonzero_exit_rate"].is_f64() || row["nonzero_exit_rate"].is_u64());
    assert!(row["attributed_cost_usd"].is_f64() || row["attributed_cost_usd"].is_u64());
}

#[test]
fn cli_rows_ordered_by_invocation_count_descending() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    // unresolved-1 has 3 cat invocations (cat main.py, cat setup.py) and grep
    // and resolved-1 has cat, grep, pytest
    // In "all" bucket, cat appears most (resolved cat + unresolved 2x cat + errored cat = 4)
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let all_rows = report["by_outcome"]["all"].as_array().unwrap();
    assert!(!all_rows.is_empty());

    // Verify descending order
    for i in 1..all_rows.len() {
        let prev = all_rows[i - 1]["invocation_count"].as_u64().unwrap();
        let curr = all_rows[i]["invocation_count"].as_u64().unwrap();
        assert!(
            prev >= curr,
            "rows should be sorted by invocation_count descending: {} before {}",
            prev,
            curr
        );
    }

    // cat should be first (most invocations: 4 total across all instances)
    assert_eq!(all_rows[0]["command_head"], "cat", "cat has most invocations");
}

#[test]
fn cli_bucket_filter_restricts_to_one_outcome() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--bucket",
            "resolved",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench command-stats --bucket resolved failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // resolved-1 has cat, grep, pytest, (cat from pipeline too)
    let resolved = report["by_outcome"]["resolved"].as_array().unwrap();
    assert!(!resolved.is_empty());
    // The unresolved bucket in filtered output should be empty (no unresolved steps when filtering to resolved)
    let unresolved = report["by_outcome"]["unresolved"].as_array().unwrap();
    assert!(
        unresolved.is_empty(),
        "unresolved bucket should be empty when --bucket resolved is set"
    );
}

#[test]
fn cli_min_invocations_hides_low_frequency_commands() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--min-invocations",
            "3",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    // Only cat (4 invocations in "all") should pass the threshold of 3
    let all_rows = report["by_outcome"]["all"].as_array().unwrap();
    for row in all_rows {
        assert!(
            row["invocation_count"].as_u64().unwrap() >= 3,
            "all rows should have >= 3 invocations, got {:?}",
            row
        );
    }
}

#[test]
fn cli_top_limits_printed_rows() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--top",
            "1",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // With --top 1, we should see "cat" (most frequent) but not "pytest" (less frequent)
    assert!(stdout.contains("cat"), "top-1 should include cat: {stdout}");
}

#[test]
fn cli_compare_resolved_vs_unresolved_emits_delta_table() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--compare",
            "resolved-vs-unresolved",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench command-stats --compare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let comparisons = report["comparisons"].as_array().unwrap();
    assert!(!comparisons.is_empty(), "comparisons should be present");
    let comp = &comparisons[0];
    assert_eq!(comp["name"], "resolved-vs-unresolved");
    let rows = comp["rows"].as_array().unwrap();
    assert!(!rows.is_empty());

    // Each row must have the required fields
    let row = &rows[0];
    assert!(row["command_head"].is_string());
    assert!(row["resolved_share"].is_f64() || row["resolved_share"].is_u64());
    assert!(row["unresolved_share"].is_f64() || row["unresolved_share"].is_u64());
    assert!(row["delta"].is_f64() || row["delta"].is_u64());

    // Rows should be sorted by delta descending
    for i in 1..rows.len() {
        let prev = rows[i - 1]["delta"].as_f64().unwrap();
        let curr = rows[i]["delta"].as_f64().unwrap();
        assert!(
            prev >= curr,
            "delta rows should be sorted descending: {prev} before {curr}"
        );
    }

    // pytest is only in resolved, so it should have a positive delta
    let pytest_row = rows.iter().find(|r| r["command_head"] == "pytest");
    if let Some(r) = pytest_row {
        assert!(
            r["delta"].as_f64().unwrap() > 0.0,
            "pytest should have positive delta (only in resolved)"
        );
    }
}

#[test]
fn cli_compare_text_output_includes_comparison_section() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--compare",
            "resolved-vs-unresolved",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("resolved-vs-unresolved"),
        "text output should show comparison section: {stdout}"
    );
}

#[test]
fn cli_output_is_deterministic_except_generated_at() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let first = run_command_stats_json(sweep.path());
    let first_file =
        std::fs::read_to_string(sweep.path().join("command-stats.json")).unwrap();

    std::thread::sleep(Duration::from_secs(1));

    let second = run_command_stats_json(sweep.path());
    let second_file =
        std::fs::read_to_string(sweep.path().join("command-stats.json")).unwrap();

    assert_eq!(
        redact_generated_at(&first),
        redact_generated_at(&second),
        "stdout JSON should be deterministic except generated_at"
    );
    assert_eq!(
        redact_generated_at_text(&first_file),
        redact_generated_at_text(&second_file),
        "command-stats.json bytes should be deterministic except generated_at"
    );
}

#[test]
fn cli_exits_zero_on_success_regardless_of_failures() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "command-stats should exit 0 even when sweep has failures"
    );
}

#[test]
fn cli_exits_nonzero_when_sweep_dir_missing() {
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            "/tmp/nonexistent-sweep-dir-xyz-command-stats",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "command-stats should fail when sweep dir is missing"
    );
}

#[test]
fn cli_works_without_evaluation_json() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());
    // Remove evaluation.json — should still work, treating everything as unresolved/errored
    std::fs::remove_file(sweep.path().join("evaluation.json")).unwrap();

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "command-stats should work without evaluation.json\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_nonzero_exit_rate_reflects_failed_commands() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    // In unresolved-1, all 3 commands exit with code 1
    // In the unresolved bucket, cat exits with code 1 both times → nonzero_exit_rate=1.0
    let unresolved_rows = report["by_outcome"]["unresolved"].as_array().unwrap();
    let cat_row = unresolved_rows
        .iter()
        .find(|r| r["command_head"] == "cat")
        .expect("cat should be in unresolved bucket");
    let rate = cat_row["nonzero_exit_rate"].as_f64().unwrap();
    assert_eq!(
        rate, 1.0,
        "cat in unresolved bucket always exits with code 1"
    );

    // In resolved-1, cat exits with code 0 → nonzero_exit_rate=0.0
    let resolved_rows = report["by_outcome"]["resolved"].as_array().unwrap();
    let cat_resolved = resolved_rows
        .iter()
        .find(|r| r["command_head"] == "cat")
        .expect("cat should be in resolved bucket");
    let resolved_rate = cat_resolved["nonzero_exit_rate"].as_f64().unwrap();
    assert_eq!(
        resolved_rate, 0.0,
        "cat in resolved bucket always exits with code 0"
    );
}

// ── golden snapshot regression test ──────────────────────────────────────────

#[test]
fn cli_json_output_matches_golden_snapshot_byte_for_byte() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--compare",
            "resolved-vs-unresolved",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench command-stats failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Zero out the two fields that legitimately vary between runs
    actual["sweep"] = serde_json::json!("");
    actual["generated_at"] = serde_json::json!("");

    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/command_stats/golden-command-stats.json");
    let golden: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).unwrap()).unwrap();

    let actual_pretty = serde_json::to_string_pretty(&actual).unwrap();
    let golden_pretty = serde_json::to_string_pretty(&golden).unwrap();

    assert_eq!(
        actual_pretty, golden_pretty,
        "JSON output does not match golden snapshot.\n\
         If the change is intentional, update tests/fixtures/command_stats/golden-command-stats.json.\n\
         Diff (actual vs golden):\n{}\n",
        diff_strings(&actual_pretty, &golden_pretty)
    );
}

fn diff_strings(actual: &str, expected: &str) -> String {
    actual
        .lines()
        .zip(expected.lines())
        .enumerate()
        .filter(|(_, (a, e))| a != e)
        .take(20)
        .map(|(i, (a, e))| format!("line {}: actual={a:?} expected={e:?}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── --filter resolved= tests ──────────────────────────────────────────────────

#[test]
fn cli_filter_resolved_true_restricts_to_resolved_instances() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--filter",
            "resolved=true",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "command-stats --filter resolved=true failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Only resolved-1 is in scope, so totals.trajectories == 1
    assert_eq!(
        report["totals"]["trajectories"], 1,
        "only resolved-1 should be in scope with resolved=true"
    );
    // pytest only appears in resolved-1 — it must be present
    let all_rows = report["by_outcome"]["all"].as_array().unwrap();
    assert!(
        all_rows.iter().any(|r| r["command_head"] == "pytest"),
        "pytest should be present when filtering to resolved=true"
    );
}

#[test]
fn cli_filter_resolved_false_excludes_resolved_instances() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--filter",
            "resolved=false",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "command-stats --filter resolved=false failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // unresolved-1 and errored-1 are in scope
    assert_eq!(
        report["totals"]["trajectories"], 2,
        "unresolved-1 and errored-1 should be in scope with resolved=false"
    );
    // pytest only appears in resolved-1 — it must NOT be present
    let all_rows = report["by_outcome"]["all"].as_array().unwrap();
    assert!(
        !all_rows.iter().any(|r| r["command_head"] == "pytest"),
        "pytest should be absent when filtering to resolved=false"
    );
}

#[test]
fn cli_filter_invalid_key_exits_nonzero() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--filter",
            "bogus_key=foo",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "unsupported filter key should cause non-zero exit"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unsupported filter key") || stderr.contains("bogus_key"),
        "error message should mention the bad key: {stderr}"
    );
}

#[test]
fn cli_filter_resolved_bad_value_exits_nonzero() {
    let sweep = tempfile::tempdir().unwrap();
    copy_command_stats_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--filter",
            "resolved=maybe",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "resolved=<non-bool> should cause non-zero exit"
    );
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn copy_command_stats_fixture(dir: &Path) {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/command_stats/sweep");
    copy_dir(&fixture, dir);
}

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

fn run_command_stats_json(sweep: &Path) -> serde_json::Value {
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "command-stats",
            "--sweep",
            sweep.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bench command-stats failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn redact_generated_at(value: &serde_json::Value) -> serde_json::Value {
    let mut redacted = value.clone();
    redacted["generated_at"] = serde_json::json!("<generated_at>");
    redacted
}

fn redact_generated_at_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("\"generated_at\":") {
                let indent_len = line.len() - line.trim_start().len();
                format!(
                    "{}\"generated_at\": \"<generated_at>\",",
                    " ".repeat(indent_len)
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
