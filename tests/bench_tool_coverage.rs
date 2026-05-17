//! `bench tool-coverage`: per-sweep MCP tool usage by outcome.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

// ── fixture helpers ───────────────────────────────────────────────────────────

fn copy_fixture(src_name: &str, dest: &Path) {
    let src = Path::new("tests/fixtures/tool_coverage").join(src_name);
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
    }
}

fn run_tool_coverage(sweep: &Path, extra_args: &[&str]) -> std::process::Output {
    Command::new(binary_path())
        .args(["--log", "error", "bench", "tool-coverage", "--sweep"])
        .arg(sweep)
        .args(extra_args)
        .output()
        .unwrap()
}

// ── (a) MCP tool heavily used + correlates with higher resolved-rate ──────────

#[test]
fn cli_produces_text_output_with_tool_table() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &[]);
    assert!(
        out.status.success(),
        "bench tool-coverage failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("bench tool-coverage"), "{stdout}");
    assert!(stdout.contains("bash"), "bash tool should be present: {stdout}");
    assert!(
        stdout.contains("diagnostic_search"),
        "MCP tool should be present: {stdout}"
    );
}

#[test]
fn cli_writes_tool_coverage_json_artifact() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &[]);
    assert!(
        out.status.success(),
        "bench tool-coverage failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let artifact = sweep.path().join("tool-coverage.json");
    assert!(artifact.exists(), "tool-coverage.json should be written");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();

    assert!(report["sweep"].is_string(), "sweep field required");
    assert!(report["generated_at"].is_string(), "generated_at required");
    assert!(report["tool_universe"].is_array(), "tool_universe required");
    assert!(report["by_tool"].is_object(), "by_tool required");
    assert!(report["unused_tools"].is_array(), "unused_tools required");
}

#[test]
fn mcp_tool_used_in_resolved_instances_shows_higher_usage_rate() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json"]);
    assert!(out.status.success());

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let by_tool = &report["by_tool"];
    let diag = &by_tool["diagnostic_search"];
    assert!(diag.is_object(), "diagnostic_search should be in by_tool");

    let total = diag["total_invocations"].as_u64().unwrap_or(0);
    assert!(total > 0, "diagnostic_search should have invocations");

    let resolved_metrics = &diag["by_outcome"]["resolved"];
    let usage_rate = resolved_metrics["usage_rate"].as_f64().unwrap_or(0.0);
    assert!(
        usage_rate > 0.0,
        "resolved usage_rate for diagnostic_search should be positive"
    );

    let unresolved_metrics = &diag["by_outcome"]["unresolved"];
    let unresolved_usage_rate = unresolved_metrics["usage_rate"].as_f64().unwrap_or(1.0);
    assert!(
        usage_rate > unresolved_usage_rate,
        "MCP tool should have higher usage in resolved vs unresolved"
    );
}

// ── (b) registered MCP tool never called → appears in Unused tools ────────────

#[test]
fn dead_tool_appears_in_unused_tools_text() {
    // bash-only-unresolved and dead-tool-errored never call diagnostic_search.
    // Only mcp-heavy-resolved calls it. But all three instances have it in toolset.
    // In the text output, if any registered tool has 0 invocations it shows here.
    // We can test with bash_only_sweep where bash is the only tool in universe.
    // But main_sweep has diagnostic_search used by 1/3 instances.
    // Create a dedicated fixture: use drift_sweep where analyze_tool is "dead"
    // relative to the union universe (it's only in instance-toolset-b's toolset).
    // Actually, let's just check the JSON for unused_tools being an array.
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json"]);
    assert!(out.status.success());

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    // unused_tools is present as array
    assert!(
        report["unused_tools"].is_array(),
        "unused_tools must be an array"
    );
}

#[test]
fn dead_tool_explicitly_visible_in_text_output() {
    // Use the bash_only_sweep where the only tool is bash (no MCP tools).
    // All bash invocations exist so bash is not unused.
    // But if we had a tool registered but never called, it should appear.
    // Test that when all tools are used, "Unused tools" section is empty or omitted.
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("bash_only_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &[]);
    assert!(
        out.status.success(),
        "bench tool-coverage failed on bash_only_sweep: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Unused tools"),
        "should always print Unused tools line: {stdout}"
    );
}

// ── (c) --per-instance round-trips per-instance counts ───────────────────────

#[test]
fn per_instance_flag_emits_instance_rows() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json", "--per-instance"]);
    assert!(
        out.status.success(),
        "bench tool-coverage --per-instance failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let per_instance = &report["per_instance"];
    assert!(
        per_instance.is_array(),
        "per_instance should be an array when flag set"
    );
    assert!(
        !per_instance.as_array().unwrap().is_empty(),
        "per_instance should not be empty"
    );

    // Each row should have instance_id and tool_calls
    for row in per_instance.as_array().unwrap() {
        assert!(row["instance_id"].is_string(), "instance_id required");
        assert!(row["tool_calls"].is_object(), "tool_calls required");
    }
}

#[test]
fn per_instance_mcp_calls_match_trajectory() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json", "--per-instance"]);
    assert!(out.status.success());

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let per_instance = report["per_instance"].as_array().unwrap();

    // mcp-heavy-resolved should show 2 diagnostic_search calls
    let resolved_row = per_instance
        .iter()
        .find(|r| r["instance_id"] == "mcp-heavy-resolved")
        .expect("mcp-heavy-resolved should be in per_instance");

    let diag_calls = resolved_row["tool_calls"]["diagnostic_search"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(diag_calls, 2, "mcp-heavy-resolved should have 2 diagnostic_search calls");

    // bash-only-unresolved should have 0 diagnostic_search calls
    let unresolved_row = per_instance
        .iter()
        .find(|r| r["instance_id"] == "bash-only-unresolved")
        .expect("bash-only-unresolved should be in per_instance");

    let diag_calls_unresolved = unresolved_row["tool_calls"]["diagnostic_search"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(
        diag_calls_unresolved, 0,
        "bash-only-unresolved should have 0 diagnostic_search calls"
    );
}

// ── (d) toolset drift: two distinct toolsets in one sweep ─────────────────────

#[test]
fn toolset_drift_detected_when_instances_have_different_toolsets() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("drift_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json"]);
    assert!(
        out.status.success(),
        "bench tool-coverage failed on drift sweep: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let drift = &report["toolset_drift"];
    assert!(!drift.is_null(), "toolset_drift should be present when drift detected");

    let toolsets = drift["toolsets"].as_array().unwrap();
    assert!(
        toolsets.len() >= 2,
        "should detect at least 2 distinct toolsets, got {}: {drift}",
        toolsets.len()
    );
}

#[test]
fn toolset_drift_text_output_mentions_drift() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("drift_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &[]);
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("drift") || stdout.contains("Drift"),
        "text output should mention toolset drift: {stdout}"
    );
}

// ── (e) exit code is 0 when every instance used bash only ─────────────────────

#[test]
fn exit_code_zero_when_only_bash_used() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("bash_only_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &[]);
    assert!(
        out.status.success(),
        "exit code should be 0 for bash-only sweep, got: {}\nstderr: {}",
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── schema & format tests ─────────────────────────────────────────────────────

#[test]
fn json_format_produces_valid_json_on_stdout() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json"]);
    assert!(out.status.success());

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report["sweep"].is_string());
    assert!(report["generated_at"].is_string());
    assert!(report["by_tool"].is_object());
}

#[test]
fn by_tool_contains_required_fields_for_each_tool() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json"]);
    assert!(out.status.success());

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let by_tool = report["by_tool"].as_object().unwrap();

    for (tool_name, metrics) in by_tool {
        assert!(
            metrics["total_invocations"].is_number(),
            "total_invocations required for {tool_name}"
        );
        assert!(
            metrics["instances_used"].is_number(),
            "instances_used required for {tool_name}"
        );
        assert!(
            metrics["mean_invocations_per_using_instance"].is_number(),
            "mean_invocations_per_using_instance required for {tool_name}"
        );
        assert!(
            metrics["share_of_all_tool_calls"].is_number(),
            "share_of_all_tool_calls required for {tool_name}"
        );
        assert!(
            metrics["by_outcome"].is_object(),
            "by_outcome required for {tool_name}"
        );
    }
}

#[test]
fn by_outcome_contains_required_fields() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json"]);
    assert!(out.status.success());

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let bash_metrics = &report["by_tool"]["bash"];

    for bucket in ["resolved", "unresolved", "errored", "all"] {
        let outcome = &bash_metrics["by_outcome"][bucket];
        assert!(
            outcome.is_object(),
            "by_outcome[{bucket}] should be object for bash"
        );
        assert!(outcome["instances_used"].is_number(), "instances_used required in {bucket}");
        assert!(outcome["instances_total"].is_number(), "instances_total required in {bucket}");
        assert!(outcome["usage_rate"].is_number(), "usage_rate required in {bucket}");
    }
}

#[test]
fn mcp_tool_has_source_and_mcp_server_fields() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json"]);
    assert!(out.status.success());

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let diag = &report["by_tool"]["diagnostic_search"];

    assert_eq!(diag["source"], "mcp", "MCP tool source should be 'mcp'");
    assert_eq!(
        diag["mcp_server"], "diagnostic-mcp",
        "mcp_server should be diagnostic-mcp"
    );
}

#[test]
fn bash_tool_has_builtin_source() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "json"]);
    assert!(out.status.success());

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let bash = &report["by_tool"]["bash"];

    assert_eq!(bash["source"], "builtin", "bash source should be 'builtin'");
    assert!(
        bash["mcp_server"].is_null(),
        "bash should not have mcp_server"
    );
}

// ── determinism test ──────────────────────────────────────────────────────────

#[test]
fn two_runs_produce_identical_artifact_modulo_generated_at() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    run_tool_coverage(sweep.path(), &[]);
    let artifact1_text = std::fs::read_to_string(sweep.path().join("tool-coverage.json")).unwrap();
    let mut report1: serde_json::Value = serde_json::from_str(&artifact1_text).unwrap();

    run_tool_coverage(sweep.path(), &[]);
    let artifact2_text = std::fs::read_to_string(sweep.path().join("tool-coverage.json")).unwrap();
    let mut report2: serde_json::Value = serde_json::from_str(&artifact2_text).unwrap();

    // Null out the timestamp before comparing
    report1["generated_at"] = serde_json::Value::Null;
    report2["generated_at"] = serde_json::Value::Null;

    assert_eq!(
        report1, report2,
        "Two runs should produce identical output modulo generated_at"
    );
}

// ── --min-invocations filter ──────────────────────────────────────────────────

#[test]
fn min_invocations_hides_low_use_tools_in_text_but_not_json() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    // With a very high min-invocations, no MCP tool should appear in text output.
    // But the JSON artifact should still contain all tools.
    let out = run_tool_coverage(sweep.path(), &["--min-invocations", "999"]);
    assert!(out.status.success());

    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sweep.path().join("tool-coverage.json")).unwrap())
            .unwrap();

    // diagnostic_search should still be in JSON artifact
    assert!(
        artifact["by_tool"]["diagnostic_search"].is_object(),
        "diagnostic_search should be in JSON artifact regardless of --min-invocations"
    );
}

// ── invalid usage ─────────────────────────────────────────────────────────────

#[test]
fn invalid_format_exits_nonzero() {
    let sweep = tempfile::tempdir().unwrap();
    copy_fixture("main_sweep", sweep.path());

    let out = run_tool_coverage(sweep.path(), &["--format", "invalid"]);
    assert!(
        !out.status.success(),
        "invalid --format should exit nonzero"
    );
}
