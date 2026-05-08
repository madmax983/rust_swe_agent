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
                    });
                    // Continue to the next candidate.
                }
                Err(e) => {
                    // Non-transient: surface immediately, do not attempt fallback.
                    return Err(e);
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
mod tests {
    use super::*;
    use crate::model::ModelUsage;

    struct AlwaysOk(String);
    struct AlwaysFail(String, bool);

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

    #[test]
    fn name_returns_primary() {
        let f = FallbackModel::new(vec![
            Box::new(AlwaysOk("primary".into())),
            Box::new(AlwaysOk("secondary".into())),
        ]);
        assert_eq!(f.name(), "primary");
    }
}
