//! Sweep-level webhook notifications for operators.
//!
//! Mirrors the posture of [`super::webhook`] (bounded channel, background
//! sender, best-effort delivery) but targets the **sweep** operator rather
//! than the per-trajectory consumer.  Events are POSTed wrapped in a
//! `SweepWebhookEnvelope` with `schema_version`, `sweep_id`, the event
//! payload, and an RFC-3339 `emitted_at` timestamp.
//!
//! All string fields are redacted with the run-time [`crate::redaction::Redactor`]
//! before the payload is serialised and sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use tokio::runtime::{Handle, TryCurrentError};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::redaction::Redactor;
use crate::stream::SchemaVersion;

const DEFAULT_SWEEP_WEBHOOK_BUFFER_CAPACITY: usize = 1024;
const SWEEP_WEBHOOK_HTTP_TIMEOUT_SECS: u64 = 5;

/// Sweep-level events posted to the operator webhook.
///
/// Every variant's string fields are redacted before being placed in the
/// envelope.  The `type` discriminator is the stable wire name used by
/// downstream consumers.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SweepNotificationEvent {
    /// Emitted once when the sweep worker loop starts (after preflight).
    SweepStarted {
        total_instances: usize,
        model: String,
    },
    /// Emitted when 25 %, 50 %, or 75 % of instances have completed.
    SweepMilestone {
        /// One of 0.25, 0.50, or 0.75.
        completed_share: f64,
        completed: usize,
        total: usize,
    },
    /// Emitted after each instance finishes (submitted, errored, or budget-halted).
    InstanceCompleted {
        instance_id: String,
        resolved: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        failure_category: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_secs: Option<f64>,
    },
    /// Emitted when the systemic-failure circuit breaker trips.
    SystemicHaltTripped {
        dominant_category: String,
        /// Fraction 0..=1, not a percentage.
        share: f64,
    },
    /// Emitted when cumulative spend crosses 25 / 50 / 75 / 100 % of the
    /// `--sweep-cost-limit-usd` ceiling.  Only fires when the flag is set.
    CostThresholdCrossed {
        /// One of 0.25, 0.50, 0.75, or 1.00.
        threshold_share: f64,
        cumulative_cost_usd: f64,
        cost_limit_usd: f64,
    },
    /// Emitted once when the sweep exits for any reason.
    SweepCompleted {
        total_resolved: usize,
        total_attempted: usize,
        total_cost_usd: f64,
        wallclock_secs: f64,
        terminal_reason: String,
        webhook_events_dropped: u64,
    },
    /// Posted only by `bench doctor` to check reachability; not part of the
    /// runtime taxonomy and never emitted during a real sweep.
    DoctorProbe { sweep_id: String },
}

impl SweepNotificationEvent {
    /// Stable wire name included in the `type` field of the envelope.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::SweepStarted { .. } => "sweep_started",
            Self::SweepMilestone { .. } => "sweep_milestone",
            Self::InstanceCompleted { .. } => "instance_completed",
            Self::SystemicHaltTripped { .. } => "systemic_halt_tripped",
            Self::CostThresholdCrossed { .. } => "cost_threshold_crossed",
            Self::SweepCompleted { .. } => "sweep_completed",
            Self::DoctorProbe { .. } => "doctor_probe",
        }
    }

    /// Returns a new event with every string field passed through `redactor`.
    /// Numeric / boolean fields are not modified.
    #[must_use]
    pub fn redacted(self, redactor: &Redactor) -> Self {
        use crate::redaction::surface;
        match self {
            Self::SweepStarted {
                total_instances,
                model,
            } => Self::SweepStarted {
                total_instances,
                model: redactor.redact_text(&model, surface::STREAM).text,
            },
            // No string fields to redact in these variants.
            Self::SweepMilestone { .. } | Self::CostThresholdCrossed { .. } => self,
            Self::InstanceCompleted {
                instance_id,
                resolved,
                failure_category,
                cost_usd,
                duration_secs,
            } => Self::InstanceCompleted {
                instance_id: redactor.redact_text(&instance_id, surface::STREAM).text,
                resolved,
                failure_category: failure_category
                    .map(|s| redactor.redact_text(&s, surface::STREAM).text),
                cost_usd,
                duration_secs,
            },
            Self::SystemicHaltTripped {
                dominant_category,
                share,
            } => Self::SystemicHaltTripped {
                dominant_category: redactor
                    .redact_text(&dominant_category, surface::STREAM)
                    .text,
                share,
            },
            Self::SweepCompleted {
                total_resolved,
                total_attempted,
                total_cost_usd,
                wallclock_secs,
                terminal_reason,
                webhook_events_dropped,
            } => Self::SweepCompleted {
                total_resolved,
                total_attempted,
                total_cost_usd,
                wallclock_secs,
                terminal_reason: redactor.redact_text(&terminal_reason, surface::STREAM).text,
                webhook_events_dropped,
            },
            Self::DoctorProbe { sweep_id } => Self::DoctorProbe {
                sweep_id: redactor.redact_text(&sweep_id, surface::STREAM).text,
            },
        }
    }
}

/// JSON envelope POSTed for every sweep notification.
///
/// Schema: `{ "schema_version": {"major":1,"minor":0}, "sweep_id": "…",
///            "event": { "type": "…", …fields }, "emitted_at": "…" }`.
#[derive(Debug, Serialize)]
pub struct SweepWebhookEnvelope {
    pub schema_version: SchemaVersion,
    pub sweep_id: String,
    pub event: SweepNotificationEvent,
    pub emitted_at: String,
}

struct EnvelopeMsg {
    event: SweepNotificationEvent,
}

/// Non-blocking sweep-level webhook sink.
///
/// `emit` enqueues into a bounded channel; a background task dequeues,
/// applies the redactor, builds the envelope, and POSTs via HTTP.
/// Buffer-full and HTTP failures are counted toward `dropped_count`, which
/// appears in the `sweep_completed` payload.
///
/// The agent / sweep loop is **never** blocked on webhook delivery.
#[derive(Debug)]
pub struct SweepWebhookSink {
    tx: mpsc::Sender<EnvelopeMsg>,
    dropped: Arc<AtomicU64>,
    join_handle: tokio::task::JoinHandle<()>,
}

/// Failure to create a [`SweepWebhookSink`].
#[derive(Debug, thiserror::Error)]
pub enum SweepWebhookSinkError {
    #[error("sweep webhook sink requires an active Tokio runtime")]
    NoRuntime(#[source] TryCurrentError),
    #[error("sweep webhook buffer capacity must be greater than zero")]
    InvalidBufferCapacity,
    #[error("invalid sweep webhook URL: {0}")]
    InvalidUrl(String),
    #[error("failed to build sweep webhook HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("invalid sweep webhook header `{name}`: {reason}")]
    InvalidHeader { name: String, reason: String },
}

impl SweepWebhookSink {
    /// Creates a sink with the default buffer capacity (1024).
    pub fn new(
        url: String,
        headers: &[(String, String)],
        redactor: Redactor,
        sweep_id: String,
    ) -> Result<Self, SweepWebhookSinkError> {
        Self::with_buffer_capacity(
            url,
            headers,
            redactor,
            sweep_id,
            DEFAULT_SWEEP_WEBHOOK_BUFFER_CAPACITY,
        )
    }

    /// Creates a sink with a caller-specified buffer capacity.
    pub fn with_buffer_capacity(
        url: String,
        headers: &[(String, String)],
        redactor: Redactor,
        sweep_id: String,
        buffer_capacity: usize,
    ) -> Result<Self, SweepWebhookSinkError> {
        if buffer_capacity == 0 {
            return Err(SweepWebhookSinkError::InvalidBufferCapacity);
        }

        let parsed_url = reqwest::Url::parse(&url)
            .map_err(|e| SweepWebhookSinkError::InvalidUrl(e.to_string()))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(SweepWebhookSinkError::InvalidUrl(format!(
                "unsupported scheme `{}`; sweep webhook URL must use http or https",
                parsed_url.scheme()
            )));
        }

        let handle = Handle::try_current().map_err(SweepWebhookSinkError::NoRuntime)?;

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(SWEEP_WEBHOOK_HTTP_TIMEOUT_SECS));
        let mut default_headers = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let header_name =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                    SweepWebhookSinkError::InvalidHeader {
                        name: name.clone(),
                        reason: e.to_string(),
                    }
                })?;
            let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
                SweepWebhookSinkError::InvalidHeader {
                    name: name.clone(),
                    reason: e.to_string(),
                }
            })?;
            default_headers.insert(header_name, header_value);
        }
        builder = builder.default_headers(default_headers);
        let client = builder.build().map_err(SweepWebhookSinkError::Client)?;

        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_bg = dropped.clone();

        let (tx, mut rx) = mpsc::channel::<EnvelopeMsg>(buffer_capacity);

        let join_handle = handle.spawn(async move {
            while let Some(EnvelopeMsg { event }) = rx.recv().await {
                let event_type = event.event_name();
                let redacted_event = event.redacted(&redactor);
                let envelope = SweepWebhookEnvelope {
                    schema_version: SchemaVersion::default(),
                    sweep_id: sweep_id.clone(),
                    event: redacted_event,
                    emitted_at: Utc::now().to_rfc3339(),
                };
                match client.post(&url).json(&envelope).send().await {
                    Ok(resp) if !resp.status().is_success() => {
                        warn!(
                            event_type,
                            status = resp.status().as_u16(),
                            "sweep webhook POST returned non-success; counting as dropped"
                        );
                        dropped_bg.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let error_class = if e.is_timeout() {
                            "timeout"
                        } else if e.is_connect() {
                            "connection_refused"
                        } else {
                            "network_error"
                        };
                        warn!(
                            event_type,
                            error_class, "sweep webhook POST failed; counting as dropped"
                        );
                        dropped_bg.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => {
                        debug!(event_type, "sweep webhook POST succeeded");
                    }
                }
            }
        });

        Ok(Self {
            tx,
            dropped,
            join_handle,
        })
    }

    /// Enqueues a sweep event for delivery.  Non-blocking; silently drops when
    /// the buffer is full (increments the drop counter).
    pub fn emit(&self, event: SweepNotificationEvent) {
        match self.tx.try_send(EnvelopeMsg { event }) {
            Err(mpsc::error::TrySendError::Full(_)) => {
                debug!("dropping sweep webhook event: buffer full");
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Returns the current count of dropped events (buffer-full + HTTP failures).
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Returns a clone of the shared drop counter.
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        self.dropped.clone()
    }

    /// Drops the sender (signals EOF to the background task) and waits for
    /// it to finish draining and sending all queued events before returning.
    /// Bounded to 30 seconds so a slow/unresponsive endpoint cannot stall the
    /// sweep indefinitely.
    pub async fn shutdown(self) {
        drop(self.tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), self.join_handle).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const TEST_IO_TIMEOUT: Duration = Duration::from_secs(2);

    // ── RED: basic envelope shape ──────────────────────────────────────────

    #[tokio::test]
    async fn sweep_started_posts_envelope_with_schema_version_and_sweep_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let sink =
            SweepWebhookSink::new(url, &[], Redactor::disabled(), "test-sweep-42".to_owned())
                .unwrap();
        sink.emit(SweepNotificationEvent::SweepStarted {
            total_instances: 5,
            model: "test-model".to_owned(),
        });

        let socket = accept(&listener).await;
        let req = read_http(socket).await;
        let body = body_of(&req);
        let v: serde_json::Value = serde_json::from_str(body).unwrap();

        assert_eq!(v["schema_version"]["major"], 1, "major version must be 1");
        assert_eq!(v["sweep_id"], "test-sweep-42");
        assert!(v.get("emitted_at").is_some(), "emitted_at must be present");
        assert_eq!(v["event"]["type"], "sweep_started");
        assert_eq!(v["event"]["total_instances"], 5);
        assert_eq!(v["event"]["model"], "test-model");
    }

    #[tokio::test]
    async fn sweep_completed_envelope_contains_webhook_events_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let sink =
            SweepWebhookSink::new(url, &[], Redactor::disabled(), "sweep-xyz".to_owned()).unwrap();
        sink.emit(SweepNotificationEvent::SweepCompleted {
            total_resolved: 3,
            total_attempted: 5,
            total_cost_usd: 1.23,
            wallclock_secs: 60.0,
            terminal_reason: "completed".to_owned(),
            webhook_events_dropped: 7,
        });

        let socket = accept(&listener).await;
        let req = read_http(socket).await;
        let v: serde_json::Value = serde_json::from_str(body_of(&req)).unwrap();

        assert_eq!(v["event"]["type"], "sweep_completed");
        assert_eq!(v["event"]["total_resolved"], 3);
        assert_eq!(v["event"]["webhook_events_dropped"], 7);
    }

    #[tokio::test]
    async fn instance_completed_envelope_has_correct_fields() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let sink = SweepWebhookSink::new(url, &[], Redactor::disabled(), "s1".to_owned()).unwrap();
        sink.emit(SweepNotificationEvent::InstanceCompleted {
            instance_id: "django__django-1234".to_owned(),
            resolved: true,
            failure_category: None,
            cost_usd: Some(0.42),
            duration_secs: Some(30.0),
        });

        let socket = accept(&listener).await;
        let req = read_http(socket).await;
        let v: serde_json::Value = serde_json::from_str(body_of(&req)).unwrap();

        assert_eq!(v["event"]["type"], "instance_completed");
        assert_eq!(v["event"]["instance_id"], "django__django-1234");
        assert_eq!(v["event"]["resolved"], true);
        assert_eq!(v["event"]["cost_usd"], 0.42);
        assert!(
            v["event"].get("failure_category").is_none(),
            "None fields must be omitted"
        );
    }

    // ── RED: redaction ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn redactor_strips_sensitive_env_var_from_instance_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        // Synthetic env var picked up automatically by from_config_lossy.
        let unique_val = format!("sk-deadbeef-sweep-wh-{}", addr.port());
        let mut cfg = crate::config::RedactionCfg::default();
        cfg.secret_literals.push(unique_val.clone());
        let redactor = Redactor::from_config_lossy(&cfg);

        let sink = SweepWebhookSink::new(url, &[], redactor, "sweep-redact".to_owned()).unwrap();
        sink.emit(SweepNotificationEvent::InstanceCompleted {
            instance_id: unique_val.clone(),
            resolved: false,
            failure_category: None,
            cost_usd: None,
            duration_secs: None,
        });

        let socket = accept(&listener).await;
        let req = read_http(socket).await;

        assert!(
            !req.contains(&unique_val),
            "sensitive env var value must not appear verbatim in posted payload"
        );
    }

    // ── RED: drop counting ─────────────────────────────────────────────────

    #[test]
    fn new_without_tokio_runtime_returns_no_runtime_error() {
        let err = SweepWebhookSink::new(
            "http://127.0.0.1:1".to_owned(),
            &[],
            Redactor::disabled(),
            "s".to_owned(),
        )
        .unwrap_err();
        assert!(matches!(err, SweepWebhookSinkError::NoRuntime(_)));
    }

    #[tokio::test]
    async fn with_buffer_capacity_zero_returns_error() {
        let err = SweepWebhookSink::with_buffer_capacity(
            "http://127.0.0.1:1".to_owned(),
            &[],
            Redactor::disabled(),
            "s".to_owned(),
            0,
        )
        .unwrap_err();
        assert!(matches!(err, SweepWebhookSinkError::InvalidBufferCapacity));
    }

    #[tokio::test]
    async fn full_buffer_increments_drop_counter() {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let sink = SweepWebhookSink::with_buffer_capacity(
            url,
            &[],
            Redactor::disabled(),
            "s".to_owned(),
            1,
        )
        .unwrap();
        sink.emit(SweepNotificationEvent::SweepStarted {
            total_instances: 1,
            model: "m".to_owned(),
        });
        sink.emit(SweepNotificationEvent::SweepStarted {
            total_instances: 2,
            model: "m".to_owned(),
        });
        assert!(
            sink.dropped_count() >= 1,
            "at least one event must be dropped when buffer is full"
        );
    }

    // ── RED: stable event names ────────────────────────────────────────────

    #[test]
    fn event_names_match_spec() {
        assert_eq!(
            SweepNotificationEvent::SweepStarted {
                total_instances: 1,
                model: "m".into()
            }
            .event_name(),
            "sweep_started"
        );
        assert_eq!(
            SweepNotificationEvent::SweepMilestone {
                completed_share: 0.25,
                completed: 1,
                total: 4
            }
            .event_name(),
            "sweep_milestone"
        );
        assert_eq!(
            SweepNotificationEvent::InstanceCompleted {
                instance_id: "i".into(),
                resolved: true,
                failure_category: None,
                cost_usd: None,
                duration_secs: None,
            }
            .event_name(),
            "instance_completed"
        );
        assert_eq!(
            SweepNotificationEvent::SystemicHaltTripped {
                dominant_category: "c".into(),
                share: 0.8,
            }
            .event_name(),
            "systemic_halt_tripped"
        );
        assert_eq!(
            SweepNotificationEvent::CostThresholdCrossed {
                threshold_share: 0.5,
                cumulative_cost_usd: 5.0,
                cost_limit_usd: 10.0,
            }
            .event_name(),
            "cost_threshold_crossed"
        );
        assert_eq!(
            SweepNotificationEvent::SweepCompleted {
                total_resolved: 0,
                total_attempted: 0,
                total_cost_usd: 0.0,
                wallclock_secs: 0.0,
                terminal_reason: "c".into(),
                webhook_events_dropped: 0,
            }
            .event_name(),
            "sweep_completed"
        );
        assert_eq!(
            SweepNotificationEvent::DoctorProbe {
                sweep_id: "s".into()
            }
            .event_name(),
            "doctor_probe"
        );
    }

    #[tokio::test]
    async fn invalid_header_name_rejected_at_construction() {
        let err = SweepWebhookSink::new(
            "http://127.0.0.1:1".to_owned(),
            &[("invalid header name".to_owned(), "v".to_owned())],
            Redactor::disabled(),
            "s".to_owned(),
        )
        .unwrap_err();
        assert!(
            matches!(err, SweepWebhookSinkError::InvalidHeader { .. }),
            "expected InvalidHeader, got: {err}"
        );
    }

    // ── helpers ────────────────────────────────────────────────────────────

    async fn accept(listener: &TcpListener) -> tokio::net::TcpStream {
        match tokio::time::timeout(TEST_IO_TIMEOUT, listener.accept()).await {
            Ok(Ok((s, _))) => s,
            Ok(Err(e)) => panic!("accept error: {e}"),
            Err(e) => panic!("accept timed out: {e}"),
        }
    }

    async fn read_http(mut socket: tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match tokio::time::timeout(TEST_IO_TIMEOUT, socket.read(&mut chunk)).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => panic!("read error: {e}"),
                Err(e) => panic!("read timed out: {e}"),
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if is_complete_http(&buf) {
                break;
            }
        }
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn body_of(req: &str) -> &str {
        req.split_once("\r\n\r\n").map_or("", |(_, b)| b)
    }

    fn is_complete_http(buf: &[u8]) -> bool {
        let req = String::from_utf8_lossy(buf);
        let Some(end) = req.find("\r\n\r\n") else {
            return false;
        };
        let cl = req[..end]
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        buf.len() >= end + 4 + cl
    }
}
