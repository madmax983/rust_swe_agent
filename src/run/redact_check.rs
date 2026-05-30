//! `agent redact-check` — zero-cost preflight for the secret-redaction config.
//!
//! Issue #321. Runs the resolved redaction config against operator-supplied sample
//! input and reports exactly what would be masked, by source. No model call is
//! made and no environment is launched.

use std::io::Read as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactSchemaVersion;
use crate::config::Config;
use crate::error::{ConfigError, Error};
use crate::exit_code::ExitCode;
use crate::redaction::{CheckMatch, CheckResult, Redactor};

// ── Public option / result types ──────────────────────────────────────────────

/// Where the sample input comes from.
pub enum RedactCheckSource {
    /// A literal string passed via `--text`.
    Text(String),
    /// A file path passed via `--file`.
    File(PathBuf),
    /// A trajectory artifact path passed via `--trajectory`.
    /// The file is read as text and the configured redactor is applied, mirroring
    /// the view-time pass that `bench inspect` performs.
    Trajectory(PathBuf),
    /// Read from stdin (no source flag given).
    Stdin,
}

/// Output format requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactCheckFormat {
    Human,
    Json,
}

/// Options passed to [`run_redact_check`].
pub struct RedactCheckOpts {
    pub source: RedactCheckSource,
    pub format: RedactCheckFormat,
    /// When true, exit non-zero if any `custom_patterns` entry produced zero matches.
    pub strict: bool,
}

/// The output of a `redact-check` run.
pub struct RedactCheckOutput {
    /// The original, un-redacted sample text.
    pub original: String,
    /// Detailed check result from the redactor.
    pub check: CheckResult,
    /// Whether `--strict` was requested.
    pub strict: bool,
}

impl RedactCheckOutput {
    /// Compute the CLI exit code based on the check result.
    ///
    /// - [`ExitCode::RedactCheckStrictFail`] (25) when `--strict` and any
    ///   `custom_patterns` entry produced zero matches.
    /// - [`ExitCode::RedactCheckStaleLiterals`] (24) when any `secret_literals`
    ///   entry produced zero matches.
    /// - [`ExitCode::Success`] (0) otherwise.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        if self.strict && !self.check.unmatched_pattern_indices.is_empty() {
            return ExitCode::RedactCheckStrictFail;
        }
        if !self.check.unmatched_literal_indices.is_empty() {
            return ExitCode::RedactCheckStaleLiterals;
        }
        ExitCode::Success
    }
}

// ── Core function ─────────────────────────────────────────────────────────────

/// Run the redaction config against the sample input described by `opts`.
///
/// Resolves the redactor from `cfg.root.redaction` (same precedence as a real
/// run). No model call, no environment, no side effects on stored artifacts.
pub fn run_redact_check(cfg: &Config, opts: &RedactCheckOpts) -> Result<RedactCheckOutput, Error> {
    let original = read_source(&opts.source)?;

    let redactor = Redactor::from_config(&cfg.root.redaction).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "invalid custom_patterns regex: {e}"
        )))
    })?;

    let check = redactor.check(&original);

    Ok(RedactCheckOutput {
        original,
        check,
        strict: opts.strict,
    })
}

fn read_source(source: &RedactCheckSource) -> Result<String, Error> {
    match source {
        RedactCheckSource::Text(text) => Ok(text.clone()),
        RedactCheckSource::File(path) | RedactCheckSource::Trajectory(path) => {
            std::fs::read_to_string(path).map_err(Error::Io)
        }
        RedactCheckSource::Stdin => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(Error::Io)?;
            Ok(buf)
        }
    }
}

// ── Formatters ────────────────────────────────────────────────────────────────

/// Format the check output as a human-readable annotated diff.
///
/// Shows the original input and the redacted output side by side (for short
/// inputs) or as a unified view, with each match annotated by source label.
/// Raw secret values are never included in the output.
#[must_use]
pub fn format_human(output: &RedactCheckOutput) -> String {
    let mut out = String::new();

    if output.check.matches.is_empty() {
        out.push_str("redact-check: no matches found\n");
        out.push_str("  redacted: (unchanged)\n");
        return out;
    }

    out.push_str(&format!(
        "redact-check: {} match(es) found\n\n",
        output.check.matches.len()
    ));

    // Print each match with annotation
    out.push_str("matches:\n");
    for m in &output.check.matches {
        out.push_str(&format!(
            "  [{start}..{end}] source={source}  marker={marker}\n",
            start = m.start,
            end = m.end,
            source = m.source,
            marker = m.marker,
        ));
    }

    out.push('\n');
    out.push_str("redacted output:\n");
    out.push_str(&output.check.redacted);
    if !output.check.redacted.ends_with('\n') {
        out.push('\n');
    }

    if !output.check.unmatched_literal_indices.is_empty() {
        out.push('\n');
        out.push_str(&format!(
            "warning: {} configured secret_literals entry(ies) produced no match \
             (indices: {:?}) — these may be stale\n",
            output.check.unmatched_literal_indices.len(),
            output.check.unmatched_literal_indices,
        ));
    }

    if output.strict && !output.check.unmatched_pattern_indices.is_empty() {
        out.push('\n');
        out.push_str(&format!(
            "warning: {} custom_patterns entry(ies) produced no match \
             (indices: {:?}) — check for typos or renamed token formats\n",
            output.check.unmatched_pattern_indices.len(),
            output.check.unmatched_pattern_indices,
        ));
    }

    out
}

/// The stable JSON schema for `--json` output.
///
/// Every field listed here is part of the public artifact contract.
#[derive(Debug, Serialize, Deserialize)]
pub struct RedactCheckJsonOutput {
    pub artifact_kind: String,
    pub schema_version: ArtifactSchemaVersion,
    /// All matches found, in order of appearance in the original input.
    pub matches: Vec<JsonMatch>,
    /// 0-based indices of `secret_literals` entries that produced no match.
    pub unmatched_literal_indices: Vec<usize>,
    /// 0-based indices of `custom_patterns` entries that produced no match.
    pub unmatched_pattern_indices: Vec<usize>,
    /// The redacted output text (secrets replaced by markers).
    pub redacted: String,
}

/// A single match entry in the JSON output.
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonMatch {
    /// Byte offset in the original input where the match starts.
    pub start: usize,
    /// Byte offset (exclusive) in the original input where the match ends.
    pub end: usize,
    /// The stable redaction marker assigned to this match.
    pub marker: String,
    /// Source label. One of: `"literal"`, `"custom_pattern[N]"`,
    /// `"structured:KIND"` (e.g. `"structured:pem"`, `"structured:bearer"`,
    /// `"structured:github_token"`, `"structured:api_key"`,
    /// `"structured:env_assignment"`), or `"env:NAME"`.
    /// Source labels never contain raw secret values.
    pub source: String,
}

/// Produce the stable JSON output document for `--json` mode.
///
/// The returned `serde_json::Value` is the public artifact contract documented
/// in `docs/spec-secret-redaction.md`. Raw secret values are never included.
pub fn format_json(output: &RedactCheckOutput) -> Result<serde_json::Value, serde_json::Error> {
    let doc = RedactCheckJsonOutput {
        artifact_kind: "redact_check".to_owned(),
        schema_version: ArtifactSchemaVersion::CURRENT,
        matches: output
            .check
            .matches
            .iter()
            .map(|m: &CheckMatch| JsonMatch {
                start: m.start,
                end: m.end,
                marker: m.marker.clone(),
                source: m.source.clone(),
            })
            .collect(),
        unmatched_literal_indices: output.check.unmatched_literal_indices.clone(),
        unmatched_pattern_indices: output.check.unmatched_pattern_indices.clone(),
        redacted: output.check.redacted.clone(),
    };
    serde_json::to_value(&doc)
}
