//! `BroadcastSink`: a `StreamSink` backed by `tokio::sync::broadcast`.
//!
//! Each subscriber gets an independent `Receiver`. `emit` is non-blocking
//! and disconnect-safe: when there are zero receivers, `send()` returns
//! `Err(SendError)` which we deliberately ignore. Slow consumers cause
//! `Lagged` errors on receive, never on send — the agent is never
//! throttled by network speed.

use tokio::sync::broadcast;

use super::{StreamEvent, StreamSink};

/// Default channel capacity. A few hundred slots is plenty for human
/// step-rate event volume; consumers that fall behind get `Lagged`
/// errors on `recv` (not `Closed`), so they can resume from the next
/// live event.
pub const DEFAULT_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
/// A real-time streaming sink that broadcasts agent events to multiple receivers.
///
/// `BroadcastSink` decouples the event generation from transport, ensuring that the
/// agent is never throttled by slow network connections. It is disconnect-safe and
/// robust against slow consumers, utilizing `tokio::sync::broadcast` under the hood.
///
/// ## Examples
///
/// ```rust
/// # use rust_swe_agent::stream::{BroadcastSink, StreamSink, StreamEvent};
/// let sink = BroadcastSink::new(16);
/// let rx = sink.subscribe();
///
/// // Emitting an event is non-blocking and safe even if `rx` is dropped.
/// sink.emit(StreamEvent::Observation {
///     step: 1,
///     content: "Look at this output".to_string(),
///     timestamp: "2024-01-01T00:00:00Z".to_string(),
/// });
/// ```
pub struct BroadcastSink {
    tx: broadcast::Sender<StreamEvent>,
}

impl BroadcastSink {
    /// Creates a new broadcast sink with the specified channel capacity.
    ///
    /// If `capacity` is 0, it will be clamped to 1. When the channel is full,
    /// the oldest events are dropped to prevent memory exhaustion and blockages.
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// Number of currently attached receivers. Useful in tests and for
    /// shutdown logic.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Subscribes to the broadcast channel to receive real-time agent events.
    ///
    /// Each call returns an independent `Receiver` that sees all events emitted
    /// *after* the subscription is created.
    pub fn subscribe(&self) -> broadcast::Receiver<StreamEvent> {
        self.tx.subscribe()
    }
}

impl Default for BroadcastSink {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl StreamSink for BroadcastSink {
    fn emit(&self, event: StreamEvent) {
        // SendError occurs iff there are no active receivers. That's a
        // valid state — the agent runs whether or not anyone is watching.
        let _ = self.tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn ev(step: u32) -> StreamEvent {
        StreamEvent::AssistantMessage {
            step,
            content: "x".into(),
            cost_usd: None,
            timestamp: "t".into(),
        }
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_does_not_error() {
        let sink = BroadcastSink::new(8);
        // Should not panic, should not return error to caller.
        sink.emit(ev(0));
        assert_eq!(sink.receiver_count(), 0);
    }

    #[tokio::test]
    async fn subscriber_receives_subsequent_events() {
        let sink = BroadcastSink::new(8);
        let mut rx = sink.subscribe();
        sink.emit(ev(1));
        sink.emit(ev(2));
        let got1 = rx.recv().await.unwrap();
        let got2 = rx.recv().await.unwrap();
        match (got1, got2) {
            (
                StreamEvent::AssistantMessage { step: a, .. },
                StreamEvent::AssistantMessage { step: b, .. },
            ) => {
                assert_eq!(a, 1);
                assert_eq!(b, 2);
            }
            _ => panic!("wrong variants"),
        }
    }

    #[tokio::test]
    async fn dropped_subscriber_does_not_break_emit() {
        let sink = BroadcastSink::new(4);
        let rx = sink.subscribe();
        drop(rx);
        // Continues to emit fine.
        sink.emit(ev(1));
        sink.emit(ev(2));
        assert_eq!(sink.receiver_count(), 0);
    }

    #[tokio::test]
    async fn multiple_subscribers_each_get_each_event() {
        let sink = BroadcastSink::new(4);
        let mut a = sink.subscribe();
        let mut b = sink.subscribe();
        sink.emit(ev(7));
        let ea = a.recv().await.unwrap();
        let eb = b.recv().await.unwrap();
        assert!(matches!(ea, StreamEvent::AssistantMessage { step: 7, .. }));
        assert!(matches!(eb, StreamEvent::AssistantMessage { step: 7, .. }));
    }
}
