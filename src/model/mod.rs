//! The `Model` trait and its supporting types.
//!
//! Prompt caching is deliberately a first-class trait concern: cache intent
//! rides on the `Message` as a `CacheHint`, and cache effects come back in
//! `ModelUsage`. Backends interpret hints as they see fit — Anthropic maps
//! them to explicit breakpoints, OpenAI silently ignores them (OAI does
//! automatic caching without client control), deterministic models just
//! record them verbatim in the trajectory.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::ModelError;

pub mod deterministic;
pub mod litellm;

pub use deterministic::DeterministicModel;
pub use litellm::{AnthropicBackend, LitellmBackend};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Advisory: tells the backend whether this message's content is worth
/// keeping warm in a prompt cache, and whether a breakpoint marker belongs
/// here.
///
/// - `None`: no hint. Default.
/// - `Auto`: cache this if convenient (short-lived, rolling).
/// - `Breakpoint`: explicit long-lived cache marker. Anthropic-style.
///   Backends that don't model breakpoints ignore this or fold to Auto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHint {
    #[default]
    None,
    Auto,
    Breakpoint,
}

/// Per-message extra payload. Keeps the trajectory format forward-compatible
/// with Python mini-swe-agent emitting extra keys we don't know about yet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MessageExtra {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub actions: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cost: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timestamp: Option<String>,

    #[serde(flatten, default)]
    pub other: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "cache_hint_is_default")]
    pub cache_hint: CacheHint,
    #[serde(default, skip_serializing_if = "message_extra_is_empty")]
    pub extra: MessageExtra,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // Serde demands `fn(&T) -> bool`.
fn cache_hint_is_default(h: &CacheHint) -> bool {
    matches!(h, CacheHint::None)
}

fn message_extra_is_empty(e: &MessageExtra) -> bool {
    e.actions.is_none()
        && e.cost.is_none()
        && e.response.is_none()
        && e.timestamp.is_none()
        && e.other.is_empty()
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            cache_hint: CacheHint::Breakpoint,
            extra: MessageExtra::default(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            cache_hint: CacheHint::None,
            extra: MessageExtra::default(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            cache_hint: CacheHint::None,
            extra: MessageExtra::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: String,
    pub usage: ModelUsage,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct QueryOpts {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Extra provider-specific knobs — passed through opaquely.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[async_trait]
pub trait Model: Send + Sync {
    fn name(&self) -> &str;

    /// Does this backend/model honor explicit cache breakpoints? Used by
    /// `InteractiveAgent` for the status display — zero behavioral effect.
    fn supports_explicit_cache(&self) -> bool {
        false
    }

    async fn query(
        &self,
        messages: &[Message],
        opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError>;
}

/// Anthropic caps explicit cache breakpoints at 4. Enforce defensively:
/// if more than 4 messages carry `Breakpoint`, keep the **first** four
/// (stable/oldest cached prefix, which is what we want for a system prompt
/// + long-lived context) and demote the rest to `None`. Returns the
/// possibly-rewritten message list.
pub fn cap_breakpoints<const N: usize>(messages: &[Message]) -> Vec<Message> {
    let mut kept = 0usize;
    messages
        .iter()
        .cloned()
        .map(|mut m| {
            if matches!(m.cache_hint, CacheHint::Breakpoint) {
                if kept >= N {
                    m.cache_hint = CacheHint::None;
                } else {
                    kept += 1;
                }
            }
            m
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_breakpoints_demotes_excess() {
        let msgs: Vec<Message> = (0..6)
            .map(|i| {
                let mut m = Message::user(format!("m{i}"));
                m.cache_hint = CacheHint::Breakpoint;
                m
            })
            .collect();

        let capped = cap_breakpoints::<4>(&msgs);
        let bp_count = capped
            .iter()
            .filter(|m| matches!(m.cache_hint, CacheHint::Breakpoint))
            .count();
        assert_eq!(bp_count, 4);
        // First four keep Breakpoint; last two get demoted.
        for m in &capped[..4] {
            assert!(matches!(m.cache_hint, CacheHint::Breakpoint));
        }
        for m in &capped[4..] {
            assert!(matches!(m.cache_hint, CacheHint::None));
        }
    }

    #[test]
    fn cap_breakpoints_passthrough_under_limit() {
        let msgs = vec![
            {
                let mut m = Message::system("s");
                m.cache_hint = CacheHint::Breakpoint;
                m
            },
            Message::user("u"),
        ];
        let capped = cap_breakpoints::<4>(&msgs);
        assert_eq!(capped, msgs);
    }
}
