//! Measure-first SWE agent harness with a minimal bash-only loop.
//!
//! Re-exports the public surface. See module docs for the architecture.

pub mod agent;
pub mod artifact;
pub mod cli;
pub mod config;
pub mod env;
pub mod error;
pub mod ids;
pub mod model;
pub mod policy;
pub mod redaction;
pub mod run;
pub mod stream;
pub mod template;
pub mod trajectory;

pub use agent::{Agent, DefaultAgent, ExitReason, InteractiveAgent, StepOutcome};
pub use config::{Config, RedactionCfg, ToolHookCfg, ToolHooksCfg};
#[cfg(feature = "docker")]
pub use env::DockerEnvironment;
pub use env::{Environment, LocalEnvironment, RunRequest, RunResult};
pub use error::{ConfigError, EnvError, Error, ModelError};
pub use model::{
    AnthropicBackend, CacheHint, DeterministicModel, LitellmBackend, Message, MessageExtra, Model,
    ModelResponse, ModelUsage, QueryOpts, Role,
};
pub use policy::{
    PolicyCfg, PolicyCounts, PolicyDecision, PolicyEngine, PolicyProfile, PolicyRule,
};
pub use redaction::{RedactionCount, RedactionSummary, Redactor};
pub use stream::{BroadcastSink, NullSink, SseServer, StreamEvent, StreamSink};
pub use trajectory::{
    FORMAT_VERSION, FailureCategory, MessageRecord, TestInvocation, TokenUsage, Trajectory,
    TrajectoryInfo,
};
