//! Serde-visible shape of `config/*.toml`. Mirrors mini-swe-agent's layout
//! with stronger typing — enum variants for backends rather than free-text.

use serde::{Deserialize, Serialize};

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
    pub redaction: RedactionCfg,
    #[serde(default)]
    pub policy: crate::policy::PolicyCfg,
    /// Optional `extends: <path>` field — handled before serde sees this
    /// struct, but we accept/ignore it here for round-tripping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}
