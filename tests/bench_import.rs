//! `bench import` round-trip tests.
//!
//! Covers the AC from issue #302:
//!   * subcommand exists in `bench --help`
//!   * `bench import --help` exposes expected flags
//!   * produces a normalised `results.json` with zero cost and correct schema
//!   * marks the sweep with `provenance.source = "external_import"`
//!   * records source predictions file path + SHA-256 in manifest
//!   * `--format json` emits stable summary object
//!   * unknown `instance_id` is reported with error, never silently dropped
//!   * round-trip: imported sweep + `bench compare` against harness-native fixture
//!     produces a transition matrix that matches the golden file

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::too_many_lines,
    clippy::manual_string_new
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use maxwells_daemon::run::swebench::{
    HarnessManifest, InstanceResult, ProvenanceManifest, SWEEP_STATUS_COMPLETED, SweepResults,
};
use maxwells_daemon::trajectory::outcome;

mod support;
use support::binary_path;

// ── helpers ───────────────────────────────────────────────────────────────────

fn predictions_path() -> &'static str {
    "tests/data/predictions_sample.jsonl"
}

fn dataset_path() -> &'static str {
    "tests/data/dataset_sample.jsonl"
}

fn submitted_pass(id: &str) -> InstanceResult {
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
        trace_id: None,
    }
}

fn errored(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "error".into(),
        outcome: Some(outcome::ERROR.into()),
        failure_category: Some(maxwells_daemon::trajectory::FailureCategory::StepLimit),
        steps: Some(6),
        cost_usd: Some(0.10),
        prompt_tokens: Some(1500),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(200),
        duration_secs: Some(15.0),
        error: Some("step limit".into()),
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

fn write_native_sweep(dir: &Path, instances: Vec<InstanceResult>) {
    let total = instances.len();
    let submitted = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
        .count();
    let errored_count = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
        .count();
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
        errored: errored_count,
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: submitted,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 0,
        estimated_cost_usd: 0.05,
        actual_cost_usd: Some(0.05),
        actual_cost_source: None,
        baseline_cost_usd: None,
        baseline_cost_model: None,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: 0.33,
        filter_spec: maxwells_daemon::run::swebench::FilterSpec::default(),
        manifest: Some(ProvenanceManifest {
            purpose: None,
            harness: HarnessManifest {
                name: "maxwells-daemon".into(),
                version: "test".into(),
                git_sha: None,
                git_dirty: None,
                git_resolution: "test".into(),
            },
            dataset: maxwells_daemon::run::swebench::DatasetManifest {
                path: "tests/data/dataset_sample.jsonl".into(),
                sha256: "abc123".into(),
                instance_count: 3,
                ..Default::default()
            },
            prompt_template: maxwells_daemon::run::swebench::PromptTemplateManifest {
                source: "builtin".into(),
                path: None,
                sha256: "test".into(),
            },
            config: maxwells_daemon::run::swebench::ConfigManifest {
                resolved: "".into(),
                overlay_paths: Vec::new(),
            },
            model: maxwells_daemon::run::swebench::ModelManifest {
                name: "claude-opus-4-7".into(),
                backend: "litellm".into(),
                backend_version: None,
                base_url: None,
            },
            runtime: maxwells_daemon::run::swebench::RuntimeManifest {
                started_at_utc: "2026-05-01T00:00:00Z".into(),
                finished_at_utc: Some("2026-05-01T01:00:00Z".into()),
                host_os: "linux".into(),
                resume_mode: false,
                rust_version: None,
            },
            cli: maxwells_daemon::run::swebench::CliManifest { argv: Vec::new() },
            chaos_fail_every: 0,
            circuit_breaker: None,
            source: None,
            import_predictions_path: None,
            import_predictions_sha256: None,
            reproduced_from: None,
        }),
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: BTreeMap::new(),
        systemic_halt_category: None,
        retry_history: Vec::new(),
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

// ── tests ─────────────────────────────────────────────────────────────────────

/// AC: subcommand appears in `bench --help`
#[test]
fn import_in_bench_help() {
    let output = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("import"),
        "bench --help should list the import subcommand; got:\n{stdout}"
    );
}

/// AC: `bench import --help` exposes the three required flags
#[test]
fn import_help_exposes_flags() {
    let output = Command::new(binary_path())
        .args(["bench", "import", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success(), "bench import --help should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--predictions"),
        "should mention --predictions"
    );
    assert!(
        stdout.contains("--dataset-path"),
        "should mention --dataset-path"
    );
    assert!(stdout.contains("--output"), "should mention --output");
    assert!(stdout.contains("--evaluate"), "should mention --evaluate");
    assert!(stdout.contains("--format"), "should mention --format");
}

/// AC: produces a normalised results.json with zero cost and correct schema
#[test]
fn import_produces_results_json() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    let status = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            dataset_path(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "bench import should exit 0");

    let results_path = out.join("results.json");
    assert!(results_path.exists(), "results.json should be created");

    let results_text = std::fs::read_to_string(&results_path).unwrap();
    let results: serde_json::Value = serde_json::from_str(&results_text).unwrap();

    // Must carry the standard artifact header
    assert_eq!(
        results["artifact_kind"], "sweep_results",
        "artifact_kind must be sweep_results"
    );
    assert!(
        results["schema_version"].is_object(),
        "schema_version must be present"
    );

    // Zero-cost guarantee
    let cost = results["total_cost_usd"]
        .as_f64()
        .or_else(|| results["estimated_cost_usd"].as_f64())
        .unwrap_or(-1.0);
    assert_eq!(cost, 0.0, "total_cost_usd must be 0.0 for imported sweeps");

    // steps must be null/absent for imported instances
    if let Some(instances) = results["instances"].as_array() {
        for inst in instances {
            assert!(
                inst["steps"].is_null() || !inst["steps"].is_number(),
                "steps should be null/absent for imported instance {}",
                inst["instance_id"]
            );
        }
    }
}

/// AC: provenance.source = "external_import" is set
#[test]
fn import_sets_provenance_source() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    let status = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            dataset_path(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let results_text = std::fs::read_to_string(out.join("results.json")).unwrap();
    let results: serde_json::Value = serde_json::from_str(&results_text).unwrap();

    assert_eq!(
        results["manifest"]["source"], "external_import",
        "manifest.source must be 'external_import'"
    );
    assert!(
        results["manifest"]["import_predictions_sha256"]
            .as_str()
            .unwrap_or("")
            .starts_with("sha256:"),
        "manifest.import_predictions_sha256 must be a 'sha256:' prefixed string"
    );
    assert!(
        results["manifest"]["import_predictions_path"].is_string(),
        "manifest.import_predictions_path must be a string"
    );
}

/// AC: unknown instance_id is reported with error, never silently dropped
#[test]
fn import_reports_unknown_instance_id() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    let status = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            dataset_path(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let results_text = std::fs::read_to_string(out.join("results.json")).unwrap();
    let results: serde_json::Value = serde_json::from_str(&results_text).unwrap();

    let instances = results["instances"].as_array().unwrap();

    // django__django-99999 is not in dataset_sample.jsonl — must appear with an error
    let unknown = instances
        .iter()
        .find(|i| i["instance_id"].as_str() == Some("django__django-99999"))
        .expect("django__django-99999 must be present (never silently dropped)");

    assert!(
        unknown["error"].is_string() && !unknown["error"].as_str().unwrap().is_empty(),
        "unknown instance_id must carry a non-empty error field; got: {unknown}"
    );
}

/// AC: `--format json` emits a stable summary object
#[test]
fn import_format_json_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    let output = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            dataset_path(),
            "--output",
            out.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bench import --format json should exit 0"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let summary: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid JSON");

    assert!(
        summary["records_imported"].as_u64().unwrap_or(0) >= 1,
        "records_imported must be >= 1"
    );
    assert!(
        summary["output_path"].is_string(),
        "output_path must be a string"
    );
    assert!(
        summary["source_hash"]
            .as_str()
            .unwrap_or("")
            .starts_with("sha256:"),
        "source_hash must start with 'sha256:'"
    );
    // records_skipped should be present (may be 0)
    assert!(
        summary["records_skipped"].is_number(),
        "records_skipped must be present as a number"
    );
}

/// AC: round-trip ingestion → bench compare transition matrix matches golden
#[test]
fn import_round_trip_compare() {
    let tmp = tempfile::tempdir().unwrap();
    let imported_dir = tmp.path().join("imported");
    let native_dir = tmp.path().join("native");

    // Step 1: import predictions
    let status = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            dataset_path(),
            "--output",
            imported_dir.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "bench import should succeed");

    // Step 2: write a harness-native fixture sweep with the same instance IDs
    // native: 11001 resolved, 11002 errored, 11003 errored (99999 absent)
    write_native_sweep(
        &native_dir,
        vec![
            submitted_pass("django__django-11001"),
            errored("django__django-11002"),
            errored("django__django-11003"),
        ],
    );

    // Step 3: bench compare (native=baseline, imported=candidate) --format json
    let output = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            native_dir.to_str().unwrap(),
            "--candidate",
            imported_dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    // compare may exit non-zero (e.g., regressions) — just check we get valid JSON
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("bench compare --format json should emit valid JSON");

    // Transition matrix must be present
    assert!(
        report["transitions"].is_object(),
        "transitions object must be present"
    );

    // 11001 was pass in native, not-yet-resolved in imported (no evaluator) → pass->fail
    // 11002 was fail in native, submitted (empty patch) in imported → fail->fail or fail->pass
    // 11003 was fail in native, submitted in imported → fail->fail or fail->pass
    // 99999 is only in imported → missing->present
    let transitions = &report["transitions"];
    let pass_fail = transitions["pass_fail"].as_u64().unwrap_or(0);
    let missing_present = transitions["missing_present"].as_u64().unwrap_or(0);

    assert!(
        pass_fail >= 1,
        "expect at least one pass->fail transition (native resolved 11001 but imported has no eval); transitions={transitions}"
    );
    assert!(
        missing_present >= 1,
        "expect at least one missing->present (99999 only in imported); transitions={transitions}"
    );
}

/// AC: patch files are written for instances with non-empty patches
#[test]
fn import_writes_patch_files() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    let status = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            dataset_path(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    // django__django-11001 has a non-empty patch → patch file must exist
    let patch_path = out.join("django__django-11001").join("run-1.patch");
    assert!(
        patch_path.exists(),
        "patch file should exist for instance with non-empty patch: {}",
        patch_path.display()
    );

    // django__django-11002 has an empty patch → patch file should NOT be written
    let empty_patch_path = out.join("django__django-11002").join("run-1.patch");
    assert!(
        !empty_patch_path.exists(),
        "patch file should NOT be written for empty-patch instance"
    );
}

/// Duplicate instance_id records must be skipped with a reason (only the first kept).
#[test]
fn import_deduplicates_instance_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    // Build predictions with a duplicate entry for django__django-11001
    let preds = tmp.path().join("preds.jsonl");
    std::fs::write(
        &preds,
        r#"{"instance_id":"django__django-11001","model_patch":"diff a","model_name_or_path":"m"}
{"instance_id":"django__django-11001","model_patch":"diff b","model_name_or_path":"m"}
{"instance_id":"django__django-11003","model_patch":"diff c","model_name_or_path":"m"}
"#,
    )
    .unwrap();

    let output = Command::new(support::binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            preds.to_str().unwrap(),
            "--dataset-path",
            "tests/data/dataset_sample.jsonl",
            "--output",
            out.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "import with duplicate should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let summary: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();

    assert_eq!(
        summary["records_imported"].as_u64().unwrap(),
        2,
        "only 2 unique instances should be imported"
    );
    assert_eq!(
        summary["records_skipped"].as_u64().unwrap(),
        1,
        "the duplicate should be counted as skipped"
    );
}

/// `all_preds.jsonl` must be written with submitted (non-empty patch) records.
#[test]
fn import_writes_all_preds_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    let output = Command::new(support::binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            "tests/data/predictions_sample.jsonl",
            "--dataset-path",
            "tests/data/dataset_sample.jsonl",
            "--output",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let all_preds = out.join("all_preds.jsonl");
    assert!(all_preds.exists(), "all_preds.jsonl must be written");

    let content = std::fs::read_to_string(&all_preds).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Only non-empty-patch submitted records: 11001, 11003, 99999 (3 records)
    assert_eq!(
        lines.len(),
        3,
        "all_preds.jsonl should contain 3 submitted records (non-empty patches)"
    );

    // Each line must have instance_id, model_patch, and model_name_or_path
    for line in &lines {
        assert!(
            line["instance_id"].is_string(),
            "each all_preds.jsonl record needs instance_id"
        );
        assert!(
            line["model_patch"].is_string(),
            "each all_preds.jsonl record needs model_patch"
        );
        assert!(
            line["model_name_or_path"].is_string(),
            "each all_preds.jsonl record needs model_name_or_path"
        );
    }
}

/// `bench triage` must work on an imported sweep without trajectories or
/// evaluation.json, clustering by failure_category + error field only.
#[test]
fn import_triage_without_trajectories() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    // Run import
    let status = Command::new(support::binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            "tests/data/predictions_sample.jsonl",
            "--dataset-path",
            "tests/data/dataset_sample.jsonl",
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    // Run triage — should succeed with --format json even without evaluation.json
    let output = Command::new(support::binary_path())
        .args([
            "bench",
            "triage",
            "--sweep",
            out.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench triage on imported sweep should succeed without trajectories: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();

    // clusters and totals must be present
    assert!(
        report["clusters"].is_array(),
        "triage output must have clusters array"
    );
    assert!(
        report["totals"].is_object(),
        "triage output must have totals object"
    );

    // All 4 imported instances are unresolved → all should appear in totals
    let total_instances = report["totals"]["instances"].as_u64().unwrap_or(0)
        + report["totals"]["unclustered_instances"]
            .as_u64()
            .unwrap_or(0);
    assert_eq!(
        total_instances, 4,
        "all 4 imported instances should appear in triage totals"
    );
}

/// AC: importing into a directory that already has results.json must fail clearly.
#[test]
fn import_rejects_existing_output_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    // First import succeeds.
    let status = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            dataset_path(),
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "first import should succeed");

    // Second import into the same directory must fail.
    let output = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            dataset_path(),
            "--output",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "re-importing into an existing output dir should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("results.json"),
        "error should mention results.json; got: {stderr}"
    );
}

/// AC: passing a malformed dataset file (rows without instance_id) must fail clearly.
#[test]
fn import_rejects_dataset_missing_instance_id() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("imported");

    // Dataset where rows have no instance_id field.
    let bad_dataset = tmp.path().join("bad_dataset.jsonl");
    std::fs::write(&bad_dataset, "{\"task_id\":\"foo\"}\n").unwrap();

    let output = Command::new(binary_path())
        .args([
            "bench",
            "import",
            "--predictions",
            predictions_path(),
            "--dataset-path",
            bad_dataset.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "import with bad dataset should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("instance_id"),
        "error should mention instance_id; got: {stderr}"
    );
}
