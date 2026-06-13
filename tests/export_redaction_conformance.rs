//! Shared conformance tests for all registered trajectory exporters.
//!
//! These tests enforce the two invariants in docs/spec-export.md:
//!   1. Every exporter in the registry redacts secrets (no raw credential may appear in output).
//!   2. The spec document mentions every format name and stability tier declared in the registry.
//!
//! Adding an exporter that skips redaction, or forgetting to document it in spec-export.md,
//! fails CI here.

use maxwells_daemon::model::Message;
use maxwells_daemon::trajectory::Trajectory;
use maxwells_daemon::trajectory::export::registry;

/// A real-looking GitHub personal access token used as a canary secret in the fixture.
const CANARY_TOKEN: &str = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcde012345";

fn fixture_trajectory() -> Trajectory {
    let mut t = Trajectory::new();
    t.info.task = Some(format!("Fix tests using token {CANARY_TOKEN}"));
    t.info.outcome = Some(format!("submitted with key {CANARY_TOKEN}"));
    t.record_message(&Message::system(format!(
        "You are an agent. Secret: {CANARY_TOKEN}"
    )));
    t.record_message(&Message::user(format!(
        "Hello agent, use {CANARY_TOKEN} to authenticate"
    )));
    t.record_message(&Message::assistant(format!(
        "Authenticated with {CANARY_TOKEN}, proceeding"
    )));
    t
}

#[test]
fn all_registered_exporters_redact_secrets() {
    let traj = fixture_trajectory();
    let formats = registry();
    assert!(
        !formats.is_empty(),
        "registry() returned no exporters — expected at least MarkdownExporter"
    );
    for fmt in &formats {
        let output = (fmt.render)(&traj);
        assert!(
            !output.contains(CANARY_TOKEN),
            "Exporter '{}' (tier: {}) leaked raw secret token in output.\n\
             Every exporter MUST apply Redactor::default_enabled() on surface::EXPORT.\n\
             Output (first 400 chars):\n{}",
            fmt.name,
            fmt.tier.as_str(),
            &output[..output.len().min(400)]
        );
        assert!(
            output.contains("[REDACTED:"),
            "Exporter '{}' (tier: {}) produced no redaction markers — \
             expected [REDACTED:...] tokens where the secret appeared.\n\
             Output (first 400 chars):\n{}",
            fmt.name,
            fmt.tier.as_str(),
            &output[..output.len().min(400)]
        );
    }
}

#[test]
fn spec_documents_every_registered_format() {
    let formats = registry();
    let spec = match std::fs::read_to_string("docs/spec-export.md") {
        Ok(s) => s,
        Err(e) => panic!("docs/spec-export.md must exist (see issue #516): {e}"),
    };
    for fmt in &formats {
        assert!(
            spec.contains(fmt.name),
            "docs/spec-export.md does not mention format name '{}'. \
             Every registered exporter must be documented in the spec.",
            fmt.name
        );
        assert!(
            spec.contains(fmt.tier.as_str()),
            "docs/spec-export.md does not mention stability tier '{}' (used by format '{}'). \
             The spec must define and use both stability tiers.",
            fmt.tier.as_str(),
            fmt.name
        );
    }
}
