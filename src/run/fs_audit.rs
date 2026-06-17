//! `agent fs-audit` — post-hoc filesystem boundary audit for local runs (issue #511).
//!
//! Scans bash commands recorded in trajectory files for path references that
//! resolve outside the configured workdir: absolute paths not under workdir,
//! `..` traversals that escape it, and references to well-known out-of-tree
//! locations (`$HOME`, `/etc`, `/tmp` when not the workdir, system dirs).
//!
//! Zero-cost: read-only over trajectory files, no model calls, no network.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactSchemaVersion;
use crate::error::Error;
use crate::exit_code::ExitCode;

const SCHEMA_VERSION: ArtifactSchemaVersion = ArtifactSchemaVersion::new(1, 0);

// ── Path extraction regex ─────────────────────────────────────────────────────

#[allow(clippy::expect_used)]
fn path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Matches (in priority order):
        // 1. $HOME / ${HOME} references (word-boundary after HOME to avoid
        //    false positives like $HOMEBREW_NO_AUTO_UPDATE)
        // 2. Tilde home references: bare ~, ~/path, ~user, ~user/path
        // 3. ../ dotdot traversals (one or more levels)
        // 4. Absolute paths starting with /
        // Path continuation: anything that isn't whitespace or a shell metachar.
        Regex::new(
            r#"\$(?:\{HOME\}|HOME\b)(?:/[^\s"'|&;<>(){}\\:]*)?|~(?:[a-zA-Z][a-zA-Z0-9_]*)?(?:/[^\s"'|&;<>(){}\\:]*)?|(?:\.\./?)+[^\s"'|&;<>(){}\\:]*|/[a-zA-Z0-9._][^\s"'|&;<>(){}\\:]*"#,
        )
        .expect("fs-audit path regex is valid")
    })
}

// ── Access classification ─────────────────────────────────────────────────────

/// Best-effort access classification for a filesystem path found in a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    Read,
    Write,
    Ambiguous,
}

impl AccessKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Ambiguous => "ambiguous",
        }
    }
}

const READ_HEADS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "file",
    "stat",
    "ls",
    "du",
    "find",
    "diff",
    "cmp",
    "strings",
    "hexdump",
    "xxd",
    "grep",
    "rg",
    "egrep",
    "fgrep",
    "readlink",
    "od",
    "cut",
    "sort",
    "uniq",
    "md5sum",
    "sha256sum",
    "sha1sum",
];

const WRITE_HEADS: &[&str] = &[
    "rm", "rmdir", "mkdir", "touch", "chmod", "chown", "ln", "install", "truncate", "dd", "tee",
    "mktemp", "mkfifo", "mknod",
];

fn classify_access(command: &str) -> AccessKind {
    let head = extract_command_head(command);
    let head = head.as_str();

    if READ_HEADS.contains(&head) {
        return AccessKind::Read;
    }
    if WRITE_HEADS.contains(&head) {
        return AccessKind::Write;
    }
    // Detect shell write-redirect operators (> or >>) anywhere in the command
    if has_write_redirect(command) {
        return AccessKind::Write;
    }
    AccessKind::Ambiguous
}

/// Return `true` when the command string contains `>` or `>>` redirection.
///
/// This is a conservative check — it will trigger on `>>` inside quoted
/// strings, but false positives here only cause `ambiguous` → `write`
/// upgrades, which is acceptable for best-effort classification.
fn has_write_redirect(command: &str) -> bool {
    command.contains('>') && !command.contains(">&-")
}

/// Extract the primary command head, stripping `sudo`, `time`, `env`, and
/// leading `VAR=val` environment assignments.
fn extract_command_head(command: &str) -> String {
    let mut tokens = command.split_whitespace();
    loop {
        let Some(tok) = tokens.next() else {
            return String::new();
        };
        // Strip leading subshell parens before any other check so that
        // `(sudo cmd)` correctly identifies `cmd` as the head.
        let tok = tok.trim_start_matches('(');
        if tok.is_empty() {
            continue;
        }
        // Strip known prefix commands
        if matches!(tok, "sudo" | "time" | "env" | "nohup" | "nice") {
            continue;
        }
        // Strip VAR=val environment assignments (e.g. FOO=bar cmd)
        if !tok.starts_with('-') && tok.contains('=') {
            let before_eq = tok.split('=').next().unwrap_or("");
            if before_eq.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
        }
        return tok.to_owned();
    }
}

// ── Path outside-workdir check ────────────────────────────────────────────────

/// Return `true` when `path` resolves outside `workdir`.
///
/// Conservative (recall-biased): `..` traversals and `$HOME`/`~/` references
/// are always flagged regardless of the workdir value.
fn is_outside_workdir(path: &str, workdir: &str) -> bool {
    // Home directory references — always outside workdir.
    // Use precise boundary for $HOME to avoid matching $HOMEBREW-style vars.
    if path == "$HOME" || path.starts_with("$HOME/") || path.starts_with("${HOME}") {
        return true;
    }
    // Tilde: bare ~, ~/path, ~user, ~user/path — all outside workdir
    if path.starts_with('~') {
        return true;
    }
    // Dotdot traversals — always flag (conservative)
    if path.starts_with("../") || path == ".." || path.starts_with("..\\") {
        return true;
    }
    // Absolute paths
    if path.starts_with('/') {
        let workdir = workdir.trim_end_matches('/');
        let p = path.trim_end_matches('/');
        // Inside workdir?
        if p == workdir || p.starts_with(&format!("{workdir}/")) {
            return false;
        }
        return true;
    }
    false
}

/// Return `true` when `path` is suppressed by any entry in the allowlist.
///
/// A path matches an allowlist entry when it starts with the entry (treated
/// as a directory prefix, e.g. `/etc` suppresses `/etc/passwd`).
fn is_allowlisted(path: &str, allow: &[PathBuf]) -> bool {
    for entry in allow {
        let entry_str = entry.to_string_lossy();
        let entry_norm = entry_str.trim_end_matches('/');
        let path_norm = path.trim_end_matches('/');
        if path_norm == entry_norm || path_norm.starts_with(&format!("{entry_norm}/")) {
            return true;
        }
        // For non-absolute patterns (e.g. $HOME, ~/…), use path-component matching
        // to avoid over-suppression from substring coincidences.
        if !entry_str.starts_with('/') {
            let e: &str = entry_str.as_ref();
            if path == e
                || path.starts_with(&format!("{e}/"))
                || path.ends_with(&format!("/{e}"))
                || path.contains(&format!("/{e}/"))
            {
                return true;
            }
        }
    }
    false
}

// ── Data types ────────────────────────────────────────────────────────────────

/// A single finding: a bash command that accessed a path outside the workdir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsAuditFinding {
    pub instance_id: String,
    pub step_index: usize,
    pub command_head: String,
    pub matched_path: String,
    pub access: AccessKind,
}

/// Aggregate audit report.
#[derive(Debug, Serialize, Deserialize)]
pub struct FsAuditReport {
    pub artifact_kind: String,
    pub schema_version: ArtifactSchemaVersion,
    pub source: String,
    pub workdir: String,
    pub trajectories_scanned: usize,
    pub total_findings: usize,
    pub findings: Vec<FsAuditFinding>,
    pub scan_errors: Vec<String>,
}

impl FsAuditReport {
    /// Compute the process exit code for this report.
    ///
    /// Findings take precedence over scan errors: if both are present,
    /// `FsAuditFindings` (45) is returned so CI gates on the more actionable signal.
    pub fn exit_code(&self) -> ExitCode {
        if self.total_findings > 0 {
            ExitCode::FsAuditFindings
        } else if !self.scan_errors.is_empty() {
            ExitCode::FsAuditScanError
        } else {
            ExitCode::Success
        }
    }
}

// ── Public option types ───────────────────────────────────────────────────────

/// Input source: single trajectory file or a sweep directory.
#[derive(Debug, Clone)]
pub enum FsAuditSource {
    Trajectory(PathBuf),
    Sweep(PathBuf),
}

/// Output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsAuditFormat {
    Text,
    Json,
}

pub fn parse_format(s: &str) -> Result<FsAuditFormat, crate::error::ConfigError> {
    match s {
        "text" | "" => Ok(FsAuditFormat::Text),
        "json" => Ok(FsAuditFormat::Json),
        other => Err(crate::error::ConfigError::Invalid(format!(
            "--format '{other}' is not valid; use 'text' or 'json'"
        ))),
    }
}

/// Options passed to [`run_fs_audit`].
pub struct FsAuditOpts {
    pub source: FsAuditSource,
    /// Override the workdir for path resolution. When `None`, the workdir is
    /// taken from each trajectory's `info.local_workdir` (or `/repo` as fallback).
    pub workdir_override: Option<PathBuf>,
    /// Paths to suppress from findings. A finding is suppressed when its
    /// `matched_path` starts with any entry in this list.
    pub allow: Vec<PathBuf>,
    pub format: FsAuditFormat,
}

// ── Lightweight trajectory deserializer ──────────────────────────────────────

#[derive(Deserialize)]
struct LightTrajectory {
    info: Option<LightInfo>,
    messages: Option<Vec<LightMessage>>,
}

#[derive(Deserialize)]
struct LightInfo {
    #[serde(default)]
    local_workdir: Option<String>,
}

#[derive(Deserialize)]
struct LightMessage {
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    extra: Option<LightExtra>,
}

#[derive(Deserialize, Default)]
struct LightExtra {
    actions: Option<Vec<String>>,
}

// ── Trajectory scanning ───────────────────────────────────────────────────────

/// Replace single-quoted substrings with spaces so that the path regex does
/// not match paths embedded in shell string literals (e.g. `sed 's/a/b/'` or
/// `sed -n '/TODO/p'`).
///
/// Handles backslash-escaped single quotes (`\'`) outside single-quoted strings
/// so that a lone `\'` does not flip `in_sq` and incorrectly mute the rest of
/// the command.
fn strip_single_quoted(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut in_sq = false;
    let mut escaped = false;
    for ch in cmd.chars() {
        if in_sq {
            if ch == '\'' {
                in_sq = false;
            }
            out.push(' ');
        } else if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
            out.push(ch);
        } else if ch == '\'' {
            in_sq = true;
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Extract bash commands from a message (actions field, then bash code blocks).
fn extract_bash_commands(msg: &LightMessage) -> Vec<String> {
    if let Some(extra) = &msg.extra {
        if let Some(actions) = &extra.actions {
            // When the actions field is present it is authoritative — return
            // the filtered bash commands without falling back to content.  A
            // turn that has only __SUBMIT__ or tool-call actions executed no
            // bash, so scanning the content would produce false positives.
            return actions
                .iter()
                .filter(|a| a.as_str() != "__SUBMIT__" && !is_tool_call(a))
                .cloned()
                .collect();
        }
    }
    // No actions field: fall back to bash fenced code blocks in content.
    extract_bash_from_content(msg.content.as_deref().unwrap_or(""))
}

fn is_tool_call(action: &str) -> bool {
    let trimmed = action.trim_start();
    // Require valid JSON so that bash grouping blocks like `{ cmd; }` are not
    // misidentified as tool calls.
    trimmed.starts_with('{') && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
}

fn extract_bash_from_content(content: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut search_from = 0;
    while let Some(start) = content[search_from..].find("```bash\n") {
        let abs_start = search_from + start + 8;
        if let Some(end_rel) = content[abs_start..].find("\n```") {
            commands.push(content[abs_start..abs_start + end_rel].to_owned());
            search_from = abs_start + end_rel + 4;
        } else {
            // Unterminated code block
            let rest = content[abs_start..].trim();
            if !rest.is_empty() {
                commands.push(rest.to_owned());
            }
            break;
        }
    }
    commands
}

/// Resolve the workdir to use for a trajectory: override > trajectory info > default.
fn resolve_workdir(override_wd: Option<&PathBuf>, traj_info_wd: Option<&str>) -> String {
    if let Some(ov) = override_wd {
        return ov.to_string_lossy().into_owned();
    }
    if let Some(wd) = traj_info_wd {
        if !wd.is_empty() {
            return wd.to_owned();
        }
    }
    "/repo".to_owned()
}

/// Derive the instance_id from the trajectory file path.
fn derive_instance_id(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).map_or_else(
        || path.display().to_string(),
        |n| n.strip_suffix(".traj.json").unwrap_or(n).to_owned(),
    )
}

/// Scan a single trajectory file and return all findings.
fn scan_trajectory(
    path: &Path,
    workdir_override: Option<&PathBuf>,
    allow: &[PathBuf],
) -> Result<(Vec<FsAuditFinding>, String), std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    let traj: LightTrajectory = serde_json::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

    let traj_workdir = traj.info.as_ref().and_then(|i| i.local_workdir.as_deref());
    let workdir = resolve_workdir(workdir_override, traj_workdir);

    let instance_id = derive_instance_id(path);
    let Some(messages) = traj.messages else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "trajectory is missing the required 'messages' field",
        ));
    };

    let mut findings = Vec::new();

    for (step_index, msg) in messages.iter().enumerate() {
        let role = msg.role.as_deref().unwrap_or("");
        if role != "assistant" {
            continue;
        }

        let commands = extract_bash_commands(msg);
        for command in &commands {
            let command_head = extract_command_head(command);
            let access = classify_access(command);
            // Strip single-quoted strings before path extraction to avoid
            // matching paths embedded in shell string literals (e.g. sed 's/a/b/').
            let scanned = strip_single_quoted(command);

            for mat in path_re().find_iter(&scanned) {
                let matched = mat.as_str();

                if mat.start() > 0 {
                    let bytes = scanned.as_bytes();
                    let prev = bytes.get(mat.start() - 1).copied();
                    let prev2 = mat
                        .start()
                        .checked_sub(2)
                        .and_then(|i| bytes.get(i))
                        .copied();

                    // Skip URL authority: `https://host/path` — the regex matches
                    // `/host/path` starting at the second `/` in `://`; `prev` is `/`
                    // and `prev2` is `:`.  Only skip when both conditions hold so
                    // that POSIX double-slash paths (`cat //etc/passwd`) are flagged.
                    if prev == Some(b'/') && prev2 == Some(b':') {
                        continue;
                    }

                    // Skip `:path` form used by some URI schemes (e.g. `file:///`).
                    if prev == Some(b':') {
                        continue;
                    }

                    // For absolute paths, skip if inside a relative path token:
                    // e.g. `cat src/run/mod.rs` → the regex matches `/run/mod.rs`
                    // whose preceding byte is `c` from `src`.
                    if matched.starts_with('/')
                        && matches!(prev, Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'.')
                    {
                        continue;
                    }

                    // For tilde references, skip when the preceding byte is an
                    // alphanumeric or `@` — these indicate a version/package specifier
                    // like `foo@~1.2.3` where bash does not expand the tilde.
                    if matched.starts_with('~')
                        && matches!(prev, Some(c) if c.is_ascii_alphanumeric() || c == b'@' || c == b'-' || c == b'+')
                    {
                        continue;
                    }
                }
                if !is_outside_workdir(matched, &workdir) {
                    continue;
                }
                if is_allowlisted(matched, allow) {
                    continue;
                }
                findings.push(FsAuditFinding {
                    instance_id: instance_id.clone(),
                    step_index,
                    command_head: command_head.clone(),
                    matched_path: matched.to_owned(),
                    access,
                });
            }
        }
    }

    Ok((findings, workdir))
}

/// Collect all `*.traj.json` files under `dir`, sorted for determinism.
fn collect_traj_files(dir: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    collect_traj_files_recursive(dir, &mut files, &mut errors);
    files.sort();
    (files, errors)
}

fn collect_traj_files_recursive(dir: &Path, out: &mut Vec<PathBuf>, errors: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            errors.push(format!("{}: {e}", dir.display()));
            return;
        }
    };
    for entry_result in entries {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                errors.push(format!("{}: {e}", entry.path().display()));
                continue;
            }
        };
        let path = entry.path();
        if ft.is_dir() {
            collect_traj_files_recursive(&path, out, errors);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".traj.json"))
        {
            out.push(path);
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the filesystem boundary audit.
///
/// Returns a report with all findings and any scan errors. The caller decides
/// the exit code via [`FsAuditReport::exit_code`].
pub fn run_fs_audit(opts: &FsAuditOpts) -> Result<FsAuditReport, Error> {
    let wd_override = opts.workdir_override.as_ref();

    match &opts.source {
        FsAuditSource::Trajectory(path) => {
            if !path.exists() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("trajectory file not found: {}", path.display()),
                )));
            }
            let source = path.display().to_string();
            match scan_trajectory(path, wd_override, &opts.allow) {
                Ok((findings, workdir)) => {
                    let total = findings.len();
                    Ok(FsAuditReport {
                        artifact_kind: "fs_audit".to_owned(),
                        schema_version: SCHEMA_VERSION,
                        source,
                        workdir,
                        trajectories_scanned: 1,
                        total_findings: total,
                        findings,
                        scan_errors: vec![],
                    })
                }
                Err(e) => Err(Error::Io(e)),
            }
        }

        FsAuditSource::Sweep(dir) => {
            if !dir.is_dir() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("sweep path is not a directory: {}", dir.display()),
                )));
            }
            let source = dir.display().to_string();

            let (traj_files, walk_errors) = collect_traj_files(dir);
            let mut all_findings = Vec::new();
            let mut scan_errors = walk_errors;
            let mut effective_workdir = wd_override
                .map_or_else(|| "/repo".to_owned(), |p| p.to_string_lossy().into_owned());

            for traj_path in &traj_files {
                match scan_trajectory(traj_path, wd_override, &opts.allow) {
                    Ok((mut findings, workdir)) => {
                        // Use the workdir from the first trajectory that has one,
                        // unless an override was provided.
                        if wd_override.is_none()
                            && workdir != "/repo"
                            && effective_workdir == "/repo"
                        {
                            effective_workdir = workdir;
                        }
                        all_findings.append(&mut findings);
                    }
                    Err(e) => {
                        scan_errors.push(format!("{}: {e}", traj_path.display()));
                    }
                }
            }

            let total = all_findings.len();
            Ok(FsAuditReport {
                artifact_kind: "fs_audit".to_owned(),
                schema_version: SCHEMA_VERSION,
                source,
                workdir: effective_workdir,
                trajectories_scanned: traj_files.len(),
                total_findings: total,
                findings: all_findings,
                scan_errors,
            })
        }
    }
}

// ── Formatters ────────────────────────────────────────────────────────────────

/// Format the report as human-readable text.
pub fn format_text(report: &FsAuditReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "fs-audit: scanned {} trajectories, found {} finding(s) [workdir: {}]",
        report.trajectories_scanned, report.total_findings, report.workdir
    );

    if !report.findings.is_empty() {
        let _ = writeln!(out, "\nFindings:");
        for f in &report.findings {
            let _ = writeln!(
                out,
                "  [{}] step {} | {} | {} | {}",
                f.instance_id,
                f.step_index,
                f.command_head,
                f.matched_path,
                f.access.as_str()
            );
        }
    }

    if !report.scan_errors.is_empty() {
        let _ = writeln!(out, "\nScan errors:");
        for err in &report.scan_errors {
            let _ = writeln!(out, "  {err}");
        }
    }

    out
}

/// Format the report as a JSON value.
pub fn format_json(report: &FsAuditReport) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(report)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // ── is_outside_workdir ────────────────────────────────────────────────────

    #[test]
    fn etc_is_outside_repo_workdir() {
        assert!(is_outside_workdir("/etc/passwd", "/repo"));
    }

    #[test]
    fn tmp_is_outside_repo_workdir() {
        assert!(is_outside_workdir("/tmp/foo", "/repo"));
    }

    #[test]
    fn usr_lib_is_outside_repo_workdir() {
        assert!(is_outside_workdir("/usr/lib/python3", "/repo"));
    }

    #[test]
    fn var_is_outside_repo_workdir() {
        assert!(is_outside_workdir("/var/log/app.log", "/repo"));
    }

    #[test]
    fn repo_path_is_inside_workdir() {
        assert!(!is_outside_workdir("/repo/solution.py", "/repo"));
    }

    #[test]
    fn repo_itself_is_inside_workdir() {
        assert!(!is_outside_workdir("/repo", "/repo"));
    }

    #[test]
    fn repo_with_trailing_slash_is_inside_workdir() {
        assert!(!is_outside_workdir("/repo/", "/repo"));
    }

    #[test]
    fn home_dollar_is_always_outside() {
        assert!(is_outside_workdir("$HOME/.ssh/id_rsa", "/repo"));
        assert!(is_outside_workdir("$HOME", "/repo"));
        assert!(is_outside_workdir("${HOME}/foo", "/repo"));
    }

    #[test]
    fn tilde_is_always_outside() {
        assert!(is_outside_workdir("~/config", "/repo"));
    }

    #[test]
    fn dotdot_is_always_outside() {
        assert!(is_outside_workdir("../etc/shadow", "/repo"));
        assert!(is_outside_workdir("../../etc/passwd", "/repo"));
        assert!(is_outside_workdir("..", "/repo"));
    }

    // ── classify_access ───────────────────────────────────────────────────────

    #[test]
    fn cat_is_read() {
        assert_eq!(classify_access("cat /etc/passwd"), AccessKind::Read);
    }

    #[test]
    fn rm_is_write() {
        assert_eq!(classify_access("rm /var/log/app.log"), AccessKind::Write);
    }

    #[test]
    fn echo_redirect_is_write() {
        assert_eq!(
            classify_access("echo secret > /tmp/leak.txt"),
            AccessKind::Write
        );
    }

    #[test]
    fn grep_is_read() {
        assert_eq!(classify_access("grep -r pattern /etc/"), AccessKind::Read);
    }

    #[test]
    fn chmod_is_write() {
        assert_eq!(classify_access("chmod 777 /tmp/file"), AccessKind::Write);
    }

    #[test]
    fn python_script_is_ambiguous() {
        assert_eq!(classify_access("python /opt/run.py"), AccessKind::Ambiguous);
    }

    // ── extract_command_head ──────────────────────────────────────────────────

    #[test]
    fn extracts_simple_head() {
        assert_eq!(extract_command_head("cat /etc/passwd"), "cat");
    }

    #[test]
    fn strips_sudo() {
        assert_eq!(extract_command_head("sudo rm -rf /var"), "rm");
    }

    #[test]
    fn strips_env_assignment() {
        assert_eq!(extract_command_head("FOO=bar cat /etc/passwd"), "cat");
    }

    #[test]
    fn handles_empty_command() {
        assert_eq!(extract_command_head(""), "");
    }

    // ── extract_bash_from_content ─────────────────────────────────────────────

    #[test]
    fn extracts_bash_fence() {
        let content = "```bash\ncat /etc/passwd\n```";
        let cmds = extract_bash_from_content(content);
        assert_eq!(cmds, vec!["cat /etc/passwd"]);
    }

    #[test]
    fn ignores_non_bash_fences() {
        let content = "```python\nprint('hello')\n```";
        let cmds = extract_bash_from_content(content);
        assert!(cmds.is_empty());
    }

    #[test]
    fn extracts_multiple_bash_fences() {
        let content = "Step 1:\n```bash\ncat /etc/passwd\n```\nStep 2:\n```bash\nls /repo\n```";
        let cmds = extract_bash_from_content(content);
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0], "cat /etc/passwd");
        assert_eq!(cmds[1], "ls /repo");
    }

    // ── is_allowlisted ────────────────────────────────────────────────────────

    #[test]
    fn allowlist_suppresses_prefix_match() {
        let allow = vec![PathBuf::from("/etc")];
        assert!(is_allowlisted("/etc/passwd", &allow));
        assert!(is_allowlisted("/etc/shadow", &allow));
    }

    #[test]
    fn allowlist_does_not_suppress_non_match() {
        let allow = vec![PathBuf::from("/etc")];
        assert!(!is_allowlisted("/tmp/foo", &allow));
        assert!(!is_allowlisted("/var/log/app.log", &allow));
    }

    #[test]
    fn allowlist_exact_match() {
        let allow = vec![PathBuf::from("/etc")];
        assert!(is_allowlisted("/etc", &allow));
        assert!(is_allowlisted("/etc/", &allow));
    }

    // ── path regex ────────────────────────────────────────────────────────────

    #[test]
    fn regex_finds_absolute_path() {
        let matches: Vec<_> = path_re().find_iter("cat /etc/passwd").collect();
        assert!(matches.iter().any(|m| m.as_str() == "/etc/passwd"));
    }

    #[test]
    fn regex_finds_home_ref() {
        let matches: Vec<_> = path_re().find_iter("cat $HOME/.ssh/id_rsa").collect();
        assert!(matches.iter().any(|m| m.as_str().starts_with("$HOME")));
    }

    #[test]
    fn regex_finds_dotdot() {
        let matches: Vec<_> = path_re().find_iter("cat ../../etc/shadow").collect();
        assert!(matches.iter().any(|m| m.as_str().starts_with("..")));
    }

    #[test]
    fn regex_finds_tilde() {
        let matches: Vec<_> = path_re().find_iter("ls ~/docs").collect();
        assert!(matches.iter().any(|m| m.as_str().starts_with("~/")));
    }

    // ── exit_code ─────────────────────────────────────────────────────────────

    #[test]
    fn exit_code_success_when_clean() {
        let report = FsAuditReport {
            artifact_kind: "fs_audit".to_owned(),
            schema_version: SCHEMA_VERSION,
            source: "./sweep".to_owned(),
            workdir: "/repo".to_owned(),
            trajectories_scanned: 1,
            total_findings: 0,
            findings: vec![],
            scan_errors: vec![],
        };
        assert_eq!(report.exit_code(), ExitCode::Success);
    }

    #[test]
    fn exit_code_findings_when_violations() {
        let report = FsAuditReport {
            artifact_kind: "fs_audit".to_owned(),
            schema_version: SCHEMA_VERSION,
            source: "./sweep".to_owned(),
            workdir: "/repo".to_owned(),
            trajectories_scanned: 1,
            total_findings: 1,
            findings: vec![FsAuditFinding {
                instance_id: "inst".to_owned(),
                step_index: 0,
                command_head: "cat".to_owned(),
                matched_path: "/etc/passwd".to_owned(),
                access: AccessKind::Read,
            }],
            scan_errors: vec![],
        };
        assert_eq!(report.exit_code(), ExitCode::FsAuditFindings);
    }

    #[test]
    fn exit_code_scan_error_when_errors_no_findings() {
        let report = FsAuditReport {
            artifact_kind: "fs_audit".to_owned(),
            schema_version: SCHEMA_VERSION,
            source: "./sweep".to_owned(),
            workdir: "/repo".to_owned(),
            trajectories_scanned: 0,
            total_findings: 0,
            findings: vec![],
            scan_errors: vec!["bad.traj.json: parse error".to_owned()],
        };
        assert_eq!(report.exit_code(), ExitCode::FsAuditScanError);
    }

    #[test]
    fn findings_exit_code_takes_priority_over_scan_error() {
        let report = FsAuditReport {
            artifact_kind: "fs_audit".to_owned(),
            schema_version: SCHEMA_VERSION,
            source: "./sweep".to_owned(),
            workdir: "/repo".to_owned(),
            trajectories_scanned: 1,
            total_findings: 1,
            findings: vec![FsAuditFinding {
                instance_id: "inst".to_owned(),
                step_index: 0,
                command_head: "cat".to_owned(),
                matched_path: "/etc/passwd".to_owned(),
                access: AccessKind::Read,
            }],
            scan_errors: vec!["other.traj.json: parse error".to_owned()],
        };
        assert_eq!(report.exit_code(), ExitCode::FsAuditFindings);
    }

    // ── format_text ───────────────────────────────────────────────────────────

    #[test]
    fn format_text_shows_summary_line() {
        let report = FsAuditReport {
            artifact_kind: "fs_audit".to_owned(),
            schema_version: SCHEMA_VERSION,
            source: "./sweep".to_owned(),
            workdir: "/repo".to_owned(),
            trajectories_scanned: 2,
            total_findings: 0,
            findings: vec![],
            scan_errors: vec![],
        };
        let text = format_text(&report);
        assert!(text.contains("fs-audit"), "must contain 'fs-audit'");
        assert!(
            text.contains("2 trajectories"),
            "must mention trajectory count"
        );
        assert!(text.contains("0 finding(s)"), "must mention finding count");
        assert!(text.contains("/repo"), "must mention workdir");
    }

    #[test]
    fn format_text_shows_findings() {
        let report = FsAuditReport {
            artifact_kind: "fs_audit".to_owned(),
            schema_version: SCHEMA_VERSION,
            source: "./sweep".to_owned(),
            workdir: "/repo".to_owned(),
            trajectories_scanned: 1,
            total_findings: 1,
            findings: vec![FsAuditFinding {
                instance_id: "inst1".to_owned(),
                step_index: 2,
                command_head: "cat".to_owned(),
                matched_path: "/etc/passwd".to_owned(),
                access: AccessKind::Read,
            }],
            scan_errors: vec![],
        };
        let text = format_text(&report);
        assert!(text.contains("inst1"), "must list instance_id");
        assert!(text.contains("/etc/passwd"), "must list matched_path");
        assert!(text.contains("read"), "must list access kind");
        assert!(text.contains("cat"), "must list command_head");
    }

    #[test]
    fn format_text_shows_scan_errors() {
        let report = FsAuditReport {
            artifact_kind: "fs_audit".to_owned(),
            schema_version: SCHEMA_VERSION,
            source: "./sweep".to_owned(),
            workdir: "/repo".to_owned(),
            trajectories_scanned: 1,
            total_findings: 0,
            findings: vec![],
            scan_errors: vec!["bad.traj.json: parse error".to_owned()],
        };
        let text = format_text(&report);
        assert!(
            text.contains("Scan errors"),
            "must show scan errors section"
        );
        assert!(
            text.contains("bad.traj.json"),
            "must show the error message"
        );
    }

    // ── resolve_workdir ───────────────────────────────────────────────────────

    #[test]
    fn override_takes_precedence_over_traj_info() {
        let ov = PathBuf::from("/workspace");
        assert_eq!(resolve_workdir(Some(&ov), Some("/repo")), "/workspace");
    }

    #[test]
    fn traj_info_used_when_no_override() {
        assert_eq!(resolve_workdir(None, Some("/custom")), "/custom");
    }

    #[test]
    fn default_fallback_is_slash_repo() {
        assert_eq!(resolve_workdir(None, None), "/repo");
    }

    // ── derive_instance_id ────────────────────────────────────────────────────

    #[test]
    fn derives_instance_id_from_path() {
        let p = Path::new("/sweep/django__django-11422.traj.json");
        assert_eq!(derive_instance_id(p), "django__django-11422");
    }

    #[test]
    fn parse_format_accepts_text_and_json() {
        assert_eq!(parse_format("text").unwrap(), FsAuditFormat::Text);
        assert_eq!(parse_format("json").unwrap(), FsAuditFormat::Json);
        assert_eq!(parse_format("").unwrap(), FsAuditFormat::Text);
    }

    #[test]
    fn parse_format_rejects_unknown() {
        assert!(parse_format("csv").is_err());
        assert!(parse_format("xml").is_err());
    }

    // ── is_outside_workdir: tilde and HOME boundary ───────────────────────────

    #[test]
    fn bare_tilde_is_always_outside() {
        assert!(is_outside_workdir("~", "/repo"));
    }

    #[test]
    fn tilde_user_is_always_outside() {
        assert!(is_outside_workdir("~alice/docs", "/repo"));
        assert!(is_outside_workdir("~root", "/repo"));
    }

    #[test]
    fn home_dollar_precise_boundary() {
        // $HOME itself and paths under it are outside
        assert!(is_outside_workdir("$HOME", "/repo"));
        assert!(is_outside_workdir("$HOME/file", "/repo"));
        assert!(is_outside_workdir("${HOME}/file", "/repo"));
    }

    // ── regex: HOME variable boundary and bare tilde ─────────────────────────

    #[test]
    fn regex_does_not_match_homebrew_var() {
        let matches: Vec<_> = path_re()
            .find_iter("export HOMEBREW_NO_AUTO_UPDATE=1")
            .collect();
        assert!(
            matches.is_empty(),
            "HOMEBREW var must not match as $HOME reference, got: {matches:?}"
        );
    }

    #[test]
    fn regex_matches_bare_tilde() {
        let matches: Vec<_> = path_re().find_iter("tar cf /repo/home.tar ~").collect();
        assert!(
            matches.iter().any(|m| m.as_str() == "~"),
            "bare ~ must be detected as a home reference"
        );
    }

    #[test]
    fn regex_matches_tilde_user() {
        let matches: Vec<_> = path_re().find_iter("ls ~alice/docs").collect();
        assert!(
            matches.iter().any(|m| m.as_str().starts_with("~alice")),
            "~user/path must be detected as a home reference"
        );
    }

    // ── extract_bash_commands: submit-only does not fall back to content ──────

    #[test]
    fn submit_only_actions_does_not_scan_content() {
        // When the actions field is present but only has __SUBMIT__, the
        // function must NOT fall back to scanning content for bash blocks.
        let msg = LightMessage {
            role: Some("assistant".to_owned()),
            content: Some("```bash\ncat /etc/passwd\n```".to_owned()),
            extra: Some(LightExtra {
                actions: Some(vec!["__SUBMIT__".to_owned()]),
            }),
        };
        let cmds = extract_bash_commands(&msg);
        assert!(
            cmds.is_empty(),
            "submit-only turn must yield no bash commands, got: {cmds:?}"
        );
    }

    // ── scan_trajectory: missing messages field is a scan error ──────────────

    #[test]
    fn missing_messages_field_is_scan_error() {
        use std::io::Write as _;
        let mut tf = tempfile::NamedTempFile::new().unwrap();
        write!(tf, r#"{{"info": {{"local_workdir": "/repo"}}}}"#).unwrap();
        let result = scan_trajectory(tf.path(), None, &[]);
        assert!(
            result.is_err(),
            "missing 'messages' field must produce a scan error"
        );
    }

    // ── strip_single_quoted ───────────────────────────────────────────────────

    #[test]
    fn sed_substitution_expression_is_blanked() {
        // `sed 's/foo/bar/'` — blanked, no path match
        let result = strip_single_quoted("sed 's/foo/bar/' /repo/main.py");
        assert!(
            !result.contains("/foo/bar/"),
            "sed expression must be blanked, got: {result:?}"
        );
        assert!(
            result.contains("/repo/main.py"),
            "non-quoted path must be preserved, got: {result:?}"
        );
    }

    #[test]
    fn sed_address_pattern_is_blanked() {
        // `sed -n '/TODO/p'` — quoted sed address starts with `/` but is not
        // a filesystem path; it must be blanked to avoid a false positive.
        let result = strip_single_quoted("sed -n '/TODO/p' /repo/main.py");
        assert!(
            !result.contains("/TODO/p"),
            "sed address must be blanked, got: {result:?}"
        );
        assert!(
            result.contains("/repo/main.py"),
            "non-quoted path must be preserved, got: {result:?}"
        );
    }

    // ── scan loop guards ──────────────────────────────────────────────────────

    #[test]
    fn url_path_component_is_not_a_finding() {
        // `curl https://example.com/data` — the regex matches `/example.com/data`
        // starting at the second `/` in `://`; prev=`/`, prev2=`:` → URL guard fires.
        let scanned = strip_single_quoted("curl https://example.com/data");
        let bytes = scanned.as_bytes();
        let surviving: Vec<_> = path_re()
            .find_iter(&scanned)
            .filter(|m| {
                if m.start() > 0 {
                    let prev = bytes.get(m.start() - 1).copied();
                    let prev2 = m.start().checked_sub(2).and_then(|i| bytes.get(i)).copied();
                    if prev == Some(b'/') && prev2 == Some(b':') {
                        return false;
                    }
                    if prev == Some(b':') {
                        return false;
                    }
                }
                is_outside_workdir(m.as_str(), "/repo")
            })
            .collect();
        assert!(
            surviving.is_empty(),
            "URL path component must not become a finding, got: {surviving:?}"
        );
    }

    #[test]
    fn double_slash_posix_path_is_a_finding() {
        // `cat //etc/passwd` — POSIX double-slash absolute path; guard must NOT
        // skip it (prev=`/`, prev2=` ` — not `://`).
        let scanned = strip_single_quoted("cat //etc/passwd");
        let bytes = scanned.as_bytes();
        let findings: Vec<_> = path_re()
            .find_iter(&scanned)
            .filter(|m| {
                if m.start() > 0 {
                    let prev = bytes.get(m.start() - 1).copied();
                    let prev2 = m.start().checked_sub(2).and_then(|i| bytes.get(i)).copied();
                    if prev == Some(b'/') && prev2 == Some(b':') {
                        return false;
                    }
                    if prev == Some(b':') {
                        return false;
                    }
                }
                is_outside_workdir(m.as_str(), "/repo")
            })
            .collect();
        assert!(
            !findings.is_empty(),
            "//etc/passwd must be detected as out-of-workdir"
        );
    }

    #[test]
    fn relative_path_inside_token_is_not_a_finding() {
        // `cat src/run/mod.rs` — the regex matches `/run/mod.rs` but the
        // preceding byte is `c` (word char) → token-boundary guard fires.
        let scanned = strip_single_quoted("cat src/run/mod.rs");
        let bytes = scanned.as_bytes();
        let findings: Vec<_> = path_re()
            .find_iter(&scanned)
            .filter(|m| {
                if m.start() > 0 {
                    let prev = bytes.get(m.start() - 1).copied();
                    if matched_starts_with_slash(m.as_str())
                        && matches!(prev, Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'.')
                    {
                        return false;
                    }
                }
                is_outside_workdir(m.as_str(), "/repo")
            })
            .collect();
        assert!(
            findings.is_empty(),
            "relative path token must not produce a finding, got: {findings:?}"
        );
    }

    fn matched_starts_with_slash(s: &str) -> bool {
        s.starts_with('/')
    }

    #[test]
    fn version_spec_tilde_is_not_a_finding() {
        // `npm install foo@~1.2.3` — `~` is preceded by `@`, not a word boundary.
        let scanned = strip_single_quoted("npm install foo@~1.2.3");
        let bytes = scanned.as_bytes();
        let findings: Vec<_> = path_re()
            .find_iter(&scanned)
            .filter(|m| {
                if m.start() > 0 {
                    let prev = bytes.get(m.start() - 1).copied();
                    if m.as_str().starts_with('~')
                        && matches!(prev, Some(c) if c.is_ascii_alphanumeric() || c == b'@' || c == b'-' || c == b'+')
                    {
                        return false;
                    }
                }
                is_outside_workdir(m.as_str(), "/repo")
            })
            .collect();
        assert!(
            findings.is_empty(),
            "version-spec tilde must not produce a finding, got: {findings:?}"
        );
    }
}
