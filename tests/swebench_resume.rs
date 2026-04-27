//! Resume behavior for the `bench swebench` sweep:
//!   * pre-existing valid trajectory → task is skipped (no agent runs)
//!   * pre-existing invalid trajectory → file is treated as absent and the
//!     task re-runs, producing a fresh, valid trajectory
//!   * without `--resume`, valid pre-existing files are *not* skipped

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;

use rust_swe_agent::Config;
use rust_swe_agent::run::swebench::{SwebenchArgs, run};
use rust_swe_agent::trajectory::{FORMAT_VERSION, Trajectory, TrajectoryInfo, outcome};

fn write_dataset(path: &Path, instance_ids: &[&str]) {
    let mut s = String::new();
    for id in instance_ids {
        // A no-op problem statement is fine — the deterministic model never
        // reads it; only the instance_id matters for trajectory naming.
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\"}}"
        );
    }
    std::fs::write(path, s).unwrap();
}

fn write_valid_trajectory(path: &Path, marker: &str) {
    let mut info = TrajectoryInfo {
        outcome: Some(outcome::SUBMITTED.into()),
        exit_reason: Some("submitted".into()),
        steps: Some(0),
        ..Default::default()
    };
    info.other.insert(
        "test_marker".into(),
        serde_json::Value::String(marker.into()),
    );
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info,
        messages: vec![],
    };
    std::fs::write(path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();
}

fn submit_only_responses() -> Vec<String> {
    // First model turn already submits — no shell action required, so the
    // local environment is never invoked beyond template setup.
    vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfresh-run\n```".into()]
}

#[tokio::test]
async fn resume_skips_valid_trajectory_and_reruns_invalid() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["pre-valid", "pre-invalid", "fresh"]);

    // Instance 1: pre-existing *valid* trajectory → must be skipped.
    let valid_path = output.join("pre-valid.traj.json");
    write_valid_trajectory(&valid_path, "preserved");
    let valid_before = std::fs::read(&valid_path).unwrap();

    // Instance 2: pre-existing *invalid* trajectory → treated as absent.
    let invalid_path = output.join("pre-invalid.traj.json");
    std::fs::write(&invalid_path, "{\"trajectory_format\":\"mini-swe-").unwrap();

    // Instance 3: no pre-existing file → fresh run.

    let cfg = Config::defaults().unwrap();
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 2,
        config: cfg,
        resume: true,
        deterministic_responses: Some(submit_only_responses()),
    })
    .await
    .unwrap();

    assert_eq!(results.total, 3);
    assert_eq!(results.skipped, 1, "exactly one task should be skipped");

    // Skipped instance: trajectory file is byte-identical to what we wrote.
    let valid_after = std::fs::read(&valid_path).unwrap();
    assert_eq!(
        valid_before, valid_after,
        "skipped trajectory should be untouched"
    );

    // Invalid pre-existing trajectory was overwritten with a fresh, parseable one.
    let reread: Trajectory =
        serde_json::from_str(&std::fs::read_to_string(&invalid_path).unwrap()).unwrap();
    assert_eq!(reread.trajectory_format, FORMAT_VERSION);
    assert_eq!(reread.info.outcome.as_deref(), Some(outcome::SUBMITTED));

    // Fresh instance got a brand-new trajectory file too.
    let fresh_path = output.join("fresh.traj.json");
    let fresh: Trajectory =
        serde_json::from_str(&std::fs::read_to_string(&fresh_path).unwrap()).unwrap();
    assert_eq!(fresh.info.outcome.as_deref(), Some(outcome::SUBMITTED));

    // Summary table mentions the skipped count.
    let table = results.summary_table();
    assert!(
        table.contains("Skipped:            1 — trajectory already on disk"),
        "missing skipped row: {table}"
    );
}

#[tokio::test]
async fn without_resume_existing_trajectories_are_overwritten() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["only"]);

    let traj_path = output.join("only.traj.json");
    write_valid_trajectory(&traj_path, "stale");

    let cfg = Config::defaults().unwrap();
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        config: cfg,
        resume: false,
        deterministic_responses: Some(submit_only_responses()),
    })
    .await
    .unwrap();

    assert_eq!(results.total, 1);
    assert_eq!(results.skipped, 0, "no tasks may be skipped without --resume");

    // The stale marker we wrote should have been overwritten by a fresh run.
    let reread: Trajectory =
        serde_json::from_str(&std::fs::read_to_string(&traj_path).unwrap()).unwrap();
    assert!(
        !reread.info.other.contains_key("test_marker"),
        "stale trajectory was not overwritten without --resume"
    );
}
