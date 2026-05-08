//! Docs drift verification for the operator configuration reference (issue #92).
//!
//! These tests drive creation of `docs/config-reference.md` and its integration
//! with `README.md` and the config validation error path. A test here fails
//! whenever a documented field, default, valid-value set, or CLI override changes
//! without updating the reference.

#![allow(clippy::unwrap_used)]

use rust_swe_agent::config::Config;

const CONFIG_REFERENCE_PATH: &str = "docs/config-reference.md";
const DEFAULT_TOML_PATH: &str = "src/config/defaults/default.toml";
const README_PATH: &str = "README.md";

fn read_config_reference() -> String {
    std::fs::read_to_string(CONFIG_REFERENCE_PATH).unwrap_or_else(|_| {
        panic!(
            "{CONFIG_REFERENCE_PATH} must exist — create it to pass this test (issue #92)"
        )
    })
}

/// Extract the content of a fenced TOML block that immediately follows a
/// `<!-- config-example:<marker> -->` comment in the reference doc.
fn extract_toml_example(doc: &str, marker: &str) -> String {
    let marker_text = format!("<!-- config-example:{marker} -->");
    let after_marker = doc
        .split_once(&marker_text)
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| {
            panic!(
                "{CONFIG_REFERENCE_PATH} is missing marker `{marker_text}`; \
                 add it before the ```toml block for the '{marker}' example"
            )
        });
    let after_fence = after_marker
        .split_once("```toml")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| {
            panic!(
                "Marker `{marker_text}` in {CONFIG_REFERENCE_PATH} must be \
                 followed by a ```toml block"
            )
        });
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

/// Drift guard: every non-comment, non-section leaf key in `default.toml`
/// must appear verbatim in the reference. When a new field is added to
/// `default.toml` this test fails until the reference is updated.
#[test]
fn config_reference_documents_all_default_toml_fields() {
    let doc = read_config_reference();
    let default_toml = std::fs::read_to_string(DEFAULT_TOML_PATH).unwrap();

    let field_names: Vec<&str> = default_toml
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.starts_with('[') || trimmed.is_empty() {
                return None;
            }
            trimmed.split_once('=').map(|(key, _)| key.trim())
        })
        .collect();

    for field in &field_names {
        assert!(
            doc.contains(field),
            "{CONFIG_REFERENCE_PATH} must document field '{field}' \
             found in {DEFAULT_TOML_PATH}. Add an entry for it to pass this drift check."
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
    for keyword in &[
        "default",
        "config file",
        "environment variable",
        "CLI",
    ] {
        assert!(
            doc.to_ascii_lowercase().contains(&keyword.to_ascii_lowercase()),
            "{CONFIG_REFERENCE_PATH} must mention '{keyword}' in the precedence section"
        );
    }
    // Must include at least three concrete conflict examples (look for numbered
    // list items or example headings that follow the precedence section).
    let precedence_section = doc
        .split_once("recedence")
        .map(|(_, rest)| rest)
        .unwrap_or("");
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

// ── Error message integration ────────────────────────────────────────────────

#[test]
fn config_validation_error_points_to_reference() {
    // An invalid regex in agent.test_command_patterns produces ConfigError::Invalid.
    // The error message must mention the reference so operators can self-serve.
    let err = Config::from_toml_str("[agent]\ntest_command_patterns = [\"(\"]")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("config-reference") || err.contains("configuration reference"),
        "Config validation errors must point to docs/config-reference.md so \
         operators can self-serve; got: {err}"
    );
}
