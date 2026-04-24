//! Local shell environment. Spawns `bash -lc <cmd>` via `tokio::process`,
//! collects stdout/stderr, enforces timeout, converts SIGKILL-from-timeout
//! into `timed_out = true` rather than an error.

use async_trait::async_trait;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{Environment, RunRequest, RunResult};
use crate::error::EnvError;

pub struct LocalEnvironment {
    shell: String,
}

impl Default for LocalEnvironment {
    fn default() -> Self {
        Self {
            shell: default_shell(),
        }
    }
}

impl LocalEnvironment {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = shell.into();
        self
    }
}

fn default_shell() -> String {
    if cfg!(windows) {
        "cmd.exe".to_owned()
    } else {
        "bash".to_owned()
    }
}

#[async_trait]
impl Environment for LocalEnvironment {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new(&self.shell);
            c.arg("/C").arg(&req.command);
            c
        } else {
            let mut c = Command::new(&self.shell);
            c.arg("-c").arg(&req.command);
            c
        };

        if let Some(cwd) = req.cwd.as_ref() {
            cmd.current_dir(cwd);
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(EnvError::Io)?;

        // Take pipes so we can read them concurrently with `wait`.
        let mut stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("stdout pipe missing".into()))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("stderr pipe missing".into()))?;

        let wait_fut = async move {
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            let read_stdout = stdout_pipe.read_to_end(&mut stdout_buf);
            let read_stderr = stderr_pipe.read_to_end(&mut stderr_buf);
            let (r1, r2) = tokio::join!(read_stdout, read_stderr);
            r1.map_err(EnvError::Io)?;
            r2.map_err(EnvError::Io)?;
            let status = child.wait().await.map_err(EnvError::Io)?;
            Ok::<_, EnvError>((
                String::from_utf8_lossy(&stdout_buf).into_owned(),
                String::from_utf8_lossy(&stderr_buf).into_owned(),
                status.code().unwrap_or(-1),
            ))
        };

        match tokio::time::timeout(req.timeout, wait_fut).await {
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn echo_roundtrips() {
        let env = LocalEnvironment::new();
        let r = env.run(RunRequest::new("echo hello")).await.unwrap();
        assert_eq!(r.stdout.trim(), "hello");
        assert_eq!(r.exit_code, 0);
        assert!(!r.timed_out);
    }

    #[tokio::test]
    async fn nonzero_exit_surfaces() {
        let env = LocalEnvironment::new();
        let r = env.run(RunRequest::new("exit 7")).await.unwrap();
        assert_eq!(r.exit_code, 7);
    }

    #[tokio::test]
    async fn env_var_passthrough() {
        let env = LocalEnvironment::new();
        let mut req = RunRequest::new("echo $RSA_TEST_VAR");
        req.env.insert("RSA_TEST_VAR".into(), "from_test".into());
        let r = env.run(req).await.unwrap();
        assert_eq!(r.stdout.trim(), "from_test");
    }

    #[tokio::test]
    async fn timeout_flags_timed_out() {
        let env = LocalEnvironment::new();
        let req = RunRequest::new("sleep 5").with_timeout(Duration::from_millis(100));
        let r = env.run(req).await.unwrap();
        assert!(r.timed_out);
    }

    #[tokio::test]
    async fn stderr_is_captured_separately() {
        let env = LocalEnvironment::new();
        let r = env
            .run(RunRequest::new("echo out && echo err >&2"))
            .await
            .unwrap();
        assert_eq!(r.stdout.trim(), "out");
        assert_eq!(r.stderr.trim(), "err");
    }
}
