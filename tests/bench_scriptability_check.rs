//! `bench scriptability-check` — preflight check for MCP servers and hooks.
//!
//! Issue #306. Validates that configured MCP servers and hooks are wired up
//! correctly before any paid sweep. Zero model calls; exits 23
//! (`scriptability_check_failure`) when any server or hook fails.
//!
//! RED phase: these tests fail until the implementation exists.
//! GREEN phase: implement src/run/scriptability_check.rs and wire CLI.
//! REFACTOR phase: clean up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

mod support;
use support::binary_path;

use maxwells_daemon::exit_code::ExitCode;
use maxwells_daemon::run::scriptability_check::{
    HookCheckResult, McpServerCheckResult, McpToolCheckResult, ScriptabilityCheckArgs,
    ScriptabilityCheckReport, render_text,
};

// ── Exit code ─────────────────────────────────────────────────────────────────

#[test]
fn scriptability_check_failure_exit_code_is_23() {
    assert_eq!(ExitCode::ScriptabilityCheckFailure.as_i32(), 23);
}

#[test]
fn scriptability_check_failure_is_distinct_from_preflight_failure() {
    assert_ne!(
        ExitCode::ScriptabilityCheckFailure.as_i32(),
        ExitCode::PreflightFailure.as_i32()
    );
}

#[test]
fn scriptability_check_failure_outcome_class_string() {
    assert_eq!(
        ExitCode::ScriptabilityCheckFailure.outcome_class(),
        "scriptability_check_failure"
    );
}

// ── Struct fields ─────────────────────────────────────────────────────────────

#[test]
fn mcp_tool_check_result_has_expected_fields() {
    let r = McpToolCheckResult {
        name: "diagnose".into(),
        has_input_schema: true,
        schema_valid: true,
    };
    assert_eq!(r.name, "diagnose");
    assert!(r.has_input_schema);
    assert!(r.schema_valid);
}

#[test]
fn mcp_server_check_result_has_expected_fields() {
    let r = McpServerCheckResult {
        name: "mcp-0".into(),
        command: "my-mcp-server".into(),
        ok: true,
        negotiated_protocol_version: Some("2025-11-25".into()),
        tools: vec![],
        duration_ms: 42,
        error: None,
    };
    assert_eq!(r.name, "mcp-0");
    assert!(r.ok);
    assert_eq!(r.negotiated_protocol_version.as_deref(), Some("2025-11-25"));
    assert_eq!(r.duration_ms, 42);
    assert!(r.error.is_none());
}

#[test]
fn hook_check_result_has_expected_fields() {
    let r = HookCheckResult {
        name: "guard".into(),
        phase: "pre_tool_use".into(),
        ok: true,
        exit_code: Some(0),
        duration_ms: 5,
        template_render_ok: true,
        blocking: Some(false),
        stdout_bytes: 0,
        stderr_bytes: 0,
        error: None,
    };
    assert_eq!(r.name, "guard");
    assert_eq!(r.phase, "pre_tool_use");
    assert!(r.ok);
    assert_eq!(r.exit_code, Some(0));
    assert!(r.template_render_ok);
    assert_eq!(r.blocking, Some(false));
}

#[test]
fn report_fields_exist() {
    let report = ScriptabilityCheckReport {
        artifact_kind: maxwells_daemon::artifact::ArtifactKind::ScriptabilityCheck,
        schema_version: maxwells_daemon::artifact::ArtifactSchemaVersion::CURRENT,
        generated_at: "2026-01-01T00:00:00Z".into(),
        config: "<defaults>".into(),
        servers: vec![],
        hooks: vec![],
        all_ok: true,
    };
    assert!(report.all_ok);
    assert_eq!(report.config, "<defaults>");
}

// ── ArtifactKind ──────────────────────────────────────────────────────────────

#[test]
fn artifact_kind_scriptability_check_label() {
    use maxwells_daemon::artifact::ArtifactKind;
    assert_eq!(
        ArtifactKind::ScriptabilityCheck.label(),
        "scriptability_check"
    );
}

// ── run() with empty config ───────────────────────────────────────────────────

#[tokio::test]
async fn run_with_no_servers_no_hooks_is_all_ok() {
    let args = ScriptabilityCheckArgs {
        config_path: None,
        output: None,
    };
    let report = maxwells_daemon::run::scriptability_check::run(&args)
        .await
        .unwrap();
    assert!(report.all_ok);
    assert!(report.servers.is_empty());
    assert!(report.hooks.is_empty());
}

// ── report has no total_cost_usd ──────────────────────────────────────────────

#[tokio::test]
async fn report_has_no_total_cost_usd_field() {
    let args = ScriptabilityCheckArgs {
        config_path: None,
        output: None,
    };
    let report = maxwells_daemon::run::scriptability_check::run(&args)
        .await
        .unwrap();
    let json = serde_json::to_value(&report).unwrap();
    assert!(
        json.get("total_cost_usd").is_none(),
        "report must not include total_cost_usd: {json}"
    );
}

// ── artifact_kind in JSON ─────────────────────────────────────────────────────

#[tokio::test]
async fn report_artifact_kind_serialises_to_scriptability_check() {
    let args = ScriptabilityCheckArgs {
        config_path: None,
        output: None,
    };
    let report = maxwells_daemon::run::scriptability_check::run(&args)
        .await
        .unwrap();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["artifact_kind"], "scriptability_check");
}

// ── hook execution ────────────────────────────────────────────────────────────

#[tokio::test]
async fn passing_pre_tool_use_hook_results_in_ok() {
    let config = maxwells_daemon::config::Config::from_toml_str(
        r#"
[[agent.hooks.pre_tool_use]]
name = "always-pass"
command = "true"
"#,
    )
    .unwrap();

    let report = maxwells_daemon::run::scriptability_check::run_with_config(&config, None)
        .await
        .unwrap();

    assert!(report.all_ok, "all hooks passed, should be ok");
    assert_eq!(report.hooks.len(), 1);
    let hook = &report.hooks[0];
    assert!(hook.ok, "hook should be ok");
    assert_eq!(hook.phase, "pre_tool_use");
    assert!(hook.template_render_ok);
    assert_eq!(
        hook.blocking,
        Some(false),
        "passing pre_tool_use hook should not be blocking"
    );
    assert_eq!(hook.exit_code, Some(0));
}

#[tokio::test]
async fn failing_pre_tool_use_hook_makes_all_ok_false() {
    let config = maxwells_daemon::config::Config::from_toml_str(
        r#"
[[agent.hooks.pre_tool_use]]
name = "always-fail"
command = "false"
"#,
    )
    .unwrap();

    let report = maxwells_daemon::run::scriptability_check::run_with_config(&config, None)
        .await
        .unwrap();

    assert!(!report.all_ok, "failed hook should make all_ok false");
    assert_eq!(report.hooks.len(), 1);
    let hook = &report.hooks[0];
    assert!(!hook.ok, "hook should not be ok");
    assert_eq!(
        hook.blocking,
        Some(true),
        "failing pre_tool_use hook should be blocking"
    );
}

#[tokio::test]
async fn post_tool_use_hook_blocking_is_none() {
    let config = maxwells_daemon::config::Config::from_toml_str(
        r#"
[[agent.hooks.post_tool_use]]
name = "informational"
command = "echo done"
"#,
    )
    .unwrap();

    let report = maxwells_daemon::run::scriptability_check::run_with_config(&config, None)
        .await
        .unwrap();

    assert_eq!(report.hooks.len(), 1);
    let hook = &report.hooks[0];
    assert_eq!(hook.phase, "post_tool_use");
    assert_eq!(
        hook.blocking, None,
        "post_tool_use hooks have no blocking field"
    );
}

#[tokio::test]
async fn hook_template_render_failure_recorded() {
    let config = maxwells_daemon::config::Config::from_toml_str(
        r#"
[[agent.hooks.pre_tool_use]]
name = "broken-template"
command = "{{ unclosed_brace"
"#,
    )
    .unwrap();

    let report = maxwells_daemon::run::scriptability_check::run_with_config(&config, None)
        .await
        .unwrap();

    assert_eq!(report.hooks.len(), 1);
    let hook = &report.hooks[0];
    assert!(
        !hook.template_render_ok,
        "template render should have failed"
    );
    assert!(!hook.ok, "hook with render failure should not be ok");
    assert!(hook.error.is_some(), "error should be recorded");
}

#[tokio::test]
async fn hook_stdout_and_stderr_bytes_are_recorded() {
    let config = maxwells_daemon::config::Config::from_toml_str(
        r#"
[[agent.hooks.post_tool_use]]
name = "with-output"
command = "echo hello"
"#,
    )
    .unwrap();

    let report = maxwells_daemon::run::scriptability_check::run_with_config(&config, None)
        .await
        .unwrap();

    assert_eq!(report.hooks.len(), 1);
    let hook = &report.hooks[0];
    assert!(
        hook.stdout_bytes > 0,
        "stdout_bytes should be > 0 for 'echo hello'"
    );
}

// ── MCP server checks ─────────────────────────────────────────────────────────

#[tokio::test]
async fn broken_mcp_server_makes_all_ok_false() {
    let config = maxwells_daemon::config::Config::from_toml_str(
        r#"
[[agent.mcp_servers]]
command = "__no_such_binary_xyz_scriptability_check__"
timeout_secs = 1
"#,
    )
    .unwrap();

    let report = maxwells_daemon::run::scriptability_check::run_with_config(&config, None)
        .await
        .unwrap();

    assert!(!report.all_ok, "broken server should make all_ok false");
    assert_eq!(report.servers.len(), 1);
    let server = &report.servers[0];
    assert!(!server.ok, "server should not be ok");
    assert!(server.error.is_some(), "error should be recorded");
    assert!(server.negotiated_protocol_version.is_none());
}

#[tokio::test]
async fn server_name_is_derived_from_index() {
    let config = maxwells_daemon::config::Config::from_toml_str(
        r#"
[[agent.mcp_servers]]
command = "__no_such_binary_xyz_scriptability_check__"
timeout_secs = 1
"#,
    )
    .unwrap();

    let report = maxwells_daemon::run::scriptability_check::run_with_config(&config, None)
        .await
        .unwrap();

    assert_eq!(report.servers[0].name, "mcp-0");
}

// ── text output ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn render_text_shows_pass_fail_table() {
    let args = ScriptabilityCheckArgs {
        config_path: None,
        output: None,
    };
    let report = maxwells_daemon::run::scriptability_check::run(&args)
        .await
        .unwrap();
    let text = render_text(&report);
    assert!(
        text.contains("scriptability-check"),
        "text output should mention command: {text}"
    );
}

#[tokio::test]
async fn render_text_shows_ok_for_passing_hook() {
    let config = maxwells_daemon::config::Config::from_toml_str(
        r#"
[[agent.hooks.pre_tool_use]]
name = "my-guard"
command = "true"
"#,
    )
    .unwrap();

    let report = maxwells_daemon::run::scriptability_check::run_with_config(&config, None)
        .await
        .unwrap();
    let text = render_text(&report);
    assert!(text.contains("my-guard"), "hook name should appear: {text}");
    assert!(text.contains("ok"), "should show ok status: {text}");
}

// ── output artifact file ──────────────────────────────────────────────────────

#[tokio::test]
async fn run_writes_artifact_to_output_path() {
    let outdir = tempfile::tempdir().unwrap();
    let args = ScriptabilityCheckArgs {
        config_path: None,
        output: Some(outdir.path().to_owned()),
    };
    let _report = maxwells_daemon::run::scriptability_check::run(&args)
        .await
        .unwrap();

    let artifact_path = outdir.path().join("scriptability_check.json");
    assert!(artifact_path.exists(), "artifact file should be written");

    let json: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(&artifact_path).unwrap()).unwrap();
    assert_eq!(json["artifact_kind"], "scriptability_check");
    assert!(json.get("total_cost_usd").is_none());
}

// ── CLI integration ───────────────────────────────────────────────────────────

#[test]
fn cli_scriptability_check_exits_zero_with_no_config() {
    let out = Command::new(binary_path())
        .args(["--log", "error", "bench", "scriptability-check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_scriptability_check_json_format_produces_valid_json() {
    let out = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "scriptability-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid JSON");
    assert_eq!(value["artifact_kind"], "scriptability_check");
    assert!(
        value.get("total_cost_usd").is_none(),
        "must not include total_cost_usd"
    );
}

#[test]
fn cli_scriptability_check_with_broken_server_exits_23() {
    let config = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
    std::fs::write(
        config.path(),
        r#"
[[agent.mcp_servers]]
command = "__no_such_binary_xyz_scriptability_check__"
timeout_secs = 1
"#,
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args(["--log", "error", "bench", "scriptability-check", "--config"])
        .arg(config.path())
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "expected nonzero exit\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(23),
        "expected exit 23 (scriptability_check_failure)"
    );
}

#[test]
fn cli_scriptability_check_writes_artifact_when_output_given() {
    let outdir = tempfile::tempdir().unwrap();

    let out = Command::new(binary_path())
        .args(["--log", "error", "bench", "scriptability-check", "--output"])
        .arg(outdir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let artifact_path = outdir.path().join("scriptability_check.json");
    assert!(
        artifact_path.exists(),
        "artifact should be written to output dir"
    );

    let json: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(&artifact_path).unwrap()).unwrap();
    assert_eq!(json["artifact_kind"], "scriptability_check");
}

#[test]
fn cli_scriptability_check_outcome_class_on_failure() {
    let config = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
    std::fs::write(
        config.path(),
        r#"
[[agent.mcp_servers]]
command = "__no_such_binary_xyz_scriptability_check__"
timeout_secs = 1
"#,
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args(["--log", "error", "bench", "scriptability-check", "--config"])
        .arg(config.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("scriptability_check_failure"),
        "stderr should contain outcome_class: {stderr}"
    );
}

// ── invalid schema marks server as failed ─────────────────────────────────────

#[test]
fn mcp_server_check_result_ok_false_when_has_invalid_schema_tools() {
    // A server that succeeded but has a tool with invalid schema should not be ok.
    let result = McpServerCheckResult {
        name: "mcp-0".into(),
        command: "my-server".into(),
        ok: false,
        negotiated_protocol_version: Some("2025-11-25".into()),
        tools: vec![McpToolCheckResult {
            name: "broken-tool".into(),
            has_input_schema: true,
            schema_valid: false,
        }],
        duration_ms: 10,
        error: Some("1 tool(s) have invalid inputSchema (expected JSON object)".into()),
    };
    assert!(
        !result.ok,
        "server with invalid-schema tool should not be ok"
    );
    assert!(result.error.is_some());
}
