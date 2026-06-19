//! Measure-first SWE agent harness with runtime MCP toolsets.
//!
//! Re-exports the public surface. See module docs for the architecture.
// Many async runner functions hold large state machines by design.
// The allocation behaviour is correct; the lint is informational only.
#![allow(clippy::large_futures)]

pub mod agent;
pub mod annotation;
pub mod artifact;
pub mod cli;
pub mod config;
pub mod cost;
pub mod env;
pub mod error;
pub mod exit_code;
pub mod fingerprint;
pub mod ids;
pub mod model;
pub mod policy;
pub mod prompt_guard;
pub mod redaction;
pub mod run;
pub mod skills;
pub mod stagnation;
pub mod stream;
pub mod telemetry;
pub mod template;
pub mod tool;
pub mod trajectory;
pub mod ui;

pub use run::dataset::{
    CacheStatus, DatasetSource, DatasetSourceKind, SwebenchAlias, SwebenchSplit, cache_path_for,
    check_cache, default_cache_dir, resolve_dataset, write_cache,
};

pub use agent::{Agent, DefaultAgent, ExitReason, InteractiveAgent, StepOutcome};
pub use config::{
    Config, McpServerCfg, RedactionCfg, SkillCfg, ToolCfg, ToolHookCfg, ToolHooksCfg,
};
#[cfg(feature = "docker")]
pub use env::DockerEnvironment;
pub use env::{Environment, LocalEnvironment, RunRequest, RunResult};
pub use error::{ConfigError, EnvError, Error, ModelError};
pub use exit_code::ExitCode;
pub use model::{
    AnthropicBackend, CacheHint, DeterministicModel, FallbackAttemptRecord, FallbackModel,
    LitellmBackend, Message, MessageExtra, Model, ModelResponse, ModelUsage, QueryOpts, Role,
    SamplingParams,
};
pub use policy::{
    PolicyCfg, PolicyConfigError, PolicyCounts, PolicyDecision, PolicyEngine, PolicyEvaluation,
    PolicyProfile, PolicyRule,
};
pub use prompt_guard::{PromptGuard, UntrustedKind};
pub use redaction::{RedactionCount, RedactionSummary, Redactor};
pub use skills::{
    ActiveSkill, ActiveSkillManifest, ActiveSkillSet, ResolvedSkillContext, SkillActivationReason,
    SkillManifest, SkillRegistry, SkillResolveRequest, resolve_for_task,
};
pub use stream::{
    BroadcastSink, MultiSink, NullSink, SseServer, StatusLineStderrSink, StreamEvent, StreamSink,
};
pub use tool::{
    BASH_TOOL_NAME, CommandTool, McpStdioServer, ToolCall, ToolDefinition, ToolInvocation,
    ToolManifestEntry, ToolOutput, ToolPromptInfo, ToolProvider, ToolRegistry, ToolSource,
    ToolsetManifest,
};
pub use trajectory::{
    FORMAT_VERSION, FailureCategory, FallbackSummary, MessageRecord, TestInvocation, TokenUsage,
    Trajectory, TrajectoryInfo, VerificationCheck, VerificationResult, verification_status,
};
