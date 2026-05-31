//! Error types. One top-level `Error` composed of focused subsystem enums,
//! so call sites can match on the specific failure mode without a catch-all.

use crate::model::ModelError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Model(#[from] ModelError),

    #[error(transparent)]
    Env(#[from] EnvError),

    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Template(#[from] minijinja::Error),

    #[error("trajectory io: {0}")]
    Trajectory(String),

    #[error("github pr: {0}")]
    Github(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// One or more operator-supplied verification checks did not pass.
    /// `(failed_count, total_count)`.
    #[error("verification failed: {0} of {1} check(s) did not pass")]
    VerificationFailed(usize, usize),

    /// Agent stagnation detected: the same bash action was repeated at least K
    /// times within the trailing W-step window. The trajectory is already
    /// finalized on disk with `failure_category = "agent_stagnation"` before
    /// this error is returned. Callers that aggregate results (e.g. the sweep
    /// runner) should read failure metadata from the trajectory; this error
    /// exists only so the CLI can exit with code 12.
    #[error("agent stagnation detected: action repeated {count} times in {window}-step window")]
    AgentStagnation { count: u32, window: u32 },

    /// Preflight gate failure (e.g. config drift with --fail-on-change).
    #[error("{0}")]
    Preflight(String),

    /// bench bisect budget exhausted
    #[error("bisect budget exhausted")]
    BisectBudgetExhausted,

    /// bench bisect schema break
    #[error("bisect schema break")]
    BisectSchemaBreak,

    /// bench audit failure
    #[error("audit: {0}")]
    Audit(String),
}

#[derive(Debug, Error)]
pub enum EnvError {
    #[error("command failed: {0}")]
    CommandFailed(String),

    #[error("command timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("docker not installed or not on PATH")]
    DockerNotInstalled,

    #[error("docker daemon unreachable: {0}")]
    DockerDaemonUnreachable(String),

    #[error("container start failed: {0}")]
    ContainerStartFailed(String),

    #[error("unexpected process exit: {0}")]
    UnexpectedExit(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(String),

    #[error("toml parse failed: {0} (see docs/config-reference.md for format and examples)")]
    Toml(String),

    #[error("include chain exceeded {0} levels (possible cycle)")]
    IncludeDepthExceeded(usize),

    #[error("invalid config: {0} (see docs/config-reference.md for valid values and examples)")]
    Invalid(String),

    #[error("{0}")]
    Usage(String),
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        Self::Toml(e.to_string())
    }
}
