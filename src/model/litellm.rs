//! `LitellmBackend`: wraps `litellm-rs`'s `completion()` for multi-provider
//! dispatch (OpenAI, Anthropic, Azure, Google, OpenRouter, etc.).
//!
//! Routing is by model-name prefix and is handled inside `litellm-rs`.
//! API credentials come from env vars the way Python LiteLLM expects them
//! (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, ...). We don't pass them
//! explicitly.
//!
//! ## Cache hints
//! Our `CacheHint::Auto`/`Breakpoint` are advisory. We still apply the
//! 4-breakpoint cap defensively before handing off — this preserves the
//! architectural invariant ("agent loop is naive about caps") even where
//! the upstream doesn't currently propagate cache markers on plain-text
//! parts. `litellm-rs` 0.4.16's `ContentPart::Text` has no
//! `cache_control` field, so explicit per-text-block cache markers are a
//! known gap there. The Anthropic provider still benefits from
//! prompt-prefix automatic caching when enabled.
//!
//! ## Cost
//! After a successful call we ask `litellm-rs`'s built-in cost calculator
//! (`generic_cost_per_token`) for a USD figure. If the model isn't in its
//! pricing table, `cost_usd` stays `None`.

use async_trait::async_trait;

use litellm_rs::core::cost::{UsageTokens, generic_cost_per_token};
use litellm_rs::{CompletionOptions, assistant_message, completion, system_message, user_message};

use super::{Message, Model, ModelResponse, ModelUsage, QueryOpts, Role, cap_breakpoints};
use crate::error::ModelError;

const BREAKPOINT_CAP: usize = 4;

pub struct LitellmBackend {
    model: String,
    /// Optional override for `max_tokens`. Falls back to `QueryOpts.max_tokens`.
    default_max_tokens: Option<u32>,
}

impl LitellmBackend {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            default_max_tokens: None,
        }
    }

    #[must_use]
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.default_max_tokens = Some(n);
        self
    }
}

/// Anthropic-family detection. Used by the agent and CLI to decide whether
/// to advertise explicit-cache support — `litellm-rs` will route correctly
/// either way.
pub fn is_anthropic_model(name: &str) -> bool {
    name.split('/').any(|segment| segment.starts_with("claude"))
}

/// Provider routing: same convention `litellm-rs` uses internally.
fn parse_provider(model: &str) -> (&str, &str) {
    if let Some(idx) = model.find('/') {
        let (provider, rest) = model.split_at(idx);
        (provider, &rest[1..])
    } else if model.starts_with("claude") {
        ("anthropic", model)
    } else {
        ("openai", model)
    }
}

#[async_trait]
impl Model for LitellmBackend {
    fn name(&self) -> &str {
        &self.model
    }

    fn supports_explicit_cache(&self) -> bool {
        is_anthropic_model(&self.model)
    }

    async fn query(
        &self,
        messages: &[Message],
        opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        // Defensive 4-breakpoint cap. Even though `litellm-rs` 0.4.16
        // doesn't currently surface per-text-part cache markers, we keep
        // the policy because (a) it's the architectural contract our
        // `Model` trait advertises, and (b) future versions of litellm-rs
        // may propagate hints through `extra_params`.
        let capped = cap_breakpoints::<BREAKPOINT_CAP>(messages);

        let lite_msgs = capped
            .iter()
            .map(|m| match m.role {
                Role::System => system_message(m.content.clone()),
                Role::Assistant => assistant_message(m.content.clone()),
                // Tool messages map to user role for litellm-rs's flat
                // completion API; the `tool_call_id` round-trip would need
                // the multi-block path which we don't use.
                Role::User | Role::Tool => user_message(m.content.clone()),
            })
            .collect::<Vec<_>>();

        let lite_opts = CompletionOptions {
            temperature: opts.temperature,
            max_tokens: opts.max_tokens.or(self.default_max_tokens),
            ..CompletionOptions::default()
        };

        let resp = completion(&self.model, lite_msgs, Some(lite_opts))
            .await
            .map_err(|e| ModelError::Request(e.to_string()))?;

        let choice = resp
            .choices
            .first()
            .ok_or_else(|| ModelError::Malformed("response had zero choices".into()))?;

        let content = extract_text_content(choice);

        let (input_tokens, output_tokens, cache_read_tokens) =
            resp.usage.as_ref().map_or((0, 0, 0), |u| {
                let cached = u64::from(
                    u.prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0),
                );
                let (input_tokens, cache_read_tokens) =
                    split_prompt_usage(u64::from(u.prompt_tokens), cached);
                (
                    input_tokens,
                    u64::from(u.completion_tokens),
                    cache_read_tokens,
                )
            });

        // Cost: try the built-in calculator. If the model isn't in the
        // price table we leave `cost_usd = None` rather than guess.
        let cost_usd = resp.usage.as_ref().and_then(|u| {
            let (provider, base_model) = parse_provider(&self.model);
            let usage_tokens = UsageTokens::from(u.clone());
            generic_cost_per_token(base_model, &usage_tokens, provider)
                .ok()
                .map(|breakdown| {
                    breakdown.input_cost
                        + breakdown.output_cost
                        + breakdown.cache_cost
                        + breakdown.reasoning_cost
                })
        });

        let raw = serde_json::to_value(&resp)
            .map_err(|e| ModelError::Malformed(format!("response not JSON-serializable: {e}")))?;

        Ok(ModelResponse {
            content,
            usage: ModelUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens: 0, // Not surfaced by litellm-rs Usage.
                cost_usd,
            },
            raw,
        })
    }
}

fn extract_text_content(choice: &litellm_rs::Choice) -> String {
    use litellm_rs::core::types::content::ContentPart;
    use litellm_rs::core::types::message::MessageContent;

    match &choice.message.content {
        Some(MessageContent::Text(t)) => t.clone(),
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

// LiteLLM commonly reports `prompt_tokens` as the full prompt total while
// also surfacing cached reads separately, so normalize to the uncached split
// that the rest of the codebase expects.
fn split_prompt_usage(prompt_tokens: u64, cached_tokens: u64) -> (u64, u64) {
    if cached_tokens <= prompt_tokens {
        (prompt_tokens - cached_tokens, cached_tokens)
    } else {
        (prompt_tokens, cached_tokens)
    }
}

/// Backwards-compatible alias for code that previously referenced the
/// direct-reqwest `AnthropicBackend`. The single `LitellmBackend` now
/// covers all providers; `is_anthropic_model` still gates the explicit-cache
/// claim in the trait.
pub type AnthropicBackend = LitellmBackend;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_anthropic_models() {
        assert!(is_anthropic_model("claude-opus-4-7"));
        assert!(is_anthropic_model("anthropic/claude-sonnet-4-6"));
        assert!(is_anthropic_model("openrouter/anthropic/claude-sonnet-4-6"));
        assert!(!is_anthropic_model("gpt-4"));
        assert!(!is_anthropic_model("openrouter/meta/llama-3"));
    }

    #[test]
    fn parses_provider_prefix() {
        assert_eq!(parse_provider("gpt-4o"), ("openai", "gpt-4o"));
        assert_eq!(
            parse_provider("anthropic/claude-opus-4-7"),
            ("anthropic", "claude-opus-4-7")
        );
        assert_eq!(
            parse_provider("claude-opus-4-7"),
            ("anthropic", "claude-opus-4-7")
        );
        assert_eq!(parse_provider("openrouter/x/y"), ("openrouter", "x/y"));
    }

    #[test]
    fn backend_advertises_capabilities() {
        let b = LitellmBackend::new("claude-opus-4-7").with_max_tokens(2048);
        assert_eq!(b.name(), "claude-opus-4-7");
        assert!(b.supports_explicit_cache());

        let b_prefixed = LitellmBackend::new("openrouter/anthropic/claude-opus-4-7");
        assert!(b_prefixed.supports_explicit_cache());

        let b2 = LitellmBackend::new("gpt-4");
        assert!(!b2.supports_explicit_cache());
    }

    #[test]
    fn split_prompt_usage_subtracts_cached_subset_from_input_tokens() {
        let (input_tokens, cache_read_tokens) = split_prompt_usage(1_000, 800);
        assert_eq!(input_tokens, 200);
        assert_eq!(cache_read_tokens, 800);
    }

    #[test]
    fn split_prompt_usage_preserves_prompt_tokens_when_cached_exceeds_prompt_total() {
        let (input_tokens, cache_read_tokens) = split_prompt_usage(100, 800);
        assert_eq!(input_tokens, 100);
        assert_eq!(cache_read_tokens, 800);
    }
}
