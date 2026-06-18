//! `bench variance` — tests covering all AC items from issue #524.
//!
//! Red → Green → Refactor TDD cycle.
//!
//! AC items covered:
//! AC1: rejects single-slot sweeps (runs <= 1) with non-zero exit and clear message
//! AC2: per-instance stability class: always_resolved, always_failed, flaky
//! AC3: sweep-wide noise metrics: flaky count/share, pass_at_k, all_of_k, CI
//! AC4: recommended rerun count for --ci-width target
//! AC5: --format text|json; JSON is schema-versioned
//! AC6: --filter and --class selectors
//! AC7: reads only on-disk artifacts ($0 cost — no model or network calls)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::items_after_statements
)]

use std::collections::BTreeMap;
use std::path::Path;

use maxwells_daemon::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest, HarnessManifest, InstanceResult, ModelManifest,
    PromptTemplateManifest, ProvenanceManifest, RuntimeManifest, SWEEP_STATUS_COMPLETED,
    SweepResults,
};
use maxwells_daemon::run::variance::{BenchVarianceArgs, StabilityClass, compute_variance};
use maxwells_daemon::trajectory::outcome;

mod support;
use support::binary_path;

// ── fixture helpers ───────────────────────────────────────────────────────────

fn rerun_instance(id: &str, runs: u32, resolved_count: u32) -> InstanceResult {
    let pass_at_1 = {
        // simulate: first slot resolved if any resolved
        resolved_count > 0
    };
    InstanceResult {
        instance_id: id.into(),
        exit_reason: if resolved_count > 0 {
            "submitted".into()
        } else {
            "error".into()
        },
        outcome: Some(if resolved_count > 0 {
            outcome::SUBMITTED.into()
        } else {
            outcome::ERROR.into()
        }),
        failure_category: None,
        steps: Some(10),
        cost_usd: Some(0.01),
        prompt_tokens: Some(500),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(100),
        duration_secs: Some(5.0),
        error: None,
        github_pr_error: None,
        patch_present: resolved_count > 0,
        non_empty_patch: resolved_count > 0,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs,
        resolved_count,
        pass_at_1,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
        trace_id: None,
    }
}

fn make_manifest() -> ProvenanceManifest {
    let argv = vec![
        "max".into(),
        "bench".into(),
        "swebench".into(),
        "--reruns".into(),
        "3".into(),
    ];
    ProvenanceManifest {
        purpose: None,
        harness: HarnessManifest {
            name: "maxwells-daemon".into(),
            version: "0.1.0-test".into(),
            git_sha: Some("deadbeef".into()),
            git_dirty: Some(false),
            git_resolution: "exact".into(),
        },
        dataset: DatasetManifest {
            path: "tests/fixtures/test.jsonl".into(),
            sha256: "abc123".into(),
            instance_count: 3,
            filter_spec: None,
            ..Default::default()
        },
        prompt_template: PromptTemplateManifest {
            source: "inline".into(),
            path: None,
            sha256: "tpl123".into(),
        },
        config: ConfigManifest {
            resolved: "default".into(),
            overlay_paths: Vec::new(),
        },
        model: ModelManifest {
            name: "claude-opus-4-7".into(),
            backend: "litellm".into(),
            backend_version: None,
            base_url: None,
        },
        runtime: RuntimeManifest {
            started_at_utc: "2026-05-01T00:00:00Z".into(),
            finished_at_utc: Some("2026-05-01T00:10:00Z".into()),
            host_os: "linux".into(),
            resume_mode: false,
            rust_version: Some("rustc 1.85.0".into()),
        },
        cli: CliManifest { argv },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    }
}

fn write_results(dir: &Path, instances: Vec<InstanceResult>) {
    let total = instances.len();
    let manifest = make_manifest();
    let pass_at_k = if total > 0 {
        instances.iter().filter(|i| i.resolved_count > 0).count() as f64 / total as f64
    } else {
        0.0
    };
    let submitted = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
        .count();
    let errored = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
        .count();
    let failures_by_category = BTreeMap::new();
    let sweep = SweepResults {
        total,
        sweep_status: SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: total,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted,
        submitted_with_tests: 0,
        skipped: 0,
        errored,
        failures_by_category,
        budget_halted: 0,
        with_patch: submitted,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 0,
        estimated_cost_usd: 0.03,
        actual_cost_usd: Some(0.03),
        actual_cost_source: None,
        baseline_cost_usd: None,
        baseline_cost_model: None,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k,
        filter_spec: Default::default(),
        manifest: Some(manifest),
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: vec![],
        partial: 0,
        span_export_dropped: 0,
    };
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&sweep).unwrap(),
    )
    .unwrap();
}

// ── AC1: rejects single-slot sweeps ──────────────────────────────────────────

#[test]
fn ac1_rejects_single_slot_sweep() {
    let dir = tempfile::tempdir().unwrap();
    // Write a sweep where all instances have runs == 1 (single slot)
    write_results(
        dir.path(),
        vec![
            rerun_instance("task-a", 1, 1),
            rerun_instance("task-b", 1, 0),
        ],
    );

    let result = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    });

    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("single")
            || msg.contains("rerun")
            || msg.contains("slots")
            || msg.contains("runs"),
        "expected single-slot rejection message, got: {msg}"
    );
}

#[test]
fn ac1_rejects_missing_results_json() {
    let dir = tempfile::tempdir().unwrap();
    // No results.json — should fail with a clear error
    let result = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    });
    assert!(result.is_err(), "expected error for missing results.json");
}

#[test]
fn ac1_rejects_budget_halted_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let halted = serde_json::json!({
        "total": 3,
        "sweep_status": "completed",
        "budget_halted": 2,
        "instances": []
    });
    std::fs::write(
        dir.path().join("results.json"),
        serde_json::to_string(&halted).unwrap(),
    )
    .unwrap();

    let result = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    });

    assert!(result.is_err(), "expected error for budget-halted sweep");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("budget") || msg.contains("halted"),
        "error should mention budget halt, got: {msg}"
    );
}

#[test]
fn ac1_rejects_incomplete_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let cancelled = serde_json::json!({"total": 2, "sweep_status": "cancelled", "instances": []});
    std::fs::write(
        dir.path().join("results.json"),
        serde_json::to_string(&cancelled).unwrap(),
    )
    .unwrap();

    let result = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    });

    assert!(result.is_err(), "expected error for cancelled sweep");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("cancelled") || msg.contains("completed") || msg.contains("status"),
        "error should mention sweep status, got: {msg}"
    );
}

#[test]
fn ac1_excludes_single_slot_instances_in_mixed_sweep() {
    let dir = tempfile::tempdir().unwrap();
    // Mixed sweep: two 3-slot instances and one 1-slot instance
    write_results(
        dir.path(),
        vec![
            rerun_instance("multi-a", 3, 2),
            rerun_instance("multi-b", 3, 3),
            rerun_instance("single", 1, 1), // should be excluded
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    })
    .unwrap();

    // Only the two multi-slot instances should appear
    assert_eq!(report.instances.len(), 2);
    assert!(
        report.instances.iter().all(|i| i.instance_id != "single"),
        "single-slot instance must be excluded from variance analysis"
    );
}

// ── AC2: per-instance stability classes ───────────────────────────────────────

#[test]
fn ac2_always_resolved_classification() {
    let dir = tempfile::tempdir().unwrap();
    // Instance resolved all 3 slots → always_resolved
    write_results(dir.path(), vec![rerun_instance("task-a", 3, 3)]);

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    })
    .unwrap();

    assert_eq!(report.instances.len(), 1);
    let inst = &report.instances[0];
    assert_eq!(inst.instance_id, "task-a");
    assert_eq!(inst.resolved_slots, 3);
    assert_eq!(inst.total_slots, 3);
    assert_eq!(inst.stability_class, StabilityClass::AlwaysResolved);
}

#[test]
fn ac2_always_failed_classification() {
    let dir = tempfile::tempdir().unwrap();
    // Instance resolved 0 of 3 slots → always_failed
    write_results(dir.path(), vec![rerun_instance("task-a", 3, 0)]);

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    })
    .unwrap();

    let inst = &report.instances[0];
    assert_eq!(inst.stability_class, StabilityClass::AlwaysFailed);
}

#[test]
fn ac2_flaky_classification() {
    let dir = tempfile::tempdir().unwrap();
    // Instance resolved 2 of 4 slots → flaky
    write_results(dir.path(), vec![rerun_instance("task-a", 4, 2)]);

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    })
    .unwrap();

    let inst = &report.instances[0];
    assert_eq!(inst.stability_class, StabilityClass::Flaky);
}

#[test]
fn ac2_mixed_classes_in_one_sweep() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            rerun_instance("always-resolved", 3, 3),
            rerun_instance("always-failed", 3, 0),
            rerun_instance("flaky", 3, 1),
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    })
    .unwrap();

    assert_eq!(report.instances.len(), 3);

    let find = |id: &str| {
        report
            .instances
            .iter()
            .find(|i| i.instance_id == id)
            .unwrap()
    };
    assert_eq!(
        find("always-resolved").stability_class,
        StabilityClass::AlwaysResolved
    );
    assert_eq!(
        find("always-failed").stability_class,
        StabilityClass::AlwaysFailed
    );
    assert_eq!(find("flaky").stability_class, StabilityClass::Flaky);
}

// ── AC3: sweep-wide noise metrics ─────────────────────────────────────────────

#[test]
fn ac3_noise_metrics_all_resolved() {
    let dir = tempfile::tempdir().unwrap();
    // All 3 instances always resolved
    write_results(
        dir.path(),
        vec![
            rerun_instance("a", 3, 3),
            rerun_instance("b", 3, 3),
            rerun_instance("c", 3, 3),
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    })
    .unwrap();

    let noise = &report.noise;
    assert_eq!(noise.flaky_count, 0);
    assert!((noise.flaky_share - 0.0).abs() < 1e-9);
    assert!(
        (noise.pass_at_k - 1.0).abs() < 1e-9,
        "pass_at_k={}",
        noise.pass_at_k
    );
    assert!(
        (noise.all_of_k - 1.0).abs() < 1e-9,
        "all_of_k={}",
        noise.all_of_k
    );
    assert!(
        (noise.pass_at_1 - 1.0).abs() < 1e-9,
        "pass_at_1={}",
        noise.pass_at_1
    );
    // CI must include 1.0 when all pass
    assert!(noise.ci_upper >= 1.0 - 1e-9, "ci_upper={}", noise.ci_upper);
    // Wilson CI lower for p=1, n=9 slots is ~0.70; check it's meaningfully high
    assert!(
        noise.ci_lower >= 0.6,
        "ci_lower should be high, got {}",
        noise.ci_lower
    );
}

#[test]
fn ac3_noise_metrics_mixed() {
    let dir = tempfile::tempdir().unwrap();
    // 3 instances, 3 slots each: always_resolved, always_failed, flaky(1/3)
    write_results(
        dir.path(),
        vec![
            rerun_instance("always-resolved", 3, 3),
            rerun_instance("always-failed", 3, 0),
            rerun_instance("flaky", 3, 1),
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    })
    .unwrap();

    let noise = &report.noise;
    // 1 flaky instance out of 3
    assert_eq!(noise.flaky_count, 1);
    assert!(
        (noise.flaky_share - 1.0 / 3.0).abs() < 1e-9,
        "flaky_share={}",
        noise.flaky_share
    );
    // pass_at_k (any-of-k): always_resolved and flaky both have resolved_count > 0 → 2/3
    assert!(
        (noise.pass_at_k - 2.0 / 3.0).abs() < 1e-9,
        "pass_at_k={}",
        noise.pass_at_k
    );
    // all_of_k: only always_resolved has resolved_count == runs → 1/3
    assert!(
        (noise.all_of_k - 1.0 / 3.0).abs() < 1e-9,
        "all_of_k={}",
        noise.all_of_k
    );
    // pass_at_1 (per-slot): total_resolved/total_slots = (3+0+1)/9 = 4/9
    let expected_pass_at_1 = 4.0 / 9.0;
    assert!(
        (noise.pass_at_1 - expected_pass_at_1).abs() < 1e-9,
        "pass_at_1={} expected={}",
        noise.pass_at_1,
        expected_pass_at_1
    );
    // CI bounds should be non-NaN and within [0,1]
    assert!(noise.ci_lower >= 0.0 && noise.ci_lower <= 1.0);
    assert!(noise.ci_upper >= 0.0 && noise.ci_upper <= 1.0);
    assert!(noise.ci_lower <= noise.ci_upper);
    // CI should contain the estimate
    assert!(noise.ci_lower <= noise.pass_at_1 + 1e-9);
    assert!(noise.ci_upper >= noise.pass_at_1 - 1e-9);
}

#[test]
fn ac3_spread_between_best_and_worst_case() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            rerun_instance("a", 5, 5), // always resolved
            rerun_instance("b", 5, 3), // flaky
            rerun_instance("c", 5, 0), // always failed
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: None,
    })
    .unwrap();

    let noise = &report.noise;
    // pass_at_k (any-of-k, best-case): a and b both have any resolved → 2/3
    assert!((noise.pass_at_k - 2.0 / 3.0).abs() < 1e-9);
    // all_of_k (all-of-k, worst-case): only a has all resolved → 1/3
    assert!((noise.all_of_k - 1.0 / 3.0).abs() < 1e-9);
    // Spread = pass_at_k - all_of_k = 1/3
    assert!(noise.pass_at_k > noise.all_of_k);
}

// ── AC4: recommended rerun count ─────────────────────────────────────────────

#[test]
fn ac4_recommended_reruns_for_ci_width() {
    let dir = tempfile::tempdir().unwrap();
    // p ≈ 0.5, high variance → many reruns needed
    write_results(
        dir.path(),
        vec![
            rerun_instance("a", 2, 1),
            rerun_instance("b", 2, 1),
            rerun_instance("c", 2, 1),
            rerun_instance("d", 2, 1),
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: Some(0.05), // narrow target: 5% half-width
        filter: vec![],
        class: None,
    })
    .unwrap();

    // With p=0.5, need n >= (1.96/0.05)^2 * 0.25 ≈ 384 slots
    // With 4 instances, that's ceil(384/4) = 96 reruns per instance minimum
    // So recommended should be > current reruns (2) when ci_width=0.05
    assert!(
        report.noise.recommended_reruns.is_some(),
        "expected a recommended rerun count"
    );
    let rec = report.noise.recommended_reruns.unwrap();
    assert!(
        rec > 2,
        "recommended reruns ({rec}) should exceed current 2 for such a narrow CI target"
    );
}

#[test]
fn ac4_current_reruns_already_sufficient() {
    let dir = tempfile::tempdir().unwrap();
    // With 100 instances at 5 reruns each, p=1.0 → CI is very tight
    let instances: Vec<InstanceResult> = (0..100)
        .map(|i| rerun_instance(&format!("task-{i}"), 5, 5))
        .collect();
    write_results(dir.path(), instances);

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: Some(0.10), // generous: 10% half-width
        filter: vec![],
        class: None,
    })
    .unwrap();

    // With p=1.0, variance is 0, so CI width is 0 → current reruns suffice
    // recommended_reruns should be None or 1
    if let Some(rec) = report.noise.recommended_reruns {
        assert!(
            rec <= 5,
            "recommended reruns ({rec}) should not exceed current 5 when CI already sufficient"
        );
    }
}

#[test]
fn ac4_recommends_reruns_for_extreme_p_small_n() {
    let dir = tempfile::tempdir().unwrap();
    // 3 instances × 2 reruns, all pass → p=1.0 but only 6 total slots
    write_results(
        dir.path(),
        vec![
            rerun_instance("a", 2, 2),
            rerun_instance("b", 2, 2),
            rerun_instance("c", 2, 2),
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: Some(0.05),
        filter: vec![],
        class: None,
    })
    .unwrap();

    // Wilson CI half-width for p=1, n=6 is ~0.20 >> 0.05 target
    assert!(
        report.noise.recommended_reruns.is_some(),
        "should recommend reruns when p=1 but Wilson CI is still wide"
    );
}

// ── AC5: --format text|json ───────────────────────────────────────────────────

#[test]
fn ac5_json_output_is_schema_versioned() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![rerun_instance("a", 3, 3), rerun_instance("b", 3, 1)],
    );

    // Call binary with --format json
    let output = std::process::Command::new(binary_path())
        .args(["bench", "variance", "--sweep"])
        .arg(dir.path())
        .args(["--format", "json"])
        .output()
        .expect("failed to run max binary");

    assert!(
        output.status.success(),
        "expected exit 0, got {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value =
        serde_json::from_str(&stdout).expect("JSON output should be valid JSON");

    // Must have schema_version and artifact_kind fields
    assert!(
        val.get("schema_version").is_some(),
        "JSON must have schema_version, got: {stdout}"
    );
    assert!(
        val.get("artifact_kind").is_some(),
        "JSON must have artifact_kind, got: {stdout}"
    );
    assert_eq!(
        val["artifact_kind"].as_str().unwrap(),
        "bench_variance_report",
        "artifact_kind must be bench_variance_report"
    );
    // Must have instances and noise fields
    assert!(val.get("instances").is_some());
    assert!(val.get("noise").is_some());
}

#[test]
fn ac5_text_output_contains_stability_classes() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            rerun_instance("always-resolved", 3, 3),
            rerun_instance("always-failed", 3, 0),
            rerun_instance("flaky-instance", 3, 1),
        ],
    );

    let output = std::process::Command::new(binary_path())
        .args(["bench", "variance", "--sweep"])
        .arg(dir.path())
        .args(["--format", "text"])
        .output()
        .expect("failed to run max binary");

    assert!(
        output.status.success(),
        "expected exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("always_resolved") || stdout.contains("AlwaysResolved"),
        "text output should mention always_resolved, got: {stdout}"
    );
    assert!(
        stdout.contains("always_failed") || stdout.contains("AlwaysFailed"),
        "text output should mention always_failed, got: {stdout}"
    );
    assert!(
        stdout.contains("flaky"),
        "text output should mention flaky, got: {stdout}"
    );
}

#[test]
fn ac5_invalid_format_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![rerun_instance("a", 3, 2)]);

    let output = std::process::Command::new(binary_path())
        .args(["bench", "variance", "--sweep"])
        .arg(dir.path())
        .args(["--format", "csv"])
        .output()
        .expect("failed to run max binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit for invalid format"
    );
}

// ── AC6: --filter and --class selectors ───────────────────────────────────────

#[test]
fn ac6_class_filter_flaky_only() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            rerun_instance("always-resolved", 3, 3),
            rerun_instance("always-failed", 3, 0),
            rerun_instance("flaky-a", 3, 1),
            rerun_instance("flaky-b", 3, 2),
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: Some("flaky".into()),
    })
    .unwrap();

    assert_eq!(
        report.instances.len(),
        2,
        "expected 2 flaky instances, got {}",
        report.instances.len()
    );
    for inst in &report.instances {
        assert_eq!(
            inst.stability_class,
            StabilityClass::Flaky,
            "class filter should only return flaky instances"
        );
    }
}

#[test]
fn ac6_class_filter_always_resolved() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            rerun_instance("a", 3, 3),
            rerun_instance("b", 3, 0),
            rerun_instance("c", 3, 2),
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: Some("always_resolved".into()),
    })
    .unwrap();

    assert_eq!(report.instances.len(), 1);
    assert_eq!(report.instances[0].instance_id, "a");
}

#[test]
fn ac6_invalid_class_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![rerun_instance("a", 3, 2)]);

    let result = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec![],
        class: Some("unknown_class".into()),
    });

    assert!(result.is_err(), "invalid --class should return an error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("unknown_class") || msg.contains("class") || msg.contains("invalid"),
        "error message should mention the invalid class, got: {msg}"
    );
}

#[test]
fn ac6_cli_class_filter_via_binary() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            rerun_instance("always-resolved", 3, 3),
            rerun_instance("flaky", 3, 1),
        ],
    );

    let output = std::process::Command::new(binary_path())
        .args(["bench", "variance", "--sweep"])
        .arg(dir.path())
        .args(["--format", "json", "--class", "flaky"])
        .output()
        .expect("failed to run max binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let instances = val["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["instance_id"].as_str().unwrap(), "flaky");
}

#[test]
fn ac6_substring_filter() {
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![
            rerun_instance("task-foo-1", 3, 1),
            rerun_instance("task-bar-2", 3, 1),
        ],
    );

    let report = compute_variance(&BenchVarianceArgs {
        sweep_dir: dir.path().to_path_buf(),
        ci_width: None,
        filter: vec!["foo".into()],
        class: None,
    })
    .unwrap();

    assert_eq!(report.instances.len(), 1);
    assert_eq!(report.instances[0].instance_id, "task-foo-1");
}

#[test]
fn ac4_invalid_ci_width_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![rerun_instance("a", 3, 2)]);

    for bad in &[0.0_f64, -0.1, f64::NAN, f64::INFINITY] {
        let result = compute_variance(&BenchVarianceArgs {
            sweep_dir: dir.path().to_path_buf(),
            ci_width: Some(*bad),
            filter: vec![],
            class: None,
        });
        assert!(result.is_err(), "expected error for ci_width={bad}, got ok");
    }
}

// ── AC7: zero cost — reads only on-disk artifacts ─────────────────────────────

#[test]
fn ac7_cli_single_slot_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    write_results(dir.path(), vec![rerun_instance("a", 1, 1)]);

    let output = std::process::Command::new(binary_path())
        .args(["bench", "variance", "--sweep"])
        .arg(dir.path())
        .output()
        .expect("failed to run max binary");

    assert!(
        !output.status.success(),
        "single-slot sweep must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("single")
            || stderr.contains("rerun")
            || stderr.contains("slot")
            || stderr.contains("runs"),
        "error message should mention single-slot / rerun requirement, got: {stderr}"
    );
}

#[test]
fn ac7_reads_no_model_or_network() {
    // Verify: the command completes successfully without any model API key set
    let dir = tempfile::tempdir().unwrap();
    write_results(
        dir.path(),
        vec![rerun_instance("a", 3, 3), rerun_instance("b", 3, 1)],
    );

    // Deliberately unset ANTHROPIC_API_KEY so any model call would fail
    let output = std::process::Command::new(binary_path())
        .args(["bench", "variance", "--sweep"])
        .arg(dir.path())
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("LITELLM_KEY")
        .output()
        .expect("failed to run max binary");

    assert!(
        output.status.success(),
        "variance must succeed without any API key (zero cost), stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
