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
use litellm_rs::{
    CompletionOptions, LiteLLMError, ProviderError, assistant_message, completion, system_message,
    user_message,
};

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
    name.to_ascii_lowercase()
        .split('/')
        .any(|segment| segment.starts_with("claude") || segment.contains("anthropic.claude"))
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
            .map(|(m, _hint)| match m.role {
                // Future: when litellm-rs supports cache_control on MessageContent::Text,
                // apply `_hint` here.
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
            .map_err(classify_litellm_error)?;

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
            responding_model: None,
            fallback_attempts: Vec::new(),
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

/// Map a `LiteLLMError` (= `GatewayError`) to the coarser `ModelError`
/// taxonomy so `FallbackModel` can apply the right retry policy.
///
/// Transient → `RateLimited` or `Request`.
/// Non-transient → `MissingCredentials`, `Malformed`, or `Refused`.
fn classify_litellm_error(e: LiteLLMError) -> ModelError {
    match e {
        // Explicit rate-limit → always transient. Embed the structured
        // Retry-After seconds into the message text so that
        // `ModelError::retry_after_secs()` can recover it later (e.g.
        // when a fallback succeeds and the governor needs to set the floor).
        LiteLLMError::RateLimit {
            message,
            retry_after,
            ..
        } => {
            if let Some(secs) = retry_after {
                ModelError::RateLimited(format!("{message} retry-after: {secs}"))
            } else {
                ModelError::RateLimited(message)
            }
        }
        // Network / connectivity / service-unavailable / provider 5xx → transient.
        // Internal is used by litellm-rs for provider 5xx via api_error(500, …).
        LiteLLMError::Network(msg)
        | LiteLLMError::Unavailable(msg)
        | LiteLLMError::Timeout(msg)
        | LiteLLMError::Internal(msg) => ModelError::Request(msg),
        LiteLLMError::HttpClient(e) => ModelError::Request(e.to_string()),
        // Auth/credentials → non-transient; trying a different key won't help.
        LiteLLMError::Auth(msg) | LiteLLMError::Forbidden(msg) => {
            ModelError::MissingCredentials(msg)
        }
        // Bad request / bad model name → non-transient; retrying won't fix it.
        LiteLLMError::BadRequest(msg)
        | LiteLLMError::Validation(msg)
        | LiteLLMError::NotFound(msg) => ModelError::Malformed(msg),
        // Provider-level errors: delegate to litellm's own retryability judgment.
        LiteLLMError::Provider(ref e) => classify_provider_error(e, &format!("{e}")),
        // Everything else (Config, Serialization, …): treat as non-transient.
        _ => ModelError::Malformed(e.to_string()),
    }
}

fn classify_provider_error(e: &ProviderError, msg: &str) -> ModelError {
    // Provider-native 429s (RateLimit variant or ApiError{status:429}) must map
    // to RateLimited so the sweep governor tracks them even when a fallback
    // succeeds and swallows the error before it bubbles up.
    if e.http_status() == 429 {
        ModelError::RateLimited(msg.to_owned())
    } else if e.is_retryable() {
        ModelError::Request(msg.to_owned())
    } else {
        // Non-retryable provider errors: auth failures, content policy,
        // context-length overflow, bad model config, etc.
        ModelError::Malformed(msg.to_owned())
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
        assert!(is_anthropic_model("openrouter/anthropic.claude-sonnet-4-6"));
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

        let b_dotted = LitellmBackend::new("openrouter/anthropic.claude-opus-4-7");
        assert!(b_dotted.supports_explicit_cache());

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

    #[test]
    fn rate_limit_error_maps_to_rate_limited() {
        let e = LiteLLMError::RateLimit {
            message: "429".into(),
            retry_after: None,
            rpm_limit: None,
            tpm_limit: None,
        };
        assert!(matches!(
            classify_litellm_error(e),
            ModelError::RateLimited(_)
        ));
    }

    #[test]
    fn auth_error_maps_to_missing_credentials() {
        assert!(matches!(
            classify_litellm_error(LiteLLMError::Auth("401".into())),
            ModelError::MissingCredentials(_)
        ));
        assert!(matches!(
            classify_litellm_error(LiteLLMError::Forbidden("403".into())),
            ModelError::MissingCredentials(_)
        ));
    }

    #[test]
    fn bad_request_maps_to_malformed() {
        assert!(matches!(
            classify_litellm_error(LiteLLMError::BadRequest("bad model".into())),
            ModelError::Malformed(_)
        ));
        assert!(matches!(
            classify_litellm_error(LiteLLMError::NotFound("no such model".into())),
            ModelError::Malformed(_)
        ));
        assert!(matches!(
            classify_litellm_error(LiteLLMError::Validation("invalid param".into())),
            ModelError::Malformed(_)
        ));
    }

    #[test]
    fn network_and_unavailable_map_to_request() {
        assert!(matches!(
            classify_litellm_error(LiteLLMError::Network("timeout".into())),
            ModelError::Request(_)
        ));
        assert!(matches!(
            classify_litellm_error(LiteLLMError::Unavailable("5xx".into())),
            ModelError::Request(_)
        ));
        assert!(matches!(
            classify_litellm_error(LiteLLMError::Timeout("deadline".into())),
            ModelError::Request(_)
        ));
    }

    #[test]
    fn auth_and_bad_request_are_not_transient() {
        let auth = classify_litellm_error(LiteLLMError::Auth("bad key".into()));
        assert!(!auth.is_transient(), "auth errors must not be transient");
        let bad_req = classify_litellm_error(LiteLLMError::BadRequest("400".into()));
        assert!(!bad_req.is_transient(), "bad request must not be transient");
    }

    #[test]
    fn rate_limit_and_network_are_transient() {
        let rl = classify_litellm_error(LiteLLMError::RateLimit {
            message: "429".into(),
            retry_after: None,
            rpm_limit: None,
            tpm_limit: None,
        });
        assert!(rl.is_transient(), "rate limit must be transient");
        let net = classify_litellm_error(LiteLLMError::Network("conn refused".into()));
        assert!(net.is_transient(), "network errors must be transient");
    }

    #[test]
    fn internal_gateway_error_is_transient() {
        // GatewayError::Internal is used by litellm-rs for provider 5xx via api_error(500, …).
        assert!(matches!(
            classify_litellm_error(LiteLLMError::Internal("provider 500".into())),
            ModelError::Request(_)
        ));
    }

    #[test]
    fn provider_rate_limit_maps_to_rate_limited_not_request() {
        // ProviderError::RateLimit and ApiError{429} must reach the governor as
        // RateLimited, not as a generic transient Request.
        let rl = classify_provider_error(
            &ProviderError::rate_limit("test-provider", None),
            "429 from provider",
        );
        assert!(
            matches!(rl, ModelError::RateLimited(_)),
            "ProviderError::RateLimit should map to RateLimited, got {rl:?}"
        );

        let api_429 = classify_provider_error(
            &ProviderError::ApiError {
                provider: "test-provider",
                status: 429,
                message: "rate limited".into(),
            },
            "api 429",
        );
        assert!(
            matches!(api_429, ModelError::RateLimited(_)),
            "ProviderError::ApiError(429) should map to RateLimited, got {api_429:?}"
        );
    }

    #[test]
    fn rate_limit_with_structured_retry_after_embeds_value_in_message() {
        let e = classify_litellm_error(LiteLLMError::RateLimit {
            message: "too many requests".into(),
            retry_after: Some(45),
            rpm_limit: None,
            tpm_limit: None,
        });
        match e {
            crate::error::ModelError::RateLimited(msg) => {
                assert!(
                    msg.contains("retry-after: 45"),
                    "structured retry_after should be embedded in message: {msg}"
                );
                assert_eq!(
                    crate::error::ModelError::RateLimited(msg).retry_after_secs(),
                    Some(45),
                    "retry_after_secs should parse back the embedded value"
                );
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_without_retry_after_preserves_original_message() {
        let e = classify_litellm_error(LiteLLMError::RateLimit {
            message: "quota exceeded".into(),
            retry_after: None,
            rpm_limit: None,
            tpm_limit: None,
        });
        match e {
            crate::error::ModelError::RateLimited(msg) => {
                assert_eq!(msg, "quota exceeded");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }
}
