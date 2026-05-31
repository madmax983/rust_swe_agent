use super::FallbackAttemptRecord;
use thiserror::Error;

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
    AllCandidatesFailed(String, Vec<FallbackAttemptRecord>),

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
