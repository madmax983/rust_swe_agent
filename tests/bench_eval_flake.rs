//! Integration tests for `bench eval-flake` (issue #294).
//!
//! Uses a synthetic evaluator stub to verify:
//!   1. Flake detection: `is_flaky=true`, `flake_rate≈0.67` for an instance with
//!      verdicts [resolved, unresolved, resolved] across 3 replays.
//!   2. Stable instance: `is_flaky=false`, `flake_rate=0.0` for always-resolved.
//!   3. `bench compare --flake-report` excludes exactly the flaky instance.
//!   4. Summary text names the exclusion count explicitly.
//!   5. Degenerate case: all paired instances flaky exits with UsageError (2).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::collections::HashMap;
use std::path::PathBuf;

use maxwells_daemon::run::eval_flake::{
    EvalFlakeArgs, EvalFlakeStubConfig, InstanceVerdictStub, Verdict, run_with_stub,
};

mod support;
use support::binary_path;

// ── helper to write a minimal sweep results.json ─────────────────────────────

fn write_sweep_results(dir: &std::path::Path, instances: &[(&str, bool)]) -> PathBuf {
    use std::fmt::Write as _;
    let mut rows = String::new();
    for (id, resolved) in instances {
        let outcome = if *resolved { "submitted" } else { "error" };
        let resolved_count = i32::from(*resolved);
        writeln!(
            rows,
            r#"{{"instance_id":"{id}","exit_reason":"{outcome}","outcome":"{outcome}","failure_category":null,"steps":4,"cost_usd":0.01,"prompt_tokens":100,"cache_read_tokens":0,"cache_creation_tokens":0,"completion_tokens":50,"duration_secs":5.0,"error":null,"github_pr_error":null,"patch_present":{resolved},"non_empty_patch":{resolved},"attempts":1,"retry_reasons":[],"runs":1,"resolved_count":{resolved_count},"pass_at_1":{resolved},"tests_run_before_submit":false,"last_tests_passed":null,"fallback_count":null,"final_model":null,"retry_id":null,"previous_failure_category":null,"trace_id":null}}"#,
        )
        .unwrap();
    }
    let sweep_json = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 10},
        "instances": rows.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()).collect::<Vec<_>>(),
        "total": instances.len(),
        "submitted": instances.iter().filter(|(_, r)| *r).count(),
        "skipped": 0,
        "errored": instances.iter().filter(|(_, r)| !*r).count(),
        "budget_halted": 0,
        "retries": 0,
        "retried_instances": 0,
        "total_prompt_tokens": 100u64,
        "total_completion_tokens": 50u64,
        "total_cache_read_tokens": 0u64,
        "total_cache_creation_tokens": 0u64,
        "estimated_cost_usd": 0.01,
        "cost_limit_usd": null,
        "sweep_status": "complete",
        "cancel_exit_code": null,
        "systemic_halt_category": null,
        "github_pr_failures": 0
    });
    let path = dir.join("results.json");
    std::fs::write(&path, serde_json::to_string_pretty(&sweep_json).unwrap()).unwrap();
    path
}

fn write_patch(dir: &std::path::Path, instance_id: &str, content: &str) {
    let patch_path = dir.join(format!("{instance_id}.patch"));
    std::fs::write(patch_path, content).unwrap();
}

// ── AC: integration test 1 ────────────────────────────────────────────────────
// Alternating verdicts → is_flaky=true, flake_rate≈0.67
// Always-resolved → is_flaky=false, flake_rate=0.0

#[test]
fn flaky_instance_detected_with_correct_rate() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-flaky", true), ("inst-stable", true)]);
    write_patch(
        &sweep,
        "inst-flaky",
        "--- a/x.py\n+++ b/x.py\n@@ -1 +1 @@\n-old\n+new\n",
    );
    write_patch(
        &sweep,
        "inst-stable",
        "--- a/y.py\n+++ b/y.py\n@@ -1 +1 @@\n-a\n+b\n",
    );

    // inst-flaky alternates: resolved, unresolved, resolved
    // inst-stable: always resolved
    let mut stub_verdicts: HashMap<String, Vec<InstanceVerdictStub>> = HashMap::new();
    stub_verdicts.insert(
        "inst-flaky".into(),
        vec![
            InstanceVerdictStub::new("inst-flaky", Verdict::Resolved),
            InstanceVerdictStub::new("inst-flaky", Verdict::Unresolved),
            InstanceVerdictStub::new("inst-flaky", Verdict::Resolved),
        ],
    );
    stub_verdicts.insert(
        "inst-stable".into(),
        vec![
            InstanceVerdictStub::new("inst-stable", Verdict::Resolved),
            InstanceVerdictStub::new("inst-stable", Verdict::Resolved),
            InstanceVerdictStub::new("inst-stable", Verdict::Resolved),
        ],
    );

    let args = EvalFlakeArgs {
        sweep_dir: sweep,
        replays: 3,
        output: None,
        concurrency: 1,
    };
    let stub = EvalFlakeStubConfig {
        verdicts: stub_verdicts,
    };
    let report = run_with_stub(&args, &stub).unwrap();

    let flaky = report
        .instances
        .iter()
        .find(|i| i.instance_id == "inst-flaky")
        .expect("inst-flaky must be in report");
    assert!(flaky.is_flaky, "inst-flaky must be flagged flaky");
    assert!(
        (flaky.flake_rate - 0.666_666_7_f32).abs() < 0.01,
        "flake_rate expected ≈0.67, got {}",
        flaky.flake_rate
    );
    assert_eq!(flaky.verdicts.len(), 3);

    let stable = report
        .instances
        .iter()
        .find(|i| i.instance_id == "inst-stable")
        .expect("inst-stable must be in report");
    assert!(!stable.is_flaky, "inst-stable must not be flaky");
    assert_eq!(stable.flake_rate, 0.0_f32, "stable flake_rate must be 0.0");

    // Summary
    assert_eq!(report.summary.flaky_count, 1);
    assert_eq!(report.summary.instances_evaluated, 2);
    assert_eq!(report.summary.replays, 3);
    assert!(
        (report.summary.flaky_rate - 0.5_f32).abs() < 0.01,
        "flaky_rate expected 0.5 (1 of 2)"
    );
}

// ── AC: total_cost_usd is always 0.0 ─────────────────────────────────────────

#[test]
fn total_cost_usd_is_zero() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(
        &sweep,
        "inst-a",
        "--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-x\n+y\n",
    );

    let mut stubs: HashMap<String, Vec<InstanceVerdictStub>> = HashMap::new();
    stubs.insert(
        "inst-a".into(),
        vec![
            InstanceVerdictStub::new("inst-a", Verdict::Resolved),
            InstanceVerdictStub::new("inst-a", Verdict::Resolved),
            InstanceVerdictStub::new("inst-a", Verdict::Resolved),
        ],
    );
    let args = EvalFlakeArgs {
        sweep_dir: sweep,
        replays: 3,
        output: None,
        concurrency: 1,
    };
    let stub = EvalFlakeStubConfig { verdicts: stubs };
    let report = run_with_stub(&args, &stub).unwrap();
    assert_eq!(report.total_cost_usd, 0.0_f64, "cost must always be 0.0");
}

// ── AC: artifact written to sweep dir ─────────────────────────────────────────

#[test]
fn artifact_written_to_sweep_dir() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(
        &sweep,
        "inst-a",
        "--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-x\n+y\n",
    );

    let mut stubs: HashMap<String, Vec<InstanceVerdictStub>> = HashMap::new();
    stubs.insert(
        "inst-a".into(),
        vec![
            InstanceVerdictStub::new("inst-a", Verdict::Resolved),
            InstanceVerdictStub::new("inst-a", Verdict::Resolved),
            InstanceVerdictStub::new("inst-a", Verdict::Resolved),
        ],
    );
    let args = EvalFlakeArgs {
        sweep_dir: sweep.clone(),
        replays: 3,
        output: None,
        concurrency: 1,
    };
    let stub = EvalFlakeStubConfig { verdicts: stubs };
    run_with_stub(&args, &stub).unwrap();

    let artifact_path = sweep.join("eval-flake.json");
    assert!(
        artifact_path.exists(),
        "eval-flake.json must be written to sweep dir"
    );

    let content = std::fs::read_to_string(&artifact_path).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(val["artifact_kind"], "eval_flake_report");
    assert_eq!(val["total_cost_usd"], 0.0);
}

// ── AC: errored instances skipped (no patch) ──────────────────────────────────

#[test]
fn errored_instances_without_patch_skipped() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    // inst-no-patch has no patch file and is marked errored
    write_sweep_results(&sweep, &[("inst-patched", true), ("inst-no-patch", false)]);
    write_patch(
        &sweep,
        "inst-patched",
        "--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-x\n+y\n",
    );
    // deliberately don't write patch for inst-no-patch

    let mut stubs: HashMap<String, Vec<InstanceVerdictStub>> = HashMap::new();
    stubs.insert(
        "inst-patched".into(),
        vec![
            InstanceVerdictStub::new("inst-patched", Verdict::Resolved),
            InstanceVerdictStub::new("inst-patched", Verdict::Resolved),
        ],
    );
    let args = EvalFlakeArgs {
        sweep_dir: sweep,
        replays: 2,
        output: None,
        concurrency: 1,
    };
    let stub = EvalFlakeStubConfig { verdicts: stubs };
    let report = run_with_stub(&args, &stub).unwrap();

    assert_eq!(
        report.instances.len(),
        1,
        "only patched instance should be evaluated"
    );
    assert_eq!(report.instances[0].instance_id, "inst-patched");
    assert_eq!(report.summary.instances_evaluated, 1);
}

// ── AC: dominant_disagrees_with_sweep_count ───────────────────────────────────

#[test]
fn dominant_disagrees_with_sweep_count_correct() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    // inst-a: sweep said resolved, dominant verdict is unresolved (2 of 3)
    // inst-b: sweep said resolved, dominant verdict is resolved (always resolved)
    write_sweep_results(&sweep, &[("inst-a", true), ("inst-b", true)]);
    write_patch(
        &sweep,
        "inst-a",
        "--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-x\n+y\n",
    );
    write_patch(
        &sweep,
        "inst-b",
        "--- a/g.py\n+++ b/g.py\n@@ -1 +1 @@\n-a\n+b\n",
    );

    let mut stubs: HashMap<String, Vec<InstanceVerdictStub>> = HashMap::new();
    stubs.insert(
        "inst-a".into(),
        vec![
            InstanceVerdictStub::new("inst-a", Verdict::Unresolved),
            InstanceVerdictStub::new("inst-a", Verdict::Unresolved),
            InstanceVerdictStub::new("inst-a", Verdict::Resolved),
        ],
    );
    stubs.insert(
        "inst-b".into(),
        vec![
            InstanceVerdictStub::new("inst-b", Verdict::Resolved),
            InstanceVerdictStub::new("inst-b", Verdict::Resolved),
            InstanceVerdictStub::new("inst-b", Verdict::Resolved),
        ],
    );
    let args = EvalFlakeArgs {
        sweep_dir: sweep,
        replays: 3,
        output: None,
        concurrency: 1,
    };
    let stub = EvalFlakeStubConfig { verdicts: stubs };
    let report = run_with_stub(&args, &stub).unwrap();

    assert_eq!(
        report.summary.dominant_disagrees_with_sweep_count, 1,
        "inst-a dominant=unresolved disagrees with sweep=resolved"
    );
}

// ── AC: bench compare --flake-report excludes flaky instances ─────────────────
// AC: "1 flaky instance excluded" in summary text
// This uses the library-level compute() function via a helper.

#[test]
fn compare_excludes_flaky_instances_from_mcnemar() {
    let work = tempfile::tempdir().unwrap();
    let base = work.path().join("base");
    let cand = work.path().join("cand");
    let flake_dir = work.path().join("flake");
    for d in [&base, &cand, &flake_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    // 4 instances: 1 flaky (inst-flaky), 3 stable
    write_sweep_results(
        &base,
        &[
            ("inst-flaky", true),
            ("inst-a", true),
            ("inst-b", false),
            ("inst-c", false),
        ],
    );
    write_sweep_results(
        &cand,
        &[
            ("inst-flaky", false), // changed in candidate
            ("inst-a", true),
            ("inst-b", true), // improved in candidate
            ("inst-c", false),
        ],
    );

    // Write eval-flake.json flagging inst-flaky
    write_patch(
        &flake_dir,
        "inst-flaky",
        "--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-x\n+y\n",
    );
    let mut stubs: HashMap<String, Vec<InstanceVerdictStub>> = HashMap::new();
    stubs.insert(
        "inst-flaky".into(),
        vec![
            InstanceVerdictStub::new("inst-flaky", Verdict::Resolved),
            InstanceVerdictStub::new("inst-flaky", Verdict::Unresolved),
            InstanceVerdictStub::new("inst-flaky", Verdict::Resolved),
        ],
    );
    write_sweep_results(&flake_dir, &[("inst-flaky", true)]);
    let flake_args = EvalFlakeArgs {
        sweep_dir: flake_dir.clone(),
        replays: 3,
        output: None,
        concurrency: 1,
    };
    let stub = EvalFlakeStubConfig { verdicts: stubs };
    run_with_stub(&flake_args, &stub).unwrap();
    let flake_report_path = flake_dir.join("eval-flake.json");

    let compare_report =
        maxwells_daemon::run::compare::compute(&maxwells_daemon::run::compare::CompareArgs {
            baseline: base,
            candidate: cand,
            format: maxwells_daemon::run::compare::CompareFormat::Text,
            max_regressions: None,
            max_patch_size_regression_pct: None,
            breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: false,
            cost_attribution_min_delta_usd: 0.0,
            min_significance: None,
            regression_significance: None,
            allow_underpowered: true,
            flake_report: Some(flake_report_path),
        })
        .unwrap();

    // inst-flaky excluded → paired instances = 3 (inst-a, inst-b, inst-c)
    assert_eq!(
        compare_report.resolved_rate_significance.paired_n, 3,
        "paired_n must exclude the flaky instance"
    );
    assert_eq!(
        compare_report.flaky_instances_excluded, 1,
        "must record exactly 1 flaky exclusion"
    );
    let table = compare_report.human_table();
    assert!(
        table.contains("1 flaky instance excluded"),
        "table must name the exclusion count; got:\n{table}"
    );
}

// ── AC: all paired instances flaky → UsageError ──────────────────────────────

#[test]
fn compare_all_flaky_exits_usage_error() {
    let work = tempfile::tempdir().unwrap();
    let base = work.path().join("base");
    let cand = work.path().join("cand");
    let flake_dir = work.path().join("flake");
    for d in [&base, &cand, &flake_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    // Only 1 instance; it's flaky → all paired instances excluded
    write_sweep_results(&base, &[("inst-only", true)]);
    write_sweep_results(&cand, &[("inst-only", false)]);
    write_sweep_results(&flake_dir, &[("inst-only", true)]);
    write_patch(
        &flake_dir,
        "inst-only",
        "--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-x\n+y\n",
    );

    let mut stubs: HashMap<String, Vec<InstanceVerdictStub>> = HashMap::new();
    stubs.insert(
        "inst-only".into(),
        vec![
            InstanceVerdictStub::new("inst-only", Verdict::Resolved),
            InstanceVerdictStub::new("inst-only", Verdict::Unresolved),
            InstanceVerdictStub::new("inst-only", Verdict::Resolved),
        ],
    );
    let flake_args = EvalFlakeArgs {
        sweep_dir: flake_dir.clone(),
        replays: 3,
        output: None,
        concurrency: 1,
    };
    let stub = EvalFlakeStubConfig { verdicts: stubs };
    run_with_stub(&flake_args, &stub).unwrap();
    let flake_report_path = flake_dir.join("eval-flake.json");

    let result =
        maxwells_daemon::run::compare::compute(&maxwells_daemon::run::compare::CompareArgs {
            baseline: base,
            candidate: cand,
            format: maxwells_daemon::run::compare::CompareFormat::Text,
            max_regressions: None,
            max_patch_size_regression_pct: None,
            breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: false,
            cost_attribution_min_delta_usd: 0.0,
            min_significance: None,
            regression_significance: None,
            allow_underpowered: true,
            flake_report: Some(flake_report_path),
        });

    match result {
        Err(maxwells_daemon::error::Error::Config(maxwells_daemon::error::ConfigError::Usage(
            msg,
        ))) => {
            assert!(
                msg.contains("flak"),
                "error message must mention flake; got: {msg}"
            );
        }
        other => panic!("expected UsageError for all-flaky case, got: {other:?}"),
    }
}

// ── AC: bench eval-flake subcommand appears in --help ─────────────────────────

#[test]
fn eval_flake_subcommand_in_help() {
    let bin = binary_path();
    let out = std::process::Command::new(&bin)
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("eval-flake"),
        "bench --help must list eval-flake subcommand; got:\n{help}"
    );
}

// ── AC: bench compare --help shows --flake-report ─────────────────────────────

#[test]
fn compare_help_shows_flake_report_flag() {
    let bin = binary_path();
    let out = std::process::Command::new(&bin)
        .args(["bench", "compare", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("--flake-report"),
        "bench compare --help must show --flake-report; got:\n{help}"
    );
}

// ── AC: bench inspect --help shows --flake-report ─────────────────────────────

#[test]
fn inspect_help_shows_flake_report_flag() {
    let bin = binary_path();
    let out = std::process::Command::new(&bin)
        .args(["bench", "inspect", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("--flake-report"),
        "bench inspect --help must show --flake-report; got:\n{help}"
    );
}
