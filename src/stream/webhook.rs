use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error};

use super::{StreamEvent, StreamSink};

/// A `StreamSink` that forwards events to an HTTP webhook endpoint via POST.
///
/// It does not block the agent loop. Instead, `emit` sends the event into an
/// unbounded channel, and a background task dequeues and POSTs each event to
/// the specified URL using `reqwest`.
#[derive(Debug)]
pub struct WebhookSink {
    tx: mpsc::UnboundedSender<StreamEvent>,
}

impl WebhookSink {
    /// Creates a new `WebhookSink` and spawns the background posting task.
    pub fn new(url: String) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();

        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build();

            let client = match client {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to build reqwest client for webhook: {}", e);
                    return;
                }
            };

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

        Self { tx }
    }
}

impl StreamSink for WebhookSink {
    fn emit(&self, event: StreamEvent) {
        // We ignore the error. If the background task died, we just drop the event.
        let _ = self.tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_webhook_sink_emits_http_post() {
        // Spin up a local TCP listener to act as our mock HTTP server.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        // Create the sink and emit an event.
        let sink = WebhookSink::new(url);

        let event = StreamEvent::RunStarted {
            task: "test task".into(),
            model: "test model".into(),
            started_at: "now".into(),
        };

        sink.emit(event.clone());

        // Wait for the incoming connection from reqwest and read the payload.
        let (mut socket, _) = listener.accept().await.unwrap();

        let mut buf = vec![0; 1024];
        let n = socket.read(&mut buf).await.unwrap();
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
}
