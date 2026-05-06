//! Trajectory serialization — wire-compatible with mini-swe-agent's
//! `mini-swe-agent-1.1` format.
//!
//! Field declaration order on structs controls JSON key order in
//! `serde_json` output, so we match Python's layout exactly: `format`,
//! `info`, `messages`.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::model::{Message, MessageExtra};

pub const FORMAT_VERSION: &str = "mini-swe-agent-1.1";

/// Coarse run outcome. Exactly one of three values, suitable for computing
/// pass@1-style metrics from trajectory files alone:
/// `"submitted"` | `"step_limit_reached"` | `"error"`.
#[cfg(any(
    feature = "markdown-export",
    feature = "csv-export",
    feature = "mermaid-export"
))]
pub mod export;

/// Contains constants describing the final macro-outcome of an agent run.
///
/// These values provide a coarse summary useful for high-level telemetry and metrics.
pub mod outcome {
    /// Indicates the agent explicitly decided it successfully finished the task.
    pub const SUBMITTED: &str = "submitted";
    /// Indicates the agent exhausted its maximum allowed steps without submitting.
    pub const STEP_LIMIT_REACHED: &str = "step_limit_reached";
    /// Indicates the agent crashed or failed due to an unrecoverable system error.
    pub const ERROR: &str = "error";
    /// Indicates the agent's run was halted because its allocated financial or token budget ran out.
    pub const BUDGET_EXHAUSTED: &str = "budget_exhausted";
}

/// Contains constants for specific technical exit reasons outside of normal loop termination.
///
/// These values provide insight into *why* an agent run was forcefully halted.
pub mod exit_reason {
    /// Indicates the agent took too much real-world time to complete its task.
    pub const WALLCLOCK_TIMEOUT: &str = "wallclock_timeout";
}

/// Closed set of non-success terminal failure modes for sweeps.
///
/// This enum categorizes failures to help developers understand which part of the
/// system broke down, be it the environment, the model, or the agent's internal logic.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// The agent failed to launch because the underlying execution environment (like Docker) failed to initialize.
    EnvSetup,
    /// Communication with the Language Model API failed (e.g., rate limits, network timeouts).
    ModelApi,
    /// The Language Model produced an output that the agent could not parse into valid tool calls or commands.
    ModelParse,
    /// The agent hit the hard limit for the number of allowed steps without completing the task.
    StepLimit,
    /// The financial or token cost of the run exceeded the configured maximum.
    CostLimit,
    /// Per-task USD ceiling was reached mid-loop. The harness terminated the
    /// agent; any patch accumulated before the cap fired is preserved.
    BudgetExhausted,
    /// The run exceeded the maximum allowed real-world time.
    WallclockTimeout,
    /// The agent encountered a fatal error within its own control logic or harness.
    AgentInternal,
    /// Patch was captured but `git apply --check` rejected it at capture time.
    PatchApplyInvalid,
    /// Agent submitted but the captured diff was empty (zero bytes).
    PatchEmpty,
    /// The failure did not map to any known category. This is the catch-all for unexpected crashes.
    Unknown,
}

/// Default list of test commands the agent recognizes across multiple ecosystems.
///
/// If an agent issues one of these commands during a run, it is logged in the trajectory
/// so developers can measure if the agent verified its own code before submitting.
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

/// An executable pattern used to match a portion of an agent's shell command to identify if it is a test invocation.
///
/// Internally this can be a simple literal prefix match, or a regular expression.
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

/// Represents a single recorded instance where the agent ran a test suite.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestInvocation {
    /// The step number in the agent's main loop where the test was executed.
    pub step_index: u32,
    /// The raw shell command executed by the agent.
    pub command: String,
    /// The exit code of the shell command. Usually `0` means the tests passed.
    pub exit_code: i32,
    /// The specific pattern (e.g., `"cargo test"`) that triggered the recording of this invocation.
    pub matched_pattern: String,
}

/// Compiles a final list of `TestCommandPattern` matchers based on configuration.
///
/// This takes the hardcoded [`DEFAULT_TEST_COMMAND_PATTERNS`] and allows users to append
/// custom regex patterns or completely replace the default list.
///
/// # Examples
///
/// ```
/// use rust_swe_agent::trajectory::effective_test_command_patterns;
///
/// // Keep defaults and add a custom regex for a proprietary testing framework.
/// let patterns = effective_test_command_patterns(&["^bazel test".to_string()], false).unwrap();
/// assert!(patterns.len() > 16);
///
/// // Discard defaults and strictly use only one regex
/// let custom = effective_test_command_patterns(&["^npx jest".to_string()], true).unwrap();
/// assert_eq!(custom.len(), 1);
/// ```
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

/// Parses a shell command to see if it invokes a known testing framework.
///
/// It does this by evaluating the command (handling shell logic like `&&`, `;`, `|`)
/// and checking if any segment matches a [`TestCommandPattern`].
///
/// # Examples
///
/// ```
/// use rust_swe_agent::trajectory::{detect_test_command, effective_test_command_patterns};
///
/// let patterns = effective_test_command_patterns(&[], false).unwrap();
///
/// // Detects basic commands
/// let matched = detect_test_command("cargo test --all", &patterns);
/// assert_eq!(matched.as_deref(), Some("cargo test"));
///
/// // Detects chained commands
/// let matched_chained = detect_test_command("echo hello && pytest tests/", &patterns);
/// assert_eq!(matched_chained.as_deref(), Some("pytest"));
/// ```
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

/// Represents the cumulative number of tokens consumed by the Language Model during the run.
///
/// Models with Context Caching (like Anthropic's Claude) differentiate between newly evaluated
/// tokens, read tokens from cache, and newly cached tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    /// The number of new, uncached tokens passed in the prompt that the model had to evaluate.
    pub prompt_tokens: u64,
    /// The number of prompt tokens that the model was able to read instantly from its context cache.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_read_tokens: u64,
    /// The number of prompt tokens that the model processed and stored into its context cache for future use.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_creation_tokens: u64,
    /// The number of tokens generated by the model as a response.
    pub completion_tokens: u64,
}

impl TokenUsage {
    /// Computes the absolute total number of prompt tokens processed, combining uncached, cached, and creation tokens.
    ///
    /// Uses saturating addition to prevent overflow.
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_swe_agent::trajectory::TokenUsage;
    ///
    /// let usage = TokenUsage {
    ///     prompt_tokens: 100,
    ///     cache_read_tokens: 50,
    ///     cache_creation_tokens: 10,
    ///     completion_tokens: 0,
    /// };
    /// assert_eq!(usage.total_prompt_tokens(), 160);
    /// ```
    #[must_use]
    pub fn total_prompt_tokens(&self) -> u64 {
        self.prompt_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    /// Checks if any form of context caching was utilized during the run.
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_swe_agent::trajectory::TokenUsage;
    ///
    /// let usage = TokenUsage {
    ///     cache_read_tokens: 5,
    ///     ..Default::default()
    /// };
    /// assert!(usage.has_cached_prompt_tokens());
    /// ```
    #[must_use]
    pub fn has_cached_prompt_tokens(&self) -> bool {
        self.cache_read_tokens > 0 || self.cache_creation_tokens > 0
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Contains metadata and summary statistics for an entire agent run.
///
/// This serves as the "header" of the trajectory file. Most fields are `Option`
/// because the trajectory format is append-only—if an agent crashes early, some metrics
/// (like `duration_secs`) may not be computable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrajectoryInfo {
    /// A description of the issue or feature the agent was asked to solve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// The specific LLM model identifier used (e.g., `"claude-3-5-sonnet-20241022"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// System-level technical reason for termination (see [`exit_reason`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_reason: Option<String>,
    /// Categorized termination cause, making debugging sweeps easier (see [`FailureCategory`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    /// The high-level result of the run (see [`outcome`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The final patch or summary the agent provided when it submitted the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    /// The financial cost of API requests made during the run, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// Cumulative token usage metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    /// How long the agent loop ran before terminating, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// Total number of action loops the agent completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    /// A log of all known test commands the agent executed during its run.
    #[serde(default)]
    pub test_invocations: Vec<TestInvocation>,
    /// Whether the agent successfully executed a test command before calling submit.
    #[serde(default)]
    pub tests_run_before_submit: bool,
    /// Indicates whether the *last* executed test command returned a successful (`0`) exit code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tests_passed: Option<bool>,
    /// The wall-clock time the run started, formatted as an ISO 8601 string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// The wall-clock time the run concluded, formatted as an ISO 8601 string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// A catch-all map for arbitrary data we want to preserve in the JSON but don't explicitly parse.
    #[serde(flatten, default)]
    pub other: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A serialized version of a message exchanged during the agent run.
///
/// Unlike [`crate::model::Message`], this struct is exclusively for serialization and
/// ensures strict JSON compatibility with Python trajectory readers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    /// Who sent the message (e.g., `"system"`, `"user"`, `"assistant"`).
    pub role: String,
    /// The actual text payload.
    pub content: String,
    /// Optional metadata related to this message, like tool call parsing.
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

/// Represents the complete historical log of an agent run.
///
/// A Trajectory contains the initial setup, every message exchanged with the model,
/// all environment outputs (commands, file changes), and the final outcome metrics.
/// It is fundamentally designed to be serialized to disk so runs can be reviewed,
/// evaluated, or replayed later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trajectory {
    /// The specific schema version string (e.g. `"mini-swe-agent-1.1"`).
    pub trajectory_format: String,
    /// Run metadata and end-of-run summaries.
    pub info: TrajectoryInfo,
    /// The chronologically ordered log of LLM interactions and tool usages.
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
    /// Creates a new, blank trajectory with the correct `trajectory_format` pre-populated.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a new message to the trajectory log.
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_swe_agent::trajectory::Trajectory;
    /// use rust_swe_agent::model::Message;
    ///
    /// let mut t = Trajectory::new();
    /// t.record_message(&Message::system("You are a helpful assistant."));
    /// assert_eq!(t.messages.len(), 1);
    /// ```
    pub fn record_message(&mut self, m: &Message) {
        self.messages.push(MessageRecord {
            role: role_to_string(m.role),
            content: m.content.clone(),
            extra: m.extra.clone(),
        });
    }

    /// Appends a message while forcibly attaching specific metadata.
    ///
    /// This is useful when the agent harness wants to inject metrics (like cost or actions taken)
    /// that aren't natively attached to the [`Message`] object but need to exist in the trajectory log.
    pub fn record_with_extra(&mut self, m: &Message, extra: MessageExtra) {
        self.messages.push(MessageRecord {
            role: role_to_string(m.role),
            content: m.content.clone(),
            extra,
        });
    }

    /// Serializes the entire trajectory to formatted JSON and writes it to disk.
    pub fn save_pretty(&self, path: &Path) -> Result<(), crate::error::Error> {
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        Ok(())
    }

    /// Serializes the trajectory to a formatted JSON string in memory.
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
