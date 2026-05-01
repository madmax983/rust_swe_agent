//! Trajectory serialization — wire-compatible with mini-swe-agent's
//! `mini-swe-agent-1.1` format.
//!
//! Field declaration order on structs controls JSON key order in
//! `serde_json` output, so we match Python's layout exactly: `format`,
//! `info`, `messages`.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::model::{Message, MessageExtra};

pub const FORMAT_VERSION: &str = "mini-swe-agent-1.1";

/// Coarse run outcome. Exactly one of three values, suitable for computing
/// pass@1-style metrics from trajectory files alone:
/// `"submitted"` | `"step_limit_reached"` | `"error"`.
#[cfg(any(feature = "markdown-export", feature = "csv-export"))]
pub mod export;

pub mod outcome {
    pub const SUBMITTED: &str = "submitted";
    pub const STEP_LIMIT_REACHED: &str = "step_limit_reached";
    pub const ERROR: &str = "error";
}

pub mod exit_reason {
    pub const WALLCLOCK_TIMEOUT: &str = "wallclock_timeout";
}

/// Closed set of non-success terminal failure modes for sweeps.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    EnvSetup,
    ModelApi,
    ModelParse,
    StepLimit,
    CostLimit,
    WallclockTimeout,
    AgentInternal,
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_creation_tokens: u64,
    pub completion_tokens: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrajectoryInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(flatten, default)]
    pub other: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "extra_is_empty")]
    pub extra: MessageExtra,
}

fn extra_is_empty(e: &MessageExtra) -> bool {
    e.actions.is_none()
        && e.cost.is_none()
        && e.response.is_none()
        && e.timestamp.is_none()
        && e.other.is_empty()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trajectory {
    pub trajectory_format: String,
    pub info: TrajectoryInfo,
    pub messages: Vec<MessageRecord>,
}

impl Default for Trajectory {
    fn default() -> Self {
        Self {
            trajectory_format: FORMAT_VERSION.to_owned(),
            info: TrajectoryInfo::default(),
            messages: Vec::new(),
        }
    }
}

impl Trajectory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_message(&mut self, m: &Message) {
        self.messages.push(MessageRecord {
            role: role_to_string(m.role),
            content: m.content.clone(),
            extra: m.extra.clone(),
        });
    }

    pub fn record_with_extra(&mut self, m: &Message, extra: MessageExtra) {
        self.messages.push(MessageRecord {
            role: role_to_string(m.role),
            content: m.content.clone(),
            extra,
        });
    }

    pub fn save_pretty(&self, path: &Path) -> Result<(), crate::error::Error> {
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        Ok(())
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

fn role_to_string(r: crate::model::Role) -> String {
    match r {
        crate::model::Role::System => "system".into(),
        crate::model::Role::User => "user".into(),
        crate::model::Role::Assistant => "assistant".into(),
        crate::model::Role::Tool => "tool".into(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::{Message, Role};

    #[test]
    fn format_key_ordering() {
        let mut t = Trajectory::new();
        t.info.task = Some("t".into());
        t.record_message(&Message::system("s"));

        let json = t.to_json_pretty().unwrap();
        let format_pos = json.find("\"trajectory_format\"").unwrap();
        let info_pos = json.find("\"info\"").unwrap();
        let messages_pos = json.find("\"messages\"").unwrap();
        assert!(format_pos < info_pos);
        assert!(info_pos < messages_pos);
    }

    #[test]
    fn roundtrip_through_json() {
        let mut t = Trajectory::new();
        t.info.task = Some("hello".into());
        t.info.steps = Some(2);

        let mut asst = Message::assistant("ok");
        asst.extra.actions = Some(vec!["echo hi".into()]);
        asst.extra.cost = Some(0.0001);
        t.record_message(&Message::system("sys"));
        t.record_message(&Message {
            role: Role::Assistant,
            content: asst.content.clone(),
            cache_hint: crate::model::CacheHint::default(),
            extra: asst.extra,
        });

        let json = t.to_json_pretty().unwrap();
        let back: Trajectory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.trajectory_format, FORMAT_VERSION);
        assert_eq!(back.info.task.as_deref(), Some("hello"));
        assert_eq!(back.messages.len(), 2);
        assert_eq!(
            back.messages[1].extra.actions.as_deref(),
            Some(&["echo hi".to_owned()][..])
        );
    }

    #[test]
    fn outcome_token_usage_duration_round_trip() {
        let mut t = Trajectory::new();
        t.info.outcome = Some(outcome::SUBMITTED.into());
        t.info.token_usage = Some(TokenUsage {
            prompt_tokens: 1234,
            cache_read_tokens: 567,
            cache_creation_tokens: 89,
            completion_tokens: 56,
        });
        t.info.duration_secs = Some(12.5);

        let json = t.to_json_pretty().unwrap();
        assert!(json.contains("\"outcome\": \"submitted\""));
        assert!(json.contains("\"prompt_tokens\": 1234"));
        assert!(json.contains("\"cache_read_tokens\": 567"));
        assert!(json.contains("\"cache_creation_tokens\": 89"));
        assert!(json.contains("\"completion_tokens\": 56"));
        assert!(json.contains("\"duration_secs\": 12.5"));

        let back: Trajectory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.info.outcome.as_deref(), Some("submitted"));
        assert_eq!(
            back.info.token_usage,
            Some(TokenUsage {
                prompt_tokens: 1234,
                cache_read_tokens: 567,
                cache_creation_tokens: 89,
                completion_tokens: 56
            })
        );
        assert_eq!(back.info.duration_secs, Some(12.5));
    }

    #[test]
    fn legacy_token_usage_without_cache_fields_defaults_to_zero() {
        let json = r#"{
  "trajectory_format": "mini-swe-agent-1.1",
  "info": {
    "token_usage": {
      "prompt_tokens": 1000,
      "completion_tokens": 25
    }
  },
  "messages": []
}"#;
        let back: Trajectory = serde_json::from_str(json).unwrap();
        assert_eq!(
            back.info.token_usage,
            Some(TokenUsage {
                prompt_tokens: 1000,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                completion_tokens: 25,
            })
        );
    }

    #[test]
    fn new_fields_optional_omitted_when_none() {
        let t = Trajectory::new();
        let json = t.to_json_pretty().unwrap();
        assert!(!json.contains("outcome"));
        assert!(!json.contains("token_usage"));
        assert!(!json.contains("duration_secs"));
    }

    #[test]
    fn unknown_keys_preserved() {
        let json = r#"{
  "trajectory_format": "mini-swe-agent-1.1",
  "info": {"task": "x", "future_field": "y"},
  "messages": []
}"#;
        let t: Trajectory = serde_json::from_str(json).unwrap();
        assert_eq!(
            t.info.other.get("future_field"),
            Some(&serde_json::json!("y"))
        );
    }
}
