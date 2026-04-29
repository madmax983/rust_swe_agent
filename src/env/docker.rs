//! `DockerEnvironment`: shells out to the `docker` CLI, which sidesteps the
//! whole bindgen / native-openssl / libssh2 chain that the `docker-api` and
//! `bollard` crates pull in.
//!
//! `start()` does a `docker version` preflight, then `docker run -d --rm`
//! with a `rust-swe-agent=1` label.
//! `run()` calls `docker exec`.
//! `shutdown()` calls `docker rm -f`. Async path.
//! `Drop` calls `docker rm -f` synchronously, best-effort. Cannot await.
//! `cleanup_orphans()` reaps by label — covers the SIGKILL leak case Python
//! has too.

use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{Environment, RunRequest, RunResult};
use crate::error::EnvError;
use crate::ids::ContainerId;

pub const LABEL: &str = "rust-swe-agent=1";

pub struct DockerEnvironment {
    container_id: ContainerId,
    image: String,
    workdir: PathBuf,
    shutdown_sent: AtomicBool,
    cleanup_on_drop: bool,
}

impl DockerEnvironment {
    /// Start a new container and return the handle.
    pub async fn start(image: impl Into<String>, workdir: PathBuf) -> Result<Self, EnvError> {
        let image = image.into();
        preflight().await?;

        let wd_str = workdir.to_string_lossy().into_owned();
        let out = Command::new("docker")
            .args(["run", "-d", "--rm", "--label", LABEL, "-w", &wd_str])
            .arg(&image)
            .args(["sleep", "infinity"])
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
        cmd.args(["exec", "-w", &wd]);
        for (k, v) in &req.env {
            cmd.args(["-e", &format!("{k}={v}")]);
        }
        cmd.arg(self.container_id.as_str())
            .args(["bash", "-c", &req.command])
            .stdin(StdStdio::null())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(EnvError::Io)?;
        let mut stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("docker exec stdout pipe missing".into()))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("docker exec stderr pipe missing".into()))?;

        let fut = async move {
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            let (r1, r2) = tokio::join!(
                stdout_pipe.read_to_end(&mut stdout_buf),
                stderr_pipe.read_to_end(&mut stderr_buf),
            );
            r1.map_err(EnvError::Io)?;
            r2.map_err(EnvError::Io)?;
            let status = child.wait().await.map_err(EnvError::Io)?;
            Ok::<_, EnvError>((
                String::from_utf8_lossy(&stdout_buf).into_owned(),
                String::from_utf8_lossy(&stderr_buf).into_owned(),
                status.code().unwrap_or(-1),
            ))
        };

        match tokio::time::timeout(req.timeout, fut).await {
            Ok(Ok((stdout, stderr, exit_code))) => Ok(RunResult {
                stdout,
                stderr,
                exit_code,
                timed_out: false,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(RunResult {
                stdout: String::new(),
                stderr: format!("timed out after {:?}", req.timeout),
                exit_code: -1,
                timed_out: true,
            }),
        }
    }

    async fn shutdown(&mut self) -> Result<(), EnvError> {
        self.shutdown_sent.store(true, Ordering::SeqCst);
        let out = Command::new("docker")
            .args(["rm", "-f", self.container_id.as_str()])
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::piped())
            .output()
            .await
            .map_err(EnvError::Io)?;
        if !out.status.success() {
            return Err(EnvError::CommandFailed(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(())
    }
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

/// Reap any container with our label. Called by `rust-swe-agent cleanup`.
/// Returns the list of reaped container ids.
pub async fn cleanup_orphans() -> Result<Vec<String>, EnvError> {
    preflight().await?;
    let list = Command::new("docker")
        .args(["ps", "-q", "--filter", &format!("label={LABEL}")])
        .stdin(StdStdio::null())
        .output()
        .await
        .map_err(EnvError::Io)?;
    if !list.status.success() {
        return Err(EnvError::CommandFailed(
            String::from_utf8_lossy(&list.stderr).trim().to_string(),
        ));
    }
    let ids: Vec<String> = String::from_utf8_lossy(&list.stdout)
        .lines()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
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

#[allow(dead_code)]
const _COMPILE_TIME_USED: Duration = Duration::from_secs(0);
