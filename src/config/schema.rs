//! Serde-visible shape of `config/*.toml`. Mirrors mini-swe-agent's layout
//! with stronger typing — enum variants for backends rather than free-text.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    #[default]
    Default,
    Interactive,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvKind {
    #[default]
    Local,
    Docker,
}

/// Network isolation mode for Docker container runs (issue #523).
///
/// `Unrestricted` (the default) preserves today's behavior — no `--network`
/// flag is passed to `docker run`. `None` adds `--network none` to disable
/// all container egress. `Custom(s)` accepts an allowlist string for future
/// proxy-enforcement; the string is recorded in the manifest and reported by
/// `env preview`, but no additional isolation is applied in this slice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NetworkMode {
    /// Default bridge networking — no `--network` flag, no isolation.
    #[default]
    Unrestricted,
    /// Pass `--network none` to `docker run`; all external calls fail.
    None,
    /// Allowlist string accepted for forward-compat; not yet enforced.
    Custom(String),
}

impl NetworkMode {
    /// The string representation stored in config and emitted by `env preview`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::None => "none",
            Self::Custom(s) => s.as_str(),
        }
    }

    /// Returns the value to pass after `--network` in `docker run`, or `None`
    /// when no `--network` flag should be added (unrestricted = today's default).
    #[must_use]
    pub fn docker_network_arg(&self) -> Option<&str> {
        match self {
            Self::None => Some("none"),
            Self::Unrestricted | Self::Custom(_) => None,
        }
    }
}

impl std::str::FromStr for NetworkMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "" => Err(
                "network_mode cannot be empty; use \"unrestricted\", \"none\", or an allowlist string"
                    .to_owned(),
            ),
            "unrestricted" => Ok(Self::Unrestricted),
            "none" => Ok(Self::None),
            other => Ok(Self::Custom(other.to_owned())),
        }
    }
}

impl Serialize for NetworkMode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NetworkMode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCfg {
    #[serde(default)]
    pub kind: AgentKind,
    #[serde(default = "default_step_limit")]
    pub step_limit: u32,
    #[serde(default)]
    pub cost_limit_usd: Option<f64>,
    /// Per-task USD ceiling enforced inside the agent loop. When the
    /// accumulated task spend meets or exceeds this value, the loop
    /// terminates with `failure_category: budget_exhausted` and any
    /// patch accumulated so far is preserved. Default: `None` (opt-in).
    #[serde(default)]
    pub per_task_budget_usd: Option<f64>,
    /// When `true`, the budget block is not appended to observations.
    /// Useful for A/B experiments: agent-sees-budget vs. agent-does-not.
    /// Only meaningful when `per_task_budget_usd` is set.
    #[serde(default)]
    pub hide_budget_from_agent: bool,
    /// Handlebars template for the budget status block appended to each
    /// observation when `per_task_budget_usd` is set and
    /// `hide_budget_from_agent` is false. Available variables:
    /// `budget_used`, `budget_limit`, `budget_remaining_pct`, `turn`, `max_turns`.
    #[serde(default = "default_budget_block_template")]
    pub budget_block_template: String,
    #[serde(default = "default_format_error_template")]
    pub format_error_template: String,
    #[serde(default = "default_observation_template")]
    pub observation_template: String,
    #[serde(default = "default_observation_max_bytes")]
    pub observation_max_bytes: usize,
    #[serde(default = "default_observation_head_ratio")]
    pub observation_head_ratio: f64,
    #[serde(default = "default_tool_hook_timeout_secs")]
    pub tool_hook_timeout_secs: u64,
    #[serde(default)]
    pub test_command_patterns: Vec<String>,
    #[serde(default)]
    pub test_command_patterns_replace: bool,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerCfg>,
    #[serde(default)]
    pub tools: Vec<ToolCfg>,
    #[serde(default)]
    pub hooks: ToolHooksCfg,
    /// Enable in-loop stagnation detection. Default: `true` (opt-out with `false`).
    #[serde(default = "default_detect_stagnation")]
    pub detect_stagnation: bool,
    /// Minimum number of identical canonicalized actions within the window to
    /// trip the stagnation detector. Default: 4.
    #[serde(default = "default_stagnation_repeat_threshold")]
    pub stagnation_repeat_threshold: u32,
    /// Size of the trailing-steps window examined by the stagnation detector.
    /// Must be >= `stagnation_repeat_threshold`. Default: 8.
    #[serde(default = "default_stagnation_window")]
    pub stagnation_window: u32,
    /// Token budget for the prompt sent to the model on each turn. When the
    /// projected input exceeds this value, older tool observations are elided
    /// oldest-first until the projection fits. Default: `None` (no cap).
    ///
    /// Uses a byte-based approximation (1 token ≈ 4 bytes) when a live
    /// tokenizer is unavailable.
    #[serde(default)]
    pub history_max_input_tokens: Option<u64>,
    /// Keep only the last N tool observations in the model-visible prompt.
    /// Older observations are replaced with a short elision marker. Default:
    /// `None` (keep all).
    ///
    /// When both `history_max_input_tokens` and `history_keep_last_observations`
    /// are set, whichever elides more observations wins.
    #[serde(default)]
    pub history_keep_last_observations: Option<usize>,
    /// Maximum number of re-prompt retries when the model returns an empty or
    /// unparseable response before the run fails with `failure_category:
    /// model_parse`. Default: `3` (up to 3 additional re-prompt attempts after
    /// the first unactionable response). Set to `0` to abort on the very first
    /// unactionable response (reproduces the pre-retry behavior).
    #[serde(default = "default_parse_error_retries")]
    pub parse_error_retries: u32,
}

fn default_step_limit() -> u32 {
    50
}

fn default_budget_block_template() -> String {
    "\nBudget: ${{ budget_used }} of ${{ budget_limit }} used ({{ budget_remaining_pct }}% remaining), turn {{ turn }} of {{ max_turns }}".into()
}

fn default_format_error_template() -> String {
    "Your response did not include a valid tool call.".into()
}

fn default_observation_template() -> String {
    "Exit code: {{ returncode }}\nOutput:\n{{ output }}".into()
}

fn default_observation_max_bytes() -> usize {
    16_384
}

fn default_observation_head_ratio() -> f64 {
    0.5
}

fn default_tool_hook_timeout_secs() -> u64 {
    10
}

fn default_detect_stagnation() -> bool {
    true
}

fn default_stagnation_repeat_threshold() -> u32 {
    4
}

fn default_stagnation_window() -> u32 {
    8
}

fn default_parse_error_retries() -> u32 {
    3
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolHooksCfg {
    #[serde(default)]
    pub pre_tool_use: Vec<ToolHookCfg>,
    #[serde(default)]
    pub post_tool_use: Vec<ToolHookCfg>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolHookCfg {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCfg {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerCfg {
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCfg {
    pub name: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Ordered list of fallback model names tried on transient provider
    /// failures. Empty by default — a run without this field cannot
    /// silently introduce a secondary model.
    #[serde(default)]
    pub fallback_models: Vec<String>,
}

fn default_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvCfg {
    #[serde(default)]
    pub kind: EnvKind,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub docker_image: Option<String>,
    #[serde(default = "default_workdir")]
    pub workdir: String,
    /// Deterministic chaos fault injection: when `> 0`, the environment is
    /// wrapped in a decorator that replaces every Nth bash invocation with a
    /// synthetic timeout (issue #340). `0` (the default) preserves normal
    /// behavior — no failures are injected. Driven by the `--chaos-fail-every`
    /// CLI flag; recorded in the run manifest for reproducibility.
    #[serde(default)]
    pub chaos_fail_every: u32,
    /// Network isolation mode for the Docker container (issue #523).
    /// `unrestricted` (default) preserves today's behavior — no `--network` flag.
    /// `none` adds `--network none` to `docker run`, disabling all egress.
    /// Any other non-empty string is accepted as a future allowlist value but
    /// not yet enforced.
    #[serde(default)]
    pub network_mode: NetworkMode,
}

fn default_timeout_secs() -> u64 {
    60
}

fn default_workdir() -> String {
    "/workspace".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptCfg {
    #[serde(default)]
    pub system: String,
    #[serde(default)]
    pub instance: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SweepCfg {
    /// Cap aggregate provider request rate across all workers.
    /// Mirrors the `--max-rpm` CLI flag; the CLI value takes precedence.
    #[serde(default)]
    pub max_rpm: Option<u32>,
    /// Cap aggregate input-token rate across all workers (tokens per minute).
    /// Mirrors the `--max-input-tpm` CLI flag; the CLI value takes precedence.
    #[serde(default)]
    pub max_input_tpm: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCfg {
    /// Enable harness-level skill discovery and activation. Disabled by
    /// default to preserve prompt stability unless a run opts in.
    #[serde(default)]
    pub enabled: bool,
    /// When enabled, allow the harness to activate skills from task/manifests
    /// without an explicit `$skill-name` mention.
    #[serde(default = "default_skill_auto_load")]
    pub auto_load: bool,
    /// Directories or `SKILL.md` files to scan. Paths are resolved by the
    /// harness process; `~/...` expands to the user home directory.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Hard cap on active skills injected into a run.
    #[serde(default = "default_skill_max_active")]
    pub max_active: usize,
}

impl Default for SkillCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_load: default_skill_auto_load(),
            paths: Vec::new(),
            max_active: default_skill_max_active(),
        }
    }
}

const fn default_skill_auto_load() -> bool {
    true
}

const fn default_skill_max_active() -> usize {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactionCfg {
    /// Enable runtime redaction before observations, trajectories, streams,
    /// exports, and shareable artifacts are persisted or emitted.
    #[serde(default = "default_redaction_enabled")]
    pub enabled: bool,
    /// Explicit literal values to redact for this run. Values are used at run
    /// time only and are themselves redacted from exported config manifests.
    #[serde(default)]
    pub secret_literals: Vec<String>,
    /// Extra regex patterns whose full matches are redacted for this run.
    #[serde(default)]
    pub custom_patterns: Vec<String>,
    /// Unsafe escape hatch: allow submitted patch/prediction artifacts to
    /// contain configured secret literals. Redaction remains enabled unless
    /// `enabled = false` is also set.
    #[serde(default)]
    pub unsafe_allow_secret_leaks: bool,
}

impl Default for RedactionCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            secret_literals: Vec::new(),
            custom_patterns: Vec::new(),
            unsafe_allow_secret_leaks: false,
        }
    }
}

const fn default_redaction_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RootCfg {
    #[serde(default)]
    pub agent: AgentCfg,
    #[serde(default)]
    pub model: ModelCfg,
    #[serde(default)]
    pub environment: EnvCfg,
    #[serde(default)]
    pub prompts: PromptCfg,
    #[serde(default)]
    pub sweep: SweepCfg,
    #[serde(default)]
    pub skills: SkillCfg,
    #[serde(default)]
    pub redaction: RedactionCfg,
    #[serde(default)]
    pub policy: crate::policy::PolicyCfg,
    /// Optional `extends: <path>` field — handled before serde sees this
    /// struct, but we accept/ignore it here for round-tripping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_retries_defaults_to_three() {
        let cfg: AgentCfg = toml::from_str("").unwrap();
        assert_eq!(cfg.parse_error_retries, 3);
    }

    // ── NetworkMode RED-phase tests ──────────────────────────────────────────

    #[test]
    fn network_mode_default_is_unrestricted() {
        let cfg = EnvCfg::default();
        assert_eq!(cfg.network_mode, NetworkMode::Unrestricted);
    }

    #[test]
    fn network_mode_none_parses_from_toml() {
        let cfg: EnvCfg = toml::from_str(r#"network_mode = "none""#).unwrap();
        assert_eq!(cfg.network_mode, NetworkMode::None);
    }

    #[test]
    fn network_mode_unrestricted_parses_from_toml() {
        let cfg: EnvCfg = toml::from_str(r#"network_mode = "unrestricted""#).unwrap();
        assert_eq!(cfg.network_mode, NetworkMode::Unrestricted);
    }

    #[test]
    fn network_mode_empty_string_fails_parse() {
        let result: Result<EnvCfg, _> = toml::from_str(r#"network_mode = """#);
        assert!(result.is_err(), "empty network_mode should fail to parse");
    }

    #[test]
    fn network_mode_none_docker_network_arg_is_none_str() {
        assert_eq!(NetworkMode::None.docker_network_arg(), Some("none"));
    }

    #[test]
    fn network_mode_unrestricted_docker_network_arg_is_absent() {
        assert_eq!(NetworkMode::Unrestricted.docker_network_arg(), None);
    }

    #[test]
    fn network_mode_as_str_roundtrips() {
        assert_eq!(NetworkMode::Unrestricted.as_str(), "unrestricted");
        assert_eq!(NetworkMode::None.as_str(), "none");
    }
}
