use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use tokio::runtime::{Handle, TryCurrentError};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{StreamEvent, StreamSink};

const DEFAULT_WEBHOOK_BUFFER_CAPACITY: usize = 1024;
const WEBHOOK_HTTP_TIMEOUT_SECS: u64 = 5;

/// Stable schema version sent in every webhook envelope.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaVersion {
    pub major: u32,
    pub minor: u32,
}

impl Default for SchemaVersion {
    fn default() -> Self {
        Self { major: 1, minor: 0 }
    }
}

/// The envelope wrapper POSTed to the webhook URL for every event.
///
/// Schema: `{ "schema_version": {"major":1,"minor":0}, "run_id": "...",
///            "event": { "type": "...", ...fields }, "emitted_at": "..." }`
/// For `RunEnded` events an additional `webhook_events_dropped` field is
/// appended at the envelope level so operators can detect missed events.
#[derive(Debug, Serialize)]
pub struct WebhookEnvelope {
    pub schema_version: SchemaVersion,
    pub run_id: String,
    pub event: StreamEvent,
    pub emitted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_events_dropped: Option<u64>,
}

/// Raw event carried through the bounded channel to the background sender.
/// The envelope is built inside the background task so that `RunEnded`'s
/// `webhook_events_dropped` is read *after* all prior HTTP responses have
/// been processed — giving an accurate final count.
struct EnvelopeMsg {
    event: StreamEvent,
    run_id: String,
}

/// A `StreamSink` that forwards events to an HTTP webhook endpoint via POST.
///
/// Non-blocking: `emit` enqueues into a bounded channel; a background task
/// dequeues and POSTs each event wrapped in the schema envelope.  Events are
/// silently dropped when the buffer is full.  HTTP failures (timeout, 4xx,
/// 5xx, network errors) are logged at `warn` and counted toward the
/// `dropped` counter, which appears in the `RunEnded` envelope.
///
/// The agent loop is **never** blocked on webhook delivery.
#[derive(Debug)]
pub struct WebhookSink {
    tx: mpsc::Sender<EnvelopeMsg>,
    /// Shared drop counter — incremented both in `emit` (buffer-full) and
    /// in the background task (HTTP failures).
    dropped: Arc<AtomicU64>,
}

/// Failure to create a [`WebhookSink`].
#[derive(Debug, thiserror::Error)]
pub enum WebhookSinkError {
    #[error("webhook sink requires an active Tokio runtime")]
    NoRuntime(#[source] TryCurrentError),
    #[error("webhook buffer capacity must be greater than zero")]
    InvalidBufferCapacity,
    #[error("invalid webhook URL: {0}")]
    InvalidUrl(String),
    #[error("failed to build webhook HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("invalid webhook header `{name}`: {reason}")]
    InvalidHeader { name: String, reason: String },
}

impl WebhookSink {
    /// Creates a `WebhookSink` with the default buffer capacity.
    ///
    /// `headers` are injected on every POST (e.g. `Authorization: Bearer …`).
    /// Headers are not logged.  Returns `Err(InvalidHeader)` if any header
    /// name or value is not a valid HTTP header — surface this before the
    /// agent starts rather than silently omitting auth headers.
    pub fn new(url: String, headers: &[(String, String)]) -> Result<Self, WebhookSinkError> {
        Self::with_buffer_capacity(url, headers, DEFAULT_WEBHOOK_BUFFER_CAPACITY)
    }

    /// Creates a `WebhookSink` with a caller-specified buffer capacity.
    pub fn with_buffer_capacity(
        url: String,
        headers: &[(String, String)],
        buffer_capacity: usize,
    ) -> Result<Self, WebhookSinkError> {
        if buffer_capacity == 0 {
            return Err(WebhookSinkError::InvalidBufferCapacity);
        }

        // Validate the URL eagerly so a typo surfaces before the run starts
        // rather than silently dropping every event from the background task.
        let parsed_url =
            reqwest::Url::parse(&url).map_err(|e| WebhookSinkError::InvalidUrl(e.to_string()))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(WebhookSinkError::InvalidUrl(format!(
                "unsupported scheme `{}`; webhook URL must use http or https",
                parsed_url.scheme()
            )));
        }

        let handle = Handle::try_current().map_err(WebhookSinkError::NoRuntime)?;

        let mut builder =
            reqwest::Client::builder().timeout(Duration::from_secs(WEBHOOK_HTTP_TIMEOUT_SECS));
        // Pre-build the default header map so we don't parse on every request.
        // Reject invalid headers eagerly so misconfigured auth headers surface
        // at startup rather than causing every POST to be rejected.
        let mut default_headers = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let header_name =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                    WebhookSinkError::InvalidHeader {
                        name: name.clone(),
                        reason: e.to_string(),
                    }
                })?;
            let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
                WebhookSinkError::InvalidHeader {
                    name: name.clone(),
                    reason: e.to_string(),
                }
            })?;
            default_headers.insert(header_name, header_value);
        }
        builder = builder.default_headers(default_headers);
        let client = builder.build().map_err(WebhookSinkError::Client)?;

        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_bg = dropped.clone();

        let (tx, mut rx) = mpsc::channel::<EnvelopeMsg>(buffer_capacity);

        handle.spawn(async move {
            while let Some(EnvelopeMsg { event, run_id }) = rx.recv().await {
                let event_type = event.event_name();
                // Build the envelope here — for RunEnded, the drop count is
                // read after all prior sends complete, giving an accurate total.
                let is_run_ended = matches!(event, StreamEvent::RunEnded { .. });
                let webhook_events_dropped =
                    is_run_ended.then(|| dropped_bg.load(Ordering::Relaxed));
                let envelope = WebhookEnvelope {
                    schema_version: SchemaVersion::default(),
                    run_id,
                    event,
                    emitted_at: Utc::now().to_rfc3339(),
                    webhook_events_dropped,
                };
                match client.post(&url).json(&envelope).send().await {
                    Ok(resp) if !resp.status().is_success() => {
                        warn!(
                            event_type,
                            status = resp.status().as_u16(),
                            "webhook POST returned non-success; counting as dropped"
                        );
                        dropped_bg.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        // Log only the classified error — reqwest::Error's Display
                        // includes the request URL, which may contain credentials.
                        let error_class = if e.is_timeout() {
                            "timeout"
                        } else if e.is_connect() {
                            "connection_refused"
                        } else {
                            "network_error"
                        };
                        warn!(
                            event_type,
                            error_class, "webhook POST failed; counting as dropped"
                        );
                        dropped_bg.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => {
                        debug!(event_type, "webhook POST succeeded");
                    }
                }
            }
        });

        Ok(Self { tx, dropped })
    }

    /// Returns the current count of events that were dropped (buffer-full or
    /// HTTP failure).  Used by `mini::run` to emit the `RunEnded` envelope
    /// field and the stderr warning.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Returns a clone of the shared drop counter so the caller can read it
    /// after this sink has been passed into a `dyn StreamSink` trait object.
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        self.dropped.clone()
    }
}

/// Internal wrapper so `WebhookSinkHandle` is what actually implements
/// `StreamSink` — the `run_id` must travel with the sink.
pub struct WebhookSinkHandle {
    inner: Arc<WebhookSink>,
    run_id: String,
}

impl WebhookSinkHandle {
    pub fn new(inner: Arc<WebhookSink>, run_id: String) -> Self {
        Self { inner, run_id }
    }
}

impl StreamSink for WebhookSinkHandle {
    fn emit(&self, event: StreamEvent) {
        let msg = EnvelopeMsg {
            event,
            run_id: self.run_id.clone(),
        };
        match self.inner.tx.try_send(msg) {
            Err(mpsc::error::TrySendError::Full(_)) => {
                debug!("dropping webhook event: buffer full");
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const TEST_IO_TIMEOUT: Duration = Duration::from_secs(1);

    fn run_id() -> String {
        "test-run-id".to_owned()
    }

    #[tokio::test]
    async fn test_webhook_sink_emits_http_post_with_envelope() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let sink_inner = Arc::new(WebhookSink::new(url, &[]).unwrap());
        let sink = WebhookSinkHandle::new(sink_inner, run_id());

        let event = StreamEvent::RunStarted {
            task: "test task".into(),
            model: "test model".into(),
            started_at: "now".into(),
        };
        sink.emit(event);

        let mut socket = accept_connection(&listener).await;
        let mut buf = vec![0; 4096];
        let n = read_bytes(&mut socket, &mut buf).await;
        let req = String::from_utf8_lossy(&buf[..n]);

        assert!(req.starts_with("POST / HTTP/1.1"));

        let body_start = req.find("\r\n\r\n").unwrap() + 4;
        let body = &req[body_start..];
        let v: serde_json::Value = serde_json::from_str(body).unwrap();

        // Must have schema envelope.
        assert_eq!(v["schema_version"]["major"], 1);
        assert!(v.get("run_id").is_some());
        assert!(v.get("emitted_at").is_some());
        assert_eq!(v["event"]["type"], "run_started");
        assert_eq!(v["event"]["task"], "test task");
    }

    #[test]
    fn new_reports_missing_tokio_runtime() {
        let err = WebhookSink::new("http://127.0.0.1:1".to_owned(), &[]).unwrap_err();
        assert!(matches!(err, WebhookSinkError::NoRuntime(_)));
    }

    #[test]
    fn with_buffer_capacity_rejects_zero_capacity() {
        let err =
            WebhookSink::with_buffer_capacity("http://127.0.0.1:1".to_owned(), &[], 0).unwrap_err();
        assert!(matches!(err, WebhookSinkError::InvalidBufferCapacity));
    }

    #[tokio::test]
    async fn invalid_header_name_is_rejected() {
        let headers = vec![("invalid header name".to_owned(), "value".to_owned())];
        let err = WebhookSink::new("http://127.0.0.1:1".to_owned(), &headers).unwrap_err();
        assert!(
            matches!(err, WebhookSinkError::InvalidHeader { ref name, .. } if name == "invalid header name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn emit_drops_events_when_buffer_is_full() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        {
            let _guard = rt.enter();
            let sink_inner = Arc::new(WebhookSink::with_buffer_capacity(url, &[], 1).unwrap());
            let sink = WebhookSinkHandle::new(sink_inner.clone(), run_id());

            sink.emit(run_started("first"));
            sink.emit(run_started("second"));

            // Buffer size is 1, so second should be dropped.
            assert_eq!(sink_inner.dropped_count(), 1);
            drop(sink);
        }

        rt.block_on(async move {
            let listener = TcpListener::from_std(std_listener).unwrap();
            let socket = accept_connection(&listener).await;
            let request = read_full_http_request(socket).await;
            assert!(request.contains("\"task\":\"first\""));
            assert!(!request.contains("\"task\":\"second\""));

            let second = tokio::time::timeout(Duration::from_millis(150), listener.accept()).await;
            assert!(second.is_err(), "second event must not be delivered");
        });
    }

    #[tokio::test]
    async fn run_ended_envelope_includes_drop_count() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let sink_inner = Arc::new(WebhookSink::new(url, &[]).unwrap());
        let sink = WebhookSinkHandle::new(sink_inner, run_id());

        sink.emit(StreamEvent::RunEnded {
            exit_reason: "submitted".into(),
            failure_category: None,
            final_output: None,
            steps: 1,
            total_cost_usd: 0.0,
            ended_at: "now".into(),
        });

        let socket = accept_connection(&listener).await;
        let request = read_full_http_request(socket).await;
        let body_start = request.find("\r\n\r\n").unwrap() + 4;
        let v: serde_json::Value = serde_json::from_str(&request[body_start..]).unwrap();

        assert!(
            v.get("webhook_events_dropped").is_some(),
            "run_ended envelope must have webhook_events_dropped"
        );
    }

    #[tokio::test]
    async fn custom_headers_appear_in_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let headers = vec![("X-Test-Header".to_owned(), "hello-world".to_owned())];
        let sink_inner = Arc::new(WebhookSink::new(url, &headers).unwrap());
        let sink = WebhookSinkHandle::new(sink_inner, run_id());

        sink.emit(run_started("hdr-test"));

        let socket = accept_connection(&listener).await;
        let request = read_full_http_request(socket).await;
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains("x-test-header: hello-world"),
            "custom header not found in: {request}"
        );
    }

    fn run_started(task: &str) -> StreamEvent {
        StreamEvent::RunStarted {
            task: task.to_owned(),
            model: "test model".to_owned(),
            started_at: "now".to_owned(),
        }
    }

    async fn accept_connection(listener: &TcpListener) -> tokio::net::TcpStream {
        match tokio::time::timeout(TEST_IO_TIMEOUT, listener.accept()).await {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(e)) => panic!("failed to accept: {e}"),
            Err(e) => panic!("timed out: {e}"),
        }
    }

    async fn read_bytes<R>(reader: &mut R, buf: &mut [u8]) -> usize
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        match tokio::time::timeout(TEST_IO_TIMEOUT, reader.read(buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => panic!("read error: {e}"),
            Err(e) => panic!("read timeout: {e}"),
        }
    }

    async fn read_full_http_request(mut socket: tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = read_bytes(&mut socket, &mut chunk).await;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if is_complete(&buf) {
                break;
            }
        }
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn is_complete(buf: &[u8]) -> bool {
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
