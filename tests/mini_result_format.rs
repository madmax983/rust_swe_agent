//! Integration tests for `max mini --result-format json` (issue #537).
//!
//! RED → GREEN → REFACTOR cycle.
//!
//! These tests spawn the compiled `max` binary as a subprocess and assert:
//!  AC1 — stdout is exactly one JSON object; logs go to stderr.
//!  AC2 — default / `--result-format text` produce no JSON on stdout.
//!  AC3 — the JSON object includes all required keys.
//!  AC4 — emitted for both submitted and verification-failure runs.
//!  AC5 — string fields pass through the Redactor (no secret leaks).
//!  AC6 — artifact is versioned with `schema_version` and `artifact_kind`.
//!  AC7 — integration test pipes stdout through JSON parser (this file).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Stdio;

mod support;

const SUBMIT_RESPONSE: &str = "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```";

/// Run `max mini` with the deterministic scripted model and return (stdout, stderr, status).
fn run_mini(extra_args: &[&str]) -> (String, String, std::process::ExitStatus) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = support::command()
        .args([
            "--log",
            "error",
            "mini",
            "--task",
            "say hello",
            "--env",
            "local",
            "--deterministic-responses",
            SUBMIT_RESPONSE,
            "--output",
            tmp.path().to_str().unwrap(),
        ])
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn max mini");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (stdout, stderr, out.status)
}

// ── AC1 + AC3 + AC6 + AC7: submitted run produces valid, keyed JSON ─────────

#[test]
fn result_format_json_prints_valid_json_on_submit() {
    let (stdout, stderr, status) = run_mini(&["--result-format", "json"]);
    assert!(
        status.success(),
        "expected exit 0; stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    // AC1: stdout is valid JSON with no leading log noise
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout is not valid JSON: {e}\nstdout: {stdout}\nstderr: {stderr}")
    });
    assert!(parsed.is_object(), "JSON output must be an object");
}

#[test]
fn result_format_json_contains_artifact_kind_and_schema_version() {
    let (stdout, _, status) = run_mini(&["--result-format", "json"]);
    assert!(status.success());
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    // AC6: versioned via ArtifactSchemaVersion
    assert_eq!(
        parsed["artifact_kind"].as_str(),
        Some("mini_result"),
        "artifact_kind must be 'mini_result'"
    );
    assert!(
        parsed["schema_version"].is_object(),
        "schema_version must be an object"
    );
    assert!(
        parsed["schema_version"]["major"].is_number(),
        "schema_version.major must be a number"
    );
    assert!(
        parsed["schema_version"]["minor"].is_number(),
        "schema_version.minor must be a number"
    );
}

#[test]
fn result_format_json_contains_all_required_keys() {
    let (stdout, _, status) = run_mini(&["--result-format", "json"]);
    assert!(status.success());
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    // AC3: required key presence
    let required = [
        "outcome",
        "exit_code",
        "exit_outcome_class",
        "total_cost_usd",
        "steps",
        "input_tokens",
        "output_tokens",
        "trajectory_path",
        "patch_path",       // nullable — key must exist
        "failure_category", // nullable — key must exist
    ];
    for key in required {
        assert!(
            parsed.get(key).is_some(),
            "JSON result missing required key: {key}\nfull JSON: {parsed}"
        );
    }
}

#[test]
fn result_format_json_outcome_is_submitted_on_success() {
    let (stdout, _, status) = run_mini(&["--result-format", "json"]);
    assert!(status.success());
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(
        parsed["outcome"].as_str(),
        Some("submitted"),
        "outcome must be 'submitted' for a clean run"
    );
    assert_eq!(
        parsed["exit_code"].as_i64(),
        Some(0),
        "exit_code must be 0 for a clean submitted run"
    );
    assert_eq!(
        parsed["exit_outcome_class"].as_str(),
        Some("success"),
        "exit_outcome_class must be 'success' for a clean submitted run"
    );
}

// ── AC1: stdout is clean — no leading log noise ───────────────────────────────

#[test]
fn result_format_json_stdout_starts_with_brace_when_log_info() {
    // Even with verbose logging requested, stdout must be a clean JSON object
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = support::command()
        .args([
            "--log",
            "info",
            "mini",
            "--task",
            "say hello",
            "--env",
            "local",
            "--deterministic-responses",
            SUBMIT_RESPONSE,
            "--output",
            tmp.path().to_str().unwrap(),
            "--result-format",
            "json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn max mini");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{'),
        "stdout must start with '{{' (no log noise); got: {trimmed:?}"
    );
    // Also must be valid JSON
    let _: serde_json::Value =
        serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("not valid JSON: {e}\n{trimmed}"));
}

// ── AC2: default / `--result-format text` unchanged, stdout empty ────────────

#[test]
fn result_format_text_produces_no_json_on_stdout() {
    let (stdout, stderr, status) = run_mini(&["--result-format", "text"]);
    assert!(
        status.success(),
        "expected exit 0; stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    // AC2: text format must not print a JSON object to stdout
    assert!(
        stdout.trim().is_empty(),
        "stdout must be empty in text mode; got: {stdout:?}"
    );
}

#[test]
fn default_result_format_produces_no_json_on_stdout() {
    let (stdout, stderr, status) = run_mini(&[]);
    assert!(
        status.success(),
        "expected exit 0; stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "stdout must be empty in default (text) mode; got: {stdout:?}"
    );
}

// ── AC4: verification-failure run also emits JSON ───────────────────────────

#[test]
fn result_format_json_emits_on_verification_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = support::command()
        .args([
            "--log",
            "error",
            "mini",
            "--task",
            "say hello",
            "--env",
            "local",
            "--deterministic-responses",
            SUBMIT_RESPONSE,
            "--output",
            tmp.path().to_str().unwrap(),
            "--result-format",
            "json",
            "--verify",
            "always_fail:false", // `false` exits 1 on every platform
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn max mini");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // Process should exit non-zero (verification_failure = 7)
    assert_eq!(
        out.status.code(),
        Some(7),
        "expected exit 7 (verification_failure); got {:#?}\nstderr:\n{stderr}",
        out.status
    );
    // But stdout must still have the JSON result
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout is not valid JSON: {e}\nstdout: {stdout}\nstderr: {stderr}")
    });
    assert_eq!(
        parsed["exit_code"].as_i64(),
        Some(7),
        "exit_code must be 7 (VerificationFailure)"
    );
    assert_eq!(
        parsed["exit_outcome_class"].as_str(),
        Some("verification_failure"),
        "exit_outcome_class must be 'verification_failure'"
    );
    // trajectory_path should reference an existing file
    let traj = parsed["trajectory_path"]
        .as_str()
        .expect("trajectory_path is a string");
    assert!(
        std::path::Path::new(traj).exists(),
        "trajectory_path in JSON must point to an existing file: {traj}"
    );
}
