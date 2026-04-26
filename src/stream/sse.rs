//! Minimal HTTP/1.1 + Server-Sent Events server for streaming
//! `StreamEvent`s.
//!
//! Why not axum/hyper?
//! * Project policy keeps deps lean (`litellm-rs` even has default
//!   features stripped to avoid pulling actix). SSE is one direction
//!   and one MIME type; a few hundred lines of tokio is enough.
//!
//! Behavior:
//! * Bind a TCP listener; spawn one task per accepted connection.
//! * Read just enough of the HTTP request to consume `\r\n\r\n` (we
//!   ignore method / path / headers — any GET subscribes).
//! * Reply 200 with `Content-Type: text/event-stream` and stream
//!   `event: <name>\ndata: <json>\n\n` frames.
//! * On any write error (client disconnected), drop the receiver and
//!   the per-connection task — never propagate to the agent.
//! * `shutdown()` signals the accept loop to stop and joins the task,
//!   so the runner can exit cleanly after the agent finishes.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use super::{BroadcastSink, StreamEvent};

/// SSE response head + initial framing. Sent verbatim once the request
/// line + headers have been consumed. CORS open since this is a
/// local-dev tool.
const SSE_RESPONSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache, no-transform\r\n\
Connection: keep-alive\r\n\
X-Accel-Buffering: no\r\n\
Access-Control-Allow-Origin: *\r\n\
\r\n\
: connected\n\
retry: 2000\n\n";

/// Handle to a running SSE server. Drop or call `shutdown()` to stop.
pub struct SseServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl SseServer {
    /// Bind and start serving. Returns once the listener is bound, so
    /// callers can synchronously attach a client immediately afterwards
    /// (no race on first events).
    pub async fn start(addr: SocketAddr, sink: Arc<BroadcastSink>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        let (sd_tx, sd_rx) = oneshot::channel();
        let handle = tokio::spawn(accept_loop(listener, sink, sd_rx));
        Ok(Self {
            addr: bound,
            shutdown_tx: Some(sd_tx),
            handle: Some(handle),
        })
    }

    /// The actual bound address (resolves port `0` to the OS-picked port).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signal the accept loop to stop and wait for it. Per-connection
    /// tasks are NOT joined — they're already living off broadcast::recv
    /// and will exit on their next write attempt or `Closed` recv.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

impl Drop for SseServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    sink: Arc<BroadcastSink>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::debug!("sse: shutdown signal received");
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        tracing::debug!(?peer, "sse: client connected");
                        let rx = sink.subscribe();
                        tokio::spawn(handle_connection(stream, rx, peer));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sse: accept failed");
                        // Transient OS-level errors shouldn't kill the
                        // server. A small yield avoids a hot loop on
                        // persistent failure (e.g. fd exhaustion).
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    mut rx: broadcast::Receiver<StreamEvent>,
    peer: SocketAddr,
) {
    if let Err(e) = consume_request_head(&mut stream).await {
        tracing::debug!(?peer, error = %e, "sse: bad request head");
        return;
    }

    if stream.write_all(SSE_RESPONSE_HEAD).await.is_err() {
        return;
    }
    if stream.flush().await.is_err() {
        return;
    }

    loop {
        match rx.recv().await {
            Ok(event) => {
                let frame = format_sse_frame(&event);
                if stream.write_all(frame.as_bytes()).await.is_err() {
                    tracing::debug!(?peer, "sse: client write failed (disconnect)");
                    return;
                }
                // Best-effort flush; ignore failures (next write catches it).
                let _ = stream.flush().await;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // Slow consumer dropped events. Tell them via a comment
                // and stay connected — better partial than dead.
                let note = format!(": lagged {skipped}\n\n");
                if stream.write_all(note.as_bytes()).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                // Sink dropped (agent run completed and CLI is exiting).
                let _ = stream.write_all(b"event: stream_closed\ndata: {}\n\n").await;
                let _ = stream.shutdown().await;
                return;
            }
        }
    }
}

/// Read until `\r\n\r\n` or the cap (8 KiB) — whichever first. We don't
/// parse the request; any GET subscribes. Cap defends against a slow /
/// malicious client streaming headers forever.
async fn consume_request_head(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    loop {
        if total >= buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed before request head",
            ));
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(());
        }
    }
}

fn format_sse_frame(event: &StreamEvent) -> String {
    let payload = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    // SSE forbids embedded newlines in a single `data:` line; agent
    // outputs (stdout, observations) routinely contain them. Split on
    // `\n` and emit one `data:` line per chunk — the EventSource API
    // re-joins them with `\n`.
    let mut out = String::with_capacity(payload.len() + 64);
    out.push_str("event: ");
    out.push_str(event.event_name());
    out.push('\n');
    for line in payload.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::stream::StreamSink;
    use std::time::Duration;

    fn ev(step: u32) -> StreamEvent {
        StreamEvent::BashStart {
            step,
            command: format!("echo {step}"),
            timestamp: "t".into(),
        }
    }

    #[test]
    fn frame_contains_event_and_data() {
        let f = format_sse_frame(&ev(1));
        assert!(f.starts_with("event: bash_start\n"));
        assert!(f.contains("data: "));
        assert!(f.ends_with("\n\n"));
    }

    #[test]
    fn multiline_payload_is_split_across_data_lines() {
        let e = StreamEvent::Observation {
            step: 0,
            content: "line1\nline2".into(),
            timestamp: "t".into(),
        };
        let f = format_sse_frame(&e);
        // Multi-line value will JSON-encode the \n as `\n` (escaped),
        // so the frame should still be a single `data:` line. Verify:
        // exactly one `data: ` line.
        let data_lines = f.lines().filter(|l| l.starts_with("data: ")).count();
        assert_eq!(data_lines, 1, "frame: {f:?}");
    }

    #[tokio::test]
    async fn end_to_end_subscribe_and_receive() {
        let sink = Arc::new(BroadcastSink::new(8));
        let server = SseServer::start("127.0.0.1:0".parse().unwrap(), sink.clone())
            .await
            .unwrap();
        let addr = server.local_addr();

        // Subscribe with a raw TCP request.
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /events HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();

        // Wait for subscriber to attach before emitting.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if sink.receiver_count() >= 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() <= deadline,
                "client never attached"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Emit one event.
        sink.emit(ev(42));

        // Read a chunk and check we see the event name + payload.
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let s = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            s.contains("HTTP/1.1 200 OK"),
            "missing status, got: {s:?}"
        );
        assert!(
            s.contains("text/event-stream"),
            "missing content-type, got: {s:?}"
        );

        // Drain until we see the event frame (handshake may flush
        // separately from first data frame).
        let mut acc = s;
        while !acc.contains("event: bash_start") {
            let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0, "stream closed before event frame");
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(acc.contains("\"command\":\"echo 42\""));

        // Disconnect mid-stream — agent emit must not panic.
        drop(client);
        // Give the server a tick to notice.
        tokio::time::sleep(Duration::from_millis(50)).await;
        sink.emit(ev(99));
        sink.emit(ev(100));

        server.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_releases_listener() {
        let sink = Arc::new(BroadcastSink::new(8));
        let server = SseServer::start("127.0.0.1:0".parse().unwrap(), sink.clone())
            .await
            .unwrap();
        let addr = server.local_addr();
        server.shutdown().await;
        // Port should be reusable.
        let _again = TcpListener::bind(addr).await.unwrap();
    }
}
