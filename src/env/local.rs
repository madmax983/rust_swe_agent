//! Local shell environment. Spawns `bash -lc <cmd>` via `tokio::process`,
//! collects stdout/stderr, enforces timeout, converts SIGKILL-from-timeout
//! into `timed_out = true` rather than an error.

use async_trait::async_trait;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
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

type PipeCollector = JoinHandle<Result<(), EnvError>>;
type PipeBuffer = Arc<Mutex<Vec<u8>>>;

pub struct LocalEnvironment {
    shell: String,
    pub workdir: Option<std::path::PathBuf>,
}

impl Default for LocalEnvironment {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            workdir: None,
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

    #[must_use]
    pub fn with_workdir(mut self, workdir: Option<std::path::PathBuf>) -> Self {
        self.workdir = workdir;
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
            if cwd.is_relative() {
                if let Some(ref wd) = self.workdir {
                    cmd.current_dir(wd.join(cwd));
                } else {
                    cmd.current_dir(cwd);
                }
            } else {
                cmd.current_dir(cwd);
            }
        } else if let Some(ref wd) = self.workdir {
            cmd.current_dir(wd);
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
            Some(stdin) => {
                let pipe = child
                    .stdin
                    .take()
                    .ok_or_else(|| EnvError::UnexpectedExit("stdin pipe missing".into()))?;
                Some(spawn_stdin_writer(pipe, stdin))
            }
            None => None,
        };

        // Take pipes so we can read them concurrently with `wait`.
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("stdout pipe missing".into()))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| EnvError::UnexpectedExit("stderr pipe missing".into()))?;
        let (stdout_task, stdout_buffer) = spawn_pipe_collector(stdout_pipe);
        let (stderr_task, stderr_buffer) = spawn_pipe_collector(stderr_pipe);

        match wait_for_child(&mut child, req.timeout, req.cancellation).await? {
            ChildStop::Exited(status) => {
                process_guard.disarm();
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
                process_guard.terminate_and_wait(&mut child).await;
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
                process_guard.terminate_and_wait(&mut child).await;
                abort_stdin_writer(stdin_task).await;
                let stdout = partial_reader_output(stdout_task, stdout_buffer).await;
                let stderr = partial_reader_output(stderr_task, stderr_buffer).await;
                Ok(RunResult {
                    stdout,
                    stderr: append_status_message(stderr, "cancelled"),
                    exit_code: -1,
                    timed_out: false,
                })
            }
        }
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
    let max_size = 16 * 1024 * 1024;
    loop {
        let n = pipe.read(&mut chunk).await.map_err(EnvError::Io)?;
        if n == 0 {
            return Ok(());
        }
        let mut lock = buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if lock.len() + n > max_size {
            let remain = max_size.saturating_sub(lock.len());
            if remain > 0 {
                lock.extend_from_slice(&chunk[..remain]);
            }
            lock.extend_from_slice(b"\n[Output truncated due to 16MB size limit]");
            return Ok(());
        }
        lock.extend_from_slice(&chunk[..n]);
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
        let _ = tokio::time::timeout(FORCE_KILL_WAIT, terminate_process_tree_async(pid)).await;
        let _ = child.start_kill();
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
        .spawn();
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
        let mut req = RunRequest::new(env_echo_command());
        req.env.insert("RSA_TEST_VAR".into(), "from_test".into());
        let r = env.run(req).await.unwrap();
        assert_eq!(r.stdout.trim(), "from_test");
    }

    #[tokio::test]
    async fn stdin_is_passed_to_child() {
        let env = LocalEnvironment::new();
        let req = RunRequest::new(stdin_echo_command()).with_stdin("from stdin\n");
        let r = env.run(req).await.unwrap();
        assert_eq!(r.stdout.trim(), "from stdin");
    }

    #[tokio::test]
    async fn exited_child_preserves_output_when_stdin_pipe_breaks() {
        let env = LocalEnvironment::new();
        let req = RunRequest::new(exit_without_reading_stdin_command())
            .with_stdin("x".repeat(16 * 1024 * 1024));

        let r = env.run(req).await.unwrap();

        assert_eq!(r.exit_code, 7);
        assert_eq!(r.stdout.trim(), "child stdout");
        assert_eq!(r.stderr.trim(), "child stderr");
        assert!(!r.timed_out);
    }

    #[tokio::test]
    async fn timeout_flags_timed_out() {
        let env = LocalEnvironment::new();
        let req = RunRequest::new(sleep_command()).with_timeout(Duration::from_millis(100));
        let r = env.run(req).await.unwrap();
        assert!(r.timed_out);
    }

    #[cfg(not(windows))]
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
            process_exits_within(pid, Duration::from_secs(3)),
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

    #[tokio::test]
    async fn relative_cwd_joined_with_workdir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workdir_path = temp_dir.path().to_path_buf();
        let subdir_path = workdir_path.join("subdir");
        std::fs::create_dir(&subdir_path).unwrap();

        let env = LocalEnvironment::new().with_workdir(Some(workdir_path));
        let mut req = RunRequest::new("echo hello > test.txt");
        req.cwd = Some(std::path::PathBuf::from("subdir"));

        let r = env.run(req).await.unwrap();
        assert_eq!(r.exit_code, 0);

        let target_file = subdir_path.join("test.txt");
        assert!(
            target_file.exists(),
            "test.txt should have been written to the joined relative path: {target_file:?}"
        );
        let contents = std::fs::read_to_string(&target_file).unwrap();
        assert!(contents.contains("hello"));
    }

    fn env_echo_command() -> &'static str {
        if cfg!(windows) {
            "echo %RSA_TEST_VAR%"
        } else {
            "echo $RSA_TEST_VAR"
        }
    }

    fn stdin_echo_command() -> &'static str {
        if cfg!(windows) { "more" } else { "cat" }
    }

    fn exit_without_reading_stdin_command() -> &'static str {
        if cfg!(windows) {
            "echo child stdout & echo child stderr 1>&2 & exit /B 7"
        } else {
            "printf 'child stdout\n'; printf 'child stderr\n' >&2; exit 7"
        }
    }

    fn sleep_command() -> &'static str {
        if cfg!(windows) {
            "for /L %i in (1,0,2) do @rem"
        } else {
            "sleep 5"
        }
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

    #[cfg(not(windows))]
    fn process_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(not(windows))]
    fn process_exits_within(pid: u32, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if !process_is_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        !process_is_alive(pid)
    }
}
