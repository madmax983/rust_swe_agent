//! Trajectory serialization — wire-compatible with mini-swe-agent's
//! `mini-swe-agent-1.2` format.
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

/// The version string for the trajectory serialization format.
///
/// Used to maintain compatibility with consumers expecting the `mini-swe-agent-1.2` structure.
pub const FORMAT_VERSION: &str = "mini-swe-agent-1.3";

/// Trajectory-level summary of fallback behavior for a single agent run.
///
/// Present only when `model.fallback_models` was configured; absent for
/// single-model runs so legacy artifact consumers see no change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FallbackSummary {
    /// The model name from `config.model.name` (requested primary).
    pub primary_model: String,
    /// The model that produced the last successful response, or the last
    /// attempted model when all candidates failed (`all_failed = true`).
    pub final_model: String,
    /// `true` when at least one fallback attempt was made.
    pub fallback_happened: bool,
    /// Number of failed transient attempts before the final success.
    pub fallback_count: u32,
    /// All model names tried in order (primary first).
    pub attempted_models: Vec<String>,
    /// Per-attempt failure records for the failed attempts.
    pub failed_attempts: Vec<FallbackAttemptRecord>,
    /// `true` when every model in the chain failed transiently and no
    /// model produced a response. `final_model` is the last attempted
    /// model in that case — not a responding model.
    #[serde(default, skip_serializing_if = "is_false")]
    pub all_failed: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !b
}

/// Provenance record for a single resume of a mid-run checkpoint.
/// Appended to `info.resume_history` when an agent is resumed from a
/// partial trajectory. Treated as ignored metadata by `bench reproduce`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeRecord {
    /// `info.started_at` from the original (pre-resume) trajectory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_started_at: Option<String>,
    /// ISO 8601 timestamp when this resume was initiated.
    pub resumed_at: String,
    /// Steps already completed before this resume.
    pub prior_steps: u32,
    /// Accumulated cost (USD) already spent before this resume.
    pub prior_cost_usd: f64,
    /// Git SHA of the harness binary at resume time (may differ from the
    /// original run's SHA after a harness upgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_git_sha_at_resume: Option<String>,
}

/// Operator-supplied verification check run after the agent finishes.
/// Operator-supplied verification check run after the agent finishes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationCheck {
    /// The descriptive name of the check.
    pub name: String,
    /// The shell command executed to perform the check.
    pub command: String,
}

/// Per-check evidence recorded after a verification check runs.
/// Per-check evidence recorded after a verification check runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationResult {
    /// The descriptive name of the check.
    pub name: String,
    /// The shell command executed to perform the check.
    pub command: String,
    /// The exit code returned by the command.
    pub exit_code: i32,
    /// The execution time in milliseconds.
    pub duration_ms: u64,
    /// Indicates whether the check succeeded (exit code 0).
    pub passed: bool,
    /// A truncated snippet of the standard output.
    pub stdout_preview: String,
    /// A truncated snippet of the standard error.
    pub stderr_preview: String,
    /// Indicates if the command execution was terminated due to a timeout.
    #[serde(default, skip_serializing_if = "is_false")]
    pub timed_out: bool,
}

/// Operator-facing verification status values.
/// Operator-facing verification status values.
pub mod verification_status {
    /// Indicates all verification checks passed successfully.
    pub const VERIFIED: &str = "verified";
    /// Indicates no verification checks were configured or run.
    pub const UNVERIFIED: &str = "unverified";
    /// Indicates one or more verification checks failed.
    pub const VERIFICATION_FAILED: &str = "verification_failed";
}

/// Coarse run outcome. Exactly one of three values, suitable for computing
/// pass@1-style metrics from trajectory files alone:
/// `"submitted"` | `"step_limit_reached"` | `"error"`.
pub mod export;

/// High-level run outcomes, used for computing pass/fail metrics.
pub mod outcome {
    /// The agent successfully submitted a solution.
    pub const SUBMITTED: &str = "submitted";
    /// The agent reached the maximum allowed steps before submitting.
    pub const STEP_LIMIT_REACHED: &str = "step_limit_reached";
    /// The agent encountered a fatal error during the run.
    pub const ERROR: &str = "error";
    /// The agent exceeded the maximum allowed cost budget.
    pub const BUDGET_EXHAUSTED: &str = "budget_exhausted";
}

/// Reasons an agent run might exit unexpectedly before submitting.
pub mod exit_reason {
    /// The run was manually cancelled by the user.
    pub const CANCELLED: &str = "cancelled";
    /// The run exceeded the maximum allowed wallclock time.
    pub const WALLCLOCK_TIMEOUT: &str = "wallclock_timeout";
}

/// Closed set of non-success terminal failure modes for sweeps.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// Failure during initial environment setup or repository cloning.
    EnvSetup,
    /// Repeated errors calling the model API.
    ModelApi,
    /// Repeated failures to parse the model's response format.
    ModelParse,
    /// The maximum number of agent loop iterations was reached.
    StepLimit,
    /// The maximum USD cost budget was reached.
    CostLimit,
    /// Per-task USD ceiling was reached mid-loop. The harness terminated the
    /// agent; any patch accumulated before the cap fired is preserved.
    BudgetExhausted,
    /// The maximum real-time execution duration was reached.
    WallclockTimeout,
    /// An internal logic error within the agent harness.
    AgentInternal,
    /// Patch was captured but `git apply --check` rejected it at capture time.
    PatchApplyInvalid,
    /// Agent submitted but the captured diff was empty (zero bytes).
    PatchEmpty,
    /// A configured secret literal was found in a submission artifact.
    SecretLeakDetected,
    /// The agent repeated the same action without progress and was halted.
    AgentStagnation,
    /// History could not be compacted to fit within `history_max_input_tokens`
    /// even after eliding all older observations. The run is terminated rather
    /// than sending an oversized prompt or triggering a provider context error.
    HistoryCompactionFailed,
    /// An unknown or unclassified failure occurred, or a value produced by a
    /// newer harness version that this reader does not recognise.
    #[serde(other)]
    Unknown,
}

impl FailureCategory {
    /// Returns `true` for operator-actionable failure classes that indicate a
    /// systemic misconfiguration rather than an expected per-task outcome.
    ///
    /// The circuit breaker only trips on actionable categories: a sweep where
    /// every task hits the step limit is working as designed, but one where
    /// every task gets a 401 from the API means the key is invalid.
    ///
    /// Actionable: `EnvSetup`, `ModelApi`.
    /// Non-actionable: everything else (expected per-task outcomes, internal
    /// errors, or conditions already governed by other budget controls).
    #[must_use]
    pub fn is_actionable(self) -> bool {
        matches!(
            self,
            Self::EnvSetup | Self::ModelApi | Self::HistoryCompactionFailed
        )
    }
}

/// The default list of command prefixes that are recognized as test invocations.
///
/// Used to detect when the agent runs commands like `pytest` or `cargo test`
/// during an execution loop.
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

/// A compiled pattern used to detect if a shell command is a test invocation.
///
/// Supports both literal prefix matching (e.g., "pytest") and regex matching.
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

/// Represents a detected test command executed by the agent during its run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestInvocation {
    /// The zero-based step index during which the test was executed.
    pub step_index: u32,
    /// The raw shell command that was executed.
    pub command: String,
    /// The exit code returned by the test command.
    pub exit_code: i32,
    /// The specific `TestCommandPattern` source that matched this command.
    pub matched_pattern: String,
}

/// Combines the default test patterns with any operator-supplied extra patterns.
///
/// If `replace_defaults` is true, the `DEFAULT_TEST_COMMAND_PATTERNS` are omitted,
/// and only the `extra_patterns` are compiled and returned.
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

/// Scans a command string to determine if it invokes a known test runner.
///
/// Handles bash-like quoting and escaping to extract the underlying command
/// before matching against the provided `patterns`.
///
/// ## Examples
/// ```
/// use maxwells_daemon::trajectory::{detect_test_command, effective_test_command_patterns};
///
/// let patterns = effective_test_command_patterns(&[], false).unwrap();
/// let cmd = "cargo test --all-features";
/// assert!(detect_test_command(cmd, &patterns).is_some());
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
/// Token consumption metrics for an agent run.
pub struct TokenUsage {
    /// The number of tokens used in the prompt.
    pub prompt_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    /// The number of tokens read from the prompt cache.
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    /// The number of tokens used to create the prompt cache.
    pub cache_creation_tokens: u64,
    /// The number of tokens generated in the completion.
    pub completion_tokens: u64,
}

impl TokenUsage {
    #[must_use]
    /// Calculates the total number of prompt tokens, including cached ones.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::trajectory::TokenUsage;
    /// let usage = TokenUsage { prompt_tokens: 10, cache_read_tokens: 5, cache_creation_tokens: 2, completion_tokens: 20 };
    /// assert_eq!(usage.total_prompt_tokens(), 17);
    /// ```
    pub fn total_prompt_tokens(&self) -> u64 {
        self.prompt_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    #[must_use]
    /// Checks if any prompt tokens were cached.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::trajectory::TokenUsage;
    /// let usage = TokenUsage { prompt_tokens: 10, cache_read_tokens: 5, cache_creation_tokens: 0, completion_tokens: 20 };
    /// assert!(usage.has_cached_prompt_tokens());
    /// ```
    pub fn has_cached_prompt_tokens(&self) -> bool {
        self.cache_read_tokens > 0 || self.cache_creation_tokens > 0
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
/// Detailed information and metrics about the trajectory run.
pub struct TrajectoryInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The task or issue description the agent was trying to solve.
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The name of the primary model used for the run.
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The reason the agent loop exited (e.g., cancelled, wallclock timeout).
    pub exit_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The category of failure, if the run did not succeed.
    pub failure_category: Option<FailureCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The high-level outcome of the run (e.g., submitted, error).
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The final output or submission from the agent.
    pub final_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The total cost of the run in USD, combining actual and baseline costs.
    pub total_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The actual cost incurred during the run in USD.
    pub actual_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The source or method used to calculate the actual cost.
    pub actual_cost_source: Option<CostSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The baseline or estimated cost in USD, typically without caching.
    pub baseline_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The model used to calculate the baseline cost.
    pub baseline_cost_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Aggregated token usage statistics for the entire run.
    pub token_usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The total duration of the run in seconds.
    pub duration_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Summary of any secret redactions performed during the run.
    pub redaction: Option<crate::redaction::RedactionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The number of steps the agent took.
    pub steps: Option<u32>,
    #[serde(default)]
    /// A list of test commands invoked during the run and their results.
    pub test_invocations: Vec<TestInvocation>,
    #[serde(default)]
    /// Indicates whether tests were run before the agent submitted the solution.
    pub tests_run_before_submit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Indicates whether the last executed test suite passed.
    pub last_tests_passed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The ISO 8601 formatted timestamp when the run started.
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The ISO 8601 formatted timestamp when the run ended.
    pub ended_at: Option<String>,
    /// Command-policy telemetry: counts of allowed/asked/blocked/yolo commands.
    #[serde(default, skip_serializing_if = "crate::policy::PolicyCounts::is_empty")]
    pub policy_counts: crate::policy::PolicyCounts,
    /// Fallback telemetry for runs that used `model.fallback_models`. `None`
    /// for single-model runs (preserves legacy artifact shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_summary: Option<FallbackSummary>,
    /// Operator-facing verification outcome. `"verified"` | `"unverified"` |
    /// `"verification_failed"`. Set on every run that reaches the verification
    /// phase; absent on trajectories written before this feature was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_status: Option<String>,
    /// Per-check evidence for runs where verification checks were configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification_results: Vec<VerificationResult>,
    /// Whether this trajectory file represents a mid-run checkpoint rather than
    /// a completed run. `true` while the agent is running; `false` (or absent)
    /// on the final write. Old files without this field parse as `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
    /// Human-readable reason the trajectory is partial.
    /// `"in_progress"` during a live run; `"interrupted"` if the process was
    /// killed without a clean shutdown. `None` on completed trajectories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_reason: Option<String>,
    /// Audit trail of resume events. Empty for trajectories that ran
    /// continuously. Each entry corresponds to one `--resume` continuation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resume_history: Vec<ResumeRecord>,
    /// OpenTelemetry trace ID assigned when `--otlp-endpoint` is active.
    /// 32 lowercase hex chars (128-bit). Correlates this trajectory with OTLP
    /// spans in the operator's observability backend.
    /// `None` for sweeps run without OTLP export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Absolute canonicalized local working directory for the agent run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_workdir: Option<String>,
    #[serde(flatten, default)]
    /// Any other arbitrary metadata associated with the run.
    pub other: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// A single message record in the trajectory, capturing the interaction between the agent and the environment/model.
pub struct MessageRecord {
    /// The role of the message sender (e.g., "user", "assistant", "system").
    pub role: String,
    /// The text content of the message.
    pub content: String,
    #[serde(default, skip_serializing_if = "extra_is_empty")]
    /// Additional metadata associated with the message, such as tool calls or costs.
    pub extra: MessageExtra,
}

fn extra_is_empty(e: &MessageExtra) -> bool {
    e.actions.is_none()
        && e.cost.is_none()
        && e.response.is_none()
        && e.timestamp.is_none()
        && e.model_latency_ms.is_none()
        && e.tool_latency_ms.is_none()
        && e.harness_overhead_ms.is_none()
        && e.sampling.is_none()
        && e.other.is_empty()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkLineage {
    pub parent_sweep_path: String,
    pub parent_instance_id: String,
    pub parent_trajectory_sha256: String,
    pub fork_step: u32,
    pub tail_overrides: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
/// The complete record of an agent's execution, including metadata, telemetry, and all messages.
///
/// This struct is serializeable to the `mini-swe-agent-1.2` JSON format and is the primary artifact
/// generated at the end of a run.
pub struct Trajectory {
    /// The format version of the trajectory schema.
    pub trajectory_format: String,
    /// Detailed information and metrics about the run.
    pub info: TrajectoryInfo,
    /// The sequence of messages exchanged during the run.
    pub messages: Vec<MessageRecord>,
    /// Metadata recording how this trajectory was forked from a parent run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_lineage: Option<ForkLineage>,
}

impl Serialize for Trajectory {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let len = if self.fork_lineage.is_some() { 6 } else { 5 };
        let mut state = serializer.serialize_struct("Trajectory", len)?;
        state.serialize_field("trajectory_format", &self.trajectory_format)?;
        state.serialize_field("artifact_kind", &crate::artifact::ArtifactKind::Trajectory)?;
        state.serialize_field(
            "schema_version",
            &crate::artifact::ArtifactSchemaVersion::CURRENT,
        )?;
        state.serialize_field("info", &self.info)?;
        state.serialize_field("messages", &self.messages)?;
        if let Some(ref lineage) = self.fork_lineage {
            state.serialize_field("fork_lineage", lineage)?;
        }
        state.end()
    }
}

impl Default for Trajectory {
    fn default() -> Self {
        Self {
            trajectory_format: FORMAT_VERSION.to_owned(),
            info: TrajectoryInfo::default(),
            messages: Vec::new(),
            fork_lineage: None,
        }
    }
}

impl Trajectory {
    /// Creates a new, empty trajectory with default settings.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::trajectory::Trajectory;
    /// let traj = Trajectory::new();
    /// assert_eq!(traj.messages.len(), 0);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a new message in the trajectory.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::trajectory::Trajectory;
    /// use maxwells_daemon::model::Message;
    /// let mut traj = Trajectory::new();
    /// traj.record_message(&Message::user("Hello"));
    /// assert_eq!(traj.messages.len(), 1);
    /// ```
    pub fn record_message(&mut self, m: &Message) {
        self.messages.push(MessageRecord {
            role: role_to_string(m.role),
            content: m.content.clone(),
            extra: m.extra.clone(),
        });
    }

    /// Records a new message in the trajectory, along with explicit extra metadata.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::trajectory::Trajectory;
    /// use maxwells_daemon::model::{Message, MessageExtra};
    /// let mut traj = Trajectory::new();
    /// traj.record_with_extra(&Message::user("Hello"), MessageExtra::default());
    /// assert_eq!(traj.messages.len(), 1);
    /// ```
    pub fn record_with_extra(&mut self, m: &Message, extra: MessageExtra) {
        self.messages.push(MessageRecord {
            role: role_to_string(m.role),
            content: m.content.clone(),
            extra,
        });
    }

    /// Saves the trajectory to a file in a human-readable JSON format.
    ///
    /// ## Examples
    ///
    /// ```rust,no_run
    /// use maxwells_daemon::trajectory::Trajectory;
    /// use std::path::Path;
    /// let traj = Trajectory::new();
    /// traj.save_pretty(Path::new("run.traj.json")).unwrap();
    /// ```
    pub fn save_pretty(&self, path: &Path) -> Result<(), crate::error::Error> {
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        Ok(())
    }

    /// Atomically writes a mid-run checkpoint of this trajectory with `partial: true`.
    ///
    /// Uses a write-to-tmp-then-rename strategy so a crash during the write
    /// never corrupts the previously-persisted checkpoint. The caller must
    /// ensure the parent directory of `path` already exists.
    ///
    /// ## Examples
    ///
    /// ```rust,no_run
    /// use maxwells_daemon::trajectory::Trajectory;
    /// use std::path::Path;
    /// let traj = Trajectory::new();
    /// traj.save_partial_atomic(Path::new("run.traj.json")).unwrap();
    /// ```
    pub fn save_partial_atomic(&self, path: &Path) -> Result<(), crate::error::Error> {
        // Build a clone with partial=true for the checkpoint write.
        let mut checkpoint = self.clone();
        checkpoint.info.partial = true;
        checkpoint.info.partial_reason = Some("in_progress".into());

        let tmp_path = path.with_extension("partial.tmp");
        let s = serde_json::to_string_pretty(&checkpoint)?;
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(s.as_bytes())?;
            f.sync_all()?;
        }
        // Best-effort fsync of parent directory (ensures rename is durable).
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Reconstructs the in-memory message history from serialized trajectory
    /// records, for use when resuming an agent from a mid-run checkpoint.
    ///
    /// Each `MessageRecord` is converted back to a `Message` using its `role`
    /// and `content`. The `cache_hint` is set to `None` on all messages; the
    /// agent's `retag_cache_hints` will re-apply the rolling cache policy on
    /// the next model call.
    pub fn messages_as_model_history(&self) -> Vec<crate::model::Message> {
        use crate::model::{CacheHint, Message, Role};
        self.messages
            .iter()
            .map(|rec| {
                let role = match rec.role.as_str() {
                    "system" => Role::System,
                    "assistant" => Role::Assistant,
                    "tool" => Role::Tool,
                    _ => Role::User,
                };
                Message {
                    role,
                    content: rec.content.clone(),
                    cache_hint: CacheHint::None,
                    extra: rec.extra.clone(),
                }
            })
            .collect()
    }

    /// Serializes the trajectory to a pretty-printed JSON string.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::trajectory::Trajectory;
    /// let traj = Trajectory::new();
    /// let json = traj.to_json_pretty().unwrap();
    /// assert!(json.contains("mini-swe-agent-1.3"));
    /// ```
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Load a trajectory for `instance_id` from `dir`.
///
/// Checks `{dir}/{instance_id}.traj.json` (root format) first, then
/// `{dir}/{instance_id}/run-1.traj.json` (nested format). Returns `None` if
/// neither file exists or can be parsed.
pub fn load_trajectory_for_instance(
    dir: &std::path::Path,
    instance_id: &str,
) -> Option<Trajectory> {
    let candidates = [
        dir.join(format!("{instance_id}.traj.json")),
        dir.join(instance_id).join("run-1.traj.json"),
    ];
    for path in &candidates {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(traj) = serde_json::from_str::<Trajectory>(&text) {
                return Some(traj);
            }
        }
    }
    None
}

/// Load all trajectories for `instance_id` from `dir`, covering all known layouts.
///
/// Checks these single-file layouts first (in priority order) and returns a
/// single-element vec when one is found:
/// - `{dir}/{id}.traj.json` (flat / root format)
/// - `{dir}/{id}/trajectory.json` (nested single-run format)
/// - `{dir}/trajectories/{id}.traj.json` (extracted bundle format)
///
/// When none of the above exist, scans for `{dir}/{id}/run-1.traj.json`,
/// `run-2.traj.json`, … until the sequence breaks (multi-run sweeps).
/// Returns an empty vec when no trajectory files are found.
pub fn load_all_trajectories_for_instance(
    dir: &std::path::Path,
    instance_id: &str,
) -> Vec<Trajectory> {
    // Single-file layouts (tried in priority order).
    let single_candidates = [
        dir.join(format!("{instance_id}.traj.json")),
        dir.join(instance_id).join("trajectory.json"),
        dir.join("trajectories")
            .join(format!("{instance_id}.traj.json")),
    ];
    for path in &single_candidates {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(traj) = serde_json::from_str::<Trajectory>(&text) {
                return vec![traj];
            }
        }
    }
    let mut trajs = Vec::new();
    for n in 1u32.. {
        let path = dir.join(instance_id).join(format!("run-{n}.traj.json"));
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                if let Ok(traj) = serde_json::from_str::<Trajectory>(&text) {
                    trajs.push(traj);
                }
            }
            Err(_) => break,
        }
    }
    trajs
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
  "trajectory_format": "mini-swe-agent-1.2",
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
  "trajectory_format": "mini-swe-agent-1.2",
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
