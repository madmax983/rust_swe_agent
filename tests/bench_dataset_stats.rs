//! Integration tests for `bench dataset-stats` subcommand (issue #281).
//!
//! RED-phase: these tests verify the required behavior of `bench dataset-stats`.
//! They will fail to compile or run until the GREEN phase implements the subcommand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

// We will simulate running the subcommand via argument parsing or calling a public entry point if we expose it,
// or by mocking the core calculator in `maxwells_daemon::run::dataset_stats`.
// Let's test the statistical logic directly to ensure maximum TDD precision, and then verify the CLI integration.

use maxwells_daemon::run::swebench::SweBenchInstance;

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
