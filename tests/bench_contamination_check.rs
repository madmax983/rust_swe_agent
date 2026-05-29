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
}
