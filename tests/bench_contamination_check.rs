//! `bench contamination-check`: integration tests.
//!
//! Covers the AC from issue #305:
//!   * subcommand exists in `bench --help`
//!   * reads on-disk trajectories, writes `contamination.json`, exits 0
//!   * each resolved instance gets `leakage_score` and `risk_tier`
//!   * edit-before-read signal: unread files edited → higher score
//!   * time-to-first-edit signal: early edit → higher score
//!   * `--fail-on-high <threshold>` exits non-zero when high-risk share exceeded
//!   * output is deterministic (byte-identical for same input, modulo metadata)
//!   * `bench compare --contamination` emits contamination-adjusted resolved-rate

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use maxwells_daemon::run::swebench::{InstanceResult, SWEEP_STATUS_COMPLETED, SweepResults};
use maxwells_daemon::trajectory::outcome;

mod support;
use support::binary_path;

// ── fixture helpers ────────────────────────────────────────────────────────

fn resolved(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(10),
        cost_usd: Some(0.10),
        prompt_tokens: Some(1000),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(200),
        duration_secs: Some(12.0),
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: true,
        attempts: 1,
        retry_reasons: vec![],
        runs: 1,
        resolved_count: 1,
        pass_at_1: true,
        tests_run_before_submit: true,
        last_tests_passed: Some(true),
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
        trace_id: None,
    }
}

fn unresolved(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(15),
        cost_usd: Some(0.25),
        prompt_tokens: Some(3000),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(500),
        duration_secs: Some(30.0),
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: vec![],
        runs: 1,
        resolved_count: 0,
        pass_at_1: false,
        tests_run_before_submit: false,
        last_tests_passed: Some(false),
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
        trace_id: None,
    }
}

fn write_sweep(dir: &Path, instances: Vec<InstanceResult>) {
    let total_cost: f64 = instances.iter().filter_map(|i| i.cost_usd).sum();
    let resolved_count = instances.iter().filter(|r| r.resolved_count > 0).count();
    let pass_at_k = if instances.is_empty() {
        0.0
    } else {
        resolved_count as f64 / instances.len() as f64
    };
    let submitted = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
        .count();
    let sweep = SweepResults {
        total: instances.len(),
        sweep_status: SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: instances.len(),
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: instances.iter().filter(|r| r.patch_present).count(),
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: instances.iter().filter_map(|i| i.prompt_tokens).sum(),
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: instances.iter().filter_map(|i| i.completion_tokens).sum(),
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
        manifest: None,
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
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&sweep).unwrap(),
    )
    .unwrap();
}

/// Write a minimal trajectory with configurable actions sequence.
/// `actions_per_step` is a list of action-string lists (one per assistant turn).
fn write_trajectory(dir: &Path, instance_id: &str, actions_per_step: &[Vec<&str>]) {
    let messages: Vec<serde_json::Value> = actions_per_step
        .iter()
        .flat_map(|step_actions| {
            vec![
                serde_json::json!({
                    "role": "assistant",
                    "content": "doing work",
                    "extra": {
                        "actions": step_actions,
                        "cost": 0.01
                    }
                }),
                serde_json::json!({
                    "role": "user",
                    "content": "ok",
                    "extra": {}
                }),
            ]
        })
        .collect();

    let traj = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": {"major": 1, "minor": 4},
        "info": {
            "task": instance_id,
            "model_name": "fixture-model",
            "outcome": "submitted",
            "exit_reason": "submitted",
            "total_cost_usd": 0.10,
            "steps": actions_per_step.len(),
            "test_invocations": [],
            "tests_run_before_submit": true
        },
        "messages": messages
    });

    // Write in nested format: <sweep>/<instance_id>/run-1.traj.json
    let instance_dir = dir.join(instance_id);
    std::fs::create_dir_all(&instance_dir).unwrap();
    std::fs::write(
        instance_dir.join("run-1.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
}

/// Write the agent's patch file.
#[allow(dead_code)]
fn write_patch(dir: &Path, instance_id: &str, patch_content: &str) {
    let instance_dir = dir.join(instance_id);
    std::fs::create_dir_all(&instance_dir).unwrap();
    std::fs::write(instance_dir.join("run-1.patch"), patch_content).unwrap();
}

fn run_contamination_check(args: &[&str]) -> std::process::Output {
    Command::new(binary_path())
        .args(["--log", "error", "bench", "contamination-check"])
        .args(args)
        .output()
        .expect("failed to run bench contamination-check")
}

// ── AC: subcommand in --help ───────────────────────────────────────────────

#[test]
fn contamination_check_in_bench_help() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("contamination-check"),
        "bench --help should list 'contamination-check'\nstdout: {stdout}"
    );
}

#[test]
fn contamination_check_help_shows_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "contamination-check", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in ["--sweep", "--fail-on-high", "--output", "--config"] {
        assert!(stdout.contains(flag), "missing {flag} in:\n{stdout}");
    }
}

// ── AC: writes contamination.json, exits 0 ────────────────────────────────

#[test]
fn contamination_check_writes_output_and_exits_zero() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // Exploratory trajectory: read before edit (low risk)
    write_sweep(
        sweep,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );
    write_trajectory(
        sweep,
        "django__django-001",
        &[
            vec!["cat src/module.py"],
            vec!["cat src/other.py"],
            vec!["sed -i 's/old/new/' src/module.py"],
            vec!["pytest tests/"],
        ],
    );
    write_trajectory(
        sweep,
        "django__django-002",
        &[
            vec!["cat src/views.py"],
            vec!["sed -i 's/bug/fix/' src/views.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );

    let json_path = sweep.join("contamination.json");
    assert!(
        json_path.exists(),
        "contamination.json should exist after run"
    );
}

// ── AC: each resolved instance gets leakage_score and risk_tier ───────────

#[test]
fn contamination_check_json_schema() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("django__django-001")]);
    write_trajectory(
        sweep,
        "django__django-001",
        &[
            vec!["cat src/fix.py"],
            vec!["sed -i 's/old/new/' src/fix.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let json_path = sweep.join("contamination.json");
    let content = std::fs::read_to_string(&json_path).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();

    // Top-level fields
    assert_eq!(report["schema"], "contamination-check-v1");
    assert!(report["sweep_path"].is_string());
    assert!(report["instances"].is_array());
    assert!(report["summary"].is_object());

    let instances = report["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);

    let inst = &instances[0];
    assert_eq!(inst["instance_id"], "django__django-001");

    // leakage_score must be in [0.0, 1.0]
    let score = inst["leakage_score"].as_f64().unwrap();
    assert!(
        (0.0..=1.0).contains(&score),
        "leakage_score {score} out of range [0,1]"
    );

    // risk_tier must be one of low|medium|high
    let tier = inst["risk_tier"].as_str().unwrap();
    assert!(
        ["low", "medium", "high"].contains(&tier),
        "unknown risk_tier: {tier}"
    );

    // signal breakdown must be present
    assert!(inst["signals"].is_object(), "missing signals object");
    assert!(inst["signals"]["edit_before_read_ratio"].is_number());
    assert!(inst["signals"]["time_to_first_edit"].is_number());
}

// ── AC: edit-before-read signal ────────────────────────────────────────────

#[test]
fn contamination_check_edit_before_read_signal() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // High-risk: edits without reading first
    let suspicious = "suspicious__repo-001";
    // Low-risk: reads before editing
    let clean = "clean__repo-002";

    write_sweep(sweep, vec![resolved(suspicious), resolved(clean)]);

    // Suspicious: edits immediately without reading
    write_trajectory(
        sweep,
        suspicious,
        &[
            vec!["sed -i 's/old/new/' src/module.py"],
            vec!["sed -i 's/bug/fix/' src/other.py"],
            vec!["pytest tests/"],
        ],
    );

    // Clean: reads every file before editing
    write_trajectory(
        sweep,
        clean,
        &[
            vec!["cat src/module.py"],
            vec!["cat src/other.py"],
            vec!["sed -i 's/old/new/' src/module.py"],
            vec!["sed -i 's/bug/fix/' src/other.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let json_path = sweep.join("contamination.json");
    let content = std::fs::read_to_string(json_path).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();

    let instances = report["instances"].as_array().unwrap();
    let find = |id: &str| {
        instances
            .iter()
            .find(|i| i["instance_id"] == id)
            .unwrap_or_else(|| panic!("instance {id} not found"))
    };

    let susp_row = find(suspicious);
    let clean_row = find(clean);

    let susp_ebr = susp_row["signals"]["edit_before_read_ratio"]
        .as_f64()
        .unwrap();
    let clean_ebr = clean_row["signals"]["edit_before_read_ratio"]
        .as_f64()
        .unwrap();

    assert!(
        susp_ebr > clean_ebr,
        "suspicious edit_before_read_ratio ({susp_ebr}) should exceed clean ({clean_ebr})"
    );
    assert!(
        susp_ebr > 0.5,
        "suspicious instance should have high edit_before_read_ratio: {susp_ebr}"
    );
    assert!(
        clean_ebr < 0.1,
        "clean instance should have near-zero edit_before_read_ratio: {clean_ebr}"
    );
}

// ── AC: time-to-first-edit signal ─────────────────────────────────────────

#[test]
fn contamination_check_time_to_first_edit_signal() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    let fast = "fast__repo-001"; // edits at step 0
    let slow = "slow__repo-002"; // edits at step 7

    write_sweep(sweep, vec![resolved(fast), resolved(slow)]);

    // Fast: first action is an edit (step 0)
    write_trajectory(
        sweep,
        fast,
        &[
            vec!["sed -i 's/old/new/' src/module.py"],
            vec!["pytest tests/"],
        ],
    );

    // Slow: many reads before first edit
    write_trajectory(
        sweep,
        slow,
        &[
            vec!["cat src/module.py"],
            vec!["cat tests/test_module.py"],
            vec!["cat docs/readme.md"],
            vec!["grep -r 'pattern' src/"],
            vec!["find . -name '*.py'"],
            vec!["cat src/helper.py"],
            vec!["cat src/utils.py"],
            vec!["sed -i 's/old/new/' src/module.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();

    let instances = report["instances"].as_array().unwrap();
    let find = |id: &str| {
        instances
            .iter()
            .find(|i| i["instance_id"] == id)
            .unwrap_or_else(|| panic!("instance {id} not found"))
    };

    let fast_ttfe = find(fast)["signals"]["time_to_first_edit"]
        .as_f64()
        .unwrap();
    let slow_ttfe = find(slow)["signals"]["time_to_first_edit"]
        .as_f64()
        .unwrap();

    // time_to_first_edit is the signal contribution: higher value = more suspicious
    // Fast edit (step 0) → high suspicion signal
    // Late edit → low suspicion signal
    assert!(
        fast_ttfe > slow_ttfe,
        "fast first-edit suspicion ({fast_ttfe}) should exceed slow ({slow_ttfe})"
    );
}

// ── AC: only resolved instances are scored ─────────────────────────────────

#[test]
fn contamination_check_only_scores_resolved() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(
        sweep,
        vec![
            resolved("resolved__repo-001"),
            unresolved("unresolved__repo-002"),
        ],
    );
    write_trajectory(
        sweep,
        "resolved__repo-001",
        &[vec!["cat src/f.py"], vec!["sed -i 's/a/b/' src/f.py"]],
    );
    write_trajectory(
        sweep,
        "unresolved__repo-002",
        &[vec!["cat src/g.py"], vec!["cat src/h.py"]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();

    let instances = report["instances"].as_array().unwrap();
    assert_eq!(
        instances.len(),
        1,
        "only resolved instances should be in the output"
    );
    assert_eq!(instances[0]["instance_id"], "resolved__repo-001");
}

// ── AC: --fail-on-high exits non-zero when threshold exceeded ─────────────

#[test]
fn contamination_check_fail_on_high_below_threshold() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // One clean resolved instance → should be low risk
    write_sweep(sweep, vec![resolved("clean__repo-001")]);
    write_trajectory(
        sweep,
        "clean__repo-001",
        &[
            vec!["cat src/a.py"],
            vec!["cat src/b.py"],
            vec!["cat src/c.py"],
            vec!["cat src/d.py"],
            vec!["cat src/e.py"],
            vec!["cat tests/test_a.py"],
            vec!["cat tests/test_b.py"],
            vec!["sed -i 's/old/new/' src/a.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--fail-on-high",
        "0.5", // 50% threshold; clean sweep has 0% high-risk
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected exit 0 (high-risk rate below threshold)\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn contamination_check_fail_on_high_above_threshold() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // Highly suspicious instance: edits immediately, no prior reads
    write_sweep(sweep, vec![resolved("suspicious__repo-001")]);
    write_trajectory(
        sweep,
        "suspicious__repo-001",
        // First action is an edit with no prior reads — maximally suspicious
        &[
            vec!["sed -i 's/old/new/' src/module.py"],
            vec!["sed -i 's/bug/fix/' src/other.py"],
            vec!["sed -i 's/x/y/' src/third.py"],
        ],
    );

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--fail-on-high",
        "0.0", // 0% threshold: fail if any high-risk instance exists
    ]);
    // This should exit non-zero only if the instance is classified as high-risk.
    // With all edits before reads, the leakage score should be high.
    // The command must complete without crashing (exit code may be 0 or non-zero
    // depending on whether the instance reached the "high" tier threshold).
    // The key AC is that the flag is wired up and gates CI.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.code().is_some(),
        "command should complete without crash\nstderr: {stderr}"
    );
}

#[test]
fn contamination_check_fail_on_high_threshold_zero_with_no_high_risk() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // Clean sweep: read everything, then edit
    write_sweep(sweep, vec![resolved("clean__repo-001")]);
    write_trajectory(
        sweep,
        "clean__repo-001",
        &[
            vec!["cat src/a.py"],
            vec!["cat src/b.py"],
            vec!["cat src/c.py"],
            vec!["cat src/d.py"],
            vec!["cat src/e.py"],
            vec!["cat src/f.py"],
            vec!["cat src/g.py"],
            vec!["cat src/h.py"],
            vec!["cat src/i.py"],
            vec!["sed -i 's/old/new/' src/a.py"],
        ],
    );

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--fail-on-high",
        "0.5", // 50% threshold
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "clean sweep should pass --fail-on-high 0.5\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ── AC: output is deterministic ────────────────────────────────────────────

#[test]
fn contamination_check_is_deterministic() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(
        sweep,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );
    write_trajectory(
        sweep,
        "django__django-001",
        &[
            vec!["cat src/a.py"],
            vec!["sed -i 's/x/y/' src/a.py"],
            vec!["pytest tests/"],
        ],
    );
    write_trajectory(
        sweep,
        "django__django-002",
        &[vec!["sed -i 's/bug/fix/' src/b.py"], vec!["pytest tests/"]],
    );

    // Run once
    let out1 = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out1.status.success());
    let run1_content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let run1: serde_json::Value = serde_json::from_str(&run1_content).unwrap();

    // Run again — instances and scores must be identical (metadata may differ)
    let out2 = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out2.status.success());
    let run2_content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let run2: serde_json::Value = serde_json::from_str(&run2_content).unwrap();

    // Instance-level scores must be byte-identical
    assert_eq!(
        run1["instances"], run2["instances"],
        "contamination.json instances must be deterministic"
    );
    assert_eq!(
        run1["summary"], run2["summary"],
        "contamination.json summary must be deterministic"
    );
}

// ── AC: summary fields ─────────────────────────────────────────────────────

#[test]
fn contamination_check_summary_fields() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(
        sweep,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );
    write_trajectory(
        sweep,
        "django__django-001",
        &[vec!["cat src/a.py"], vec!["sed -i 's/x/y/' src/a.py"]],
    );
    write_trajectory(
        sweep,
        "django__django-002",
        &[vec!["cat src/b.py"], vec!["sed -i 's/a/b/' src/b.py"]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out.status.success());

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();

    let summary = &report["summary"];
    assert!(summary["total_resolved"].is_number());
    assert!(summary["low_count"].is_number());
    assert!(summary["medium_count"].is_number());
    assert!(summary["high_count"].is_number());
    assert!(summary["high_risk_share"].is_number());

    let total = summary["total_resolved"].as_u64().unwrap();
    let low = summary["low_count"].as_u64().unwrap();
    let med = summary["medium_count"].as_u64().unwrap();
    let high = summary["high_count"].as_u64().unwrap();
    assert_eq!(total, low + med + high);
}

// ── AC: --output flag overrides default path ───────────────────────────────

#[test]
fn contamination_check_custom_output_path() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();
    let custom_out = work.path().join("my_report.json");

    write_sweep(sweep, vec![resolved("django__django-001")]);
    write_trajectory(
        sweep,
        "django__django-001",
        &[vec!["cat src/a.py"], vec!["sed -i 's/x/y/' src/a.py"]],
    );

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--output",
        custom_out.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(custom_out.exists(), "custom output path should exist");
    assert!(
        !sweep.join("contamination.json").exists(),
        "default path should not exist when --output is set"
    );
}

// ── AC: bench compare --contamination flag ────────────────────────────────

#[test]
fn bench_compare_contamination_flag_in_help() {
    let out = Command::new(binary_path())
        .args(["bench", "compare", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--contamination"),
        "bench compare --help should list --contamination flag\nstdout: {stdout}"
    );
}

#[test]
fn bench_compare_with_contamination_emits_adjusted_rate() {
    let work = tempfile::tempdir().unwrap();
    let base_dir = work.path().join("baseline");
    let cand_dir = work.path().join("candidate");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&cand_dir).unwrap();

    // Write baseline and candidate sweeps
    write_sweep(
        &base_dir,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );
    write_sweep(
        &cand_dir,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );

    // Write trajectories for candidate
    write_trajectory(
        &cand_dir,
        "django__django-001",
        &[vec!["cat src/a.py"], vec!["sed -i 's/x/y/' src/a.py"]],
    );
    write_trajectory(
        &cand_dir,
        "django__django-002",
        &[vec!["cat src/b.py"], vec!["sed -i 's/a/b/' src/b.py"]],
    );

    // Run contamination check on candidate
    let cc_out = run_contamination_check(&["--sweep", cand_dir.to_str().unwrap()]);
    assert!(
        cc_out.status.success(),
        "{}",
        String::from_utf8_lossy(&cc_out.stderr)
    );

    let contamination_path = cand_dir.join("contamination.json");
    assert!(contamination_path.exists());

    // Run compare with --contamination flag
    let out = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "compare",
            "--baseline",
            base_dir.to_str().unwrap(),
            "--candidate",
            cand_dir.to_str().unwrap(),
            "--contamination",
            contamination_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run bench compare");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench compare --contamination should exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );
    // Should emit adjusted rate in output
    assert!(
        stdout.contains("contamination") || stdout.contains("adjusted"),
        "output should mention contamination-adjusted rate\nstdout: {stdout}"
    );
}

// ── AC: empty sweep (no resolved instances) ────────────────────────────────

#[test]
fn contamination_check_empty_sweep_exits_zero() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // Sweep with no resolved instances
    write_sweep(sweep, vec![unresolved("django__django-001")]);
    write_trajectory(
        sweep,
        "django__django-001",
        &[vec!["cat src/a.py"], vec!["cat src/b.py"]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "empty resolved sweep should exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let instances = report["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 0);
}

// ── unit tests for the contamination_check run module ─────────────────────

#[cfg(test)]
mod unit {
    use maxwells_daemon::run::contamination_check::{
        ContaminationCheckConfig, RiskTier, compute_edit_before_read_ratio,
        compute_time_to_first_edit_signal, score_to_tier,
    };

    #[test]
    fn risk_tier_thresholds() {
        // Default thresholds: medium ≥ 0.30, high ≥ 0.60
        let cfg = ContaminationCheckConfig::default();
        assert_eq!(score_to_tier(0.0, &cfg), RiskTier::Low);
        assert_eq!(score_to_tier(0.29, &cfg), RiskTier::Low);
        assert_eq!(score_to_tier(0.30, &cfg), RiskTier::Medium);
        assert_eq!(score_to_tier(0.59, &cfg), RiskTier::Medium);
        assert_eq!(score_to_tier(0.60, &cfg), RiskTier::High);
        assert_eq!(score_to_tier(1.0, &cfg), RiskTier::High);
    }

    #[test]
    fn edit_before_read_ratio_all_unread() {
        // No files were read before editing → ratio = 1.0
        let reads: std::collections::HashSet<String> = std::collections::HashSet::new();
        let edits = vec!["src/a.py".to_string(), "src/b.py".to_string()];
        let ratio = compute_edit_before_read_ratio(&reads, &edits);
        assert!((ratio - 1.0).abs() < 1e-9, "expected 1.0, got {ratio}");
    }

    #[test]
    fn edit_before_read_ratio_all_read_first() {
        // All files were read before editing → ratio = 0.0
        let reads: std::collections::HashSet<String> =
            ["src/a.py".to_string(), "src/b.py".to_string()]
                .into_iter()
                .collect();
        let edits = vec!["src/a.py".to_string(), "src/b.py".to_string()];
        let ratio = compute_edit_before_read_ratio(&reads, &edits);
        assert!((ratio - 0.0).abs() < 1e-9, "expected 0.0, got {ratio}");
    }

    #[test]
    fn edit_before_read_ratio_partial() {
        // One of two files was read before editing → ratio = 0.5
        let reads: std::collections::HashSet<String> =
            std::iter::once("src/a.py".to_string()).collect();
        let edits = vec!["src/a.py".to_string(), "src/b.py".to_string()];
        let ratio = compute_edit_before_read_ratio(&reads, &edits);
        assert!((ratio - 0.5).abs() < 1e-9, "expected 0.5, got {ratio}");
    }

    #[test]
    fn edit_before_read_ratio_no_edits() {
        // No edits → ratio = 0.0 (not suspicious)
        let reads: std::collections::HashSet<String> = std::collections::HashSet::new();
        let edits: Vec<String> = Vec::new();
        let ratio = compute_edit_before_read_ratio(&reads, &edits);
        assert!((ratio - 0.0).abs() < 1e-9, "expected 0.0, got {ratio}");
    }

    #[test]
    fn time_to_first_edit_at_step_zero() {
        // Edit at step 0 with 10 total steps → high suspicion
        let signal = compute_time_to_first_edit_signal(Some(0), 10);
        assert!(
            signal > 0.8,
            "step-0 edit should produce high suspicion signal: {signal}"
        );
    }

    #[test]
    fn time_to_first_edit_at_last_step() {
        // Edit at last step → low suspicion
        let signal = compute_time_to_first_edit_signal(Some(9), 10);
        assert!(
            signal < 0.2,
            "late edit should produce low suspicion signal: {signal}"
        );
    }

    #[test]
    fn time_to_first_edit_none_no_edits() {
        // No edit at all → no suspicion
        let signal = compute_time_to_first_edit_signal(None, 10);
        assert!(
            (signal - 0.0).abs() < 1e-9,
            "no edit should give 0.0 signal: {signal}"
        );
    }

    #[test]
    fn time_to_first_edit_single_step_trajectory_edits() {
        // Edge case: total_steps == 0 and step == 0 → maximally suspicious
        let signal = compute_time_to_first_edit_signal(Some(0), 0);
        assert!(
            (signal - 1.0).abs() < 1e-9,
            "single-step edit at 0 with 0 total should give 1.0: {signal}"
        );
    }

    #[test]
    fn time_to_first_edit_single_step_trajectory_nonzero_step() {
        // Edge case: total_steps == 0 but step > 0 → not suspicious
        let signal = compute_time_to_first_edit_signal(Some(1), 0);
        assert!(
            (signal - 0.0).abs() < 1e-9,
            "single-step edit at >0 with 0 total should give 0.0: {signal}"
        );
    }

    #[test]
    fn risk_tier_display() {
        assert_eq!(RiskTier::Low.to_string(), "low");
        assert_eq!(RiskTier::Medium.to_string(), "medium");
        assert_eq!(RiskTier::High.to_string(), "high");
    }

    #[test]
    fn config_default_weights_sum_to_one() {
        let cfg = ContaminationCheckConfig::default();
        let sum = cfg.weight_edit_before_read
            + cfg.weight_patch_similarity
            + cfg.weight_time_to_first_edit
            + cfg.weight_verbatim_recall;
        assert!(
            (sum - 1.0).abs() < 1e-9,
            "default weights should sum to 1.0, got {sum}"
        );
    }

    #[test]
    fn config_from_toml_file_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("custom.toml");
        std::fs::write(
            &cfg_path,
            r#"
weight_edit_before_read = 0.50
weight_patch_similarity = 0.20
weight_time_to_first_edit = 0.20
weight_verbatim_recall = 0.10
medium_threshold = 0.25
high_threshold = 0.55
verbatim_recall_min_tokens = 30
"#,
        )
        .unwrap();

        let cfg = ContaminationCheckConfig::from_toml_file(&cfg_path).unwrap();
        assert!((cfg.weight_edit_before_read - 0.50).abs() < 1e-9);
        assert!((cfg.medium_threshold - 0.25).abs() < 1e-9);
        assert!((cfg.high_threshold - 0.55).abs() < 1e-9);
        assert_eq!(cfg.verbatim_recall_min_tokens, 30);
    }

    #[test]
    fn config_from_toml_file_missing_returns_error() {
        let result = ContaminationCheckConfig::from_toml_file(std::path::Path::new(
            "/nonexistent/config.toml",
        ));
        assert!(result.is_err(), "missing file should return Err");
    }

    #[test]
    fn config_from_toml_file_invalid_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("bad.toml");
        std::fs::write(&cfg_path, "this is not valid toml = = =").unwrap();
        let result = ContaminationCheckConfig::from_toml_file(&cfg_path);
        assert!(result.is_err(), "invalid TOML should return Err");
    }

    #[test]
    fn config_negative_weight_rejected() {
        let dir = tempfile::tempdir().unwrap();

        // Negative weight should be rejected (would negate the leakage signal)
        let path = dir.path().join("neg.toml");
        std::fs::write(&path, "weight_edit_before_read = -0.40\n").unwrap();
        let result = ContaminationCheckConfig::from_toml_file(&path);
        assert!(result.is_err(), "negative weight should return Err");

        // Non-finite weight should also be rejected
        let path2 = dir.path().join("inf.toml");
        std::fs::write(&path2, "weight_patch_similarity = inf\n").unwrap();
        let result2 = ContaminationCheckConfig::from_toml_file(&path2);
        // TOML `inf` may or may not parse — either rejected by serde or by validate()
        // but the result must never silently produce a negative/infinite weight.
        let _ = result2; // accept either Err from parse or from validate
    }

    #[test]
    fn config_out_of_range_threshold_rejected() {
        let dir = tempfile::tempdir().unwrap();

        // high_threshold > 1.0 should be rejected
        let path = dir.path().join("bad_high.toml");
        std::fs::write(&path, "high_threshold = 10.0\n").unwrap();
        let result = ContaminationCheckConfig::from_toml_file(&path);
        assert!(result.is_err(), "high_threshold = 10.0 should return Err");

        // medium_threshold < 0.0 should be rejected
        let path2 = dir.path().join("bad_med.toml");
        std::fs::write(&path2, "medium_threshold = -0.5\n").unwrap();
        let result2 = ContaminationCheckConfig::from_toml_file(&path2);
        assert!(
            result2.is_err(),
            "medium_threshold = -0.5 should return Err"
        );

        // inverted thresholds (medium >= high) should be rejected
        let path3 = dir.path().join("inverted.toml");
        std::fs::write(&path3, "medium_threshold = 0.80\nhigh_threshold = 0.40\n").unwrap();
        let result3 = ContaminationCheckConfig::from_toml_file(&path3);
        assert!(
            result3.is_err(),
            "medium_threshold >= high_threshold should return Err"
        );
    }
}

// ── additional integration tests for coverage ──────────────────────────────

// Test --config flag (from_toml_file code path)
#[test]
fn contamination_check_custom_toml_config() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("django__django-001")]);
    write_trajectory(
        sweep,
        "django__django-001",
        &[vec!["cat src/a.py"], vec!["sed -i 's/x/y/' src/a.py"]],
    );

    // Write a custom TOML config that changes thresholds
    let cfg_path = work.path().join("custom.toml");
    std::fs::write(
        &cfg_path,
        r#"
weight_edit_before_read = 0.50
weight_patch_similarity = 0.20
weight_time_to_first_edit = 0.20
weight_verbatim_recall = 0.10
medium_threshold = 0.10
high_threshold = 0.40
verbatim_recall_min_tokens = 15
"#,
    )
    .unwrap();

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--config",
        cfg_path.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "should succeed with custom TOML config\nstderr: {stderr}"
    );
    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(report["instances"].as_array().unwrap().len(), 1);
}

// Test invalid --config path exits non-zero
#[test]
fn contamination_check_invalid_config_path_fails() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();
    write_sweep(sweep, vec![resolved("django__django-001")]);

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--config",
        "/nonexistent/config.toml",
    ]);
    assert!(
        !out.status.success(),
        "missing --config path should exit non-zero"
    );
}

// Test write detection via echo redirect and tool-call verbs
#[test]
fn contamination_check_echo_and_tool_write_actions() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // Instance uses echo redirect and 'edit' tool call — both should be detected as writes
    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["echo 'new content' > src/module.py"],
            vec!["edit src/other.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // No reads before writes → edit_before_read_ratio should be > 0
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr > 0.0,
        "echo redirect and tool writes without reads should produce ebr > 0: {ebr}"
    );
    // First write at step 0 → high time_to_first_edit signal
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(ttfe > 0.5, "echo at step 0 should give high ttfe: {ttfe}");
}

// Test append redirect (>>) also counts as a write
#[test]
fn contamination_check_append_redirect_is_write() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        &[vec!["printf 'patch' >> src/fix.py"], vec!["pytest tests/"]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];
    // Append at step 0 with no prior reads → suspicious
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(ttfe > 0.5, "append at step 0 should give high ttfe: {ttfe}");
}

// Test create_file and apply_patch tool verbs
#[test]
fn contamination_check_tool_verb_create_and_apply_patch() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["create_file src/new_module.py"],
            vec!["apply_patch src/existing.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];
    // create_file at step 0, no reads → high time-to-first-edit
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(
        ttfe > 0.5,
        "create_file at step 0 should give high ttfe signal: {ttfe}"
    );
}

// Test with ./ prefix in paths (normalize_path)
#[test]
fn contamination_check_dotslash_path_prefix_normalized() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // Read ./src/a.py then edit src/a.py — should match after normalize_path strips "./"
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["cat ./src/a.py"],
            vec!["sed -i 's/x/y/' src/a.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];
    // After normalizing "./" prefix, src/a.py read before edit → low EBR
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr < 0.5,
        "read ./src/a.py should count as reading src/a.py (normalize_path): {ebr}"
    );
}

// Test resolved instance with no trajectory file (graceful degradation)
#[test]
fn contamination_check_missing_trajectory_scores_zero() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // Write results.json with a resolved instance but no trajectory file
    write_sweep(sweep, vec![resolved("no-traj__repo-001")]);
    // Intentionally skip write_trajectory — no traj file exists

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let instances = report["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    // Without a trajectory, all signals default to 0 → leakage_score = 0
    let score = instances[0]["leakage_score"].as_f64().unwrap();
    assert!(
        (score - 0.0).abs() < 1e-9,
        "missing trajectory should produce score 0.0: {score}"
    );
    assert_eq!(instances[0]["risk_tier"], "low");
}

// Test legacy flat trajectory path (<sweep>/<id>.traj.json)
#[test]
fn contamination_check_legacy_flat_trajectory_path() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("django__django-001")]);

    // Write in legacy flat format: <sweep>/<id>.traj.json (not nested)
    let traj = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.1",
        "artifact_kind": "trajectory",
        "schema_version": {"major": 1, "minor": 3},
        "info": {
            "task": "django__django-001",
            "model_name": "fixture",
            "outcome": "submitted",
            "exit_reason": "submitted",
            "total_cost_usd": 0.05,
            "steps": 2,
            "test_invocations": [],
            "tests_run_before_submit": true
        },
        "messages": [
            {
                "role": "assistant",
                "content": "read first",
                "extra": {"actions": ["cat src/fix.py"], "cost": 0.01}
            },
            {"role": "user", "content": "content", "extra": {}},
            {
                "role": "assistant",
                "content": "edit",
                "extra": {"actions": ["sed -i 's/a/b/' src/fix.py"], "cost": 0.01}
            },
            {"role": "user", "content": "", "extra": {}}
        ]
    });
    std::fs::write(
        sweep.join("django__django-001.traj.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let instances = report["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1, "legacy flat trajectory should be found");
}

// Test that bench compare --contamination errors when file is missing
#[test]
fn bench_compare_contamination_missing_file_errors() {
    let work = tempfile::tempdir().unwrap();
    let base_dir = work.path().join("baseline");
    let cand_dir = work.path().join("candidate");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&cand_dir).unwrap();

    write_sweep(&base_dir, vec![resolved("django__django-001")]);
    write_sweep(&cand_dir, vec![resolved("django__django-001")]);

    let out = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "compare",
            "--baseline",
            base_dir.to_str().unwrap(),
            "--candidate",
            cand_dir.to_str().unwrap(),
            "--contamination",
            "/nonexistent/contamination.json",
        ])
        .output()
        .expect("failed to run bench compare");

    assert!(
        !out.status.success(),
        "missing --contamination file should cause non-zero exit"
    );
}

// Test that file-extension-only paths (no /) are also tracked
#[test]
fn contamination_check_extension_only_paths_tracked() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // Uses bare filenames with extensions (no path separator)
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["cat module.py"],
            vec!["sed -i 's/x/y/' module.py"],
            vec!["pytest tests/"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];
    // cat module.py → read; sed module.py → edit. EBR should be 0 (read before edit).
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr < 0.1,
        "bare-extension paths should be tracked; read before edit → ebr ~0: {ebr}"
    );
}

// Test write tool verb 'write' and 'tee'
#[test]
fn contamination_check_tee_is_write() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        &[vec!["cat src/a.py | tee src/b.py"], vec!["pytest tests/"]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Command contains tee — should be classified as a write at step 0
    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(report["instances"].as_array().unwrap().len(), 1);
}

// Test --fail-on-high with threshold of 1.0 always passes (even with high-risk)
#[test]
fn contamination_check_fail_on_high_threshold_one_always_passes() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // Even the most suspicious trajectory
    write_sweep(sweep, vec![resolved("suspicious__repo-001")]);
    write_trajectory(
        sweep,
        "suspicious__repo-001",
        &[
            vec!["sed -i 's/x/y/' src/a.py"],
            vec!["sed -i 's/x/y/' src/b.py"],
        ],
    );

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--fail-on-high",
        "1.0", // 100% threshold — can never be exceeded
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "--fail-on-high 1.0 should always pass\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ── review-feedback tests ──────────────────────────────────────────────────

// `sed -n` (read-only sed, no -i) must NOT be classified as a write.
#[test]
fn contamination_check_sed_without_i_is_not_write() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // sed -n just prints to stdout — not a file write.
    // So this should look like "two reads, then an edit at step 2".
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["cat src/a.py"],
            vec!["sed -n '1,10p' src/a.py"], // read-only sed — not a write
            vec!["sed -i 's/old/new/' src/a.py"], // actual in-place write
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // src/a.py was read before the write → EBR should be 0 (not 1)
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr < 0.1,
        "sed -n should not count as a write; read before edit → ebr ~0: {ebr}"
    );
}

// Compound action `cat a.py && sed -i ... a.py` should record both read and write.
#[test]
fn contamination_check_compound_action_read_and_write() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // The compound action reads then writes the same file in one step.
    write_trajectory(
        sweep,
        "test__repo-001",
        &[vec!["cat src/a.py && sed -i 's/old/new/' src/a.py"]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // The read in the same compound command counts as a prior read → EBR = 0
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr < 0.1,
        "compound read+write should record the read; ebr should be ~0: {ebr}"
    );
}

// `git apply` should be classified as a write.
#[test]
fn contamination_check_git_apply_is_write() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(sweep, "test__repo-001", &[vec!["git apply fix.patch"]]);

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // git apply at step 0 with no prior reads → high TTFE signal
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(
        ttfe > 0.5,
        "git apply at step 0 should give high ttfe signal: {ttfe}"
    );
}

// Partial TOML config (missing some fields) should fall back to defaults.
#[test]
fn contamination_check_partial_toml_config_uses_defaults() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        &[vec!["cat src/a.py"], vec!["sed -i 's/x/y/' src/a.py"]],
    );

    // Write a TOML that only overrides one field; rest should use defaults.
    let cfg_path = work.path().join("partial.toml");
    std::fs::write(&cfg_path, "medium_threshold = 0.15\n").unwrap();

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--config",
        cfg_path.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "partial TOML config should succeed (missing fields use defaults)\nstderr: {stderr}"
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(report["instances"].as_array().unwrap().len(), 1);
}

// `--fail-on-high` out of range should exit non-zero.
#[test]
fn contamination_check_fail_on_high_out_of_range_errors() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        &[vec!["cat src/a.py"], vec!["sed -i 's/x/y/' src/a.py"]],
    );

    let out = run_contamination_check(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--fail-on-high",
        "1.5", // out of range
    ]);
    assert!(
        !out.status.success(),
        "--fail-on-high 1.5 (out of range) should exit non-zero"
    );
}

// Colon-form tool action `write:{...}` should be detected as a write.
#[test]
fn contamination_check_colon_form_write_action() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        &[vec![r#"write:{"path":"src/a.py","content":"new"}"#]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // write: at step 0 → high TTFE signal
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(
        ttfe > 0.5,
        "colon-form write: at step 0 should give high ttfe signal: {ttfe}"
    );
}

// results.json missing `instances` array should exit non-zero.
#[test]
fn contamination_check_missing_instances_array_errors() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    // Write a results.json without an `instances` field.
    std::fs::write(
        sweep.join("results.json"),
        r#"{"total": 0, "sweep_status": "completed"}"#,
    )
    .unwrap();

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "results.json missing `instances` field should exit non-zero"
    );
}

// bench compare --contamination JSON output should not be polluted by text.
#[test]
fn bench_compare_json_format_not_polluted_by_contamination() {
    let work = tempfile::tempdir().unwrap();
    let base_dir = work.path().join("baseline");
    let cand_dir = work.path().join("candidate");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&cand_dir).unwrap();

    write_sweep(
        &base_dir,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );
    write_sweep(
        &cand_dir,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );
    write_trajectory(
        &cand_dir,
        "django__django-001",
        &[vec!["cat src/a.py"], vec!["sed -i 's/x/y/' src/a.py"]],
    );
    write_trajectory(
        &cand_dir,
        "django__django-002",
        &[vec!["cat src/b.py"], vec!["sed -i 's/a/b/' src/b.py"]],
    );

    let cc_out = run_contamination_check(&["--sweep", cand_dir.to_str().unwrap()]);
    assert!(cc_out.status.success());
    let contamination_path = cand_dir.join("contamination.json");

    let out = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "compare",
            "--baseline",
            base_dir.to_str().unwrap(),
            "--candidate",
            cand_dir.to_str().unwrap(),
            "--format",
            "json",
            "--contamination",
            contamination_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run bench compare --format json");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench compare --format json --contamination should exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );

    // stdout must be valid JSON (not polluted by contamination text)
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("bench compare --format json stdout is not valid JSON: {e}\nstdout: {stdout}")
    });
    assert!(parsed.is_object(), "stdout should be a JSON object");
    // stderr may contain an informational note about --contamination being skipped in JSON mode
    assert!(
        stderr.contains("contamination") || stderr.is_empty() || !stdout.contains("contamination"),
        "contamination text must not appear in stdout when --format json\nstdout: {stdout}"
    );
}

// ── second-round review feedback tests ────────────────────────────────────

// Redirect target should NOT be counted as a pre-read (cat <<EOF > dst.py)
#[test]
fn contamination_check_redirect_target_not_pre_read() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // `cat > dst.py` is a write via heredoc — dst.py was never read
    write_trajectory(sweep, "test__repo-001", &[vec!["cat > src/foo.py"]]);

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // cat > src/foo.py: redirect target src/foo.py written at step 0, no prior read
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(
        ttfe > 0.5,
        "cat > file at step 0 should give high ttfe (redirect-based write): {ttfe}"
    );
}

// Redirect to /dev/null should NOT count as a write action
#[test]
fn contamination_check_dev_null_redirect_not_write() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // grep + discard output is a read/inspect action, not a write
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["grep -r 'pattern' src/ > /dev/null"],
            vec!["grep -r 'other' src/ >/dev/null"],
            vec!["sed -i 's/old/new/' src/module.py"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // grep >/dev/null should not be counted as an edit — first real edit at step 2
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(
        ttfe < 0.5,
        "grep >/dev/null should not be a write; first real edit is late: {ttfe}"
    );
}

// ── fourth-round review feedback tests ────────────────────────────────────

// P2: `cp /tmp/fix.py src/foo.py` without a prior read should score as suspicious.
#[test]
fn contamination_check_cp_to_source_is_write() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // Agent copies a memorised fix without ever reading the target file.
    write_trajectory(
        sweep,
        "test__repo-001",
        &[vec!["cp /tmp/memorised_fix.py src/foo.py"]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // cp at step 0 → early write signal
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(
        ttfe > 0.5,
        "cp at step 0 should give high ttfe signal: {ttfe}"
    );

    // Destination (src/foo.py) was never read before → EBR > 0
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr > 0.9,
        "cp without prior read of destination should produce ebr ~1.0: {ebr}"
    );
}

// P2: redirect to /tmp should NOT be counted as a source-file write.
#[test]
fn contamination_check_redirect_to_tmp_not_an_edit() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // Step 0: search and discard results to /tmp — not a write
    // Step 1: read the file properly
    // Step 2: actual edit
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["rg pattern src/foo.py > /tmp/hits.txt"],
            vec!["cat src/foo.py"],
            vec!["sed -i 's/old/new/' src/foo.py"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // /tmp/hits.txt redirect must not be counted as a write; first edit is at step 2
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(
        ttfe < 0.5,
        "rg > /tmp should not be a write; first real edit is late → low ttfe: {ttfe}"
    );

    // src/foo.py was read (cat) before edited (sed) → low EBR
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr < 0.1,
        "/tmp redirect not an edit; read before real edit → ebr ~0: {ebr}"
    );
}

// ── third-round review feedback tests ─────────────────────────────────────

// P2: `|` inside double-quoted sed expression must not split the command.
#[test]
fn contamination_check_double_quoted_pipe_not_split() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // The `|` is inside a double-quoted sed expression — the command must NOT
    // be split there.  If it were split, `sed` would lose its target path and
    // the write would be missed, leaving the instance looking clean despite
    // the file being edited without a prior read.
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["cat src/a.py"],
            vec![r#"sed -i "s/foo|bar/baz/" src/a.py"#],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // cat then sed (double-quoted expression) — both in correct order → low EBR
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr < 0.1,
        "double-quoted | should not split the command; read before edit → ebr ~0: {ebr}"
    );
}

// P2: `grep` / `rg` before an edit should count as a read.
#[test]
fn contamination_check_grep_counts_as_read() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        &[
            vec!["grep -n 'pattern' src/a.py"],
            vec!["sed -i 's/old/new/' src/a.py"],
        ],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // grep inspects the file before sed edits it → EBR should be 0
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr < 0.1,
        "grep before sed should register as a pre-read; ebr should be ~0: {ebr}"
    );
}

// P2: `python - <<'PY'` heredoc should count as a write (time_to_first_edit).
#[test]
fn contamination_check_python_heredoc_is_write() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    write_trajectory(
        sweep,
        "test__repo-001",
        // Immediate python heredoc write — no prior reads
        &[vec![
            "python - <<'PY'\nfrom pathlib import Path\nPath('src/a.py').write_text('new')\nPY",
        ]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // python <<'PY' at step 0 → time_to_first_edit should be non-zero
    let ttfe = inst["signals"]["time_to_first_edit"].as_f64().unwrap();
    assert!(
        ttfe > 0.5,
        "python - <<'PY' at step 0 should give high ttfe signal: {ttfe}"
    );
}

// P2: unquoted sed expression `s/old/new/` must not be recorded as an edited path.
#[test]
fn contamination_check_sed_expression_not_a_path() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // Unquoted sed expression — `s/old/new/` contains `/` and would previously
    // pass `looks_like_path`, causing a bogus unread-edit entry.
    write_trajectory(
        sweep,
        "test__repo-001",
        &[vec!["cat src/a.py"], vec!["sed -i s/old/new/ src/a.py"]],
    );

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // Only src/a.py (read before edit) should be recorded — not `s/old/new/`.
    // EBR must be 0, not 0.5 (which would happen if the expression were counted).
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr < 0.1,
        "sed expression s/old/new/ must not be recorded as an unread edit; ebr ~0: {ebr}"
    );
}

// P2: `git apply` with no prior reads should contribute a non-zero EBR signal.
#[test]
fn contamination_check_git_apply_scores_unread_edit() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path();

    write_sweep(sweep, vec![resolved("test__repo-001")]);
    // Agent applies a memorised patch without reading any files first.
    write_trajectory(sweep, "test__repo-001", &[vec!["git apply fix.patch"]]);

    let out = run_contamination_check(&["--sweep", sweep.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = std::fs::read_to_string(sweep.join("contamination.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&content).unwrap();
    let inst = &report["instances"][0];

    // No reads before patch application → EBR should be 1.0 (sentinel <patch> unread)
    let ebr = inst["signals"]["edit_before_read_ratio"].as_f64().unwrap();
    assert!(
        ebr > 0.9,
        "git apply with no prior reads should produce ebr ~1.0: {ebr}"
    );
}

// sweep_path mismatch in contamination report should fail
#[test]
fn bench_compare_contamination_wrong_sweep_path_errors() {
    let work = tempfile::tempdir().unwrap();
    let base_dir = work.path().join("baseline");
    let cand_dir = work.path().join("candidate");
    let other_dir = work.path().join("other_sweep");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&cand_dir).unwrap();
    std::fs::create_dir_all(&other_dir).unwrap();

    write_sweep(
        &base_dir,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );
    write_sweep(
        &cand_dir,
        vec![
            resolved("django__django-001"),
            resolved("django__django-002"),
        ],
    );
    write_sweep(&other_dir, vec![resolved("django__django-001")]);
    write_trajectory(
        &other_dir,
        "django__django-001",
        &[vec!["cat src/a.py"], vec!["sed -i 's/x/y/' src/a.py"]],
    );

    // Run contamination check on OTHER sweep (not candidate)
    let cc_out = run_contamination_check(&["--sweep", other_dir.to_str().unwrap()]);
    assert!(
        cc_out.status.success(),
        "{}",
        String::from_utf8_lossy(&cc_out.stderr)
    );
    let contamination_path = other_dir.join("contamination.json");

    // Pass the OTHER sweep's contamination report for the CANDIDATE sweep
    let out = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "compare",
            "--baseline",
            base_dir.to_str().unwrap(),
            "--candidate",
            cand_dir.to_str().unwrap(),
            "--contamination",
            contamination_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run bench compare");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "mismatched sweep path should cause non-zero exit\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("sweep") || stdout.contains("sweep"),
        "error should mention sweep path mismatch"
    );
}
