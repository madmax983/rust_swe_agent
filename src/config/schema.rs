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
    pub hooks: ToolHooksCfg,
}

fn default_step_limit() -> u32 {
    50
}

fn default_format_error_template() -> String {
    "Your response did not include a shell command.".into()
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCfg {
    pub name: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
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
    /// Optional `extends: <path>` field — handled before serde sees this
    /// struct, but we accept/ignore it here for round-tripping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}
