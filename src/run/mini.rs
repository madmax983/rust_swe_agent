//! One-shot task runner — the port of `run/mini.py`. Resolves backend from
//! the model name, builds `DefaultAgent`, runs to completion, writes a
//! trajectory file and (if submitted) an output artifact.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::agent::{Agent, DefaultAgent, default::DefaultAgentBuilder};
use crate::config::{Config, EnvKind};
#[cfg(feature = "docker")]
use crate::env::DockerEnvironment;
use crate::env::{Environment, LocalEnvironment, RunRequest};
use crate::error::Error;
use crate::model::litellm::LitellmBackend;
use crate::model::{DeterministicModel, Model, ModelUsage};
use crate::stream::{BroadcastSink, SseServer, StreamSink};
use crate::trajectory::FailureCategory;

const PATCH_BASE_ENV: &str = "RUST_SWE_AGENT_PATCH_BASE";

/// How a runner should snapshot the agent's working tree as a unified diff
/// after submission. Optional on `MiniArgs` because patch capture only
/// makes sense for benchmark sweeps; the standalone `bench mini` CLI run
/// has no baseline to diff against.
pub struct PatchCaptureSpec {
    /// The dataset's stated baseline. When `None`, diff is taken against
    /// `HEAD`. SWE-bench instances typically carry the resolved SHA.
    pub base_commit: Option<String>,
    /// Working tree where `git diff` runs. For docker envs this matches
    /// the container's `-w`; for local envs it must point at a real git
    /// checkout.
    pub workdir: PathBuf,
    /// Where to write the `.patch` artifact. Empty diffs are still
    /// persisted (zero-byte file) so downstream tooling can distinguish
    /// "agent ran and changed nothing" from "agent never reached this
    /// instance".
    pub patch_path: PathBuf,
    /// When `true`, skip `git apply --check` and empty-diff validation.
    /// Escape hatch for non-git environments; not for normal use.
    pub skip_patch_validation: bool,
}

/// Reasons why patch validation can fail after a successful `git diff` capture.
#[derive(Debug)]
pub enum PatchValidationFailure {
    /// The captured diff was empty (zero bytes).
    Empty,
    /// `git apply --check` rejected the patch; carries trimmed stderr.
    ApplyFailed(String),
}

pub struct MiniArgs {
    pub task: String,
    pub extra_context: Option<String>,
    pub config: Config,
    pub output_dir: PathBuf,
    pub trajectory_name: String,
    pub deterministic_responses: Option<Vec<String>>,
    /// Optional fixed `ModelUsage` reported by the deterministic backend
    /// on every call. Only meaningful when `deterministic_responses` is
    /// `Some`. Lets tests drive cost/budget logic without a real API.
    pub deterministic_usage_per_call: Option<ModelUsage>,
    /// Optional wallclock budget for the agent loop. When elapsed, any
    /// in-flight environment command is dropped and the trajectory is
    /// finalized as `wallclock_timeout`.
    pub task_timeout_secs: Option<u64>,
    /// Optional SSE stream endpoint to bind. When `Some`, the runner
    /// starts a server before the agent runs and shuts it down after.
    pub stream_addr: Option<SocketAddr>,
    /// When `Some` and the agent submits, the runner captures a `git
    /// diff` of `workdir` against `base_commit` and writes it to
    /// `patch_path`. Capture failures downgrade the run's recorded
    /// outcome to `error` rather than crashing the runner.
    pub patch_capture: Option<PatchCaptureSpec>,
}

#[allow(clippy::too_many_lines)]
pub async fn run(args: MiniArgs) -> Result<(), Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    let model = build_model(
        &args.config,
        args.deterministic_responses,
        args.deterministic_usage_per_call.clone(),
    );
    let env = build_env(&args.config).await?;

    // Bring up the SSE server first so any client that connects right
    // after CLI startup catches the `run_started` event the builder
    // emits below.
    let (sink, server): (Option<Arc<dyn StreamSink>>, Option<SseServer>) = match args.stream_addr {
        Some(addr) => {
            let bcast = Arc::new(BroadcastSink::default());
            let server = SseServer::start(addr, bcast.clone()).await.map_err(|e| {
                Error::Trajectory(format!("failed to bind SSE server on {addr}: {e}"))
            })?;
            tracing::info!(addr = %server.local_addr(), "streaming events on http://{}/", server.local_addr());
            (Some(bcast as Arc<dyn StreamSink>), Some(server))
        }
        None => (None, None),
    };

    let mut agent: DefaultAgent = DefaultAgentBuilder {
        config: args.config.clone(),
        model,
        env,
        task: args.task.clone(),
        extra_context: args.extra_context.clone(),
        renderer: None,
        stream: sink,
    }
    .build()?;

    let traj_path = args
        .output_dir
        .join(format!("{}.traj.json", args.trajectory_name));

    // Run the agent. On error, finalize the trajectory with
    // `outcome="error"` so the partial run is still a self-contained
    // record of what happened — then propagate.
    let run_result = run_agent_with_optional_timeout(&mut agent, args.task_timeout_secs).await;
    if let Err(e) = &run_result {
        finalize_error_trajectory(&mut agent, e);
    }

    // Patch capture happens before the trajectory is saved so any
    // capture failure can be reflected as `outcome: "error"` rather
    // than leaving a stale `submitted` record on disk.
    let mut patch_written = false;
    if let (Ok(crate::agent::ExitReason::Submitted { .. }), Some(spec)) =
        (run_result.as_ref(), args.patch_capture.as_ref())
    {
        match capture_patch(agent.env.as_ref(), spec).await {
            Ok(diff) => {
                // Always write the patch file — operators need to inspect
                // failed patches too.
                std::fs::write(&spec.patch_path, &diff)?;
                patch_written = true;

                match check_patch_validity(agent.env.as_ref(), spec, &diff).await {
                    Ok(()) => {}
                    Err(PatchValidationFailure::Empty) => {
                        tracing::warn!(
                            instance = %args.trajectory_name,
                            "agent submitted but produced an empty diff; downgrading to error"
                        );
                        agent.trajectory.info.exit_reason = Some("error".into());
                        agent.trajectory.info.failure_category = Some(FailureCategory::PatchEmpty);
                        agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
                    }
                    Err(PatchValidationFailure::ApplyFailed(reason)) => {
                        tracing::warn!(
                            instance = %args.trajectory_name,
                            error = %reason,
                            "patch apply check failed; downgrading outcome to error"
                        );
                        agent.trajectory.info.exit_reason = Some("error".into());
                        agent.trajectory.info.failure_category =
                            Some(FailureCategory::PatchApplyInvalid);
                        agent.trajectory.info.other.insert(
                            "patch_apply_error".into(),
                            serde_json::Value::String(reason),
                        );
                        agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
                    }
                }
            }
            Err(reason) => {
                tracing::warn!(
                    instance = %args.trajectory_name,
                    error = %reason,
                    "patch capture failed; downgrading outcome to error"
                );
                agent.trajectory.info.exit_reason = Some("error".into());
                agent.trajectory.info.failure_category = Some(FailureCategory::EnvSetup);
                agent
                    .trajectory
                    .info
                    .other
                    .insert("patch_error".into(), serde_json::Value::String(reason));
                agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
            }
        }
    }

    agent.trajectory.save_pretty(&traj_path)?;

    let exit = match run_result {
        Ok(exit) => exit,
        Err(e) => {
            if let Some(server) = server {
                server.shutdown().await;
            }
            return Err(e);
        }
    };

    if let crate::agent::ExitReason::Submitted { final_output } = &exit {
        let out_path = args
            .output_dir
            .join(format!("{}.output.txt", args.trajectory_name));
        std::fs::write(&out_path, final_output)?;
    }

    tracing::info!(?traj_path, patch_written, "trajectory written");

    if let Some(server) = server {
        server.shutdown().await;
    }
    Ok(())
}

fn finalize_error_trajectory(agent: &mut DefaultAgent, err: &Error) {
    agent
        .trajectory
        .info
        .exit_reason
        .get_or_insert_with(|| "error".into());
    agent
        .trajectory
        .info
        .failure_category
        .get_or_insert_with(|| classify_error(err));
    agent
        .trajectory
        .info
        .other
        .entry("error_message".into())
        .or_insert_with(|| serde_json::Value::String(err.to_string()));
    agent.finalize_run_metadata(crate::trajectory::outcome::ERROR);
}

async fn run_agent_with_optional_timeout(
    agent: &mut DefaultAgent,
    task_timeout_secs: Option<u64>,
) -> Result<crate::agent::ExitReason, Error> {
    let Some(secs) = task_timeout_secs else {
        return agent.run().await;
    };
    let timeout = Duration::from_secs(secs);
    if let Ok(result) = tokio::time::timeout(timeout, agent.run()).await {
        return result;
    }
    let shutdown_error = agent.env.shutdown().await.err();
    agent.finalize_wallclock_timeout(timeout);
    if let Some(err) = shutdown_error {
        agent.trajectory.info.other.insert(
            "environment_shutdown_error".into(),
            serde_json::Value::String(err.to_string()),
        );
    }
    Err(Error::Trajectory(format!(
        "task wallclock timeout after {secs}s"
    )))
}

fn classify_error(err: &Error) -> FailureCategory {
    match err {
        Error::Env(_) => FailureCategory::EnvSetup,
        Error::Model(crate::error::ModelError::Malformed(_)) => FailureCategory::ModelParse,
        Error::Model(_) => FailureCategory::ModelApi,
        _ => FailureCategory::AgentInternal,
    }
}

/// Validate that `diff` applies cleanly against `spec.base_commit` in a clean
/// checkout, without touching the agent's working tree.
///
/// - Returns `Ok(())` immediately when `spec.skip_patch_validation` is `true`
///   or `spec.base_commit` is `None` (standalone runs without a base).
/// - Returns `Err(PatchValidationFailure::Empty)` when `diff` is empty.
/// - Returns `Err(PatchValidationFailure::ApplyFailed(_))` when the worktree
///   setup or `git apply --check` exits non-zero.
///
/// A temporary git worktree is created at `base_commit` so the check runs
/// against the clean base state rather than the (already-modified) working
/// tree.  Temp files are placed inside `spec.workdir` (the volume mount point
/// in Docker) using relative paths so the shell command works correctly in
/// both local and Docker environments.  The worktree and patch file are always
/// removed after the check, success or failure.
pub(crate) async fn check_patch_validity(
    env: &dyn Environment,
    spec: &PatchCaptureSpec,
    diff: &str,
) -> Result<(), PatchValidationFailure> {
    if spec.skip_patch_validation {
        return Ok(());
    }
    let Some(base_commit) = spec.base_commit.as_deref() else {
        return Ok(());
    };
    if diff.is_empty() {
        return Err(PatchValidationFailure::Empty);
    }
    validate_git_rev(base_commit).map_err(PatchValidationFailure::ApplyFailed)?;

    // Use full nanoseconds for better uniqueness across concurrent calls.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    // Place temp files inside spec.workdir so they are accessible from inside
    // Docker containers (where the workdir is the mounted volume).  Relative
    // names are used in the shell command so paths are correct regardless of
    // how the volume is mapped inside the container.
    let patch_name = format!(".patch_validate_{unique}.patch");
    let wt_name = format!(".patch_validate_wt_{unique}");
    let tmp_patch = spec.workdir.join(&patch_name);

    std::fs::write(&tmp_patch, diff.as_bytes())
        .map_err(|e| PatchValidationFailure::ApplyFailed(format!("write temp: {e}")))?;

    // `patch_base_shell_arg` generates a shell-quoted env-var reference that
    // expands to `base_commit` at runtime (cross-platform: POSIX `$VAR` /
    // Windows `%VAR%`).
    let base_arg = patch_base_shell_arg(base_commit);

    // Create a clean worktree at base_commit (relative path, valid in Docker),
    // validate the patch there, then always remove the worktree even on failure.
    // `../{patch_name}` navigates from inside the worktree back to spec.workdir.
    let cmd = format!(
        "git worktree add -q --detach {wt} {base_arg} \
        && git -C {wt} apply --check --no-3way -- ../{patch} ; \
        RC=$? ; git worktree remove --force {wt} 2>/dev/null ; exit $RC",
        wt = shell_quote_path(&wt_name),
        patch = shell_quote_path(&patch_name),
    );
    let mut req = RunRequest::new(cmd).with_timeout(Duration::from_secs(30));
    req.cwd = Some(spec.workdir.clone());
    req.env
        .insert(PATCH_BASE_ENV.into(), base_commit.to_owned());

    let result = env
        .run(req)
        .await
        .map_err(|e| PatchValidationFailure::ApplyFailed(format!("env exec: {e}")))?;

    let _ = std::fs::remove_file(&tmp_patch);

    if result.timed_out || result.exit_code != 0 {
        let reason = result.stderr.trim();
        return Err(PatchValidationFailure::ApplyFailed(if reason.is_empty() {
            "git apply --check failed (no stderr)".into()
        } else {
            reason.to_owned()
        }));
    }
    Ok(())
}

fn shell_quote_path(path: &str) -> String {
    // Single-quote the path for POSIX shells, escaping embedded single-quotes.
    if cfg!(windows) {
        format!("\"{}\"", path.replace('"', "\\\""))
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// Snapshot the working tree at `spec.workdir` as a unified diff against
/// `spec.base_commit` (or `HEAD`). Returns the diff text on success or a
/// human-readable reason string on failure. Pure: writes nothing.
///
/// We use `--no-color`, `--binary`, and `--unified=3` to match the format
/// the SWE-bench evaluator (`sb-cli`) consumes and `git apply` accepts.
async fn capture_patch(env: &dyn Environment, spec: &PatchCaptureSpec) -> Result<String, String> {
    // Keep paths in `cwd` and the base revision in the environment so valid
    // revspec punctuation does not become shell syntax.
    let base = validate_git_rev(spec.base_commit.as_deref().unwrap_or("HEAD"))?;
    let base_arg = patch_base_shell_arg(base);
    let cmd = format!("git diff --no-color --binary --unified=3 --end-of-options {base_arg} -- .");
    let mut req = RunRequest::new(cmd).with_timeout(Duration::from_secs(60));
    req.cwd = Some(spec.workdir.clone());
    req.env.insert(PATCH_BASE_ENV.into(), base.to_owned());
    let result = env
        .run(req)
        .await
        .map_err(|e| format!("env exec failed: {e}"))?;
    if result.timed_out {
        return Err(format!("git diff timed out: {}", result.stderr.trim()));
    }
    if result.exit_code != 0 {
        return Err(format!(
            "git diff exited {} ({})",
            result.exit_code,
            result.stderr.trim()
        ));
    }
    Ok(result.stdout)
}

fn validate_git_rev(rev: &str) -> Result<&str, String> {
    let valid = !rev.is_empty() && rev.chars().all(|ch| !ch.is_control());
    if valid {
        Ok(rev)
    } else {
        Err(format!("invalid git revision for diff base: {rev:?}"))
    }
}

fn patch_base_shell_arg(rev: &str) -> String {
    if cfg!(windows) {
        if rev.contains('"') {
            quote_git_rev_for_cmd(rev)
        } else {
            format!("\"%{PATCH_BASE_ENV}%\"")
        }
    } else {
        format!("\"${PATCH_BASE_ENV}\"")
    }
}

fn quote_git_rev_for_cmd(rev: &str) -> String {
    let needs_quotes = rev.chars().any(char::is_whitespace);
    let mut out = String::with_capacity(rev.len() + usize::from(needs_quotes) * 2);
    if needs_quotes {
        out.push('"');
    }
    for ch in rev.chars() {
        match (needs_quotes, ch) {
            (_, '"') => out.push_str("\\\""),
            (false, '^') => out.push_str("^^"),
            (false, '&' | '|' | '<' | '>' | '(' | ')' | '%') => {
                out.push('^');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    if needs_quotes {
        out.push('"');
    }
    out
}

fn build_model(
    cfg: &Config,
    deterministic: Option<Vec<String>>,
    deterministic_usage_per_call: Option<ModelUsage>,
) -> Arc<dyn Model> {
    if let Some(responses) = deterministic {
        let model = match deterministic_usage_per_call {
            Some(usage) => DeterministicModel::with_usage(responses, usage),
            None => DeterministicModel::new(responses),
        };
        return Arc::new(model);
    }
    // `LitellmBackend` dispatches by model prefix; credentials come from
    // the usual provider env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
    // …) the way Python LiteLLM expects.
    let backend =
        LitellmBackend::new(cfg.root.model.name.clone()).with_max_tokens(cfg.root.model.max_tokens);
    Arc::new(backend)
}

async fn build_env(cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    match cfg.root.environment.kind {
        EnvKind::Local => Ok(Box::new(LocalEnvironment::new())),
        EnvKind::Docker => build_docker_env(cfg).await,
    }
}

#[cfg(feature = "docker")]
async fn build_docker_env(cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    let image = cfg.root.environment.docker_image.clone().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "environment.kind=docker requires environment.docker_image".into(),
        ))
    })?;
    let wd = PathBuf::from(cfg.root.environment.workdir.clone());
    let env = DockerEnvironment::start(image, wd).await?;
    Ok(Box::new(env))
}

#[cfg(not(feature = "docker"))]
#[allow(clippy::unused_async)] // Mirrors the docker-feature signature.
async fn build_docker_env(_cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "docker support not compiled in — rebuild with --features docker".into(),
    )))
}

/// Derive a filename-safe trajectory name from a task string.
pub fn slugify(task: &str) -> String {
    let mut s: String = task
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let trimmed = s.trim_matches('-').to_lowercase();
    let cut: String = trimmed.chars().take(64).collect();
    if cut.is_empty() { "task".into() } else { cut }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("   "), "task");
        assert_eq!(slugify("A/B/C"), "a-b-c");
    }

    #[test]
    fn validate_git_rev_accepts_revspec_chars_and_rejects_empty_or_control() {
        assert_eq!(validate_git_rev("HEAD"), Ok("HEAD"));
        assert_eq!(validate_git_rev("abc123"), Ok("abc123"));
        assert_eq!(validate_git_rev("HEAD@{1}"), Ok("HEAD@{1}"));
        assert_eq!(validate_git_rev("v1.2^{commit}"), Ok("v1.2^{commit}"));
        assert_eq!(
            validate_git_rev("refs/heads/feat%test"),
            Ok("refs/heads/feat%test")
        );
        assert_eq!(
            validate_git_rev("refs/heads/feat\"test"),
            Ok("refs/heads/feat\"test")
        );
        assert_eq!(validate_git_rev("HEAD^{/foo bar}"), Ok("HEAD^{/foo bar}"));
        assert_eq!(
            validate_git_rev("HEAD^{/foo && bar}"),
            Ok("HEAD^{/foo && bar}")
        );
        assert!(validate_git_rev("HEAD\nmain").is_err());
        assert!(validate_git_rev("").is_err());
    }

    #[tokio::test]
    async fn capture_patch_accepts_reflog_revision_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        std::fs::write(repo.join("hello.txt"), "committed\n").unwrap();
        git(&repo, &["commit", "-am", "advance"]);
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("HEAD@{1}".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_accepts_peeled_tag_revision_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["tag", "-a", "v1.2", "-m", "v1.2"]);
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("v1.2^{commit}".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_accepts_percent_ref_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["branch", "feat%test"]);
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("refs/heads/feat%test".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_accepts_quoted_ref_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        write_packed_ref(&repo, "refs/heads/feat\"test");
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("refs/heads/feat\"test".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_accepts_commit_message_search_revision_with_space() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "foo bar base"]);
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some("HEAD^{/foo bar}".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let diff = capture_patch(&LocalEnvironment::new(), &spec)
            .await
            .unwrap();
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
    }

    #[tokio::test]
    async fn capture_patch_quotes_shell_metacharacters_in_revision_base() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);

        let spec = PatchCaptureSpec {
            base_commit: Some("HEAD && echo injected > injected.txt".into()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };

        let result = capture_patch(&LocalEnvironment::new(), &spec).await;
        assert!(result.is_err(), "{result:?}");
        assert!(!repo.join("injected.txt").exists());
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init"]);
        git(dir, &["config", "user.email", "test@example.invalid"]);
        git(dir, &["config", "user.name", "Test User"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        git(dir, &["config", "tag.gpgsign", "false"]);
    }

    fn write_packed_ref(dir: &Path, ref_name: &str) {
        let hash = git_stdout(dir, &["rev-parse", "HEAD"]);
        std::fs::write(
            dir.join(".git").join("packed-refs"),
            format!("{} {}\n", hash.trim(), ref_name),
        )
        .unwrap();
        let resolved = git_stdout(dir, &["rev-parse", ref_name]);
        assert_eq!(resolved.trim(), hash.trim());
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = git_raw(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let out = git_raw(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn git_raw(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
    }

    // ── RED-phase tests: patch validation ──────────────────────────────────

    #[tokio::test]
    async fn patch_validation_empty_diff_yields_patch_empty_error() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        let spec = PatchCaptureSpec {
            base_commit: Some(base_sha),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        // empty diff string → should fail with PatchEmpty
        let result = check_patch_validity(&LocalEnvironment::new(), &spec, "").await;
        assert!(
            matches!(result, Err(PatchValidationFailure::Empty)),
            "expected PatchEmpty, got {result:?}"
        );
    }

    #[tokio::test]
    async fn patch_validation_valid_patch_passes() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        // Modify workdir (leave unstaged) — worktree approach checks against
        // a clean checkout at base_sha, so this does not need to be reverted.
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();

        let spec = PatchCaptureSpec {
            base_commit: Some(base_sha.clone()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        let diff = capture_patch(&LocalEnvironment::new(), &spec)
            .await
            .unwrap();

        let result = check_patch_validity(&LocalEnvironment::new(), &spec, &diff).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn patch_validation_wrong_base_yields_apply_invalid() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        let initial_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        // Apply "before → after" change and commit it
        std::fs::write(repo.join("hello.txt"), "after\n").unwrap();
        git(&repo, &["commit", "-am", "second"]);

        // Build a diff that patches "before" → "after"
        let _spec_diff = PatchCaptureSpec {
            base_commit: Some(initial_sha.clone()),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        // HEAD is at "second" commit; diff against initial gives before→after
        // But workdir already has "after" committed, so diff is empty from cwd.
        // Instead, craft the diff manually to simulate a stale patch.
        let stale_diff = "diff --git a/hello.txt b/hello.txt\n\
index 8a1218a..24c5735 100644\n\
--- a/hello.txt\n\
+++ b/hello.txt\n\
@@ -1 +1 @@\n\
-before\n\
+after\n";

        // Validate against the CURRENT HEAD (which already has "after") — should fail
        let head_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        let spec_validate = PatchCaptureSpec {
            base_commit: Some(head_sha),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        let result =
            check_patch_validity(&LocalEnvironment::new(), &spec_validate, stale_diff).await;
        assert!(
            matches!(result, Err(PatchValidationFailure::ApplyFailed(_))),
            "expected ApplyFailed, got {result:?}"
        );
    }

    #[tokio::test]
    async fn patch_validation_skipped_when_flag_set() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        let spec = PatchCaptureSpec {
            base_commit: Some(base_sha),
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: true,
        };
        // Empty diff + skip_patch_validation=true → should succeed
        let result = check_patch_validity(&LocalEnvironment::new(), &spec, "").await;
        assert!(result.is_ok(), "expected Ok with skip flag, got {result:?}");
    }

    #[tokio::test]
    async fn patch_validation_skipped_when_no_base_commit() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);

        let spec = PatchCaptureSpec {
            base_commit: None, // no base → validation is gated off
            workdir: repo.clone(),
            patch_path: work.path().join("out.patch"),
            skip_patch_validation: false,
        };
        let result = check_patch_validity(&LocalEnvironment::new(), &spec, "").await;
        assert!(
            result.is_ok(),
            "expected Ok when base_commit is None, got {result:?}"
        );
    }

    // ── End-to-end tests through mini::run() ──────────────────────────────

    /// AC (b): agent submits with no workdir changes → empty diff → outcome
    /// downgraded to error with failure_category=patch_empty.
    #[tokio::test]
    async fn mini_run_empty_diff_yields_patch_empty_outcome() {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("hello.txt"), "content\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        let base_sha = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        let runs_dir = work.path().join("runs");
        let patch_path = work.path().join("out.patch");

        let mut cfg = crate::config::Config::defaults().unwrap();
        cfg.root.agent.step_limit = 5;

        let args = MiniArgs {
            task: "do nothing".into(),
            extra_context: None,
            config: cfg,
            output_dir: runs_dir.clone(),
            trajectory_name: "empty-diff-test".into(),
            // Submit immediately without touching the repo → empty diff
            deterministic_responses: Some(vec![
                "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
            ]),
            deterministic_usage_per_call: None,
            task_timeout_secs: Some(30),
            stream_addr: None,
            patch_capture: Some(PatchCaptureSpec {
                base_commit: Some(base_sha),
                workdir: repo.clone(),
                patch_path: patch_path.clone(),
                skip_patch_validation: false,
            }),
        };

        run(args).await.unwrap();

        // Trajectory must have outcome=error, failure_category=patch_empty
        let traj_path = runs_dir.join("empty-diff-test.traj.json");
        let traj_json = std::fs::read_to_string(&traj_path).unwrap();
        let traj: serde_json::Value = serde_json::from_str(&traj_json).unwrap();

        assert_eq!(
            traj["info"]["outcome"].as_str(),
            Some("error"),
            "expected error outcome; trajectory:\n{traj_json}"
        );
        assert_eq!(
            traj["info"]["failure_category"].as_str(),
            Some("patch_empty"),
            "expected patch_empty failure_category; trajectory:\n{traj_json}"
        );

        // Patch file must be written even though the diff is empty
        assert!(patch_path.exists(), "patch file should exist on disk");
        let patch_content = std::fs::read_to_string(&patch_path).unwrap();
        assert!(
            patch_content.is_empty(),
            "patch file should be empty for a no-op submission"
        );
    }
}
