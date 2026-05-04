//! Error types. One top-level `Error` composed of focused subsystem enums,
//! so call sites can match on the specific failure mode without a catch-all.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Model(#[from] ModelError),

    #[error(transparent)]
    Env(#[from] EnvError),

    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("template render failed: {0}")]
    Template(String),

    #[error("trajectory io: {0}")]
    Trajectory(String),

    #[error("github pr: {0}")]
    Github(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model request failed: {0}")]
    Request(String),

    #[error("model returned malformed response: {0}")]
    Malformed(String),

    #[error("model refused request: {0}")]
    Refused(String),

    #[error("missing credentials: {0}")]
    MissingCredentials(String),

    #[error("rate limited: {0}")]
    RateLimited(String),
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

    #[error("toml parse failed: {0}")]
    Toml(String),

    #[error("include chain exceeded {0} levels (possible cycle)")]
    IncludeDepthExceeded(usize),

    #[error("invalid config: {0}")]
    Invalid(String),
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        Self::Toml(e.to_string())
    }
}
