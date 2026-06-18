//! `bench budget-fit`: fixture tests covering all six AC items from issue #268.
//!
//! Red → Green → Refactor TDD cycle.
//!
//! AC items covered:
//! (a) resolved ≤40 steps, cap=80 → recommended_cap=40, tightening impact
//! (b) 8/10 at step_limit with write-class → recommend raise, delta>0
//! (c) 8/10 at step_limit with noop/read-class → no raise recommendation
//! (d) no wall-clock timeout → wall_clock recommended_cap=null, others still produce
//! (e) determinism: byte-identical JSON modulo generated_at
//! (f) --target-percentile 50 materially changes recommendation vs default 95

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
use std::process::Command;

use maxwells_daemon::run::budget_fit::{BudgetFitArgs, compute_budget_fit};
use maxwells_daemon::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest, HarnessManifest, InstanceResult, ModelManifest,
    PromptTemplateManifest, ProvenanceManifest, RuntimeManifest, SweepResults,
};
use maxwells_daemon::trajectory::FailureCategory;
use maxwells_daemon::trajectory::outcome;

mod support;
use support::binary_path;

// ── fixture builders ───────────────────────────────────────────────────────

fn resolved_instance(id: &str, steps: u32, cost_usd: f64, duration_secs: f64) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(steps),
        cost_usd: Some(cost_usd),
        prompt_tokens: Some(500),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(100),
        duration_secs: Some(duration_secs),
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
        trace_id: None,
    }
}

fn cap_bound_instance(
    id: &str,
    cat: FailureCategory,
    steps: u32,
    cost_usd: f64,
    duration_secs: f64,
) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "error".into(),
        outcome: Some(outcome::ERROR.into()),
        failure_category: Some(cat),
        steps: Some(steps),
        cost_usd: Some(cost_usd),
        prompt_tokens: Some(800),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(200),
        duration_secs: Some(duration_secs),
        error: Some("step limit reached".into()),
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
        trace_id: None,
    }
}

fn unresolved_other_instance(id: &str, steps: u32, cost_usd: f64) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "error".into(),
        outcome: Some(outcome::ERROR.into()),
        failure_category: Some(FailureCategory::EnvSetup),
        steps: Some(steps),
        cost_usd: Some(cost_usd),
        prompt_tokens: Some(400),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(80),
        duration_secs: Some(5.0),
        error: Some("env setup failed".into()),
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
        trace_id: None,
    }
}

fn make_manifest(step_limit: Option<u32>, task_timeout_secs: Option<u64>) -> ProvenanceManifest {
    let mut argv = vec!["max".into(), "bench".into(), "swebench".into()];
    if let Some(sl) = step_limit {
        argv.push("--step-limit".into());
        argv.push(sl.to_string());
    }
    if let Some(tt) = task_timeout_secs {
        argv.push("--task-timeout-secs".into());
        argv.push(tt.to_string());
    }
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
            instance_count: 10,
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

fn write_results(dir: &Path, instances: Vec<InstanceResult>, manifest: ProvenanceManifest) {
    let total = instances.len();
    let submitted = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
        .count();
    let errored = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
        .count();
    let failures_by_category: BTreeMap<FailureCategory, usize> = {
        let mut m = BTreeMap::new();
        for inst in &instances {
            if let Some(c) = inst.failure_category {
                *m.entry(c).or_insert(0) += 1;
            }
        }
        m
    };
    let total_cost: f64 = instances.iter().filter_map(|i| i.cost_usd).sum();
    let resolved = instances.iter().filter(|r| r.resolved_count > 0).count();
    let pass_at_k = if total > 0 {
        resolved as f64 / total as f64
    } else {
        0.0
    };
    let sweep = SweepResults {
        total,
        sweep_status: maxwells_daemon::run::swebench::SWEEP_STATUS_COMPLETED.into(),
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
        estimated_cost_usd: total_cost,
        actual_cost_usd: Some(total_cost),
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

/// Write a minimal behavior.json with per-instance class counts.
/// `class_map` maps instance_id to dominant_class name.
fn write_behavior_json(dir: &Path, class_map: &[(&str, &str)]) {
    use serde_json::{Map, Value, json};
    let per_instance: Vec<Value> = class_map
        .iter()
        .map(|(id, cls)| {
            let mut cc = Map::new();
            cc.insert((*cls).to_string(), Value::from(10_u32));
            json!({
                "instance_id": id,
                "class_counts": cc
            })
        })
        .collect();
    let report = json!({
        "sweep": dir.to_string_lossy(),
        "generated_at": "2026-05-01T00:10:00Z",
        "taxonomy_version": 1,
        "totals": Map::new(),
        "by_outcome": Map::new(),
        "comparisons": {
            "shape_deltas": [],
            "dominant_delta_class": null
        },
        "per_instance": per_instance,
        "unclassified_heads": Map::new()
    });
    std::fs::write(
        dir.join("behavior.json"),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .unwrap();
}

// ── AC (a): resolved ≤40 steps, cap=80 → recommended=40, tightening impact ─

#[test]
fn test_a_resolved_under_cap_recommends_tighten() {
    let dir = tempfile::tempdir().unwrap();
    // 10 resolved instances, all finishing in ≤40 steps
    let steps_vals: Vec<u32> = vec![10, 15, 20, 25, 30, 32, 35, 38, 39, 40];
    let instances: Vec<InstanceResult> = steps_vals
        .iter()
        .enumerate()
        .map(|(i, &s)| resolved_instance(&format!("inst-{i:02}"), s, 0.05, f64::from(s) * 2.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(80), None));

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    let steps_axis = report.axes.iter().find(|a| a.axis_name == "steps").unwrap();

    // P95 of [10,15,20,25,30,32,35,38,39,40] rounded up = 40
    assert_eq!(
        steps_axis.recommended_cap,
        Some(40.0),
        "P95 of resolved should be 40"
    );
    assert!(
        steps_axis.configured_cap == Some(80.0),
        "configured cap should be 80"
    );
    // Impact of tightening from 80 to 40: cost savings, ≤0 resolved_delta
    let impact = steps_axis
        .projected_impact_if_recommended
        .as_ref()
        .expect("tightening impact must be present");
    assert!(
        impact.estimated_resolved_delta <= 0,
        "tightening should not gain resolved instances"
    );
    assert!(
        impact.estimated_cost_delta_usd <= 0.0,
        "tightening should not increase cost (got {})",
        impact.estimated_cost_delta_usd
    );
}

// ── AC (b): 8/10 at step_limit, write-class → raise recommendation ──────────

#[test]
fn test_b_write_class_cap_bound_recommends_raise() {
    let dir = tempfile::tempdir().unwrap();
    // 2 resolved + 8 at step_limit
    let mut instances = vec![
        resolved_instance("inst-00", 20, 0.05, 40.0),
        resolved_instance("inst-01", 25, 0.06, 50.0),
    ];
    for i in 0..8 {
        instances.push(cap_bound_instance(
            &format!("inst-cap-{i:02}"),
            FailureCategory::StepLimit,
            30,
            0.10,
            60.0,
        ));
    }
    write_results(dir.path(), instances, make_manifest(Some(30), Some(120)));
    // behavior.json with write-class for all cap-bound instances
    let class_map: Vec<(&str, &str)> = (0..8)
        .map(|i| {
            (
                Box::leak(format!("inst-cap-{i:02}").into_boxed_str()) as &str,
                "write",
            )
        })
        .collect();
    write_behavior_json(dir.path(), &class_map);

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    let steps_axis = report.axes.iter().find(|a| a.axis_name == "steps").unwrap();

    // Should recommend raising the cap (recommended_cap > configured_cap)
    assert!(
        steps_axis.recommended_cap.unwrap_or(0.0) > steps_axis.configured_cap.unwrap_or(f64::MAX),
        "recommended_cap ({:?}) should exceed configured_cap ({:?}) when write-class instances hit the cap",
        steps_axis.recommended_cap,
        steps_axis.configured_cap
    );
    // Projected impact should show positive resolved delta
    let impact = steps_axis
        .projected_impact_if_recommended
        .as_ref()
        .expect("impact must be present");
    assert!(
        impact.estimated_resolved_delta > 0,
        "expected positive resolved delta when write-class cap-bound instances exist, got {}",
        impact.estimated_resolved_delta
    );
    // Rationale should mention raising
    assert!(
        steps_axis
            .recommended_cap_rationale
            .to_lowercase()
            .contains("raise")
            || steps_axis
                .recommended_cap_rationale
                .to_lowercase()
                .contains("raising"),
        "rationale should mention raising: {}",
        steps_axis.recommended_cap_rationale
    );
}

// ── AC (c): 8/10 at step_limit, noop/read-class → no raise ─────────────────

#[test]
fn test_c_stuck_class_cap_bound_does_not_recommend_raise() {
    let dir = tempfile::tempdir().unwrap();
    let mut instances = vec![
        resolved_instance("inst-00", 20, 0.05, 40.0),
        resolved_instance("inst-01", 25, 0.06, 50.0),
    ];
    for i in 0..8 {
        instances.push(cap_bound_instance(
            &format!("inst-cap-{i:02}"),
            FailureCategory::StepLimit,
            30,
            0.10,
            60.0,
        ));
    }
    write_results(dir.path(), instances, make_manifest(Some(30), Some(120)));
    // behavior.json with noop-class for all cap-bound instances
    let class_map: Vec<(&str, &str)> = (0..8)
        .map(|i| {
            (
                Box::leak(format!("inst-cap-{i:02}").into_boxed_str()) as &str,
                "noop",
            )
        })
        .collect();
    write_behavior_json(dir.path(), &class_map);

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    let steps_axis = report.axes.iter().find(|a| a.axis_name == "steps").unwrap();

    // Should NOT recommend raising (recommended_cap should be ≤ configured_cap)
    let recommended = steps_axis.recommended_cap.unwrap_or(0.0);
    let configured = steps_axis.configured_cap.unwrap_or(f64::MAX);
    assert!(
        recommended <= configured,
        "should not recommend raising cap when noop-class instances hit the cap: recommended={recommended}, configured={configured}"
    );
    // Projected impact should NOT show positive delta from raising
    let impact = steps_axis
        .projected_impact_if_recommended
        .as_ref()
        .expect("impact must be present");
    assert!(
        impact.estimated_resolved_delta <= 0,
        "noop-class cap-bound should not yield positive resolved delta, got {}",
        impact.estimated_resolved_delta
    );
    // Rationale should mention that raising is not recommended
    let rationale_lower = steps_axis.recommended_cap_rationale.to_lowercase();
    assert!(
        rationale_lower.contains("stuck")
            || rationale_lower.contains("noop")
            || rationale_lower.contains("not recommended")
            || rationale_lower.contains("unlikely"),
        "rationale should indicate raising is not helpful: {}",
        steps_axis.recommended_cap_rationale
    );
}

// ── AC (d): no wall-clock timeout → wall_clock recommended_cap=null ─────────

#[test]
fn test_d_no_wall_clock_cap_emits_null_recommendation() {
    let dir = tempfile::tempdir().unwrap();
    // No task_timeout_secs in manifest
    let instances: Vec<InstanceResult> = (0..5)
        .map(|i| resolved_instance(&format!("inst-{i}"), 15, 0.03, 30.0))
        .collect();
    write_results(
        dir.path(),
        instances,
        make_manifest(Some(40), None /* no wall-clock cap */),
    );

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    let wc_axis = report
        .axes
        .iter()
        .find(|a| a.axis_name == "wall_clock_s")
        .expect("wall_clock_s axis must be present even without cap");

    assert_eq!(
        wc_axis.configured_cap, None,
        "wall_clock configured_cap should be null when no timeout configured"
    );
    assert_eq!(
        wc_axis.recommended_cap, None,
        "wall_clock recommended_cap should be null when no cap configured"
    );
    assert!(
        wc_axis
            .recommended_cap_rationale
            .to_lowercase()
            .contains("no cap"),
        "rationale should note no cap configured: {}",
        wc_axis.recommended_cap_rationale
    );

    // Other axes (steps) should still have recommendations
    let steps_axis = report.axes.iter().find(|a| a.axis_name == "steps").unwrap();
    assert!(
        steps_axis.recommended_cap.is_some(),
        "steps axis should still produce a recommendation"
    );
}

// ── AC (e): determinism ──────────────────────────────────────────────────────

#[test]
fn test_e_determinism_byte_identical_modulo_generated_at() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..10)
        .map(|i| {
            if i < 8 {
                resolved_instance(
                    &format!("inst-{i:02}"),
                    10 + i * 2,
                    0.05,
                    20.0 + f64::from(i),
                )
            } else {
                cap_bound_instance(
                    &format!("inst-cap-{i}"),
                    FailureCategory::StepLimit,
                    40,
                    0.10,
                    80.0,
                )
            }
        })
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(40), Some(90)));

    let args = BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    };

    let report1 = compute_budget_fit(&args).unwrap();
    let report2 = compute_budget_fit(&args).unwrap();

    // Serialize both, zero out generated_at, compare
    let json1 = serde_json::to_value(&report1).unwrap();
    let json2 = serde_json::to_value(&report2).unwrap();

    fn strip_generated_at(mut v: serde_json::Value) -> serde_json::Value {
        if let serde_json::Value::Object(ref mut m) = v {
            m.remove("generated_at");
        }
        v
    }

    assert_eq!(
        strip_generated_at(json1),
        strip_generated_at(json2),
        "two runs over the same sweep should produce identical JSON modulo generated_at"
    );
}

// ── AC (f): --target-percentile 50 changes recommendation ────────────────────

#[test]
fn test_f_target_percentile_changes_recommendation() {
    let dir = tempfile::tempdir().unwrap();
    // Wide step distribution so p50 and p95 differ materially
    let steps_vals: Vec<u32> = vec![5, 8, 10, 12, 15, 20, 25, 30, 38, 40];
    let instances: Vec<InstanceResult> = steps_vals
        .iter()
        .enumerate()
        .map(|(i, &s)| resolved_instance(&format!("inst-{i:02}"), s, 0.05, f64::from(s) * 2.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(80), None));

    let report_p95 = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    let report_p50 = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 50,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    let steps_p95 = report_p95
        .axes
        .iter()
        .find(|a| a.axis_name == "steps")
        .unwrap()
        .recommended_cap;
    let steps_p50 = report_p50
        .axes
        .iter()
        .find(|a| a.axis_name == "steps")
        .unwrap()
        .recommended_cap;

    assert!(
        steps_p50 < steps_p95,
        "--target-percentile 50 should produce a materially lower recommendation than p95: p50={steps_p50:?}, p95={steps_p95:?}"
    );
}

// ── CLI integration: subcommand in --help ────────────────────────────────────

#[test]
fn bench_budget_fit_in_help() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .expect("failed to run bench --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("budget-fit"),
        "bench --help should list budget-fit subcommand"
    );
}

// ── CLI integration: writes budget-fit.json and exits 0 ─────────────────────

#[test]
fn bench_budget_fit_cli_writes_artifact_and_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..5)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10 + i * 3, 0.04, 20.0 + f64::from(i)))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(60), Some(120)));

    let out = Command::new(binary_path())
        .args([
            "bench",
            "budget-fit",
            "--sweep",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to run bench budget-fit");

    assert!(
        out.status.success(),
        "bench budget-fit should exit 0 on success; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.path().join("budget-fit.json").exists(),
        "budget-fit.json should be written"
    );
}

// ── CLI integration: --format json prints valid JSON ─────────────────────────

#[test]
fn bench_budget_fit_format_json_prints_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..5)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10 + i * 3, 0.04, 20.0 + f64::from(i)))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(60), Some(120)));

    let out = Command::new(binary_path())
        .args([
            "bench",
            "budget-fit",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run bench budget-fit --format json");

    assert!(out.status.success());
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--format json output should be valid JSON");
    assert!(
        parsed.get("axes").is_some(),
        "JSON output should contain 'axes'"
    );
    assert!(
        parsed.get("summary").is_some(),
        "JSON output should contain 'summary'"
    );
}

// ── CLI integration: no instances at cap is exit 0 (finding, not error) ──────

#[test]
fn bench_budget_fit_no_cap_failures_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    // All resolved, none near the cap
    let instances: Vec<InstanceResult> = (0..10)
        .map(|i| resolved_instance(&format!("inst-{i}"), 5 + i as u32, 0.02, 10.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(100), None));

    let out = Command::new(binary_path())
        .args([
            "bench",
            "budget-fit",
            "--sweep",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to run bench budget-fit");

    assert!(
        out.status.success(),
        "no-cap-failures is a finding, not an error; exit code should be 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── CLI integration: --axis restricts output to one axis ─────────────────────

#[test]
fn bench_budget_fit_axis_flag_restricts_output() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..5)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10 + i as u32, 0.04, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(60), Some(120)));

    let out = Command::new(binary_path())
        .args([
            "bench",
            "budget-fit",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
            "--axis",
            "steps",
        ])
        .output()
        .expect("failed to run bench budget-fit --axis steps");

    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let axes = parsed["axes"].as_array().expect("axes must be an array");
    assert_eq!(
        axes.len(),
        1,
        "--axis steps should produce exactly one axis"
    );
    assert_eq!(axes[0]["axis_name"], "steps");
}

// ── waste_estimate_usd includes step-limit costs ─────────────────────────────
//
// Regression guard: prior implementation only counted cost_limit cap-bound
// instances; step_limit and wallclock_timeout failures were silently excluded.

#[test]
fn waste_estimate_usd_includes_step_limit_costs() {
    let dir = tempfile::tempdir().unwrap();
    // 2 resolved + 4 step_limit cap-bound (cost_usd = 0.10 each) + 2 unresolved_other (env_setup)
    let mut instances = vec![
        resolved_instance("res-0", 15, 0.05, 30.0),
        resolved_instance("res-1", 20, 0.06, 40.0),
    ];
    for i in 0..4 {
        instances.push(cap_bound_instance(
            &format!("sl-{i}"),
            FailureCategory::StepLimit,
            30,
            0.10,
            60.0,
        ));
    }
    for i in 0..2 {
        instances.push(unresolved_other_instance(&format!("env-{i}"), 5, 0.01));
    }
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    // 4 step_limit instances each cost $0.10 → waste = $0.40
    assert!(
        (report.summary.waste_estimate_usd - 0.40).abs() < 1e-9,
        "waste_estimate_usd should equal total cost of step-limit cap-bound instances (4 × $0.10 = $0.40), got {}",
        report.summary.waste_estimate_usd
    );
}

// ── mixed per-task cost caps are rejected ─────────────────────────────────────

#[test]
fn mixed_cost_caps_in_resolved_config_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();

    // Manifest with BOTH agent.per_task_budget_usd AND agent.cost_limit_usd set.
    let manifest = ProvenanceManifest {
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
            // Both cost caps set — ambiguous failure categories
            resolved:
                "[agent]\nstep_limit = 30\nper_task_budget_usd = 0.10\ncost_limit_usd = 0.08\n"
                    .into(),
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
        cli: CliManifest {
            argv: vec!["max".into(), "bench".into(), "swebench".into()],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    };
    write_results(dir.path(), instances, manifest);

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "sweep with both agent.per_task_budget_usd and agent.cost_limit_usd should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("per_task_budget_usd") && msg.contains("cost_limit_usd"),
        "error should name both conflicting config keys: {msg}"
    );
}

// ── per_task_budget_usd from resolved config TOML ────────────────────────────

#[test]
fn per_task_budget_usd_read_from_resolved_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    // 3 instances stopped by BudgetExhausted (per-task budget), 2 resolved.
    let mut instances: Vec<InstanceResult> = (0..2)
        .map(|i| resolved_instance(&format!("res-{i}"), 10, 0.05, 20.0))
        .collect();
    for i in 0..3 {
        instances.push(cap_bound_instance(
            &format!("budget-{i}"),
            FailureCategory::BudgetExhausted,
            20,
            0.15,
            30.0,
        ));
    }

    // Manifest with NO --per-task-budget-usd in argv; value in resolved TOML.
    let manifest = ProvenanceManifest {
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
            instance_count: 5,
            filter_spec: None,
            ..Default::default()
        },
        prompt_template: PromptTemplateManifest {
            source: "inline".into(),
            path: None,
            sha256: "tpl123".into(),
        },
        config: ConfigManifest {
            resolved: "[agent]\nstep_limit = 30\nper_task_budget_usd = 0.10\n".into(),
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
        cli: CliManifest {
            argv: vec!["max".into(), "bench".into(), "swebench".into()],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    };
    write_results(dir.path(), instances, manifest);

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: Some("cost_usd".into()),
        filter: vec![],
    })
    .unwrap();

    let cost_axis = report
        .axes
        .iter()
        .find(|a| a.axis_name == "cost_usd")
        .unwrap();
    assert_eq!(
        cost_axis.configured_cap,
        Some(0.10),
        "should read per_task_budget_usd=0.10 from manifest.config.resolved TOML"
    );
    assert_eq!(
        cost_axis.configured_cap_source.as_deref(),
        Some("manifest.config.resolved[agent.per_task_budget_usd]"),
        "cap source should point to resolved config"
    );
    // BudgetExhausted instances should be counted as cost cap-bound
    let cap_bound = cost_axis
        .distribution_by_outcome
        .get("unresolved_cap_bound")
        .expect("unresolved_cap_bound bucket must exist");
    assert_eq!(
        cap_bound.count, 3,
        "3 BudgetExhausted instances should be bucketed as cost cap-bound"
    );
}

// ── rerun sweeps (runs > 1) are rejected ──────────────────────────────────────

#[test]
fn rerun_sweep_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    // Build a normal instance then manually set runs > 1 in the JSON.
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    // Patch the first instance to have runs = 2 (instances is a JSON array).
    let path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    if let Some(first) = val["instances"].as_array_mut().and_then(|a| a.first_mut()) {
        first["runs"] = serde_json::json!(2);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(result.is_err(), "rerun sweep (runs > 1) should be rejected");
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("rerun") || msg.contains("runs"),
        "error should mention reruns: {msg}"
    );
}

// ── model-changing retry is rejected ─────────────────────────────────────────

#[test]
fn model_changing_retry_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    // Patch in retry_history with a model override (caps unchanged).
    let path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-model",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "model": "claude-haiku-4-5" },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 2,
        "pre_errored": 1,
        "pre_resolved_count": 2,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "retry that changes model should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("model") || msg.contains("retry"),
        "error should mention model: {msg}"
    );
}

// ── step cap from resolved manifest config (not just argv) ───────────────────

#[test]
fn step_limit_read_from_resolved_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    // Manifest with NO --step-limit in argv but step_limit = 45 in resolved TOML.
    let instances: Vec<InstanceResult> = (0..5)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10 + i as u32 * 3, 0.05, 20.0))
        .collect();

    // Build manifest without --step-limit in argv; put the value in resolved TOML.
    let manifest = ProvenanceManifest {
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
            instance_count: 5,
            filter_spec: None,
            ..Default::default()
        },
        prompt_template: PromptTemplateManifest {
            source: "inline".into(),
            path: None,
            sha256: "tpl123".into(),
        },
        config: ConfigManifest {
            // TOML containing agent.step_limit = 45 (no --step-limit in argv)
            resolved: "[agent]\nstep_limit = 45\n".into(),
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
        cli: CliManifest {
            // No --step-limit in argv
            argv: vec!["max".into(), "bench".into(), "swebench".into()],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    };
    write_results(dir.path(), instances, manifest);

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: Some("steps".into()),
        filter: vec![],
    })
    .unwrap();

    let steps_axis = report.axes.iter().find(|a| a.axis_name == "steps").unwrap();
    assert_eq!(
        steps_axis.configured_cap,
        Some(45.0),
        "should read step_limit=45 from manifest.config.resolved TOML when not in argv"
    );
    assert_eq!(
        steps_axis.configured_cap_source.as_deref(),
        Some("manifest.config.resolved[agent.step_limit]"),
        "cap source should point to resolved config"
    );
}

// ── empty evaluation.json is treated as an incompatible artifact ──────────────

#[test]
fn empty_evaluation_json_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    // Write evaluation.json with instances: [] — all sweep instances are uncovered.
    let eval_json = serde_json::json!({
        "sweep": dir.path().to_string_lossy(),
        "generated_at": "2026-05-01T00:10:00Z",
        "instances": []
    });
    std::fs::write(
        dir.path().join("evaluation.json"),
        serde_json::to_string_pretty(&eval_json).unwrap(),
    )
    .unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "empty evaluation.json on a non-empty sweep should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("missing") || msg.contains("stale") || msg.contains("partial"),
        "error should describe the incompatible artifact: {msg}"
    );
}

// ── unknown filter key is a usage error ──────────────────────────────────────

#[test]
fn unknown_filter_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        // typo: "failure_catgory" instead of "failure_category"
        filter: vec!["failure_catgory=step_limit".into()],
    });

    assert!(
        result.is_err(),
        "unknown filter key should be a usage error, not silently ignored"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("failure_catgory") || msg.contains("recognised") || msg.contains("known"),
        "error should name the bad key: {msg}"
    );
}

// ── retry that only raises sweep budget is accepted ──────────────────────────

#[test]
fn retry_with_only_sweep_budget_change_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    // Original manifest has NO explicitly configured per-instance caps in argv.
    // When the retry also omits them from override_delta, retry_swebench_args runs at
    // the same CLI defaults as the original — no silent cap reset.
    write_results(dir.path(), instances, make_manifest(None, None));

    // Patch in a retry_history entry that only changes sweep_cost_limit_usd.
    // Include all required RetryHistoryEntry fields so load_sweep can deserialize.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-1",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 20.0 },
        "count": 3,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_ok(),
        "retry that only changes sweep_cost_limit_usd should be accepted; got: {:?}",
        result.unwrap_err()
    );
}

// ── unit: distribution stats ─────────────────────────────────────────────────

#[test]
fn distribution_stats_empty_returns_zero_count() {
    use maxwells_daemon::run::budget_fit::compute_distribution;
    let stats = compute_distribution(&[]);
    assert_eq!(stats.count, 0);
    assert!(stats.p95.is_none());
    assert!(stats.mean.is_none());
}

#[test]
fn distribution_stats_single_value() {
    use maxwells_daemon::run::budget_fit::compute_distribution;
    let stats = compute_distribution(&[42.0]);
    assert_eq!(stats.count, 1);
    assert_eq!(stats.p95, Some(42.0));
    assert_eq!(stats.mean, Some(42.0));
    assert_eq!(stats.max, Some(42.0));
}

#[test]
fn distribution_p95_matches_expected() {
    use maxwells_daemon::run::budget_fit::compute_distribution;
    // 10 values: [10,15,20,25,30,32,35,38,39,40]
    let vals: Vec<f64> = vec![10.0, 15.0, 20.0, 25.0, 30.0, 32.0, 35.0, 38.0, 39.0, 40.0];
    let stats = compute_distribution(&vals);
    // P95 index = 0.95 * 9 = 8.55 → lo=8(39), hi=9(40), frac=0.55 → 39 + 0.55 = 39.55
    let p95: f64 = stats.p95.expect("p95 should be Some");
    assert!(
        (p95 - 39.55_f64).abs() < 0.01,
        "p95 should be ~39.55, got {p95}"
    );
}

// ── tied behavior class leaves instance unclassified ─────────────────────────
//
// When two action classes have equal counts in behavior.json, budget-fit should
// not count the instance as either progress or stuck. An arbitrary tiebreaker
// would incorrectly trigger a raise or stuck recommendation.

#[test]
fn tied_behavior_class_is_not_classified() {
    use serde_json::{Map, Value, json};

    let dir = tempfile::tempdir().unwrap();
    // 2 resolved + 6 cap-bound at step_limit
    let mut instances = vec![
        resolved_instance("res-0", 10, 0.02, 20.0),
        resolved_instance("res-1", 12, 0.02, 24.0),
    ];
    for i in 0..6 {
        instances.push(cap_bound_instance(
            &format!("cap-{i}"),
            FailureCategory::StepLimit,
            30,
            0.10,
            60.0,
        ));
    }
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    // behavior.json with tied classes (write: 5, noop: 5) for all cap-bound instances.
    let tied_class_counts = {
        let mut cc = Map::new();
        cc.insert("write".into(), Value::from(5_u32));
        cc.insert("noop".into(), Value::from(5_u32));
        cc
    };
    let per_instance: Vec<Value> = (0..6)
        .map(|i| json!({ "instance_id": format!("cap-{i}"), "class_counts": &tied_class_counts }))
        .collect();
    let report = json!({
        "sweep": dir.path().to_string_lossy(),
        "generated_at": "2026-05-01T00:10:00Z",
        "taxonomy_version": 1,
        "totals": Map::new(),
        "by_outcome": Map::new(),
        "comparisons": { "shape_deltas": [], "dominant_delta_class": null },
        "per_instance": per_instance,
        "unclassified_heads": Map::new()
    });
    std::fs::write(
        dir.path().join("behavior.json"),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: Some("steps".into()),
        filter: vec![],
    })
    .unwrap();

    let steps_axis = result.axes.iter().find(|a| a.axis_name == "steps").unwrap();
    // Tied instances are unclassified → no progress or stuck majority → no raise recommendation.
    let rec = steps_axis.recommended_cap.unwrap_or(0.0);
    assert!(
        rec <= steps_axis.configured_cap.unwrap_or(f64::MAX),
        "tied behavior class should not trigger a raise recommendation: recommended={rec}"
    );
    let rationale = steps_axis.recommended_cap_rationale.to_lowercase();
    assert!(
        !rationale.contains("raising") || !rationale.contains("progress"),
        "tied class should not produce a progress-based raise rationale: {rationale}"
    );
}

// ── progress-class cap hits are excluded from waste_estimate_usd ──────────────
//
// When behavior.json is present, progress-class cap-bound instances represent
// under-provisioned work (not wasted spend), so they should not contribute to
// waste_estimate_usd.  Only stuck/unclassified instances are conservative waste.

#[test]
fn progress_class_cap_hits_excluded_from_waste() {
    let dir = tempfile::tempdir().unwrap();
    let mut instances = vec![resolved_instance("res-0", 10, 0.02, 20.0)];
    // 2 progress-class (write) cap-bound instances, cost $0.10 each
    for i in 0..2 {
        instances.push(cap_bound_instance(
            &format!("prog-{i}"),
            FailureCategory::StepLimit,
            30,
            0.10,
            60.0,
        ));
    }
    // 2 stuck-class (noop) cap-bound instances, cost $0.10 each
    for i in 0..2 {
        instances.push(cap_bound_instance(
            &format!("stuck-{i}"),
            FailureCategory::StepLimit,
            30,
            0.10,
            60.0,
        ));
    }
    write_results(dir.path(), instances, make_manifest(Some(30), None));
    // behavior.json: prog-* → write, stuck-* → noop
    let class_map: &[(&str, &str)] = &[
        ("prog-0", "write"),
        ("prog-1", "write"),
        ("stuck-0", "noop"),
        ("stuck-1", "noop"),
    ];
    write_behavior_json(dir.path(), class_map);

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    // Only the 2 stuck instances ($0.10 each = $0.20) contribute to waste.
    // The 2 progress instances ($0.20 total) should be excluded.
    let waste = report.summary.waste_estimate_usd;
    assert!(
        (waste - 0.20).abs() < 1e-9,
        "waste should be $0.20 (only stuck instances); got {waste}"
    );
}

// ── stuck-class P-target at or above cap emits no recommendation ──────────────
//
// When stuck-class instances are the majority of cap-bound failures but P{target}
// of resolved is ≥ cap, no useful action exists: raising is bad (stuck), and
// tightening would cut resolved instances.  The recommendation should be None.

#[test]
fn stuck_class_ptarget_at_or_above_cap_emits_no_recommendation() {
    let dir = tempfile::tempdir().unwrap();
    // Resolved instances with wall-clock duration ABOVE the cap (can happen when the
    // run finishes and submits before the timeout fires).
    // Wall-clock cap = 60s, resolved durations = [70, 75], P95 = 75 ≥ 60.
    let mut instances = vec![
        resolved_instance("res-0", 10, 0.05, 70.0),
        resolved_instance("res-1", 12, 0.06, 75.0),
    ];
    // 8 wallclock-timeout, noop-class cap-bound instances at exactly 60s
    for i in 0..8 {
        instances.push(cap_bound_instance(
            &format!("wc-{i}"),
            FailureCategory::WallclockTimeout,
            25,
            0.10,
            60.0,
        ));
    }
    // task_timeout_secs = 60 → wall_clock configured_cap = 60
    write_results(dir.path(), instances, make_manifest(None, Some(60)));
    // All cap-bound instances have noop behavior (stuck)
    let class_map: Vec<(&str, &str)> = (0..8)
        .map(|i| {
            (
                Box::leak(format!("wc-{i}").into_boxed_str()) as &str,
                "noop",
            )
        })
        .collect();
    write_behavior_json(dir.path(), &class_map);

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: Some("wall_clock_s".into()),
        filter: vec![],
    })
    .unwrap();

    let wc_axis = report
        .axes
        .iter()
        .find(|a| a.axis_name == "wall_clock_s")
        .unwrap();
    // P95 of [70, 75] = 75 ≥ cap (60) AND stuck-class → no recommendation
    assert!(
        wc_axis.recommended_cap.is_none(),
        "stuck-class with P-target ≥ cap should emit no recommendation; got {:?}",
        wc_axis.recommended_cap
    );
    let rationale = wc_axis.recommended_cap_rationale.to_lowercase();
    assert!(
        rationale.contains("well-sized") || rationale.contains("unlikely"),
        "rationale should note cap is well-sized and raising unlikely to help: {rationale}"
    );
}

// ── tightening savings use per-instance actual value, not cap reduction ────────
//
// For instances between new_cap and old_cap, savings = (actual - new_cap) units,
// not the full (old_cap - new_cap).  The old formula overestimated savings for
// instances that stopped before the cap.

#[test]
fn tightening_savings_use_actual_value_not_cap_reduction() {
    let dir = tempfile::tempdir().unwrap();
    // Resolved: steps [10, 15] with cost $0.010 and $0.015 (exactly $0.001/step)
    // Cap-bound: steps [20, 25, 30] with cost $0.020, $0.025, $0.030 ($0.001/step)
    // cap = 30, P95([10, 15]) = 15 → new_cap = 15
    let instances = vec![
        resolved_instance("res-0", 10, 0.010, 20.0),
        resolved_instance("res-1", 15, 0.015, 30.0),
        cap_bound_instance("cap-0", FailureCategory::StepLimit, 20, 0.020, 40.0),
        cap_bound_instance("cap-1", FailureCategory::StepLimit, 25, 0.025, 50.0),
        cap_bound_instance("cap-2", FailureCategory::StepLimit, 30, 0.030, 60.0),
    ];
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: Some("steps".into()),
        filter: vec![],
    })
    .unwrap();

    let steps_axis = report.axes.iter().find(|a| a.axis_name == "steps").unwrap();
    // P95 of [10, 15] = 15 → tightening recommended from 30 to 15
    assert_eq!(steps_axis.recommended_cap, Some(15.0));

    let impact = steps_axis
        .projected_impact_if_recommended
        .as_ref()
        .expect("tightening impact must be present");

    // mean_cost_per_unit = (0.010+0.015+0.020+0.025+0.030) / (10+15+20+25+30)
    //                     = 0.100 / 100 = 0.001 USD/step exactly.
    // Eligible instances (steps > 15): cap-0(20), cap-1(25), cap-2(30)
    // New formula saved_units = (min(20,30)-15) + (min(25,30)-15) + (min(30,30)-15)
    //                         = 5 + 10 + 15 = 30 steps
    // savings = 0.001 * 30 = $0.030
    // Old formula would give: (30-15) * 3 * 0.001 = $0.045
    let expected_savings = 0.030_f64;
    let actual_savings = -impact.estimated_cost_delta_usd; // delta is negative (savings)
    assert!(
        (actual_savings - expected_savings).abs() < 1e-6,
        "savings should be ${expected_savings:.6} (per-instance actual value); got ${actual_savings:.6}"
    );
}

// ── stale evaluation.json after retry is rejected ─────────────────────────────
//
// When a sweep has retry_history and evaluation.json is present, the evaluation
// may have been done before the retry; budget-fit should reject it as potentially
// stale so operators are not silently given pre-retry resolved bucketing.
// NOTE: original uses no configured caps in argv so the silent-cap-reset check
// does not fire; only the stale-eval detection should trigger here.

#[test]
fn stale_evaluation_after_retry_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    // No caps in argv so the silent-cap-reset check does not interfere.
    write_results(dir.path(), instances, make_manifest(None, None));

    // Patch in a non-cap-changing retry (sweep_cost_limit_usd only).
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-1",
        "timestamp_utc": "2026-05-01T00:10:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    // Write evaluation.json that predates the retry (stale).
    // Legacy format (no artifact_kind/schema_version header) accepted by load_evaluation_results.
    let eval_json = serde_json::json!({
        "provenance": {
            "backend": "sb-cli",
            "eval_ended_at": "2026-05-01T00:05:00Z"
        },
        "instances": [
            { "instance_id": "inst-0", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" },
            { "instance_id": "inst-1", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" },
            { "instance_id": "inst-2", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" }
        ]
    });
    std::fs::write(
        dir.path().join("evaluation.json"),
        serde_json::to_string_pretty(&eval_json).unwrap(),
    )
    .unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "evaluation.json with retry history should be rejected as potentially stale"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("stale") || msg.contains("retry"),
        "error should mention stale evaluation or retry history: {msg}"
    );
}

// ── CLI per-task-budget-usd + config cost_limit_usd is rejected ───────────────
//
// When --per-task-budget-usd is passed via CLI and agent.cost_limit_usd is set in
// the resolved config, DefaultAgent::step checks cost_limit_usd first and can fire
// CostLimit before BudgetExhausted on any given instance.  Budget-fit cannot use a
// single cap for the cost_usd axis in that scenario and must reject it.

#[test]
fn cli_per_task_budget_with_config_cost_limit_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();

    // Manifest: --per-task-budget-usd in argv, cost_limit_usd in resolved config.
    let manifest = ProvenanceManifest {
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
            // config has cost_limit_usd; CLI has --per-task-budget-usd (set below in argv)
            resolved: "[agent]\nstep_limit = 30\ncost_limit_usd = 0.08\n".into(),
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
        cli: CliManifest {
            argv: vec![
                "max".into(),
                "bench".into(),
                "swebench".into(),
                "--per-task-budget-usd".into(),
                "0.10".into(),
            ],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    };
    write_results(dir.path(), instances, manifest);

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "CLI --per-task-budget-usd with config agent.cost_limit_usd should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("per-task-budget-usd") || msg.contains("cost_limit_usd"),
        "error should mention the conflicting cost caps: {msg}"
    );
}

// ── behavior.json after a no-cap-change retry is accepted ────────────────────
//
// A freshly regenerated behavior.json after `bench behavior --per-instance` is
// a valid artifact.  Budget-fit cannot distinguish stale from fresh by timestamp
// alone so it accepts behavior.json regardless of retry_history, as long as the
// retry did not silently reset any originally-configured per-instance caps.
// Original manifest has NO caps in argv → no silent-cap-reset check fires.

#[test]
fn behavior_after_retry_without_cap_change_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    // No configured caps in argv → retry that omits them is not a silent reset.
    write_results(dir.path(), instances, make_manifest(None, None));

    // Patch in a retry that only changes sweep_cost_limit_usd (no cap fields).
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-1",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    // Write a valid behavior.json — should be accepted, not rejected.
    write_behavior_json(dir.path(), &[("inst-0", "write")]);

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_ok(),
        "behavior.json after a no-cap-change retry should be accepted; got: {:?}",
        result.unwrap_err()
    );
}

// ── retry that silently resets a configured cap is rejected ──────────────────
//
// retry_swebench_args rebuilds from CLI defaults, not from the original manifest.
// If the original run had --step-limit 30 in argv but the retry's override_delta
// omits step_limit, the retry will run at the CLI default (e.g. 100), silently
// changing the per-instance cap.  Budget-fit must reject this scenario.

#[test]
fn silent_cap_reset_retry_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    // Original manifest has --step-limit 30 in argv.
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    // Patch in a retry whose override_delta only changes sweep_cost_limit_usd.
    // step_limit is absent from the delta → retry runs at CLI default, not 30.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-silent-reset",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "retry that silently resets an originally-configured step cap should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("reset") || msg.contains("implicit") || msg.contains("retry"),
        "error should mention the implicit cap reset: {msg}"
    );
}

// ── well-sized cap rationale appears in headline (not "insufficient data") ────
//
// When the dominant axis has cap-bound failures but recommended_cap is None because
// P{target} >= cap (cap already well-sized), the headline should propagate the
// per-axis rationale rather than saying "insufficient data for recommendation".

#[test]
fn well_sized_cap_rationale_appears_in_headline() {
    let dir = tempfile::tempdir().unwrap();
    // Wall-clock cap = 60s. Resolved instances finish ABOVE the cap (70, 75).
    // 8 stuck-class wallclock-timeout cap-bound instances at 60s.
    // P95([70, 75]) = 75 >= 60 → recommended_cap = None with "well-sized" rationale.
    let mut instances = vec![
        resolved_instance("res-0", 10, 0.05, 70.0),
        resolved_instance("res-1", 12, 0.06, 75.0),
    ];
    for i in 0..8 {
        instances.push(cap_bound_instance(
            &format!("wc-{i}"),
            FailureCategory::WallclockTimeout,
            25,
            0.10,
            60.0,
        ));
    }
    write_results(dir.path(), instances, make_manifest(None, Some(60)));
    let class_map: Vec<(&str, &str)> = (0..8)
        .map(|i| {
            (
                Box::leak(format!("wc-{i}").into_boxed_str()) as &str,
                "noop",
            )
        })
        .collect();
    write_behavior_json(dir.path(), &class_map);

    let report = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    })
    .unwrap();

    let headline = report.summary.headline_recommendation.to_lowercase();
    assert!(
        !headline.contains("insufficient data"),
        "headline should not say 'insufficient data' when the cap is already well-sized: {headline}"
    );
    assert!(
        headline.contains("well-sized")
            || headline.contains("unlikely")
            || headline.contains("raising"),
        "headline should include the well-sized rationale: {headline}"
    );
}

// ── retry with same cap value in override_delta is accepted ──────────────────
//
// When an operator passes --step-limit 30 to bench retry (same as the original),
// bench retry records step_limit=30 in override_delta.  The presence of the field
// should not be treated as a cap change since the value is identical.

#[test]
fn retry_preserving_same_step_limit_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    // Original has --step-limit 30.
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    // Retry explicitly re-states the same step_limit (30 == original).
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-same-cap",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "step_limit": 30 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_ok(),
        "retry that re-states the same step_limit value should be accepted; got: {:?}",
        result.unwrap_err()
    );
}

// ── retry omitting CLI-default step-limit is accepted ────────────────────────
//
// When the original sweep used --step-limit 50 (= CLI default) and a retry omits
// step_limit from override_delta, retry_swebench_args rebuilds at Config::defaults()
// which also resolves to 50.  No actual cap change occurred.

#[test]
fn retry_omitting_default_step_limit_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    // Original has --step-limit 50 (the CLI default).
    write_results(dir.path(), instances, make_manifest(Some(50), None));

    // Retry only changes sweep_cost_limit_usd; step_limit absent from delta.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-default-cap",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_ok(),
        "retry omitting --step-limit when original used the CLI default (50) should be accepted; got: {:?}",
        result.unwrap_err()
    );
}

// ── config-sourced per-task budget missing from retry is rejected ─────────────
//
// When the original sweep's per-task budget cap came from manifest.config.resolved
// TOML (not from --per-task-budget-usd in argv) and the retry omits both --config
// and --per-task-budget-usd, Config::defaults() has no cost cap.  Budget-fit must
// reject this as a silent reset regardless of whether the cap was argv- or
// config-sourced.

#[test]
fn config_sourced_per_task_budget_silent_reset_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();

    // Manifest: NO --per-task-budget-usd in argv; cap comes from resolved TOML only.
    let manifest = ProvenanceManifest {
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
            resolved: "[agent]\nper_task_budget_usd = 0.10\n".into(),
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
        cli: CliManifest {
            // No --per-task-budget-usd in argv; cap comes from the config overlay.
            argv: vec!["max".into(), "bench".into(), "swebench".into()],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    };
    write_results(dir.path(), instances, manifest);

    // Retry omits per_task_budget_usd from override_delta.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-no-budget",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "retry that drops a config-sourced per_task_budget_usd should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("reset") || msg.contains("caps") || msg.contains("retry"),
        "error should mention the implicit cap reset: {msg}"
    );
}

// ── retry that re-states the same model is accepted ──────────────────────────
//
// When an operator explicitly passes --model X to bench retry and X is the same
// model the original sweep used, override_delta records a model field but the
// population is not mixed-model.  Budget-fit must accept this.

#[test]
fn retry_restating_same_model_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    // Original model: claude-opus-4-7 (matches make_manifest default).
    write_results(dir.path(), instances, make_manifest(None, None));

    // Retry explicitly re-states the same model.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-same-model",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "model": "claude-opus-4-7" },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_ok(),
        "retry that re-states the same model should be accepted; got: {:?}",
        result.unwrap_err()
    );
}

// ── multi-attempt sweep (attempts > 1) is rejected ────────────────────────────
//
// run_one accumulates cost_usd across all API-retry attempts while steps and
// duration_secs come from the terminal attempt only.  budget-fit must reject any
// sweep where at least one instance used more than one attempt.

#[test]
fn multi_attempt_sweep_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    // Patch the first instance to have attempts = 2.
    let path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    if let Some(first) = val["instances"].as_array_mut().and_then(|a| a.first_mut()) {
        first["attempts"] = serde_json::json!(2);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "sweep with attempts > 1 should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("attempts") || msg.contains("retry"),
        "error should mention multi-attempt instances: {msg}"
    );
}

// ── original with config overlays + retry is rejected ────────────────────────
//
// When the original sweep had non-empty overlay_paths in the manifest, a retry
// that omits --config may have run without those overlays.  OverrideDelta does
// not record config file usage, so budget-fit cannot verify preservation.

#[test]
fn original_config_overlay_with_retry_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();

    // Manifest with a non-empty overlay_paths.
    let manifest = ProvenanceManifest {
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
            resolved: "[agent]\nstep_limit = 30\n".into(),
            overlay_paths: vec!["custom_prompts.toml".into()],
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
        cli: CliManifest {
            argv: vec!["max".into(), "bench".into(), "swebench".into()],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    };
    write_results(dir.path(), instances, manifest);

    // Patch in a retry that doesn't change caps or model.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-no-config",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "sweep with config overlays + retry should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("overlay") || msg.contains("config") || msg.contains("retry"),
        "error should mention config overlays: {msg}"
    );
}

// ── original with docker environment + retry is rejected ─────────────────────
//
// When the original ran in docker and a retry exists, the retry may have run
// in a different environment because --env/--docker-image are not recorded in
// OverrideDelta.  budget-fit must reject the merged population.

#[test]
fn original_docker_environment_with_retry_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();

    // Manifest with docker in the resolved config.
    let manifest = ProvenanceManifest {
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
            resolved: "[agent]\nstep_limit = 30\n\n[environment]\nkind = \"docker\"\n".into(),
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
        cli: CliManifest {
            argv: vec!["max".into(), "bench".into(), "swebench".into()],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    };
    write_results(dir.path(), instances, manifest);

    // Patch in a retry.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-maybe-local",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "original with docker environment + retry should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("docker") || msg.contains("environment") || msg.contains("retry"),
        "error should mention environment: {msg}"
    );
}

// ── budget-halted instances are rejected ──────────────────────────────────────
//
// When a sweep hits --sweep-cost-limit-usd, tasks that never ran get
// exit_reason = "budget_halt" with no steps/cost/duration.  Including them
// in n_total would distort at-cap percentile analysis.

#[test]
fn budget_halted_sweep_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(Some(30), None));

    // Patch the first instance to look like a budget-halted (never-ran) row.
    let path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    if let Some(first) = val["instances"].as_array_mut().and_then(|a| a.first_mut()) {
        first["exit_reason"] = serde_json::json!("budget_halt");
    }
    std::fs::write(&path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "sweep with budget-halted instances should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("budget_halt") || msg.contains("budget"),
        "error should mention budget-halted rows: {msg}"
    );
}

// ── harness-mismatched retry entry is rejected ────────────────────────────────
//
// An entry in retry_history with harness_mismatch=true was produced by a
// different harness version and may have different agent-loop accounting or
// failure categorization.  Budget-fit must reject it.

#[test]
fn harness_mismatch_retry_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(None, None));

    // Patch in a retry entry with harness_mismatch=true.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-mismatch",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": true,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "retry with harness_mismatch=true should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("harness_mismatch") || msg.contains("harness"),
        "error should mention harness mismatch: {msg}"
    );
}

// ── fresh evaluation.json after retry is accepted ─────────────────────────────
//
// If evaluation.json was regenerated AFTER the last retry (generated_at >
// retry timestamp_utc), the eval reflects post-retry resolved state and
// budget-fit should accept it without a stale-eval error.

#[test]
fn fresh_evaluation_after_retry_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(None, None));

    // Retry happened at 00:05; eval was regenerated at 00:10 (fresh).
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-1",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    // evaluation.json generated AFTER the retry — this is a fresh eval.
    // Legacy format (no artifact_kind/schema_version header) accepted by load_evaluation_results.
    let eval_json = serde_json::json!({
        "provenance": {
            "backend": "sb-cli",
            "eval_ended_at": "2026-05-01T00:10:00Z"
        },
        "instances": [
            { "instance_id": "inst-0", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" },
            { "instance_id": "inst-1", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" },
            { "instance_id": "inst-2", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" }
        ]
    });
    std::fs::write(
        dir.path().join("evaluation.json"),
        serde_json::to_string_pretty(&eval_json).unwrap(),
    )
    .unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_ok(),
        "fresh evaluation.json (generated after retry) should be accepted: {:?}",
        result.unwrap_err()
    );
}

// ── resumed sweep is rejected ─────────────────────────────────────────────────
//
// A sweep run with --resume includes rows from a prior invocation whose caps,
// model, and config may differ.  Budget-fit cannot verify the prior run used
// identical settings, so it must reject any sweep where
// manifest.runtime.resume_mode = true.

#[test]
fn resumed_sweep_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();

    // Build a manifest with resume_mode = true.
    let mut manifest = make_manifest(None, None);
    manifest.runtime.resume_mode = true;

    write_results(dir.path(), instances, manifest);

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "sweep with resume_mode=true should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("resume") || msg.contains("resume_mode"),
        "error should mention resume or resume_mode: {msg}"
    );
}

// ── mixed cost caps accepted when --axis steps is used ────────────────────────
//
// When both agent.per_task_budget_usd and agent.cost_limit_usd are set,
// budget-fit normally rejects the sweep because the cost_usd axis cannot be
// analyzed against a single cap.  With --axis steps the cost axis is never
// produced, so the ambiguity is irrelevant and the run should succeed.

#[test]
fn mixed_cost_caps_accepted_with_non_cost_axis() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..5)
        .map(|i| resolved_instance(&format!("inst-{i}"), 20, 0.05, 30.0))
        .collect();

    // Manifest with both cost cap keys in the resolved config.
    let manifest = ProvenanceManifest {
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
            instance_count: 10,
            filter_spec: None,
            ..Default::default()
        },
        prompt_template: PromptTemplateManifest {
            source: "inline".into(),
            path: None,
            sha256: "tpl123".into(),
        },
        config: ConfigManifest {
            resolved: "[agent]\nper_task_budget_usd = 1.0\ncost_limit_usd = 0.5".into(),
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
        cli: CliManifest {
            argv: vec!["max".into(), "bench".into(), "swebench".into()],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    };
    write_results(dir.path(), instances, manifest);

    // Without --axis: rejected due to mixed cost caps.
    let result_no_axis = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });
    assert!(
        result_no_axis.is_err(),
        "mixed cost caps without --axis should be rejected"
    );

    // With --axis steps: accepted because cost_usd axis is not produced.
    let result_steps = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: Some("steps".into()),
        filter: vec![],
    });
    assert!(
        result_steps.is_ok(),
        "mixed cost caps with --axis steps should be accepted: {:?}",
        result_steps.unwrap_err()
    );
}

// ── none-backend evaluation.json is rejected ─────────────────────────────────
//
// evaluation.json produced with `bench evaluate --backend none` writes
// resolved=false for every submitted patch (presence check only).  Budget-fit
// must reject it to avoid silently treating all instances as unresolved.

#[test]
fn none_backend_evaluation_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(None, None));

    // Write evaluation.json with backend = "none".
    let eval_json = serde_json::json!({
        "sweep": dir.path().to_string_lossy(),
        "generated_at": "2026-05-01T00:05:00Z",
        "provenance": {
            "backend": "none"
        },
        "instances": [
            { "instance_id": "inst-0", "resolved_count": 0, "resolved": false, "tests_failed": [], "eval_exit_reason": "none" },
            { "instance_id": "inst-1", "resolved_count": 0, "resolved": false, "tests_failed": [], "eval_exit_reason": "none" },
            { "instance_id": "inst-2", "resolved_count": 0, "resolved": false, "tests_failed": [], "eval_exit_reason": "none" }
        ]
    });
    std::fs::write(
        dir.path().join("evaluation.json"),
        serde_json::to_string_pretty(&eval_json).unwrap(),
    )
    .unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "evaluation.json with backend=none should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("none") || msg.contains("backend"),
        "error should mention backend or none: {msg}"
    );
}

// ── stale behavior.json after retry is rejected ───────────────────────────────
//
// When behavior.json.generated_at predates the last retry's timestamp_utc,
// the action class counts reflect pre-retry instance states and can
// misclassify retried cap-bound rows.  Budget-fit should reject it.

#[test]
fn stale_behavior_after_retry_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(None, None));

    // Retry happened at 00:10; behavior.json was generated at 00:05 (stale).
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-1",
        "timestamp_utc": "2026-05-01T00:10:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    // behavior.json predates the retry.
    let behavior_json = serde_json::json!({
        "generated_at": "2026-05-01T00:05:00Z",
        "per_instance": [
            { "instance_id": "inst-0", "class_counts": { "write": 5, "read": 2 } },
            { "instance_id": "inst-1", "class_counts": { "write": 3, "noop": 1 } },
            { "instance_id": "inst-2", "class_counts": { "read": 4 } }
        ]
    });
    std::fs::write(
        dir.path().join("behavior.json"),
        serde_json::to_string_pretty(&behavior_json).unwrap(),
    )
    .unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "stale behavior.json (predates retry) should be rejected"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("behavior") || msg.contains("stale") || msg.contains("retry"),
        "error should mention behavior, stale, or retry: {msg}"
    );
}

// ── fresh behavior.json after retry is accepted ───────────────────────────────
//
// When behavior.json.generated_at is after the last retry's timestamp_utc,
// the action class counts are post-retry and the enrichment is valid.

#[test]
fn fresh_behavior_after_retry_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(None, None));

    // Retry happened at 00:05; behavior.json was generated at 00:10 (fresh).
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([{
        "retry_id": "retry-1",
        "timestamp_utc": "2026-05-01T00:05:00Z",
        "selection": {},
        "override_delta": { "sweep_cost_limit_usd": 30.0 },
        "count": 1,
        "harness_mismatch": false,
        "pre_submitted": 3,
        "pre_errored": 0,
        "pre_resolved_count": 3,
        "post_submitted": 3,
        "post_errored": 0,
        "post_resolved_count": 3
    }]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    // behavior.json generated AFTER the retry.
    let behavior_json = serde_json::json!({
        "generated_at": "2026-05-01T00:10:00Z",
        "per_instance": [
            { "instance_id": "inst-0", "class_counts": { "write": 5, "read": 2 } },
            { "instance_id": "inst-1", "class_counts": { "write": 3, "noop": 1 } },
            { "instance_id": "inst-2", "class_counts": { "read": 4 } }
        ]
    });
    std::fs::write(
        dir.path().join("behavior.json"),
        serde_json::to_string_pretty(&behavior_json).unwrap(),
    )
    .unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_ok(),
        "fresh behavior.json (generated after retry) should be accepted: {:?}",
        result.unwrap_err()
    );
}

// ── unparseable retry timestamp treated as stale eval ─────────────────────────
//
// If any retry history entry has a missing or unparseable timestamp_utc,
// the eval freshness check cannot be trusted and must treat the eval as stale.

#[test]
fn unparseable_retry_timestamp_treats_eval_as_stale() {
    let dir = tempfile::tempdir().unwrap();
    let instances: Vec<InstanceResult> = (0..3)
        .map(|i| resolved_instance(&format!("inst-{i}"), 10, 0.05, 20.0))
        .collect();
    write_results(dir.path(), instances, make_manifest(None, None));

    // One parseable retry at 00:05, one with a broken timestamp.
    // eval generated_at is 00:10 (after the parseable retry), but the broken
    // entry could represent a retry that happened even later — treat as stale.
    let results_path = dir.path().join("results.json");
    let text = std::fs::read_to_string(&results_path).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&text).unwrap();
    val["retry_history"] = serde_json::json!([
        {
            "retry_id": "retry-1",
            "timestamp_utc": "2026-05-01T00:05:00Z",
            "selection": {},
            "override_delta": { "sweep_cost_limit_usd": 30.0 },
            "count": 1,
            "harness_mismatch": false,
            "pre_submitted": 3,
            "pre_errored": 0,
            "pre_resolved_count": 3,
            "post_submitted": 3,
            "post_errored": 0,
            "post_resolved_count": 3
        },
        {
            "retry_id": "retry-2",
            "timestamp_utc": "not-a-timestamp",
            "selection": {},
            "override_delta": {},
            "count": 0,
            "harness_mismatch": false,
            "pre_submitted": 3,
            "pre_errored": 0,
            "pre_resolved_count": 3,
            "post_submitted": 3,
            "post_errored": 0,
            "post_resolved_count": 3
        }
    ]);
    std::fs::write(&results_path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

    // eval_ended_at is after the parseable retry — but the unparseable entry
    // should still make eval_is_fresh_after_retry return false.
    // Legacy format (no artifact_kind/schema_version header) accepted by load_evaluation_results.
    let eval_json = serde_json::json!({
        "provenance": {
            "backend": "sb-cli",
            "eval_ended_at": "2026-05-01T00:10:00Z"
        },
        "instances": [
            { "instance_id": "inst-0", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" },
            { "instance_id": "inst-1", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" },
            { "instance_id": "inst-2", "resolved_count": 1, "resolved": true, "tests_failed": [], "eval_exit_reason": "resolved" }
        ]
    });
    std::fs::write(
        dir.path().join("evaluation.json"),
        serde_json::to_string_pretty(&eval_json).unwrap(),
    )
    .unwrap();

    let result = compute_budget_fit(&BudgetFitArgs {
        sweep_dir: dir.path().to_path_buf(),
        at_cap_tolerance: 0.05,
        target_percentile: 95,
        axis: None,
        filter: vec![],
    });

    assert!(
        result.is_err(),
        "unparseable retry timestamp should cause eval to be treated as stale"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("stale") || msg.contains("retry"),
        "error should mention stale evaluation or retry history: {msg}"
    );
}
