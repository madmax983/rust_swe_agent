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
pub mod fallback;
pub mod litellm;

pub use deterministic::DeterministicModel;
pub use fallback::FallbackModel;
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

    /// Wall-clock spent inside the model provider call that produced this
    /// assistant turn. Absent on turns where the model produced no
    /// measurable latency (deterministic fixtures, replay).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model_latency_ms: Option<u64>,

    /// Wall-clock spent executing the tool/bash command for the
    /// observation turn that follows the assistant proposal. Absent on
    /// turns that did not run a tool (format error, policy block, etc.).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_latency_ms: Option<u64>,

    /// Harness-side wall-clock between the previous turn's measurement
    /// boundary and this turn's measurement boundary: rate-limit waits,
    /// retries, redaction, file IO, template rendering. Always recorded
    /// for live runs; absent on replays where it cannot be reconstructed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub harness_overhead_ms: Option<u64>,

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
        && e.model_latency_ms.is_none()
        && e.tool_latency_ms.is_none()
        && e.harness_overhead_ms.is_none()
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

/// Record of a single failed model attempt within a fallback chain.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FallbackAttemptRecord {
    /// Name of the model that was attempted.
    pub model: String,
    /// Coarse failure reason, safe for logs and artifacts.
    pub failure_reason: String,
    /// Retry-After seconds parsed from the rate-limit error, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: String,
    pub usage: ModelUsage,
    pub raw: serde_json::Value,
    /// The model that actually produced this response. `None` when the
    /// primary model responded normally (single-model path, no fallback).
    /// `FallbackModel` always populates this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responding_model: Option<String>,
    /// Failed attempts before this response. Empty when no fallback occurred.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_attempts: Vec<FallbackAttemptRecord>,
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

    /// Signals that wall-clock measurements of `query` are not meaningful
    /// for this backend (deterministic fixtures, in-memory replay). The
    /// agent loop uses this to *omit* `MessageExtra.model_latency_ms`
    /// instead of writing a near-zero value that would be indistinguishable
    /// from a fast real backend on inspection.
    fn skip_latency_telemetry(&self) -> bool {
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
/// + long-lived context) and demote the rest to `None`. Returns an iterator
/// over `(&Message, CacheHint)`.
pub fn cap_breakpoints<const N: usize>(
    messages: &[Message],
) -> impl Iterator<Item = (&Message, CacheHint)> + '_ {
    let mut kept = 0usize;
    messages.iter().map(move |m| {
        let mut hint = m.cache_hint;
        if matches!(hint, CacheHint::Breakpoint) {
            if kept >= N {
                hint = CacheHint::None;
            } else {
                kept += 1;
            }
        }
        (m, hint)
    })
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

        let capped: Vec<_> = cap_breakpoints::<4>(&msgs).collect();
        let bp_count = capped
            .iter()
            .filter(|(_, hint)| matches!(hint, &CacheHint::Breakpoint))
            .count();
        assert_eq!(bp_count, 4);
        // First four keep Breakpoint; last two get demoted.
        for (_, hint) in &capped[..4] {
            assert!(matches!(hint, CacheHint::Breakpoint));
        }
        for (_, hint) in &capped[4..] {
            assert!(matches!(hint, CacheHint::None));
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
        let capped: Vec<_> = cap_breakpoints::<4>(&msgs).collect();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].1, CacheHint::Breakpoint);
        assert_eq!(capped[1].1, CacheHint::None);
    }
}
#[cfg(test)]
pub mod deterministic_chaos_test;
