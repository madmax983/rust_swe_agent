use std::time::Duration;
use tokio::runtime::{Handle, TryCurrentError};
use tokio::sync::mpsc;
use tracing::debug;

use super::{StreamEvent, StreamSink};

const DEFAULT_WEBHOOK_BUFFER_CAPACITY: usize = 1024;

/// A `StreamSink` that forwards events to an HTTP webhook endpoint via POST.
///
/// It does not block the agent loop. Instead, `emit` tries to enqueue the event
/// into a bounded channel, and a background task dequeues and POSTs each event
/// to the specified URL using `reqwest`. Events are dropped when the bounded
/// buffer is full.
#[derive(Debug)]
pub struct WebhookSink {
    tx: mpsc::Sender<StreamEvent>,
}

/// Failure to create a [`WebhookSink`].
#[derive(Debug, thiserror::Error)]
pub enum WebhookSinkError {
    /// No Tokio runtime is active on the current thread.
    #[error("webhook sink requires an active Tokio runtime")]
    NoRuntime(#[source] TryCurrentError),
    /// Webhook buffering must be bounded to at least one event.
    #[error("webhook buffer capacity must be greater than zero")]
    InvalidBufferCapacity,
    /// The HTTP client could not be built.
    #[error("failed to build webhook HTTP client")]
    Client(#[source] reqwest::Error),
}

impl WebhookSink {
    /// Creates a new `WebhookSink` with the default bounded buffer capacity.
    pub fn new(url: String) -> Result<Self, WebhookSinkError> {
        Self::with_buffer_capacity(url, DEFAULT_WEBHOOK_BUFFER_CAPACITY)
    }

    /// Creates a new `WebhookSink` with a caller-provided bounded buffer capacity.
    pub fn with_buffer_capacity(
        url: String,
        buffer_capacity: usize,
    ) -> Result<Self, WebhookSinkError> {
        if buffer_capacity == 0 {
            return Err(WebhookSinkError::InvalidBufferCapacity);
        }

        let handle = Handle::try_current().map_err(WebhookSinkError::NoRuntime)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(WebhookSinkError::Client)?;
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(buffer_capacity);

        handle.spawn(async move {
            while let Some(event) = rx.recv().await {
                match client.post(&url).json(&event).send().await {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            debug!("Webhook returned non-success status: {}", resp.status());
                        }
                    }
                    Err(e) => {
                        debug!("Failed to send webhook event: {}", e);
                    }
                }
            }
        });

        Ok(Self { tx })
    }
}

impl StreamSink for WebhookSink {
    fn emit(&self, event: StreamEvent) {
        match self.tx.try_send(event) {
            Err(mpsc::error::TrySendError::Full(_)) => {
                debug!("Dropping webhook event because the buffer is full");
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

    const WEBHOOK_TEST_IO_TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn test_webhook_sink_emits_http_post() {
        // Spin up a local TCP listener to act as our mock HTTP server.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        // Create the sink and emit an event.
        let sink = WebhookSink::new(url).unwrap();

        let event = StreamEvent::RunStarted {
            task: "test task".into(),
            model: "test model".into(),
            started_at: "now".into(),
        };

        sink.emit(event.clone());

        // Wait for the incoming connection from reqwest and read the payload.
        let mut socket = accept_webhook_connection(&listener).await;

        let mut buf = vec![0; 1024];
        let n = read_webhook_bytes(&mut socket, &mut buf).await;
        let request_str = String::from_utf8_lossy(&buf[..n]);

        // Verify it's a POST request
        assert!(request_str.starts_with("POST / HTTP/1.1"));

        // Find the JSON body
        let body_start = request_str.find("\r\n\r\n").unwrap() + 4;
        let body = &request_str[body_start..];

        // Parse it back and assert it matches (rudimentary check).
        // Since StreamEvent serialization is tested elsewhere, we just check for basic substrings.
        assert!(body.contains("\"type\":\"run_started\""));
        assert!(body.contains("\"task\":\"test task\""));
        assert!(body.contains("\"model\":\"test model\""));
    }

    #[test]
    fn new_reports_missing_tokio_runtime() {
        let err = WebhookSink::new("http://127.0.0.1:1".to_owned()).unwrap_err();

        assert!(matches!(err, WebhookSinkError::NoRuntime(_)));
    }

    #[test]
    fn with_buffer_capacity_rejects_zero_capacity() {
        let err =
            WebhookSink::with_buffer_capacity("http://127.0.0.1:1".to_owned(), 0).unwrap_err();

        assert!(matches!(err, WebhookSinkError::InvalidBufferCapacity));
    }

    #[test]
    fn emit_drops_events_when_webhook_buffer_is_full() {
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
            let sink = WebhookSink::with_buffer_capacity(url, 1).unwrap();

            sink.emit(run_started("first"));
            sink.emit(run_started("second"));

            assert_eq!(sink.tx.capacity(), 0);
            assert_eq!(sink.tx.max_capacity(), 1);
            drop(sink);
        }

        rt.block_on(async move {
            let listener = TcpListener::from_std(std_listener).unwrap();
            let socket = accept_webhook_connection(&listener).await;

            let request = read_http_request(socket).await;
            assert!(request.contains("\"task\":\"first\""));
            assert!(!request.contains("\"task\":\"second\""));

            let second = tokio::time::timeout(Duration::from_millis(150), listener.accept()).await;
            assert!(second.is_err());
        });
    }

    fn run_started(task: &str) -> StreamEvent {
        StreamEvent::RunStarted {
            task: task.to_owned(),
            model: "test model".to_owned(),
            started_at: "now".to_owned(),
        }
    }

    async fn accept_webhook_connection(listener: &TcpListener) -> tokio::net::TcpStream {
        match tokio::time::timeout(WEBHOOK_TEST_IO_TIMEOUT, listener.accept()).await {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(e)) => panic!("failed to accept webhook POST connection: {e}"),
            Err(e) => panic!("timed out waiting for webhook POST connection: {e}"),
        }
    }

    async fn read_webhook_bytes<R>(reader: &mut R, buf: &mut [u8]) -> usize
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        match tokio::time::timeout(WEBHOOK_TEST_IO_TIMEOUT, reader.read(buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => panic!("failed to read webhook POST request: {e}"),
            Err(e) => panic!("timed out reading webhook POST request: {e}"),
        }
    }

    async fn read_http_request(mut socket: tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0_u8; 1024];

        loop {
            let n = read_webhook_bytes(&mut socket, &mut chunk).await;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if request_is_complete(&buf) {
                break;
            }
        }

        socket
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();

        String::from_utf8_lossy(&buf).into_owned()
    }

    fn request_is_complete(buf: &[u8]) -> bool {
        let request = String::from_utf8_lossy(buf);
        let Some(header_end) = request.find("\r\n\r\n") else {
            return false;
        };
        let content_length = request[..header_end]
            .lines()
            .find_map(|line| {
                let line = line.to_ascii_lowercase();
                line.strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);

        buf.len() >= header_end + 4 + content_length
    }
}
