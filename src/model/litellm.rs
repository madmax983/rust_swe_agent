//! `AnthropicBackend`: a direct `reqwest`-based client for Anthropic's
//! Messages API. See the note in `Cargo.toml` about why we did not use the
//! `litellm-rs` crate from crates.io (it's a full proxy *server*, not a
//! client library).
//!
//! This module translates our `Message` + `CacheHint` → Anthropic's message
//! format and their `cache_control: { type: "ephemeral" }` block markers.
//! We enforce Anthropic's 4-breakpoint cap defensively in our wrapper.
//!
//! The module is kept narrow on purpose — everything provider-specific lives
//! here; the agent loop stays backend-agnostic.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{
    CacheHint, Message, Model, ModelResponse, ModelUsage, QueryOpts, Role, cap_breakpoints,
};
use crate::error::ModelError;

const ANTHROPIC_API: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const BREAKPOINT_CAP: usize = 4;

pub struct AnthropicBackend {
    model: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    max_tokens_default: u32,
}

impl AnthropicBackend {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Result<Self, ModelError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| ModelError::Request(e.to_string()))?;
        Ok(Self {
            model: model.into(),
            api_key: api_key.into(),
            base_url: ANTHROPIC_API.to_owned(),
            client,
            max_tokens_default: 4096,
        })
    }

    /// For tests: point at a mock server.
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    #[must_use]
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens_default = n;
        self
    }
}

/// Prefix dispatch: route by leading token of the model name. We currently
/// only *serve* Anthropic, but the dispatch helper lives here so the backend
/// map has an obvious home when we add more.
pub fn is_anthropic_model(name: &str) -> bool {
    let n = name.strip_prefix("anthropic/").unwrap_or(name);
    n.starts_with("claude")
}

#[derive(Serialize)]
struct AnthropicSystemBlock<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct AnthropicContentBlock<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: Vec<AnthropicContentBlock<'a>>,
}

#[derive(Serialize, Clone, Copy)]
struct CacheControl {
    #[serde(rename = "type")]
    ty: &'static str,
}

impl CacheControl {
    const EPHEMERAL: Self = Self { ty: "ephemeral" };
}

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<AnthropicSystemBlock<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Deserialize, Debug)]
struct AnthropicResponse {
    content: Vec<AnthropicResponseBlock>,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Deserialize, Debug, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize, Debug)]
struct AnthropicResponseBlock {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    text: String,
}

fn cache_control_for(hint: CacheHint) -> Option<CacheControl> {
    match hint {
        CacheHint::None => None,
        // Auto and Breakpoint both map to ephemeral — the only kind Anthropic
        // exposes today. The difference between them is one of intent: the
        // agent uses Breakpoint for stable long-lived content and Auto for
        // rolling windows. Anthropic doesn't distinguish, so neither do we.
        CacheHint::Auto | CacheHint::Breakpoint => Some(CacheControl::EPHEMERAL),
    }
}

#[async_trait]
impl Model for AnthropicBackend {
    fn name(&self) -> &str {
        &self.model
    }

    fn supports_explicit_cache(&self) -> bool {
        true
    }

    async fn query(
        &self,
        messages: &[Message],
        opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        let capped = cap_breakpoints::<BREAKPOINT_CAP>(messages);

        let mut system_blocks: Vec<AnthropicSystemBlock<'_>> = Vec::new();
        let mut body_msgs: Vec<AnthropicMessage<'_>> = Vec::new();

        for m in &capped {
            match m.role {
                Role::System => {
                    system_blocks.push(AnthropicSystemBlock {
                        ty: "text",
                        text: &m.content,
                        cache_control: cache_control_for(m.cache_hint),
                    });
                }
                Role::User | Role::Tool => {
                    body_msgs.push(AnthropicMessage {
                        role: "user",
                        content: vec![AnthropicContentBlock {
                            ty: "text",
                            text: &m.content,
                            cache_control: cache_control_for(m.cache_hint),
                        }],
                    });
                }
                Role::Assistant => {
                    body_msgs.push(AnthropicMessage {
                        role: "assistant",
                        content: vec![AnthropicContentBlock {
                            ty: "text",
                            text: &m.content,
                            cache_control: cache_control_for(m.cache_hint),
                        }],
                    });
                }
            }
        }

        let req = AnthropicRequest {
            model: &self.model,
            max_tokens: opts.max_tokens.unwrap_or(self.max_tokens_default),
            messages: body_msgs,
            system: system_blocks,
            temperature: opts.temperature,
        };

        let resp = self
            .client
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&req)
            .send()
            .await
            .map_err(|e| ModelError::Request(e.to_string()))?;

        let status = resp.status();
        let raw_text = resp
            .text()
            .await
            .map_err(|e| ModelError::Request(e.to_string()))?;

        if !status.is_success() {
            if status.as_u16() == 429 {
                return Err(ModelError::RateLimited(raw_text));
            }
            return Err(ModelError::Request(format!("HTTP {status}: {raw_text}")));
        }

        let parsed: AnthropicResponse = serde_json::from_str(&raw_text)
            .map_err(|e| ModelError::Malformed(format!("{e}: {raw_text}")))?;
        let raw_json: serde_json::Value =
            serde_json::from_str(&raw_text).map_err(|e| ModelError::Malformed(e.to_string()))?;

        let content = parsed
            .content
            .iter()
            .filter(|b| b.ty == "text")
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("");

        Ok(ModelResponse {
            content,
            usage: ModelUsage {
                input_tokens: parsed.usage.input_tokens,
                output_tokens: parsed.usage.output_tokens,
                cache_read_tokens: parsed.usage.cache_read_input_tokens,
                cache_creation_tokens: parsed.usage.cache_creation_input_tokens,
                cost_usd: None, // We do not maintain a price table; leave None.
            },
            raw: raw_json,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_prefix_dispatch() {
        assert!(is_anthropic_model("claude-opus-4-7"));
        assert!(is_anthropic_model("anthropic/claude-sonnet-4-6"));
        assert!(!is_anthropic_model("gpt-4"));
        assert!(!is_anthropic_model("openrouter/meta/llama-3"));
    }

    #[test]
    fn cache_control_mapping() {
        assert!(cache_control_for(CacheHint::None).is_none());
        assert!(cache_control_for(CacheHint::Auto).is_some());
        assert!(cache_control_for(CacheHint::Breakpoint).is_some());
    }
}
