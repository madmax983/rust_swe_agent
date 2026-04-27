//! Real-time streaming of agent step events.
//!
//! `DefaultAgent` emits a `StreamEvent` at each meaningful boundary —
//! assistant message, bash start/result, observation, format error, run
//! start/end. A `StreamSink` decouples emission from transport: tests use
//! an in-memory sink, production wires a `BroadcastSink` whose events are
//! served over HTTP/SSE by `sse::SseServer`.
//!
//! Disconnect-safety is the load-bearing requirement (spec AC #3): emit
//! must never block or fail. `BroadcastSink::emit` ignores `SendError`
//! when there are zero subscribers, and the SSE per-connection task drops
//! on the first write error without disturbing other clients or the agent.

use serde::Serialize;

pub mod broadcast;
pub mod sse;

pub use broadcast::BroadcastSink;
pub use sse::SseServer;

/// One step-level event in the agent trajectory.
///
/// Serialized as JSON with a `type` discriminator; SSE frames use the
/// same string as the `event:` field name so clients can `addEventListener`
/// per kind.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Emitted once at agent start, after the system + instance prompts
    /// are recorded.
    RunStarted {
        task: String,
        model: String,
        started_at: String,
    },
    /// LLM produced a response (before action parsing).
    AssistantMessage {
        step: u32,
        content: String,
        cost_usd: Option<f64>,
        timestamp: String,
    },
    /// About to execute a bash command.
    BashStart {
        step: u32,
        command: String,
        timestamp: String,
    },
    /// Bash command finished.
    BashResult {
        step: u32,
        exit_code: i32,
        stdout: String,
        stderr: String,
        timed_out: bool,
        timestamp: String,
    },
    /// Observation message recorded into the trajectory after a bash run.
    Observation {
        step: u32,
        content: String,
        timestamp: String,
    },
    /// Model output was malformed; format-error template was sent back.
    FormatError {
        step: u32,
        content: String,
        timestamp: String,
    },
    /// Agent loop ended.
    RunEnded {
        exit_reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        failure_category: Option<crate::trajectory::FailureCategory>,
        final_output: Option<String>,
        steps: u32,
        total_cost_usd: f64,
        ended_at: String,
    },
}

impl StreamEvent {
    /// Stable wire name; used as the SSE `event:` field.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run_started",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::BashStart { .. } => "bash_start",
            Self::BashResult { .. } => "bash_result",
            Self::Observation { .. } => "observation",
            Self::FormatError { .. } => "format_error",
            Self::RunEnded { .. } => "run_ended",
        }
    }
}

/// Sink the agent emits to. Implementations MUST be non-blocking and
/// MUST NOT fail visibly to the agent — disconnect handling is the
/// sink's job.
pub trait StreamSink: Send + Sync {
    fn emit(&self, event: StreamEvent);
}

/// No-op sink. Useful as a default when streaming is disabled, so the
/// agent loop doesn't branch on `Option<...>` at every emission point.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl StreamSink for NullSink {
    fn emit(&self, _event: StreamEvent) {}
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn event_names_are_snake_case() {
        let e = StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "now".into(),
        };
        assert_eq!(e.event_name(), "run_started");
    }

    #[test]
    fn serializes_with_type_tag() {
        let e = StreamEvent::BashStart {
            step: 3,
            command: "echo hi".into(),
            timestamp: "t".into(),
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["type"], "bash_start");
        assert_eq!(json["step"], 3);
        assert_eq!(json["command"], "echo hi");
    }

    #[test]
    fn null_sink_swallows_events() {
        let s = NullSink;
        s.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });
    }
}
