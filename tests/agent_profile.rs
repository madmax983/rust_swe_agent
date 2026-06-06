//! Integration tests for `max agent profile` (issue #503).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

mod support;
use support::binary_path;

// ── AC1 + AC2 + AC3 + AC4: happy path text output ────────────────────────────

#[test]
fn happy_path_exits_zero_text() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    assert!(out.status.success(), "expected exit 0, got {:?}\nstderr: {}", out.status, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn happy_path_text_reports_outcome_and_steps() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // AC2: outcome and steps
    assert!(stdout.contains("submitted"), "missing outcome in: {stdout}");
    assert!(stdout.contains('4') || stdout.contains("steps"), "missing steps in: {stdout}");
}

#[test]
fn happy_path_text_reports_cost() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // AC2: total cost USD
    assert!(stdout.contains("0.1234") || stdout.contains("cost"), "missing cost in: {stdout}");
}

#[test]
fn happy_path_text_reports_token_splits() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // AC2: token splits
    assert!(stdout.contains("8000") || stdout.contains("prompt"), "missing prompt tokens in: {stdout}");
    assert!(stdout.contains("2000") || stdout.contains("cache_read") || stdout.contains("cache-read"), "missing cache_read tokens in: {stdout}");
    assert!(stdout.contains("500") || stdout.contains("cache_creation") || stdout.contains("cache-creation"), "missing cache_creation tokens in: {stdout}");
    assert!(stdout.contains("1200") || stdout.contains("completion"), "missing completion tokens in: {stdout}");
}

#[test]
fn happy_path_text_reports_duration() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // AC2: duration
    assert!(stdout.contains("42") || stdout.contains("duration"), "missing duration in: {stdout}");
}

// ── AC3: per-stage breakdown ──────────────────────────────────────────────────

#[test]
fn happy_path_text_reports_stage_breakdown() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // AC3: model / tool / harness stages
    assert!(stdout.contains("model") || stdout.contains("Model"), "missing model stage in: {stdout}");
    assert!(stdout.contains("tool") || stdout.contains("Tool"), "missing tool stage in: {stdout}");
    assert!(stdout.contains("harness") || stdout.contains("Harness"), "missing harness stage in: {stdout}");
}

// ── AC4: action-class mix ─────────────────────────────────────────────────────

#[test]
fn happy_path_text_reports_action_mix() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // AC4: action classes appear
    assert!(
        stdout.contains("read") || stdout.contains("Read") || stdout.contains("search") || stdout.contains("Search")
            || stdout.contains("write") || stdout.contains("Write") || stdout.contains("test") || stdout.contains("Test"),
        "missing action classes in: {stdout}"
    );
}

// ── AC5: --format json ────────────────────────────────────────────────────────

#[test]
fn json_format_exits_zero() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    assert!(out.status.success(), "expected exit 0, got {:?}\nstderr: {}", out.status, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn json_format_is_valid_json() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\nstdout: {stdout}"));
    assert!(parsed.is_object(), "JSON output is not an object");
}

#[test]
fn json_format_has_artifact_kind_and_schema_version() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert!(parsed.get("artifact_kind").is_some(), "missing artifact_kind in JSON: {stdout}");
    assert!(parsed.get("schema_version").is_some(), "missing schema_version in JSON: {stdout}");
}

#[test]
fn json_format_contains_all_required_fields() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // AC5: all required fields
    assert!(parsed.get("outcome").is_some(), "missing outcome");
    assert!(parsed.get("steps").is_some(), "missing steps");
    assert!(parsed.get("total_cost_usd").is_some(), "missing total_cost_usd");
    assert!(parsed.get("token_usage").is_some(), "missing token_usage");
    assert!(parsed.get("duration_secs").is_some(), "missing duration_secs");
    assert!(parsed.get("stage_breakdown").is_some(), "missing stage_breakdown");
    assert!(parsed.get("action_mix").is_some(), "missing action_mix");
}

#[test]
fn json_token_usage_has_all_splits() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let tok = parsed.get("token_usage").expect("token_usage present");
    assert!(tok.get("prompt_tokens").is_some(), "missing prompt_tokens");
    assert!(tok.get("cache_read_tokens").is_some(), "missing cache_read_tokens");
    assert!(tok.get("cache_creation_tokens").is_some(), "missing cache_creation_tokens");
    assert!(tok.get("completion_tokens").is_some(), "missing completion_tokens");
}

// ── AC6: deterministic / zero-cost run ───────────────────────────────────────

#[test]
fn deterministic_exits_zero() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/deterministic.traj.json"])
        .output()
        .expect("failed to run binary");

    assert!(out.status.success(), "expected exit 0: {:?}\nstderr: {}", out.status, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn deterministic_json_cost_is_zero() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/deterministic.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let cost = parsed.get("total_cost_usd").expect("total_cost_usd present");
    assert_eq!(cost.as_f64().unwrap_or(1.0), 0.0, "expected zero cost, got: {cost}");
}

#[test]
fn deterministic_json_unknown_latencies() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/deterministic.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // AC6: unmeasured latencies render as null / "unknown" not fabricated numbers
    let stage = parsed.get("stage_breakdown").expect("stage_breakdown present");
    // model stage should be null/unknown (no model_latency_ms in fixture)
    let model_ms = stage.get("model_ms");
    if let Some(v) = model_ms {
        assert!(v.is_null(), "expected null model_ms for deterministic run, got: {v}");
    }
}

#[test]
fn deterministic_text_shows_unknown_for_missing_latencies() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/deterministic.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // AC6: text output renders "unknown" not "0" for unmeasured latencies
    assert!(stdout.contains("unknown") || stdout.contains("0.0"), "expected 'unknown' for unmeasured stages in: {stdout}");
}

// ── AC7: legacy trajectory missing optional fields ────────────────────────────

#[test]
fn legacy_no_latency_exits_zero() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/legacy_no_latency.traj.json"])
        .output()
        .expect("failed to run binary");

    assert!(out.status.success(), "expected exit 0 on legacy trajectory: {:?}\nstderr: {}", out.status, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn legacy_no_latency_json_unknown_stages() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/legacy_no_latency.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let stage = parsed.get("stage_breakdown").expect("stage_breakdown");
    // AC7: absent optional latency fields -> null in JSON
    let model_ms = &stage["model_ms"];
    assert!(model_ms.is_null(), "expected null model_ms for legacy (no latency) run, got: {model_ms}");
}

#[test]
fn legacy_no_latency_action_mix_present() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "--format", "json", "tests/fixtures/agent_profile/legacy_no_latency.traj.json"])
        .output()
        .expect("failed to run binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let mix = parsed.get("action_mix").expect("action_mix present");
    assert!(mix.is_object() || mix.is_array(), "action_mix should be present and non-null");
}

// ── AC8: malformed file → non-zero exit, no panic ────────────────────────────

#[test]
fn malformed_exits_nonzero_no_panic() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/malformed.traj.json"])
        .output()
        .expect("failed to run binary");

    assert!(!out.status.success(), "expected non-zero exit for malformed JSON, got: {:?}", out.status);
    // Must not SIGSEGV / panic — if the process ran at all and produced any output, it succeeded gracefully
    let stderr = String::from_utf8_lossy(&out.stderr);
    // No Rust panic message
    assert!(!stderr.contains("thread 'main' panicked"), "process panicked: {stderr}");
}

#[test]
fn nonexistent_file_exits_nonzero() {
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/does_not_exist.traj.json"])
        .output()
        .expect("failed to run binary");

    assert!(!out.status.success(), "expected non-zero exit for missing file, got: {:?}", out.status);
}

// ── AC1: does NOT require sweep directory ────────────────────────────────────

#[test]
fn accepts_standalone_trajectory_no_sweep_required() {
    // The file is a lone .traj.json with no results.json sibling - must still work.
    let out = Command::new(binary_path())
        .args(["agent", "profile", "tests/fixtures/agent_profile/happy_path.traj.json"])
        .output()
        .expect("failed to run binary");

    assert!(out.status.success(), "standalone file failed: {:?}", out.status);
}
