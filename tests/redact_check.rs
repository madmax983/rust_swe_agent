//! Tests for `agent redact-check` (issue #321).
//!
//! Red phase: these tests reference the not-yet-implemented public API.
//! They will fail to compile until the implementation is in place.

#![allow(clippy::unwrap_used)]

use maxwells_daemon::config::Config;
use maxwells_daemon::redaction::Redactor;
use maxwells_daemon::run::redact_check::{
    RedactCheckFormat, RedactCheckOpts, RedactCheckSource, format_human, format_json,
    run_redact_check,
};

// ── Redactor::check unit tests ────────────────────────────────────────────────

#[test]
fn check_configured_literal_match_annotated_as_literal() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["my-secret-value"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("prefix my-secret-value suffix");
    let sources: Vec<&str> = result.matches.iter().map(|m| m.source.as_str()).collect();
    assert!(
        sources.contains(&"literal"),
        "expected a 'literal' source match, got: {sources:?}"
    );
}

#[test]
fn check_github_token_annotated_as_structured_github_token() {
    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let token = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    let result = redactor.check(&format!("token: {token}"));
    let sources: Vec<&str> = result.matches.iter().map(|m| m.source.as_str()).collect();
    assert!(
        sources.contains(&"structured:github_token"),
        "expected 'structured:github_token', got: {sources:?}"
    );
}

#[test]
fn check_bearer_token_annotated_as_structured_bearer() {
    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("Authorization: Bearer abcdefghijklmnopqrstuvwx");
    let sources: Vec<&str> = result.matches.iter().map(|m| m.source.as_str()).collect();
    assert!(
        sources.contains(&"structured:bearer"),
        "expected 'structured:bearer', got: {sources:?}"
    );
}

#[test]
fn check_custom_pattern_annotated_with_index() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
custom_patterns = ["MYSECRET-[0-9]+", "OTHERSECRET-[A-Z]+"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("token: MYSECRET-123 and OTHERSECRET-ABC");
    let sources: Vec<&str> = result.matches.iter().map(|m| m.source.as_str()).collect();
    assert!(
        sources.contains(&"custom_pattern[0]"),
        "expected 'custom_pattern[0]', got: {sources:?}"
    );
    assert!(
        sources.contains(&"custom_pattern[1]"),
        "expected 'custom_pattern[1]', got: {sources:?}"
    );
}

#[test]
fn check_env_assignment_annotated_as_structured_env_assignment() {
    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("API_KEY=super-secret-value-here\n");
    let sources: Vec<&str> = result.matches.iter().map(|m| m.source.as_str()).collect();
    assert!(
        sources.contains(&"structured:env_assignment"),
        "expected 'structured:env_assignment', got: {sources:?}"
    );
}

#[test]
fn check_pem_block_annotated_as_structured_pem() {
    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
    let result = redactor.check(pem);
    let sources: Vec<&str> = result.matches.iter().map(|m| m.source.as_str()).collect();
    assert!(
        sources.contains(&"structured:pem"),
        "expected 'structured:pem', got: {sources:?}"
    );
}

#[test]
fn check_api_key_annotated_as_structured_api_key() {
    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("key: sk-ant-AAAAAAAAAAAAAAAAAAAAAA");
    let sources: Vec<&str> = result.matches.iter().map(|m| m.source.as_str()).collect();
    assert!(
        sources.contains(&"structured:api_key"),
        "expected 'structured:api_key', got: {sources:?}"
    );
}

#[test]
fn check_returns_redacted_text_with_markers() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["supersecret"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("value: supersecret here");
    assert!(
        !result.redacted.contains("supersecret"),
        "raw secret leaked into redacted output: {}",
        result.redacted
    );
    assert!(
        result.redacted.contains("[REDACTED:"),
        "expected REDACTED marker in output: {}",
        result.redacted
    );
}

#[test]
fn check_match_byte_offsets_are_correct() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["SECRET"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let input = "prefix SECRET suffix";
    let result = redactor.check(input);
    assert!(!result.matches.is_empty(), "expected at least one match");
    let m = &result.matches[0];
    assert_eq!(&input[m.start..m.end], "SECRET");
}

#[test]
fn check_marker_in_checkmatch_matches_redacted_text() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["mysecret"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("value: mysecret");
    assert!(!result.matches.is_empty());
    let m = &result.matches[0];
    assert!(
        result.redacted.contains(&m.marker),
        "marker '{}' not found in redacted text '{}'",
        m.marker,
        result.redacted
    );
}

// ── Unmatched literal / pattern tracking ─────────────────────────────────────

#[test]
fn check_unmatched_literal_indices_empty_when_all_matched() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["lit1", "lit2"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("first lit1 then lit2 and done");
    assert!(
        result.unmatched_literal_indices.is_empty(),
        "expected all literals matched, but got unmatched: {:?}",
        result.unmatched_literal_indices
    );
}

#[test]
fn check_unmatched_literal_indices_reports_stale_literal() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["lit1", "stale-secret-that-wont-match"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("only lit1 is here");
    assert!(
        result.unmatched_literal_indices.contains(&1),
        "expected index 1 to be unmatched, got: {:?}",
        result.unmatched_literal_indices
    );
    assert!(
        !result.unmatched_literal_indices.contains(&0),
        "index 0 should be matched: {:?}",
        result.unmatched_literal_indices
    );
}

#[test]
fn check_unmatched_literal_indices_all_when_none_matched() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["stale1", "stale2"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("no secrets here");
    assert_eq!(
        result.unmatched_literal_indices,
        vec![0, 1],
        "expected both literals unmatched"
    );
}

#[test]
fn check_unmatched_pattern_indices_reports_zero_match_pattern() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
custom_patterns = ["MATCHED-[0-9]+", "UNMATCHED-[0-9]+"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("token: MATCHED-123");
    assert!(
        result.unmatched_pattern_indices.contains(&1),
        "expected pattern 1 unmatched, got: {:?}",
        result.unmatched_pattern_indices
    );
    assert!(
        !result.unmatched_pattern_indices.contains(&0),
        "pattern 0 should have matched: {:?}",
        result.unmatched_pattern_indices
    );
}

#[test]
fn check_no_literals_configured_returns_empty_unmatched() {
    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("some text");
    assert!(
        result.unmatched_literal_indices.is_empty(),
        "no literals configured, so nothing should be unmatched"
    );
    assert!(
        result.unmatched_pattern_indices.is_empty(),
        "no patterns configured, so nothing should be unmatched"
    );
}

// ── run_redact_check (integration-level) ──────────────────────────────────────

#[test]
fn run_redact_check_missing_file_returns_usage_error() {
    // A missing --file path must produce a usage/config error (exit 2), not an
    // internal I/O error (exit 1), so CI wrappers can distinguish typos from
    // infrastructure failures.
    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::File("/nonexistent/path/sample.txt".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let result = run_redact_check(&cfg, &opts);
    assert!(result.is_err(), "expected an error for missing file");
    let err = result.unwrap_err();
    let err_str = format!("{err}");
    assert!(
        err_str.contains("sample.txt") || err_str.contains("cannot read"),
        "expected usage error mentioning the path, got: {err_str}"
    );
    // Must be a Config/Usage error, not a bare Io error.
    assert!(
        matches!(
            err,
            maxwells_daemon::Error::Config(maxwells_daemon::error::ConfigError::Usage(_))
        ),
        "expected Config(Usage(...)) error variant, got: {err:?}"
    );
}

#[test]
fn run_redact_check_text_source_produces_matches() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["toplevel-secret"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("contains toplevel-secret value".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert!(
        !output.check.matches.is_empty(),
        "expected at least one match"
    );
    assert!(
        !output.check.redacted.contains("toplevel-secret"),
        "raw secret leaked: {}",
        output.check.redacted
    );
}

#[test]
fn run_redact_check_file_source_reads_and_redacts() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("sample.txt");
    std::fs::write(
        &file_path,
        "token: ghp_0123456789ABCDEF0123456789ABCDEF0123\n",
    )
    .unwrap();

    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::File(file_path),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert!(
        !output.check.matches.is_empty(),
        "expected github token to be matched from file"
    );
}

#[test]
fn run_redact_check_exits_nonzero_on_stale_literal() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["stale-token-value"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("no secrets here".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert!(
        !output.check.unmatched_literal_indices.is_empty(),
        "expected stale literal to be reported"
    );
    let code = output.exit_code();
    assert_ne!(
        code,
        maxwells_daemon::ExitCode::Success,
        "expected non-zero exit for stale literal"
    );
}

#[test]
fn run_redact_check_exits_zero_when_all_literals_matched() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["lit1", "lit2"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("value: lit1 and lit2 both here".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert!(
        output.check.unmatched_literal_indices.is_empty(),
        "expected no unmatched literals"
    );
    assert_eq!(
        output.exit_code(),
        maxwells_daemon::ExitCode::Success,
        "expected exit 0 when all literals matched"
    );
}

#[test]
fn run_redact_check_strict_exits_nonzero_on_unmatched_pattern() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
custom_patterns = ["TYPO-[0-9]+"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("no token here".into()),
        format: RedactCheckFormat::Human,
        strict: true,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert!(
        !output.check.unmatched_pattern_indices.is_empty(),
        "expected unmatched pattern"
    );
    let code = output.exit_code();
    assert_ne!(
        code,
        maxwells_daemon::ExitCode::Success,
        "expected non-zero exit in strict mode for unmatched pattern"
    );
}

#[test]
fn run_redact_check_strict_success_when_all_patterns_matched() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
custom_patterns = ["MATCHED-[0-9]+"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("token: MATCHED-999".into()),
        format: RedactCheckFormat::Human,
        strict: true,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert_eq!(
        output.exit_code(),
        maxwells_daemon::ExitCode::Success,
        "strict mode should exit 0 when all patterns matched"
    );
}

// ── JSON output ───────────────────────────────────────────────────────────────

#[test]
fn format_json_output_has_required_fields() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["secretval"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("data: secretval here".into()),
        format: RedactCheckFormat::Json,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let json_val = format_json(&output).unwrap();
    assert_eq!(json_val["artifact_kind"], "redact_check");
    // redact-check uses its own schema version (1.0), not the repo-wide CURRENT.
    assert_eq!(json_val["schema_version"]["major"], 1);
    assert_eq!(json_val["schema_version"]["minor"], 0);
    assert!(json_val["matches"].is_array());
    let matches = json_val["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "expected at least one match in JSON");
    let first_match = &matches[0];
    assert!(first_match["start"].is_number());
    assert!(first_match["end"].is_number());
    assert!(first_match["marker"].is_string());
    assert!(first_match["source"].is_string());
}

#[test]
fn format_json_does_not_contain_raw_secret() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["do-not-leak-this"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("value: do-not-leak-this".into()),
        format: RedactCheckFormat::Json,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let json_str = serde_json::to_string(&format_json(&output).unwrap()).unwrap();
    assert!(
        !json_str.contains("do-not-leak-this"),
        "raw secret appeared in JSON output: {json_str}"
    );
}

#[test]
fn format_json_source_labels_do_not_contain_raw_values() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["raw-literal-value"]
custom_patterns = ["RAW-PATTERN-[0-9]+"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("raw-literal-value and RAW-PATTERN-42".into()),
        format: RedactCheckFormat::Json,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let json_str = serde_json::to_string(&format_json(&output).unwrap()).unwrap();
    // Source labels must only reference index or type, not the raw values
    assert!(
        !json_str.contains("raw-literal-value"),
        "raw literal leaked in JSON: {json_str}"
    );
}

// ── Human-readable format ─────────────────────────────────────────────────────

#[test]
fn format_human_shows_source_annotation() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["my-configured-secret"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("value: my-configured-secret end".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let text = format_human(&output);
    assert!(
        text.contains("literal"),
        "expected 'literal' annotation in human output:\n{text}"
    );
}

#[test]
fn format_human_does_not_contain_raw_secret() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["secret-no-leak"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("contains: secret-no-leak".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let text = format_human(&output);
    assert!(
        !text.contains("secret-no-leak"),
        "raw secret leaked in human output:\n{text}"
    );
}

// ── Exit code contract ────────────────────────────────────────────────────────

#[test]
fn exit_code_is_zero_when_no_literals_configured() {
    let cfg = Config::from_toml_str("[redaction]\nenabled = true").unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("anything goes here".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert_eq!(
        output.exit_code(),
        maxwells_daemon::ExitCode::Success,
        "no configured literals → should exit 0"
    );
}

#[test]
fn exit_code_is_stale_literals_when_no_literal_matched() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["not-in-sample"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("no match here".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert_eq!(
        output.exit_code(),
        maxwells_daemon::ExitCode::RedactCheckStaleLiterals,
        "unmatched literals → should exit with RedactCheckStaleLiterals"
    );
}

#[test]
fn exit_code_is_strict_fail_when_pattern_unmatched_in_strict_mode() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
custom_patterns = ["NOMATCH-[0-9]+"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("nothing to match".into()),
        format: RedactCheckFormat::Human,
        strict: true,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert_eq!(
        output.exit_code(),
        maxwells_daemon::ExitCode::RedactCheckStrictFail,
        "unmatched pattern in strict mode → should exit RedactCheckStrictFail"
    );
}

#[test]
fn exit_code_strict_fail_takes_priority_over_stale_literals() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["not-in-sample"]
custom_patterns = ["ALSO-MISSING-[0-9]+"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("empty sample".into()),
        format: RedactCheckFormat::Human,
        strict: true,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    assert_eq!(
        output.exit_code(),
        maxwells_daemon::ExitCode::RedactCheckStrictFail,
        "strict fail should take priority over stale literals"
    );
}

// ── Regression tests for post-review fixes ────────────────────────────────────

#[test]
fn configured_literal_overlapping_structured_rule_not_reported_stale() {
    // Regression: a configured literal that also matches a structured rule
    // (e.g. an API key in secret_literals) was being reported as stale because
    // overlap filtering dropped the literal match before its config_index was
    // recorded.
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["sk-ant-AAAABBBBCCCCDDDDEEEE"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    let result = redactor.check("key: sk-ant-AAAABBBBCCCCDDDDEEEE end");
    assert!(
        result.unmatched_literal_indices.is_empty(),
        "configured literal overlapping with api_key structured rule should not be stale; \
         got unmatched: {:?}",
        result.unmatched_literal_indices
    );
}

#[test]
fn format_human_prints_stale_warning_even_when_no_matches() {
    // Regression: format_human was returning early when matches were empty,
    // skipping the stale-literal warning block.
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["not-in-sample"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("nothing here".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let text = format_human(&output);
    assert!(
        text.contains("warning"),
        "expected stale-literal warning even when sample has no matches:\n{text}"
    );
    assert!(
        text.contains('0'),
        "expected index 0 mentioned in warning:\n{text}"
    );
}

#[test]
fn format_human_prints_strict_warning_even_when_no_matches() {
    // Regression: format_human skipped the --strict warning when matches were empty.
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
custom_patterns = ["NOMATCH-[0-9]+"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("nothing here".into()),
        format: RedactCheckFormat::Human,
        strict: true,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let text = format_human(&output);
    assert!(
        text.contains("warning"),
        "expected strict warning even when sample has no matches:\n{text}"
    );
}

#[test]
fn unmatched_literal_index_preserved_with_preceding_empty_and_duplicate_entries() {
    // Regression: when secret_literals contains empty strings or duplicate values
    // before a stale entry, the reported unmatched index must point to the stale
    // entry's *original* config position, not the compacted rule-array position.
    //
    // Config positions:
    //   0: ""           (empty — skipped, no rule created)
    //   1: "dup"        (first occurrence)
    //   2: "dup"        (duplicate — no second rule created)
    //   3: "stale-xyz"  (stale — not present in sample)
    //
    // Without the fix, the stale literal would be reported at index 1 (its compacted
    // rule index) instead of index 3 (its original config position).
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["", "dup", "dup", "stale-xyz"]
"#,
    )
    .unwrap();
    let redactor = Redactor::from_config(&cfg.root.redaction).unwrap();
    // Input matches "dup" (indices 1 and 2) but not "stale-xyz" (index 3).
    let result = redactor.check("value: dup here");
    assert!(
        result.unmatched_literal_indices.contains(&3),
        "stale literal at config position 3 should be in unmatched_literal_indices; \
         got: {:?}",
        result.unmatched_literal_indices
    );
    assert!(
        !result.unmatched_literal_indices.contains(&0),
        "empty entry at position 0 should not appear in unmatched_literal_indices; \
         got: {:?}",
        result.unmatched_literal_indices
    );
    // "dup" matched — indices 1 and 2 should not appear as unmatched.
    assert!(
        !result.unmatched_literal_indices.contains(&1),
        "matched dup at position 1 should not be in unmatched; got: {:?}",
        result.unmatched_literal_indices
    );
    assert!(
        !result.unmatched_literal_indices.contains(&2),
        "matched dup at position 2 should not be in unmatched; got: {:?}",
        result.unmatched_literal_indices
    );
}

#[test]
fn format_human_suppresses_redacted_body_when_stale_literal_present() {
    // When there are unmatched (stale) literals, format_human must not print
    // the redacted output body — the sample could contain unredacted bytes
    // if a pattern/literal failed to match them.
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["matched-lit", "stale-lit"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("value: matched-lit here".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let text = format_human(&output);
    assert!(
        text.contains("suppressed"),
        "expected redacted output to be suppressed when stale literals present:\n{text}"
    );
    assert!(
        !text.contains("matched-lit"),
        "raw literal should not appear in output:\n{text}"
    );
}

#[test]
fn format_human_shows_redacted_body_when_no_failures() {
    // When all configured literals match, the redacted body should be shown
    // (with secrets replaced by markers).
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["all-matched"]
"#,
    )
    .unwrap();
    let opts = RedactCheckOpts {
        source: RedactCheckSource::Text("value: all-matched here".into()),
        format: RedactCheckFormat::Human,
        strict: false,
    };
    let output = run_redact_check(&cfg, &opts).unwrap();
    let text = format_human(&output);
    assert!(
        !text.contains("suppressed"),
        "redacted body should be shown when no failures:\n{text}"
    );
    assert!(
        text.contains("redacted output"),
        "expected 'redacted output' section:\n{text}"
    );
    assert!(
        !text.contains("all-matched"),
        "raw literal should be replaced by marker in output:\n{text}"
    );
}
