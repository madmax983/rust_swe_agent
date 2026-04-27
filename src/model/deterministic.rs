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
}

impl DeterministicModel {
    pub fn new(responses: impl IntoIterator<Item = String>) -> Self {
        Self {
            name: "deterministic".to_owned(),
            responses: Mutex::new(responses.into_iter().collect()),
            call_count: Mutex::new(0),
            record: Mutex::new(Vec::new()),
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

        let content = {
            let mut q = self
                .responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            q.pop_front().ok_or_else(|| {
                ModelError::Malformed("deterministic model: no scripted response left".into())
            })?
        };

        // Zero token counts: replay runs and CI tests must produce valid
        // trajectories without implying any real API spend.
        Ok(ModelResponse {
            content,
            usage: ModelUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_usd: Some(0.0),
            },
            raw: serde_json::json!({"deterministic": true}),
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
