//! `bench scriptability-check`: zero-cost preflight for MCP servers and hooks.
//!
//! Spawns each configured MCP server, performs the initialize + tools/list
//! handshake, dry-runs each configured hook with a synthetic context, then
//! reports pass/fail with timing. No model calls; no network calls beyond what
//! an MCP server itself initiates.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::config::Config;
use crate::env::{Environment, LocalEnvironment};
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::template::Renderer;
use crate::tool::ToolProvider;

// ── public argument struct ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ScriptabilityCheckArgs {
    /// Optional path to a TOML config file. When `None`, the embedded defaults are used.
    pub config_path: Option<PathBuf>,
    /// When provided, the JSON artifact is written to `<output>/scriptability_check.json`.
    pub output: Option<PathBuf>,
}

// ── report types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolCheckResult {
    pub name: String,
    pub has_input_schema: bool,
    pub schema_valid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerCheckResult {
    pub name: String,
    pub command: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negotiated_protocol_version: Option<String>,
    pub tools: Vec<McpToolCheckResult>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookCheckResult {
    pub name: String,
    pub phase: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub template_render_ok: bool,
    /// Present for `pre_tool_use` hooks only; `true` when the hook would block
    /// tool execution (non-zero exit or timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptabilityCheckReport {
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
    pub generated_at: String,
    /// Config file path used, or `"<defaults>"` when none was provided.
    pub config: String,
    pub servers: Vec<McpServerCheckResult>,
    pub hooks: Vec<HookCheckResult>,
    pub all_ok: bool,
    // NOTE: total_cost_usd is intentionally absent — this command makes zero
    // model calls and the AC requires the artifact to omit this field.
}

// ── public entry points ───────────────────────────────────────────────────────

/// Load config from `args.config_path` (or defaults) and run the check.
pub async fn run(args: &ScriptabilityCheckArgs) -> Result<ScriptabilityCheckReport, Error> {
    let config = match &args.config_path {
        Some(path) => Config::load(path)?,
        None => Config::defaults()?,
    };
    let config_label = args
        .config_path
        .as_ref()
        .map_or_else(|| "<defaults>".to_owned(), |p| p.display().to_string());

    let report = run_inner(&config, &config_label).await?;

    if let Some(output_dir) = &args.output {
        write_artifact(output_dir, &report)?;
    }

    Ok(report)
}

/// Run the check against an already-loaded `Config` (useful for tests).
pub async fn run_with_config(
    config: &Config,
    output: Option<&Path>,
) -> Result<ScriptabilityCheckReport, Error> {
    let report = run_inner(config, "<config>").await?;
    if let Some(output_dir) = output {
        write_artifact(output_dir, &report)?;
    }
    Ok(report)
}

// ── internal implementation ───────────────────────────────────────────────────

async fn run_inner(config: &Config, config_label: &str) -> Result<ScriptabilityCheckReport, Error> {
    let env = LocalEnvironment::new();
    let redactor = Redactor::from_config_lossy(&config.root.redaction);
    let renderer = Renderer::new();

    let mut servers = Vec::new();
    for (i, server_cfg) in config.root.agent.mcp_servers.iter().enumerate() {
        let result = check_mcp_server(&env, server_cfg, i, &redactor).await;
        servers.push(result);
    }

    let mut hooks = Vec::new();
    for hook_cfg in &config.root.agent.hooks.pre_tool_use {
        let result = check_hook(
            &env,
            hook_cfg,
            "pre_tool_use",
            config.root.agent.tool_hook_timeout_secs,
            &renderer,
            &redactor,
        )
        .await;
        hooks.push(result);
    }
    for hook_cfg in &config.root.agent.hooks.post_tool_use {
        let result = check_hook(
            &env,
            hook_cfg,
            "post_tool_use",
            config.root.agent.tool_hook_timeout_secs,
            &renderer,
            &redactor,
        )
        .await;
        hooks.push(result);
    }

    let all_ok = servers.iter().all(|s| s.ok) && hooks.iter().all(|h| h.ok);

    Ok(ScriptabilityCheckReport {
        artifact_kind: ArtifactKind::ScriptabilityCheck,
        schema_version: ArtifactSchemaVersion::CURRENT,
        generated_at: chrono_now_utc(),
        config: config_label.to_owned(),
        servers,
        hooks,
        all_ok,
    })
}

async fn check_mcp_server(
    env: &LocalEnvironment,
    cfg: &crate::config::McpServerCfg,
    index: usize,
    redactor: &Redactor,
) -> McpServerCheckResult {
    let name = format!("mcp-{index}");
    let command_display = redactor.redact_text(&cfg.command, surface::INSPECT).text;
    let started = Instant::now();

    match crate::tool::McpStdioServer::discover(env, cfg, 30, None).await {
        Ok(server) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            let tools = server
                .tools()
                .iter()
                .map(|tool| {
                    let has_input_schema = tool.input_schema.is_some();
                    let schema_valid = tool
                        .input_schema
                        .as_ref()
                        .map_or(false, |v: &serde_json::Value| v.is_object());
                    McpToolCheckResult {
                        name: tool.name.clone(),
                        has_input_schema,
                        schema_valid,
                    }
                })
                .collect();
            McpServerCheckResult {
                name,
                command: command_display,
                ok: true,
                negotiated_protocol_version: Some(server.protocol_version().to_owned()),
                tools,
                duration_ms,
                error: None,
            }
        }
        Err(err) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            let error_msg = redactor
                .redact_text(&err.to_string(), surface::INSPECT)
                .text;
            McpServerCheckResult {
                name,
                command: command_display,
                ok: false,
                negotiated_protocol_version: None,
                tools: vec![],
                duration_ms,
                error: Some(error_msg),
            }
        }
    }
}

async fn check_hook(
    env: &LocalEnvironment,
    hook: &crate::config::ToolHookCfg,
    phase: &str,
    default_timeout_secs: u64,
    renderer: &Renderer,
    redactor: &Redactor,
) -> HookCheckResult {
    let context = synthetic_hook_context(hook, phase);
    let started = Instant::now();

    // Attempt to render the template.
    let rendered_command = match renderer.render_str(&hook.command, &context) {
        Ok(cmd) => cmd,
        Err(err) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            return HookCheckResult {
                name: hook.name.clone(),
                phase: phase.to_owned(),
                ok: false,
                exit_code: None,
                duration_ms,
                template_render_ok: false,
                blocking: if phase == "pre_tool_use" {
                    Some(true)
                } else {
                    None
                },
                stdout_bytes: 0,
                stderr_bytes: 0,
                error: Some(redactor.redact_text(&err.to_string(), surface::INSPECT).text),
            };
        }
    };

    // Build env vars for the hook.
    let env_vars = match build_hook_env(&context) {
        Ok(v) => v,
        Err(err) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            return HookCheckResult {
                name: hook.name.clone(),
                phase: phase.to_owned(),
                ok: false,
                exit_code: None,
                duration_ms,
                template_render_ok: true,
                blocking: if phase == "pre_tool_use" {
                    Some(true)
                } else {
                    None
                },
                stdout_bytes: 0,
                stderr_bytes: 0,
                error: Some(redactor.redact_text(&err.to_string(), surface::INSPECT).text),
            };
        }
    };

    let timeout_secs = hook.timeout_secs.unwrap_or(default_timeout_secs);
    let mut req = crate::env::RunRequest::new(rendered_command.clone())
        .with_timeout(std::time::Duration::from_secs(timeout_secs));
    req.env = env_vars;

    let result: Result<crate::env::RunResult, crate::error::EnvError> = env.run(req).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(run_result) => {
            let ok = run_result.exit_code == 0 && !run_result.timed_out;
            let stdout_bytes = run_result.stdout.len();
            let stderr_bytes = run_result.stderr.len();
            let blocking = if phase == "pre_tool_use" {
                Some(!ok)
            } else {
                None
            };
            let error = if !ok {
                let raw = if run_result.timed_out {
                    format!("hook timed out after {timeout_secs}s")
                } else {
                    let combined = run_result.stderr.trim().to_owned();
                    if combined.is_empty() {
                        format!("exit_code={}", run_result.exit_code)
                    } else {
                        combined
                    }
                };
                Some(redactor.redact_text(&raw, surface::INSPECT).text)
            } else {
                None
            };
            HookCheckResult {
                name: hook.name.clone(),
                phase: phase.to_owned(),
                ok,
                exit_code: Some(run_result.exit_code),
                duration_ms,
                template_render_ok: true,
                blocking,
                stdout_bytes,
                stderr_bytes,
                error,
            }
        }
        Err(err) => {
            let error_msg = redactor.redact_text(&err.to_string(), surface::INSPECT).text;
            HookCheckResult {
                name: hook.name.clone(),
                phase: phase.to_owned(),
                ok: false,
                exit_code: None,
                duration_ms,
                template_render_ok: true,
                blocking: if phase == "pre_tool_use" {
                    Some(true)
                } else {
                    None
                },
                stdout_bytes: 0,
                stderr_bytes: 0,
                error: Some(error_msg),
            }
        }
    }
}

/// Build a deterministic synthetic context for hook template rendering.
fn synthetic_hook_context(hook: &crate::config::ToolHookCfg, phase: &str) -> serde_json::Value {
    let returncode = if phase == "pre_tool_use" {
        serde_json::Value::Null
    } else {
        serde_json::Value::Number(0.into())
    };
    serde_json::json!({
        "hook": {
            "phase": phase,
            "name": hook.name,
        },
        "tool": {
            "name": "bash",
        },
        "task": "scriptability-check-preflight",
        "model": "preflight",
        "step": 0,
        "command": "",
        "tool_input": "",
        "returncode": returncode,
        "stdout": "",
        "stderr": "",
        "output": "",
        "timed_out": false,
        "total_cost_usd": 0.0,
    })
}

fn build_hook_env(
    context: &serde_json::Value,
) -> Result<BTreeMap<String, String>, Error> {
    let mut env = BTreeMap::new();
    insert_hook_env_str(&mut env, "MAXWELL_HOOK_NAME", "RUST_SWE_AGENT_HOOK_NAME", &context["hook"]["name"]);
    insert_hook_env_str(&mut env, "MAXWELL_HOOK_PHASE", "RUST_SWE_AGENT_HOOK_PHASE", &context["hook"]["phase"]);
    insert_hook_env_str(&mut env, "MAXWELL_TOOL_NAME", "RUST_SWE_AGENT_TOOL_NAME", &context["tool"]["name"]);
    insert_hook_env_str(&mut env, "MAXWELL_TASK", "RUST_SWE_AGENT_TASK", &context["task"]);
    insert_hook_env_str(&mut env, "MAXWELL_MODEL", "RUST_SWE_AGENT_MODEL", &context["model"]);
    insert_hook_env_str(&mut env, "MAXWELL_STEP", "RUST_SWE_AGENT_STEP", &context["step"]);
    insert_hook_env_str(&mut env, "MAXWELL_COMMAND", "RUST_SWE_AGENT_COMMAND", &context["command"]);
    insert_hook_env_str(&mut env, "MAXWELL_TOOL_INPUT", "RUST_SWE_AGENT_TOOL_INPUT", &context["tool_input"]);
    insert_hook_env_str(&mut env, "MAXWELL_EXIT_CODE", "RUST_SWE_AGENT_EXIT_CODE", &context["returncode"]);
    insert_hook_env_str(&mut env, "MAXWELL_STDOUT", "RUST_SWE_AGENT_STDOUT", &context["stdout"]);
    insert_hook_env_str(&mut env, "MAXWELL_STDERR", "RUST_SWE_AGENT_STDERR", &context["stderr"]);
    insert_hook_env_str(&mut env, "MAXWELL_OUTPUT", "RUST_SWE_AGENT_OUTPUT", &context["output"]);
    insert_hook_env_str(&mut env, "MAXWELL_TIMED_OUT", "RUST_SWE_AGENT_TIMED_OUT", &context["timed_out"]);
    insert_hook_env_str(&mut env, "MAXWELL_TOTAL_COST_USD", "RUST_SWE_AGENT_TOTAL_COST_USD", &context["total_cost_usd"]);
    let ctx_json = serde_json::to_string(context)?;
    env.insert("MAXWELL_CONTEXT_JSON".to_owned(), ctx_json.clone());
    env.insert("RUST_SWE_AGENT_CONTEXT_JSON".to_owned(), ctx_json);
    Ok(env)
}

fn insert_hook_env_str(
    env: &mut BTreeMap<String, String>,
    maxwell_key: &str,
    legacy_key: &str,
    value: &serde_json::Value,
) {
    let s = json_to_env_str(value);
    env.insert(maxwell_key.to_owned(), s.clone());
    env.insert(legacy_key.to_owned(), s);
}

fn json_to_env_str(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn write_artifact(output_dir: &Path, report: &ScriptabilityCheckReport) -> Result<(), Error> {
    std::fs::create_dir_all(output_dir)?;
    let path = output_dir.join("scriptability_check.json");
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(&path, json)?;
    Ok(())
}

fn chrono_now_utc() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Format as ISO 8601 UTC without external deps.
    let s = secs;
    let mins = s / 60;
    let hours = mins / 60;
    let days_since_epoch = hours / 24;
    // Approximate — good enough for an audit timestamp.
    let _ = days_since_epoch; // suppress unused
    format_unix_ts(secs)
}

fn format_unix_ts(secs: u64) -> String {
    // Simple UTC formatter without chrono.
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let total_days = secs / 86400;

    // Compute Gregorian date from days since 1970-01-01
    let (year, month, day) = days_to_ymd(total_days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970u64;
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }
    let month_days: &[u64] = if is_leap(year) {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u64;
    for &md in month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

const fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

// ── text renderer ─────────────────────────────────────────────────────────────

pub fn render_text(report: &ScriptabilityCheckReport) -> String {
    let mut out = String::new();
    let status = if report.all_ok { "PASS" } else { "FAIL" };
    let _ = writeln!(out, "bench scriptability-check  [{status}]");
    let _ = writeln!(out, "config: {}", report.config);
    let _ = writeln!(out, "generated: {}", report.generated_at);
    let _ = writeln!(out);

    if report.servers.is_empty() {
        let _ = writeln!(out, "MCP servers: (none configured)");
    } else {
        let _ = writeln!(out, "MCP servers:");
        for server in &report.servers {
            let ok_str = if server.ok { "ok" } else { "FAIL" };
            let _ = write!(out, "  [{ok_str:4}] {} ({})", server.name, server.command);
            if let Some(ver) = &server.negotiated_protocol_version {
                let _ = write!(out, "  protocol={ver}");
            }
            let _ = writeln!(out, "  {}ms", server.duration_ms);
            if !server.tools.is_empty() {
                for tool in &server.tools {
                    let schema_str = match (tool.has_input_schema, tool.schema_valid) {
                        (false, _) => "no-schema",
                        (true, false) => "invalid-schema",
                        (true, true) => "schema-ok",
                    };
                    let _ = writeln!(out, "      tool: {}  [{}]", tool.name, schema_str);
                }
            }
            if let Some(err) = &server.error {
                let _ = writeln!(out, "      error: {err}");
            }
        }
    }

    let _ = writeln!(out);

    if report.hooks.is_empty() {
        let _ = writeln!(out, "hooks: (none configured)");
    } else {
        let _ = writeln!(out, "hooks:");
        for hook in &report.hooks {
            let ok_str = if hook.ok { "ok" } else { "FAIL" };
            let exit_str = hook
                .exit_code
                .map_or_else(|| "-".to_owned(), |c| c.to_string());
            let _ = writeln!(
                out,
                "  [{ok_str:4}] {phase:<13} {name}  exit={exit_str}  {ms}ms",
                phase = hook.phase,
                name = hook.name,
                ms = hook.duration_ms,
            );
            if let Some(err) = &hook.error {
                let _ = writeln!(out, "      error: {err}");
            }
        }
    }

    let _ = writeln!(out);
    if report.all_ok {
        let _ = writeln!(out, "Result: all checks passed.");
    } else {
        let failed_servers = report.servers.iter().filter(|s| !s.ok).count();
        let failed_hooks = report.hooks.iter().filter(|h| !h.ok).count();
        let _ = writeln!(
            out,
            "Result: {failed_servers} server(s) and {failed_hooks} hook(s) failed."
        );
    }

    out
}
