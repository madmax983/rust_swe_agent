//! The Chaos Engineer's favorite LLM tool: `ChaosModel`.
//!
//! This module provides a decorator for any [`Model`] that deterministically
//! injects simulated failures into LLM queries. It's designed to test the agent's
//! resilience to transient API errors (like rate limits or malformed responses)
//! without needing a flaky underlying network or service.
//!
//! By forcing `ModelError`s at set intervals, we can ensure the agent loop
//! gracefully recovers and falls back properly.

use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::ModelError;
use crate::model::{Message, Model, ModelResponse, QueryOpts};

/// A decorator that wraps an inner [`Model`] and injects failures deterministically.
///
/// It counts invocations and, on every Nth invocation, returns a synthesized error
/// rather than actually delegating the query to the underlying model.
pub struct ChaosModel {
    inner: Arc<dyn Model>,
    invocation_count: Arc<AtomicUsize>,
    fail_every: usize,
    error_type: ModelErrorType,
}

#[derive(Clone, Copy)]
pub enum ModelErrorType {
    RateLimited,
    Malformed,
    Refused,
}

impl ChaosModel {
    pub fn new(inner: Arc<dyn Model>, fail_every: usize, error_type: ModelErrorType) -> Self {
        Self {
            inner,
            invocation_count: Arc::new(AtomicUsize::new(0)),
            fail_every,
            error_type,
        }
    }
}

#[async_trait]
impl Model for ChaosModel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn skip_latency_telemetry(&self) -> bool {
        self.inner.skip_latency_telemetry()
    }

    async fn query(
        &self,
        messages: &[Message],
        opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        let count = self.invocation_count.fetch_add(1, Ordering::SeqCst) + 1;

        if self.fail_every > 0 && count % self.fail_every == 0 {
            return Err(match self.error_type {
                ModelErrorType::RateLimited => {
                    ModelError::RateLimited("simulated chaos failure: rate limited".to_string())
                }
                ModelErrorType::Malformed => {
                    ModelError::Malformed("simulated chaos failure: malformed response".to_string())
                }
                ModelErrorType::Refused => {
                    ModelError::Refused("simulated chaos failure: refused".to_string())
                }
            });
        }

        self.inner.query(messages, opts).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::QueryOpts;
    use crate::model::deterministic::DeterministicModel;

    #[tokio::test]
    async fn test_chaos_model_injects_failures() {
        let inner = Arc::new(DeterministicModel::new([
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]));
        let model = ChaosModel::new(inner, 3, ModelErrorType::RateLimited);

        // 1st request - Should succeed
        let res1 = model.query(&[], &QueryOpts::default()).await.unwrap();
        assert_eq!(res1.content, "a");

        // 2nd request - Should succeed
        let res2 = model.query(&[], &QueryOpts::default()).await.unwrap();
        assert_eq!(res2.content, "b");

        // 3rd request - Should fail (injected rate limit)
        let res3 = model.query(&[], &QueryOpts::default()).await;
        assert!(res3.is_err(), "Expected chaos error on 3rd request");

        // 4th request - Should succeed again
        let res4 = model.query(&[], &QueryOpts::default()).await.unwrap();
        assert_eq!(res4.content, "c");
    }
}
