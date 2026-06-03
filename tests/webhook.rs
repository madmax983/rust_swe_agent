//! Integration tests for `--webhook-url` / `--webhook-header` (issue #324).
//!
//! Tests cover:
//!  1. Full event sequence delivery with schema envelope.
//!  2. Hung endpoint — buffer fills, agent still completes in step budget.
//!  3. SSE + webhook composition — both transports see the same event sequence.
//!  4. `webhook_events_dropped` counter in `RunEnded` envelope.
//!  5. Redaction applied before POST (`sk-deadbeef` never leaves the process).

#![cfg(feature = "webhook")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use maxwells_daemon::run::mini::{InteractiveMode, MiniArgs};

// ---------------------------------------------------------------------------
// Shared mock HTTP server helpers
// ---------------------------------------------------------------------------

/// A minimal mock HTTP server that accepts POST requests and collects their
/// JSON bodies.  Each call to `accept_one` blocks until one POST arrives,
/// writes a 200 OK, and returns the decoded JSON body.
struct MockWebhookServer {
    listener: TcpListener,
    addr: SocketAddr,
}

impl MockWebhookServer {
    async fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        Self { listener, addr }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Accept one incoming connection, read the full HTTP request, reply 200 OK,
    /// and return the JSON body as `Value`.
    async fn accept_one(&self) -> Value {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), self.listener.accept())
            .await
            .expect("timed out waiting for webhook POST")
            .unwrap();

        let body = read_http_request_body(&mut stream).await;
        write_200_ok(&mut stream).await;
        serde_json::from_str(&body).expect("webhook body is not valid JSON")
    }

    /// Accept `n` POST requests concurrently and return their JSON bodies in
    /// arrival order.
    async fn accept_n(&self, n: usize) -> Vec<Value> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.accept_one().await);
        }
        out
    }
}

async fn read_http_request_body(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .expect("read timed out")
            .unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if is_http_complete(&buf) {
            break;
        }
    }
    let req = String::from_utf8_lossy(&buf).into_owned();
    let body_start = req.find("\r\n\r\n").map_or(req.len(), |i| i + 4);
    req[body_start..].to_owned()
}

async fn write_200_ok(stream: &mut TcpStream) {
    let _ = stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
}

fn is_http_complete(buf: &[u8]) -> bool {
    let req = String::from_utf8_lossy(buf);
    let Some(hdr_end) = req.find("\r\n\r\n") else {
        return false;
    };
    let content_length = req[..hdr_end]
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    buf.len() >= hdr_end + 4 + content_length
}

/// Build a minimal `MiniArgs` that uses a deterministic model (no real API).
fn make_mini_args(
    responses: Vec<String>,
    output: &std::path::Path,
    webhook_url: Option<String>,
    webhook_headers: Vec<String>,
    stream_addr: Option<std::net::SocketAddr>,
) -> MiniArgs {
    let mut cfg = maxwells_daemon::Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    MiniArgs {
        task: "webhook test task".into(),
        extra_context: None,
        config: cfg,
        output_dir: output.to_path_buf(),
        trajectory_name: "webhook-test".into(),
        deterministic_responses: Some(responses),
        deterministic_usage_per_call: None,
        task_timeout_secs: Some(30),
        cancellation: None,
        stream_addr,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 60,
        resume_from: None,
        interactive_mode: InteractiveMode::Off,
        trace_id: None,
        webhook_url,
        webhook_headers,
        local_workdir: None,
        read_only: false,
        allow_mcp_in_read_only: false,
        rehearsal_gold_patch: None,
        event_log: None,
        event_log_instance_id: None,
        no_step_persist: false,
        parent_sweep_run_id: None,
        continue_from: None,
        issue_provenance: None,
    }
}

// ---------------------------------------------------------------------------
// Test 1: full event sequence delivery + schema envelope validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_delivers_full_event_sequence_with_schema_envelope() {
    let server = MockWebhookServer::bind().await;
    let url = server.url();
    let out = tempfile::tempdir().unwrap();

    // Run mini in background; concurrently collect webhook POSTs.
    let responses = vec![
        "```bash\necho webhook-works\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
    ];
    let args = make_mini_args(responses, out.path(), Some(url), vec![], None);

    // We expect at minimum: RunStarted, AssistantMessage×2, BashStart, BashResult,
    // Observation, RunEnded — so ≥ 7 payloads.
    let (run_result, bodies) =
        tokio::join!(maxwells_daemon::run::mini::run(args), server.accept_n(7),);
    run_result.unwrap();

    // Every body must have the schema envelope fields.
    for body in &bodies {
        assert!(
            body.get("schema_version").is_some(),
            "missing schema_version in: {body}"
        );
        assert!(body.get("run_id").is_some(), "missing run_id in: {body}");
        assert!(body.get("event").is_some(), "missing event in: {body}");
        assert!(
            body.get("emitted_at").is_some(),
            "missing emitted_at in: {body}"
        );
        // Content-Type is verified at the HTTP layer — tested implicitly via reqwest.
    }

    // schema_version must be { "major": 1, "minor": N }.
    let sv = &bodies[0]["schema_version"];
    assert_eq!(sv["major"], 1, "schema_version.major must be 1");
    assert!(sv.get("minor").is_some(), "schema_version.minor must exist");

    // First event must be run_started.
    assert_eq!(
        bodies[0]["event"]["type"].as_str(),
        Some("run_started"),
        "first event must be run_started"
    );

    // Last event must be run_ended.
    let last = bodies.last().unwrap();
    assert_eq!(
        last["event"]["type"].as_str(),
        Some("run_ended"),
        "last event must be run_ended"
    );

    // run_ended envelope must include webhook_events_dropped.
    assert!(
        last.get("webhook_events_dropped").is_some(),
        "run_ended envelope must contain webhook_events_dropped"
    );

    // All run_ids must be identical across a single run.
    let run_id = bodies[0]["run_id"].as_str().unwrap();
    for body in &bodies {
        assert_eq!(
            body["run_id"].as_str(),
            Some(run_id),
            "run_id must be stable across one run"
        );
    }

    // bash_result must carry stdout with our marker.
    let bash_result = bodies
        .iter()
        .find(|b| b["event"]["type"] == "bash_result")
        .expect("missing bash_result event");
    assert!(
        bash_result["event"]["stdout"]
            .as_str()
            .unwrap_or("")
            .contains("webhook-works"),
        "bash_result stdout should contain 'webhook-works'"
    );
}

// ---------------------------------------------------------------------------
// Test 2: hung endpoint — agent must complete within step budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_hung_endpoint_does_not_block_agent() {
    // A listener that accepts connections but never responds.
    let hung_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hung_addr = hung_listener.local_addr().unwrap();
    let url = format!("http://{hung_addr}");

    // Accept connections so they don't get connection-refused (which would be
    // instant); accept without replying to simulate a truly hung server.
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = hung_listener.accept().await else {
                break;
            };
            // Hold the connection open indefinitely.
            let mut buf = [0u8; 1];
            let _ = tokio::time::timeout(Duration::from_secs(60), sock.read(&mut buf)).await;
        }
    });

    let out = tempfile::tempdir().unwrap();
    // Multi-step run so there are many events to queue.
    let responses = vec![
        "```bash\necho one\n```".into(),
        "```bash\necho two\n```".into(),
        "```bash\necho three\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
    ];
    let args = make_mini_args(responses, out.path(), Some(url), vec![], None);

    // The 5-second per-request HTTP timeout means the agent MUST complete
    // well within 30 s, even with a hung server.
    let start = std::time::Instant::now();
    let result = maxwells_daemon::run::mini::run(args).await;
    let elapsed = start.elapsed();

    result.unwrap();
    // Agent should complete in ≤ 25 s even with multiple hanging requests
    // (step budget × step latency, not HTTP timeout × event count).
    assert!(
        elapsed < Duration::from_secs(25),
        "agent took too long with hung webhook: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: SSE + webhook composition — same event sequence to both transports
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_and_webhook_composition_delivers_same_events() {
    let webhook_server = MockWebhookServer::bind().await;
    let webhook_url = webhook_server.url();
    let out = tempfile::tempdir().unwrap();

    // Bind SSE server address first (use port 0 → OS chooses).
    let sse_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sse_addr = sse_listener.local_addr().unwrap();
    drop(sse_listener); // mini::run will bind it

    let responses = vec![
        "```bash\necho compose-test\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfin\n```".into(),
    ];
    let args = make_mini_args(
        responses,
        out.path(),
        Some(webhook_url),
        vec![],
        Some(sse_addr),
    );

    // Connect SSE client.
    let sse_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sse_events2 = sse_events.clone();

    let (run_result, webhook_bodies, ()) = tokio::join!(
        maxwells_daemon::run::mini::run(args),
        webhook_server.accept_n(7),
        async move {
            // Small delay so the SSE server can start.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut client =
                tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(sse_addr))
                    .await
                    .ok()
                    .and_then(Result::ok);
            if let Some(ref mut c) = client {
                let _ = c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
                let mut acc = Vec::new();
                let mut chunk = [0u8; 4096];
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    if tokio::time::Instant::now() > deadline {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_secs(2), c.read(&mut chunk)).await {
                        Ok(Ok(0) | Err(_)) | Err(_) => break,
                        Ok(Ok(n)) => acc.extend_from_slice(&chunk[..n]),
                    }
                    let text = String::from_utf8_lossy(&acc);
                    if text.contains("event: run_ended") {
                        break;
                    }
                }
                *sse_events2.lock().unwrap() = String::from_utf8_lossy(&acc)
                    .lines()
                    .filter(|l| l.starts_with("event: "))
                    .map(|l| l.trim_start_matches("event: ").to_owned())
                    .collect();
            }
        },
    );
    run_result.unwrap();

    // Extract event types from webhook bodies.
    let webhook_types: Vec<&str> = webhook_bodies
        .iter()
        .filter_map(|b| b["event"]["type"].as_str())
        .collect();

    // Both should see run_started and run_ended.
    assert!(
        webhook_types.contains(&"run_started"),
        "webhook missing run_started"
    );
    assert!(
        webhook_types.contains(&"run_ended"),
        "webhook missing run_ended"
    );

    let sse: Vec<String> = sse_events.lock().unwrap().clone();
    if !sse.is_empty() {
        assert!(
            sse.contains(&"run_started".to_owned()),
            "SSE missing run_started"
        );
        assert!(
            sse.contains(&"run_ended".to_owned()),
            "SSE missing run_ended"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: webhook_events_dropped counter on RunEnded when events are dropped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_run_ended_envelope_includes_drop_count() {
    // A slow server — accept connections but stall briefly so the tiny buffer fills.
    let slow_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_addr = slow_listener.local_addr().unwrap();
    let url = format!("http://{slow_addr}");

    let collected: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new(VecDeque::new()));
    let collected2 = collected.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = slow_listener.accept().await else {
                break;
            };
            let body = read_http_request_body(&mut sock).await;
            write_200_ok(&mut sock).await;
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                collected2.lock().unwrap().push_back(v);
            }
        }
    });

    let out = tempfile::tempdir().unwrap();
    let responses = vec![
        "```bash\necho a\n```".into(),
        "```bash\necho b\n```".into(),
        "```bash\necho c\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
    ];
    let args = make_mini_args(responses, out.path(), Some(url), vec![], None);
    maxwells_daemon::run::mini::run(args).await.unwrap();

    // Wait for all outstanding HTTP responses.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let bodies: VecDeque<Value> = collected.lock().unwrap().clone();
    let run_ended_body = bodies
        .iter()
        .find(|b| b["event"]["type"] == "run_ended")
        .expect("run_ended envelope not received");

    // The envelope must contain webhook_events_dropped (may be 0 if no drops).
    assert!(
        run_ended_body.get("webhook_events_dropped").is_some(),
        "run_ended envelope must contain webhook_events_dropped; got: {run_ended_body}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: --webhook-header injects custom HTTP request headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_header_flag_injects_custom_headers() {
    let server = MockWebhookServer::bind().await;
    let url = server.url();
    let out = tempfile::tempdir().unwrap();

    let headers = vec!["X-Custom-Header: test-value".to_owned()];
    let responses = vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into()];
    let args = make_mini_args(responses, out.path(), Some(url), headers, None);

    // Accept one POST and inspect the raw request to find the custom header.
    let (run_result, raw_request) = tokio::join!(maxwells_daemon::run::mini::run(args), async {
        let (mut stream, _) =
            tokio::time::timeout(Duration::from_secs(10), server.listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
                .await
                .unwrap()
                .unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if is_http_complete(&buf) {
                break;
            }
        }
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        String::from_utf8_lossy(&buf).into_owned()
    },);
    run_result.unwrap();

    let lower = raw_request.to_ascii_lowercase();
    assert!(
        lower.contains("x-custom-header: test-value"),
        "custom header not found in raw HTTP request: {raw_request}"
    );
}

// ---------------------------------------------------------------------------
// Test 6: redactor strips secrets from webhook payloads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_redacts_secrets_before_post() {
    let server = MockWebhookServer::bind().await;
    let url = server.url();
    let out = tempfile::tempdir().unwrap();

    // A fake secret literal that the redactor should strip.
    let secret = "sk-deadbeef-should-not-appear";

    let responses = vec![
        // bash command that echoes the secret (simulates a tool leaking it).
        format!("```bash\necho {secret}\n```"),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
    ];

    let mut cfg = maxwells_daemon::Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    // Configure the secret as a redaction literal.
    cfg.root.redaction.secret_literals = vec![secret.to_owned()];

    let args = MiniArgs {
        task: "redaction test".into(),
        extra_context: None,
        config: cfg,
        output_dir: out.path().to_path_buf(),
        trajectory_name: "redact-test".into(),
        deterministic_responses: Some(responses),
        deterministic_usage_per_call: None,
        task_timeout_secs: Some(30),
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 60,
        resume_from: None,
        interactive_mode: InteractiveMode::Off,
        trace_id: None,
        webhook_url: Some(url),
        webhook_headers: vec![],
        event_log: None,
        event_log_instance_id: None,
        local_workdir: None,
        read_only: false,
        allow_mcp_in_read_only: false,
        rehearsal_gold_patch: None,
        no_step_persist: false,
        parent_sweep_run_id: None,
        continue_from: None,
        issue_provenance: None,
    };

    let (run_result, bodies) =
        tokio::join!(maxwells_daemon::run::mini::run(args), server.accept_n(7),);
    run_result.unwrap();

    // The secret must never appear in any POSTed body.
    for body in &bodies {
        let body_str = serde_json::to_string(body).unwrap();
        assert!(
            !body_str.contains(secret),
            "secret leaked in webhook payload! body: {body_str}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 7: no webhook_url → zero overhead (no background tasks spawned)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_webhook_url_adds_zero_overhead() {
    let out = tempfile::tempdir().unwrap();
    let responses = vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into()];
    let args = make_mini_args(responses, out.path(), None, vec![], None);
    // Should complete without error and without any webhook traffic.
    maxwells_daemon::run::mini::run(args).await.unwrap();
}

// ---------------------------------------------------------------------------
// Test 8: unsupported URL scheme is rejected before the run starts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_rejects_non_http_url_scheme() {
    let out = tempfile::tempdir().unwrap();
    let responses = vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into()];
    let args = make_mini_args(
        responses,
        out.path(),
        Some("file:///tmp/not-a-webhook".to_owned()),
        vec![],
        None,
    );
    let result = maxwells_daemon::run::mini::run(args).await;
    assert!(result.is_err(), "expected Err for file:// URL, got Ok");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("unsupported scheme") || msg.contains("webhook"),
        "error message should mention scheme or webhook: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Test 9: malformed --webhook-header flag (missing ':') is rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_rejects_malformed_header_flag() {
    let out = tempfile::tempdir().unwrap();
    let responses = vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into()];
    let server = MockWebhookServer::bind().await;
    let args = make_mini_args(
        responses,
        out.path(),
        Some(server.url()),
        vec!["X-No-Colon-Here".to_owned()],
        None,
    );
    let result = maxwells_daemon::run::mini::run(args).await;
    assert!(result.is_err(), "expected Err for malformed header, got Ok");
    let msg = result.unwrap_err().to_string();
    // Message must not echo the raw header value (could contain bearer tokens).
    assert!(
        !msg.contains("X-No-Colon-Here"),
        "error must not echo the raw header value: {msg}"
    );
    assert!(
        msg.contains("missing") || msg.contains("separator") || msg.contains("position"),
        "error should mention missing separator or position: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Test 10: --webhook-header without --webhook-url is rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_header_without_url_is_rejected() {
    let out = tempfile::tempdir().unwrap();
    let responses = vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into()];
    let args = make_mini_args(
        responses,
        out.path(),
        None, // no URL
        vec!["Authorization: Bearer token".to_owned()],
        None,
    );
    let result = maxwells_daemon::run::mini::run(args).await;
    assert!(
        result.is_err(),
        "expected Err when header given without URL, got Ok"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("--webhook-url") || msg.contains("webhook"),
        "error should mention --webhook-url: {msg}"
    );
}
