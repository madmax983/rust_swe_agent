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

    #[error(transparent)]
    Template(#[from] minijinja::Error),

    #[error("trajectory io: {0}")]
    Trajectory(String),

    #[error("github pr: {0}")]
    Github(String),

    #[error("github issue: {0}")]
    GithubIssue(#[from] crate::run::github_issue::GithubIssueError),

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

    /// Replay only: prompt fingerprint drift detected at step {0}.
    /// Recorded fingerprint and actual fingerprint differ.
    #[error("replay prompt drift at step {0}: fingerprints do not match")]
    ReplayDrift(usize),

    /// Generic scripted model exhaustion: the response queue ran out at step {0}.
    /// Used by `DeterministicModel` in any context (tests, sweeps, replay).
    /// Replay code translates this into `ScriptedResponsesExhausted` so that
    /// exit-code routing can distinguish replay-structural failures from ordinary
    /// test/sweep failures.
    #[error("scripted responses exhausted at step {0}")]
    ResponsesExhausted(u32),

    /// Replay only: the scripted-response queue is exhausted at step {0},
    /// meaning the cassette trajectory is structurally incompatible with the
    /// current agent (e.g. the agent issued more model calls than were recorded).
    #[error("replay response exhausted: no scripted response for step {0}")]
    ScriptedResponsesExhausted(u32),

    /// Replay only: the trajectory has no fingerprint at step {0} and
    /// `--allow-unfingerprinted` was not passed.
    #[error(
        "trajectory has no fingerprint at step {0}; \
         use --allow-unfingerprinted to permit replaying legacy trajectories"
    )]
    ReplayUnfingerprintedLegacy(usize),
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
