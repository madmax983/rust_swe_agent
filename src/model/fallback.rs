use async_trait::async_trait;

use crate::error::ModelError;
use crate::model::{Message, Model, ModelResponse, QueryOpts};

/// A model decorator that tries a primary model, and if it fails, falls back to a secondary model.
pub struct FallbackModel {
    primary: Box<dyn Model>,
    secondary: Box<dyn Model>,
}

impl FallbackModel {
    pub fn new(primary: Box<dyn Model>, secondary: Box<dyn Model>) -> Self {
        Self { primary, secondary }
    }
}

#[async_trait]
impl Model for FallbackModel {
    fn name(&self) -> &'static str {
        "fallback"
    }

    fn supports_explicit_cache(&self) -> bool {
        self.primary.supports_explicit_cache()
    }

    async fn query(
        &self,
        messages: &[Message],
        opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        match self.primary.query(messages, opts).await {
            Ok(res) => Ok(res),
            Err(e) => {
                tracing::warn!("primary model failed: {}, falling back to secondary", e);
                self.secondary.query(messages, opts).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::deterministic::DeterministicModel;

    #[tokio::test]
    async fn fallback_uses_primary_on_success() {
        let primary = Box::new(DeterministicModel::new(vec!["primary_success".into()]));
        let secondary = Box::new(DeterministicModel::new(vec!["secondary_success".into()]));
        let model = FallbackModel::new(primary, secondary);

        let res = model.query(&[], &QueryOpts::default()).await.unwrap();
        assert_eq!(res.content, "primary_success");
    }

    #[tokio::test]
    async fn fallback_uses_secondary_on_primary_failure() {
        let primary = Box::new(DeterministicModel::new(vec![]));
        let secondary = Box::new(DeterministicModel::new(vec!["secondary_success".into()]));
        let model = FallbackModel::new(primary, secondary);

        let res = model.query(&[], &QueryOpts::default()).await.unwrap();
        assert_eq!(res.content, "secondary_success");
    }

    #[tokio::test]
    async fn fallback_fails_if_both_fail() {
        let primary = Box::new(DeterministicModel::new(vec![]));
        let secondary = Box::new(DeterministicModel::new(vec![]));
        let model = FallbackModel::new(primary, secondary);

        let res = model.query(&[], &QueryOpts::default()).await;
        assert!(res.is_err());
    }
}
