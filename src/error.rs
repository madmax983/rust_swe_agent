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

    /// One or more operator-supplied verification checks did not pass.
    /// `(failed_count, total_count)`.
    #[error("verification failed: {0} of {1} check(s) did not pass")]
    VerificationFailed(usize, usize),
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

    /// All models in the fallback chain failed with transient errors.
    /// The message names every attempted model and its coarse failure reason.
    /// The structured attempt records are preserved for trajectory telemetry.
    #[error("all fallback candidates failed: {0}")]
    AllCandidatesFailed(String, Vec<FailedAttempt>),
}

/// A single failed attempt record carried inside `AllCandidatesFailed`.
/// Mirrors `model::FallbackAttemptRecord` without creating a cross-layer
/// import cycle between `error` and `model`.
#[derive(Debug, Clone)]
pub struct FailedAttempt {
    pub model: String,
    pub reason: String,
    pub retry_after_secs: Option<u64>,
}

impl ModelError {
    /// Returns `true` for errors that are worth retrying with a fallback model.
    ///
    /// Transient: rate limits, network/timeout failures, provider 5xx.
    /// Non-transient: bad credentials, bad model name, malformed request,
    /// context-window overflow, content policy, all-candidates-failed.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::RateLimited(_) | Self::Request(_))
    }

    /// Parse a Retry-After duration in seconds from a `RateLimited` error
    /// message. Returns `None` for all other variants or when no numeric
    /// Retry-After value is present in the message text.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        let Self::RateLimited(msg) = self else {
            return None;
        };
        let msg_lower = msg.to_ascii_lowercase();
        for prefix in ["retry-after: ", "retry_after: ", "retry after: "] {
            if let Some(pos) = msg_lower.find(prefix) {
                let digits: &str = msg[pos + prefix.len()..]
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap_or("");
                if let Ok(n) = digits.parse::<u64>() {
                    return Some(n);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_secs_parses_numeric_value_from_rate_limited() {
        let e = ModelError::RateLimited("rate limited retry-after: 30 please wait".into());
        assert_eq!(e.retry_after_secs(), Some(30));
    }

    #[test]
    fn retry_after_secs_returns_none_when_not_present() {
        let e = ModelError::RateLimited("too many requests".into());
        assert_eq!(e.retry_after_secs(), None);
    }

    #[test]
    fn retry_after_secs_returns_none_for_non_rate_limited_variants() {
        assert_eq!(ModelError::Malformed("bad".into()).retry_after_secs(), None);
        assert_eq!(ModelError::Request("err".into()).retry_after_secs(), None);
    }

    #[test]
    fn retry_after_secs_handles_underscore_variant() {
        let e = ModelError::RateLimited("retry_after: 60".into());
        assert_eq!(e.retry_after_secs(), Some(60));
    }

    #[test]
    fn retry_after_secs_handles_space_variant() {
        let e = ModelError::RateLimited("retry after: 90".into());
        assert_eq!(e.retry_after_secs(), Some(90));
    }

    #[test]
    fn retry_after_secs_handles_missing_digit_after_prefix() {
        let e = ModelError::RateLimited("retry-after: ".into());
        assert_eq!(e.retry_after_secs(), None);
    }

    #[test]
    fn is_transient_identifies_transient_and_non_transient_errors() {
        assert!(ModelError::RateLimited(String::new()).is_transient());
        assert!(ModelError::Request(String::new()).is_transient());
        assert!(!ModelError::Malformed(String::new()).is_transient());
        assert!(!ModelError::Refused(String::new()).is_transient());
        assert!(!ModelError::MissingCredentials(String::new()).is_transient());
        assert!(!ModelError::AllCandidatesFailed(String::new(), vec![]).is_transient());
    }
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
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        Self::Toml(e.to_string())
    }
}
