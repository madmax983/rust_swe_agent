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
    /// Environment type string: `"local"` or `"docker"`.
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
    /// Env type string: `"local"` or `"docker"`.
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
    let workdir_raw = cfg.root.environment.workdir.clone();
    // Redact the workdir itself — a path containing a configured secret literal
    // (e.g. a temp-mount name) must not appear verbatim in host_paths.
    let workdir = redact(&redactor, &workdir_raw);

    let hooks = build_hooks_preview(cfg, &redactor);
    let mcp_servers = build_mcp_servers_preview(cfg, &redactor, &workdir_raw);
    // Docker only forwards env vars that are explicitly listed in RunRequest.env,
    // so the host process environment is not inherited.
    let env_vars = if opts.env_type == "local" {
        build_env_vars_preview(opts)
    } else {
        Vec::new()
    };
    let policy = build_policy_preview(cfg, &redactor);
    let mut findings = collect_findings(opts, &workdir, &mcp_servers, &env_vars);

    // Validate the policy config now so the preview catches configs that will
    // fail at runtime (invalid profile name, invalid regex pattern). Run the
    // error text through the redactor — an invalid regex containing a secret
    // literal would otherwise leak verbatim into the findings.
    if let Err(e) = crate::policy::PolicyEngine::from_cfg(&cfg.root.policy) {
        findings.push(PreviewFinding {
            severity: "warning".into(),
            message: redact(
                &redactor,
                &format!("Policy config is invalid and will fail at agent startup: {e}"),
            ),
        });
    }

    // Docker requires a configured image; warn early so CI gates catch this
    // before a live run fails in build_docker_env.
    if opts.env_type == "docker"
        && cfg
            .root
            .environment
            .docker_image
            .as_deref()
            .unwrap_or("")
            .is_empty()
    {
        findings.push(PreviewFinding {
            severity: "warning".into(),
            message: "Docker environment has no docker_image configured; agent startup will fail"
                .into(),
        });
    }

    // Warn when this binary was compiled without the docker feature — the actual
    // run path immediately rejects docker configs in that case.
    #[cfg(not(feature = "docker"))]
    if opts.env_type == "docker" {
        findings.push(PreviewFinding {
            severity: "warning".into(),
            message: "Docker environment requested but this binary was compiled without the \
                      'docker' feature; agent startup will fail"
                .into(),
        });
    }

    // Warn when --env mismatches config.environment.kind — actual runs always
    // use the kind from the config, so a mismatched preview gives a false signal.
    let config_env_type = match cfg.root.environment.kind {
        crate::config::EnvKind::Local => "local",
        crate::config::EnvKind::Docker => "docker",
    };
    if opts.env_type != config_env_type {
        findings.push(PreviewFinding {
            severity: "warning".into(),
            message: format!(
                "Preview --env '{}' does not match config environment.kind '{}'; \
                 actual runs will use '{}'",
                opts.env_type, config_env_type, config_env_type
            ),
        });
    }

    // For docker, the workdir is the container's cwd, not a host path.
    let host_paths = if opts.env_type == "local" {
        vec![workdir]
    } else {
        vec![]
    };

    EnvPreview {
        schema_version: 1,
        env_type: opts.env_type.clone(),
        host_paths,
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
            // Analyse the RAW (pre-redaction) command for path checks: the
            // redactor could mask the leading absolute path token, causing the
            // outside-workdir check to silently pass on a redacted marker.
            let outside_workdir = mcp_is_outside_workdir(&s.command, workdir_canonical);
            // Only the display string goes through the redactor.
            let command = redact(redactor, &s.command);
            McpServerPreview {
                name: format!("mcp-{i}"),
                command,
                outside_workdir,
            }
        })
        .collect()
}

/// Determine whether the executable in an MCP command string lies outside
/// `workdir_canonical` (already stripped of trailing `/`).
///
/// Handles leading `KEY=value` shell-environment assignments so that
/// `FOO=bar /usr/local/bin/mcp` resolves to `/usr/local/bin/mcp`, not
/// the `FOO=bar` assignment token.
///
/// Relative commands (no leading `/` or `./`) resolve through `$PATH` and
/// almost always land outside the workdir, so they are treated as outside.
///
/// Compound shell commands (`cmd1 && cmd2`, `cmd1 ; cmd2`, pipes) cannot be
/// safely analyzed statically — we conservatively treat them as outside workdir.
fn mcp_is_outside_workdir(raw_command: &str, workdir_canonical: &str) -> bool {
    if has_shell_operators(raw_command) {
        return true;
    }
    let exe = extract_exe_path(raw_command);
    if !exe.starts_with('/') {
        // Relative or PATH-resolved command: flag as outside workdir.
        return true;
    }
    exe != workdir_canonical && !exe.starts_with(&format!("{workdir_canonical}/"))
}

/// Returns `true` when `command` contains shell list/pipe metacharacters that
/// make static executable-path analysis unreliable.
fn has_shell_operators(command: &str) -> bool {
    command.contains("&&") || command.contains("||") || command.contains(';') || {
        // Pipe must be outside `||` to avoid double-counting; `||` already caught above.
        // We use a simple byte scan: if we see `|` not preceded or followed by `|`.
        let bytes = command.as_bytes();
        bytes.windows(1).enumerate().any(|(i, w)| {
            w[0] == b'|'
                && bytes.get(i.wrapping_sub(1)).copied() != Some(b'|')
                && bytes.get(i + 1).copied() != Some(b'|')
        })
    }
}

/// Extract the executable path from a shell command string as an owned `String`.
///
/// Handles:
/// - Leading `KEY=value` shell-env assignments, including quoted values
///   (`FOO='bar baz' /path/bin` → `/path/bin`)
/// - Double- and single-quoted exe paths with spaces
///   (`"/path/with space" arg` → `/path/with space`)
///
/// Returns an owned `String` so quotes can be stripped without lifetime issues.
fn extract_exe_path(command: &str) -> String {
    let mut rest = command.trim();

    // Skip leading shell env assignments: KEY=value, KEY='q val', KEY="q val".
    // We scan for '=' to find the key, then consume the value token with
    // shell_token_end (respecting quotes), so `FOO='bar baz'` is consumed as a
    // single unit rather than splitting on the space inside the quotes.
    while let Some(eq_pos) = rest.find('=') {
        let key = &rest[..eq_pos];
        if !is_valid_shell_key(key) {
            break;
        }
        let after_eq = &rest[eq_pos + 1..];
        let val_end = shell_token_end(after_eq);
        rest = rest[eq_pos + 1 + val_end..].trim_start();
    }

    // Extract the exe, stripping surrounding quotes if present.
    if let Some(inner) = rest.strip_prefix('"') {
        inner
            .split_once('"')
            .map_or_else(|| rest.to_owned(), |(path, _)| path.to_owned())
    } else if let Some(inner) = rest.strip_prefix('\'') {
        inner
            .split_once('\'')
            .map_or_else(|| rest.to_owned(), |(path, _)| path.to_owned())
    } else {
        rest.split_whitespace().next().unwrap_or(rest).to_owned()
    }
}

/// Returns `true` when `key` is a valid POSIX shell variable name (non-empty,
/// alphanumeric + `_` only, no `/` or other path characters).
fn is_valid_shell_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Returns the byte length of one shell token starting at `s`, respecting
/// single- and double-quoted strings so that `'bar baz'` counts as one token.
fn shell_token_end(s: &str) -> usize {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, '"')) => {
            for (i, c) in chars {
                if c == '"' {
                    return i + 1;
                }
            }
            s.len()
        }
        Some((_, '\'')) => {
            for (i, c) in chars {
                if c == '\'' {
                    return i + 1;
                }
            }
            s.len()
        }
        _ => s.find(|c: char| c.is_ascii_whitespace()).unwrap_or(s.len()),
    }
}

fn build_env_vars_preview(opts: &EnvPreviewOpts) -> Vec<EnvVarPreview> {
    // Use vars_os() + lossy conversion so a single non-UTF-8 environment entry
    // (valid on Unix) does not cause a panic and abort the entire preview.
    std::env::vars_os()
        .filter_map(|(name_os, value_os)| {
            let name = name_os.to_string_lossy().into_owned();
            if !is_sensitive_var_name(&name) {
                return None;
            }
            // Sensitive env var values are ALWAYS masked when --show-values is
            // not set, regardless of whether the config-level redactor is
            // enabled. This prevents [redaction].enabled = false from leaking
            // API keys and tokens into preview output.
            let value_or_redacted = if opts.show_values {
                value_os.to_string_lossy().into_owned()
            } else {
                "[REDACTED:env_preview]".to_owned()
            };
            Some(EnvVarPreview {
                name,
                value_or_redacted,
                sensitive: true,
            })
        })
        .collect()
}

fn build_policy_preview(cfg: &Config, redactor: &Redactor) -> PolicyPreview {
    PolicyPreview {
        profile: redact(redactor, &cfg.root.policy.profile),
        // Policy pattern strings go through the redactor so secret literals
        // embedded in operator-supplied regexes are not printed verbatim.
        extra_deny: cfg
            .root
            .policy
            .extra_deny_patterns
            .iter()
            .map(|p| redact(redactor, p))
            .collect(),
        extra_allow: cfg
            .root
            .policy
            .extra_allow_patterns
            .iter()
            .map(|p| redact(redactor, p))
            .collect(),
    }
}

fn collect_findings(
    opts: &EnvPreviewOpts,
    workdir: &str,
    mcp_servers: &[McpServerPreview],
    env_vars: &[EnvVarPreview],
) -> Vec<PreviewFinding> {
    let mut findings: Vec<PreviewFinding> = Vec::new();

    // Local environment: bash commands are not confined to the reported workdir.
    // LocalEnvironment only chdir(req.cwd) when cwd is explicitly set on the
    // RunRequest; DefaultAgent leaves cwd unset, so the process may read paths
    // anywhere on the host.
    if opts.env_type == "local" {
        findings.push(PreviewFinding {
            severity: "warning".into(),
            message:
                "Local environment: bash commands are not confined to workdir (full host filesystem access)".into(),
        });
    }

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

/// Render an [`EnvPreview`] as a human-readable text block.
///
/// This is the same output that `agent env preview` (text mode) writes to
/// stdout. Extracted here so it can be unit-tested independently of the CLI.
#[must_use]
pub fn format_preview_text(preview: &EnvPreview) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "=== Agent Environment Preview ===");
    let _ = writeln!(out, "env_type:       {}", preview.env_type);
    let _ = writeln!(out, "host_paths:     {}", preview.host_paths.join(", "));
    let _ = writeln!(out, "network_egress: {}", preview.network_egress);
    let _ = writeln!(out);
    let _ = writeln!(out, "--- Hooks ---");
    if preview.hooks.pre_tool_use.is_empty() && preview.hooks.post_tool_use.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for h in &preview.hooks.pre_tool_use {
            let _ = writeln!(out, "  PreToolUse  [{}]: {}", h.name, h.command);
        }
        for h in &preview.hooks.post_tool_use {
            let _ = writeln!(out, "  PostToolUse [{}]: {}", h.name, h.command);
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "--- MCP Servers ---");
    if preview.mcp_servers.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for m in &preview.mcp_servers {
            let flag = if m.outside_workdir {
                " [OUTSIDE WORKDIR]"
            } else {
                ""
            };
            let _ = writeln!(out, "  {}: {}{}", m.name, m.command, flag);
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "--- Env Vars (sensitive) ---");
    if preview.env_vars.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for ev in &preview.env_vars {
            let _ = writeln!(out, "  {}: {}", ev.name, ev.value_or_redacted);
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "--- Policy ---");
    let _ = writeln!(out, "  profile:     {}", preview.policy.profile);
    let _ = writeln!(
        out,
        "  extra_deny:  {}",
        if preview.policy.extra_deny.is_empty() {
            "(none)".to_owned()
        } else {
            preview.policy.extra_deny.join(", ")
        }
    );
    let _ = writeln!(
        out,
        "  extra_allow: {}",
        if preview.policy.extra_allow.is_empty() {
            "(none)".to_owned()
        } else {
            preview.policy.extra_allow.join(", ")
        }
    );
    let _ = writeln!(out);
    if preview.findings.is_empty() {
        let _ = writeln!(out, "--- Findings: CLEAN ---");
    } else {
        let _ = writeln!(out, "--- Findings ---");
        for f in &preview.findings {
            let _ = writeln!(out, "  [{}] {}", f.severity.to_uppercase(), f.message);
        }
    }
    out
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
