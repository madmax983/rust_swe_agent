//! Integration tests for `bench dataset-stats` subcommand (issue #281).
//!
//! RED-phase: these tests verify the required behavior of `bench dataset-stats`.
//! They will fail to compile or run until the GREEN phase implements the subcommand.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::cloned_ref_to_slice_refs
)]

use std::path::PathBuf;

// We will simulate running the subcommand via argument parsing or calling a public entry point if we expose it,
// or by mocking the core calculator in `maxwells_daemon::run::dataset_stats`.
// Let's test the statistical logic directly to ensure maximum TDD precision, and then verify the CLI integration.

use maxwells_daemon::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest as SweBenchDatasetManifest, HarnessManifest,
    InstanceResult, ModelManifest, PromptTemplateManifest, ProvenanceManifest, RuntimeManifest,
    SweBenchInstance, SweepResults,
};

#[test]
fn test_mock_dataset_stats_computation() {
    // Write mock instances representing different repos, problem statement lengths, and expected tests
    let mut other_map1 = serde_json::Map::new();
    other_map1.insert(
        "FAIL_TO_PASS".into(),
        serde_json::json!(["test_a", "test_b"]),
    );
    other_map1.insert("PASS_TO_PASS".into(), serde_json::json!(["test_c"]));
    other_map1.insert(
        "patch".into(),
        serde_json::json!(
            "--- a/django/contrib/admin/options.py\n+++ b/django/contrib/admin/options.py\n"
        ),
    );

    let mut other_map2 = serde_json::Map::new();
    other_map2.insert("FAIL_TO_PASS".into(), serde_json::json!(["test_d"]));
    other_map2.insert(
        "patch".into(),
        serde_json::json!("--- a/pytest/main.py\n+++ b/pytest/main.py\n"),
    );

    let inst1 = SweBenchInstance {
        instance_id: "inst-1".into(),
        repo: Some("django/django".into()),
        base_commit: Some("commit1".into()),
        problem_statement: Some("Short problem statement here".into()), // 4 words
        image: None,
        other: other_map1,
    };

    let inst2 = SweBenchInstance {
        instance_id: "inst-2".into(),
        repo: Some("pytest-dev/pytest".into()),
        base_commit: Some("commit2".into()),
        problem_statement: Some("This is a much longer problem statement designed to test distribution calculations properly.".into()), // 13 words
        image: None,
        other: other_map2,
    };

    let slice_instances = vec![inst1.clone(), inst2.clone()];
    let full_instances = vec![inst1, inst2];

    // Core calculator test
    let stats = maxwells_daemon::run::dataset_stats::compute_stats(
        &slice_instances,
        &full_instances,
        "gpt-4",
        &PathBuf::from("/nonexistent-runs"),
        &None, // dataset hash
    )
    .unwrap();

    assert_eq!(stats.total_instances, 2);
    assert_eq!(stats.repos.len(), 2);
    assert!(stats.languages.contains(&"Python".to_string()));

    // Verify token distribution (using litellm-rs TokenCounter)
    // gpt-4 has overhead + tokens.
    assert!(stats.problem_statement_tokens.min > 0);
    assert!(stats.problem_statement_tokens.max >= stats.problem_statement_tokens.min);

    // Verify expected tests distribution (3 for inst1, 1 for inst2)
    assert_eq!(stats.expected_tests.min, 1);
    assert_eq!(stats.expected_tests.max, 3);
    assert_eq!(stats.expected_tests.p90, 3);
}

#[test]
fn test_skew_detection_repo_coverage() {
    let mut other_map = serde_json::Map::new();
    other_map.insert(
        "patch".into(),
        serde_json::json!("--- a/django/contrib/admin/options.py\n"),
    );

    let inst1 = SweBenchInstance {
        instance_id: "inst-1".into(),
        repo: Some("django/django".into()),
        base_commit: Some("commit1".into()),
        problem_statement: Some("test".into()),
        image: None,
        other: other_map.clone(),
    };
    let inst2 = SweBenchInstance {
        instance_id: "inst-2".into(),
        repo: Some("pytest-dev/pytest".into()),
        base_commit: Some("commit2".into()),
        problem_statement: Some("test".into()),
        image: None,
        other: other_map.clone(),
    };
    let inst3 = SweBenchInstance {
        instance_id: "inst-3".into(),
        repo: Some("pandas-dev/pandas".into()),
        base_commit: Some("commit3".into()),
        problem_statement: Some("test".into()),
        image: None,
        other: other_map.clone(),
    };

    let slice_instances = vec![inst1.clone()];
    let full_instances = vec![inst1, inst2, inst3];

    let stats = maxwells_daemon::run::dataset_stats::compute_stats(
        &slice_instances,
        &full_instances,
        "gpt-4",
        &PathBuf::from("/nonexistent-runs"),
        &None,
    )
    .unwrap();

    assert!(
        stats.slice_skew,
        "Slice skew must be true due to low repo coverage (<50%)"
    );
}

#[test]
fn test_skew_detection_token_length() {
    let mut other_map = serde_json::Map::new();
    other_map.insert(
        "patch".into(),
        serde_json::json!("--- a/django/contrib/admin/options.py\n"),
    );

    let inst1 = SweBenchInstance {
        instance_id: "inst-1".into(),
        repo: Some("django/django".into()),
        base_commit: Some("commit1".into()),
        problem_statement: Some("short".into()), // 1 token
        image: None,
        other: other_map.clone(),
    };
    let inst2 = SweBenchInstance {
        instance_id: "inst-2".into(),
        repo: Some("django/django".into()),
        base_commit: Some("commit2".into()),
        problem_statement: Some(
            "this is a much longer problem statement designed to skew the median length difference"
                .into(),
        ), // 13 tokens
        image: None,
        other: other_map.clone(),
    };

    let slice_instances = vec![inst1.clone()]; // median length = 1
    let full_instances = vec![inst1, inst2]; // median length = 7. Difference is >25%.

    let stats = maxwells_daemon::run::dataset_stats::compute_stats(
        &slice_instances,
        &full_instances,
        "gpt-4",
        &PathBuf::from("/nonexistent-runs"),
        &None,
    )
    .unwrap();

    assert!(
        stats.slice_skew,
        "Slice skew must be true due to median token length deviation (>25%)"
    );
}

#[test]
fn test_language_detection_case_insensitivity() {
    let mut other_map = serde_json::Map::new();
    other_map.insert(
        "patch".into(),
        serde_json::json!("--- a/src/main.Rs\n+++ b/src/main.Rs\n"),
    );

    let inst = SweBenchInstance {
        instance_id: "inst-1".into(),
        repo: Some("test/repo".into()),
        base_commit: Some("commit1".into()),
        problem_statement: Some("test".into()),
        image: None,
        other: other_map,
    };

    let stats = maxwells_daemon::run::dataset_stats::compute_stats(
        &[inst.clone()],
        &[inst],
        "gpt-4",
        &PathBuf::from("/nonexistent-runs"),
        &None,
    )
    .unwrap();

    assert!(
        stats.languages.contains(&"Rust".to_string()),
        "Languages list should contain Rust even if extension is mixed-case (.Rs): {:?}",
        stats.languages
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_historical_resolved_rates_legacy_and_hash_handling() {
    use std::fs::{create_dir_all, write};
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let runs_dir = dir.path().join("runs");
    create_dir_all(&runs_dir).unwrap();

    // 1. Construct legacy SweepResults
    let legacy_results = SweepResults {
        total: 1,
        sweep_status: "completed".to_string(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 1,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: 1,
        submitted_with_tests: 0,
        skipped: 0,
        errored: 0,
        failures_by_category: Default::default(),
        budget_halted: 0,
        with_patch: 1,
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
        manifest: None,
        cost_limit_usd: None,
        instances: vec![InstanceResult {
            instance_id: "inst-1".to_string(),
            exit_reason: "submitted".to_string(),
            outcome: Some("submitted".to_string()),
            failure_category: None,
            steps: Some(1),
            cost_usd: Some(0.0),
            prompt_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(0),
            duration_secs: Some(0.0),
            error: None,
            github_pr_error: None,
            patch_present: true,
            non_empty_patch: true,
            attempts: 1,
            retry_reasons: vec![],
            runs: 0,
            resolved_count: 0,
            pass_at_1: false,
            tests_run_before_submit: false,
            last_tests_passed: None,
            fallback_count: None,
            final_model: None,
            retry_id: None,
            previous_failure_category: None,
            trace_id: None,
        }],
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: Default::default(),
        systemic_halt_category: None,
        retry_history: vec![],
        partial: 0,
        span_export_dropped: 0,
    };
    let legacy_dir = runs_dir.join("legacy");
    create_dir_all(&legacy_dir).unwrap();
    let legacy_path = legacy_dir.join("results.json");
    write(
        &legacy_path,
        serde_json::to_string_pretty(&legacy_results).unwrap(),
    )
    .unwrap();

    let manifest_template = ProvenanceManifest {
        purpose: None,
        harness: HarnessManifest {
            name: "max".to_string(),
            version: "0.1.0".to_string(),
            git_sha: None,
            git_dirty: Some(false),
            git_resolution: "exact".to_string(),
        },
        dataset: SweBenchDatasetManifest {
            path: "dataset.jsonl".to_string(),
            sha256: "matching_hash".to_string(),
            instance_count: 1,
            filter_spec: None,
            source_kind: "local".to_string(),
            alias: None,
            split: None,
            source_revision: None,
            cache_path: None,
            selected_row_count: 1,
            post_filter_row_count: 1,
        },
        prompt_template: PromptTemplateManifest {
            source: "builtin".to_string(),
            path: None,
            sha256: "deadbeef".to_string(),
        },
        config: ConfigManifest {
            resolved: String::new(),
            overlay_paths: vec![],
        },
        model: ModelManifest {
            name: "gpt-4".to_string(),
            backend: "litellm".to_string(),
            backend_version: None,
            base_url: None,
        },
        runtime: RuntimeManifest {
            started_at_utc: "2026-01-01T00:00:00Z".to_string(),
            finished_at_utc: None,
            host_os: "linux".to_string(),
            resume_mode: false,
            rust_version: None,
        },
        cli: CliManifest { argv: vec![] },
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
    };

    // 2. Write a modern results.json that has a manifest matching our expected hash
    let mut modern_matching_results = legacy_results.clone();
    modern_matching_results.manifest = Some(manifest_template.clone());
    modern_matching_results.instances = vec![InstanceResult {
        runs: 1,
        resolved_count: 1,
        ..legacy_results.instances[0].clone()
    }];
    let modern_matching_dir = runs_dir.join("modern_matching");
    create_dir_all(&modern_matching_dir).unwrap();
    let modern_matching_path = modern_matching_dir.join("results.json");
    write(
        &modern_matching_path,
        serde_json::to_string_pretty(&modern_matching_results).unwrap(),
    )
    .unwrap();

    // 3. Write a modern results.json with non-matching manifest hash
    let mut modern_mismatched_results = legacy_results.clone();
    let mut mismatched_manifest = manifest_template.clone();
    mismatched_manifest.dataset.sha256 = "other_hash".to_string();
    modern_mismatched_results.manifest = Some(mismatched_manifest);
    modern_mismatched_results.instances = vec![InstanceResult {
        runs: 1,
        resolved_count: 0,
        ..legacy_results.instances[0].clone()
    }];
    let modern_mismatched_dir = runs_dir.join("modern_mismatched");
    create_dir_all(&modern_mismatched_dir).unwrap();
    let modern_mismatched_path = modern_mismatched_dir.join("results.json");
    write(
        &modern_mismatched_path,
        serde_json::to_string_pretty(&modern_mismatched_results).unwrap(),
    )
    .unwrap();

    // 4. Write a modern results.json that matches manifest but is not completed ("running")
    let mut modern_running_results = legacy_results.clone();
    modern_running_results.sweep_status = "running".to_string();
    modern_running_results.manifest = Some(manifest_template);
    modern_running_results.instances = vec![InstanceResult {
        runs: 1,
        resolved_count: 0,
        ..legacy_results.instances[0].clone()
    }];
    let modern_running_dir = runs_dir.join("modern_running");
    create_dir_all(&modern_running_dir).unwrap();
    let modern_running_path = modern_running_dir.join("results.json");
    write(
        &modern_running_path,
        serde_json::to_string_pretty(&modern_running_results).unwrap(),
    )
    .unwrap();

    let inst = SweBenchInstance {
        instance_id: "inst-1".into(),
        repo: Some("django/django".into()),
        base_commit: Some("commit1".into()),
        problem_statement: Some("test".into()),
        image: None,
        other: serde_json::Map::new(),
    };

    // Case A: dataset hash is known (Some("matching_hash"))
    // Legacy results.json (manifest: null) should be EXCLUDED, mismatched should be EXCLUDED.
    // So only modern_matching is included: resolved_count=1, runs=1 -> rate=1.0.
    let stats_hash_known = maxwells_daemon::run::dataset_stats::compute_stats(
        &[inst.clone()],
        &[inst.clone()],
        "gpt-4",
        &runs_dir,
        &Some("matching_hash".to_string()),
    )
    .unwrap();

    let hist = stats_hash_known.historical_resolved_rate.unwrap();
    assert_eq!(hist.min, 1.0);
    assert_eq!(hist.max, 1.0);
    assert_eq!(hist.p50, 1.0);

    // Case B: dataset hash is None
    // Legacy results.json should be INCLUDED and use legacy-aware resolution.
    // Legacy has runs=0, resolved_count=0, but outcome="submitted" and no failure_category,
    // so it resolves to resolved=1, runs=1 -> rate=1.0.
    // Modern matching has resolved_count=1, runs=1 -> rate=1.0.
    // Modern mismatched has resolved_count=0, runs=1 -> rate=0.0.
    // All 3 matched runs aggregate to sum_resolved=2, sum_runs=3 -> rate=2/3.
    let stats_hash_none =
        maxwells_daemon::run::dataset_stats::compute_stats(&[inst], &[], "gpt-4", &runs_dir, &None)
            .unwrap();

    let hist_none = stats_hash_none.historical_resolved_rate.unwrap();
    assert_eq!(hist_none.min, 2.0 / 3.0);
    assert_eq!(hist_none.max, 2.0 / 3.0);
    assert_eq!(hist_none.p50, 2.0 / 3.0);
}
