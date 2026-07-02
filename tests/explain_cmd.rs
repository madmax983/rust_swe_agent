//! Integration tests for `max explain` (issue #535).
//!
//! Spawns the `max` binary and asserts the read-only, offline, $0 contract:
//! resolving exit codes / outcome classes / failure categories to a documented
//! meaning + remediation, the stable JSON schema, the usage-error (exit 2) on an
//! unknown selector, and the built-in index when no selector is given.

#![allow(clippy::unwrap_used)]

use std::process::Command;

mod support;
use support::binary_path;

/// A fresh `max explain …` command with all provider keys removed, proving the
/// command needs no API key (read-only, offline, $0).
fn explain() -> Command {
    let mut cmd = Command::new(binary_path());
    cmd.arg("explain");
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("OPENAI_API_KEY");
    cmd
}

// ── AC: resolve an exit code integer ─────────────────────────────────────────

#[test]
fn explain_exit_code_integer_exits_zero() {
    let out = explain().arg("7").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("verification_failure"),
        "expected verification_failure in:\n{stdout}"
    );
}

// ── AC: resolve an outcome class name, case-insensitively ────────────────────

#[test]
fn explain_outcome_class_case_insensitive() {
    let lower = explain().arg("verification_failure").output().unwrap();
    let upper = explain().arg("VERIFICATION_FAILURE").output().unwrap();
    assert_eq!(lower.status.code(), Some(0));
    assert_eq!(upper.status.code(), Some(0));
    assert_eq!(lower.stdout, upper.stdout, "case must not change output");
}

// ── AC: resolve a failure category by snake_case and PascalCase ──────────────

#[test]
fn explain_failure_category_snake_and_pascal() {
    let snake = explain().arg("step_limit").output().unwrap();
    let pascal = explain().arg("StepLimit").output().unwrap();
    assert_eq!(
        snake.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&snake.stderr)
    );
    assert_eq!(pascal.status.code(), Some(0));
    let s = String::from_utf8_lossy(&snake.stdout);
    assert!(s.contains("step_limit"), "expected step_limit in:\n{s}");
    assert_eq!(snake.stdout, pascal.stdout, "snake and Pascal must agree");
}

// ── AC: --format json emits the stable schema-versioned object ───────────────

#[test]
fn explain_json_has_stable_schema() {
    let out = explain().args(["--format", "json", "7"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], "1.0");
    assert_eq!(v["code"], 7);
    assert_eq!(v["outcome_class"], "verification_failure");
    assert!(v["meaning"].is_string() && !v["meaning"].as_str().unwrap().is_empty());
    assert!(v["remediation"].is_string() && !v["remediation"].as_str().unwrap().is_empty());
    assert!(v["docs_ref"].is_string() && !v["docs_ref"].as_str().unwrap().is_empty());
}

#[test]
fn explain_json_failure_category_has_null_code() {
    let out = explain()
        .args(["--format", "json", "step_limit"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["outcome_class"], "step_limit");
    assert!(v["code"].is_null(), "failure category has no exit code");
}

// ── AC: unknown selector → usage error (exit 2), lists selector families ──────

#[test]
fn explain_unknown_selector_is_usage_error() {
    let out = explain()
        .arg("definitely_not_a_real_selector")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "unknown selector must exit 2 (usage_error)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage_error"),
        "stderr should carry outcome_class: usage_error:\n{stderr}"
    );
    // Must list the valid selector families to guide the operator.
    assert!(
        stderr.contains("exit code")
            && stderr.contains("outcome class")
            && stderr.contains("failure category"),
        "usage error must list valid selector families:\n{stderr}"
    );
}

// ── AC: no selector → built-in index, exit 0 ─────────────────────────────────

#[test]
fn explain_no_selector_lists_index() {
    let out = explain().output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // A sampling from each taxonomy must appear in the index.
    for needle in [
        "verification_failure",
        "step_limit",
        "host_not_ready",
        "unknown",
    ] {
        assert!(
            stdout.contains(needle),
            "index missing '{needle}':\n{stdout}"
        );
    }
}

#[test]
fn explain_index_json_lists_all_entries() {
    let out = explain().args(["--format", "json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], "1.0");
    let entries = v["entries"].as_array().unwrap();
    // 53 exit-code classes (0–50, 130, 137) + 15 failure categories, minus the
    // 1 merged collision (agent_stagnation) = 67 distinct entries.
    assert_eq!(entries.len(), 67, "unexpected index size");
}

// ── AC: offline / $0 — runs with no API key set ──────────────────────────────

#[test]
fn explain_runs_offline_with_no_api_key() {
    // env_clear removes *everything*; the command must still succeed because it
    // reads only compiled-in data — no network, no model, no key.
    let mut cmd = Command::new(binary_path());
    cmd.env_clear();
    cmd.args(["explain", "verification_failure"]);
    let out = cmd.output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "explain must work with a cleared environment; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
