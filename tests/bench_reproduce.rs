//! Tests for `bench reproduce`: manifest loading, drift detection,
//! reproducibility report generation, and CLI arg parsing.
//!
//! Red phase: these tests are written before the implementation exists.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use maxwells_daemon::run::reproduce::{
    DriftSeverity, build_reproducibility_report, compare_manifests, load_manifest_from_sweep,
    render_summary,
};
use maxwells_daemon::run::swebench::{
    HarnessManifest, InstanceResult, ProvenanceManifest, SweepResults,
};

// ── helpers ────────────────────────────────────────────────────────────────

fn write_results_json(dir: &Path, results: &SweepResults) {
    let text = serde_json::to_string_pretty(results).unwrap();
    std::fs::write(dir.join("results.json"), text).unwrap();
}

fn minimal_manifest(model_name: &str, git_sha: Option<&str>) -> ProvenanceManifest {
    ProvenanceManifest {
        purpose: None,
        harness: HarnessManifest {
            name: "max".into(),
            version: "0.1.0".into(),
            git_sha: git_sha.map(str::to_owned),
            git_dirty: Some(false),
            git_resolution: "exact".into(),
        },
        dataset: maxwells_daemon::run::swebench::DatasetManifest {
            path: "dataset.jsonl".into(),
            sha256: "abc123".into(),
            instance_count: 10,
            filter_spec: None,
            source_kind: "local".into(),
            alias: None,
            split: None,
            source_revision: None,
            cache_path: None,
            selected_row_count: 10,
            post_filter_row_count: 10,
        },
        prompt_template: maxwells_daemon::run::swebench::PromptTemplateManifest {
            source: "builtin".into(),
            path: None,
            sha256: "deadbeef".into(),
        },
        config: maxwells_daemon::run::swebench::ConfigManifest {
            resolved: "[model]\nname = \"claude-opus-4-7\"\n".into(),
            overlay_paths: vec![],
        },
        model: maxwells_daemon::run::swebench::ModelManifest {
            name: model_name.into(),
            backend: "litellm".into(),
            backend_version: None,
            base_url: None,
        },
        runtime: maxwells_daemon::run::swebench::RuntimeManifest {
            started_at_utc: "2026-01-01T00:00:00Z".into(),
            finished_at_utc: Some("2026-01-01T01:00:00Z".into()),
            host_os: "linux".into(),
            resume_mode: false,
            rust_version: Some("1.85.0".into()),
        },
        cli: maxwells_daemon::run::swebench::CliManifest {
            argv: vec!["max".into(), "bench".into(), "swebench".into()],
        },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
        merged_from: None,
    }
}

fn instance_result(id: &str, resolved: bool) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: if resolved {
            "submitted".into()
        } else {
            "step_limit_reached".into()
        },
        outcome: Some(
            if resolved {
                "submitted"
            } else {
                "step_limit_reached"
            }
            .into(),
        ),
        failure_category: if resolved {
            None
        } else {
            Some(maxwells_daemon::trajectory::FailureCategory::StepLimit)
        },
        steps: Some(5),
        cost_usd: Some(0.01),
        prompt_tokens: Some(100),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(50),
        duration_secs: Some(2.0),
        error: None,
        github_pr_error: None,
        patch_present: resolved,
        non_empty_patch: resolved,
        attempts: 1,
        retry_reasons: vec![],
        runs: 1,
        resolved_count: u32::from(resolved),
        pass_at_1: resolved,
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

fn minimal_sweep_results(manifest: Option<ProvenanceManifest>) -> SweepResults {
    SweepResults {
        total: 0,
        sweep_status: "completed".into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 0,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: Default::default(),
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
        filter_spec: Default::default(),
        manifest,
        cost_limit_usd: None,
        instances: vec![],
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: Default::default(),
        systemic_halt_category: None,
        retry_history: vec![],
        partial: 0,
        span_export_dropped: 0,
    }
}

// ── load_manifest_from_sweep ───────────────────────────────────────────────

#[test]
fn load_manifest_rejects_missing_results_json() {
    let dir = tempfile::tempdir().unwrap();
    let err = load_manifest_from_sweep(dir.path()).unwrap_err();
    let msg = err.to_string();
    // Should fail with an I/O or config error — not panic
    assert!(
        msg.contains("results.json") || msg.contains("No such file") || msg.contains("os error"),
        "unexpected error: {msg}"
    );
}

#[test]
fn load_manifest_rejects_results_without_manifest_block() {
    let dir = tempfile::tempdir().unwrap();
    let results = minimal_sweep_results(None);
    write_results_json(dir.path(), &results);

    let err = load_manifest_from_sweep(dir.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("manifest") || msg.contains("no provenance"),
        "unexpected error: {msg}"
    );
}

#[test]
fn load_manifest_returns_manifest_from_valid_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = minimal_manifest("claude-opus-4-7", Some("abc123sha"));
    let results = minimal_sweep_results(Some(manifest));
    write_results_json(dir.path(), &results);

    let loaded = load_manifest_from_sweep(dir.path()).unwrap();
    assert_eq!(loaded.model.name, "claude-opus-4-7");
    assert_eq!(loaded.harness.git_sha.as_deref(), Some("abc123sha"));
}

// ── compare_manifests ──────────────────────────────────────────────────────

#[test]
fn compare_manifests_finds_no_drift_when_identical() {
    let m = minimal_manifest("claude-opus-4-7", Some("deadbeef"));
    let drifts = compare_manifests(&m, &m);
    assert!(drifts.is_empty(), "expected no drift, got: {drifts:?}");
}

#[test]
fn compare_manifests_flags_harness_sha_mismatch_as_hard_drift() {
    let original = minimal_manifest("claude-opus-4-7", Some("sha-original"));
    let current = minimal_manifest("claude-opus-4-7", Some("sha-different"));
    let drifts = compare_manifests(&original, &current);

    let sha_drift = drifts
        .iter()
        .find(|d| d.field.contains("git_sha"))
        .expect("expected a harness.git_sha drift entry");

    assert_eq!(sha_drift.severity, DriftSeverity::Hard);
    assert!(
        sha_drift.message.contains("sha-original"),
        "message should include original SHA"
    );
    assert!(
        sha_drift.message.contains("sha-different"),
        "message should include current SHA"
    );
}

#[test]
fn compare_manifests_skips_sha_drift_when_either_sha_is_absent() {
    let original = minimal_manifest("claude-opus-4-7", None);
    let current = minimal_manifest("claude-opus-4-7", Some("some-sha"));
    let drifts = compare_manifests(&original, &current);
    assert!(
        !drifts.iter().any(|d| d.field.contains("git_sha")),
        "should not flag SHA when original has no SHA"
    );
}

#[test]
fn compare_manifests_flags_dataset_hash_mismatch_as_hard_drift() {
    let original = minimal_manifest("claude-opus-4-7", Some("sha"));
    let mut current = original.clone();
    current.dataset.sha256 = "different_hash".into();

    let drifts = compare_manifests(&original, &current);
    let hash_drift = drifts
        .iter()
        .find(|d| d.field.contains("sha256"))
        .expect("expected a dataset.sha256 drift entry");

    assert_eq!(hash_drift.severity, DriftSeverity::Hard);
}

#[test]
fn compare_manifests_flags_model_name_mismatch_as_hard_drift() {
    let original = minimal_manifest("claude-opus-4-7", Some("sha"));
    let current = minimal_manifest("claude-sonnet-4-6", Some("sha"));
    let drifts = compare_manifests(&original, &current);

    let model_drift = drifts
        .iter()
        .find(|d| d.field.contains("model"))
        .expect("expected a model.name drift entry");

    assert_eq!(model_drift.severity, DriftSeverity::Hard);
    assert!(model_drift.message.contains("claude-opus-4-7"));
    assert!(model_drift.message.contains("claude-sonnet-4-6"));
}

#[test]
fn drift_field_is_whitelisted_when_field_in_allow_list() {
    use maxwells_daemon::run::reproduce::DriftField;

    let field = DriftField {
        field: "harness.git_sha".into(),
        severity: DriftSeverity::Hard,
        source_value: Some("abc".into()),
        current_value: Some("def".into()),
        message: "SHA mismatch".into(),
    };

    assert!(field.is_whitelisted(&["harness.git_sha".into()]));
    assert!(!field.is_whitelisted(&["dataset.sha256".into()]));
    assert!(!field.is_whitelisted(&[]));
}

// ── build_reproducibility_report ──────────────────────────────────────────

#[test]
fn build_report_counts_matched_resolved_instances() {
    let dir = tempfile::tempdir().unwrap();
    let source_dir = dir.path().to_path_buf();

    let originals = vec![
        instance_result("task-1", true),
        instance_result("task-2", false),
    ];
    let replays = vec![
        instance_result("task-1", true),
        instance_result("task-2", false),
    ];

    let report = build_reproducibility_report(
        &source_dir,
        "sha256:abc".into(),
        &originals,
        &replays,
        &source_dir,
    );

    assert_eq!(report.instances.len(), 2);
    // task-1: both resolved → matched
    assert_eq!(report.aggregate.matched, 1);
    // task-2: both unresolved, same category → both_unresolved_same_category
    assert_eq!(report.aggregate.both_unresolved_same_category, 1);
    assert_eq!(report.aggregate.flipped_to_resolved, 0);
    assert_eq!(report.aggregate.flipped_to_unresolved, 0);
}

#[test]
fn build_report_counts_flipped_to_resolved() {
    let dir = tempfile::tempdir().unwrap();

    let originals = vec![instance_result("task-1", false)];
    let replays = vec![instance_result("task-1", true)];

    let report = build_reproducibility_report(
        dir.path(),
        "sha256:abc".into(),
        &originals,
        &replays,
        dir.path(),
    );

    assert_eq!(report.aggregate.flipped_to_resolved, 1);
    assert_eq!(report.aggregate.matched, 0);
}

#[test]
fn build_report_counts_flipped_to_unresolved() {
    let dir = tempfile::tempdir().unwrap();

    let originals = vec![instance_result("task-1", true)];
    let replays = vec![instance_result("task-1", false)];

    let report = build_reproducibility_report(
        dir.path(),
        "sha256:abc".into(),
        &originals,
        &replays,
        dir.path(),
    );

    assert_eq!(report.aggregate.flipped_to_unresolved, 1);
    assert_eq!(report.aggregate.matched, 0);
}

#[test]
fn build_report_marks_errored_for_missing_replay_instance() {
    let dir = tempfile::tempdir().unwrap();

    let originals = vec![
        instance_result("task-1", true),
        instance_result("task-2", false),
    ];
    // replay only has task-1; task-2 is missing
    let replays = vec![instance_result("task-1", true)];

    let report = build_reproducibility_report(
        dir.path(),
        "sha256:abc".into(),
        &originals,
        &replays,
        dir.path(),
    );

    // task-1 matches, task-2 is missing in replay → errored
    assert_eq!(report.aggregate.matched, 1);
    assert_eq!(report.aggregate.errored, 1);
}

#[test]
fn build_report_records_reproduced_from_block() {
    let dir = tempfile::tempdir().unwrap();
    let source_dir = dir.path().to_path_buf();

    let report =
        build_reproducibility_report(&source_dir, "sha256:deadbeef".into(), &[], &[], &source_dir);

    assert_eq!(report.reproduced_from.manifest_hash, "sha256:deadbeef");
    assert_eq!(
        report.reproduced_from.sweep_dir,
        source_dir.display().to_string()
    );
}

// ── render_summary ─────────────────────────────────────────────────────────

#[test]
fn render_summary_includes_total_instance_count() {
    let dir = tempfile::tempdir().unwrap();
    let originals = vec![
        instance_result("t1", true),
        instance_result("t2", true),
        instance_result("t3", false),
    ];
    let replays = originals.clone();
    let report = build_reproducibility_report(
        dir.path(),
        "sha256:x".into(),
        &originals,
        &replays,
        dir.path(),
    );
    let summary = render_summary(&report);
    assert!(
        summary.contains('3') || summary.contains("3 instance"),
        "summary should mention total instances: {summary}"
    );
}

#[test]
fn render_summary_includes_percent_matched() {
    let dir = tempfile::tempdir().unwrap();
    let originals = vec![instance_result("t1", true), instance_result("t2", true)];
    let replays = originals.clone();
    let report = build_reproducibility_report(
        dir.path(),
        "sha256:x".into(),
        &originals,
        &replays,
        dir.path(),
    );
    let summary = render_summary(&report);
    // 2/2 both resolved → 100% matched
    assert!(
        summary.contains("100") || summary.contains("100.0"),
        "summary should show 100% match: {summary}"
    );
}

#[test]
fn render_summary_includes_patch_identical_percentage() {
    let dir = tempfile::tempdir().unwrap();
    let originals = vec![instance_result("t1", true)];
    let replays = vec![instance_result("t1", true)];
    let report = build_reproducibility_report(
        dir.path(),
        "sha256:x".into(),
        &originals,
        &replays,
        dir.path(),
    );
    let summary = render_summary(&report);
    // summary must mention patch-identical percentage
    assert!(
        summary.contains("patch"),
        "summary should mention patch-identical: {summary}"
    );
}

// ── CLI arg parsing ────────────────────────────────────────────────────────

#[test]
fn cli_parses_bench_reproduce_required_args() {
    use clap::Parser as _;
    use maxwells_daemon::cli::Cli;
    use maxwells_daemon::cli::args::BenchCmd;

    let cli = Cli::parse_from([
        "max",
        "bench",
        "reproduce",
        "--from",
        "/tmp/source-sweep",
        "--output",
        "/tmp/replay-sweep",
    ]);

    let maxwells_daemon::cli::Command::Bench { cmd } = cli.command else {
        panic!("expected bench reproduce command, got something else");
    };
    let BenchCmd::Reproduce(cmd) = *cmd else {
        panic!("expected bench reproduce command, got something else");
    };

    assert_eq!(cmd.from.to_str().unwrap(), "/tmp/source-sweep");
    assert_eq!(cmd.output.to_str().unwrap(), "/tmp/replay-sweep");
    assert!(cmd.allow_drift.is_empty());
    assert!(cmd.limit.is_none());
}

#[test]
fn cli_parses_bench_reproduce_optional_overrides() {
    use clap::Parser as _;
    use maxwells_daemon::cli::Cli;
    use maxwells_daemon::cli::args::BenchCmd;

    let cli = Cli::parse_from([
        "max",
        "bench",
        "reproduce",
        "--from",
        "/tmp/src",
        "--output",
        "/tmp/dst",
        "--allow-drift",
        "harness.git_sha",
        "--limit",
        "5",
        "--skip-model-probe",
    ]);

    let maxwells_daemon::cli::Command::Bench { cmd } = cli.command else {
        panic!("expected bench reproduce");
    };
    let BenchCmd::Reproduce(cmd) = *cmd else {
        panic!("expected bench reproduce");
    };

    assert_eq!(cmd.allow_drift, vec!["harness.git_sha"]);
    assert_eq!(cmd.limit, Some(5));
    assert!(cmd.skip_model_probe);
}

// ── hard drift abort logic ─────────────────────────────────────────────────

#[test]
fn unwhitelisted_hard_drifts_are_reported() {
    use maxwells_daemon::run::reproduce::filter_hard_drifts;
    let original = minimal_manifest("claude-opus-4-7", Some("sha-a"));
    let current = minimal_manifest("claude-opus-4-7", Some("sha-b"));
    let drifts = compare_manifests(&original, &current);

    let hard = filter_hard_drifts(&drifts, &[]);
    assert_eq!(hard.len(), 1);
    assert_eq!(hard[0].field, "harness.git_sha");
}

#[test]
fn whitelisted_hard_drifts_are_excluded() {
    use maxwells_daemon::run::reproduce::filter_hard_drifts;
    let original = minimal_manifest("claude-opus-4-7", Some("sha-a"));
    let current = minimal_manifest("claude-opus-4-7", Some("sha-b"));
    let drifts = compare_manifests(&original, &current);

    let hard = filter_hard_drifts(&drifts, &["harness.git_sha".into()]);
    assert!(hard.is_empty(), "expected empty after whitelist: {hard:?}");
}

// ── Gap 1: config hash hard drift ──────────────────────────────────────────

#[test]
fn compare_manifests_flags_config_resolved_mismatch_as_hard_drift() {
    let original = minimal_manifest("claude-opus-4-7", Some("sha"));
    let mut current = original.clone();
    current.config.resolved = "[model]\nname = \"claude-sonnet-4-6\"\n".into();

    let drifts = compare_manifests(&original, &current);
    let config_drift = drifts
        .iter()
        .find(|d| d.field == "config.resolved")
        .expect("expected config.resolved drift");

    assert_eq!(config_drift.severity, DriftSeverity::Hard);
}

#[test]
fn compare_manifests_no_drift_when_config_unchanged() {
    let m = minimal_manifest("claude-opus-4-7", Some("sha"));
    let drifts = compare_manifests(&m, &m);
    assert!(
        drifts.iter().all(|d| d.field != "config.resolved"),
        "should not flag config drift when resolved config is identical"
    );
}

// ── Gap 1: soft drifts ─────────────────────────────────────────────────────

#[test]
fn compare_manifests_flags_rust_version_change_as_soft_drift() {
    let original = minimal_manifest("claude-opus-4-7", Some("sha"));
    let mut current = original.clone();
    current.runtime.rust_version = Some("1.90.0".into());

    let drifts = compare_manifests(&original, &current);
    let rv_drift = drifts
        .iter()
        .find(|d| d.field == "runtime.rust_version")
        .expect("expected runtime.rust_version drift");

    assert_eq!(rv_drift.severity, DriftSeverity::Soft);
    assert!(
        rv_drift.message.contains("1.85.0"),
        "message: {}",
        rv_drift.message
    );
    assert!(
        rv_drift.message.contains("1.90.0"),
        "message: {}",
        rv_drift.message
    );
}

#[test]
fn compare_manifests_flags_host_os_change_as_soft_drift() {
    let original = minimal_manifest("claude-opus-4-7", Some("sha"));
    let mut current = original.clone();
    current.runtime.host_os = "macos".into();

    let drifts = compare_manifests(&original, &current);
    let os_drift = drifts
        .iter()
        .find(|d| d.field == "runtime.host_os")
        .expect("expected runtime.host_os drift");

    assert_eq!(os_drift.severity, DriftSeverity::Soft);
    assert!(os_drift.message.contains("linux") && os_drift.message.contains("macos"));
}

#[test]
fn soft_drifts_are_excluded_from_filter_hard_drifts() {
    use maxwells_daemon::run::reproduce::filter_hard_drifts;
    let original = minimal_manifest("claude-opus-4-7", Some("sha"));
    let mut current = original.clone();
    current.runtime.rust_version = Some("1.99.0".into());

    let drifts = compare_manifests(&original, &current);
    let hard = filter_hard_drifts(&drifts, &[]);
    assert!(
        hard.iter().all(|d| d.field != "runtime.rust_version"),
        "soft drift should not appear in filter_hard_drifts output"
    );
}

// ── Gap 2: top-3 diverging failure categories in render_summary ────────────

#[test]
fn render_summary_lists_top_diverging_failure_categories() {
    use maxwells_daemon::run::reproduce::InstanceComparisonEntry;
    use maxwells_daemon::run::reproduce::top_diverging_failure_categories;

    let instances = vec![
        // flipped to unresolved → replay_failure_category = step_limit
        InstanceComparisonEntry {
            instance_id: "t1".into(),
            original_resolved: true,
            replay_resolved: false,
            original_failure_category: None,
            replay_failure_category: Some("step_limit".into()),
            patch_identical: false,
        },
        InstanceComparisonEntry {
            instance_id: "t2".into(),
            original_resolved: true,
            replay_resolved: false,
            original_failure_category: None,
            replay_failure_category: Some("step_limit".into()),
            patch_identical: false,
        },
        // flipped to resolved → original_failure_category = model_api
        InstanceComparisonEntry {
            instance_id: "t3".into(),
            original_resolved: false,
            replay_resolved: true,
            original_failure_category: Some("model_api".into()),
            replay_failure_category: None,
            patch_identical: false,
        },
    ];

    let top = top_diverging_failure_categories(&instances, 3);
    // step_limit appears 2×, model_api 1×
    assert_eq!(top[0].0, "step_limit");
    assert_eq!(top[0].1, 2);
    assert_eq!(top[1].0, "model_api");
    assert_eq!(top[1].1, 1);
}

#[test]
fn render_summary_includes_top_categories_section_when_divergences_exist() {
    let dir = tempfile::tempdir().unwrap();

    let originals = vec![instance_result("t1", true), instance_result("t2", true)];
    let replays = vec![
        instance_result("t1", false), // flipped to unresolved
        instance_result("t2", true),
    ];

    let report = build_reproducibility_report(
        dir.path(),
        "sha256:x".into(),
        &originals,
        &replays,
        dir.path(),
    );
    let summary = render_summary(&report);

    assert!(
        summary.contains("step_limit") || summary.contains("diverging"),
        "summary should mention diverging failure categories: {summary}"
    );
}

#[test]
fn render_summary_omits_top_categories_when_no_divergences() {
    let dir = tempfile::tempdir().unwrap();
    let instances = vec![instance_result("t1", true), instance_result("t2", false)];
    // replay identical to original → no divergences
    let report = build_reproducibility_report(
        dir.path(),
        "sha256:x".into(),
        &instances,
        &instances,
        dir.path(),
    );
    let summary = render_summary(&report);
    // No "top diverging" line expected when there are no diverging instances
    assert!(
        !summary.contains("top diverging failure categories:"),
        "no diverging categories expected: {summary}"
    );
}

// ── Gap 3: reproduced_from in ProvenanceManifest ───────────────────────────

#[test]
fn manifest_reproduced_from_field_serializes_and_deserializes() {
    use maxwells_daemon::run::swebench::ManifestReproducedFrom;

    let reproduced = ManifestReproducedFrom {
        manifest_hash: "manifest-hash:deadbeef".into(),
        sweep_dir: "/tmp/source-sweep".into(),
    };

    let mut manifest = minimal_manifest("claude-opus-4-7", Some("sha"));
    manifest.reproduced_from = Some(reproduced);

    let json = serde_json::to_string_pretty(&manifest).unwrap();
    assert!(
        json.contains("reproduced_from"),
        "reproduced_from should appear in JSON"
    );
    assert!(json.contains("manifest-hash:deadbeef"));

    let roundtrip: ProvenanceManifest = serde_json::from_str(&json).unwrap();
    let rf = roundtrip
        .reproduced_from
        .expect("reproduced_from should survive round-trip");
    assert_eq!(rf.manifest_hash, "manifest-hash:deadbeef");
    assert_eq!(rf.sweep_dir, "/tmp/source-sweep");
}

#[test]
fn manifest_reproduced_from_is_absent_for_normal_sweeps() {
    let manifest = minimal_manifest("claude-opus-4-7", Some("sha"));
    assert!(
        manifest.reproduced_from.is_none(),
        "normal (non-reproduce) sweep manifests must not have reproduced_from"
    );
    // Also verify it's omitted from JSON (skip_serializing_if = Option::is_none)
    let json = serde_json::to_string(&manifest).unwrap();
    assert!(
        !json.contains("reproduced_from"),
        "reproduced_from key should be absent when None: {json}"
    );
}

#[test]
fn sweep_results_with_reproduced_from_roundtrips_through_json() {
    use maxwells_daemon::run::swebench::ManifestReproducedFrom;

    let mut manifest = minimal_manifest("claude-opus-4-7", Some("sha"));
    manifest.reproduced_from = Some(ManifestReproducedFrom {
        manifest_hash: "manifest-hash:abc123".into(),
        sweep_dir: "/runs/sweep-1".into(),
    });

    let results = minimal_sweep_results(Some(manifest));
    let json = serde_json::to_string_pretty(&results).unwrap();
    let loaded: SweepResults = serde_json::from_str(&json).unwrap();
    let rf = loaded
        .manifest
        .expect("manifest")
        .reproduced_from
        .expect("reproduced_from");
    assert_eq!(rf.manifest_hash, "manifest-hash:abc123");
}
