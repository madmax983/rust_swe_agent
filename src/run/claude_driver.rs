//! Drive the Claude Code CLI (`claude`) as the agent backend.
//!
//! The harness ships a built-in bash-first loop, but this module lets an
//! operator point the *same* run machinery at the Claude Code CLI instead.
//! `claude` runs headless in `--output-format stream-json` mode; we read its
//! newline-delimited message stream and translate each message into the
//! harness [`Trajectory`](crate::trajectory::Trajectory) so that patch
//! capture, verification, `bench inspect`, and evaluation all keep working
//! unchanged. The exploration question this answers: *can a more capable
//! coding agent drive the loop and still leave us an inspectable receipt?*
//!
//! ## Seam
//!
//! [`drive`] takes a fully-built [`DefaultAgent`] — which already owns the
//! environment, redactor, and a trajectory seeded with the system + task
//! messages — and fills in the rest of the trajectory from Claude Code's
//! stream, mirroring the finalization that `DefaultAgent::step` performs on
//! its own terminal paths. It returns the same [`ExitReason`] the built-in
//! loop would, so `mini::run` can treat both backends identically.
//!
//! ## Scope (this slice)
//!
//! - Local environment only (the CLI edits the host working tree directly).
//! - Cost and tokens are read from Claude Code's authoritative `result`
//!   message (`CostSource::ProviderReported`).
//! - Model selection is delegated to Claude Code's own configuration; the
//!   actually-responding model is recorded back into the trajectory.
//! - The `claude` binary path is overridable via `MAXWELLS_CLAUDE_BIN` so the
//!   parser can be exercised deterministically at $0 with a fixture script.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::agent::default::truncate_observation_text;
use crate::agent::{DefaultAgent, ExitReason};
use crate::cost::CostSource;
use crate::error::Error;
use crate::model::{Message, MessageExtra};
use crate::redaction::surface;
use crate::stream::StreamEvent;
use crate::trajectory::{
    FailureCategory, TestCommandPattern, TestInvocation, TokenUsage, detect_test_command,
    effective_test_command_patterns, outcome,
};

/// Full toolset handed to Claude Code, one entry per `--allowedTools` value
/// (the CLI documents it as `<tools...>`). Mirrors the operator's choice to
/// let the agent edit directly; mutations still land in the working tree, so
/// `git diff` patch capture in `mini::run` is agnostic to *how* they were
/// made. Web/Task tools are intentionally omitted to keep runs local and
/// reproducible.
const ALLOWED_TOOLS: [&str; 8] = [
    "Bash",
    "Edit",
    "MultiEdit",
    "Write",
    "Read",
    "Glob",
    "Grep",
    "NotebookEdit",
];

/// Environment variable that overrides the `claude` binary path. Used by
/// tests to substitute a deterministic fixture script.
const CLAUDE_BIN_ENV: &str = "MAXWELLS_CLAUDE_BIN";

/// Grace period for `child.wait()` after the stream closes. Covers normal
/// cleanup; exceeded → kill and continue with whatever was parsed.
const WAIT_GRACE_SECS: Duration = Duration::from_secs(30);

/// Storage cap: recorded config `content` is truncated to this many bytes in
/// the trajectory (a pathological file is flagged `truncated` rather than
/// bloating the artifact).
const CONFIG_MAX_FILE_BYTES: usize = 256 * 1024;
/// Hard cap on bytes read into memory per file. Bounds memory even for a
/// multi-GB asset dropped under `.claude/` — the hash is still computed over the
/// *whole* file by streaming, so the fingerprint stays faithful, but only this
/// much is buffered for content/redaction.
const CONFIG_MAX_READ_BYTES: usize = 1024 * 1024;
/// Cap on the number of discovered config files recorded, across all scopes.
const CONFIG_MAX_FILES: usize = 200;

/// One Claude Code config file the harness found in play for this run.
struct DiscoveredFile {
    /// Filesystem path, for display/audit (redacted before it is recorded).
    display_path: String,
    /// `"project"`, `"ancestor"` (a `CLAUDE.md` above the workdir), or `"user"`.
    scope: &'static str,
    /// `"CLAUDE.md"`, `"AGENTS.md"`, `"settings"`, `"agent"`, `"skill"`,
    /// `"rule"`, or `"mcp"`.
    kind: &'static str,
    /// SHA-256 of the *whole* on-disk file (streamed) — a tamper-evident
    /// fingerprint independent of the read window or redaction below.
    sha256: String,
    /// Full byte length of the on-disk file.
    bytes: u64,
    /// Up to [`CONFIG_MAX_READ_BYTES`] of raw content; redacted then truncated
    /// to [`CONFIG_MAX_FILE_BYTES`] before it is recorded.
    content: String,
    /// True when the file is larger than [`CONFIG_MAX_FILE_BYTES`] (the recorded
    /// content is a prefix, not the whole file).
    truncated: bool,
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary, returning the
/// (possibly shorter) string and whether it was cut.
fn truncate_to_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_owned(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_owned(), true)
}

fn sha256_finish_hex(hasher: Sha256) -> String {
    use std::fmt::Write as _;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Read one config file into a [`DiscoveredFile`], or `None` if unreadable.
///
/// Reads in fixed chunks: the hash covers the whole file (constant memory),
/// while only the first [`CONFIG_MAX_READ_BYTES`] are retained for content so a
/// huge/symlinked asset can't allocate unbounded memory.
fn read_config_file(
    path: &Path,
    scope: &'static str,
    kind: &'static str,
) -> Option<DiscoveredFile> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    let mut total: u64 = 0;
    let mut content_bytes: Vec<u8> = Vec::new();
    loop {
        let n = reader.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        if content_bytes.len() < CONFIG_MAX_READ_BYTES {
            let take = (CONFIG_MAX_READ_BYTES - content_bytes.len()).min(n);
            content_bytes.extend_from_slice(&buf[..take]);
        }
    }
    Some(DiscoveredFile {
        display_path: path.display().to_string(),
        scope,
        kind,
        sha256: sha256_finish_hex(hasher),
        bytes: total,
        content: String::from_utf8_lossy(&content_bytes).into_owned(),
        truncated: total > CONFIG_MAX_FILE_BYTES as u64,
    })
}

/// Recursively collect files under `dir` (best-effort), honoring the global cap.
/// Symlinks are skipped (via `DirEntry::file_type`, which does not follow them)
/// so a symlinked entry can't redirect the walk outside the worktree.
fn collect_config_dir(
    dir: &Path,
    scope: &'static str,
    kind: &'static str,
    out: &mut Vec<DiscoveredFile>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= CONFIG_MAX_FILES {
            return;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_config_dir(&path, scope, kind, out);
        } else if ft.is_file() {
            if let Some(f) = read_config_file(&path, scope, kind) {
                out.push(f);
            }
        }
    }
}

/// True only for a *regular* file that is not itself a symlink. Uses
/// `symlink_metadata` so a singleton config path (e.g. `.claude/CLAUDE.md`)
/// that an untrusted worktree points outside the repo is not followed and
/// recorded — matching the symlink-skipping recursive walk.
fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Discover the Claude Code configuration that Claude Code will auto-discover at
/// `cwd` (project scope) and under `~/.claude` (user scope). This mirrors what
/// the CLI loads in fidelity mode so the audit record reflects what was in play.
fn discover_claude_config(cwd: &Path) -> Vec<DiscoveredFile> {
    let mut out = Vec::new();

    // Project scope: singleton files at the workdir root and under `.claude/`,
    // then the agent/skill/rule dirs. Mirrors what Claude Code loads as project
    // memory + config (https://code.claude.com/docs/en/memory).
    let project_files: &[(&str, &'static str)] = &[
        ("CLAUDE.md", "CLAUDE.md"),
        ("CLAUDE.local.md", "CLAUDE.md"),
        (".claude/CLAUDE.md", "CLAUDE.md"),
        ("AGENTS.md", "AGENTS.md"),
        (".claude/settings.json", "settings"),
        (".claude/settings.local.json", "settings"),
        (".mcp.json", "mcp"),
    ];
    for (rel, kind) in project_files {
        let p = cwd.join(rel);
        if is_regular_file(&p) {
            if let Some(f) = read_config_file(&p, "project", kind) {
                out.push(f);
            }
        }
    }
    collect_config_dir(&cwd.join(".claude/agents"), "project", "agent", &mut out);
    collect_config_dir(&cwd.join(".claude/skills"), "project", "skill", &mut out);
    collect_config_dir(&cwd.join(".claude/rules"), "project", "rule", &mut out);

    // Ancestor scope: Claude Code loads `CLAUDE.md`/`CLAUDE.local.md` from the
    // directory hierarchy above the workdir, so record those too (the global cap
    // bounds a deep tree).
    let mut ancestor = cwd.parent();
    while let Some(dir) = ancestor {
        if out.len() >= CONFIG_MAX_FILES {
            break;
        }
        for name in ["CLAUDE.md", "CLAUDE.local.md"] {
            let p = dir.join(name);
            if is_regular_file(&p) {
                if let Some(f) = read_config_file(&p, "ancestor", "CLAUDE.md") {
                    out.push(f);
                }
            }
        }
        ancestor = dir.parent();
    }

    // User scope: ~/.claude singletons plus agent/skill dirs.
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let claude = PathBuf::from(home).join(".claude");
        let user_files: &[(&str, &'static str)] =
            &[("CLAUDE.md", "CLAUDE.md"), ("settings.json", "settings")];
        for (rel, kind) in user_files {
            let p = claude.join(rel);
            if is_regular_file(&p) {
                if let Some(f) = read_config_file(&p, "user", kind) {
                    out.push(f);
                }
            }
        }
        collect_config_dir(&claude.join("agents"), "user", "agent", &mut out);
        collect_config_dir(&claude.join("skills"), "user", "skill", &mut out);
    }
    out
}

/// Overwrite `info.other["toolset"]` with the toolset actually exposed to Claude
/// Code. `DefaultAgentBuilder` stamps the harness `ToolRegistry` manifest (just
/// `bash` once the driver's rejection guards run), which misrepresents what the
/// CLI could call — so tool-coverage/drift reports would treat `Edit`/`Write`/
/// `Read`/etc. as unavailable.
///
/// In isolated mode `--tools` restricts the CLI to `ALLOWED_TOOLS`, so that is
/// authoritative. In fidelity mode the CLI also exposes its default and ambient
/// (`.claude`/plugin/MCP) tools, so prefer the actual list from the stream's
/// `system/init` message when available, falling back to `ALLOWED_TOOLS`.
fn record_driver_toolset(agent: &mut DefaultAgent, isolated: bool, init_tools: Option<&[String]>) {
    let names: Vec<String> = if isolated {
        ALLOWED_TOOLS.iter().map(|s| (*s).to_owned()).collect()
    } else {
        init_tools.filter(|t| !t.is_empty()).map_or_else(
            || ALLOWED_TOOLS.iter().map(|s| (*s).to_owned()).collect(),
            <[String]>::to_vec,
        )
    };
    let tools: Vec<Value> = names
        .iter()
        .map(|name| {
            serde_json::json!({
                "name": name,
                "description": "Claude Code CLI tool",
                "source": "claude_code",
            })
        })
        .collect();
    agent
        .trajectory
        .info
        .other
        .insert("toolset".into(), serde_json::json!({ "tools": tools }));
}

/// Record, under `info.other["claude_code_config"]`, the Claude Code config that
/// shaped this run. In fidelity mode the ambient config is live, so its files
/// (path + scope + kind + raw-bytes hash + redacted content) are captured for
/// audit. In isolated mode `--bare` bypasses discovery, so only a marker is
/// recorded. Content is redacted with the `TRAJECTORY` surface before storage;
/// the hash is of the raw bytes so tampering is still detectable.
fn record_claude_config(agent: &mut DefaultAgent, cwd: &Path, isolated: bool) {
    let mut meta = serde_json::Map::new();
    meta.insert("isolated".into(), Value::Bool(isolated));
    if isolated {
        meta.insert(
            "discovery".into(),
            Value::String("bypassed (--bare strips ambient .claude config)".into()),
        );
        agent
            .trajectory
            .info
            .other
            .insert("claude_code_config".into(), Value::Object(meta));
        return;
    }

    let mut files = Vec::new();
    let mut truncated_any = false;
    for f in discover_claude_config(cwd)
        .into_iter()
        .take(CONFIG_MAX_FILES)
    {
        // Redact path and content on the TRAJECTORY surface — a workdir, $HOME,
        // or filename segment can itself contain a configured secret literal.
        let path = agent
            .redactor
            .redact_text(&f.display_path, surface::TRAJECTORY)
            .text;
        // Redact the *full* read window before applying the storage cap, so a
        // secret straddling the truncation boundary is still redacted (matching
        // the observation path's redact-then-truncate order).
        let redacted = agent
            .redactor
            .redact_text(&f.content, surface::TRAJECTORY)
            .text;
        let (content, content_truncated) = truncate_to_bytes(&redacted, CONFIG_MAX_FILE_BYTES);
        let truncated = f.truncated || content_truncated;

        let mut obj = serde_json::Map::new();
        obj.insert("path".into(), Value::String(path));
        obj.insert("scope".into(), Value::String(f.scope.into()));
        obj.insert("kind".into(), Value::String(f.kind.into()));
        obj.insert("sha256".into(), Value::String(f.sha256));
        obj.insert("bytes".into(), Value::Number(f.bytes.into()));
        if truncated {
            obj.insert("truncated".into(), Value::Bool(true));
            truncated_any = true;
        }
        obj.insert("content".into(), Value::String(content));
        files.push(Value::Object(obj));
    }
    meta.insert(
        "file_count".into(),
        Value::Number((files.len() as u64).into()),
    );
    if truncated_any {
        meta.insert("truncated".into(), Value::Bool(true));
    }
    meta.insert("files".into(), Value::Array(files));
    agent
        .trajectory
        .info
        .other
        .insert("claude_code_config".into(), Value::Object(meta));
}

/// Drive a single run through the Claude Code CLI, filling `agent.trajectory`
/// and returning the terminal [`ExitReason`].
///
/// `workdir` is the directory `claude` runs in (and where edits land). When
/// `None`, the current process directory is used — matching the built-in
/// local environment's behavior.
///
/// `append_system_prompt` is passed verbatim to `claude --append-system-prompt`
/// when `Some`. Callers should supply the rendered operator system prompt only
/// when it has been authored for Claude Code (the built-in default contains
/// harness-protocol text that Claude Code doesn't use).
/// `isolated` selects the spawn posture:
/// - `false` (default — *fidelity*): run Claude Code as the team really does —
///   OAuth/keychain auth, ambient `.claude` discovery (hooks, skills, plugins,
///   MCP, memory, `CLAUDE.md`), native session persistence, and the team's own
///   permission settings. The harness *records* the discovered config (see
///   [`discover_claude_config`]) into the trajectory so the run is auditable
///   without being sterilized. This is the enterprise-auditing case.
/// - `true` (*isolation*): pass `--bare` (skip ambient discovery), `--tools`
///   (restrict to [`ALLOWED_TOOLS`]), and `--no-session-persistence` for a
///   reproducible measurement run. Note: `--bare` forces API-key-only auth, so
///   OAuth/keychain logins do not apply in this mode.
#[allow(clippy::too_many_lines)]
pub async fn drive(
    agent: &mut DefaultAgent,
    task: String,
    extra_context: Option<&str>,
    workdir: Option<&Path>,
    timeout_secs: Option<u64>,
    append_system_prompt: Option<&str>,
    isolated: bool,
) -> Result<ExitReason, Error> {
    let bin = std::env::var(CLAUDE_BIN_ENV).unwrap_or_else(|_| "claude".to_owned());
    let cwd = match workdir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let step_limit = agent.config.root.agent.step_limit;

    if agent.config.root.model.name != "claude-opus-4-7" {
        // The default is just clap's placeholder; only warn when the operator
        // actively set a model, since we don't forward it to Claude Code.
        tracing::warn!(
            model = %agent.config.root.model.name,
            "--driver claude-code delegates model selection to Claude Code; \
             --model is not forwarded"
        );
    }

    // Fold any merged extra-context / active-skill guidance into the prompt so
    // the backend actually sees the context the trajectory claims was present.
    let prompt = match extra_context {
        Some(ctx) if !ctx.trim().is_empty() => format!("{task}\n\n{ctx}"),
        _ => task.clone(),
    };

    // Honor the operator's configured spend cap by forwarding it to Claude
    // Code's own `--max-budget-usd`; an over-budget result is also downgraded
    // post-hoc in `finalize` so spend controls hold even if the cap is fuzzy.
    let cost_cap = [
        agent.config.root.agent.cost_limit_usd,
        agent.config.root.agent.per_task_budget_usd,
    ]
    .into_iter()
    .flatten()
    .min_by(f64::total_cmp);

    // Cancellation token (sweep Ctrl-C) shared by the agent; cloned so we can
    // race it against the child without holding a borrow on `agent`.
    let mut cancel = agent.cancellation.clone();

    // If cancellation already landed during setup (e.g. a sweep Ctrl-C before the
    // driver started), return the interrupt before spawning so the external CLI
    // never touches the worktree — matching DefaultAgent::step, which returns
    // UserInterrupt before any tool execution when already cancelled.
    if cancel
        .as_ref()
        .is_some_and(crate::env::CancellationToken::is_cancelled)
    {
        return Ok(ExitReason::UserInterrupt);
    }

    // Audit the Claude Code configuration that will actually shape this run.
    // In fidelity mode ambient `.claude` config is live, so discover and record
    // it (paths + hashes + redacted content). In isolated mode `--bare` strips
    // it, so record only the marker that discovery was bypassed.
    record_claude_config(agent, &cwd, isolated);
    // Baseline toolset (refined from system/init in finalize for fidelity mode);
    // ensures even an interrupted/timed-out run records the driver's toolset
    // rather than the stale harness manifest.
    record_driver_toolset(agent, isolated, None);

    let mut cmd = Command::new(&bin);
    cmd.kill_on_drop(true)
        .arg("-p")
        .arg(&prompt)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--max-turns")
        .arg(step_limit.to_string())
        .arg("--allowedTools")
        .args(ALLOWED_TOOLS);
    if isolated {
        // Reproducible measurement: strip ambient `.claude` discovery, restrict
        // the available toolset (not just auto-approval), and keep prompts out
        // of Claude Code's on-disk session history. `--bare` makes auth strictly
        // ANTHROPIC_API_KEY/apiKeyHelper (no OAuth/keychain).
        cmd.arg("--bare")
            .arg("--no-session-persistence")
            // `--tools` takes a single comma-separated value (unlike
            // `--allowedTools`, which accepts a space-separated list).
            .arg("--tools")
            .arg(ALLOWED_TOOLS.join(","));
    }
    if let Some(cap) = cost_cap {
        cmd.arg("--max-budget-usd").arg(format!("{cap}"));
    }
    if let Some(prompt) = append_system_prompt {
        cmd.arg("--append-system-prompt").arg(prompt);
    }
    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    tracing::info!(
        bin = %bin,
        cwd = %cwd.display(),
        max_turns = step_limit,
        append_system_prompt = append_system_prompt.is_some(),
        isolated,
        "spawning Claude Code driver"
    );

    let mut child = cmd.spawn().map_err(|e| {
        Error::Trajectory(format!(
            "failed to spawn `{bin}` for --driver claude-code: {e}"
        ))
    })?;

    // Drain stderr concurrently so a chatty child can never deadlock us. Read
    // in fixed-size chunks so a single very-long line (no newline) cannot
    // allocate beyond the cap before the loop body runs. Keep reading to EOF
    // even after the cap to avoid SIGPIPE on a closed read end.
    let stderr_handle = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            const CAP: usize = 1024 * 1024;
            let mut retained: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            let mut stderr = stderr;
            loop {
                match stderr.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if retained.len() < CAP {
                            let take = (CAP - retained.len()).min(n);
                            retained.extend_from_slice(&tmp[..take]);
                        }
                        // Continue reading (and discarding) past the cap so
                        // the child's write end is never blocked.
                    }
                }
            }
            String::from_utf8_lossy(&retained).into_owned()
        })
    });

    // Take stdout out of the child so `process_stream` does not hold a borrow
    // on `child` — that lets the timeout/cancel branches kill it.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Trajectory("claude child has no stdout pipe".into()))?;
    let timeout_dur = timeout_secs.map(Duration::from_secs);
    let run_start = Instant::now();

    // Race the stream against the optional wallclock timeout and the
    // operator's cancellation token (sweep Ctrl-C). The branches yield an
    // owned `Outcome` so none of them borrows `agent` during the select; the
    // only agent borrow is the `process` future, dropped immediately after.
    //
    // Scope the borrowing `process` future so it is dropped at the block's
    // end, releasing the `&mut agent` borrow before the handlers below use it.
    let outcome = {
        let process = process_stream(agent, stdout);
        tokio::pin!(process);
        tokio::select! {
            res = &mut process => Outcome::Stream(Box::new(res)),
            () = async {
                match timeout_dur {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => Outcome::Timeout,
            () = async {
                match cancel.as_mut() {
                    Some(c) => c.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => Outcome::Cancelled,
        }
    };

    let parsed = match outcome {
        Outcome::Stream(res) => (*res)?,
        Outcome::Timeout => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let dur = timeout_dur.unwrap_or_default();
            agent.finalize_wallclock_timeout(dur);
            return Err(Error::Trajectory(format!(
                "task wallclock timeout after {}s (claude driver)",
                dur.as_secs()
            )));
        }
        Outcome::Cancelled => {
            // Kill the child and let `mini::run`'s cancellation finalizer
            // stamp the trajectory, matching the built-in interrupt path.
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(ExitReason::UserInterrupt);
        }
    };

    // Wait for the child to exit, bounded by the *remaining* task budget (not
    // the original duration) so a CLI that closes stdout near the end of its
    // timeout window cannot extend the run beyond the configured cap. The grace
    // window is further capped to WAIT_GRACE_SECS so normal, fast cleanup does
    // not consume the full remaining budget.
    let wait_deadline = timeout_dur.map_or(WAIT_GRACE_SECS, |d| {
        d.saturating_sub(run_start.elapsed()).min(WAIT_GRACE_SECS)
    });
    let exit_code = if let Ok(Ok(status)) = tokio::time::timeout(wait_deadline, child.wait()).await
    {
        status.code()
    } else {
        // Timed out or error during wait; kill the process and continue.
        let _ = child.start_kill();
        None
    };

    let stderr_text = match stderr_handle {
        Some(h) => h.await.unwrap_or_default(),
        None => String::new(),
    };

    finalize(agent, parsed, step_limit, exit_code, &stderr_text, isolated)
}

/// How the streaming race resolved: the stream finished, or the timeout /
/// cancellation token fired first.
enum Outcome {
    Stream(Box<Result<Parsed, Error>>),
    Timeout,
    Cancelled,
}

/// A test command seen in a `tool_use` block, awaiting its `tool_result` so
/// the pass/fail outcome can be paired with it by `tool_use_id`.
struct PendingTest {
    command: String,
    step_index: u32,
    matched_pattern: String,
}

/// Aggregated state pulled out of the Claude Code stream.
#[derive(Default)]
struct Parsed {
    /// Number of tool invocations — the closest analog to "agent steps".
    steps: u32,
    /// Model name Claude Code actually used (from `system/init` or `result`).
    model: Option<String>,
    /// Session id, recorded for provenance.
    session_id: Option<String>,
    claude_version: Option<String>,
    /// Tool names Claude Code reported available in `system/init` (used to
    /// record the real fidelity-mode toolset).
    tools: Option<Vec<String>>,
    /// Terminal `result` message, if one was emitted.
    result: Option<ResultMsg>,
    /// Test commands (`pytest`, etc.) awaiting their result, keyed by
    /// `tool_use_id`.
    pending_tests: HashMap<String, PendingTest>,
    /// Completed test invocations, paired with their result exit status.
    test_invocations: Vec<TestInvocation>,
}

struct ResultMsg {
    subtype: String,
    is_error: bool,
    final_text: String,
    total_cost_usd: f64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    output_tokens: u64,
}

/// Read the NDJSON stream from the child's `stdout`, recording assistant turns
/// and tool observations into the trajectory as they arrive.
async fn process_stream(
    agent: &mut DefaultAgent,
    stdout: tokio::process::ChildStdout,
) -> Result<Parsed, Error> {
    // Patterns are already validated at agent build; fall back to empty on the
    // unlikely recompile error rather than aborting the run.
    let test_patterns = effective_test_command_patterns(
        &agent.config.root.agent.test_command_patterns,
        agent.config.root.agent.test_command_patterns_replace,
    )
    .unwrap_or_default();

    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = Parsed::default();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            // Non-JSON noise on stdout (shouldn't happen in stream-json) — skip.
            continue;
        };
        match msg.get("type").and_then(Value::as_str) {
            Some("system") => handle_system(&mut parsed, &msg),
            Some("assistant") => handle_assistant(agent, &mut parsed, &msg, &test_patterns),
            Some("user") => {
                handle_user(agent, &mut parsed, &msg);
                // Per-step partial checkpoint after each observation, so an
                // interrupted long run can be inspected/resumed from here.
                maybe_checkpoint(agent, parsed.steps);
            }
            Some("result") => parsed.result = Some(parse_result(&msg)),
            _ => {} // rate_limit_event, stream_event, etc. — ignored.
        }
    }

    Ok(parsed)
}

/// Atomically persist a `partial: true` checkpoint when a checkpoint path is
/// configured, mirroring the built-in loop's per-turn write.
fn maybe_checkpoint(agent: &mut DefaultAgent, steps: u32) {
    if let Some(path) = agent.checkpoint_path.clone() {
        agent.trajectory.info.steps = Some(steps);
        agent.trajectory.info.actual_cost_usd = Some(agent.total_cost_usd);
        if let Err(e) = agent.trajectory.save_partial_atomic(&path) {
            tracing::warn!(error = %e, "claude driver checkpoint write failed; continuing");
        }
    }
}

fn handle_system(parsed: &mut Parsed, msg: &Value) {
    if msg.get("subtype").and_then(Value::as_str) == Some("init") {
        parsed.session_id = msg
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        parsed.model = msg
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        parsed.claude_version = msg
            .get("claude_code_version")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        parsed.tools = msg.get("tools").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(ToOwned::to_owned))
                .collect()
        });
    }
}

/// Record one assistant turn: join text blocks into the message content,
/// fold thinking into `extra.other`, and capture each `tool_use` as an action.
fn handle_assistant(
    agent: &mut DefaultAgent,
    parsed: &mut Parsed,
    msg: &Value,
    test_patterns: &[TestCommandPattern],
) {
    let Some(blocks) = msg
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut actions: Vec<String> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(t.to_owned());
                }
            }
            Some("thinking") => {
                if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                    thinking_parts.push(t.to_owned());
                }
            }
            Some("tool_use") => {
                parsed.steps += 1;
                // Mirror the count onto the agent so an interrupted run
                // (timeout/cancel before the result) still records its steps.
                agent.steps = parsed.steps;
                // Track Bash test commands so the following tool_result can be
                // paired into pre-submit test telemetry.
                if let (Some(id), Some("Bash")) = (
                    block.get("id").and_then(Value::as_str),
                    block.get("name").and_then(Value::as_str),
                ) {
                    if let Some(cmd) = block
                        .get("input")
                        .and_then(|i| i.get("command"))
                        .and_then(Value::as_str)
                    {
                        if let Some(matched) = detect_test_command(cmd, test_patterns) {
                            parsed.pending_tests.insert(
                                id.to_owned(),
                                PendingTest {
                                    command: cmd.to_owned(),
                                    // steps was just incremented; subtract 1 so the index
                                    // is zero-based, matching the built-in loop's convention.
                                    step_index: parsed.steps - 1,
                                    matched_pattern: matched,
                                },
                            );
                        }
                    }
                }
                actions.push(tool_action_label(block));
            }
            _ => {}
        }
    }

    if let Some(m) = msg
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
    {
        parsed.model = Some(m.to_owned());
    }

    // Skip only completely-empty turns. A thinking-only turn is still
    // recorded (empty content + `extra.thinking`) so the reasoning receipt
    // survives in the trajectory.
    if text_parts.is_empty() && actions.is_empty() && thinking_parts.is_empty() {
        return;
    }

    let content = text_parts.join("\n");
    let redacted = agent
        .redactor
        .redact_text(&content, surface::MODEL_OBSERVATION)
        .text;

    let ts = chrono::Utc::now().to_rfc3339();
    let mut extra = MessageExtra {
        timestamp: Some(ts.clone()),
        ..Default::default()
    };
    // Representative label for the bash lifecycle event emitted after the
    // assistant message (set only when the turn issued tools).
    let mut bash_label: Option<String> = None;
    if !actions.is_empty() {
        // Redact action labels (bash commands, file paths) on the trajectory
        // surface, matching the built-in loop — a tool call can carry a
        // configured literal, token, or secret-bearing path.
        for action in &mut actions {
            *action = agent.redactor.redact_text(action, surface::TRAJECTORY).text;
        }
        bash_label = tool_bash_label(&actions);
        extra.actions = Some(actions);
    }
    if !thinking_parts.is_empty() {
        let thinking = agent
            .redactor
            .redact_text(&thinking_parts.join("\n"), surface::MODEL_OBSERVATION)
            .text;
        extra
            .other
            .insert("thinking".into(), Value::String(thinking));
    }

    agent
        .trajectory
        .record_with_extra(&Message::assistant(redacted.clone()), extra);
    // Emit the per-step assistant event so live consumers (--event-log,
    // --webhook-url, --stream-addr) see activity, as the built-in loop does.
    agent.stream.emit(StreamEvent::AssistantMessage {
        step: parsed.steps,
        content: redacted,
        cost_usd: None,
        timestamp: ts.clone(),
    });
    emit_tool_bash_start(agent, parsed.steps, bash_label, ts);
}

/// Representative footer label for a turn's tool calls, used by the BashStart
/// lifecycle event so an activity-inferring dashboard (issue #649) shows the
/// in-flight tool. `None` when the turn issued no tools — a text/thinking-only
/// turn is genuinely idle. Expects already-redacted action labels.
fn tool_bash_label(actions: &[String]) -> Option<String> {
    match actions {
        [] => None,
        [only] => Some(only.clone()),
        [first, rest @ ..] => Some(format!("{first} (+{} more)", rest.len())),
    }
}

/// Emit the BashStart half of the tool lifecycle for a Claude-driver turn so
/// activity-inferring dashboards (issue #649) render the in-flight tool instead
/// of a static idle footer. The matching BashResult is emitted from
/// `handle_user`. The driver stream is not wrapped in RedactingSink, so `label`
/// must already be redacted.
fn emit_tool_bash_start(agent: &DefaultAgent, step: u32, label: Option<String>, ts: String) {
    if let Some(command) = label {
        agent.stream.emit(StreamEvent::BashStart {
            step,
            command,
            timestamp: ts,
        });
    }
}

/// Build a one-line action label for a `tool_use` block, mirroring the
/// built-in loop's `extra.actions` convention (bash command verbatim; other
/// tools as `Name(target)`).
fn tool_action_label(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
    let input = block.get("input");
    if name == "Bash" {
        if let Some(cmd) = input.and_then(|i| i.get("command")).and_then(Value::as_str) {
            return cmd.to_owned();
        }
    }
    if let Some(path) = input
        .and_then(|i| i.get("file_path").or_else(|| i.get("path")))
        .and_then(Value::as_str)
    {
        return format!("{name}({path})");
    }
    if let Some(pattern) = input.and_then(|i| i.get("pattern")).and_then(Value::as_str) {
        return format!("{name}({pattern})");
    }
    name.to_owned()
}

/// Record a `tool_result` (delivered as a `user` message) as an observation,
/// and pair any test-command results into pre-submit telemetry.
fn handle_user(agent: &mut DefaultAgent, parsed: &mut Parsed, msg: &Value) {
    let Some(blocks) = msg
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    // Parallel tool calls arrive as several `tool_result` blocks in one
    // `user` message; combine them into a single observation so the
    // trajectory keeps its alternating assistant/user shape.
    let mut parts: Vec<String> = Vec::new();
    let mut has_error = false;
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let block_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        has_error |= block_error;
        // Pair this result with a pending test command by tool_use_id. Claude
        // Code's Bash tool_result reports `is_error`, not an exit code, so map
        // success → 0 / failure → 1.
        if let Some(pending) = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(|id| parsed.pending_tests.remove(id))
        {
            let command = agent
                .redactor
                .redact_text(&pending.command, surface::TRAJECTORY)
                .text;
            parsed.test_invocations.push(TestInvocation {
                step_index: pending.step_index,
                command,
                exit_code: i32::from(block_error),
                matched_pattern: pending.matched_pattern,
            });
        }
        parts.push(tool_result_text(block.get("content")));
    }
    if parts.is_empty() {
        return;
    }
    // Redact the full combined text before truncating so that configured
    // literals spanning the head/tail boundary are not split and missed.
    // Matches the built-in loop's redact-then-truncate order.
    let raw = parts.join("\n\n");
    let redacted_raw = agent
        .redactor
        .redact_text(&raw, surface::MODEL_OBSERVATION)
        .text;
    let redacted = truncate_observation_text(
        &redacted_raw,
        agent.config.root.agent.observation_max_bytes,
        agent.config.root.agent.observation_head_ratio,
    )
    .text;
    let ts = chrono::Utc::now().to_rfc3339();
    let mut extra = MessageExtra {
        timestamp: Some(ts.clone()),
        ..Default::default()
    };
    if has_error {
        extra.other.insert("tool_error".into(), Value::Bool(true));
    }
    // Synthesize the `run_result` object the built-in loop writes for every
    // command, so consumers that key off it (`bench inspect`, command stats,
    // output-byte telemetry) treat driver observations the same. The exit code
    // is derived from the tool_result's `is_error`; output is the redacted text.
    let run_result = crate::env::RunResult {
        stdout: redacted_raw.clone(),
        stderr: String::new(),
        exit_code: i32::from(has_error),
        timed_out: false,
    };
    extra.other.insert(
        "run_result".into(),
        serde_json::to_value(&run_result).unwrap_or(Value::Null),
    );
    agent
        .trajectory
        .record_with_extra(&Message::user(redacted.clone()), extra);
    // Close the bash lifecycle opened in `handle_assistant` so the dashboard's
    // activity inference (issue #649) leaves the running state before the
    // observation reopens the thinking window. Claude Code's tool_result carries
    // `is_error`, not an exit code, so map it to 0/1. The driver stream is not
    // wrapped in RedactingSink, so emit the already-redacted output.
    agent.stream.emit(StreamEvent::BashResult {
        step: parsed.steps,
        exit_code: i32::from(has_error),
        stdout: redacted_raw,
        stderr: String::new(),
        timed_out: false,
        timestamp: ts.clone(),
    });
    // Emit the per-step observation event for live consumers, mirroring the
    // built-in loop's post-bash Observation event.
    agent.stream.emit(StreamEvent::Observation {
        step: parsed.steps,
        content: redacted,
        timestamp: ts,
    });
}

/// Flatten a `tool_result` `content` field, which is either a string or an
/// array of `{type:"text", text}` blocks.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_result(msg: &Value) -> ResultMsg {
    let usage = msg.get("usage");
    let u = |k: &str| -> u64 {
        usage
            .and_then(|x| x.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    ResultMsg {
        subtype: msg
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        is_error: msg
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        final_text: msg
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        total_cost_usd: msg
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        input_tokens: u("input_tokens"),
        cache_read_tokens: u("cache_read_input_tokens"),
        cache_creation_tokens: u("cache_creation_input_tokens"),
        output_tokens: u("output_tokens"),
    }
}

/// Stamp the trajectory with cost/tokens/outcome from the parsed stream and
/// return the terminal [`ExitReason`], mirroring `DefaultAgent`'s own
/// finalization on each terminal path.
#[allow(clippy::too_many_lines)]
fn finalize(
    agent: &mut DefaultAgent,
    mut parsed: Parsed,
    step_limit: u32,
    exit_code: Option<i32>,
    stderr_text: &str,
    isolated: bool,
) -> Result<ExitReason, Error> {
    // Refine the recorded toolset now that the stream's system/init tool list is
    // known (fidelity mode); isolated mode stays pinned to ALLOWED_TOOLS.
    record_driver_toolset(agent, isolated, parsed.tools.as_deref());
    // Provenance: record the Claude Code session so the trajectory is
    // self-describing about which backend produced it.
    if let Some(model) = parsed.model.clone() {
        agent.trajectory.info.model_name = Some(model);
    }
    let mut driver_meta = serde_json::Map::new();
    driver_meta.insert("driver".into(), Value::String("claude-code".into()));
    if let Some(s) = parsed.session_id {
        driver_meta.insert("session_id".into(), Value::String(s));
    }
    if let Some(v) = parsed.claude_version {
        driver_meta.insert("claude_code_version".into(), Value::String(v));
    }
    agent
        .trajectory
        .info
        .other
        .insert("claude_driver".into(), Value::Object(driver_meta));

    let Some(result) = parsed.result else {
        // No terminal result line: the CLI crashed or was killed. Surface
        // stderr (redacted) and let mini::run finalize as an error.
        let snippet = agent
            .redactor
            .redact_text(stderr_text.trim(), surface::TRAJECTORY)
            .text;
        return Err(Error::Trajectory(format!(
            "claude driver produced no result message (exit {:?}): {}",
            exit_code,
            truncate(&snippet, 500)
        )));
    };

    // Cost and tokens are authoritative from the result message.
    agent.steps = parsed.steps;
    agent.total_cost_usd = result.total_cost_usd;
    agent.actual_cost_source = Some(CostSource::ProviderReported);
    agent.prompt_tokens = result.input_tokens;
    agent.cache_read_tokens = result.cache_read_tokens;
    agent.cache_creation_tokens = result.cache_creation_tokens;
    agent.completion_tokens = result.output_tokens;

    agent.trajectory.info.steps = Some(parsed.steps);
    agent.trajectory.info.total_cost_usd = Some(result.total_cost_usd);
    agent.trajectory.info.ended_at = Some(chrono::Utc::now().to_rfc3339());
    agent.trajectory.info.token_usage = Some(TokenUsage {
        prompt_tokens: result.input_tokens,
        cache_read_tokens: result.cache_read_tokens,
        cache_creation_tokens: result.cache_creation_tokens,
        completion_tokens: result.output_tokens,
    });

    // Record pre-submit test telemetry. `refresh_test_metadata` (called inside
    // `finalize_run_metadata`) keys `tests_run_before_submit` off a `__SUBMIT__`
    // action message the driver never emits, so capture the pass/fail signal
    // here and stamp it explicitly on the submit path below.
    let ran_tests = !parsed.test_invocations.is_empty();
    let last_tests_passed = parsed.test_invocations.last().map(|t| t.exit_code == 0);
    agent.trajectory.info.test_invocations = std::mem::take(&mut parsed.test_invocations);

    let is_max_turns = result.subtype.contains("max_turns");

    // The configured spend caps are forwarded to Claude Code as
    // `--max-budget-usd`, but enforce them post-hoc too so spend controls
    // hold even if the CLI overshoots on its final turn.
    //
    // Mirror the built-in loop's priority order: `cost_limit_usd` fires first
    // (records `CostLimit`); `per_task_budget_usd` fires only when
    // `cost_limit_usd` is unset or not yet reached (records `BudgetExhausted`).
    let cost_limit_exceeded = agent
        .config
        .root
        .agent
        .cost_limit_usd
        .is_some_and(|cap| result.total_cost_usd >= cap);
    let budget_exhausted = !cost_limit_exceeded
        && agent
            .config
            .root
            .agent
            .per_task_budget_usd
            .is_some_and(|cap| result.total_cost_usd >= cap);

    // Claude Code's `--max-turns` bounds *agentic turns*, but the harness step
    // cap counts *tool_use* blocks — and a single turn can emit several. If the
    // parsed tool-use count exceeded the configured limit, downgrade the outcome
    // to step_limit so a run can never report `submitted` while having taken more
    // tool actions than the cap advertised.
    let step_overflow = parsed.steps > step_limit;

    if result.subtype == "success"
        && !result.is_error
        && !cost_limit_exceeded
        && !budget_exhausted
        && !step_overflow
    {
        let final_output = agent
            .redactor
            .redact_text(&result.final_text, surface::TRAJECTORY)
            .text;
        agent.trajectory.info.exit_reason = Some("submitted".into());
        agent.trajectory.info.failure_category = None;
        agent.trajectory.info.final_output = Some(final_output.clone());
        agent.finalize_run_metadata(outcome::SUBMITTED);
        // All driver test invocations precede the terminal submission, so stamp
        // pre-submit telemetry directly (refresh_test_metadata cleared it).
        if ran_tests {
            agent.trajectory.info.tests_run_before_submit = true;
            agent.trajectory.info.last_tests_passed = last_tests_passed;
        }
        emit_ended(agent, "submitted", None, Some(final_output.clone()));
        Ok(ExitReason::Submitted { final_output })
    } else if cost_limit_exceeded {
        let limit_usd = agent.config.root.agent.cost_limit_usd.unwrap_or(0.0);
        agent.trajectory.info.exit_reason = Some("cost_limit".into());
        agent.trajectory.info.failure_category = Some(FailureCategory::CostLimit);
        // cost_limit maps to the same coarse outcome as step_limit per the spec.
        agent.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
        emit_ended(agent, "cost_limit", Some(FailureCategory::CostLimit), None);
        Ok(ExitReason::CostLimit {
            limit_usd,
            spent_usd: result.total_cost_usd,
        })
    } else if budget_exhausted {
        let limit_usd = agent.config.root.agent.per_task_budget_usd.unwrap_or(0.0);
        agent.trajectory.info.exit_reason = Some("budget_exhausted".into());
        agent.trajectory.info.failure_category = Some(FailureCategory::BudgetExhausted);
        agent.finalize_run_metadata(outcome::BUDGET_EXHAUSTED);
        emit_ended(
            agent,
            "budget_exhausted",
            Some(FailureCategory::BudgetExhausted),
            None,
        );
        Ok(ExitReason::BudgetExhausted {
            limit_usd,
            spent_usd: result.total_cost_usd,
        })
    } else if is_max_turns || step_overflow {
        agent.trajectory.info.exit_reason = Some("step_limit".into());
        agent.trajectory.info.failure_category = Some(FailureCategory::StepLimit);
        agent.finalize_run_metadata(outcome::STEP_LIMIT_REACHED);
        emit_ended(agent, "step_limit", Some(FailureCategory::StepLimit), None);
        Ok(ExitReason::StepLimit { limit: step_limit })
    } else {
        // Any other terminal result (error subtype) — record as error and let
        // mini::run skip patch capture/verification.
        agent.trajectory.info.exit_reason = Some("error".into());
        agent
            .trajectory
            .info
            .failure_category
            .get_or_insert(FailureCategory::AgentInternal);
        agent.trajectory.info.other.insert(
            "claude_result_subtype".into(),
            Value::String(result.subtype.clone()),
        );
        agent.finalize_run_metadata(outcome::ERROR);
        let err_msg = if result.is_error {
            // Redact before truncating: this string is surfaced to the CLI and
            // logs, not just the (separately redacted) trajectory, so an error
            // result carrying a prompt snippet or secret must be scrubbed here.
            let redacted = agent
                .redactor
                .redact_text(&result.final_text, surface::TRAJECTORY)
                .text;
            format!(
                "claude driver ended with error: {}",
                truncate(&redacted, 500)
            )
        } else {
            format!(
                "claude driver ended with non-success result: {}",
                result.subtype
            )
        };
        Err(Error::Trajectory(err_msg))
    }
}

fn emit_ended(
    agent: &DefaultAgent,
    exit_reason: &str,
    failure_category: Option<FailureCategory>,
    final_output: Option<String>,
) {
    agent.stream.emit(StreamEvent::RunEnded {
        exit_reason: exit_reason.to_owned(),
        failure_category,
        final_output,
        steps: agent.steps,
        total_cost_usd: agent.total_cost_usd,
        ended_at: chrono::Utc::now().to_rfc3339(),
    });
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}
