//! End-to-end integration tests for the `max ui` web server (issue #319).
//!
//! These tests require the `ui-server` Cargo feature and are skipped when the
//! feature is not enabled.  Run with:
//!   cargo test --features ui-server --test ui_server

#![cfg(feature = "ui-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use maxwells_daemon::model::Message;
use maxwells_daemon::run::ui::{UiServer, discover_instances};
use maxwells_daemon::trajectory::{Trajectory, outcome};

mod support;

/// Write a minimal trajectory into `dir` for a given `instance_id`.
fn write_fixture_traj(dir: &Path, instance_id: &str, assistant_content: &str) {
    let mut t = Trajectory::new();
    t.info.outcome = Some(outcome::SUBMITTED.to_string());
    t.info.steps = Some(3);
    t.info.total_cost_usd = Some(0.12);
    t.info.duration_secs = Some(42.5);

    t.record_message(&Message::system("System prompt for test"));
    t.record_message(&Message::user("User message"));
    t.record_message(&Message::assistant(assistant_content));

    let json = serde_json::to_string_pretty(&t).unwrap();
    std::fs::write(dir.join(format!("{instance_id}.traj.json")), json).unwrap();
}

/// Bind a UI server against a sweep directory and return the server + base URL.
async fn start_test_server(sweep: &Path) -> (UiServer, String) {
    use std::sync::Arc;
    let instances = Arc::new(discover_instances(sweep).unwrap());
    let server = UiServer::start("127.0.0.1:0".parse().unwrap(), instances)
        .await
        .unwrap();
    let url = format!("http://127.0.0.1:{}", server.local_addr().port());
    (server, url)
}

// ── Index page ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn index_returns_200_html_with_instance_rows() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture_traj(dir.path(), "django__django-12345", "done");
    write_fixture_traj(dir.path(), "flask__flask-99", "done");

    let (server, url) = start_test_server(dir.path()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await.unwrap();

    assert_eq!(resp.status(), 200, "GET / should return 200");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/html"),
        "Content-Type should be text/html, got: {ct}"
    );

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("django__django-12345"),
        "index should list instance ID"
    );
    assert!(
        body.contains("flask__flask-99"),
        "index should list instance ID"
    );
    assert!(
        body.contains("submitted"),
        "index should show outcome column"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn index_rows_are_sorted_by_instance_id_ascending() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture_traj(dir.path(), "zzz-last", "done");
    write_fixture_traj(dir.path(), "aaa-first", "done");
    write_fixture_traj(dir.path(), "mmm-middle", "done");

    let (server, url) = start_test_server(dir.path()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let body = reqwest::get(&url).await.unwrap().text().await.unwrap();

    let pos_first = body.find("aaa-first").expect("aaa-first missing");
    let pos_middle = body.find("mmm-middle").expect("mmm-middle missing");
    let pos_last = body.find("zzz-last").expect("zzz-last missing");

    assert!(
        pos_first < pos_middle,
        "aaa-first must precede mmm-middle in HTML"
    );
    assert!(
        pos_middle < pos_last,
        "mmm-middle must precede zzz-last in HTML"
    );

    server.shutdown().await;
}

// ── Instance page ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn instance_page_returns_200_html() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture_traj(dir.path(), "test__instance-1", "Hello from agent");

    let (server, url) = start_test_server(dir.path()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/instance/test__instance-1"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "GET /instance/<id> should return 200");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/html"),
        "instance should be text/html, got: {ct}"
    );

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<!DOCTYPE html>"),
        "should be a full HTML page"
    );
    assert!(
        body.contains("Trajectory Export"),
        "should contain HtmlExporter title"
    );

    server.shutdown().await;
}

// ── 404 paths ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_path_returns_404() {
    let dir = tempfile::tempdir().unwrap();
    let (server, url) = start_test_server(dir.path()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let resp = reqwest::get(format!("{url}/admin")).await.unwrap();
    assert_eq!(resp.status(), 404, "unknown path should return 404");

    server.shutdown().await;
}

#[tokio::test]
async fn nonexistent_instance_returns_404() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture_traj(dir.path(), "test__instance-1", "done");

    let (server, url) = start_test_server(dir.path()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let resp = reqwest::get(format!("{url}/instance/does-not-exist"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "nonexistent instance should return 404");

    server.shutdown().await;
}

#[tokio::test]
async fn directory_traversal_attempt_returns_404() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture_traj(dir.path(), "test__instance-1", "done");

    let (server, url) = start_test_server(dir.path()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Path traversal attempt: /instance/../../etc/passwd
    // reqwest will encode this, but we verify the server rejects non-discovered IDs
    let resp = reqwest::get(format!("{url}/instance/%2E%2E%2Fetc%2Fpasswd"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "path traversal must return 404");

    server.shutdown().await;
}

// ── Health check ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn healthz_returns_200() {
    let dir = tempfile::tempdir().unwrap();
    let (server, url) = start_test_server(dir.path()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let resp = reqwest::get(format!("{url}/healthz")).await.unwrap();
    assert_eq!(resp.status(), 200, "/healthz should return 200");

    server.shutdown().await;
}

// ── Redaction integration test ────────────────────────────────────────────────

#[tokio::test]
async fn canary_secret_not_in_any_http_response() {
    // ghp_ prefix is auto-redacted by Redactor::default_enabled() as a
    // GitHub personal access token pattern.
    let canary = "ghp_CANARYTOKEN_ABC0123456789DEF0123456";

    let dir = tempfile::tempdir().unwrap();
    // Embed the canary in the assistant message content.
    write_fixture_traj(dir.path(), "secret-test", canary);

    let (server, url) = start_test_server(dir.path()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let client = reqwest::Client::new();

    // Index page should not contain the canary (it only shows metadata).
    let index_body = client.get(&url).send().await.unwrap().text().await.unwrap();
    assert!(
        !index_body.contains(canary),
        "index page must not contain canary secret"
    );

    // Instance page runs HtmlExporter which redacts via Redactor::default_enabled().
    let instance_body = client
        .get(format!("{url}/instance/secret-test"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !instance_body.contains(canary),
        "instance page must not leak canary secret; got excerpt:\n{:.300}",
        &instance_body
    );

    server.shutdown().await;
}
