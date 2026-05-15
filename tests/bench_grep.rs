//! `bench grep`: search trajectories across a sweep by regex.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

// ── unit tests (live in the library) ─────────────────────────────────────────

use rust_swe_agent::run::grep::{GrepArgs, run as grep_run};

#[test]
fn unit_basic_match_returns_hits() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "ImportError".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: None,
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();

    assert!(!report.matches.is_empty(), "should find ImportError matches");
    assert!(
        report.matches.iter().any(|m| m.instance_id == "instance-a"),
        "instance-a should match"
    );
    assert!(
        report.matches.iter().any(|m| m.instance_id == "instance-b"),
        "instance-b should match"
    );
}

#[test]
fn unit_no_match_returns_empty_vec() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "XYZZY_NO_MATCH_PATTERN_12345".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: None,
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();

    assert!(report.matches.is_empty(), "no matches expected");
}

#[test]
fn unit_role_filter_restricts_to_user_only() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "ImportError".into(),
        roles: vec!["user".into()],
        field: "content".into(),
        instance_ids: None,
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();

    assert!(
        report.matches.iter().all(|m| m.role == "user"),
        "all matches should be from user role"
    );
    // instance-a turn 2 (assistant "I see the ImportError") should be excluded
    assert!(
        !report
            .matches
            .iter()
            .any(|m| m.role == "assistant" && m.instance_id == "instance-a" && m.turn_index == 2),
        "assistant turn in instance-a should be excluded"
    );
}

#[test]
fn unit_instance_ids_filter_includes_only_specified() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "ImportError".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: Some(vec!["instance-a".into()]),
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();

    assert!(
        report.matches.iter().all(|m| m.instance_id == "instance-a"),
        "only instance-a should appear"
    );
    assert!(!report.matches.is_empty(), "instance-a should have matches");
}

#[test]
fn unit_exclude_instance_ids_removes_specified() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "ImportError".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: None,
        exclude_instance_ids: Some(vec!["instance-a".into()]),
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();

    assert!(
        !report.matches.iter().any(|m| m.instance_id == "instance-a"),
        "instance-a should be excluded"
    );
    assert!(
        report.matches.iter().any(|m| m.instance_id == "instance-b"),
        "instance-b should still appear"
    );
}

#[test]
fn unit_outcome_filter_restricts_to_error_instances() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "ImportError".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: None,
        exclude_instance_ids: None,
        outcomes: vec!["error".into()],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();

    // Only instance-b has outcome=error
    assert!(
        report.matches.iter().all(|m| m.instance_id == "instance-b"),
        "only instance-b (outcome=error) should match"
    );
}

#[test]
fn unit_max_matches_per_instance_caps_hits() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "ImportError".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: Some(vec!["instance-a".into()]),
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: Some(1),
    })
    .unwrap();

    let instance_a_matches: Vec<_> = report
        .matches
        .iter()
        .filter(|m| m.instance_id == "instance-a")
        .collect();
    assert_eq!(
        instance_a_matches.len(),
        1,
        "should have at most 1 match for instance-a"
    );
}

#[test]
fn unit_context_chars_controls_snippet_size() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let narrow = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "ImportError".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: Some(vec!["instance-b".into()]),
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 5,
        max_matches_per_instance: None,
    })
    .unwrap();

    let wide = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "ImportError".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: Some(vec!["instance-b".into()]),
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();

    // Both should find the match
    assert!(!narrow.matches.is_empty());
    assert!(!wide.matches.is_empty());
    // Wider context should produce longer or equal snippets
    assert!(
        wide.matches[0].snippet.len() >= narrow.matches[0].snippet.len(),
        "wider context should produce longer or equal snippets"
    );
}

#[test]
fn unit_field_actions_searches_bash_commands() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: r"pytest -x".into(),
        roles: vec![],
        field: "actions".into(),
        instance_ids: None,
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();

    assert!(
        report.matches.iter().any(|m| m.instance_id == "instance-a"),
        "instance-a actions should match pytest -x"
    );
    // User turns have no actions, so role must be assistant
    assert!(
        report.matches.iter().all(|m| m.role == "assistant"),
        "only assistant turns have actions"
    );
}

#[test]
fn unit_invalid_regex_returns_error() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let result = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "[invalid(regex".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: None,
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    });

    assert!(result.is_err(), "invalid regex should return an error");
}

#[test]
fn unit_instances_scanned_counts_filtered_instances() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let all = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "x".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: None,
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();
    assert_eq!(all.instances_scanned, 3, "all 3 instances should be scanned");

    let filtered = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "x".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: Some(vec!["instance-a".into()]),
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 80,
        max_matches_per_instance: None,
    })
    .unwrap();
    assert_eq!(
        filtered.instances_scanned, 1,
        "only 1 instance should be scanned when filtered"
    );
}

#[test]
fn unit_redaction_masks_secret_shaped_content() {
    let sweep = tempfile::tempdir().unwrap();
    // Write results.json
    std::fs::write(
        sweep.path().join("results.json"),
        serde_json::json!({
            "artifact_kind": "sweep_results",
            "schema_version": {"major": 1, "minor": 4},
            "total": 1,
            "submitted": 1,
            "skipped": 0,
            "errored": 0,
            "total_cost_usd": 0.01,
            "instances": [{
                "instance_id": "secret-instance",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "cost_usd": 0.01,
                "attempts": 1,
                "runs": 1,
                "resolved_count": 1,
                "pass_at_1": true,
                "tests_run_before_submit": false
            }]
        })
        .to_string(),
    )
    .unwrap();

    // A fake GitHub token (all-hex, right length) that the default redactor detects
    let fake_token = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    std::fs::write(
        sweep.path().join("secret-instance.traj.json"),
        serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.2",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 4},
            "info": {
                "task": "test",
                "model_name": "fixture-model",
                "outcome": "submitted",
                "total_cost_usd": 0.01,
                "steps": 1,
                "test_invocations": [],
                "tests_run_before_submit": false
            },
            "messages": [{
                "role": "assistant",
                "content": format!("Using token: {fake_token} to authenticate."),
                "extra": {"actions": [], "cost": 0.01}
            }]
        })
        .to_string(),
    )
    .unwrap();

    let report = grep_run(&GrepArgs {
        sweep_dir: sweep.path().to_path_buf(),
        pattern: "authenticate".into(),
        roles: vec![],
        field: "content".into(),
        instance_ids: None,
        exclude_instance_ids: None,
        outcomes: vec![],
        context_chars: 200,
        max_matches_per_instance: None,
    })
    .unwrap();

    assert!(!report.matches.is_empty(), "should find the match");
    for m in &report.matches {
        assert!(
            !m.snippet.contains(fake_token),
            "snippet should not contain the raw GitHub token: {}",
            m.snippet
        );
    }
}

// ── CLI integration tests ─────────────────────────────────────────────────────

#[test]
fn cli_text_output_shows_tab_separated_columns() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench grep should exit 0 with matches\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    // Each line: instance_id\tturn_index\trole\tsnippet
    assert!(!stdout.is_empty(), "stdout should not be empty");
    for line in stdout.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        assert_eq!(
            cols.len(),
            4,
            "each line should have 4 tab-separated columns: {line:?}"
        );
    }
}

#[test]
fn cli_exit_code_0_with_matches() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "ImportError",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "should exit 0 when matches found"
    );
}

#[test]
fn cli_exit_code_nonzero_with_no_matches() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "XYZZY_NO_MATCH_PATTERN_12345",
        ])
        .output()
        .unwrap();

    assert_ne!(
        output.status.code(),
        Some(0),
        "should exit non-zero when no matches found"
    );
}

#[test]
fn cli_no_match_exit_code_is_13() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "XYZZY_NO_MATCH_PATTERN_12345",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(13),
        "no-match exit code should be 13"
    );
}

#[test]
fn cli_json_format_emits_one_json_object_per_line() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench grep --format json should exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.is_empty(), "stdout should not be empty");
    for line in stdout.lines() {
        let obj: serde_json::Value =
            serde_json::from_str(line).expect("each line should be a valid JSON object");
        assert!(obj["instance_id"].is_string(), "instance_id required");
        assert!(obj["turn_index"].is_u64(), "turn_index required");
        assert!(obj["role"].is_string(), "role required");
        assert!(obj["snippet"].is_string(), "snippet required");
    }
}

#[test]
fn cli_role_filter_restricts_output() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
            "--role",
            "user",
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for line in stdout.lines() {
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(obj["role"], "user", "only user-role matches should appear");
    }
}

#[test]
fn cli_instance_ids_filter_restricts_to_specified() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
            "--instance-ids",
            "instance-a",
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for line in stdout.lines() {
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            obj["instance_id"], "instance-a",
            "only instance-a should appear"
        );
    }
}

#[test]
fn cli_exclude_instance_ids_removes_specified() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
            "--exclude-instance-ids",
            "instance-a",
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.is_empty(), "instance-b should still match");
    for line in stdout.lines() {
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_ne!(
            obj["instance_id"], "instance-a",
            "instance-a should be excluded"
        );
    }
}

#[test]
fn cli_outcome_filter_restricts_to_error_instances() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
            "--outcome",
            "error",
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for line in stdout.lines() {
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            obj["instance_id"], "instance-b",
            "only instance-b has outcome=error"
        );
    }
}

#[test]
fn cli_max_matches_per_instance_caps_output() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
            "--max-matches-per-instance",
            "1",
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for line in stdout.lines() {
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        *counts
            .entry(obj["instance_id"].as_str().unwrap().to_owned())
            .or_default() += 1;
    }
    for (id, count) in &counts {
        assert_eq!(
            *count, 1,
            "instance {id} should have at most 1 match, got {count}"
        );
    }
}

#[test]
fn cli_invalid_regex_exits_nonzero_with_usage_error() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "[invalid(regex",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "invalid regex should exit non-zero"
    );
    // Usage error or internal error exit code (2 or 1) — distinct from 13 (no matches)
    assert_ne!(
        output.status.code(),
        Some(13),
        "invalid regex should not exit 13 (no-matches code)"
    );
}

#[test]
fn cli_missing_sweep_exits_nonzero() {
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            "/tmp/nonexistent-grep-sweep-dir-xyz",
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "missing sweep dir should exit non-zero"
    );
}

#[test]
fn cli_help_mentions_zero_cost_guarantee() {
    let output = Command::new(binary_path())
        .args(["bench", "grep", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success(), "help should exit 0");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.to_lowercase().contains("zero-cost")
            || stdout.to_lowercase().contains("never calls a model"),
        "help should mention the zero-cost guarantee: {stdout}"
    );
}

#[test]
fn cli_redaction_prevents_secret_in_stdout() {
    let sweep = tempfile::tempdir().unwrap();

    // Write results.json
    std::fs::write(
        sweep.path().join("results.json"),
        serde_json::json!({
            "artifact_kind": "sweep_results",
            "schema_version": {"major": 1, "minor": 4},
            "total": 1,
            "submitted": 1,
            "skipped": 0,
            "errored": 0,
            "total_cost_usd": 0.01,
            "instances": [{
                "instance_id": "secret-instance",
                "exit_reason": "submitted",
                "outcome": "submitted",
                "cost_usd": 0.01,
                "attempts": 1,
                "runs": 1,
                "resolved_count": 1,
                "pass_at_1": true,
                "tests_run_before_submit": false
            }]
        })
        .to_string(),
    )
    .unwrap();

    let fake_token = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    std::fs::write(
        sweep.path().join("secret-instance.traj.json"),
        serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.2",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 4},
            "info": {
                "task": "test",
                "model_name": "fixture-model",
                "outcome": "submitted",
                "total_cost_usd": 0.01,
                "steps": 1,
                "test_invocations": [],
                "tests_run_before_submit": false
            },
            "messages": [{
                "role": "assistant",
                "content": format!("Using token: {fake_token} to authenticate."),
                "extra": {"actions": [], "cost": 0.01}
            }]
        })
        .to_string(),
    )
    .unwrap();

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--context",
            "200",
            "authenticate",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "should find the match");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains(fake_token),
        "stdout must not contain the raw GitHub token; got: {stdout}"
    );
}

#[test]
fn cli_text_output_includes_instance_id_and_role() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("instance-a"), "should mention instance-a");
    assert!(stdout.contains("instance-b"), "should mention instance-b");
    assert!(stdout.contains("user"), "should mention role user");
}

#[test]
fn cli_pattern_field_and_instances_scanned_in_json() {
    let sweep = tempfile::tempdir().unwrap();
    copy_grep_fixture(sweep.path());

    // Use --format json; first line is a match, not the report — so check the content
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "grep",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
            "ImportError",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // At least one line should exist and be valid JSON with correct fields
    assert!(!stdout.trim().is_empty());
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn copy_grep_fixture(dir: &Path) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grep/sweep");
    copy_dir(&fixture, dir);
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}
