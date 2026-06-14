//! `bench export-otlp`: backfill OTLP traces from a completed sweep (issue #513).
//!
//! Red/green/refactor TDD coverage. These tests exercise the library entry
//! points (`reconstruct` / `run` / `render_text`) and the CLI surface
//! (`max bench export-otlp`), including endpoint precedence, the `--dry-run`
//! path, the exit-code contract, and the success-metric determinism guarantee:
//! a run exported live and the same run backfilled via `export-otlp` must emit
//! byte-identical trace/span IDs.

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::await_holding_lock,
    clippy::large_futures
)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use maxwells_daemon::run::export_otlp::{ExportOtlpArgs, reconstruct, render_text, run};
use maxwells_daemon::run::swebench::{SwebenchArgs, compute_sweep_id};

mod support;
use support::binary_path;

/// Serialize tests that mutate OTEL env vars — env vars are process-global.
static ENV_VAR_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
fn env_var_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_VAR_MUTEX.get_or_init(|| Mutex::new(())).lock().unwrap()
}

// ── shared scaffolding (mirrors tests/otlp_traces.rs) ───────────────────────

fn write_dataset(path: &Path, instance_ids: &[&str]) {
    let mut s = String::new();
    for id in instance_ids {
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\"}}"
        );
    }
    std::fs::write(path, s).unwrap();
}

fn init_repo(dir: &Path) {
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@test"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
}

fn config_with_workdir(dir: &Path) -> maxwells_daemon::Config {
    let workdir = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let toml = format!("[environment]\nworkdir = \"{workdir}\"\n");
    maxwells_daemon::Config::from_toml_str(&toml).unwrap()
}

fn swebench_args(
    dataset: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    output: std::path::PathBuf,
    cfg: maxwells_daemon::Config,
    otlp_endpoint: Option<String>,
) -> SwebenchArgs {
    SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: cache_dir,
        output_dir: output,
        parallel: 1,
        config: cfg,
        reruns: 1,
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: maxwells_daemon::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 1000,
        retry_backoff_cap_s: 60,
        retry_on_resume: false,
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ntest\n```".into(),
        ]),
        deterministic_usage_per_call: None,
        config_overlay_paths: vec![],
        dry_run: false,
        skip_preflight: true,
        preflight_format: "text".into(),
        skip_model_probe: true,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "swebench".into(),
        skip_patch_validation: true,
        event_log: None,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: 30,
        install_os_signal_handlers: false,
        cancellation_signals: None,
        github_pr: None,
        reproduced_from: None,
        abort_on_systemic_failure: false,
        systemic_failure_min_samples: 5,
        systemic_failure_share_pct: 80,
        otlp_endpoint,
        otlp_metrics_interval_secs: None,
        rehearse: false,
        skip_evaluator: false,
        eval_backend: "rehearsal".to_string(),
        sb_subset: None,
        sb_split: None,
        eval_timeout_secs: None,
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A mock OTLP/HTTP collector that records every request body and replies 200.
async fn start_mock_collector() -> (
    String,
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::UnboundedReceiver<(String, String, String)>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint = format!("http://{addr}");
    // (path, headers, body)
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(String, String, String)>();
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let mut read_bytes = 0;
                loop {
                    match stream.read(&mut buf[read_bytes..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            read_bytes += n;
                            if let Some(pos) = find_subsequence(&buf[..read_bytes], b"\r\n\r\n") {
                                let headers_part = String::from_utf8_lossy(&buf[..pos]).to_string();
                                let mut content_len = 0;
                                for line in headers_part.lines() {
                                    if line.to_lowercase().starts_with("content-length:") {
                                        if let Some(val) =
                                            line.split_once(':').map(|(_, v)| v.trim())
                                        {
                                            content_len = val.parse::<usize>().unwrap_or(0);
                                        }
                                    }
                                }
                                let body_start = pos + 4;
                                if read_bytes >= body_start + content_len {
                                    let body = String::from_utf8_lossy(
                                        &buf[body_start..body_start + content_len],
                                    )
                                    .to_string();
                                    let path = headers_part
                                        .lines()
                                        .next()
                                        .and_then(|l| l.split_whitespace().nth(1))
                                        .unwrap_or("")
                                        .to_string();
                                    let _ = tx.send((path, headers_part, body));
                                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
                                    let _ = stream.write_all(response.as_bytes()).await;
                                    break;
                                }
                            }
                        }
                    }
                }
            });
        }
    });
    (endpoint, handle, rx)
}

/// Extract the set of `(traceId, spanId)` pairs from an OTLP/JSON trace body.
fn id_pairs(body: &str) -> BTreeSet<(String, String)> {
    let val: serde_json::Value = serde_json::from_str(body).unwrap();
    let mut out = BTreeSet::new();
    for rs in val["resourceSpans"].as_array().into_iter().flatten() {
        for ss in rs["scopeSpans"].as_array().into_iter().flatten() {
            for span in ss["spans"].as_array().into_iter().flatten() {
                let tid = span["traceId"].as_str().unwrap_or("").to_string();
                let sid = span["spanId"].as_str().unwrap_or("").to_string();
                out.insert((tid, sid));
            }
        }
    }
    out
}

/// Run a tiny single-instance sweep into `output`, emitting live spans to
/// `live_endpoint`, and return the live-emitted OTLP body.
async fn run_live_sweep(work: &Path, output: &Path, live_endpoint: String) -> String {
    let repo = work.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.join("dataset.jsonl");
    std::fs::create_dir_all(output).unwrap();
    write_dataset(&dataset, &["repo__DET__1"]);

    let (endpoint, _handle, mut rx) = start_mock_collector().await;
    let _ = live_endpoint; // mock endpoint chosen here
    let cfg = config_with_workdir(&repo);
    let args = swebench_args(
        dataset,
        work.to_path_buf(),
        output.to_path_buf(),
        cfg,
        Some(endpoint),
    );
    let _ = run_swebench_and_drain(args, &mut rx).await;
    // Drain the captured live body (traces only).
    drain_traces(&mut rx).await
}

async fn run_swebench_and_drain(
    args: SwebenchArgs,
    _rx: &mut tokio::sync::mpsc::UnboundedReceiver<(String, String, String)>,
) -> maxwells_daemon::run::swebench::SweepResults {
    maxwells_daemon::run::swebench::run(args).await.unwrap()
}

async fn drain_traces(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(String, String, String)>,
) -> String {
    // The traces exporter posts exactly one body to /v1/traces at sweep end.
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            Ok(Some((path, _hdr, body))) if path.ends_with("/v1/traces") => return body,
            Ok(Some(_)) => {}
            _ => panic!("did not receive a /v1/traces export"),
        }
    }
}

// ── success-metric determinism test ─────────────────────────────────────────

#[tokio::test]
async fn live_and_backfilled_exports_have_identical_ids() {
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");

    // 1. Run the sweep live, capturing the live OTLP body.
    let live_body = run_live_sweep(work.path(), &output, String::new()).await;
    let live_ids = id_pairs(&live_body);
    assert!(!live_ids.is_empty(), "live export emitted no spans");

    // 2. Backfill the same sweep dir via export-otlp into a fresh collector.
    let (endpoint, _handle, mut rx) = start_mock_collector().await;
    let summary = run(&ExportOtlpArgs {
        sweep_dir: output.clone(),
        otlp_endpoint: Some(endpoint),
        dry_run: false,
    })
    .await
    .unwrap();
    assert_eq!(summary.instance_count, 1);
    assert!(summary.span_count >= 2, "expected sweep + instance spans");

    let export_body = drain_traces(&mut rx).await;
    let export_ids = id_pairs(&export_body);

    assert_eq!(
        live_ids, export_ids,
        "re-exported run must emit identical trace/span IDs as the live export"
    );
}

// ── reconstruct() determinism (no network) ──────────────────────────────────

#[tokio::test]
async fn reconstruct_uses_stable_sweep_id() {
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");
    // Produce a real sweep dir with a dead endpoint so trace_ids are persisted.
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__REC__1"]);
    let cfg = config_with_workdir(&repo);
    let args = swebench_args(
        dataset,
        work.path().to_path_buf(),
        output.clone(),
        cfg,
        Some("http://127.0.0.1:1".into()),
    );
    maxwells_daemon::run::swebench::run(args).await.unwrap();

    let rec = reconstruct(&output).unwrap();
    // Recompute the sweep_id from the manifest's started_at_utc.
    let started = rec.started_at_utc.clone();
    let expected = compute_sweep_id(&output, &started);
    assert_eq!(rec.sweep_id, expected);
    assert_eq!(rec.instance_spans.len(), 1);
    // Instance trace id matches the persisted/recomputed value.
    let expected_tid = maxwells_daemon::telemetry::new_trace_id("repo__REC__1", &rec.sweep_id);
    assert_eq!(rec.instance_spans[0].trace_id, expected_tid);
}

// ── library-level exit/behavior tests ───────────────────────────────────────

#[tokio::test]
async fn dry_run_opens_no_socket_and_counts_spans() {
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__DRY__1"]);
    let cfg = config_with_workdir(&repo);
    let args = swebench_args(
        dataset,
        work.path().to_path_buf(),
        output.clone(),
        cfg,
        Some("http://127.0.0.1:1".into()),
    );
    maxwells_daemon::run::swebench::run(args).await.unwrap();

    // Bind a listener we expect to receive ZERO connections during dry-run.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();

    let summary = run(&ExportOtlpArgs {
        sweep_dir: output,
        otlp_endpoint: Some(format!("http://{addr}")),
        dry_run: true,
    })
    .await
    .unwrap();

    assert!(summary.dry_run);
    assert_eq!(summary.instance_count, 1);
    assert!(summary.span_count >= 2);

    match listener.accept() {
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("dry-run opened a socket to the collector"),
        Err(e) => panic!("unexpected listener error: {e}"),
    }

    let text = render_text(&summary);
    assert!(text.to_lowercase().contains("dry"));
    assert!(text.contains('1'));
}

#[tokio::test]
async fn successful_export_reports_counts() {
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__OK__1"]);
    let cfg = config_with_workdir(&repo);
    let args = swebench_args(
        dataset,
        work.path().to_path_buf(),
        output.clone(),
        cfg,
        Some("http://127.0.0.1:1".into()),
    );
    maxwells_daemon::run::swebench::run(args).await.unwrap();

    let (endpoint, _handle, mut rx) = start_mock_collector().await;
    let summary = run(&ExportOtlpArgs {
        sweep_dir: output,
        otlp_endpoint: Some(endpoint),
        dry_run: false,
    })
    .await
    .unwrap();
    assert_eq!(summary.instance_count, 1);
    assert!(summary.span_count >= 2);
    // Collector received exactly one /v1/traces body.
    let body = drain_traces(&mut rx).await;
    assert!(!id_pairs(&body).is_empty());
}

#[tokio::test]
async fn unreachable_collector_is_preflight_error() {
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__DOWN__1"]);
    let cfg = config_with_workdir(&repo);
    let args = swebench_args(
        dataset,
        work.path().to_path_buf(),
        output.clone(),
        cfg,
        Some("http://127.0.0.1:1".into()),
    );
    maxwells_daemon::run::swebench::run(args).await.unwrap();

    let err = run(&ExportOtlpArgs {
        sweep_dir: output,
        otlp_endpoint: Some("http://127.0.0.1:1".into()),
        dry_run: false,
    })
    .await
    .unwrap_err();
    assert_eq!(
        maxwells_daemon::ExitCode::from_error(&err),
        maxwells_daemon::ExitCode::PreflightFailure
    );
}

#[tokio::test]
async fn missing_endpoint_is_usage_error() {
    let _guard = env_var_lock();
    // SAFETY: serialized by env_var_lock.
    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
    }
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__NOEP__1"]);
    let cfg = config_with_workdir(&repo);
    let args = swebench_args(
        dataset,
        work.path().to_path_buf(),
        output.clone(),
        cfg,
        Some("http://127.0.0.1:1".into()),
    );
    maxwells_daemon::run::swebench::run(args).await.unwrap();

    let err = run(&ExportOtlpArgs {
        sweep_dir: output,
        otlp_endpoint: None,
        dry_run: false,
    })
    .await
    .unwrap_err();
    assert_eq!(
        maxwells_daemon::ExitCode::from_error(&err),
        maxwells_daemon::ExitCode::UsageError
    );
}

#[tokio::test]
async fn missing_sweep_dir_is_usage_error() {
    let work = tempfile::tempdir().unwrap();
    let err = run(&ExportOtlpArgs {
        sweep_dir: work.path().join("does-not-exist"),
        otlp_endpoint: Some("http://127.0.0.1:4318".into()),
        dry_run: false,
    })
    .await
    .unwrap_err();
    assert_eq!(
        maxwells_daemon::ExitCode::from_error(&err),
        maxwells_daemon::ExitCode::UsageError
    );
}

#[tokio::test]
async fn endpoint_flag_beats_env() {
    let _guard = env_var_lock();
    // SAFETY: serialized by env_var_lock.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://env-host:4318");
        std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
    }
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__PREC__1"]);
    let cfg = config_with_workdir(&repo);
    let args = swebench_args(
        dataset,
        work.path().to_path_buf(),
        output.clone(),
        cfg,
        Some("http://127.0.0.1:1".into()),
    );
    maxwells_daemon::run::swebench::run(args).await.unwrap();

    let (endpoint, _handle, mut rx) = start_mock_collector().await;
    let summary = run(&ExportOtlpArgs {
        sweep_dir: output,
        otlp_endpoint: Some(endpoint.clone()),
        dry_run: false,
    })
    .await
    .unwrap();
    // The flag endpoint must have been used, not the env var.
    assert_eq!(
        summary.endpoint.as_deref(),
        Some(format!("{endpoint}/v1/traces").as_str())
    );
    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
    }
    let _ = drain_traces(&mut rx).await;
}

#[tokio::test]
async fn otlp_headers_from_env_are_sent() {
    let _guard = env_var_lock();
    // SAFETY: serialized by env_var_lock.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", "x-export-otlp-test=tok123");
        std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_HEADERS");
    }
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__HDR__1"]);
    let cfg = config_with_workdir(&repo);
    let args = swebench_args(
        dataset,
        work.path().to_path_buf(),
        output.clone(),
        cfg,
        Some("http://127.0.0.1:1".into()),
    );
    maxwells_daemon::run::swebench::run(args).await.unwrap();

    let (endpoint, _handle, mut rx) = start_mock_collector().await;
    run(&ExportOtlpArgs {
        sweep_dir: output,
        otlp_endpoint: Some(endpoint),
        dry_run: false,
    })
    .await
    .unwrap();

    // Collect the headers of the /v1/traces request.
    let headers = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            Ok(Some((path, hdr, _body))) if path.ends_with("/v1/traces") => break hdr,
            Ok(Some(_)) => {}
            _ => panic!("did not receive a /v1/traces export"),
        }
    };
    // SAFETY: serialized by env_var_lock.
    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_HEADERS");
    }
    assert!(
        headers
            .to_lowercase()
            .contains("x-export-otlp-test: tok123"),
        "expected auth header forwarded to collector; got:\n{headers}"
    );
}

// ── CLI-level exit-code tests ───────────────────────────────────────────────

#[test]
fn cli_missing_sweep_dir_exits_2() {
    let work = tempfile::tempdir().unwrap();
    let out = Command::new(binary_path())
        .args([
            "bench",
            "export-otlp",
            "--sweep",
            work.path().join("nope").to_str().unwrap(),
            "--otlp-endpoint",
            "http://127.0.0.1:4318",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "expected usage_error (2)");
}

#[test]
fn cli_help_lists_command() {
    let out = Command::new(binary_path())
        .args(["bench", "export-otlp", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--sweep"));
    assert!(help.contains("--otlp-endpoint"));
    assert!(help.contains("--dry-run"));
}
