//! Trajectory serialization — wire-compatible with mini-swe-agent's
//! `mini-swe-agent-1.1` format.
//!
//! Field declaration order on structs controls JSON key order in
//! `serde_json` output, so we match Python's layout exactly: `format`,
//! `info`, `messages`.

use regex::Regex;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use std::path::Path;

use crate::cost::CostSource;
use crate::model::{Message, MessageExtra};

pub use crate::model::FallbackAttemptRecord;

pub const FORMAT_VERSION: &str = "mini-swe-agent-1.1";

/// Trajectory-level summary of fallback behavior for a single agent run.
///
/// Present only when `model.fallback_models` was configured; absent for
/// single-model runs so legacy artifact consumers see no change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FallbackSummary {
    /// The model name from `config.model.name` (requested primary).
    pub primary_model: String,
    /// The model that produced the last successful response.
    pub final_model: String,
    /// `true` when at least one fallback attempt was made.
    pub fallback_happened: bool,
    /// Number of failed transient attempts before the final success.
    pub fallback_count: u32,
    /// All model names tried in order (primary first).
    pub attempted_models: Vec<String>,
    /// Per-attempt failure records for the failed attempts.
    pub failed_attempts: Vec<FallbackAttemptRecord>,
}

/// Coarse run outcome. Exactly one of three values, suitable for computing
/// pass@1-style metrics from trajectory files alone:
/// `"submitted"` | `"step_limit_reached"` | `"error"`.
pub mod export;

pub mod outcome {
    pub const SUBMITTED: &str = "submitted";
    pub const STEP_LIMIT_REACHED: &str = "step_limit_reached";
    pub const ERROR: &str = "error";
    pub const BUDGET_EXHAUSTED: &str = "budget_exhausted";
}

pub mod exit_reason {
    pub const CANCELLED: &str = "cancelled";
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
    /// Per-task USD ceiling was reached mid-loop. The harness terminated the
    /// agent; any patch accumulated before the cap fired is preserved.
    BudgetExhausted,
    WallclockTimeout,
    AgentInternal,
    /// Patch was captured but `git apply --check` rejected it at capture time.
    PatchApplyInvalid,
    /// Agent submitted but the captured diff was empty (zero bytes).
    PatchEmpty,
    /// A configured secret literal was found in a submission artifact.
    SecretLeakDetected,
    Unknown,
}

pub const DEFAULT_TEST_COMMAND_PATTERNS: &[&str] = &[
    "pytest",
    "python -m pytest",
    "python -m unittest",
    "tox",
    "nox",
    "make test",
    "make check",
    "cargo test",
    "go test",
    "npm test",
    "npm run test",
    "yarn test",
    "mvn test",
    "mvn -Dtest=",
    "gradle test",
    "./gradlew test",
];

#[derive(Debug, Clone)]
pub struct TestCommandPattern {
    source: String,
    matcher: TestCommandMatcher,
}

#[derive(Debug, Clone)]
enum TestCommandMatcher {
    Literal,
    Regex(Regex),
}

impl TestCommandPattern {
    fn literal(source: &str) -> Self {
        Self {
            source: source.to_owned(),
            matcher: TestCommandMatcher::Literal,
        }
    }

    fn regex(source: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            source: source.to_owned(),
            matcher: TestCommandMatcher::Regex(Regex::new(source)?),
        })
    }

    fn matches(&self, segment: &str) -> bool {
        match &self.matcher {
            TestCommandMatcher::Literal => {
                command_segment_starts_with_pattern(segment, &self.source)
            }
            TestCommandMatcher::Regex(regex) => {
                regex_matches_command_segment_start(regex, segment, &self.source)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestInvocation {
    pub step_index: u32,
    pub command: String,
    pub exit_code: i32,
    pub matched_pattern: String,
}

pub fn effective_test_command_patterns(
    extra_patterns: &[String],
    replace_defaults: bool,
) -> Result<Vec<TestCommandPattern>, regex::Error> {
    let mut patterns = if replace_defaults {
        Vec::new()
    } else {
        DEFAULT_TEST_COMMAND_PATTERNS
            .iter()
            .map(|pattern| TestCommandPattern::literal(pattern))
            .collect()
    };
    for pattern in extra_patterns {
        patterns.push(TestCommandPattern::regex(pattern)?);
    }
    Ok(patterns)
}

#[must_use]
pub fn detect_test_command(command: &str, patterns: &[TestCommandPattern]) -> Option<String> {
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    let mut segment_start = 0usize;
    let mut chars = command.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && !single_quoted {
            escaped = true;
            continue;
        }
        if ch == '\'' && !double_quoted {
            single_quoted = !single_quoted;
            continue;
        }
        if ch == '"' && !single_quoted {
            double_quoted = !double_quoted;
            continue;
        }
        if single_quoted || double_quoted {
            continue;
        }

        if ch == ';' || ch == '\n' {
            if let Some(pattern) =
                detect_test_command_segment(&command[segment_start..idx], patterns)
            {
                return Some(pattern);
            }
            segment_start = idx + ch.len_utf8();
            continue;
        }

        if (ch == '&' || ch == '|') && chars.peek().is_some_and(|(_, next)| *next == ch) {
            if let Some(pattern) =
                detect_test_command_segment(&command[segment_start..idx], patterns)
            {
                return Some(pattern);
            }
            let (_, next) = chars.next().unwrap_or((idx, ch));
            segment_start = idx + ch.len_utf8() + next.len_utf8();
            continue;
        }

        if ch == '|' {
            if let Some(pattern) =
                detect_test_command_segment(&command[segment_start..idx], patterns)
            {
                return Some(pattern);
            }
            segment_start = idx + ch.len_utf8();
        }
    }

    detect_test_command_segment(&command[segment_start..], patterns)
}

fn detect_test_command_segment(segment: &str, patterns: &[TestCommandPattern]) -> Option<String> {
    let segment = trim_command_prefix(segment);
    if segment.starts_with('"') || segment.starts_with('\'') {
        return None;
    }
    patterns
        .iter()
        .find(|pattern| pattern.matches(segment))
        .map(|pattern| pattern.source.clone())
}

fn trim_command_prefix(mut segment: &str) -> &str {
    segment = segment.trim_start();
    while let Some(rest) = segment.strip_prefix('(') {
        segment = rest.trim_start();
    }
    segment
}

fn command_segment_starts_with_pattern(segment: &str, pattern: &str) -> bool {
    let Some(rest) = segment.strip_prefix(pattern) else {
        return false;
    };
    if rest.is_empty() || pattern.ends_with('=') {
        return true;
    }
    rest.chars().next().is_some_and(is_command_boundary)
}

fn regex_matches_command_segment_start(regex: &Regex, segment: &str, pattern: &str) -> bool {
    let Some(matched) = regex.find(segment) else {
        return false;
    };
    if matched.start() != 0 {
        return false;
    }
    let rest = &segment[matched.end()..];
    rest.is_empty()
        || pattern.ends_with('=')
        || rest.chars().next().is_some_and(is_command_boundary)
}

fn is_command_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, ')' | ';' | '&' | '|' | '<' | '>')
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

impl TokenUsage {
    #[must_use]
    pub fn total_prompt_tokens(&self) -> u64 {
        self.prompt_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    #[must_use]
    pub fn has_cached_prompt_tokens(&self) -> bool {
        self.cache_read_tokens > 0 || self.cache_creation_tokens > 0
    }
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
    pub actual_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_cost_source: Option<CostSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_cost_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction: Option<crate::redaction::RedactionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default)]
    pub test_invocations: Vec<TestInvocation>,
    #[serde(default)]
    pub tests_run_before_submit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tests_passed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Command-policy telemetry: counts of allowed/asked/blocked/yolo commands.
    #[serde(default, skip_serializing_if = "crate::policy::PolicyCounts::is_empty")]
    pub policy_counts: crate::policy::PolicyCounts,
    /// Fallback telemetry for runs that used `model.fallback_models`. `None`
    /// for single-model runs (preserves legacy artifact shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_summary: Option<FallbackSummary>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct Trajectory {
    pub trajectory_format: String,
    pub info: TrajectoryInfo,
    pub messages: Vec<MessageRecord>,
}

impl Serialize for Trajectory {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Trajectory", 5)?;
        state.serialize_field("trajectory_format", &self.trajectory_format)?;
        state.serialize_field("artifact_kind", &crate::artifact::ArtifactKind::Trajectory)?;
        state.serialize_field(
            "schema_version",
            &crate::artifact::ArtifactSchemaVersion::CURRENT,
        )?;
        state.serialize_field("info", &self.info)?;
        state.serialize_field("messages", &self.messages)?;
        state.end()
    }
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

    #[test]
    fn token_usage_total_prompt_tokens_calculates_correctly() {
        let cases = vec![
            (
                TokenUsage {
                    prompt_tokens: 100,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    completion_tokens: 50,
                },
                100,
                false,
            ),
            (
                TokenUsage {
                    prompt_tokens: 100,
                    cache_read_tokens: 50,
                    cache_creation_tokens: 0,
                    completion_tokens: 50,
                },
                150,
                true,
            ),
            (
                TokenUsage {
                    prompt_tokens: 100,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 25,
                    completion_tokens: 50,
                },
                125,
                true,
            ),
            (
                TokenUsage {
                    prompt_tokens: u64::MAX - 10,
                    cache_read_tokens: 20,
                    cache_creation_tokens: 5,
                    completion_tokens: 50,
                },
                u64::MAX,
                true,
            ),
        ];

        for (usage, expected_total, expected_cached) in cases {
            assert_eq!(usage.total_prompt_tokens(), expected_total);
            assert_eq!(usage.has_cached_prompt_tokens(), expected_cached);
        }
    }

    #[test]
    fn budget_exhausted_failure_category_serializes_as_snake_case() {
        let json = serde_json::to_string(&FailureCategory::BudgetExhausted).unwrap();
        assert_eq!(json, "\"budget_exhausted\"");
        let back: FailureCategory = serde_json::from_str("\"budget_exhausted\"").unwrap();
        assert_eq!(back, FailureCategory::BudgetExhausted);
    }

    #[test]
    fn extra_is_empty_checks_all_fields() {
        let mut extra = MessageExtra::default();
        assert!(extra_is_empty(&extra));

        extra.actions = Some(vec!["action".into()]);
        assert!(!extra_is_empty(&extra));

        extra.actions = None;
        extra.cost = Some(1.0);
        assert!(!extra_is_empty(&extra));

        extra.cost = None;
        extra.response = Some(serde_json::json!("resp"));
        assert!(!extra_is_empty(&extra));

        extra.response = None;
        extra.timestamp = Some("2024-01-01".into());
        assert!(!extra_is_empty(&extra));

        extra.timestamp = None;
        extra.other.insert("key".into(), serde_json::json!("val"));
        assert!(!extra_is_empty(&extra));
    }
}
