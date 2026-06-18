//! Tests for `agent artifact-check` (issue #534).
//! RED phase: written before implementation — will fail to compile until
//! `src/run/artifact_check.rs` is created and wired up.
#![allow(clippy::unwrap_used)]

use maxwells_daemon::run::artifact_check::{
    ArtifactCheckOpts, ArtifactCheckSource, ConformanceVerdict, format_json, format_text,
    run_artifact_check,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn check_bytes(json: &str) -> maxwells_daemon::run::artifact_check::ArtifactCheckOutput {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), json).unwrap();
    run_artifact_check(&ArtifactCheckOpts {
        source: ArtifactCheckSource::Paths(vec![tmp.path().to_owned()]),
        strict: false,
    })
    .unwrap()
}

fn check_bytes_strict(json: &str) -> maxwells_daemon::run::artifact_check::ArtifactCheckOutput {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), json).unwrap();
    run_artifact_check(&ArtifactCheckOpts {
        source: ArtifactCheckSource::Paths(vec![tmp.path().to_owned()]),
        strict: true,
    })
    .unwrap()
}

// ── AC 2: verdict types reported correctly ────────────────────────────────────

#[test]
fn valid_trajectory_returns_valid_verdict() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": { "task": "fix the bug" },
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results.len(), 1);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Valid);
    assert_eq!(
        output.results[0].artifact_kind.as_deref(),
        Some("trajectory")
    );
    assert_eq!(
        output.results[0].schema_version.as_deref(),
        Some("1.11")
    );
}

#[test]
fn invalid_trajectory_missing_required_fields_returns_invalid() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 }
        // missing: trajectory_format, info, messages
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results.len(), 1);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    assert!(
        !output.results[0].missing_fields.is_empty(),
        "missing_fields must be reported"
    );
    assert!(
        output.results[0]
            .missing_fields
            .contains(&"trajectory_format".to_owned()),
        "trajectory_format must be listed as missing"
    );
}

// ── AC 3: trajectory-specific required fields ─────────────────────────────────

#[test]
fn trajectory_missing_info_reports_it_as_missing() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "messages": []
        // missing: info
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    assert!(output.results[0].missing_fields.contains(&"info".to_owned()));
}

#[test]
fn trajectory_missing_messages_reports_it_as_missing() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {}
        // missing: messages
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    assert!(
        output.results[0]
            .missing_fields
            .contains(&"messages".to_owned())
    );
}

#[test]
fn trajectory_unknown_additive_field_does_not_fail_major_one() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": [],
        "unknown_future_field": "some_value"
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(
        output.results[0].verdict,
        ConformanceVerdict::Valid,
        "unknown additive fields in major 1 must not fail"
    );
}

// ── AC 4: unsupported_major ───────────────────────────────────────────────────

#[test]
fn future_major_version_returns_unsupported_major() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 99, "minor": 0 },
        "trajectory_format": "future-format",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::UnsupportedMajor);
}

#[test]
fn unsupported_major_is_a_failure() {
    let json = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": { "major": 2, "minor": 0 }
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::UnsupportedMajor);
    assert!(output.has_failures(), "unsupported_major must count as a failure");
}

// ── AC 5: legacy_unversioned ──────────────────────────────────────────────────

#[test]
fn missing_both_header_fields_returns_legacy_unversioned() {
    let json = serde_json::json!({
        "trajectory_format": "mini-swe-agent-0.9",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(
        output.results[0].verdict,
        ConformanceVerdict::LegacyUnversioned
    );
}

#[test]
fn legacy_unversioned_is_not_a_failure_by_default() {
    let json = serde_json::json!({ "some_data": 1 }).to_string();
    let output = check_bytes(&json);
    assert_eq!(
        output.results[0].verdict,
        ConformanceVerdict::LegacyUnversioned
    );
    assert!(
        !output.has_failures(),
        "legacy_unversioned must be a warning only by default"
    );
}

#[test]
fn legacy_unversioned_is_failure_under_strict() {
    let json = serde_json::json!({ "some_data": 1 }).to_string();
    let output = check_bytes_strict(&json);
    assert_eq!(
        output.results[0].verdict,
        ConformanceVerdict::LegacyUnversioned
    );
    assert!(
        output.has_failures(),
        "legacy_unversioned must be a failure under --strict"
    );
}

// ── AC 6: all 9 contract-table artifact kinds are validated ──────────────────

#[test]
fn sweep_results_checks_required_fields() {
    let json = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": { "major": 1, "minor": 0 }
        // missing: total, submitted, skipped, errored, failures_by_category, instances
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    for f in &["total", "submitted", "skipped", "errored", "failures_by_category", "instances"] {
        assert!(
            output.results[0].missing_fields.contains(&(*f).to_owned()),
            "sweep_results missing field: {f}"
        );
    }
}

#[test]
fn valid_sweep_results_returns_valid() {
    let json = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": { "major": 1, "minor": 11 },
        "total": 10,
        "submitted": 8,
        "skipped": 1,
        "errored": 1,
        "failures_by_category": {},
        "instances": []
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Valid);
}

#[test]
fn evaluation_results_checks_required_fields() {
    let json = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": { "major": 1, "minor": 0 }
        // missing: instances
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    assert!(
        output.results[0].missing_fields.contains(&"instances".to_owned())
    );
}

#[test]
fn forecast_report_checks_required_fields() {
    let json = serde_json::json!({
        "artifact_kind": "forecast_report",
        "schema_version": { "major": 1, "minor": 0 }
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    for f in &["calibration", "per_instance", "forecast", "resolution_rate", "threshold"] {
        assert!(
            output.results[0].missing_fields.contains(&(*f).to_owned()),
            "forecast_report missing field: {f}"
        );
    }
}

#[test]
fn calibration_report_checks_required_fields() {
    let json = serde_json::json!({
        "artifact_kind": "calibration_report",
        "schema_version": { "major": 1, "minor": 0 }
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    for f in &["forecast_path", "results_path", "verdict", "comparability", "metrics", "per_instance"] {
        assert!(
            output.results[0].missing_fields.contains(&(*f).to_owned()),
            "calibration_report missing field: {f}"
        );
    }
}

#[test]
fn preflight_report_checks_required_fields() {
    let json = serde_json::json!({
        "artifact_kind": "preflight_report",
        "schema_version": { "major": 1, "minor": 0 }
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    for f in &["mode", "checks"] {
        assert!(
            output.results[0].missing_fields.contains(&(*f).to_owned()),
            "preflight_report missing field: {f}"
        );
    }
}

#[test]
fn swebench_predictions_metadata_checks_required_fields() {
    let json = serde_json::json!({
        "artifact_kind": "swebench_predictions_metadata",
        "schema_version": { "major": 1, "minor": 0 }
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    for f in &["predictions_file", "aggregate", "row_count", "swebench_evaluator_compatible"] {
        assert!(
            output.results[0].missing_fields.contains(&(*f).to_owned()),
            "swebench_predictions_metadata missing field: {f}"
        );
    }
}

#[test]
fn bundle_manifest_checks_required_fields() {
    let json = serde_json::json!({
        "artifact_kind": "bundle_manifest",
        "schema_version": { "major": 1, "minor": 0 }
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    for f in &["source_sweep_dir", "source_manifest_hash", "harness_git_sha", "bundle_generated_at", "instance_scope", "files"] {
        assert!(
            output.results[0].missing_fields.contains(&(*f).to_owned()),
            "bundle_manifest missing field: {f}"
        );
    }
}

#[test]
fn cache_stats_report_checks_required_fields() {
    let json = serde_json::json!({
        "artifact_kind": "cache_stats_report",
        "schema_version": { "major": 1, "minor": 0 }
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(output.results[0].verdict, ConformanceVerdict::Invalid);
    for f in &["sweep", "generated_at", "cache_disabled", "sweep_totals", "instances"] {
        assert!(
            output.results[0].missing_fields.contains(&(*f).to_owned()),
            "cache_stats_report missing field: {f}"
        );
    }
}

// ── AC 7: --format json output ────────────────────────────────────────────────

#[test]
fn json_format_emits_validation_report_artifact_kind() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    let val = format_json(&output).unwrap();
    assert_eq!(
        val.get("artifact_kind").and_then(|v| v.as_str()),
        Some("validation_report"),
        "JSON must have artifact_kind = 'validation_report'"
    );
    assert!(
        val.get("schema_version").is_some(),
        "JSON must have schema_version"
    );
    assert!(
        val.get("results").is_some(),
        "JSON must have results array"
    );
}

#[test]
fn json_format_includes_verdict_counts() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    let val = format_json(&output).unwrap();
    assert!(
        val.get("summary").is_some(),
        "JSON must have summary with verdict counts"
    );
}

// ── AC 7: text format includes summary table ──────────────────────────────────

#[test]
fn text_format_includes_verdict_and_path() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    let text = format_text(&output);
    assert!(text.contains("valid"), "text must include 'valid' verdict");
}

#[test]
fn text_format_shows_missing_fields_for_invalid() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 }
        // missing required fields
    })
    .to_string();
    let output = check_bytes(&json);
    let text = format_text(&output);
    assert!(
        text.contains("trajectory_format"),
        "text must show missing field trajectory_format"
    );
}

// ── AC 8: exit codes / has_failures ──────────────────────────────────────────

#[test]
fn has_failures_false_when_all_valid() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    assert!(!output.has_failures());
}

#[test]
fn has_failures_true_when_any_invalid() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 }
    })
    .to_string();
    let output = check_bytes(&json);
    assert!(output.has_failures());
}

#[test]
fn valid_with_warnings_not_a_failure_by_default() {
    // An older minor version should produce valid_with_warnings (not invalid)
    // and should not count as a failure by default.
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 0 },
        "trajectory_format": "mini-swe-agent-1.0",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    // Older minor is valid_with_warnings (warning, not error)
    assert_ne!(output.results[0].verdict, ConformanceVerdict::Invalid);
    assert!(!output.has_failures(), "valid_with_warnings must not fail by default");
}

#[test]
fn valid_with_warnings_is_failure_under_strict() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 0 },
        "trajectory_format": "mini-swe-agent-1.0",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes_strict(&json);
    assert!(output.has_failures(), "valid_with_warnings must fail under --strict");
}

// ── AC 8: ArtifactCheckFailure exit code exists ───────────────────────────────

#[test]
fn artifact_check_failure_exit_code_is_47() {
    assert_eq!(
        maxwells_daemon::exit_code::ExitCode::ArtifactCheckFailure.as_i32(),
        47
    );
    assert_eq!(
        maxwells_daemon::exit_code::ExitCode::ArtifactCheckFailure.outcome_class(),
        "artifact_check_failure"
    );
}

// ── Zero model calls / no API key needed ──────────────────────────────────────

#[test]
fn no_api_key_required() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": []
    })
    .to_string();
    let result = check_bytes(&json);
    assert_eq!(result.results.len(), 1, "must work without any API key");
}

// ── Directory scanning ────────────────────────────────────────────────────────

#[test]
fn directory_scan_finds_json_files_recursively() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let sub_dir = tmp_dir.path().join("sub");
    std::fs::create_dir(&sub_dir).unwrap();

    let valid_json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": []
    })
    .to_string();
    std::fs::write(tmp_dir.path().join("a.json"), &valid_json).unwrap();
    std::fs::write(sub_dir.join("b.json"), &valid_json).unwrap();

    let output = run_artifact_check(&ArtifactCheckOpts {
        source: ArtifactCheckSource::Paths(vec![tmp_dir.path().to_owned()]),
        strict: false,
    })
    .unwrap();
    assert_eq!(
        output.results.len(),
        2,
        "recursive directory scan must find all JSON files"
    );
}

// ── Multiple paths ─────────────────────────────────────────────────────────────

#[test]
fn multiple_paths_all_validated() {
    let json1 = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 11 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": []
    })
    .to_string();
    let json2 = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": { "major": 1, "minor": 11 },
        "total": 1, "submitted": 1, "skipped": 0, "errored": 0,
        "failures_by_category": {}, "instances": []
    })
    .to_string();
    let tmp1 = tempfile::NamedTempFile::new().unwrap();
    let tmp2 = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp1.path(), json1).unwrap();
    std::fs::write(tmp2.path(), json2).unwrap();

    let output = run_artifact_check(&ArtifactCheckOpts {
        source: ArtifactCheckSource::Paths(vec![
            tmp1.path().to_owned(),
            tmp2.path().to_owned(),
        ]),
        strict: false,
    })
    .unwrap();
    assert_eq!(output.results.len(), 2);
    assert!(output.results.iter().all(|r| r.verdict == ConformanceVerdict::Valid));
}

// ── AC 2: schema_version reported as "major.minor" string ────────────────────

#[test]
fn schema_version_formatted_as_major_dot_minor() {
    let json = serde_json::json!({
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 5 },
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": []
    })
    .to_string();
    let output = check_bytes(&json);
    assert_eq!(
        output.results[0].schema_version.as_deref(),
        Some("1.5"),
        "schema_version must be reported as 'major.minor'"
    );
}
