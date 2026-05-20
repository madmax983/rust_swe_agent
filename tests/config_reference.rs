//! Docs drift verification for the operator configuration reference (issue #92).
//!
//! These tests drive creation of `docs/config-reference.md` and its integration
//! with `README.md` and the config validation error path. A test here fails
//! whenever a documented field, default, valid-value set, or CLI override changes
//! without updating the reference.

#![allow(clippy::unwrap_used)]

use std::process::Command;

use maxwells_daemon::config::Config;

mod support;

const CONFIG_REFERENCE_PATH: &str = "docs/config-reference.md";
const DEFAULT_TOML_PATH: &str = "src/config/defaults/default.toml";
const README_PATH: &str = "README.md";

fn read_config_reference() -> String {
    std::fs::read_to_string(CONFIG_REFERENCE_PATH).unwrap_or_else(|_| {
        panic!("{CONFIG_REFERENCE_PATH} must exist — create it to pass this test (issue #92)")
    })
}

/// Extract the content of a fenced TOML block that immediately follows a
/// `<!-- config-example:<marker> -->` comment in the reference doc.
fn extract_toml_example(doc: &str, marker: &str) -> String {
    let marker_text = format!("<!-- config-example:{marker} -->");
    let after_marker = doc.split_once(&marker_text).map_or_else(
        || {
            panic!(
                "{CONFIG_REFERENCE_PATH} is missing marker `{marker_text}`; \
                 add it before the ```toml block for the '{marker}' example"
            )
        },
        |(_, rest)| rest,
    );
    let after_fence = after_marker.split_once("```toml").map_or_else(
        || {
            panic!(
                "Marker `{marker_text}` in {CONFIG_REFERENCE_PATH} must be \
                 followed by a ```toml block"
            )
        },
        |(_, rest)| rest,
    );
    let (block, _) = after_fence.split_once("```").unwrap_or_else(|| {
        panic!(
            "TOML block after `{marker_text}` in {CONFIG_REFERENCE_PATH} is \
             unterminated"
        )
    });
    block.trim().to_string()
}

// ── Existence and README link ────────────────────────────────────────────────

#[test]
fn config_reference_exists() {
    let doc = read_config_reference();
    assert!(!doc.is_empty(), "{CONFIG_REFERENCE_PATH} must not be empty");
}

#[test]
fn readme_links_to_config_reference() {
    let readme = std::fs::read_to_string(README_PATH).unwrap();
    assert!(
        readme.contains("docs/config-reference.md"),
        "README.md must link to docs/config-reference.md (required by issue #92)"
    );
}

// ── Section and field coverage ───────────────────────────────────────────────

#[test]
fn config_reference_documents_all_top_level_sections() {
    let doc = read_config_reference();
    for section in &[
        "[agent]",
        "[model]",
        "[environment]",
        "[sweep]",
        "[redaction]",
        "[prompts]",
        "[policy]",
    ] {
        assert!(
            doc.contains(section),
            "{CONFIG_REFERENCE_PATH} must document section {section}"
        );
    }
}

/// Drift guard: every leaf key in every top-level section of `default.toml`
/// must appear as a code-formatted field name (`` `field` ``) in the reference.
/// Uses the `toml` crate to parse the file so template content in multiline
/// strings can never be misread as field names. When a new field is added to
/// `default.toml` this test fails until the reference is updated.
#[test]
fn config_reference_documents_all_default_toml_fields() {
    let doc = read_config_reference();
    let default_toml = std::fs::read_to_string(DEFAULT_TOML_PATH).unwrap();
    let parsed: toml::Value = toml::from_str(&default_toml).unwrap();

    let field_names: Vec<String> = if let toml::Value::Table(root) = &parsed {
        root.iter()
            .flat_map(|(_, section_val)| {
                if let toml::Value::Table(section) = section_val {
                    section.keys().cloned().collect::<Vec<_>>()
                } else {
                    vec![]
                }
            })
            .collect()
    } else {
        vec![]
    };

    for field in &field_names {
        let needle = format!("`{field}`");
        assert!(
            doc.contains(&needle),
            "{CONFIG_REFERENCE_PATH} must document field '{field}' as a code \
             literal (`{field}`) found in {DEFAULT_TOML_PATH}. Add an entry \
             for it to pass this drift check."
        );
    }
}

// ── TOML example round-trip checks ──────────────────────────────────────────

#[test]
fn no_key_smoke_example_parses() {
    let doc = read_config_reference();
    let toml = extract_toml_example(&doc, "no-key-smoke");
    Config::from_toml_str(&toml).unwrap_or_else(|e| {
        panic!(
            "no-key-smoke TOML example in {CONFIG_REFERENCE_PATH} must parse \
             without error; got: {e}"
        )
    });
}

#[test]
fn live_model_example_parses() {
    let doc = read_config_reference();
    let toml = extract_toml_example(&doc, "live-model");
    Config::from_toml_str(&toml).unwrap_or_else(|e| {
        panic!(
            "live-model TOML example in {CONFIG_REFERENCE_PATH} must parse \
             without error; got: {e}"
        )
    });
}

#[test]
fn docker_sweep_example_parses() {
    let doc = read_config_reference();
    let toml = extract_toml_example(&doc, "docker-sweep");
    Config::from_toml_str(&toml).unwrap_or_else(|e| {
        panic!(
            "docker-sweep TOML example in {CONFIG_REFERENCE_PATH} must parse \
             without error; got: {e}"
        )
    });
}

#[test]
fn interactive_local_example_parses() {
    let doc = read_config_reference();
    let toml = extract_toml_example(&doc, "interactive-local");
    Config::from_toml_str(&toml).unwrap_or_else(|e| {
        panic!(
            "interactive-local TOML example in {CONFIG_REFERENCE_PATH} must \
             parse without error; got: {e}"
        )
    });
}

// ── Content requirements ─────────────────────────────────────────────────────

#[test]
fn config_reference_explains_precedence() {
    let doc = read_config_reference();
    // Must name all four layers.
    for keyword in &["default", "config file", "environment variable", "CLI"] {
        assert!(
            doc.to_ascii_lowercase()
                .contains(&keyword.to_ascii_lowercase()),
            "{CONFIG_REFERENCE_PATH} must mention '{keyword}' in the precedence section"
        );
    }
    // Must include at least three concrete conflict examples (look for numbered
    // list items or example headings that follow the precedence section).
    let precedence_section = doc.split_once("recedence").map_or("", |(_, rest)| rest);
    let example_count = precedence_section
        .lines()
        .filter(|l| {
            let l = l.trim().to_ascii_lowercase();
            l.starts_with("**example") || l.starts_with("example ") || l.starts_with("- example")
        })
        .count();
    assert!(
        example_count >= 3,
        "{CONFIG_REFERENCE_PATH} must include at least three concrete precedence \
         examples labelled 'Example N' or '**Example N**'; found {example_count}"
    );
}

#[test]
fn config_reference_covers_secret_handling() {
    let doc = read_config_reference();
    // Must mention where credentials come from.
    assert!(
        doc.contains("ANTHROPIC_API_KEY") || doc.contains("OPENAI_API_KEY"),
        "{CONFIG_REFERENCE_PATH} must name at least one provider key env var \
         (ANTHROPIC_API_KEY, OPENAI_API_KEY)"
    );
    // Must prohibit raw secrets in committed configs.
    let lower = doc.to_ascii_lowercase();
    assert!(
        lower.contains("never") || lower.contains("must not") || lower.contains("do not"),
        "{CONFIG_REFERENCE_PATH} must explicitly prohibit committing raw secrets"
    );
    // Must cross-link the redaction spec.
    assert!(
        doc.contains("spec-secret-redaction"),
        "{CONFIG_REFERENCE_PATH} must link to docs/spec-secret-redaction.md"
    );
}

#[test]
fn config_reference_states_toml_format_and_migration_note() {
    let doc = read_config_reference();
    assert!(
        doc.contains("TOML"),
        "{CONFIG_REFERENCE_PATH} must state that the config format is TOML"
    );
    // Migration note for stale YAML-era docs.
    let lower = doc.to_ascii_lowercase();
    assert!(
        lower.contains("yaml") || lower.contains("migration"),
        "{CONFIG_REFERENCE_PATH} must include a migration note for stale YAML-era \
         examples (mention 'YAML' or 'migration')"
    );
}

// ── Error message integration (AC 9) ─────────────────────────────────────────

#[test]
fn config_validation_error_points_to_reference() {
    // ConfigError::Invalid (regex validation failure) must mention the reference.
    let invalid_err = Config::from_toml_str("[agent]\ntest_command_patterns = [\"(\"]")
        .unwrap_err()
        .to_string();
    assert!(
        invalid_err.contains("config-reference") || invalid_err.contains("configuration reference"),
        "ConfigError::Invalid must point to docs/config-reference.md; got: {invalid_err}"
    );

    // ConfigError::Toml (TOML parse failure) must also mention the reference so
    // operators aren't left with only a parser diagnostic.
    let toml_err = Config::from_toml_str("[[[ not valid toml")
        .unwrap_err()
        .to_string();
    assert!(
        toml_err.contains("config-reference") || toml_err.contains("configuration reference"),
        "ConfigError::Toml must point to docs/config-reference.md; got: {toml_err}"
    );
}

// ── Default value drift (AC 7 extension) ─────────────────────────────────────

/// Verifies that actual runtime default *values* (not just field names) are
/// documented. Catches changes like `step_limit` moving from 50 → 100 when the
/// field name stays the same — the field-name drift guard would miss that.
#[test]
fn config_reference_documents_actual_default_values() {
    let doc = read_config_reference();
    let cfg = Config::defaults().unwrap();

    let checks: &[(&str, String)] = &[
        ("agent.step_limit", cfg.root.agent.step_limit.to_string()),
        (
            "agent.observation_max_bytes",
            cfg.root.agent.observation_max_bytes.to_string(),
        ),
        (
            "agent.observation_head_ratio",
            cfg.root.agent.observation_head_ratio.to_string(),
        ),
        (
            "agent.tool_hook_timeout_secs",
            cfg.root.agent.tool_hook_timeout_secs.to_string(),
        ),
        ("model.name", format!("\"{}\"", cfg.root.model.name)),
        ("model.max_tokens", cfg.root.model.max_tokens.to_string()),
        (
            "environment.timeout_secs",
            cfg.root.environment.timeout_secs.to_string(),
        ),
        (
            "environment.workdir",
            format!("\"{}\"", cfg.root.environment.workdir),
        ),
    ];

    for (field, value) in checks {
        assert!(
            doc.contains(value.as_str()),
            "{CONFIG_REFERENCE_PATH} must document the actual default value \
             '{value}' for field '{field}'. Update the reference when defaults change."
        );
    }
}

// ── No-key example binary execution (AC 8) ───────────────────────────────────

/// Extracts the no-key-smoke TOML from the reference, writes it to a tempfile,
/// runs `hello-world --config <file>`, and verifies the output trajectory is
/// valid. This is the "produces a valid trajectory" check required by AC 8.
#[test]
fn no_key_smoke_example_produces_valid_trajectory() {
    let doc = read_config_reference();
    let toml = extract_toml_example(&doc, "no-key-smoke");

    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("smoke.toml");
    let output_dir = temp.path().join("runs");
    std::fs::write(&config_path, &toml).unwrap();

    let out = Command::new(support::binary_path())
        .args([
            "--log",
            "error",
            "hello-world",
            "--config",
            &config_path.display().to_string(),
            "--output",
            &output_dir.display().to_string(),
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "no-key smoke run with reference config failed\n\
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let trajectory_path = output_dir.join("hello-world.traj.json");
    assert!(
        trajectory_path.exists(),
        "trajectory not written at {}",
        trajectory_path.display()
    );

    let trajectory: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&trajectory_path).unwrap()).unwrap();
    assert_eq!(
        trajectory["trajectory_format"].as_str(),
        Some("mini-swe-agent-1.3"),
        "trajectory must be mini-swe-agent-1.3 format"
    );
    assert_eq!(
        trajectory["info"]["outcome"].as_str(),
        Some("submitted"),
        "no-key smoke must produce outcome=submitted"
    );
    assert_eq!(
        trajectory["info"]["total_cost_usd"].as_f64(),
        Some(0.0),
        "no-key smoke must cost $0"
    );
}
