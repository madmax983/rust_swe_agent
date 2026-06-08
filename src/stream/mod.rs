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
pub mod event_log;
pub mod sse;
#[cfg(feature = "webhook")]
pub mod sweep_webhook;
#[cfg(feature = "webhook")]
pub mod webhook;

pub use broadcast::BroadcastSink;
pub use event_log::EventLogSink;
pub use sse::SseServer;
#[cfg(feature = "webhook")]
pub use sweep_webhook::{
    SweepNotificationEvent, SweepWebhookEnvelope, SweepWebhookSink, SweepWebhookSinkError,
};
#[cfg(feature = "webhook")]
pub use webhook::{
    SchemaVersion, WebhookEnvelope, WebhookSink, WebhookSinkError, WebhookSinkHandle,
};

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
    /// A new auto-approve rule was created for a scope.
    #[serde(rename = "auto_approve_rule_created")]
    AutoApproveRuleCreated { scope: String },
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
            Self::AutoApproveRuleCreated { .. } => "auto_approve_rule_created",
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

/// Fan out one event to many sinks. Used so the agent can drive an SSE
/// broadcast, the ratatui dashboard, and an stderr status line from the
/// same emission path without each subsystem owning a side channel.
pub struct MultiSink {
    sinks: Vec<std::sync::Arc<dyn StreamSink>>,
}

impl MultiSink {
    #[must_use]
    pub fn new(sinks: Vec<std::sync::Arc<dyn StreamSink>>) -> Self {
        Self { sinks }
    }
}

impl StreamSink for MultiSink {
    fn emit(&self, event: StreamEvent) {
        for sink in &self.sinks {
            sink.emit(event.clone());
        }
    }
}

/// Issue #312 `--yolo`-without-`--interactive` status-line printer.
/// Prints one terse `[status] step N/M cost $X.XXXX` line to stderr on
/// each `AssistantMessage` (a clean per-step boundary that fires once
/// after every model turn, before tool execution).
pub struct StatusLineStderrSink {
    step_limit: u32,
}

impl StatusLineStderrSink {
    #[must_use]
    pub fn new(step_limit: u32) -> Self {
        Self { step_limit }
    }
}

impl StreamSink for StatusLineStderrSink {
    fn emit(&self, event: StreamEvent) {
        if let StreamEvent::AssistantMessage { step, cost_usd, .. } = event {
            let cost = cost_usd.unwrap_or(0.0);
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!(
                    "[status] step {}/{}  cost ${:.4}\n",
                    step, self.step_limit, cost
                )
                .as_bytes(),
            );
        }
    }
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

    /// Test sink that captures emitted events into a shared vec so tests
    /// can verify fan-out and ordering.
    #[derive(Default)]
    struct CapturingSink {
        events: std::sync::Mutex<Vec<&'static str>>,
    }

    impl CapturingSink {
        fn names(&self) -> Vec<&'static str> {
            self.events.lock().unwrap().clone()
        }
    }

    impl StreamSink for CapturingSink {
        fn emit(&self, event: StreamEvent) {
            self.events.lock().unwrap().push(event.event_name());
        }
    }

    #[test]
    fn multi_sink_fans_out_to_every_sink_in_order() {
        let a = std::sync::Arc::new(CapturingSink::default());
        let b = std::sync::Arc::new(CapturingSink::default());
        let multi = MultiSink::new(vec![
            a.clone() as std::sync::Arc<dyn StreamSink>,
            b.clone() as std::sync::Arc<dyn StreamSink>,
        ]);
        multi.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });
        multi.emit(StreamEvent::BashStart {
            step: 1,
            command: "echo".into(),
            timestamp: "t".into(),
        });
        assert_eq!(a.names(), vec!["run_started", "bash_start"]);
        assert_eq!(b.names(), vec!["run_started", "bash_start"]);
    }

    #[test]
    fn multi_sink_empty_is_noop() {
        let multi = MultiSink::new(vec![]);
        // Just verify it doesn't panic.
        multi.emit(StreamEvent::Observation {
            step: 0,
            content: "x".into(),
            timestamp: "t".into(),
        });
    }

    #[test]
    fn status_line_sink_silent_on_non_assistant_events() {
        // We can't easily capture the stderr write — but we can at
        // least confirm the sink doesn't panic for the non-matching arms.
        let sink = StatusLineStderrSink::new(50);
        sink.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });
        sink.emit(StreamEvent::BashStart {
            step: 1,
            command: "echo".into(),
            timestamp: "t".into(),
        });
        sink.emit(StreamEvent::BashResult {
            step: 1,
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            timestamp: "t".into(),
        });
        sink.emit(StreamEvent::Observation {
            step: 1,
            content: "x".into(),
            timestamp: "t".into(),
        });
        sink.emit(StreamEvent::FormatError {
            step: 1,
            content: "x".into(),
            timestamp: "t".into(),
        });
        sink.emit(StreamEvent::RunEnded {
            exit_reason: "submitted".into(),
            failure_category: None,
            final_output: None,
            steps: 1,
            total_cost_usd: 0.0,
            ended_at: "t".into(),
        });
    }

    #[test]
    fn status_line_sink_emits_on_assistant_message() {
        // Smoke: also doesn't panic for the matching arm.
        let sink = StatusLineStderrSink::new(7);
        sink.emit(StreamEvent::AssistantMessage {
            step: 3,
            content: "x".into(),
            cost_usd: Some(0.1234),
            timestamp: "t".into(),
        });
        sink.emit(StreamEvent::AssistantMessage {
            step: 4,
            content: "y".into(),
            cost_usd: None,
            timestamp: "t".into(),
        });
    }
}
