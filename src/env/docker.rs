//! `DockerEnvironment`: shells out to the `docker` CLI, which sidesteps the
//! whole bindgen / native-openssl / libssh2 chain that the `docker-api` and
//! `bollard` crates pull in.
//!
//! `start()` does a `docker version` preflight, then `docker run -d --rm`
//! with a `maxwells-daemon=1` label.
//! `run()` calls `docker exec`.
//! `shutdown()` calls `docker rm -f`. Async path.
//! `Drop` calls `docker rm -f` synchronously, best-effort. Cannot await.
//! `cleanup_orphans()` reaps by label — covers the SIGKILL leak case Python
//! has too.

use async_trait::async_trait;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio as StdStdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use super::{Environment, RunRequest, RunResult};
use crate::error::EnvError;
use crate::ids::ContainerId;

pub const LABEL: &str = "maxwells-daemon=1";
const LEGACY_LABEL: &str = "rust-swe-agent=1";
const FORCE_KILL_WAIT: Duration = Duration::from_secs(2);

type PipeCollector = JoinHandle<Result<(), EnvError>>;
type PipeBuffer = Arc<Mutex<Vec<u8>>>;

pub struct DockerEnvironment {
    container_id: ContainerId,
    image: String,
    workdir: PathBuf,
    shutdown_sent: AtomicBool,
    cleanup_on_drop: bool,
}

/// Build the argument list for `docker run … <image> sleep infinity`.
///
/// Extracted so unit tests can assert the exact arg shape without running
/// Docker. `network` is the value to pass after `--network` (e.g. `"none"`),
/// or `None` to omit the flag entirely (preserving today's default behavior).
fn build_run_args(image: &str, workdir: &str, label: &str, network: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "run".to_owned(),
        "-d".to_owned(),
        "--rm".to_owned(),
        "--label".to_owned(),
        label.to_owned(),
        "-w".to_owned(),
        workdir.to_owned(),
    ];
    if let Some(net) = network {
        args.push("--network".to_owned());
        args.push(net.to_owned());
    }
    args.push(image.to_owned());
    args.push("sleep".to_owned());
    args.push("infinity".to_owned());
    args
}

impl DockerEnvironment {
    /// Start a new container and return the handle.
    ///
    /// `network` controls the `--network` flag passed to `docker run`:
    /// `None` preserves today's default (bridge networking), `Some("none")`
    /// disables all egress, and any other string is forwarded verbatim.
    pub async fn start(
        image: impl Into<String>,
        workdir: PathBuf,
        network: Option<&str>,
    ) -> Result<Self, EnvError> {
        let image = image.into();
        preflight().await?;

        let wd_str = workdir.to_string_lossy();
        let run_args = build_run_args(&image, &wd_str, LABEL, network);
        let out = Command::new("docker")
            .args(&run_args)
            .stdin(StdStdio::null())
            .output()
            .await
            .map_err(EnvError::Io)?;

        if !out.status.success() {
            return Err(EnvError::ContainerStartFailed(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }

        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if id.is_empty() {
            return Err(EnvError::ContainerStartFailed(
                "docker run returned empty container id".into(),
            ));
        }

        Ok(Self {
            container_id: ContainerId::new(id),
            image,
            workdir,
            shutdown_sent: AtomicBool::new(false),
            cleanup_on_drop: true,
        })
    }

    pub fn container_id(&self) -> &ContainerId {
        &self.container_id
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn workdir(&self) -> &PathBuf {
        &self.workdir
    }

    async fn force_remove_container(&self) -> Result<(), EnvError> {
        let out = Command::new("docker")
            .args(["rm", "-f", self.container_id.as_str()])
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::piped())
            .output()
            .await
            .map_err(EnvError::Io)?;
        if !out.status.success() {
            return self.mark_shutdown_after_remove(Err(EnvError::CommandFailed(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            )));
        }
        self.mark_shutdown_after_remove(Ok(()))
    }

    fn mark_shutdown_after_remove(&self, result: Result<(), EnvError>) -> Result<(), EnvError> {
        result?;
        self.shutdown_sent.store(true, Ordering::SeqCst);
        Ok(())
    }
}

pub async fn preflight() -> Result<(), EnvError> {
    match Command::new("docker")
        .arg("version")
        .stdin(StdStdio::null())
        .stdout(StdStdio::null())
        .stderr(StdStdio::piped())
        .output()
        .await
    {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(EnvError::DockerDaemonUnreachable(
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(EnvError::DockerNotInstalled),
        Err(e) => Err(EnvError::Io(e)),
    }
}

#[async_trait]
impl Environment for DockerEnvironment {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        let wd = req
            .cwd
            .as_ref()
            .unwrap_or(&self.workdir)
            .to_string_lossy()
            .into_owned();

        let mut cmd = Command::new("docker");
        cmd.arg("exec");
        if req.stdin.is_some() {
            cmd.arg("-i");
        }
        cmd.args(["-w", &wd]);
        for (k, v) in &req.env {
            cmd.args(["-e", &format!("{k}={v}")]);
        }
        cmd.arg(self.container_id.as_str())
            .args(["bash", "-c", &req.command])
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .kill_on_drop(true);
        if req.stdin.is_some() {
            cmd.stdin(StdStdio::piped());
        } else {
            cmd.stdin(StdStdio::null());
        }

        let mut child = cmd.spawn().map_err(EnvError::Io)?;
        let stdin_task = match req.stdin {
            Some(stdin) => {
                let pipe = child.stdin.take().ok_or_else(|| {
                    EnvError::UnexpectedExit("docker exec stdin pipe missing".into())
                })?;
                Some(spawn_stdin_writer(pipe, stdin))
            }
            None => None,
        };
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("docker exec stdout pipe missing".into()))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("docker exec stderr pipe missing".into()))?;

        let (stdout_task, stdout_buffer) = spawn_pipe_collector(stdout_pipe);
        let (stderr_task, stderr_buffer) = spawn_pipe_collector(stderr_pipe);

        match wait_for_child(&mut child, req.timeout, req.cancellation).await? {
            ChildStop::Exited(status) => {
                join_stdin_writer_after_exit(stdin_task).await?;
                let stdout = join_reader(stdout_task, stdout_buffer, "stdout").await?;
                let stderr = join_reader(stderr_task, stderr_buffer, "stderr").await?;
                Ok(RunResult {
                    stdout,
                    stderr,
                    exit_code: status.code().unwrap_or(-1),
                    timed_out: false,
                })
            }
            ChildStop::TimedOut => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(FORCE_KILL_WAIT, child.wait()).await;
                abort_stdin_writer(stdin_task).await;
                let stdout = partial_reader_output(stdout_task, stdout_buffer).await;
                let stderr = partial_reader_output(stderr_task, stderr_buffer).await;
                Ok(RunResult {
                    stdout,
                    stderr: append_status_message(
                        stderr,
                        &format!("timed out after {:?}", req.timeout),
                    ),
                    exit_code: -1,
                    timed_out: true,
                })
            }
            ChildStop::Cancelled => {
                let remove_error = self.force_remove_container().await.err();
                let _ = child.start_kill();
                let _ = tokio::time::timeout(FORCE_KILL_WAIT, child.wait()).await;
                abort_stdin_writer(stdin_task).await;
                let stdout = partial_reader_output(stdout_task, stdout_buffer).await;
                let mut stderr = partial_reader_output(stderr_task, stderr_buffer).await;
                stderr = append_status_message(stderr, "cancelled");
                if let Some(err) = remove_error {
                    stderr =
                        append_status_message(stderr, &format!("container shutdown failed: {err}"));
                }
                Ok(RunResult {
                    stdout,
                    stderr,
                    exit_code: -1,
                    timed_out: false,
                })
            }
        }
    }

    async fn shutdown(&mut self) -> Result<(), EnvError> {
        self.force_remove_container().await
    }
}

fn spawn_stdin_writer(
    mut pipe: tokio::process::ChildStdin,
    stdin: String,
) -> JoinHandle<Result<(), EnvError>> {
    tokio::spawn(async move {
        pipe.write_all(stdin.as_bytes())
            .await
            .map_err(EnvError::Io)?;
        pipe.shutdown().await.map_err(EnvError::Io)
    })
}

async fn join_stdin_writer(
    handle: Option<JoinHandle<Result<(), EnvError>>>,
) -> Result<(), EnvError> {
    if let Some(handle) = handle {
        handle
            .await
            .map_err(|e| EnvError::UnexpectedExit(format!("stdin writer task failed: {e}")))??;
    }
    Ok(())
}

async fn join_stdin_writer_after_exit(
    handle: Option<JoinHandle<Result<(), EnvError>>>,
) -> Result<(), EnvError> {
    match join_stdin_writer(handle).await {
        Ok(()) => Ok(()),
        Err(err) if is_broken_pipe(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

fn is_broken_pipe(err: &EnvError) -> bool {
    matches!(err, EnvError::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe)
}

async fn abort_stdin_writer(handle: Option<JoinHandle<Result<(), EnvError>>>) {
    if let Some(handle) = handle {
        handle.abort();
        let _ = handle.await;
    }
}

enum ChildStop {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

async fn wait_for_child(
    child: &mut Child,
    timeout: Duration,
    cancellation: Option<super::CancellationToken>,
) -> Result<ChildStop, EnvError> {
    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    if let Some(mut cancellation) = cancellation {
        tokio::select! {
            status = child.wait() => Ok(ChildStop::Exited(status.map_err(EnvError::Io)?)),
            () = &mut sleep => Ok(ChildStop::TimedOut),
            () = cancellation.cancelled() => Ok(ChildStop::Cancelled),
        }
    } else {
        tokio::select! {
            status = child.wait() => Ok(ChildStop::Exited(status.map_err(EnvError::Io)?)),
            () = &mut sleep => Ok(ChildStop::TimedOut),
        }
    }
}

fn append_status_message(mut stderr: String, message: &str) -> String {
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push_str(message);
    stderr
}

fn spawn_pipe_collector<R>(pipe: R) -> (PipeCollector, PipeBuffer)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let task_buffer = Arc::clone(&buffer);
    let handle = tokio::spawn(async move { read_pipe_to_buffer(pipe, task_buffer).await });
    (handle, buffer)
}

async fn read_pipe_to_buffer<R>(mut pipe: R, buffer: Arc<Mutex<Vec<u8>>>) -> Result<(), EnvError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0u8; 8192];
    loop {
        let n = pipe.read(&mut chunk).await.map_err(EnvError::Io)?;
        if n == 0 {
            return Ok(());
        }
        buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(&chunk[..n]);
    }
}

async fn join_reader(
    handle: PipeCollector,
    buffer: PipeBuffer,
    name: &str,
) -> Result<String, EnvError> {
    handle
        .await
        .map_err(|e| EnvError::UnexpectedExit(format!("{name} reader task failed: {e}")))?
        .map(|()| buffer_to_string(&buffer))
}

async fn partial_reader_output(mut handle: PipeCollector, buffer: PipeBuffer) -> String {
    let wait = tokio::time::sleep(FORCE_KILL_WAIT);
    tokio::pin!(wait);
    tokio::select! {
        result = &mut handle => {
            let _ = result;
        }
        () = &mut wait => {
            handle.abort();
            let _ = handle.await;
        }
    }
    buffer_to_string(&buffer)
}

fn buffer_to_string(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl Drop for DockerEnvironment {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        if self.shutdown_sent.load(Ordering::SeqCst) {
            return;
        }
        // Synchronous best-effort. We deliberately do NOT try to spin up a
        // tokio runtime from Drop — that deadlocks if we're already inside
        // one. If this fails, `cleanup_orphans` can reap the container later.
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", self.container_id.as_str()])
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::null())
            .status();
    }
}

/// Reap any container with our current or legacy label. Called by `max cleanup`.
/// Returns the list of reaped container ids.
pub async fn cleanup_orphans() -> Result<Vec<String>, EnvError> {
    preflight().await?;
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for label in cleanup_labels() {
        let list = Command::new("docker")
            .args(["ps", "-q", "--filter", &format!("label={label}")])
            .stdin(StdStdio::null())
            .output()
            .await
            .map_err(EnvError::Io)?;
        if !list.status.success() {
            return Err(EnvError::CommandFailed(
                String::from_utf8_lossy(&list.stderr).trim().to_string(),
            ));
        }
        for id in String::from_utf8_lossy(&list.stdout)
            .lines()
            .filter(|s| !s.is_empty())
        {
            if seen.insert(id.to_owned()) {
                ids.push(id.to_owned());
            }
        }
    }
    if ids.is_empty() {
        return Ok(ids);
    }
    let rm = Command::new("docker")
        .args(["rm", "-f"])
        .args(&ids)
        .stdin(StdStdio::null())
        .stdout(StdStdio::null())
        .stderr(StdStdio::piped())
        .output()
        .await
        .map_err(EnvError::Io)?;
    if !rm.status.success() {
        return Err(EnvError::CommandFailed(
            String::from_utf8_lossy(&rm.stderr).trim().to_string(),
        ));
    }
    Ok(ids)
}

fn cleanup_labels() -> [&'static str; 2] {
    [LABEL, LEGACY_LABEL]
}

#[allow(dead_code)]
const _COMPILE_TIME_USED: Duration = Duration::from_secs(0);

#[cfg(test)]
mod tests {
    use super::*;

    fn test_env() -> DockerEnvironment {
        DockerEnvironment {
            container_id: ContainerId::new("test-container"),
            image: "test-image".into(),
            workdir: PathBuf::from("/workspace"),
            shutdown_sent: AtomicBool::new(false),
            cleanup_on_drop: false,
        }
    }

    #[test]
    fn failed_container_removal_does_not_mark_shutdown_sent() {
        let env = test_env();

        let result = env.mark_shutdown_after_remove(Err(EnvError::CommandFailed("boom".into())));

        assert!(result.is_err());
        assert!(!env.shutdown_sent.load(Ordering::SeqCst));
    }

    #[test]
    fn successful_container_removal_marks_shutdown_sent() {
        let env = test_env();

        assert!(env.mark_shutdown_after_remove(Ok(())).is_ok());

        assert!(env.shutdown_sent.load(Ordering::SeqCst));
    }

    #[test]
    fn cleanup_labels_cover_current_and_legacy_rename_labels() {
        assert_eq!(cleanup_labels(), [LABEL, "rust-swe-agent=1"]);
    }

    // ── Network mode RED-phase tests ─────────────────────────────────────────

    #[test]
    fn build_run_args_include_network_none_when_mode_is_none() {
        let args = build_run_args("my-image", "/workspace", LABEL, Some("none"));
        let Some(network_pos) = args.iter().position(|a| a == "--network") else {
            panic!("expected --network flag in args: {args:?}");
        };
        assert_eq!(args.get(network_pos + 1).map(String::as_str), Some("none"));
    }

    #[test]
    fn build_run_args_unchanged_for_unrestricted_mode() {
        let args_unrestricted = build_run_args("my-image", "/workspace", LABEL, None);
        assert!(
            !args_unrestricted.contains(&"--network".to_owned()),
            "unrestricted mode must not add --network flag: {args_unrestricted:?}"
        );
    }

    #[test]
    fn build_run_args_network_none_positioned_before_image() {
        let args = build_run_args("my-image", "/workspace", LABEL, Some("none"));
        let Some(network_pos) = args.iter().position(|a| a == "--network") else {
            panic!("--network missing");
        };
        let Some(image_pos) = args.iter().position(|a| a == "my-image") else {
            panic!("image missing");
        };
        assert!(
            network_pos < image_pos,
            "--network must appear before the image name"
        );
    }
}
