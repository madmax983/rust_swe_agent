//! Environment abstraction — how the agent runs shell commands.
//!
//! Stateless per command: `run` takes `&self`, not `&mut self`. Each command
//! is independent; the environment is a tool, not a session.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;

use crate::error::EnvError;

#[cfg(feature = "chaos")]
pub mod chaos;
#[cfg(feature = "docker")]
pub mod docker;
pub mod local;

#[cfg(feature = "chaos")]
pub use chaos::ChaosEnvironment;
#[cfg(feature = "docker")]
pub use docker::DockerEnvironment;
pub use local::LocalEnvironment;

#[derive(Clone)]
pub struct CancellationToken {
    rx: watch::Receiver<bool>,
}

impl CancellationToken {
    pub fn new(rx: watch::Receiver<bool>) -> Self {
        Self { rx }
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    pub async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        while self.rx.changed().await.is_ok() {
            if self.is_cancelled() {
                return;
            }
        }
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(with = "humantime_serde_compat")]
    pub timeout: Duration,
    #[serde(skip)]
    pub cancellation: Option<CancellationToken>,
}

impl RunRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            stdin: None,
            cwd: None,
            env: BTreeMap::new(),
            timeout: Duration::from_secs(60),
            cancellation: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    #[must_use]
    pub fn with_stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

impl RunResult {
    /// Stitch stdout and stderr together in the order mini-swe-agent does.
    /// Python mini uses `{output}\n{stderr}` when both are present.
    pub fn combined_output(&self) -> String {
        match (self.stdout.is_empty(), self.stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => self.stdout.clone(),
            (true, false) => self.stderr.clone(),
            (false, false) => format!("{}\n{}", self.stdout, self.stderr),
        }
    }
}

#[async_trait]
pub trait Environment: Send + Sync {
    async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError>;

    /// Graceful shutdown. For environments without persistent state (like
    /// `LocalEnvironment`), this is a no-op.
    async fn shutdown(&mut self) -> Result<(), EnvError> {
        Ok(())
    }
}

/// Minimal serde adapter so `Duration` round-trips through YAML/JSON as
/// seconds. Avoids pulling in `humantime-serde`.
mod humantime_serde_compat {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_combined_output() {
        struct TestCase {
            stdout: &'static str,
            stderr: &'static str,
            expected: &'static str,
        }

        let cases = vec![
            TestCase {
                stdout: "",
                stderr: "",
                expected: "",
            },
            TestCase {
                stdout: "hello",
                stderr: "",
                expected: "hello",
            },
            TestCase {
                stdout: "",
                stderr: "world",
                expected: "world",
            },
            TestCase {
                stdout: "hello",
                stderr: "world",
                expected: "hello\nworld",
            },
        ];

        for case in cases {
            let result = RunResult {
                stdout: case.stdout.to_string(),
                stderr: case.stderr.to_string(),
                exit_code: 0,
                timed_out: false,
            };
            assert_eq!(result.combined_output(), case.expected);
        }
    }
}
