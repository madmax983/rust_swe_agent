//! Integration: agent run with a `BroadcastSink` and a live SSE
//! subscriber. Verifies (1) events flow in real time, (2) per-step
//! outcomes (commands, exit codes, stdout) are emitted, and (3) a
//! mid-run client disconnect does not crash the agent.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use rust_swe_agent::agent::default::DefaultAgentBuilder;
use rust_swe_agent::stream::{BroadcastSink, SseServer, StreamEvent, StreamSink};
use rust_swe_agent::{
    Agent, Config, DeterministicModel, Environment, ExitReason, LocalEnvironment,
};

fn make_agent_with_sink(
    responses: Vec<String>,
    sink: Arc<dyn StreamSink>,
) -> rust_swe_agent::DefaultAgent {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    let model = Arc::new(DeterministicModel::new(responses));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "stream test".into(),
        extra_context: None,
        renderer: None,
        stream: Some(sink),
    }
    .build()
    .unwrap()
}

#[tokio::test]
async fn broadcast_sink_receives_step_outcomes() {
    let bcast = Arc::new(BroadcastSink::default());
    let mut rx = bcast.subscribe();

    let mut agent = make_agent_with_sink(
        vec![
            "```bash\necho streaming-works\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
        ],
        bcast.clone() as Arc<dyn StreamSink>,
    );

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    // Drain receiver and collect every event variant we saw.
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }

    assert!(matches!(events.first(), Some(StreamEvent::RunStarted { .. })));

    let saw_bash_start = events
        .iter()
        .any(|e| matches!(e, StreamEvent::BashStart { command, .. } if command == "echo streaming-works"));
    assert!(saw_bash_start, "missing BashStart");

    let saw_bash_result = events.iter().any(|e| {
        matches!(
            e,
            StreamEvent::BashResult { exit_code: 0, stdout, .. }
                if stdout.contains("streaming-works")
        )
    });
    assert!(saw_bash_result, "missing BashResult with stdout");

    let saw_observation = events
        .iter()
        .any(|e| matches!(e, StreamEvent::Observation { .. }));
    assert!(saw_observation, "missing Observation");

    let last = events.last().unwrap();
    match last {
        StreamEvent::RunEnded { exit_reason, final_output, .. } => {
            assert_eq!(exit_reason, "submitted");
            assert_eq!(final_output.as_deref(), Some("final"));
        }
        other => panic!("expected RunEnded last, got {other:?}"),
    }
}

#[tokio::test]
async fn sse_client_receives_events_over_tcp() {
    let bcast = Arc::new(BroadcastSink::default());
    let server = SseServer::start("127.0.0.1:0".parse().unwrap(), bcast.clone())
        .await
        .unwrap();
    let addr = server.local_addr();

    // Connect first; then run the agent.
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();

    // Wait for the per-connection task to subscribe.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while bcast.receiver_count() < 1 {
        assert!(
            tokio::time::Instant::now() <= deadline,
            "client never attached"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Run agent in background so we can read concurrently.
    let agent_sink: Arc<dyn StreamSink> = bcast.clone();
    let mut agent = make_agent_with_sink(
        vec![
            "```bash\necho hello-from-sse\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nbye\n```".into(),
        ],
        agent_sink,
    );
    let agent_handle = tokio::spawn(async move {
        let exit = agent.run().await.unwrap();
        assert!(matches!(exit, ExitReason::Submitted { .. }));
    });

    // Read until we see the bash_result event with our stdout.
    let mut acc = String::new();
    let mut buf = vec![0u8; 4096];
    let read_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !acc.contains("hello-from-sse") || !acc.contains("event: run_ended") {
        assert!(
            tokio::time::Instant::now() <= read_deadline,
            "timed out waiting for events; got:\n{acc}"
        );
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        if n == 0 {
            break;
        }
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }

    assert!(acc.contains("HTTP/1.1 200 OK"));
    assert!(acc.contains("text/event-stream"));
    assert!(acc.contains("event: run_started"));
    assert!(acc.contains("event: bash_start"));
    assert!(acc.contains("event: bash_result"));
    assert!(acc.contains("\"command\":\"echo hello-from-sse\""));
    assert!(acc.contains("event: run_ended"));

    agent_handle.await.unwrap();
    drop(client);
    server.shutdown().await;
}

#[tokio::test]
async fn agent_survives_client_disconnect_mid_run() {
    let bcast = Arc::new(BroadcastSink::default());
    let server = SseServer::start("127.0.0.1:0".parse().unwrap(), bcast.clone())
        .await
        .unwrap();
    let addr = server.local_addr();

    // Subscribe, then immediately drop the connection partway through.
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while bcast.receiver_count() < 1 {
        assert!(
            tokio::time::Instant::now() <= deadline,
            "client never attached"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A multi-step run gives the disconnect a chance to land mid-loop.
    let agent_sink: Arc<dyn StreamSink> = bcast.clone();
    let mut agent = make_agent_with_sink(
        vec![
            "```bash\necho one\n```".into(),
            "```bash\necho two\n```".into(),
            "```bash\necho three\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
        ],
        agent_sink,
    );

    // Read just a little, then drop the client.
    let mut buf = vec![0u8; 1024];
    let _ = tokio::time::timeout(Duration::from_millis(200), client.read(&mut buf)).await;
    drop(client);

    // Agent must complete normally despite the dropped subscriber.
    let exit = agent.run().await.unwrap();
    match exit {
        ExitReason::Submitted { final_output } => assert_eq!(final_output, "done"),
        other => panic!("unexpected exit: {other:?}"),
    }

    server.shutdown().await;
}
