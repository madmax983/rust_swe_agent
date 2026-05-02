//! Local shell environment. Spawns `bash -lc <cmd>` via `tokio::process`,
//! collects stdout/stderr, enforces timeout, converts SIGKILL-from-timeout
//! into `timed_out = true` rather than an error.

use async_trait::async_trait;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use super::{Environment, RunRequest, RunResult};
use crate::error::EnvError;

#[cfg(not(windows))]
const TERMINATE_GRACE: Duration = Duration::from_millis(250);
const FORCE_KILL_WAIT: Duration = Duration::from_secs(2);
#[cfg(not(windows))]
const PROCESS_EXIT_POLL: Duration = Duration::from_millis(25);

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
        let mut cmd = shell_command(&self.shell, &req.command);
        isolate_process_tree(&mut cmd);

        if let Some(cwd) = req.cwd.as_ref() {
            cmd.current_dir(cwd);
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        if req.stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(EnvError::Io)?;
        let mut process_guard = ProcessTreeGuard::new(child.id());
        let stdin_task = match req.stdin {
            Some(input) => {
                let mut stdin_pipe = child
                    .stdin
                    .take()
                    .ok_or_else(|| EnvError::UnexpectedExit("stdin pipe missing".into()))?;
                Some(tokio::spawn(async move {
                    stdin_pipe
                        .write_all(input.as_bytes())
                        .await
                        .map_err(EnvError::Io)?;
                    stdin_pipe.shutdown().await.map_err(EnvError::Io)
                }))
            }
            None => None,
        };

        // Take pipes so we can read them concurrently with `wait`.
        let mut stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("stdout pipe missing".into()))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("stderr pipe missing".into()))?;
        let stdout_task = tokio::spawn(async move { read_pipe_to_string(&mut stdout_pipe).await });
        let stderr_task = tokio::spawn(async move { read_pipe_to_string(&mut stderr_pipe).await });

        if let Ok(status) = tokio::time::timeout(req.timeout, child.wait()).await {
            let status = status.map_err(EnvError::Io)?;
            process_guard.disarm();
            if let Some(stdin_task) = stdin_task {
                join_writer(stdin_task, "stdin").await?;
            }
            let stdout = join_reader(stdout_task, "stdout").await?;
            let stderr = join_reader(stderr_task, "stderr").await?;
            return Ok(RunResult {
                stdout,
                stderr,
                exit_code: status.code().unwrap_or(-1),
                timed_out: false,
            });
        }

        process_guard.terminate_and_wait(&mut child).await;
        if let Some(stdin_task) = stdin_task {
            stdin_task.abort();
        }
        stdout_task.abort();
        stderr_task.abort();
        Ok(RunResult {
            stdout: String::new(),
            stderr: format!("timed out after {:?}", req.timeout),
            exit_code: -1,
            timed_out: true,
        })
    }
}

async fn join_writer(handle: JoinHandle<Result<(), EnvError>>, name: &str) -> Result<(), EnvError> {
    handle
        .await
        .map_err(|e| EnvError::UnexpectedExit(format!("{name} writer task failed: {e}")))?
}

async fn read_pipe_to_string<R>(pipe: &mut R) -> Result<String, EnvError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    pipe.read_to_end(&mut buf).await.map_err(EnvError::Io)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn join_reader(
    handle: JoinHandle<Result<String, EnvError>>,
    name: &str,
) -> Result<String, EnvError> {
    handle
        .await
        .map_err(|e| EnvError::UnexpectedExit(format!("{name} reader task failed: {e}")))?
}

struct ProcessTreeGuard {
    pid: Option<u32>,
    armed: bool,
}

impl ProcessTreeGuard {
    const fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }

    async fn terminate_and_wait(&mut self, child: &mut Child) {
        let Some(pid) = self.pid else {
            self.disarm();
            return;
        };
        terminate_process_tree_async(pid).await;
        let child_reaped = tokio::time::timeout(FORCE_KILL_WAIT, child.wait())
            .await
            .is_ok();
        #[cfg(unix)]
        if child_reaped {
            wait_for_process_group_exit_async(pid).await;
        }
        if child_reaped {
            self.disarm();
        }
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(pid) = self.pid {
            terminate_process_tree_blocking(pid);
        }
    }
}

#[cfg(windows)]
async fn terminate_process_tree_async(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(windows)]
fn terminate_process_tree_blocking(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
async fn terminate_process_tree_async(pid: u32) {
    signal_process_group_async(pid, "TERM").await;
    tokio::time::sleep(TERMINATE_GRACE).await;
    signal_process_group_async(pid, "KILL").await;
}

#[cfg(unix)]
fn terminate_process_tree_blocking(pid: u32) {
    signal_process_group_blocking(pid, "TERM");
    std::thread::sleep(TERMINATE_GRACE);
    signal_process_group_blocking(pid, "KILL");
    wait_for_process_group_exit_blocking(pid);
}

#[cfg(all(not(windows), not(unix)))]
async fn terminate_process_tree_async(pid: u32) {
    let pid_s = pid.to_string();
    let _ = Command::new("kill")
        .args(["-TERM", &pid_s])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    tokio::time::sleep(TERMINATE_GRACE).await;
    let _ = Command::new("kill")
        .args(["-KILL", &pid_s])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(all(not(windows), not(unix)))]
fn terminate_process_tree_blocking(pid: u32) {
    let pid_s = pid.to_string();
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid_s])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::thread::sleep(TERMINATE_GRACE);
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &pid_s])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
async fn wait_for_process_group_exit_async(pid: u32) {
    let deadline = tokio::time::Instant::now() + FORCE_KILL_WAIT;
    while tokio::time::Instant::now() < deadline {
        if !process_group_alive_async(pid).await {
            return;
        }
        tokio::time::sleep(PROCESS_EXIT_POLL).await;
    }
}

#[cfg(unix)]
fn wait_for_process_group_exit_blocking(pid: u32) {
    let deadline = std::time::Instant::now() + FORCE_KILL_WAIT;
    while std::time::Instant::now() < deadline {
        if !process_group_alive_blocking(pid) {
            return;
        }
        std::thread::sleep(PROCESS_EXIT_POLL);
    }
}

#[cfg(unix)]
async fn signal_process_group_async(pid: u32, signal: &str) {
    let group = format!("-{pid}");
    let _ = Command::new("kill")
        .args([format!("-{signal}"), "--".to_owned(), group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(unix)]
fn signal_process_group_blocking(pid: u32, signal: &str) {
    let group = format!("-{pid}");
    let _ = std::process::Command::new("kill")
        .args([format!("-{signal}"), "--".to_owned(), group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
async fn process_group_alive_async(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn process_group_alive_blocking(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn isolate_process_tree(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn isolate_process_tree(_cmd: &mut Command) {}

#[cfg(windows)]
fn shell_command(shell: &str, command: &str) -> Command {
    let mut cmd = Command::new(shell);
    cmd.raw_arg("/C").raw_arg(command);
    cmd
}

#[cfg(not(windows))]
fn shell_command(shell: &str, command: &str) -> Command {
    let mut cmd = Command::new(shell);
    cmd.arg("-c").arg(command);
    cmd
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
        let command = if cfg!(windows) {
            "echo %RSA_TEST_VAR%"
        } else {
            "echo $RSA_TEST_VAR"
        };
        let mut req = RunRequest::new(command);
        req.env.insert("RSA_TEST_VAR".into(), "from_test".into());
        let r = env.run(req).await.unwrap();
        assert_eq!(r.stdout.trim(), "from_test");
    }

    #[tokio::test]
    async fn timeout_flags_timed_out() {
        let env = LocalEnvironment::new();
        let command = if cfg!(windows) {
            "powershell -NoProfile -Command Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let req = RunRequest::new(command).with_timeout(Duration::from_millis(100));
        let r = env.run(req).await.unwrap();
        assert!(r.timed_out);
    }

    #[tokio::test]
    async fn timeout_reclaims_stubborn_process_before_returning() {
        let work = tempfile::tempdir().unwrap();
        let pid_file = work.path().join("stubborn.pid");
        let command = stubborn_process_command(&pid_file);

        let env = LocalEnvironment::new();
        let req = RunRequest::new(command).with_timeout(Duration::from_secs(1));
        let r = env.run(req).await.unwrap();

        assert!(r.timed_out);
        let pid_text = std::fs::read_to_string(&pid_file).unwrap();
        let pid = pid_text.trim().parse::<u32>().unwrap();
        assert!(
            !process_is_alive(pid),
            "timed-out process {pid} was still alive after run returned"
        );
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

    #[cfg(windows)]
    fn stubborn_process_command(pid_file: &std::path::Path) -> String {
        let path = pid_file.display().to_string().replace('\'', "''");
        format!(
            "powershell -NoProfile -Command \"$pidFile='{path}'; Set-Content -LiteralPath $pidFile -Value $PID; while ($true) {{ Start-Sleep -Milliseconds 200 }}\""
        )
    }

    #[cfg(not(windows))]
    fn stubborn_process_command(pid_file: &std::path::Path) -> String {
        let path = sh_single_quote(&pid_file.display().to_string());
        format!("trap '' TERM; printf '%s' $$ > {path}; while :; do sleep 1; done")
    }

    #[cfg(not(windows))]
    fn sh_single_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    fn process_is_alive(pid: u32) -> bool {
        if cfg!(windows) {
            std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
                    ),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        } else {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }
    }
}
