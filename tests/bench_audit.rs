//! `bench audit`: verify aggregates reconcile to trajectories and datasets.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod support;
use support::binary_path;

const FIXTURE_SWEEP: &str = "tests/fixtures/bundle/sweep";

fn copy_fixture_sweep(root: &Path) -> PathBuf {
    let dst = root.join("sweep");
    copy_dir(Path::new(FIXTURE_SWEEP), &dst);
    dst
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn help_lists_audit_subcommand_and_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit"), "stdout:\n{stdout}");

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--sweep",
        "--dataset-path",
        "--cost-tolerance-usd",
        "--wallclock-tolerance-secs",
        "--format",
    ] {
        assert!(stdout.contains(flag), "missing {flag} in:\n{stdout}");
    }
}

#[test]
fn audit_clean_sweep_passes_successfully() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench audit failed on clean sweep!\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Verify it written audit.json
    assert!(sweep.join("audit.json").exists());
    let audit_json = fs::read_to_string(sweep.join("audit.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&audit_json).unwrap();
    assert_eq!(parsed["artifact_kind"], "audit_report");
    assert_eq!(parsed["overall_pass_fail"], "pass");

    // Skip dataset verification logs skipped note
    assert!(stdout.contains("audit:dataset:skipped"));
}

#[test]
fn audit_tampered_cost_exceeds_tolerance_fails() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Mutate total_cost_usd in results.json
    let results_path = sweep.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&results_path).unwrap()).unwrap();
    results["total_cost_usd"] = serde_json::json!(1.5);
    fs::write(
        &results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(20)); // exit code 20
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit:mismatch:sweep:total_cost_usd"));
}

#[test]
fn audit_tampered_tokens_fails() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Mutate total_input_tokens in results.json
    let results_path = sweep.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&results_path).unwrap()).unwrap();
    results["total_input_tokens"] = serde_json::json!(12345);
    fs::write(
        &results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(20));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit:mismatch:sweep:total_input_tokens"));
}

#[test]
fn audit_tampered_outcome_counts_fails() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Mutate submitted count in results.json
    let results_path = sweep.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&results_path).unwrap()).unwrap();
    results["submitted"] = serde_json::json!(5);
    fs::write(
        &results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(20));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit:mismatch:sweep:submitted"));
}

#[test]
fn audit_orphan_trajectory_fails() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Write a dummy trajectory that has no row in results.json
    let orphan_path = sweep.join("gamma.traj.json");
    let dummy_traj = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.1",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 3 },
        "info": {
            "task": "gamma",
            "model_name": "deterministic",
            "outcome": "submitted",
            "total_cost_usd": 0.0
        },
        "messages": []
    });
    fs::write(
        &orphan_path,
        serde_json::to_string_pretty(&dummy_traj).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(20));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit:orphan:trajectory:gamma"));
}

#[test]
fn audit_missing_trajectory_fails() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Remove beta trajectory which is defined as errored in results.json
    fs::remove_file(sweep.join("beta.traj.json")).unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(20));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit:missing:trajectory:beta"));
}

#[test]
fn audit_evaluator_contradiction_fails() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Change evaluation.json so beta is marked resolved=true,
    // but beta's trajectory outcome in results.json is outcome="error"
    let eval_path = sweep.join("evaluation.json");
    let mut eval: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&eval_path).unwrap()).unwrap();
    eval["instances"][1]["resolved"] = serde_json::json!(true);
    fs::write(&eval_path, serde_json::to_string_pretty(&eval).unwrap()).unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(20));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit:contradiction:instance:beta"));
}

#[test]
fn audit_dataset_hash_mismatch_fails() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Provide a dummy dataset file that does not match the manifest dataset sha256 ("fixture-dataset-hash")
    let dataset_file = work.path().join("dummy_dataset.jsonl");
    fs::write(&dataset_file, "wrong hash content").unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .arg("--dataset-path")
        .arg(&dataset_file)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(20));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit:mismatch:dataset:sha256"));
}

#[test]
fn audit_old_schema_trajectory_yields_partial_logs() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());

    // Modify alpha trajectory to omit "token_usage" completely, which represents an old schema
    let alpha_path = sweep.join("alpha.traj.json");
    let mut alpha: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&alpha_path).unwrap()).unwrap();
    alpha["info"].as_object_mut().unwrap().remove("token_usage");
    fs::write(&alpha_path, serde_json::to_string_pretty(&alpha).unwrap()).unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&sweep)
        .output()
        .unwrap();

    // Recomputation doesn't trigger false mismatch for missing optional fields
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("audit:partial:trajectory:token_usage"));
}
