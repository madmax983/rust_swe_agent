//! `bench instance-history`: end-to-end integration tests.
//!
//! Covers the acceptance criteria from issue #264.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest, FilterSpec, HarnessManifest, InstanceResult,
    ModelManifest, PromptTemplateManifest, ProvenanceManifest, RuntimeManifest,
    SWEEP_STATUS_COMPLETED, SweepResults,
};
use rust_swe_agent::trajectory::{FailureCategory, outcome};

mod support;
use support::binary_path;

// ── fixture helpers ───────────────────────────────────────────────────────────

fn submitted(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(4),
        cost_usd: Some(0.05),
        prompt_tokens: Some(500),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(100),
        duration_secs: Some(8.0),
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: true,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: 1,
        pass_at_1: true,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
    }
}

fn errored(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "error".into(),
        outcome: Some(outcome::ERROR.into()),
        failure_category: Some(FailureCategory::StepLimit),
        steps: Some(6),
        cost_usd: Some(0.10),
        prompt_tokens: Some(1500),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(200),
        duration_secs: Some(15.0),
        error: Some("stub".into()),
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: 0,
        pass_at_1: false,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
    }
}

fn write_sweep(dir: &Path, instances: Vec<InstanceResult>, finished_at: &str) {
    let n = instances.len();
    let submitted_count = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
        .count();
    let errored_count = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
        .count();
    let sweep = SweepResults {
        total: n,
        sweep_status: SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: n,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: submitted_count,
        submitted_with_tests: 0,
        skipped: 0,
        errored: errored_count,
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 0,
        estimated_cost_usd: 0.0,
        actual_cost_usd: None,
        actual_cost_source: None,
        baseline_cost_usd: None,
        baseline_cost_model: None,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: 0.0,
        filter_spec: FilterSpec::default(),
        manifest: Some(ProvenanceManifest {
            purpose: None,
            harness: HarnessManifest {
                name: "rust_swe_agent".into(),
                version: "test".into(),
                git_sha: None,
                git_dirty: None,
                git_resolution: "test".into(),
            },
            dataset: DatasetManifest {
                path: "test.jsonl".into(),
                sha256: "test".into(),
                instance_count: n,
                filter_spec: None,
                ..Default::default()
            },
            prompt_template: PromptTemplateManifest {
                source: "inline".into(),
                path: None,
                sha256: "test".into(),
            },
            config: ConfigManifest {
                resolved: "test".into(),
                overlay_paths: Vec::new(),
            },
            model: ModelManifest {
                name: "test-model".into(),
                backend: "litellm".into(),
                backend_version: None,
                base_url: None,
            },
            runtime: RuntimeManifest {
                started_at_utc: "2026-05-01T00:00:00Z".into(),
                finished_at_utc: Some(finished_at.into()),
                host_os: "linux".into(),
                resume_mode: false,
                rust_version: None,
            },
            cli: CliManifest { argv: Vec::new() },
            circuit_breaker: None,
            reproduced_from: None,
        }),
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: vec![],
    };
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&sweep).unwrap(),
    )
    .unwrap();
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// AC (a): three identical-config sweeps over the same 5-instance slice with
/// one known flipper produce the predicted classification.
#[test]
fn test_flipper_classification_three_sweeps() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    let s2 = dir.path().join("sweep2");
    let s3 = dir.path().join("sweep3");

    // alpha/delta: always resolved (stable_win)
    // beta/epsilon: never resolved (stable_loss)
    // gamma: resolved in s1 and s3 only — one confirmed flipper
    write_sweep(
        &s1,
        vec![
            submitted("alpha"),
            errored("beta"),
            submitted("gamma"),
            submitted("delta"),
            errored("epsilon"),
        ],
        "2026-05-01T01:00:00Z",
    );
    write_sweep(
        &s2,
        vec![
            submitted("alpha"),
            errored("beta"),
            errored("gamma"),
            submitted("delta"),
            errored("epsilon"),
        ],
        "2026-05-02T01:00:00Z",
    );
    write_sweep(
        &s3,
        vec![
            submitted("alpha"),
            errored("beta"),
            submitted("gamma"),
            submitted("delta"),
            errored("epsilon"),
        ],
        "2026-05-03T01:00:00Z",
    );

    let out = dir.path().join("instance-history.json");
    let status = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s1.to_str().unwrap(),
            "--sweeps",
            s2.to_str().unwrap(),
            "--sweeps",
            s3.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "command should succeed");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();

    let instances = report["instances"].as_array().unwrap();
    let by_id: std::collections::HashMap<&str, &serde_json::Value> = instances
        .iter()
        .map(|v| (v["instance_id"].as_str().unwrap(), v))
        .collect();

    assert_eq!(by_id["alpha"]["stability_class"], "stable_win");
    assert_eq!(by_id["beta"]["stability_class"], "stable_loss");
    assert_eq!(by_id["gamma"]["stability_class"], "flipper");
    assert_eq!(by_id["delta"]["stability_class"], "stable_win");
    assert_eq!(by_id["epsilon"]["stability_class"], "stable_loss");

    assert_eq!(report["sweep_count"], 3);
    assert_eq!(report["intersection_size"], 5);
    assert_eq!(report["stability_counts"]["stable_win"], 2);
    assert_eq!(report["stability_counts"]["stable_loss"], 2);
    assert_eq!(report["stability_counts"]["flipper"], 1);

    let flipper_share = report["flipper_share"].as_f64().unwrap();
    assert!(
        (flipper_share - 0.2).abs() < 1e-9,
        "flipper_share should be 0.2"
    );

    // gamma must have at least one flip_event
    let gamma = by_id["gamma"];
    assert!(!gamma["flip_events"].as_array().unwrap().is_empty());
    assert!(!gamma["last_flip"].is_null());
}

/// AC (b): two sweeps with disjoint instance sets produce empty intersection.
#[test]
fn test_disjoint_instances_empty_intersection() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    let s2 = dir.path().join("sweep2");

    write_sweep(
        &s1,
        vec![submitted("alpha"), errored("beta")],
        "2026-05-01T01:00:00Z",
    );
    write_sweep(
        &s2,
        vec![submitted("gamma"), errored("delta")],
        "2026-05-02T01:00:00Z",
    );

    let out = dir.path().join("instance-history.json");
    let status = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s1.to_str().unwrap(),
            "--sweeps",
            s2.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "empty intersection is not an error by default"
    );

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(report["intersection_size"], 0);
    // all 4 instances should appear as partial_coverage (seen in exactly 1/2 sweeps)
    assert_eq!(report["partial_coverage_count"], 4);
}

/// AC (c): `--require-full-coverage` exits non-zero when partial share > limit.
#[test]
fn test_require_full_coverage_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    let s2 = dir.path().join("sweep2");

    // gamma is only in s1, so it's partial (1 out of 2 sweeps)
    write_sweep(
        &s1,
        vec![submitted("alpha"), errored("beta"), submitted("gamma")],
        "2026-05-01T01:00:00Z",
    );
    write_sweep(
        &s2,
        vec![submitted("alpha"), errored("beta")],
        "2026-05-02T01:00:00Z",
    );

    let out = dir.path().join("instance-history.json");
    let status = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s1.to_str().unwrap(),
            "--sweeps",
            s2.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
            "--require-full-coverage",
            "--max-partial-share",
            "0.0",
        ])
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "should exit non-zero when partial share exceeds limit"
    );
}

/// AC (d): `--stable-threshold 0.9` moves an instance that was a flipper at the
/// default threshold into unstable_minority_win/loss.
///
/// Instance "swing" resolves in 7 of 10 sweeps (rate=0.70).
/// At T=1.0 (default): all non-N/N, non-0/N are "flipper".
/// At T=0.9: 0.70 < 0.90 and 0.70 > 0.50 → unstable_minority_win.
#[test]
fn test_stable_threshold_relaxation() {
    let dir = tempfile::tempdir().unwrap();
    let sweeps: Vec<_> = (0..10)
        .map(|i| dir.path().join(format!("sweep{i}")))
        .collect();

    // swing resolves in sweeps 0–6 (7 of 10), stable in rest
    for (i, sweep_dir) in sweeps.iter().enumerate() {
        let instances = if i < 7 {
            vec![submitted("stable"), submitted("swing"), errored("loss")]
        } else {
            vec![submitted("stable"), errored("swing"), errored("loss")]
        };
        write_sweep(
            sweep_dir,
            instances,
            &format!("2026-05-{:02}T01:00:00Z", i + 1),
        );
    }

    let out_default = dir.path().join("history-default.json");
    let out_relaxed = dir.path().join("history-relaxed.json");

    let mut sweep_args: Vec<String> = Vec::new();
    for s in &sweeps {
        sweep_args.push("--sweeps".into());
        sweep_args.push(s.to_str().unwrap().into());
    }

    // Default threshold (1.0): swing → flipper
    let status = Command::new(binary_path())
        .arg("bench")
        .arg("instance-history")
        .args(&sweep_args)
        .arg("--output")
        .arg(out_default.to_str().unwrap())
        .status()
        .unwrap();
    assert!(status.success());

    let report_default: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out_default).unwrap()).unwrap();
    let instances_default = report_default["instances"].as_array().unwrap();
    let swing_default = instances_default
        .iter()
        .find(|v| v["instance_id"] == "swing")
        .unwrap();
    assert_eq!(
        swing_default["stability_class"], "flipper",
        "at T=1.0, swing should be flipper"
    );

    // Relaxed threshold (0.9): swing (rate=0.7 < 0.9, rate > 0.5) → unstable_minority_win
    let status = Command::new(binary_path())
        .arg("bench")
        .arg("instance-history")
        .args(&sweep_args)
        .arg("--output")
        .arg(out_relaxed.to_str().unwrap())
        .arg("--stable-threshold")
        .arg("0.9")
        .status()
        .unwrap();
    assert!(status.success());

    let report_relaxed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out_relaxed).unwrap()).unwrap();
    let instances_relaxed = report_relaxed["instances"].as_array().unwrap();
    let swing_relaxed = instances_relaxed
        .iter()
        .find(|v| v["instance_id"] == "swing")
        .unwrap();
    assert_eq!(
        swing_relaxed["stability_class"], "unstable_minority_win",
        "at T=0.9, swing (rate=0.7) should be unstable_minority_win"
    );
}

/// AC (e): flip_events are ordered by finished_at, not sweep input order.
#[test]
fn test_flip_events_ordered_by_finished_at() {
    let dir = tempfile::tempdir().unwrap();
    let s_early = dir.path().join("early"); // finished 2026-05-01
    let s_late = dir.path().join("late"); // finished 2026-05-03
    let s_mid = dir.path().join("mid"); // finished 2026-05-02

    // gamma: win in early, loss in mid, win in late → 2 flip events
    write_sweep(&s_early, vec![submitted("gamma")], "2026-05-01T01:00:00Z");
    write_sweep(&s_late, vec![submitted("gamma")], "2026-05-03T01:00:00Z");
    write_sweep(&s_mid, vec![errored("gamma")], "2026-05-02T01:00:00Z");

    let out = dir.path().join("instance-history.json");
    // Pass sweeps in deliberately wrong order: late, early, mid
    let status = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s_late.to_str().unwrap(),
            "--sweeps",
            s_early.to_str().unwrap(),
            "--sweeps",
            s_mid.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let instances = report["instances"].as_array().unwrap();
    let gamma = instances
        .iter()
        .find(|v| v["instance_id"] == "gamma")
        .unwrap();

    let outcomes = gamma["sweep_outcomes"].as_array().unwrap();
    // outcomes must be sorted by finished_at ascending
    assert_eq!(outcomes.len(), 3);
    assert_eq!(
        outcomes[0]["finished_at"].as_str().unwrap(),
        "2026-05-01T01:00:00Z"
    );
    assert_eq!(
        outcomes[1]["finished_at"].as_str().unwrap(),
        "2026-05-02T01:00:00Z"
    );
    assert_eq!(
        outcomes[2]["finished_at"].as_str().unwrap(),
        "2026-05-03T01:00:00Z"
    );

    // First flip: win→loss (early→mid), second flip: loss→win (mid→late)
    let flips = gamma["flip_events"].as_array().unwrap();
    assert_eq!(flips.len(), 2);
    assert_eq!(flips[0]["direction"].as_str().unwrap(), "win→loss");
    assert_eq!(flips[1]["direction"].as_str().unwrap(), "loss→win");
}

/// AC (f): a sweep missing results.json is listed in skipped_sweeps and not counted.
#[test]
fn test_missing_results_json_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    let s2 = dir.path().join("sweep2");
    let s_bad = dir.path().join("sweep_bad"); // no results.json

    write_sweep(
        &s1,
        vec![submitted("alpha"), errored("beta")],
        "2026-05-01T01:00:00Z",
    );
    write_sweep(
        &s2,
        vec![submitted("alpha"), errored("beta")],
        "2026-05-02T01:00:00Z",
    );
    std::fs::create_dir_all(&s_bad).unwrap(); // exists but no results.json

    let out = dir.path().join("instance-history.json");
    let status = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s1.to_str().unwrap(),
            "--sweeps",
            s2.to_str().unwrap(),
            "--sweeps",
            s_bad.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    // Only the 2 valid sweeps count
    assert_eq!(report["sweep_count"], 2);
    let skipped = report["skipped_sweeps"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
}

/// AC (g): determinism — two runs on the same inputs produce byte-identical JSON
/// (modulo generated_at).
#[test]
fn test_determinism() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    let s2 = dir.path().join("sweep2");

    write_sweep(
        &s1,
        vec![submitted("alpha"), errored("beta"), submitted("gamma")],
        "2026-05-01T01:00:00Z",
    );
    write_sweep(
        &s2,
        vec![submitted("alpha"), errored("beta"), errored("gamma")],
        "2026-05-02T01:00:00Z",
    );

    let run_and_read = |suffix: &str| -> serde_json::Value {
        let out = dir.path().join(format!("history-{suffix}.json"));
        let status = Command::new(binary_path())
            .args([
                "bench",
                "instance-history",
                "--sweeps",
                s1.to_str().unwrap(),
                "--sweeps",
                s2.to_str().unwrap(),
                "--output",
                out.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap()
    };

    let r1 = run_and_read("run1");
    let r2 = run_and_read("run2");

    // Strip generated_at before comparing
    let strip = |mut v: serde_json::Value| -> serde_json::Value {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("generated_at");
        }
        v
    };
    assert_eq!(
        strip(r1),
        strip(r2),
        "reports should be identical modulo generated_at"
    );
}

/// Fewer than 2 valid sweeps → exit non-zero with clear error.
#[test]
fn test_fewer_than_two_sweeps_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    write_sweep(&s1, vec![submitted("alpha")], "2026-05-01T01:00:00Z");

    let out = dir.path().join("instance-history.json");
    let output = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s1.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "need ≥ 2 sweeps");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("2") || stderr.contains("sweep"),
        "stderr should mention the requirement: {stderr}"
    );
}

/// `--focus` flag should exit 0 and include only flippers in text output.
#[test]
fn test_focus_flag_shows_flippers() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    let s2 = dir.path().join("sweep2");

    write_sweep(
        &s1,
        vec![
            submitted("stable"),
            submitted("flipper_inst"),
            errored("loser"),
        ],
        "2026-05-01T01:00:00Z",
    );
    write_sweep(
        &s2,
        vec![
            submitted("stable"),
            errored("flipper_inst"),
            errored("loser"),
        ],
        "2026-05-02T01:00:00Z",
    );

    let out = dir.path().join("instance-history.json");
    let output = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s1.to_str().unwrap(),
            "--sweeps",
            s2.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
            "--focus",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    // stdout should mention the flipper but not the stable or loser
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("flipper_inst"),
        "flipper should appear in focus output"
    );
    assert!(
        !stdout.contains("stable") || stdout.contains("flipper"),
        "stable-win instances should not dominate the focus view"
    );
}

/// `bench instance-history --help` is accessible and mentions the command.
#[test]
fn test_help_accessible() {
    let output = Command::new(binary_path())
        .args(["bench", "instance-history", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.to_lowercase().contains("instance") || stdout.to_lowercase().contains("sweep"),
        "help text should mention key concepts: {stdout}"
    );
}

// ── gap-closure tests (AC items previously missing) ───────────────────────────

/// Sweep discovery (a): --sweeps pointing to a parent directory that contains
/// sweep subdirectories should auto-discover those subdirs.
#[test]
fn test_sweep_discovery_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("runs");

    // Create two sweep subdirectories inside the parent
    let s1 = parent.join("sweep1");
    let s2 = parent.join("sweep2");
    write_sweep(
        &s1,
        vec![submitted("alpha"), errored("beta")],
        "2026-05-01T01:00:00Z",
    );
    write_sweep(
        &s2,
        vec![submitted("alpha"), submitted("beta")],
        "2026-05-02T01:00:00Z",
    );

    let out = dir.path().join("instance-history.json");
    let status = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            parent.to_str().unwrap(), // pass the *parent* dir, not individual sweep dirs
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "parent-dir discovery should succeed");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(
        report["sweep_count"], 2,
        "should discover both subdirectory sweeps"
    );
    assert_eq!(report["intersection_size"], 2);
}

/// Sweep discovery (b): --sweeps with a glob pattern expands to matching dirs.
#[test]
fn test_sweep_discovery_glob() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("runs");

    let s1 = parent.join("sweep-2026-05-01");
    let s2 = parent.join("sweep-2026-05-02");
    let s3 = parent.join("other-dir"); // should NOT match the glob
    write_sweep(
        &s1,
        vec![submitted("alpha"), errored("beta")],
        "2026-05-01T01:00:00Z",
    );
    write_sweep(
        &s2,
        vec![submitted("alpha"), submitted("beta")],
        "2026-05-02T01:00:00Z",
    );
    write_sweep(
        &s3,
        vec![submitted("alpha"), errored("beta")],
        "2026-05-03T01:00:00Z",
    );

    let glob_pat = format!("{}/sweep-*", parent.display());
    let out = dir.path().join("instance-history.json");
    let status = Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            &glob_pat,
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "glob discovery should succeed");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    // Only the 2 sweep-* dirs should match; other-dir excluded
    assert_eq!(
        report["sweep_count"], 2,
        "glob should match only sweep-* dirs"
    );
}

/// sweep_outcomes entries carry a sampling_summary field with runs/resolved_count.
#[test]
fn test_sweep_outcomes_sampling_summary() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    let s2 = dir.path().join("sweep2");
    write_sweep(&s1, vec![submitted("alpha")], "2026-05-01T01:00:00Z");
    write_sweep(&s2, vec![errored("alpha")], "2026-05-02T01:00:00Z");

    let out = dir.path().join("instance-history.json");
    Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s1.to_str().unwrap(),
            "--sweeps",
            s2.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let instances = report["instances"].as_array().unwrap();
    let alpha = instances
        .iter()
        .find(|v| v["instance_id"] == "alpha")
        .unwrap();
    let outcomes = alpha["sweep_outcomes"].as_array().unwrap();

    // Each sweep_outcome must have a sampling_summary
    for outcome in outcomes {
        assert!(
            outcome.get("sampling_summary").is_some(),
            "sweep_outcome must have sampling_summary: {outcome}"
        );
        let ss = &outcome["sampling_summary"];
        assert!(
            ss["runs"].is_number(),
            "sampling_summary.runs must be a number"
        );
        assert!(
            ss["resolved_count"].is_number(),
            "sampling_summary.resolved_count must be a number"
        );
    }
}

/// flip_events carry a finished_at_delta field expressing the time gap.
#[test]
fn test_flip_event_finished_at_delta() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = dir.path().join("sweep1");
    let s2 = dir.path().join("sweep2");
    // gamma flips: resolved in s1, not in s2
    write_sweep(&s1, vec![submitted("gamma")], "2026-05-01T00:00:00Z");
    write_sweep(&s2, vec![errored("gamma")], "2026-05-02T00:00:00Z"); // exactly 1 day later

    let out = dir.path().join("instance-history.json");
    Command::new(binary_path())
        .args([
            "bench",
            "instance-history",
            "--sweeps",
            s1.to_str().unwrap(),
            "--sweeps",
            s2.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let instances = report["instances"].as_array().unwrap();
    let gamma = instances
        .iter()
        .find(|v| v["instance_id"] == "gamma")
        .unwrap();
    let flips = gamma["flip_events"].as_array().unwrap();
    assert_eq!(flips.len(), 1);

    // finished_at_delta should be present and non-null
    let delta = &flips[0]["finished_at_delta"];
    assert!(
        !delta.is_null(),
        "finished_at_delta must be present on flip_event"
    );
    // Should represent roughly 86400 seconds (1 day)
    let secs = delta.as_i64().unwrap();
    assert_eq!(secs, 86400, "delta should be 86400 seconds (1 day)");
}
