//! Integration tests for `agent doctor` — zero-cost host-readiness preflight
//! (issue #526). Spawns the `max` binary and asserts exit codes, the
//! `{ check, status, detail }` JSON contract, remediation hints, and the
//! presence-only credential guarantee.

#![allow(clippy::unwrap_used)]

use std::process::Command;

mod support;
use support::binary_path;

/// Base command with both common provider keys removed so credential checks are
/// deterministic regardless of the host/CI environment.
fn doctor() -> Command {
    let mut cmd = Command::new(binary_path());
    cmd.args(["agent", "doctor"]);
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("OPENAI_API_KEY");
    cmd
}

// ── AC#1: zero-cost, exit 0 when all pass ────────────────────────────────────

#[test]
fn doctor_all_pass_exits_zero_json() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args([
            "--env",
            "local",
            "--model",
            "claude-opus-4-7",
            "--format",
            "json",
        ])
        .arg("--output")
        .arg(tmp.path())
        .env("ANTHROPIC_API_KEY", "present")
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ready"], true);
    assert_eq!(v["schema_version"], 1);

    // Every check is pass or skip (none failed).
    let checks = v["checks"].as_array().unwrap();
    for c in checks {
        let s = c["status"].as_str().unwrap();
        assert!(
            s == "pass" || s == "skip",
            "unexpected status {s} for {}",
            c["check"]
        );
    }
    // docker is skipped for a local environment (AC#2c).
    let docker = checks.iter().find(|c| c["check"] == "docker").unwrap();
    assert_eq!(docker["status"], "skip");
}

// ── AC#5: JSON shape is exactly { check, status, detail } ─────────────────────

#[test]
fn doctor_json_checklist_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args(["--model", "claude-opus-4-7", "--format", "json"])
        .arg("--output")
        .arg(tmp.path())
        .env("ANTHROPIC_API_KEY", "present")
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert!(v.get("schema_version").is_some());
    assert!(v.get("ready").is_some());
    let checks = v["checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    for c in checks {
        let obj = c.as_object().unwrap();
        let keys: Vec<&String> = obj.keys().collect();
        assert_eq!(keys.len(), 3, "each check has exactly 3 keys, got {keys:?}");
        assert!(obj.contains_key("check"));
        assert!(obj.contains_key("status"));
        assert!(obj.contains_key("detail"));
    }
}

// ── AC#2b / AC#3 / AC#5: missing credential → exit 48 + remediation ──────────

#[test]
fn doctor_missing_credential_exits_48_with_remediation() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args(["--model", "claude-opus-4-7", "--format", "json"])
        .arg("--output")
        .arg(tmp.path())
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(48));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("outcome_class: host_not_ready"),
        "stderr: {stderr}"
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ready"], false);
    let cred = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "credential")
        .unwrap();
    assert_eq!(cred["status"], "fail");
    let detail = cred["detail"].as_str().unwrap();
    assert!(detail.contains("ANTHROPIC_API_KEY"), "detail: {detail}");
    assert!(
        detail.contains("export"),
        "remediation hint missing: {detail}"
    );
}

// ── AC#4: a secret value is never printed or logged ──────────────────────────

#[test]
fn doctor_never_prints_secret_value() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args(["--model", "claude-opus-4-7"])
        .arg("--output")
        .arg(tmp.path())
        .env("ANTHROPIC_API_KEY", "SUPERSECRETVALUE123")
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("SUPERSECRETVALUE123"),
        "secret leaked into output"
    );
}

// ── AC#2b: deterministic model needs no credential → skip, exit 0 ────────────

#[test]
fn doctor_deterministic_model_skips_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args(["--model", "deterministic", "--format", "json"])
        .arg("--output")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let cred = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "credential")
        .unwrap();
    assert_eq!(cred["status"], "skip");
}

// ── AC#2c: docker is skipped for a local environment ─────────────────────────

#[test]
fn doctor_docker_skip_when_local() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args([
            "--env",
            "local",
            "--model",
            "deterministic",
            "--format",
            "json",
        ])
        .arg("--output")
        .arg(tmp.path())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let docker = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "docker")
        .unwrap();
    assert_eq!(docker["status"], "skip");
    assert!(docker["detail"].as_str().unwrap().contains("local"));
}

// ── --env falls back to config when omitted ──────────────────────────────────
// A docker-backed config must not be silently validated as local. With no
// `--env` flag and `[environment] kind = "docker"`, the docker check runs
// (status != skip) rather than being skipped as it would be for a local env.

#[test]
fn doctor_env_falls_back_to_docker_config() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("docker.toml");
    std::fs::write(&cfg, "[environment]\nkind = \"docker\"\n").unwrap();
    let out = doctor()
        .args(["--model", "deterministic", "--format", "json"])
        .arg("--config")
        .arg(&cfg)
        .arg("--output")
        .arg(tmp.path())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let docker = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "docker")
        .unwrap();
    assert_ne!(
        docker["status"], "skip",
        "docker check must run for a docker-backed config even without --env"
    );
}

// ── Docker check mirrors the run path's "no docker feature" rejection ─────────
// The test binary is built without `--features docker`, so a docker environment
// is un-runnable regardless of daemon state — the docker check must fail and
// name the missing feature (mirrors `build_docker_env`).

#[test]
fn doctor_docker_env_without_feature_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args([
            "--env",
            "docker",
            "--model",
            "claude-opus-4-7",
            "--format",
            "json",
        ])
        .arg("--output")
        .arg(tmp.path())
        .env("ANTHROPIC_API_KEY", "present")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(48));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let docker = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "docker")
        .unwrap();
    assert_eq!(docker["status"], "fail");
    let detail = docker["detail"].as_str().unwrap();
    assert!(detail.contains("docker"), "detail: {detail}");
    assert!(
        detail.contains("feature"),
        "should name the docker feature: {detail}"
    );
}

// ── AC#2a: git resolvable on PATH (present in CI) ─────────────────────────────

#[test]
fn doctor_git_check_passes_in_ci() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args(["--model", "deterministic", "--format", "json"])
        .arg("--output")
        .arg(tmp.path())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let git = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "git")
        .unwrap();
    assert_eq!(git["status"], "pass");
}

// ── AC#2e: toolchain check never spuriously fails (pass or skip) ─────────────

#[test]
fn doctor_toolchain_pass_or_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args(["--model", "deterministic", "--format", "json"])
        .arg("--output")
        .arg(tmp.path())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let tc = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "toolchain")
        .unwrap();
    let s = tc["status"].as_str().unwrap();
    assert!(s == "pass" || s == "skip", "toolchain status was {s}");
}

// ── AC#2d: un-creatable output dir fails ─────────────────────────────────────
//
// Point `--output` at a subdirectory *of a regular file*. `create_dir_all`
// then fails with "not a directory" regardless of uid — robust even when the
// test runs as root (where mode bits would not block a write).

#[test]
fn doctor_uncreatable_output_dir_exits_48() {
    let tmp = tempfile::tempdir().unwrap();
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"i am a file, not a directory").unwrap();
    let target = blocker.join("runs");

    let out = doctor()
        .args(["--model", "claude-opus-4-7", "--format", "json"])
        .arg("--output")
        .arg(&target)
        .env("ANTHROPIC_API_KEY", "present")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(48));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let dir = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "output_dir")
        .unwrap();
    assert_eq!(dir["status"], "fail");
    assert!(dir["detail"].as_str().unwrap().contains("--output"));
}

// ── AC#6: --help states no model call and $0 ─────────────────────────────────

#[test]
fn doctor_help_mentions_zero_cost_and_no_model_call() {
    let out = Command::new(binary_path())
        .args(["agent", "doctor", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(stdout.contains("$0"), "help should state $0 cost");
    assert!(
        stdout.contains("no model call"),
        "help should state no model call"
    );
}

// ── text format lists all five checks ────────────────────────────────────────

#[test]
fn doctor_text_format_lists_all_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let out = doctor()
        .args(["--model", "claude-opus-4-7"])
        .arg("--output")
        .arg(tmp.path())
        .env("ANTHROPIC_API_KEY", "present")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    for id in ["git", "credential", "docker", "output_dir", "toolchain"] {
        assert!(
            stdout.contains(id),
            "text output missing check '{id}': {stdout}"
        );
    }
}
