//! Integration tests for `bench eval-parity` (issue #502).
//!
//! Uses a synthetic evaluator stub to verify:
//!   1. Agreement rate is 1.0 when both backends agree on all instances.
//!   2. Agreement rate is correctly computed when backends disagree.
//!   3. Disagreements are sorted deterministically by instance_id.
//!   4. `--min-agreement` gate: report exposes agreement_rate for CLI gating.
//!   5. `--sample <N>` bounds the number of instances evaluated.
//!   6. Report records backend identifiers and versions.
//!   7. Report records dataset_sha256.
//!   8. Flakiness note is present in the report.
//!   9. Artifact is written to the sweep directory.
//!  10. total_cost_usd is always 0.0.
//!  11. `bench --help` lists the eval-parity subcommand.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::collections::HashMap;
use std::path::PathBuf;

use maxwells_daemon::run::eval_parity::{
    EvalParityArgs, EvalParityStubConfig, InstanceParityStub, Verdict, run_with_stub,
};

mod support;
use support::binary_path;

// ── helper: write a minimal sweep results.json ────────────────────────────────

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
        "schema_version": {"major": 1, "minor": 11},
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

fn make_patch() -> &'static str {
    "--- a/x.py\n+++ b/x.py\n@@ -1 +1 @@\n-old\n+new\n"
}

// ── AC2: agreement_rate=1.0 when all instances agree ─────────────────────────

#[test]
fn all_agree_produces_full_agreement_rate() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true), ("inst-b", false)]);
    write_patch(&sweep, "inst-a", make_patch());
    write_patch(&sweep, "inst-b", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );
    verdicts.insert(
        "inst-b".to_owned(),
        InstanceParityStub::new(Verdict::Unresolved, Verdict::Unresolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();

    assert_eq!(report.summary.instances_compared, 2);
    assert_eq!(report.summary.agreed, 2);
    assert_eq!(report.summary.disagreed, 0);
    assert!(
        (report.summary.agreement_rate - 1.0_f64).abs() < 1e-9,
        "expected agreement_rate=1.0, got {}",
        report.summary.agreement_rate
    );
    assert!(report.disagreements.is_empty());
}

// ── AC2: partial agreement ─────────────────────────────────────────────────────

#[test]
fn partial_agreement_rate_correct() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    // 3 instances: 2 agree, 1 disagrees → rate = 2/3 ≈ 0.6667
    write_sweep_results(
        &sweep,
        &[("inst-a", true), ("inst-b", false), ("inst-c", true)],
    );
    write_patch(&sweep, "inst-a", make_patch());
    write_patch(&sweep, "inst-b", make_patch());
    write_patch(&sweep, "inst-c", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved), // agree
    );
    verdicts.insert(
        "inst-b".to_owned(),
        InstanceParityStub::new(Verdict::Unresolved, Verdict::Unresolved), // agree
    );
    verdicts.insert(
        "inst-c".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Unresolved), // disagree
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();

    assert_eq!(report.summary.instances_compared, 3);
    assert_eq!(report.summary.agreed, 2);
    assert_eq!(report.summary.disagreed, 1);
    let expected_rate = 2.0_f64 / 3.0_f64;
    assert!(
        (report.summary.agreement_rate - expected_rate).abs() < 1e-9,
        "expected rate ≈ {expected_rate}, got {}",
        report.summary.agreement_rate
    );
    assert_eq!(report.disagreements.len(), 1);
    assert_eq!(report.disagreements[0].instance_id, "inst-c");
    assert_eq!(report.disagreements[0].offline_verdict, Verdict::Resolved);
    assert_eq!(
        report.disagreements[0].canonical_verdict,
        Verdict::Unresolved
    );
}

// ── AC6: disagreements are sorted deterministically by instance_id ─────────────

#[test]
fn disagreements_sorted_by_instance_id() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    // Insert in reverse alphabetical order to verify sorting
    write_sweep_results(
        &sweep,
        &[("zzz-inst", true), ("aaa-inst", true), ("mmm-inst", true)],
    );
    write_patch(&sweep, "zzz-inst", make_patch());
    write_patch(&sweep, "aaa-inst", make_patch());
    write_patch(&sweep, "mmm-inst", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "zzz-inst".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Unresolved),
    );
    verdicts.insert(
        "aaa-inst".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Unresolved),
    );
    verdicts.insert(
        "mmm-inst".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Unresolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();

    assert_eq!(report.disagreements.len(), 3);
    assert_eq!(report.disagreements[0].instance_id, "aaa-inst");
    assert_eq!(report.disagreements[1].instance_id, "mmm-inst");
    assert_eq!(report.disagreements[2].instance_id, "zzz-inst");
}

// ── AC4: min_agreement gate — rate meets threshold → agreement_rate >= min ────

#[test]
fn agreement_rate_above_threshold_is_sufficient() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(&sweep, "inst-a", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: Some(0.99),
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();
    // agreement_rate=1.0 >= 0.99, so no gate failure
    assert!(report.summary.agreement_rate >= args.min_agreement.unwrap());
}

// ── AC4: min_agreement gate — rate below threshold is detectable ───────────────

#[test]
fn agreement_rate_below_threshold_is_detectable() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true), ("inst-b", true)]);
    write_patch(&sweep, "inst-a", make_patch());
    write_patch(&sweep, "inst-b", make_patch());

    // 1 of 2 agree → rate=0.5, below threshold 0.99
    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );
    verdicts.insert(
        "inst-b".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Unresolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: Some(0.99),
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();
    // The report reflects the low rate; CLI layer gates on it
    assert!(
        report.summary.agreement_rate < args.min_agreement.unwrap(),
        "expected rate < 0.99, got {}",
        report.summary.agreement_rate
    );
}

// ── AC5: --sample bounds instances; report records sample_size ────────────────

#[test]
fn sample_bounds_instances_evaluated() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    // 5 instances available, sample=2
    let ids = ["inst-a", "inst-b", "inst-c", "inst-d", "inst-e"];
    write_sweep_results(&sweep, &ids.map(|id| (id, true)));
    for id in &ids {
        write_patch(&sweep, id, make_patch());
    }

    let mut verdicts = HashMap::new();
    for id in &ids {
        verdicts.insert(
            (*id).to_owned(),
            InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
        );
    }

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: Some(2),
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();

    assert_eq!(
        report.summary.instances_compared, 2,
        "sample=2 must limit to 2 instances"
    );
    assert_eq!(
        report.summary.sample_size,
        Some(2),
        "report must record sample_size=2"
    );
    assert!(
        report.summary.sample_method.is_some(),
        "report must record sample_method"
    );
}

// ── AC5: report records sample_method=None when no sample limit ──────────────

#[test]
fn no_sample_means_no_sample_size_recorded() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(&sweep, "inst-a", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();
    assert!(
        report.summary.sample_size.is_none(),
        "no --sample must produce sample_size=None"
    );
}

// ── AC7: report records backend identifiers ───────────────────────────────────

#[test]
fn report_records_backend_identifiers() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(&sweep, "inst-a", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        offline_backend_version: Some("0.5.0".to_owned()),
        canonical_backend_version: Some("1.2.3".to_owned()),
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();

    assert!(
        !report.offline_backend.is_empty(),
        "offline_backend must be set"
    );
    assert!(
        !report.canonical_backend.is_empty(),
        "canonical_backend must be set"
    );
    assert_eq!(
        report.offline_backend_version.as_deref(),
        Some("0.5.0"),
        "offline_backend_version must be recorded"
    );
    assert_eq!(
        report.canonical_backend_version.as_deref(),
        Some("1.2.3"),
        "canonical_backend_version must be recorded"
    );
}

// ── AC7: report records dataset_sha256 ────────────────────────────────────────

#[test]
fn report_records_dataset_sha256() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(&sweep, "inst-a", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        dataset_sha256: Some("abc123def456".to_owned()),
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();

    assert_eq!(
        report.dataset_sha256.as_deref(),
        Some("abc123def456"),
        "dataset_sha256 must be propagated to the report"
    );
}

// ── AC8: flakiness note is present in the report ─────────────────────────────

#[test]
fn report_contains_flakiness_note() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(&sweep, "inst-a", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();

    assert!(
        !report.flakiness_note.is_empty(),
        "flakiness_note must be non-empty"
    );
    assert!(
        report.flakiness_note.to_lowercase().contains("flak"),
        "flakiness_note must mention flakiness; got: {}",
        report.flakiness_note
    );
    assert!(
        report.flakiness_note.to_lowercase().contains("eval-flake")
            || report
                .flakiness_note
                .to_lowercase()
                .contains("bench eval-flake"),
        "flakiness_note should reference eval-flake; got: {}",
        report.flakiness_note
    );
}

// ── artifact written to sweep dir ─────────────────────────────────────────────

#[test]
fn artifact_written_to_sweep_dir() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(&sweep, "inst-a", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep.clone(),
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    run_with_stub(&args, &stub).unwrap();

    let artifact_path = sweep.join("eval-parity.json");
    assert!(
        artifact_path.exists(),
        "eval-parity.json must be written to sweep dir"
    );

    let content = std::fs::read_to_string(&artifact_path).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(val["artifact_kind"], "eval_parity_report");
    assert_eq!(val["total_cost_usd"], 0.0);
}

// ── total_cost_usd is always 0.0 ──────────────────────────────────────────────

#[test]
fn total_cost_usd_is_zero() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true)]);
    write_patch(&sweep, "inst-a", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Unresolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();
    assert_eq!(report.total_cost_usd, 0.0_f64, "cost must always be 0.0");
}

// ── instances without a patch file are skipped ────────────────────────────────

#[test]
fn instances_without_patch_skipped() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-patched", true), ("inst-no-patch", true)]);
    write_patch(&sweep, "inst-patched", make_patch());
    // deliberately no patch for inst-no-patch

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-patched".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );
    verdicts.insert(
        "inst-no-patch".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();

    assert_eq!(
        report.summary.instances_compared, 1,
        "only patched instance should be evaluated"
    );
    assert_eq!(report.summary.instances_compared, 1);
}

// ── AC2: JSON fields match spec ───────────────────────────────────────────────

#[test]
fn json_report_contains_required_fields() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true), ("inst-b", true)]);
    write_patch(&sweep, "inst-a", make_patch());
    write_patch(&sweep, "inst-b", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );
    verdicts.insert(
        "inst-b".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Unresolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep.clone(),
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    run_with_stub(&args, &stub).unwrap();

    let content = std::fs::read_to_string(sweep.join("eval-parity.json")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    // Required top-level fields per AC2
    assert!(
        val.get("instances_compared").is_some() || val["summary"]["instances_compared"].is_number(),
        "must have instances_compared"
    );
    assert!(
        val["summary"]["agreed"].is_number(),
        "must have agreed count"
    );
    assert!(
        val["summary"]["disagreed"].is_number(),
        "must have disagreed count"
    );
    assert!(
        val["summary"]["agreement_rate"].is_number(),
        "must have agreement_rate"
    );
    assert!(
        val["disagreements"].is_array(),
        "must have disagreements array"
    );

    // Per-disagreement entry shape
    let disagreements = val["disagreements"].as_array().unwrap();
    assert_eq!(disagreements.len(), 1);
    let d = &disagreements[0];
    assert!(
        d["instance_id"].is_string(),
        "disagreement must have instance_id"
    );
    assert!(
        d["offline_verdict"].is_string(),
        "disagreement must have offline_verdict"
    );
    assert!(
        d["canonical_verdict"].is_string(),
        "disagreement must have canonical_verdict"
    );
}

// ── AC3: render_summary produces human-readable output ───────────────────────

#[test]
fn render_summary_contains_key_metrics() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("sweep");
    std::fs::create_dir_all(&sweep).unwrap();

    write_sweep_results(&sweep, &[("inst-a", true), ("inst-b", true)]);
    write_patch(&sweep, "inst-a", make_patch());
    write_patch(&sweep, "inst-b", make_patch());

    let mut verdicts = HashMap::new();
    verdicts.insert(
        "inst-a".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Resolved),
    );
    verdicts.insert(
        "inst-b".to_owned(),
        InstanceParityStub::new(Verdict::Resolved, Verdict::Unresolved),
    );

    let args = EvalParityArgs {
        sweep_dir: sweep,
        output: None,
        concurrency: 1,
        min_agreement: None,
        sample: None,
        instances: None,
        recheck: 0,
        dataset_path: None,
        sb_subset: None,
    };
    let stub = EvalParityStubConfig {
        verdicts,
        ..Default::default()
    };
    let report = run_with_stub(&args, &stub).unwrap();
    let summary = maxwells_daemon::run::eval_parity::render_summary(&report);

    assert!(
        summary.contains('2') || summary.contains("instances"),
        "summary must mention instance count; got:\n{summary}"
    );
    assert!(
        summary.to_lowercase().contains("agreement") || summary.contains('%'),
        "summary must mention agreement rate; got:\n{summary}"
    );
    assert!(
        summary.contains('1') || summary.to_lowercase().contains("disagree"),
        "summary must mention disagreement count; got:\n{summary}"
    );
}

// ── AC1: bench --help lists eval-parity ──────────────────────────────────────

#[test]
fn eval_parity_subcommand_in_help() {
    let bin = binary_path();
    let out = std::process::Command::new(&bin)
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("eval-parity"),
        "bench --help must list eval-parity subcommand; got:\n{help}"
    );
}

// ── AC1: bench eval-parity --help shows required flags ───────────────────────

#[test]
fn eval_parity_help_shows_required_flags() {
    let bin = binary_path();
    let out = std::process::Command::new(&bin)
        .args(["bench", "eval-parity", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("--sweep"),
        "bench eval-parity --help must show --sweep; got:\n{help}"
    );
    assert!(
        help.contains("--min-agreement"),
        "bench eval-parity --help must show --min-agreement; got:\n{help}"
    );
    assert!(
        help.contains("--sample"),
        "bench eval-parity --help must show --sample; got:\n{help}"
    );
}
