use crate::redaction::{Redactor, surface};
use crate::stream::{StreamEvent, StreamSink};
use std::sync::Arc;

pub struct RedactingSink {
    inner: Arc<dyn StreamSink>,
    redactor: Redactor,
}

impl RedactingSink {
    pub fn new(inner: Arc<dyn StreamSink>, redactor: Redactor) -> Self {
        Self { inner, redactor }
    }
}

impl StreamSink for RedactingSink {
    fn emit(&self, event: StreamEvent) {
        self.inner.emit(redact_stream_event(&event, &self.redactor));
    }
}

pub fn redact_stream_event(event: &StreamEvent, redactor: &Redactor) -> StreamEvent {
    match event {
        StreamEvent::RunStarted {
            task,
            model,
            started_at,
        } => StreamEvent::RunStarted {
            task: redactor.redact_text(task, surface::STREAM).text,
            model: redactor.redact_text(model, surface::STREAM).text,
            started_at: started_at.clone(),
        },
        StreamEvent::AssistantMessage {
            step,
            content,
            cost_usd,
            timestamp,
        } => StreamEvent::AssistantMessage {
            step: *step,
            content: redactor.redact_text(content, surface::STREAM).text,
            cost_usd: *cost_usd,
            timestamp: timestamp.clone(),
        },
        StreamEvent::BashStart {
            step,
            command,
            timestamp,
        } => StreamEvent::BashStart {
            step: *step,
            command: redactor.redact_text(command, surface::STREAM).text,
            timestamp: timestamp.clone(),
        },
        StreamEvent::BashResult {
            step,
            exit_code,
            stdout,
            stderr,
            timed_out,
            timestamp,
        } => StreamEvent::BashResult {
            step: *step,
            exit_code: *exit_code,
            stdout: redactor.redact_text(stdout, surface::STREAM).text,
            stderr: redactor.redact_text(stderr, surface::STREAM).text,
            timed_out: *timed_out,
            timestamp: timestamp.clone(),
        },
        StreamEvent::Observation {
            step,
            content,
            timestamp,
        } => StreamEvent::Observation {
            step: *step,
            content: redactor.redact_text(content, surface::STREAM).text,
            timestamp: timestamp.clone(),
        },
        StreamEvent::FormatError {
            step,
            content,
            timestamp,
        } => StreamEvent::FormatError {
            step: *step,
            content: redactor.redact_text(content, surface::STREAM).text,
            timestamp: timestamp.clone(),
        },
        StreamEvent::RunEnded {
            exit_reason,
            failure_category,
            final_output,
            steps,
            total_cost_usd,
            ended_at,
        } => StreamEvent::RunEnded {
            exit_reason: exit_reason.clone(),
            failure_category: *failure_category,
            final_output: final_output
                .as_ref()
                .map(|value| redactor.redact_text(value, surface::STREAM).text),
            steps: *steps,
            total_cost_usd: *total_cost_usd,
            ended_at: ended_at.clone(),
        },
        StreamEvent::AutoApproveRuleCreated { scope } => StreamEvent::AutoApproveRuleCreated {
            scope: redactor.redact_text(scope, surface::STREAM).text,
        },
    }
}
