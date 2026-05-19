//! `agent env preview` — structured preview of the agent environment.
//!
//! Issue #313. Prints host paths, network egress, hooks, MCP servers,
//! forwarded env vars (redacted), and policy allow/deny rules. Exits 13
//! (`env_preview_warning`) when risky findings are detected.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::redaction::Redactor;

// ── Public option struct ──────────────────────────────────────────────────────

/// Options passed to [`run_env_preview`].
#[derive(Debug, Clone)]
pub struct EnvPreviewOpts {
    /// Environment type string: `"local"`, `"docker"`, or `"chaos"`.
    pub env_type: String,
    /// The task description (used for context, also run through the redactor).
    pub task: String,
    /// Optional path to a config file (already loaded into `cfg` by callers).
    pub config_path: Option<std::path::PathBuf>,
    /// When true, show actual env var values instead of `[REDACTED:env_preview]`.
    pub show_values: bool,
}

// ── Preview types ─────────────────────────────────────────────────────────────

/// A `PreToolUse` / `PostToolUse` hook entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookEntry {
    pub name: String,
    pub command: String,
}

/// Hooks section of the preview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HooksPreview {
    pub pre_tool_use: Vec<HookEntry>,
    pub post_tool_use: Vec<HookEntry>,
}

/// One MCP server entry in the preview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerPreview {
    pub name: String,
    pub command: String,
    /// `true` when the server binary path lies outside the configured workdir.
    pub outside_workdir: bool,
}

/// One environment variable entry (value is redacted unless `--show-values`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvVarPreview {
    pub name: String,
    /// The value, or a `[REDACTED:…]` marker when `sensitive` is true and
    /// `show_values` was not requested.
    pub value_or_redacted: String,
    /// `true` when the name matches a sensitive pattern (`*_API_KEY`, `*_TOKEN`,
    /// `*_SECRET`, etc.).
    pub sensitive: bool,
}

/// Policy section of the preview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyPreview {
    pub profile: String,
    pub extra_deny: Vec<String>,
    pub extra_allow: Vec<String>,
}

/// A single risky finding found during preview analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreviewFinding {
    /// Severity level; currently always `"warning"`.
    pub severity: String,
    pub message: String,
}

/// The complete structured env preview, serialisable to JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvPreview {
    /// Schema version; always `1` in this release.
    pub schema_version: u32,
    /// Env type string: `"local"`, `"docker"`, or `"chaos"`.
    pub env_type: String,
    /// Host filesystem paths the agent has access to.
    pub host_paths: Vec<String>,
    /// Network egress description: `"unrestricted"` or list of allowed hosts.
    pub network_egress: String,
    /// Hook configuration preview.
    pub hooks: HooksPreview,
    /// MCP server configuration preview.
    pub mcp_servers: Vec<McpServerPreview>,
    /// Forwarded environment variables (values redacted unless `--show-values`).
    pub env_vars: Vec<EnvVarPreview>,
    /// Policy allow/deny rules preview.
    pub policy: PolicyPreview,
    /// Risky findings; empty means a clean preview.
    pub findings: Vec<PreviewFinding>,
}

// ── Main function ─────────────────────────────────────────────────────────────

/// Build an [`EnvPreview`] from `cfg` and `opts`.
///
/// All string values that could contain secrets are passed through the
/// trajectory redactor before being stored in the returned struct.
pub fn run_env_preview(cfg: &Config, opts: &EnvPreviewOpts) -> EnvPreview {
    let redactor = Redactor::from_config_lossy(&cfg.root.redaction);
    let workdir = cfg.root.environment.workdir.clone();

    let hooks = build_hooks_preview(cfg, &redactor);
    let mcp_servers = build_mcp_servers_preview(cfg, &redactor, &workdir);
    let env_vars = build_env_vars_preview(opts, &redactor);
    let policy = build_policy_preview(cfg);
    let findings = collect_findings(opts, &workdir, &mcp_servers, &env_vars);

    EnvPreview {
        schema_version: 1,
        env_type: opts.env_type.clone(),
        host_paths: vec![workdir],
        network_egress: "unrestricted".to_owned(),
        hooks,
        mcp_servers,
        env_vars,
        policy,
        findings,
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn build_hooks_preview(cfg: &Config, redactor: &Redactor) -> HooksPreview {
    let pre_tool_use = cfg
        .root
        .agent
        .hooks
        .pre_tool_use
        .iter()
        .map(|h| HookEntry {
            name: redact(redactor, &h.name),
            command: redact(redactor, &h.command),
        })
        .collect();
    let post_tool_use = cfg
        .root
        .agent
        .hooks
        .post_tool_use
        .iter()
        .map(|h| HookEntry {
            name: redact(redactor, &h.name),
            command: redact(redactor, &h.command),
        })
        .collect();
    HooksPreview {
        pre_tool_use,
        post_tool_use,
    }
}

fn build_mcp_servers_preview(
    cfg: &Config,
    redactor: &Redactor,
    workdir: &str,
) -> Vec<McpServerPreview> {
    let workdir_canonical = workdir.trim_end_matches('/');
    cfg.root
        .agent
        .mcp_servers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let command = redact(redactor, &s.command);
            // Check whether the binary path lies outside the workdir using a
            // path-boundary-aware prefix: the exe must equal the workdir exactly
            // or start with `workdir/` to be considered inside it.
            let exe = command.split_whitespace().next().unwrap_or(&command);
            let outside_workdir = exe.starts_with('/')
                && exe != workdir_canonical
                && !exe.starts_with(&format!("{workdir_canonical}/"));
            McpServerPreview {
                name: format!("mcp-{i}"),
                command,
                outside_workdir,
            }
        })
        .collect()
}

fn build_env_vars_preview(opts: &EnvPreviewOpts, _redactor: &Redactor) -> Vec<EnvVarPreview> {
    std::env::vars()
        .filter(|(name, _)| is_sensitive_var_name(name))
        .map(|(name, value)| {
            // Sensitive env var values are ALWAYS masked when --show-values is
            // not set, regardless of whether the config-level redactor is
            // enabled. This prevents [redaction].enabled = false from leaking
            // API keys and tokens into preview output.
            let value_or_redacted = if opts.show_values {
                value
            } else {
                "[REDACTED:env_preview]".to_owned()
            };
            EnvVarPreview {
                name,
                value_or_redacted,
                sensitive: true,
            }
        })
        .collect()
}

fn build_policy_preview(cfg: &Config) -> PolicyPreview {
    PolicyPreview {
        profile: cfg.root.policy.profile.clone(),
        extra_deny: cfg.root.policy.extra_deny_patterns.clone(),
        extra_allow: cfg.root.policy.extra_allow_patterns.clone(),
    }
}

fn collect_findings(
    opts: &EnvPreviewOpts,
    workdir: &str,
    mcp_servers: &[McpServerPreview],
    env_vars: &[EnvVarPreview],
) -> Vec<PreviewFinding> {
    let mut findings: Vec<PreviewFinding> = Vec::new();

    // Wide path: local env with root or empty workdir.
    if opts.env_type == "local" && (workdir == "/" || workdir.trim_end_matches('/').is_empty()) {
        findings.push(PreviewFinding {
            severity: "warning".into(),
            message: format!("Local env with wide host path: {workdir}"),
        });
    }

    // MCP servers with binaries outside the workdir.
    for mcp in mcp_servers {
        if mcp.outside_workdir {
            findings.push(PreviewFinding {
                severity: "warning".into(),
                message: format!(
                    "MCP server '{}' binary is outside workdir ({}): {}",
                    mcp.name, workdir, mcp.command
                ),
            });
        }
    }

    // Sensitive env vars that will be visible to the agent.
    for ev in env_vars {
        if ev.sensitive {
            findings.push(PreviewFinding {
                severity: "warning".into(),
                message: format!(
                    "Sensitive env var '{}' is set and will be forwarded to the agent",
                    ev.name
                ),
            });
        }
    }

    findings
}

/// Returns `true` when the preview has at least one finding.
#[must_use]
pub fn is_risky(preview: &EnvPreview) -> bool {
    !preview.findings.is_empty()
}

/// Exposed for integration tests: verifies the sensitive-name detection logic
/// without requiring the specific var to be set in the process environment.
#[doc(hidden)]
#[must_use]
pub fn is_sensitive_var_name_test_helper(name: &str) -> bool {
    is_sensitive_var_name(name)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run `value` through the redactor on the `env_preview` surface.
fn redact(redactor: &Redactor, value: &str) -> String {
    redactor.redact_text(value, "env_preview").text
}

/// Returns `true` for env var names the harness treats as sensitive.
///
/// Logic mirrors `env_name_is_sensitive` in `src/redaction.rs` so that
/// `agent env preview` and the runtime redactor agree on which vars are secret.
/// Matches: any name containing `TOKEN`, `SECRET`, `PASSWORD`, or `CREDENTIAL`,
/// plus names where a `_`/`-` delimited segment is exactly `KEY`.
fn is_sensitive_var_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if ["TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
    {
        return true;
    }
    upper.split(['_', '-']).any(|segment| segment == "KEY")
}
