//! Rust port of mini-swe-agent.
//!
//! Re-exports the public surface. See module docs for the architecture.

pub mod agent;
pub mod cli;
pub mod config;
pub mod env;
pub mod error;
pub mod ids;
pub mod model;
pub mod run;
pub mod stream;
pub mod template;
pub mod trajectory;

pub use agent::{Agent, DefaultAgent, ExitReason, InteractiveAgent, StepOutcome};
pub use config::{Config, ToolHookCfg, ToolHooksCfg};
#[cfg(feature = "docker")]
pub use env::DockerEnvironment;
pub use env::{Environment, LocalEnvironment, RunRequest, RunResult};
pub use error::{ConfigError, EnvError, Error, ModelError};
pub use model::{
    AnthropicBackend, CacheHint, DeterministicModel, LitellmBackend, Message, MessageExtra, Model,
    ModelResponse, ModelUsage, QueryOpts, Role,
};
pub use stream::{BroadcastSink, NullSink, SseServer, StreamEvent, StreamSink};
pub use trajectory::{
    FORMAT_VERSION, FailureCategory, MessageRecord, TestInvocation, TokenUsage, Trajectory,
    TrajectoryInfo,
};
