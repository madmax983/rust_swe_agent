//! `FallbackModel`: tries a primary model and falls back to alternates only
//! on transient provider failures (rate limits, network errors, 5xx).
//!
//! Non-transient failures (bad credentials, bad model name, malformed
//! request, context overflow, content policy) are returned immediately
//! without attempting any fallback candidate.
//!
//! Every `query` populates `ModelResponse::responding_model` with the
//! name of the model that actually produced the reply, and
//! `ModelResponse::fallback_attempts` with the ordered list of failed
//! attempts so callers can attribute cost and surface the chain in
//! trajectory artifacts.

use async_trait::async_trait;

use super::{FallbackAttemptRecord, Message, Model, ModelResponse, QueryOpts};
use crate::error::{FailedAttempt, ModelError};

pub struct FallbackModel {
    /// Ordered chain: index 0 is primary, the rest are fallback candidates.
    models: Vec<Box<dyn Model>>,
}

impl FallbackModel {
    /// Build a fallback chain. `models` must be non-empty; the first entry
    /// is the primary model.
    ///
    /// # Panics
    /// Panics if `models` is empty.
    pub fn new(models: Vec<Box<dyn Model>>) -> Self {
        assert!(
            !models.is_empty(),
            "FallbackModel requires at least one model"
        );
        Self { models }
    }
}

/// Coarse, redaction-safe reason string derived from a `ModelError`.
fn coarse_reason(e: &ModelError) -> String {
    match e {
        ModelError::RateLimited(_) => "rate_limited".into(),
        ModelError::Request(_) => "request_failed".into(),
        // These should never appear in a fallback chain since we only fall
        // back on transient errors, but handle defensively.
        ModelError::Malformed(_) => "malformed_response".into(),
        ModelError::Refused(_) => "refused".into(),
        ModelError::MissingCredentials(_) => "missing_credentials".into(),
        ModelError::AllCandidatesFailed(_, _) => "all_candidates_failed".into(),
        // Replay-only errors never appear in a live fallback chain.
        ModelError::ReplayDrift(_)
        | ModelError::ScriptedResponsesExhausted(_)
        | ModelError::ReplayUnfingerprintedLegacy(_) => "replay_error".into(),
    }
}

#[async_trait]
impl Model for FallbackModel {
    fn name(&self) -> &str {
        self.models[0].name()
    }

    fn supports_explicit_cache(&self) -> bool {
        self.models[0].supports_explicit_cache()
    }

    async fn query(
        &self,
        messages: &[Message],
        opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        let mut failed_attempts: Vec<FallbackAttemptRecord> = Vec::new();

        for model in &self.models {
            match model.query(messages, opts).await {
                Ok(mut resp) => {
                    resp.responding_model = Some(model.name().to_owned());
                    resp.fallback_attempts = failed_attempts;
                    return Ok(resp);
                }
                Err(e) if e.is_transient() => {
                    failed_attempts.push(FallbackAttemptRecord {
                        model: model.name().to_owned(),
                        failure_reason: coarse_reason(&e),
                        retry_after_secs: e.retry_after_secs(),
                    });
                    // Continue to the next candidate.
                }
                Err(e) => {
                    if failed_attempts.is_empty() {
                        // No prior transient attempts: surface the non-transient error directly.
                        return Err(e);
                    }
                    // Prior transient attempts exist: include this terminal failure in the
                    // structured record so DefaultAgent can write complete telemetry
                    // (including any swallowed 429s) even for mixed-failure chains.
                    failed_attempts.push(FallbackAttemptRecord {
                        model: model.name().to_owned(),
                        failure_reason: coarse_reason(&e),
                        retry_after_secs: e.retry_after_secs(),
                    });
                    let error_attempts: Vec<FailedAttempt> = failed_attempts
                        .iter()
                        .map(|a| FailedAttempt {
                            model: a.model.clone(),
                            reason: a.failure_reason.clone(),
                            retry_after_secs: a.retry_after_secs,
                        })
                        .collect();
                    let summary = failed_attempts
                        .iter()
                        .map(|a| format!("{}: {}", a.model, a.failure_reason))
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(ModelError::AllCandidatesFailed(summary, error_attempts));
                }
            }
        }

        // Every candidate exhausted via transient failures. Preserve structured
        // attempt records so DefaultAgent can write telemetry even on all-fail.
        let error_attempts: Vec<FailedAttempt> = failed_attempts
            .iter()
            .map(|a| FailedAttempt {
                model: a.model.clone(),
                reason: a.failure_reason.clone(),
                retry_after_secs: a.retry_after_secs,
            })
            .collect();
        let summary = failed_attempts
            .iter()
            .map(|a| format!("{}: {}", a.model, a.failure_reason))
            .collect::<Vec<_>>()
            .join("; ");
        Err(ModelError::AllCandidatesFailed(summary, error_attempts))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::ModelUsage;

    struct AlwaysOk(String);
    struct AlwaysFail(String, bool);
    /// Model that rate-limits with an explicit retry-after value embedded in
    /// the error message, used to verify that `retry_after_secs` is preserved.
    struct RateLimitWithRetryAfter(String, u64);

    #[async_trait]
    impl Model for AlwaysOk {
        fn name(&self) -> &str {
            &self.0
        }
        async fn query(&self, _: &[Message], _: &QueryOpts) -> Result<ModelResponse, ModelError> {
            Ok(ModelResponse {
                content: format!("ok-from-{}", self.0),
                usage: ModelUsage::default(),
                raw: serde_json::json!({}),
                responding_model: None,
                fallback_attempts: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl Model for AlwaysFail {
        fn name(&self) -> &str {
            &self.0
        }
        async fn query(&self, _: &[Message], _: &QueryOpts) -> Result<ModelResponse, ModelError> {
            if self.1 {
                Err(ModelError::RateLimited("429".into()))
            } else {
                Err(ModelError::MissingCredentials("invalid key".into()))
            }
        }
    }

    #[async_trait]
    impl Model for RateLimitWithRetryAfter {
        fn name(&self) -> &str {
            &self.0
        }
        async fn query(&self, _: &[Message], _: &QueryOpts) -> Result<ModelResponse, ModelError> {
            Err(ModelError::RateLimited(format!(
                "rate limited retry-after: {}",
                self.1
            )))
        }
    }

    #[tokio::test]
    async fn single_model_success_no_fallback() {
        let f = FallbackModel::new(vec![Box::new(AlwaysOk("m1".into()))]);
        let resp = f.query(&[], &QueryOpts::default()).await.unwrap();
        assert_eq!(resp.responding_model.as_deref(), Some("m1"));
        assert!(resp.fallback_attempts.is_empty());
    }

    #[tokio::test]
    async fn transient_triggers_secondary() {
        let f = FallbackModel::new(vec![
            Box::new(AlwaysFail("m1".into(), true)),
            Box::new(AlwaysOk("m2".into())),
        ]);
        let resp = f.query(&[], &QueryOpts::default()).await.unwrap();
        assert_eq!(resp.responding_model.as_deref(), Some("m2"));
        assert_eq!(resp.fallback_attempts.len(), 1);
        assert_eq!(resp.fallback_attempts[0].model, "m1");
        assert_eq!(resp.fallback_attempts[0].failure_reason, "rate_limited");
    }

    #[tokio::test]
    async fn non_transient_no_fallback() {
        let f = FallbackModel::new(vec![
            Box::new(AlwaysFail("m1".into(), false)),
            Box::new(AlwaysOk("m2".into())),
        ]);
        let err = f.query(&[], &QueryOpts::default()).await.unwrap_err();
        assert!(matches!(err, ModelError::MissingCredentials(_)));
    }

    #[tokio::test]
    async fn all_failed_is_compound_error() {
        let f = FallbackModel::new(vec![
            Box::new(AlwaysFail("m1".into(), true)),
            Box::new(AlwaysFail("m2".into(), true)),
        ]);
        let err = f.query(&[], &QueryOpts::default()).await.unwrap_err();
        let ModelError::AllCandidatesFailed(ref msg, ref attempts) = err else {
            panic!("expected AllCandidatesFailed, got {err:?}");
        };
        assert!(msg.contains("m1"));
        assert!(msg.contains("m2"));
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].model, "m1");
        assert_eq!(attempts[0].reason, "rate_limited");
        assert_eq!(attempts[1].model, "m2");
        assert_eq!(attempts[1].reason, "rate_limited");
    }

    #[tokio::test]
    async fn transient_then_non_transient_preserves_attempts() {
        // Primary: transient (429). Secondary: non-transient (auth failure).
        // The 429 must not be silently dropped — AllCandidatesFailed should
        // carry both attempts so DefaultAgent can write complete telemetry.
        let f = FallbackModel::new(vec![
            Box::new(AlwaysFail("m1".into(), true)),
            Box::new(AlwaysFail("m2".into(), false)),
        ]);
        let err = f.query(&[], &QueryOpts::default()).await.unwrap_err();
        let ModelError::AllCandidatesFailed(ref msg, ref attempts) = err else {
            panic!("expected AllCandidatesFailed, got {err:?}");
        };
        assert!(msg.contains("m1"), "summary must mention primary: {msg}");
        assert!(msg.contains("m2"), "summary must mention secondary: {msg}");
        assert_eq!(attempts.len(), 2, "both attempts must be preserved");
        assert_eq!(attempts[0].model, "m1");
        assert_eq!(attempts[0].reason, "rate_limited");
        assert_eq!(attempts[1].model, "m2");
    }

    #[tokio::test]
    async fn retry_after_secs_propagates_through_all_candidates_failed() {
        // Primary fails with an embedded retry-after value; verify that the
        // FailedAttempt inside AllCandidatesFailed carries retry_after_secs.
        let f = FallbackModel::new(vec![
            Box::new(RateLimitWithRetryAfter("m1".into(), 45)),
            Box::new(RateLimitWithRetryAfter("m2".into(), 30)),
        ]);
        let err = f.query(&[], &QueryOpts::default()).await.unwrap_err();
        let ModelError::AllCandidatesFailed(_, ref attempts) = err else {
            panic!("expected AllCandidatesFailed, got {err:?}");
        };
        assert_eq!(attempts[0].retry_after_secs, Some(45));
        assert_eq!(attempts[1].retry_after_secs, Some(30));
    }

    #[test]
    fn name_returns_primary() {
        let f = FallbackModel::new(vec![
            Box::new(AlwaysOk("primary".into())),
            Box::new(AlwaysOk("secondary".into())),
        ]);
        assert_eq!(f.name(), "primary");
    }

    #[test]
    fn supports_explicit_cache_delegates_to_primary() {
        let f = FallbackModel::new(vec![Box::new(AlwaysOk("m1".into()))]);
        // DeterministicModel returns false; FallbackModel must delegate.
        assert!(!f.supports_explicit_cache());
    }

    struct AlwaysFailRequest(String);

    #[async_trait]
    impl Model for AlwaysFailRequest {
        fn name(&self) -> &str {
            &self.0
        }
        async fn query(&self, _: &[Message], _: &QueryOpts) -> Result<ModelResponse, ModelError> {
            Err(ModelError::Request("network error".into()))
        }
    }

    #[tokio::test]
    async fn transient_request_error_triggers_fallback() {
        // Request errors are transient and should trigger fallback to secondary.
        let f = FallbackModel::new(vec![
            Box::new(AlwaysFailRequest("m1".into())),
            Box::new(AlwaysOk("m2".into())),
        ]);
        let resp = f.query(&[], &QueryOpts::default()).await.unwrap();
        assert_eq!(resp.responding_model.as_deref(), Some("m2"));
        assert_eq!(resp.fallback_attempts.len(), 1);
        assert_eq!(resp.fallback_attempts[0].failure_reason, "request_failed");
    }
}
