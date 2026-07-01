//! `agent suite --check` — zero-spend preflight for an `agent suite` task pack
//! (issue #821).
//!
//! Validates that a typo'd verify command, missing test binary, malformed
//! pack, or silent config override is caught *before* any paid run — exit 3
//! (`preflight_failure`) on a fatal check, exit 0 on a clean pack. Zero model
//! calls; no writes to the output directory.
//!
//! RED phase: these tests fail until the CLI wiring exists.
//! GREEN phase: `--check`/`--check-format`/`--strict` flags + `suite_check::run`.
//! REFACTOR phase: clean up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

mod support;
use support::binary_path;

use maxwells_daemon::exit_code::ExitCode;

fn write_tasks(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    path
}

/// A `Command` for the `max` binary with no provider credentials set, so any
/// accidental real model call would fail loudly rather than silently spend.
fn no_credentials_command() -> Command {
    let mut cmd = Command::new(binary_path());
    cmd.env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY");
    cmd
}

// ── exit code success / failure ─────────────────────────────────────────────

#[test]
fn cli_check_valid_pack_exits_zero_with_no_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(
        &dir,
        "tasks.yaml",
        "- id: t1\n  task: fix the bug\n- id: t2\n  task: add a feature\n",
    );

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "expected exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("PASS"), "stdout: {stdout}");
    assert!(stdout.contains("2 task(s)"), "stdout: {stdout}");
}

#[test]
fn cli_check_exits_preflight_failure_on_duplicate_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(
        &dir,
        "tasks.yaml",
        "- id: dup\n  task: first\n- id: dup\n  task: second\n",
    );

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check"])
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(ExitCode::PreflightFailure.as_i32()),
        "expected exit {} (preflight_failure)\nstdout: {}\nstderr: {}",
        ExitCode::PreflightFailure.as_i32(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dup"),
        "stdout should name the bad id: {stdout}"
    );
}

#[test]
fn cli_check_exits_preflight_failure_on_unlaunchable_verify_command() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(
        &dir,
        "tasks.yaml",
        "- id: t1\n  task: fix it\n  verify:\n    - tests:__no_such_binary_xyz_agent_suite_check__ -q\n",
    );

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check"])
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(ExitCode::PreflightFailure.as_i32()));
}

#[test]
fn cli_check_exits_preflight_failure_on_malformed_pack() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "not: [valid, task, schema");

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check"])
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(ExitCode::PreflightFailure.as_i32()));
}

// ── --check-format json ─────────────────────────────────────────────────────

#[test]
fn cli_check_format_json_produces_valid_schema_versioned_report() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check", "--check-format", "json"])
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid JSON");
    let report = &value["suite_check"];
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["task_count"], 1);
    assert!(report["checks"].is_array());
    assert!(report["ok"].as_bool().unwrap());
}

#[test]
fn cli_check_format_json_lists_every_check_with_status() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(
        &dir,
        "tasks.yaml",
        "- id: t1\n  task: fix it\n  verify:\n    - smoke:true\n",
    );

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check", "--check-format", "json"])
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let checks = value["suite_check"]["checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    for c in checks {
        let status = c["status"].as_str().unwrap();
        assert!(
            ["pass", "fail", "warn"].contains(&status),
            "unexpected status: {status}"
        );
    }
    assert!(
        checks
            .iter()
            .any(|c| c["check"] == "verify:smoke" && c["status"] == "pass")
    );
}

// ── --strict escalates hazards ──────────────────────────────────────────────

#[test]
fn cli_check_hazard_is_warning_by_default_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
    let config = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
    std::fs::write(config.path(), "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check", "--config"])
        .arg(config.path())
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "hazards must be non-fatal by default\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_check_strict_escalates_hazard_to_preflight_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
    let config = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
    std::fs::write(config.path(), "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check", "--strict", "--config"])
        .arg(config.path())
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(ExitCode::PreflightFailure.as_i32()));
}

#[test]
fn cli_check_explicit_default_model_flag_still_suppresses_hazard() {
    // The operator explicitly passes `--model claude-opus-4-7` (the clap
    // default value) to intentionally override a config file that sets a
    // different model.name. Because `--model` has no clap default, this
    // must be tracked as "explicitly passed" and suppress the hazard, even
    // under --strict.
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
    let config = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
    std::fs::write(config.path(), "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args([
            "--check",
            "--strict",
            "--model",
            "claude-opus-4-7",
            "--config",
        ])
        .arg(config.path())
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "explicit --model (even matching the default) must suppress the hazard\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_check_rejects_unsafe_suite_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check", "--suite-name", "../escape"])
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(ExitCode::PreflightFailure.as_i32()));
}

// ── No writes to the output directory ───────────────────────────────────────

#[test]
fn cli_check_makes_no_writes_to_output_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");
    let outdir = tempfile::tempdir().unwrap();

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check", "--output"])
        .arg(outdir.path())
        .output()
        .unwrap();

    assert!(out.status.success());
    let entries: Vec<_> = std::fs::read_dir(outdir.path()).unwrap().collect();
    assert!(
        entries.is_empty(),
        "expected no writes to --output dir, found: {entries:?}"
    );
}

// ── MCP server probe reuse (scriptability-check launchability standard) ────

#[test]
fn cli_check_with_broken_mcp_server_exits_preflight_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args([
            "--check",
            "--mcp-server",
            "__no_such_binary_xyz_agent_suite_check__",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(ExitCode::PreflightFailure.as_i32()));
}

// ── --check-format / --strict require --check ───────────────────────────────

#[test]
fn cli_check_format_without_check_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tasks(&dir, "tasks.yaml", "- id: t1\n  task: fix it\n");

    let out = no_credentials_command()
        .args(["--log", "error", "agent", "suite", "--tasks-file"])
        .arg(&path)
        .args(["--check-format", "json"])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "--check-format without --check should be a usage error"
    );
}
