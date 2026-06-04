//! OTLP metrics export — integration tests (issue #493).

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::await_holding_lock,
    clippy::float_cmp,
    clippy::match_same_arms
)]

use std::fmt::Write as _;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use maxwells_daemon::run::swebench::{SwebenchArgs, run};

mod support;

/// Serialize tests that mutate environment variables.
static ENV_VAR_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
fn env_var_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_VAR_MUTEX.get_or_init(|| Mutex::new(())).lock().unwrap()
}

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

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn swebench_args_has_otlp_metrics_interval_secs_field() {
    let work = tempfile::tempdir().unwrap();
    let args = SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(
            work.path().join("dataset.jsonl"),
        ),
        dataset_cache_dir: work.path().to_path_buf(),
        output_dir: work.path().join("out"),
        parallel: 1,
        config: maxwells_daemon::Config::defaults().unwrap(),
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
        deterministic_responses: None,
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
        otlp_endpoint: None,
        otlp_metrics_interval_secs: Some(42),
        rehearse: false,
        skip_evaluator: false,
        eval_backend: "rehearsal".to_string(),
        sb_subset: None,
        sb_split: None,
        eval_timeout_secs: None,
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    };
    assert_eq!(args.otlp_metrics_interval_secs, Some(42));
}

#[tokio::test]
async fn no_otlp_metrics_traffic_when_endpoint_unset() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__A__1"]);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();

    let cfg = config_with_workdir(&repo);
    let args = SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: work.path().to_path_buf(),
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
        otlp_endpoint: None,
        otlp_metrics_interval_secs: None,
        rehearse: false,
        skip_evaluator: false,
        eval_backend: "rehearsal".to_string(),
        sb_subset: None,
        sb_split: None,
        eval_timeout_secs: None,
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    };

    let _guard = env_var_lock();
    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT");
    }

    let _results = run(args).await.unwrap();

    match listener.accept() {
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("OTLP metrics socket received connection when endpoint unset"),
        Err(e) => panic!("listener error: {e}"),
    }
}

#[tokio::test]
async fn otlp_metrics_periodic_and_final_flush() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__B__1"]);

    // Start a simple mock TCP server to capture OTLP metrics payloads.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint = format!("http://{addr}");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let server_handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut read_bytes = 0;
                loop {
                    match stream.read(&mut buf[read_bytes..]).await {
                        Ok(0) => break,
                        Ok(n) => {
                            read_bytes += n;
                            if let Some(pos) = find_subsequence(&buf[..read_bytes], b"\r\n\r\n") {
                                let headers_part = String::from_utf8_lossy(&buf[..pos]);
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
                                    let _ = tx.send((path, body));
                                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                                    let _ = stream.write_all(response.as_bytes()).await;
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    let cfg = config_with_workdir(&repo);
    let args = SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: work.path().to_path_buf(),
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
        otlp_endpoint: Some(endpoint),
        otlp_metrics_interval_secs: Some(1), // 1 second periodic export
        rehearse: false,
        skip_evaluator: false,
        eval_backend: "rehearsal".to_string(),
        sb_subset: None,
        sb_split: None,
        eval_timeout_secs: None,
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    };

    let _guard = env_var_lock();
    unsafe {
        // Enforce a very small metric interval via env var to capture periodic metrics!
        std::env::set_var("OTEL_METRIC_EXPORT_INTERVAL", "100");
    }

    let _results = run(args).await.unwrap();

    unsafe {
        std::env::remove_var("OTEL_METRIC_EXPORT_INTERVAL");
    }

    // Stop mock server
    server_handle.abort();

    // Collect all captured payloads
    let mut payloads = vec![];
    while let Ok(payload) = rx.try_recv() {
        payloads.push(payload);
    }

    // Assert we received metrics requests
    let metrics_payloads: Vec<_> = payloads
        .into_iter()
        .filter(|(path, _)| path == "/v1/metrics")
        .map(|(_, body)| body)
        .collect();

    assert!(
        !metrics_payloads.is_empty(),
        "No OTLP metrics were exported!"
    );

    // Parse the final payload (which should be the final flush)
    let final_body = metrics_payloads.last().unwrap();
    let final_val: serde_json::Value = serde_json::from_str(final_body).unwrap();

    // Verify final state counts in gauges
    let resource_metrics = final_val["resourceMetrics"].as_array().unwrap();
    assert_eq!(resource_metrics.len(), 1);

    let resource = &resource_metrics[0]["resource"];
    let resource_attrs = resource["attributes"].as_array().unwrap();
    let get_res_attr = |key: &str| {
        resource_attrs
            .iter()
            .find(|a| a["key"].as_str() == Some(key))
            .and_then(|a| a["value"]["stringValue"].as_str())
    };

    assert!(get_res_attr("sweep_id").is_some());
    assert!(get_res_attr("dataset").unwrap().contains("dataset.jsonl"));

    let scope_metrics = resource_metrics[0]["scopeMetrics"].as_array().unwrap();
    let metrics = scope_metrics[0]["metrics"].as_array().unwrap();

    let get_gauge_int = |name: &str| {
        metrics
            .iter()
            .find(|m| m["name"].as_str() == Some(name))
            .and_then(|m| m["gauge"]["dataPoints"].as_array())
            .and_then(|dp| dp.first())
            .and_then(|dp| dp["asInt"].as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap()
    };

    let get_gauge_double = |name: &str| {
        metrics
            .iter()
            .find(|m| m["name"].as_str() == Some(name))
            .and_then(|m| m["gauge"]["dataPoints"].as_array())
            .and_then(|dp| dp.first())
            .and_then(|dp| dp["asDouble"].as_f64())
            .unwrap()
    };

    assert_eq!(get_gauge_int("instances_total"), 1);
    assert_eq!(get_gauge_int("instances_completed"), 1);
    assert_eq!(get_gauge_int("instances_resolved"), 1);
    assert_eq!(get_gauge_int("instances_failed"), 0);
    assert_eq!(get_gauge_int("instances_in_flight"), 0);
    assert_eq!(get_gauge_double("resolved_rate"), 1.0);
    assert_eq!(get_gauge_double("error_rate"), 0.0);
}
