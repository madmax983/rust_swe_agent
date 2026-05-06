//! Error types. One top-level `Error` composed of focused subsystem enums,
//! so call sites can match on the specific failure mode without a catch-all.
//!
//! # Error Handling Philosophy
//!
//! We believe that error messages are the first line of documentation.
//! If a function fails at 3 AM, the error should tell you exactly *why* and ideally *how* to fix it.
//! We use a flat overarching `Error` enum that delegates to specific domains (`ModelError`, `EnvError`, `ConfigError`).

#![deny(missing_docs)]

use thiserror::Error;

/// The main error type covering all sub-systems.
///
/// This enum groups all possible failure states of the agent loop into one neat package.
///
/// # Examples
///
/// ```
/// use rust_swe_agent::error::{Error, EnvError};
/// use std::time::Duration;
///
/// let err = Error::from(EnvError::Timeout(Duration::from_secs(5)));
/// match err {
///     Error::Env(EnvError::Timeout(duration)) => {
///         assert_eq!(duration.as_secs(), 5);
///     }
///     _ => panic!("Expected Env timeout error"),
/// }
/// ```
#[derive(Debug, Error)]
pub enum Error {
    /// Errors originating from the LLM backend (e.g., rate limits, malformed JSON).
    #[error(transparent)]
    Model(#[from] ModelError),

    /// Errors originating from the execution environment (e.g., Docker crashes, command timeouts).
    #[error(transparent)]
    Env(#[from] EnvError),

    /// Errors related to configuration loading and parsing.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The templating engine (minijinja) failed to render a prompt. Usually a syntax error in your system prompt.
    #[error("template render failed: {0}")]
    Template(String),

    /// Failed to write the final trajectory output to disk. Check your disk space and permissions.
    #[error("trajectory io: {0}")]
    Trajectory(String),

    /// Failed to interact with GitHub during PR creation or evaluation.
    #[error("github pr: {0}")]
    Github(String),

    /// Standard I/O errors that aren't specific to the environment.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Failed to parse JSON. This might happen when parsing standard input/output or local state files.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Errors originating from the LLM backend.
///
/// These are typically related to network issues, rate limits, or the LLM returning gibberish.
#[derive(Debug, Error)]
pub enum ModelError {
    /// The HTTP request to the model provider failed. Check your network connection and API endpoint.
    #[error("model request failed: {0}")]
    Request(String),

    /// The model returned a response, but it couldn't be parsed correctly (e.g., unexpected format).
    #[error("model returned malformed response: {0}")]
    Malformed(String),

    /// The model provider refused to process the request (often due to safety filters or context length limits).
    #[error("model refused request: {0}")]
    Refused(String),

    /// API keys or required authentication tokens were missing. Did you set your `.env` variables?
    #[error("missing credentials: {0}")]
    MissingCredentials(String),

    /// You have hit the API rate limit. Take a breather or upgrade your tier.
    #[error("rate limited: {0}")]
    RateLimited(String),
}

/// Errors originating from the execution environment.
///
/// This usually involves Docker or bash commands going haywire.
#[derive(Debug, Error)]
pub enum EnvError {
    /// A bash command executed in the environment failed (returned a non-zero exit code).
    #[error("command failed: {0}")]
    CommandFailed(String),

    /// A command ran for too long and was killed. Try increasing the timeout limit.
    #[error("command timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// We expected to use Docker, but it couldn't be found. Please install Docker and ensure it's in your PATH.
    #[error("docker not installed or not on PATH")]
    DockerNotInstalled,

    /// Docker is installed, but the daemon isn't running or we don't have permissions to talk to it.
    #[error("docker daemon unreachable: {0}")]
    DockerDaemonUnreachable(String),

    /// Failed to start the Docker container. Check the provided image name and container configurations.
    #[error("container start failed: {0}")]
    ContainerStartFailed(String),

    /// A background process or container exited abruptly and unexpectedly.
    #[error("unexpected process exit: {0}")]
    UnexpectedExit(String),

    /// Standard I/O failure within the environment interactions.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Errors related to configuration loading and parsing.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The requested configuration file could not be found at the given path.
    #[error("config file not found: {0}")]
    NotFound(String),

    /// The configuration file is not valid TOML. Double-check your syntax!
    #[error("toml parse failed: {0}")]
    Toml(String),

    /// Config includes are nested too deeply. You likely have a cyclic include!
    #[error("include chain exceeded {0} levels (possible cycle)")]
    IncludeDepthExceeded(usize),

    /// The configuration parsed correctly as TOML, but semantic validation failed.
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        Self::Toml(e.to_string())
    }
}
