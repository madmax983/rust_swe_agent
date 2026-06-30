//! `bench export-ci`: integration tests.
//!
//! Covers the AC from issue #303:
//!   - subcommand exists in `bench --help`
//!   - `--format junit` writes JUnit XML and exits 0
//!   - resolved instances emit clean `<testcase/>` elements
//!   - unresolved/errored instances emit `<failure>` elements
//!   - `<testsuite>` aggregate attributes match results.json counts
//!   - aggregate mismatch exits with code 22
//!   - `--format github-annotations` writes annotation lines to stdout
//!   - `--format both` produces JUnit XML file and annotations on stdout
//!   - `--output` overrides the default JUnit output path
//!   - exit code is 0 regardless of resolved rate when export succeeds
//!   - snapshot/golden output for a small fixture sweep

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
use maxwells_daemon::trajectory::{FailureCategory, outcome};

mod support;
use support::binary_path;

// ── fixture helpers ────────────────────────────────────────────────────────

fn resolved(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(5),
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
        context_pressure: Default::default(),
    }
}

fn unresolved(id: &str, cat: FailureCategory) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: Some(cat),
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
        context_pressure: Default::default(),
    }
}

fn errored(id: &str, cat: FailureCategory) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "error".into(),
        outcome: Some(outcome::ERROR.into()),
        failure_category: Some(cat),
        steps: Some(3),
        cost_usd: Some(0.05),
        prompt_tokens: Some(500),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(100),
        duration_secs: Some(5.0),
        error: Some("environment setup failed".into()),
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: vec![],
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
        context_pressure: Default::default(),
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
    let failures_by_category: BTreeMap<FailureCategory, usize> = {
        let mut map = BTreeMap::new();
        for inst in &instances {
            if let Some(cat) = inst.failure_category {
                *map.entry(cat).or_insert(0) += 1;
            }
        }
        map
    };
    let submitted = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
        .count();
    let errored_count = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
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
        errored: errored_count,
        failures_by_category,
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

fn run_export_ci(args: &[&str]) -> std::process::Output {
    Command::new(binary_path())
        .args(["--log", "error", "bench", "export-ci"])
        .args(args)
        .output()
        .expect("failed to run bench export-ci")
}

// ── AC: subcommand in --help ───────────────────────────────────────────────

#[test]
fn bench_export_ci_in_help() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("export-ci"),
        "bench --help should list 'export-ci' subcommand\nstdout: {stdout}"
    );
}

// ── AC: junit format creates file and exits 0 ─────────────────────────────

#[test]
fn bench_export_ci_junit_exits_zero() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            resolved("django__django-001"),
            unresolved("django__django-002", FailureCategory::StepLimit),
            errored("django__django-003", FailureCategory::EnvSetup),
        ],
    );

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);

    assert!(
        output.status.success(),
        "bench export-ci junit should exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(out_path.exists(), "junit.xml should be written");
}

// ── AC: junit default output path ─────────────────────────────────────────

#[test]
fn bench_export_ci_junit_default_output_path() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(work.path(), vec![resolved("django__django-001")]);

    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
    ]);

    assert!(
        output.status.success(),
        "bench export-ci junit should exit 0\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let default_path = work.path().join("junit.xml");
    assert!(
        default_path.exists(),
        "default output path <sweep>/junit.xml should be created"
    );
}

// ── AC: resolved → clean testcase, unresolved → failure element ───────────

#[test]
fn bench_export_ci_junit_testcase_structure() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            resolved("django__django-001"),
            unresolved("django__django-002", FailureCategory::StepLimit),
            errored("django__django-003", FailureCategory::EnvSetup),
        ],
    );

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);

    assert!(output.status.success());

    let xml = std::fs::read_to_string(&out_path).unwrap();

    // Resolved instance → no nested <failure> or <error>
    assert!(
        xml.contains("name=\"django__django-001\""),
        "resolved testcase should be present\n{xml}"
    );
    // The resolved testcase should NOT have a failure element
    let resolved_section = extract_testcase_block(&xml, "django__django-001");
    assert!(
        !resolved_section.contains("<failure"),
        "resolved instance should not have <failure> element\n{resolved_section}"
    );
    assert!(
        !resolved_section.contains("<error"),
        "resolved instance should not have <error> element\n{resolved_section}"
    );

    // Unresolved instance → has <failure> element
    let unresolved_section = extract_testcase_block(&xml, "django__django-002");
    assert!(
        unresolved_section.contains("<failure"),
        "unresolved instance should have <failure> element\n{unresolved_section}"
    );

    // Errored instance → has <failure> or <error> element
    let errored_section = extract_testcase_block(&xml, "django__django-003");
    assert!(
        errored_section.contains("<failure") || errored_section.contains("<error"),
        "errored instance should have <failure> or <error> element\n{errored_section}"
    );
}

// ── AC: classname parsed from instance_id ─────────────────────────────────

#[test]
fn bench_export_ci_junit_classname_parsing() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(work.path(), vec![resolved("django__django-12345")]);

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);

    assert!(output.status.success());

    let xml = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        xml.contains("classname=\"django.django\""),
        "classname should be owner.repo parsed from instance_id\n{xml}"
    );
}

// ── AC: aggregate attributes match results.json ───────────────────────────

#[test]
fn bench_export_ci_junit_aggregate_attributes_match() {
    let work = tempfile::tempdir().unwrap();
    let instances = vec![
        resolved("django__django-001"),
        resolved("django__django-002"),
        unresolved("django__django-003", FailureCategory::StepLimit),
        errored("django__django-004", FailureCategory::EnvSetup),
    ];
    let total = instances.len();
    write_sweep(work.path(), instances);

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);

    assert!(output.status.success());

    let xml = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        xml.contains(&format!("tests=\"{total}\"")),
        "tests attribute should match total instances\n{xml}"
    );
    // errors should match errored count (1)
    assert!(
        xml.contains("errors=\"1\""),
        "errors attribute should match errored count\n{xml}"
    );
    // failures = submitted - resolved = 3 - 2 = 1 unresolved submitted
    assert!(
        xml.contains("failures=\"1\""),
        "failures attribute should match unresolved submitted count\n{xml}"
    );
}

// ── AC: aggregate mismatch → exit code 22 ─────────────────────────────────

#[test]
fn bench_export_ci_artifact_integrity_mismatch_exits_22() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            resolved("django__django-001"),
            unresolved("django__django-002", FailureCategory::StepLimit),
        ],
    );

    // Corrupt results.json to introduce a mismatch in total count
    let results_path = work.path().join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
    // Lie about total — real instance count is 2, claim 5
    results["total"] = serde_json::json!(5);
    std::fs::write(
        &results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);

    assert_eq!(
        output.status.code(),
        Some(22),
        "artifact integrity mismatch should exit 22\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── AC: github-annotations exits 0 and writes annotation lines ───────────

#[test]
fn bench_export_ci_github_annotations_exits_zero() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            resolved("django__django-001"),
            unresolved("django__django-002", FailureCategory::StepLimit),
            errored("django__django-003", FailureCategory::EnvSetup),
        ],
    );

    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "github-annotations",
    ]);

    assert!(
        output.status.success(),
        "bench export-ci github-annotations should exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    // Should emit ::error lines for unresolved + errored
    assert!(
        stdout.contains("::error "),
        "should emit ::error annotations\n{stdout}"
    );
    // Resolved instance should NOT produce an annotation
    assert!(
        !stdout.contains("django__django-001"),
        "resolved instance should not appear in annotations\n{stdout}"
    );
    // Unresolved and errored should appear
    assert!(
        stdout.contains("django__django-002"),
        "unresolved instance should appear in annotations\n{stdout}"
    );
    assert!(
        stdout.contains("django__django-003"),
        "errored instance should appear in annotations\n{stdout}"
    );
}

// ── AC: annotations include title= and file= fields ──────────────────────

#[test]
fn bench_export_ci_github_annotations_format() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![unresolved("django__django-002", FailureCategory::StepLimit)],
    );

    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "github-annotations",
    ]);

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("title=django__django-002"),
        "annotation should include title=instance_id\n{stdout}"
    );
    assert!(
        stdout.contains("file="),
        "annotation should include file= field\n{stdout}"
    );
}

// ── AC: --format both emits JUnit XML file and annotations to stdout ──────

#[test]
fn bench_export_ci_both_format() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            resolved("django__django-001"),
            unresolved("django__django-002", FailureCategory::ModelParse),
        ],
    );

    let junit_path = work.path().join("out.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "both",
        "--output",
        junit_path.to_str().unwrap(),
    ]);

    assert!(
        output.status.success(),
        "bench export-ci both should exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // JUnit XML file should exist
    assert!(junit_path.exists(), "JUnit XML file should be written");
    let xml = std::fs::read_to_string(&junit_path).unwrap();
    assert!(xml.starts_with("<?xml"), "should be valid XML\n{xml}");

    // Annotations should be on stdout
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("::error "),
        "should emit ::error annotations to stdout\n{stdout}"
    );
}

// ── AC: exit 0 when export succeeds regardless of resolved rate ────────────

#[test]
fn bench_export_ci_exits_zero_even_when_nothing_resolved() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            unresolved("django__django-001", FailureCategory::StepLimit),
            unresolved("django__django-002", FailureCategory::ModelParse),
            errored("django__django-003", FailureCategory::EnvSetup),
        ],
    );

    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
    ]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "exit 0 when export succeeds regardless of resolved rate\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── AC: failure message uses triage cluster category when available ────────

#[test]
fn bench_export_ci_junit_failure_message_contains_category() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![unresolved("django__django-001", FailureCategory::StepLimit)],
    );

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);

    assert!(output.status.success());

    let xml = std::fs::read_to_string(&out_path).unwrap();
    // The failure message should contain the failure category
    assert!(
        xml.contains("step_limit") || xml.contains("unresolved"),
        "failure message should contain failure category or 'unresolved'\n{xml}"
    );
}

// ── AC: snapshot / golden test ─────────────────────────────────────────────

#[test]
fn bench_export_ci_junit_golden_output() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            resolved("django__django-001"),
            unresolved("django__django-002", FailureCategory::StepLimit),
            errored("django__django-003", FailureCategory::EnvSetup),
        ],
    );

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);
    assert!(output.status.success());

    let xml = std::fs::read_to_string(&out_path).unwrap();

    // Structure: opens with XML declaration
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));

    // Has <testsuites> root element
    assert!(xml.contains("<testsuites "));
    assert!(xml.contains("</testsuites>"));

    // Has <testsuite> child
    assert!(xml.contains("<testsuite "));
    assert!(xml.contains("</testsuite>"));

    // Has <testcase> elements for all 3 instances
    assert_eq!(
        xml.matches("<testcase ").count(),
        3,
        "should have 3 testcase elements\n{xml}"
    );

    // tests="3" in the testsuite element
    assert!(xml.contains("tests=\"3\""), "tests attribute = 3\n{xml}");
}

#[test]
fn bench_export_ci_annotations_golden_output() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            resolved("django__django-001"),
            unresolved("django__django-002", FailureCategory::StepLimit),
            errored("django__django-003", FailureCategory::EnvSetup),
        ],
    );

    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "github-annotations",
    ]);
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();

    // Should have 2 annotation lines (one per non-resolved instance)
    let annotation_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("::error "))
        .collect();
    assert_eq!(
        annotation_lines.len(),
        2,
        "should have 2 annotation lines (one per non-resolved)\n{stdout}"
    );

    // Each annotation line has the expected format
    for line in &annotation_lines {
        assert!(
            line.starts_with("::error "),
            "should start with ::error \n{line}"
        );
        assert!(line.contains("title="), "should contain title=\n{line}");
        assert!(line.contains("file="), "should contain file=\n{line}");
        // format: ::error title=X,file=Y::message
        assert!(line.contains("::"), "should have :: separator\n{line}");
    }
}

// ── AC: redaction — secrets not in output ─────────────────────────────────

#[test]
fn bench_export_ci_secrets_not_in_junit_output() {
    let work = tempfile::tempdir().unwrap();

    // Instance with a "secret" in the error field
    let mut inst = errored("django__django-001", FailureCategory::ModelApi);
    inst.error = Some("API call failed with token ghp_SECRETTOKEN123456789".into());
    write_sweep(work.path(), vec![inst]);

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);

    assert!(output.status.success());

    let xml = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        !xml.contains("ghp_SECRETTOKEN123456789"),
        "GitHub token should be redacted from JUnit output\n{xml}"
    );
}

// ── AC: triage.json data used when available ──────────────────────────────

#[test]
fn bench_export_ci_uses_triage_json_when_available() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![unresolved("django__django-001", FailureCategory::StepLimit)],
    );

    // Write a minimal triage.json
    let triage = serde_json::json!({
        "artifact_kind": "triage_report",
        "schema_version": {"major": 1, "minor": 0},
        "sweep": work.path().to_str().unwrap(),
        "generated_at": "2026-05-01T00:00:00Z",
        "clusters": [
            {
                "cluster_id": "abc123",
                "failure_category": "step_limit",
                "signature_summary": "agent ran out of steps fixing a complex migration",
                "instance_count": 1,
                "total_cost_usd": 0.25,
                "exemplar_instance_id": "django__django-001",
                "exemplar_trajectory_path": "django__django-001.traj.json",
                "instance_ids": ["django__django-001"]
            }
        ],
        "totals": {
            "clusters": 1,
            "instances": 1,
            "unclustered_instances": 0,
            "unresolved_cost_usd": 0.25
        }
    });
    std::fs::write(
        work.path().join("triage.json"),
        serde_json::to_string_pretty(&triage).unwrap(),
    )
    .unwrap();

    let out_path = work.path().join("junit.xml");
    let output = run_export_ci(&[
        "--sweep",
        work.path().to_str().unwrap(),
        "--format",
        "junit",
        "--output",
        out_path.to_str().unwrap(),
    ]);

    assert!(output.status.success());

    let xml = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        xml.contains("step_limit") || xml.contains("agent ran out of steps"),
        "triage data should be reflected in JUnit output\n{xml}"
    );
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Extract the XML block for a given testcase by name attribute.
///
/// Handles both self-closing (`<testcase .../>`) and non-self-closing
/// (`<testcase ...>...</testcase>`) forms.
fn extract_testcase_block(xml: &str, name: &str) -> String {
    let search = format!("name=\"{name}\"");
    let name_pos = xml.find(&search).expect("testcase not found in XML");
    // Find the opening `<testcase ` before the name attribute.
    let from = xml[..name_pos]
        .rfind("<testcase ")
        .expect("no <testcase opening tag found");
    let tag_body = &xml[from..];
    // If self-closing, the tag ends with `/>` before the next `<`.
    // If non-self-closing, it ends with `</testcase>`.
    if let Some(sc) = tag_body.find("/>") {
        let close_tag = tag_body.find("</testcase>");
        let end = match close_tag {
            Some(ct) if ct < sc => ct + "</testcase>".len(),
            _ => sc + "/>".len(),
        };
        xml[from..from + end].to_owned()
    } else {
        let end = tag_body
            .find("</testcase>")
            .map_or(tag_body.len(), |p| p + "</testcase>".len());
        xml[from..from + end].to_owned()
    }
}
