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
    clippy::cast_precision_loss
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use maxwells_daemon::run::budget_fit::{BudgetFitArgs, compute_budget_fit};
use maxwells_daemon::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest, HarnessManifest, InstanceResult,
    ModelManifest, ProvenanceManifest, PromptTemplateManifest, RuntimeManifest, SweepResults,
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
        circuit_breaker: None,
        reproduced_from: None,
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
        .map(|(i, &s)| resolved_instance(&format!("inst-{i:02}"), s, 0.05, s as f64 * 2.0))
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
        .map(|i| (Box::leak(format!("inst-cap-{i:02}").into_boxed_str()) as &str, "write"))
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
        steps_axis.recommended_cap.unwrap_or(0.0)
            > steps_axis.configured_cap.unwrap_or(f64::MAX),
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
        .map(|i| (Box::leak(format!("inst-cap-{i:02}").into_boxed_str()) as &str, "noop"))
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
                resolved_instance(&format!("inst-{i:02}"), 10 + i * 2, 0.05, 20.0 + i as f64)
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
        .map(|(i, &s)| resolved_instance(&format!("inst-{i:02}"), s, 0.05, s as f64 * 2.0))
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
        .map(|i| resolved_instance(&format!("inst-{i}"), 10 + i * 3, 0.04, 20.0 + i as f64))
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
        .map(|i| resolved_instance(&format!("inst-{i}"), 10 + i * 3, 0.04, 20.0 + i as f64))
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
    assert_eq!(axes.len(), 1, "--axis steps should produce exactly one axis");
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
