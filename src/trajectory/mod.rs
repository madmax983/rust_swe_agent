//! Trajectory serialization — wire-compatible with mini-swe-agent's
//! `mini-swe-agent-1.1` format.
//!
//! Field declaration order on structs controls JSON key order in
//! `serde_json` output, so we match Python's layout exactly: `format`,
//! `info`, `messages`.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::model::{Message, MessageExtra};

/// The current format version string used for trajectory files.
pub const FORMAT_VERSION: &str = "mini-swe-agent-1.1";

/// Coarse run outcome. Exactly one of three values, suitable for computing
/// pass@1-style metrics from trajectory files alone:
/// `"submitted"` | `"step_limit_reached"` | `"error"`.
#[cfg(any(feature = "markdown-export", feature = "csv-export"))]
pub mod export;

/// Pre-defined outcome constants.
pub mod outcome {
    /// Indicates the agent completed its work and submitted a solution.
    pub const SUBMITTED: &str = "submitted";
    /// Indicates the agent hit the maximum allowed number of steps.
    pub const STEP_LIMIT_REACHED: &str = "step_limit_reached";
    /// Indicates the agent encountered an unrecoverable error.
    pub const ERROR: &str = "error";
}

/// Pre-defined exit reason constants.
pub mod exit_reason {
    /// Indicates the agent's run timed out.
    pub const WALLCLOCK_TIMEOUT: &str = "wallclock_timeout";
}

/// Closed set of non-success terminal failure modes for sweeps.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// Environment setup failed (e.g., could not create workspace or install dependencies).
    EnvSetup,
    /// An error occurred communicating with the model API (e.g., network error, 500 response).
    ModelApi,
    /// The model's response could not be parsed into a valid action.
    ModelParse,
    /// The agent exhausted its allowed number of turns.
    StepLimit,
    /// The agent exceeded its monetary budget.
    CostLimit,
    /// The agent run took too much real-world time.
    WallclockTimeout,
    /// An internal bug in the agent loop itself caused a crash.
    AgentInternal,
    /// Some other unknown error occurred.
    Unknown,
}

/// Token usage tracking.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    /// The number of tokens consumed by the prompt/input.
    pub prompt_tokens: u64,
    /// The number of tokens generated in the completion/output.
    pub completion_tokens: u64,
}

/// Metadata about the agent's run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrajectoryInfo {
    /// The specific task instance the agent was attempting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// The name of the underlying language model used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Why the agent stopped running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_reason: Option<String>,
    /// Categorization of the failure, if the run failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    /// The final outcome of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Any final output provided by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    /// The total cost of model queries in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// Token usage metrics for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    /// How long the run took, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// The number of steps the agent completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    /// A timestamp of when the run started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// A timestamp of when the run ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Any additional unstructured information.
    #[serde(flatten, default)]
    pub other: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A single message exchanged during a trajectory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    /// The role of the message sender (e.g., "user", "assistant", "system", "tool").
    pub role: String,
    /// The text content of the message.
    pub content: String,
    /// Additional metadata attached to the message.
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

/// Represents the complete history and metadata of an agent's run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trajectory {
    /// The string identifier for the schema version of this trajectory file.
    pub trajectory_format: String,
    /// High-level metadata about the task, outcome, and resources consumed.
    pub info: TrajectoryInfo,
    /// The chronological list of messages exchanged between the user, agent, and tools.
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
    /// Creates a new, empty trajectory.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use rust_swe_agent::trajectory::Trajectory;
    ///
    /// let traj = Trajectory::new();
    /// assert_eq!(traj.messages.len(), 0);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a new message to the trajectory.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use rust_swe_agent::trajectory::Trajectory;
    /// use rust_swe_agent::model::Message;
    ///
    /// let mut traj = Trajectory::new();
    /// traj.record_message(&Message::user("Hello, agent!"));
    /// assert_eq!(traj.messages.len(), 1);
    /// ```
    pub fn record_message(&mut self, m: &Message) {
        self.messages.push(MessageRecord {
            role: role_to_string(m.role),
            content: m.content.clone(),
            extra: m.extra.clone(),
        });
    }

    /// Appends a new message to the trajectory with explicit extra metadata.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use rust_swe_agent::trajectory::Trajectory;
    /// use rust_swe_agent::model::{Message, MessageExtra};
    ///
    /// let mut traj = Trajectory::new();
    /// let msg = Message::assistant("I am thinking...");
    /// let mut extra = MessageExtra::default();
    /// extra.cost = Some(0.02);
    ///
    /// traj.record_with_extra(&msg, extra);
    /// assert_eq!(traj.messages[0].extra.cost, Some(0.02));
    /// ```
    pub fn record_with_extra(&mut self, m: &Message, extra: MessageExtra) {
        self.messages.push(MessageRecord {
            role: role_to_string(m.role),
            content: m.content.clone(),
            extra,
        });
    }

    /// Writes the trajectory to the given path as pretty-printed JSON.
    ///
    /// ## Errors
    /// Returns an error if the file cannot be written.
    pub fn save_pretty(&self, path: &Path) -> Result<(), crate::error::Error> {
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        Ok(())
    }

    /// Serializes the trajectory to a pretty-printed JSON string.
    ///
    /// ## Errors
    /// Returns an error if serialization fails.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use rust_swe_agent::trajectory::Trajectory;
    ///
    /// let traj = Trajectory::new();
    /// let json = traj.to_json_pretty().unwrap();
    /// assert!(json.contains("mini-swe-agent-1.1"));
    /// ```
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
            completion_tokens: 56,
        });
        t.info.duration_secs = Some(12.5);

        let json = t.to_json_pretty().unwrap();
        assert!(json.contains("\"outcome\": \"submitted\""));
        assert!(json.contains("\"prompt_tokens\": 1234"));
        assert!(json.contains("\"completion_tokens\": 56"));
        assert!(json.contains("\"duration_secs\": 12.5"));

        let back: Trajectory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.info.outcome.as_deref(), Some("submitted"));
        assert_eq!(
            back.info.token_usage,
            Some(TokenUsage {
                prompt_tokens: 1234,
                completion_tokens: 56
            })
        );
        assert_eq!(back.info.duration_secs, Some(12.5));
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
