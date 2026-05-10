#![cfg(not(windows))]
#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use rust_swe_agent::Config;
use rust_swe_agent::run::swebench::{SwebenchArgs, predictions_path, run, trajectory_path_for};
use rust_swe_agent::trajectory::{FailureCategory, Trajectory, outcome};

fn write_dataset(path: &Path) {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{{\"instance_id\":\"slow\",\"problem_statement\":\"sleep longer than the wallclock timeout\"}}"
    );
    std::fs::write(path, s).unwrap();
}

fn long_running_response() -> String {
    let command = if cfg!(windows) {
        "for /L %i in (1,1,2147483647) do @rem"
    } else {
        "trap '' TERM; while :; do sleep 1; done"
    };
    format!("```bash\n{command}\n```")
}

#[tokio::test]
async fn sweep_wallclock_timeout_finalizes_trajectory_and_reclaims_worker() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    write_dataset(&dataset);

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 10;
    cfg.root.environment.timeout_secs = 60;

    let started = Instant::now();
    let results = run(SwebenchArgs {
        dataset_source: rust_swe_agent::run::dataset::DatasetSource::LocalPath(dataset),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: Some(2),
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: rust_swe_agent::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(vec![long_running_response()]),
        deterministic_usage_per_call: None,
        config_overlay_paths: Vec::new(),
        dry_run: false,
        skip_preflight: true,
        preflight_format: "text".into(),
        skip_model_probe: true,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "test".into(),
        skip_patch_validation: true,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: 30,
        install_os_signal_handlers: false,
        cancellation_signals: None,
        github_pr: None,
        reproduced_from: None,
    })
    .await
    .unwrap();
    let elapsed = started.elapsed();

    let max_elapsed = if cfg!(windows) {
        Duration::from_secs(6)
    } else {
        Duration::from_secs(5)
    };
    assert!(
        elapsed < max_elapsed,
        "worker should return promptly after the 2s deadline; elapsed={elapsed:?}, max={max_elapsed:?}"
    );
    assert_eq!(results.total, 1);
    assert_eq!(results.submitted, 0);
    assert_eq!(results.errored, 1);
    assert_eq!(
        results
            .failures_by_category
            .get(&FailureCategory::WallclockTimeout),
        Some(&1)
    );
    assert_eq!(results.instances.len(), 1);
    let row = &results.instances[0];
    assert_eq!(row.instance_id, "slow");
    assert_eq!(row.exit_reason, "wallclock_timeout");
    assert_eq!(row.outcome.as_deref(), Some(outcome::ERROR));
    assert_eq!(
        row.failure_category,
        Some(FailureCategory::WallclockTimeout)
    );
    assert!(!row.patch_present);

    let traj_text = std::fs::read_to_string(trajectory_path_for(&output, "slow")).unwrap();
    let traj: Trajectory = serde_json::from_str(&traj_text).unwrap();
    assert_eq!(traj.info.exit_reason.as_deref(), Some("wallclock_timeout"));
    assert_eq!(traj.info.outcome.as_deref(), Some(outcome::ERROR));
    assert_eq!(
        traj.info.failure_category,
        Some(FailureCategory::WallclockTimeout)
    );
    assert!(traj.info.duration_secs.is_some());
    assert!(traj.info.ended_at.is_some());

    let preds = std::fs::read_to_string(predictions_path(&output)).unwrap();
    assert!(
        preds.trim().is_empty(),
        "wallclock timeouts must not emit predictions: {preds}"
    );
}
