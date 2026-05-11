//! Scripted model for tests. Emits a pre-programmed sequence of responses.
//! Tracks how many calls were made — test code can assert on it.

use async_trait::async_trait;
use std::sync::Mutex;

use super::{Message, Model, ModelResponse, ModelUsage, QueryOpts};
use crate::error::ModelError;

pub struct DeterministicModel {
    name: String,
    responses: Mutex<std::collections::VecDeque<String>>,
    call_count: Mutex<u32>,
    record: Mutex<Vec<Vec<Message>>>,
    usage_per_call: ModelUsage,
}

impl DeterministicModel {
    pub fn new(responses: impl IntoIterator<Item = String>) -> Self {
        Self::with_usage(
            responses,
            ModelUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_usd: Some(0.0),
            },
        )
    }

    /// Construct a `DeterministicModel` that reports `usage_per_call` on
    /// every `query`. Lets sweep-budget tests trigger budget halts
    /// deterministically by making the scripted backend look like it spent
    /// non-zero tokens / dollars per call.
    pub fn with_usage(
        responses: impl IntoIterator<Item = String>,
        usage_per_call: ModelUsage,
    ) -> Self {
        Self {
            name: "deterministic".to_owned(),
            responses: Mutex::new(responses.into_iter().collect()),
            call_count: Mutex::new(0),
            record: Mutex::new(Vec::new()),
            usage_per_call,
        }
    }

    pub fn call_count(&self) -> u32 {
        *self
            .call_count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn recorded_inputs(&self) -> Vec<Vec<Message>> {
        self.record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Model for DeterministicModel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn query(
        &self,
        messages: &[Message],
        _opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        {
            let mut rec = self
                .record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            rec.push(messages.to_vec());
        }
        {
            let mut count = self
                .call_count
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *count += 1;
        }

        // call_count was just incremented above; subtract 1 for 0-indexed step.
        let step_index = {
            let count = self
                .call_count
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            count.saturating_sub(1)
        };

        let content = {
            let mut q = self
                .responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            q.pop_front()
                .ok_or(ModelError::ScriptedResponsesExhausted(step_index))?
        };

        // Sentinel: "__rate_limited__:N" → ModelError::RateLimited with
        // "retry-after: N" so the sweep's rate-limit governor test harness
        // can inject 429 responses deterministically without a real provider.
        if let Some(secs_str) = content.strip_prefix("__rate_limited__:") {
            let msg = format!("rate limited: retry-after: {secs_str}");
            return Err(ModelError::RateLimited(msg));
        }

        // Default usage is all zeroes — replay runs and CI tests produce
        // valid trajectories without implying any real API spend. Tests
        // that need to exercise cost/budget paths inject non-zero usage
        // via `with_usage`.
        Ok(ModelResponse {
            content,
            usage: self.usage_per_call.clone(),
            raw: serde_json::json!({"deterministic": true}),
            responding_model: None,
            fallback_attempts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scripted_responses_consumed_in_order() {
        let m = DeterministicModel::new(["a".into(), "b".into()]);
        let opts = QueryOpts::default();
        let r1 = m.query(&[], &opts).await.unwrap_or_else(|_| panic!());
        let r2 = m.query(&[], &opts).await.unwrap_or_else(|_| panic!());
        assert_eq!(r1.content, "a");
        assert_eq!(r2.content, "b");
        assert_eq!(m.call_count(), 2);
    }

    #[tokio::test]
    async fn errors_when_exhausted() {
        let m = DeterministicModel::new(std::iter::empty());
        let r = m.query(&[], &QueryOpts::default()).await;
        assert!(r.is_err());
    }
}
