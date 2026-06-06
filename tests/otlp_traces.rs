//! OTLP trace export — TDD test suite (issue #310).
//!
//! RED → all tests in this file should fail until the implementation is complete.

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::await_holding_lock
)]

use std::fmt::Write as _;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Serialize tests that mutate `OTEL_EXPORTER_OTLP_ENDPOINT` — env vars are
/// process-global, so concurrent tests would race on them.
static ENV_VAR_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
fn env_var_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_VAR_MUTEX.get_or_init(|| Mutex::new(())).lock().unwrap()
}

use maxwells_daemon::run::swebench::{InstanceResult, SwebenchArgs, SweepResults, run};
use maxwells_daemon::trajectory::{Trajectory, TrajectoryInfo};

mod support;
use support::binary_path;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// AC: `SwebenchArgs` exposes `otlp_endpoint`
// ---------------------------------------------------------------------------

#[test]
fn swebench_args_has_otlp_endpoint_field() {
    // Verify the field exists by constructing and reading it.
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
    assert!(args.otlp_endpoint.is_none());
}

// ---------------------------------------------------------------------------
// AC: `SweepResults` has `span_export_dropped` counter
// ---------------------------------------------------------------------------

#[test]
fn sweep_results_has_span_export_dropped() {
    // Deserialize a minimal SweepResults to verify the field exists and
    // defaults to 0 when absent (additive-minor compatible).
    let json = r#"{"total":0,"submitted":0,"skipped":0,"errored":0,"instances":[]}"#;
    let results: SweepResults = serde_json::from_str(json).unwrap();
    let _: u64 = results.span_export_dropped;
    assert_eq!(results.span_export_dropped, 0);
}

// ---------------------------------------------------------------------------
// AC: `InstanceResult` has `trace_id`
// ---------------------------------------------------------------------------

#[test]
fn instance_result_has_trace_id() {
    let ir = InstanceResult {
        instance_id: "test__repo__1".into(),
        exit_reason: "submitted".into(),
        outcome: None,
        failure_category: None,
        steps: None,
        cost_usd: None,
        prompt_tokens: None,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        completion_tokens: None,
        duration_secs: None,
        error: None,
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: vec![],
        runs: 1,
        resolved_count: 0,
        pass_at_1: false,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
        trace_id: Some("deadbeef00000000deadbeef00000000".into()),
    };
    assert_eq!(
        ir.trace_id.as_deref(),
        Some("deadbeef00000000deadbeef00000000")
    );
}

// ---------------------------------------------------------------------------
// AC: `TrajectoryInfo` has `trace_id`
// ---------------------------------------------------------------------------

#[test]
fn trajectory_info_has_trace_id() {
    let info = TrajectoryInfo {
        trace_id: Some("aabbccdd00000000aabbccdd00000000".into()),
        ..Default::default()
    };
    assert_eq!(
        info.trace_id.as_deref(),
        Some("aabbccdd00000000aabbccdd00000000")
    );
}

// ---------------------------------------------------------------------------
// AC: Trajectory serialisation round-trips `trace_id`
// ---------------------------------------------------------------------------

#[test]
fn trajectory_trace_id_serialises_and_deserialises() {
    let mut traj = Trajectory::new();
    traj.info.trace_id = Some("cafebabe00000000cafebabe00000000".into());
    let json = serde_json::to_string(&traj).unwrap();
    let decoded: Trajectory = serde_json::from_str(&json).unwrap();
    assert_eq!(
        decoded.info.trace_id.as_deref(),
        Some("cafebabe00000000cafebabe00000000")
    );
}

// ---------------------------------------------------------------------------
// AC: When OTLP disabled, the sweep produces ZERO OTel network traffic.
//
// Strategy: bind a TCP server on a random port, run a tiny 1-instance sweep
// with `otlp_endpoint = None` AND without `OTEL_EXPORTER_OTLP_ENDPOINT`.
// After the sweep completes assert the server received zero connections.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_otlp_traffic_when_endpoint_unset() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__A__1"]);

    // Bind a TCP server that we'll watch for connections.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let otlp_lookalike = format!("http://{addr}");
    // We do NOT pass this as the endpoint — but we keep the listener alive
    // to detect any accidental connection.
    let _ = otlp_lookalike; // suppress unused warning

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
        eval_timeout_secs: None, // <-- OTLP disabled
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    };

    // Serialize against other tests that mutate OTEL_EXPORTER_OTLP_ENDPOINT.
    let _guard = env_var_lock();
    // Also ensure both OTLP env vars are unset.
    // SAFETY: test-only, single-threaded context.
    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
    }

    let _results = run(args).await.unwrap();

    // The TCP listener must NOT have received any connections.
    match listener.accept() {
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // Good — no connection was made.
        }
        Ok(_) => panic!("OTLP socket received a connection even though endpoint was not set"),
        Err(e) => panic!("unexpected listener error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// AC: When sweep runs with OTLP endpoint, `trace_id` is written to every
// instance result and trajectory.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trace_id_written_to_instance_result_and_trajectory() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__B__1"]);

    // Use a dead endpoint — export failures must be tolerated, not fatal.
    let cfg = config_with_workdir(&repo);
    let args = SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: work.path().to_path_buf(),
        output_dir: output.clone(),
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
        // Dead endpoint: export will fail, but must not crash.
        otlp_endpoint: Some("http://127.0.0.1:1".into()),
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

    let results = run(args).await.unwrap();

    // Every instance result must have a trace_id.
    for inst in &results.instances {
        assert!(
            inst.trace_id.is_some(),
            "instance {} missing trace_id",
            inst.instance_id
        );
        let tid = inst.trace_id.as_deref().unwrap();
        assert_eq!(tid.len(), 32, "trace_id must be 32 hex chars, got: {tid}");
        assert!(
            tid.chars().all(|c| c.is_ascii_hexdigit()),
            "trace_id must be hex: {tid}"
        );
    }

    // The on-disk trajectory must also carry the trace_id.
    // reruns=1 → run_index loops 1..=1 → file is run-1.traj.json
    let traj_path = output.join("repo__B__1").join("run-1.traj.json");
    let traj_json = std::fs::read_to_string(&traj_path)
        .unwrap_or_else(|_| panic!("trajectory not found at {}", traj_path.display()));
    let traj: Trajectory = serde_json::from_str(&traj_json).unwrap();
    assert!(
        traj.info.trace_id.is_some(),
        "trace_id missing from trajectory"
    );
    assert_eq!(
        traj.info.trace_id, results.instances[0].trace_id,
        "trajectory trace_id must match instance_result trace_id"
    );
}

// ---------------------------------------------------------------------------
// AC: Export failures must not fail the sweep; `span_export_dropped` is
// incremented in `results.json`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_failure_does_not_fail_sweep_and_increments_counter() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__C__1"]);

    // Port 1 is reserved and will always refuse connections.
    let cfg = config_with_workdir(&repo);
    let args = SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: work.path().to_path_buf(),
        output_dir: output.clone(),
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
        otlp_endpoint: Some("http://127.0.0.1:1".into()),
        otlp_metrics_interval_secs: None,
        rehearse: false,
        skip_evaluator: false,
        eval_backend: "rehearsal".to_string(),
        sb_subset: None,
        sb_split: None,
        eval_timeout_secs: None, // dead endpoint
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    };

    // Must not panic or return Err.
    let results = run(args).await.unwrap();
    assert_eq!(results.submitted, 1, "instance must still succeed");

    // `span_export_dropped` must be > 0 because the exporter will fail.
    assert!(
        results.span_export_dropped > 0,
        "expected span_export_dropped > 0, got {}",
        results.span_export_dropped
    );

    // results.json on disk must carry the counter.
    let results_json = std::fs::read_to_string(output.join("results.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&results_json).unwrap();
    let dropped = parsed["span_export_dropped"].as_u64().unwrap_or(0);
    assert!(
        dropped > 0,
        "results.json span_export_dropped must be > 0, got {dropped}"
    );
}

// ---------------------------------------------------------------------------
// AC: `bench inspect --instance <id>` prints `trace_id` for that instance.
// ---------------------------------------------------------------------------

#[test]
fn bench_inspect_prints_trace_id_when_present() {
    let work = tempfile::tempdir().unwrap();

    // Write results.json (minimal).
    let results = serde_json::json!({
        "total": 1,
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "instances": [{
            "instance_id": "repo__D__1",
            "exit_reason": "submitted",
            "trace_id": "1234567890abcdef1234567890abcdef"
        }]
    });
    std::fs::write(
        work.path().join("results.json"),
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    // Write a matching trajectory.
    let mut traj = Trajectory::new();
    traj.info.trace_id = Some("1234567890abcdef1234567890abcdef".into());
    traj.info.outcome = Some("submitted".into());
    let inst_dir = work.path().join("repo__D__1");
    std::fs::create_dir_all(&inst_dir).unwrap();
    // resolve_trajectory_path looks for run-1.traj.json first.
    std::fs::write(
        inst_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            work.path().to_str().unwrap(),
            "--instance",
            "repo__D__1",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1234567890abcdef1234567890abcdef"),
        "bench inspect output must include trace_id; got:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// AC: `--otlp-endpoint` CLI flag exists in `bench swebench --help`.
// ---------------------------------------------------------------------------

#[test]
fn swebench_cli_has_otlp_endpoint_flag() {
    let out = Command::new(binary_path())
        .args(["bench", "swebench", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("otlp-endpoint"),
        "`bench swebench --help` must list --otlp-endpoint; got:\n{help}"
    );
}

// ---------------------------------------------------------------------------
// AC: `OTEL_EXPORTER_OTLP_ENDPOINT` env var activates tracing.
//     When set, trace_id is populated even though CLI flag is absent.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn env_var_activates_otlp_tracing() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, &["repo__E__1"]);

    // Serialize against other tests that mutate OTEL_EXPORTER_OTLP_ENDPOINT.
    let _guard = env_var_lock();
    // Set env var to a dead endpoint.
    // SAFETY: test-only, single-threaded context.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1");
    }

    let cfg = config_with_workdir(&repo);
    let args = SwebenchArgs {
        dataset_source: maxwells_daemon::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: work.path().to_path_buf(),
        output_dir: output.clone(),
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
        eval_timeout_secs: None, // env var activates instead of CLI flag
        notify_webhook_url: None,
        notify_webhook_headers: vec![],
    };

    let results = run(args).await.unwrap();

    // Unset for subsequent tests.
    // SAFETY: test-only, single-threaded context.
    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
    }

    for inst in &results.instances {
        assert!(
            inst.trace_id.is_some(),
            "instance {} must have trace_id when env var is set",
            inst.instance_id
        );
    }
}
