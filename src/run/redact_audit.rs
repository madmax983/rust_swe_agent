//! `agent redact-audit <dir>` — post-hoc secret-leak detection for sweep artifacts.
//!
//! Issue #342. The runtime [`Redactor`](crate::redaction::Redactor) masks secrets
//! *at write-time* and explicitly does not retroactively rewrite stored
//! artifacts. Trajectories and sweep outputs are routinely shared (PRs, HTML
//! exports, bundles), so a redaction-config bug or an unanticipated secret shape
//! can ship a secret to disk silently. This command scans a finished sweep tree
//! for high-confidence secret shapes — *in addition to* the configured redactor's
//! own literals and custom patterns — and reports where leaks live.
//!
//! It is a detector, not a mutator: it never rewrites artifacts (re-running the
//! sweep with a fixed config is the remediation) and it never prints raw secret
//! values. Every preview is masked with the same `[REDACTED:KIND:SIZE:HASH]`
//! marker scheme as the runtime redactor, except the hash is an *unsalted*
//! content digest so the report is byte-for-byte deterministic across runs and
//! usable with `--baseline`.
//
// Lint notes:
// - `case_sensitive_file_extension_comparisons`: sweep/bundle artifact names are
//   lowercase by harness convention; a case-insensitive compare would be wrong.
// - `cast_precision_loss`: the entropy estimate is a heuristic; f64 rounding of
//   a short token length is immaterial.
// - `naive_bytecount`: counting `\n` over already-in-memory text is fine; we do
//   not want a `bytecount` dependency for this.
#![allow(
    clippy::case_sensitive_file_extension_comparisons,
    clippy::cast_precision_loss,
    clippy::naive_bytecount
)]

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::artifact::ArtifactSchemaVersion;
use crate::config::{Config, RedactionCfg};
use crate::error::{ConfigError, Error};
use crate::exit_code::ExitCode;
use crate::redaction::{Redactor, sensitive_json_key_kind};

/// Schema version for the `redact_audit.json` artifact.
///
/// Kept separate from [`ArtifactSchemaVersion::CURRENT`] (mirroring
/// `redact-check`) so unrelated trajectory-schema bumps never silently change
/// the audit contract.
const REDACT_AUDIT_SCHEMA_VERSION: ArtifactSchemaVersion = ArtifactSchemaVersion::new(1, 0);

/// Number of context bytes captured on each side of a match for the preview.
const PREVIEW_CONTEXT: usize = 24;

/// Minimum token length for the entropy heuristic.
const ENTROPY_MIN_LEN: usize = 32;

// ── Severity ────────────────────────────────────────────────────────────────

/// Severity of a finding. Only `High` and `Medium` are "actionable"; the
/// `EntropyOnly` bucket is the noisy heuristic and never drives a failing exit
/// code on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Structurally unambiguous secret (cloud key, provider key, PEM block…).
    High,
    /// Plausible secret with a weaker shape (e.g. a JWT, which can be benign).
    Medium,
    /// High-entropy string flagged only by the heuristic detector.
    EntropyOnly,
}

impl Severity {
    /// `true` when this severity is at `medium` or above (drives exit code 32).
    const fn is_actionable(self) -> bool {
        matches!(self, Self::High | Self::Medium)
    }

    const fn label(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::EntropyOnly => "entropy_only",
        }
    }
}

// ── Detectors ───────────────────────────────────────────────────────────────

/// What a detector matches against.
enum DetectorKind {
    /// A regex; the optional capture group narrows the masked span (e.g. the
    /// value after `AccountKey=`).
    Regex { regex: Regex, group: Option<usize> },
    /// The high-entropy heuristic.
    Entropy,
}

/// A single detector in the registry.
struct Detector {
    /// Stable selector id used by `--detectors` (e.g. `aws`, `github`, `jwt`).
    id: &'static str,
    /// Specific match class recorded on each finding (e.g. `aws_access_key`).
    match_class: &'static str,
    severity: Severity,
    kind: DetectorKind,
}

/// A `(id, match_class, severity, pattern, group)` regex-detector spec.
type RegexSpec = (
    &'static str,
    &'static str,
    Severity,
    &'static str,
    Option<usize>,
);

/// The structured regex detectors, as data, so [`detector_registry`] stays small.
fn regex_specs() -> Vec<RegexSpec> {
    vec![
        (
            "aws",
            "aws_access_key",
            Severity::High,
            r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA|ANPA)[0-9A-Z]{16}\b",
            None,
        ),
        // AWS secret access keys have no fixed prefix, so anchor on the
        // conventional variable name (the shape in env dumps / logs the runtime
        // env-assignment redactor would catch). Group 1 masks the 40-char value.
        (
            "aws",
            "aws_secret_access_key",
            Severity::High,
            r#"(?i)aws_secret_access_key["']?\s*[:=]\s*["']?([A-Za-z0-9/+]{40})"#,
            Some(1),
        ),
        (
            "gcp",
            "gcp_api_key",
            Severity::High,
            r"\bAIza[0-9A-Za-z_\-]{35}\b",
            None,
        ),
        // The base64 account key after `AccountKey=` in a storage connection
        // string. Group 1 masks the key, not the label.
        (
            "azure",
            "azure_storage_key",
            Severity::High,
            r"AccountKey=([A-Za-z0-9+/]{43,}={0,2})",
            Some(1),
        ),
        (
            "anthropic",
            "anthropic_api_key",
            Severity::High,
            r"\bsk-ant-[A-Za-z0-9_\-]{20,}",
            None,
        ),
        // Body allows `_`/`-` separators, matching the runtime `structured:api_key`
        // family; the `sk-ant-` family is covered by the `anthropic` detector and
        // wins ties via the deterministic overlap filter.
        (
            "openai",
            "openai_api_key",
            Severity::High,
            r"\bsk-(?:proj-)?[A-Za-z0-9][A-Za-z0-9_-]{16,}",
            None,
        ),
        (
            "huggingface",
            "huggingface_token",
            Severity::High,
            r"\bhf_[A-Za-z0-9]{20,}\b",
            None,
        ),
        // Classic (`ghp_`, `gho_`, …) and fine-grained (`github_pat_`).
        (
            "github",
            "github_pat",
            Severity::High,
            r"\b(?:gh[pousr]_[A-Za-z0-9_]{36,}|github_pat_[A-Za-z0-9_]{22,})\b",
            None,
        ),
        (
            "slack",
            "slack_token",
            Severity::High,
            r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b",
            None,
        ),
        (
            "jwt",
            "jwt",
            Severity::Medium,
            r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
            None,
        ),
        (
            "pem",
            "pem_private_key",
            Severity::High,
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            None,
        ),
    ]
}

/// Build the full detector registry.
///
/// Returns an error only if a built-in regex fails to compile, which is a
/// programming error rather than user input.
fn detector_registry() -> Result<Vec<Detector>, regex::Error> {
    let mut detectors = Vec::new();
    for (id, match_class, severity, pattern, group) in regex_specs() {
        detectors.push(Detector {
            id,
            match_class,
            severity,
            kind: DetectorKind::Regex {
                regex: Regex::new(pattern)?,
                group,
            },
        });
    }
    detectors.push(Detector {
        id: "entropy",
        match_class: "high_entropy_string",
        severity: Severity::EntropyOnly,
        kind: DetectorKind::Entropy,
    });
    Ok(detectors)
}

/// Every selectable detector id (for `--detectors` validation and docs),
/// deduplicated and in registry order (a detector family such as `aws` may have
/// more than one underlying shape).
#[must_use]
pub fn detector_ids() -> Vec<&'static str> {
    let regs = detector_registry().unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for d in &regs {
        if seen.insert(d.id) {
            out.push(d.id);
        }
    }
    out
}

// ── Options / report types ──────────────────────────────────────────────────

/// Output destination for the human-readable summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditFormat {
    /// Human-readable summary table on stdout.
    Human,
    /// The JSON report on stdout.
    Json,
}

/// Options for [`run_redact_audit`].
pub struct AuditOpts {
    /// Directory tree of sweep artifacts to scan.
    pub dir: PathBuf,
    /// Subset of detector ids to run; `None` runs all.
    pub detectors: Option<Vec<String>>,
    /// Drop the high-entropy heuristic.
    pub disable_entropy: bool,
    /// Optional previous report; findings already present are not counted "new".
    pub baseline: Option<PathBuf>,
}

/// A single secret-leak finding. Never contains a raw secret value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finding {
    /// Path relative to the scanned directory. Bundle members use the
    /// `archive.tar.gz!inner/path` form.
    pub file: String,
    /// Byte offset of the match start within the UTF-8-decoded file content.
    pub byte_offset: usize,
    /// 1-based line number of the match start.
    pub line: usize,
    /// Detector that produced this finding (e.g. `aws`, `github`, `entropy`).
    pub detector_id: String,
    /// Severity bucket.
    pub severity: Severity,
    /// Specific match class (e.g. `aws_access_key`, `jwt`).
    pub match_class: String,
    /// Stable, non-reversible digest of the raw secret (first 12 hex of an
    /// unsalted SHA-256). Used for `--baseline` identity; never the raw value.
    pub match_fingerprint: String,
    /// Redacted preview of the surrounding context. Never the raw secret.
    pub preview: String,
    /// `true` when this finding is absent from the supplied `--baseline`.
    pub is_new: bool,
}

/// Counts by severity.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditSummary {
    /// Total findings across all severities.
    pub total: usize,
    /// Findings at `high` severity.
    pub high: usize,
    /// Findings at `medium` severity.
    pub medium: usize,
    /// Findings flagged only by the entropy heuristic.
    pub entropy_only: usize,
    /// Findings absent from the supplied baseline (equals `total` when no
    /// baseline is given).
    pub new_findings: usize,
}

/// The deterministic `redact_audit.json` report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditReport {
    /// Always `"redact_audit"`.
    pub artifact_kind: String,
    /// Schema version of this report.
    pub schema_version: ArtifactSchemaVersion,
    /// The scanned directory, as supplied.
    pub scanned_dir: String,
    /// Number of artifact files scanned.
    pub files_scanned: usize,
    /// Total lines scanned across all files (for false-positive-rate math).
    pub lines_scanned: usize,
    /// Detector ids that were active for this run.
    pub detectors: Vec<String>,
    /// All findings, ordered deterministically.
    pub findings: Vec<Finding>,
    /// Files that could not be read or extracted; presence drives exit code 33.
    pub scan_errors: Vec<String>,
    /// Severity rollup.
    pub summary: AuditSummary,
}

impl AuditReport {
    /// Compute the CLI exit code.
    ///
    /// - [`ExitCode::RedactAuditFindings`] (32) when any **new** finding is at
    ///   `medium`+ severity.
    /// - [`ExitCode::RedactAuditScanError`] (33) when a file could not be read.
    /// - [`ExitCode::Success`] (0) otherwise.
    ///
    /// Findings take precedence over scan errors: a real leak is the headline,
    /// and either way the non-zero code blocks a CI publish gate.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        let actionable_new = self
            .findings
            .iter()
            .any(|f| f.is_new && f.severity.is_actionable());
        if actionable_new {
            ExitCode::RedactAuditFindings
        } else if self.scan_errors.is_empty() {
            ExitCode::Success
        } else {
            ExitCode::RedactAuditScanError
        }
    }
}

// ── Core ────────────────────────────────────────────────────────────────────

/// Accumulated scan state, threaded through the per-file walk.
#[derive(Default)]
struct ScanAcc {
    findings: Vec<Finding>,
    scan_errors: Vec<String>,
    files_scanned: usize,
    lines_scanned: usize,
}

/// Operator-defined matchers from the `[redaction]` config.
///
/// Configured literals/custom patterns are scanned directly (not only via the
/// runtime redactor) so a configured secret that also matches a built-in
/// structured shape — e.g. a literal used as `Bearer <literal>` — is still
/// reported, instead of being dropped by the redactor's internal overlap
/// resolution. The runtime [`Redactor`] is additionally used as a detection
/// *oracle* (see [`collect_redactor_oracle_matches`]) so the audit reports every
/// shape that would have been masked at write time: bearer tokens, sensitive
/// `NAME=value` env-assignments, and ambient env-var literal values.
struct ConfiguredMatchers {
    literals: Vec<String>,
    patterns: Vec<Regex>,
    /// The runtime redactor built from the same `[redaction]` config, used as a
    /// write-time-parity oracle. `None` when redaction is disabled or the
    /// redactor failed to construct (configured-matcher fallback still applies).
    redactor: Option<Redactor>,
}

impl ConfiguredMatchers {
    /// Build from `[redaction]`. Returns a config error if a `custom_patterns`
    /// entry fails to compile (same validation the runtime redactor performs).
    fn from_cfg(cfg: &crate::config::RedactionCfg) -> Result<Self, Error> {
        if !cfg.enabled {
            return Ok(Self {
                literals: Vec::new(),
                patterns: Vec::new(),
                redactor: None,
            });
        }
        let mut seen = BTreeSet::new();
        let mut literals = Vec::new();
        for lit in &cfg.secret_literals {
            if !lit.is_empty() && seen.insert(lit.clone()) {
                literals.push(lit.clone());
            }
        }
        let mut patterns = Vec::new();
        for pat in &cfg.custom_patterns {
            patterns.push(Regex::new(pat).map_err(|e| {
                Error::Config(ConfigError::Invalid(format!(
                    "invalid custom_patterns regex: {e}"
                )))
            })?);
        }
        let redactor = Redactor::from_config(cfg).map_err(|e| {
            Error::Config(ConfigError::Invalid(format!(
                "invalid custom_patterns regex: {e}"
            )))
        })?;
        Ok(Self {
            literals,
            patterns,
            redactor: Some(redactor),
        })
    }
}

/// Resolved-config view used to recover a sweep's recorded `[redaction]` block.
#[derive(Deserialize)]
struct ResolvedRedactionConfig {
    #[serde(default)]
    redaction: Option<RedactionCfg>,
}

/// Merge the `[redaction]` config recorded in the sweep's provenance into `cfg`.
///
/// Completed sweeps record their resolved config (as a TOML string under
/// `/manifest/config/resolved` or `/config/resolved`) in `manifest.json` *and*
/// embed the same block inside `results.json` (`build_manifest`); `bench bundle`
/// falls back to the `results.json` copy when no standalone `manifest.json`
/// exists. Mirroring that here means auditing a sweep with defaults still
/// applies the literals/custom_patterns the sweep actually ran with. Recorded
/// entries are *added* to the CLI config (union, not replace), so an explicit
/// `--config` is never weakened. Redaction markers and uncompilable patterns are
/// skipped. Best-effort: missing or malformed provenance leaves `cfg` unchanged.
fn merge_recorded_sweep_redaction(dir: &Path, cfg: &mut RedactionCfg) {
    let mut existing_literals: BTreeSet<String> = cfg.secret_literals.iter().cloned().collect();
    let mut existing_patterns: BTreeSet<String> = cfg.custom_patterns.iter().cloned().collect();

    // Both files can carry the resolved config; scan each, deduping across them.
    for source_name in ["manifest.json", "results.json"] {
        let Ok(text) = std::fs::read_to_string(dir.join(source_name)) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        for pointer in ["/manifest/config/resolved", "/config/resolved"] {
            let Some(resolved) = json.pointer(pointer).and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Ok(parsed) = toml::from_str::<ResolvedRedactionConfig>(resolved) else {
                continue;
            };
            let Some(recorded) = parsed.redaction else {
                continue;
            };
            // If the sweep recorded redaction as active, run the oracle even when
            // the CLI invocation defaulted `enabled` off and the manifest carries
            // no recoverable literals/patterns (they are stored already-redacted).
            if recorded.enabled {
                cfg.enabled = true;
            }
            for lit in recorded.secret_literals {
                if !lit.is_empty()
                    && !lit.starts_with("[REDACTED:")
                    && existing_literals.insert(lit.clone())
                {
                    cfg.secret_literals.push(lit);
                }
            }
            for pat in recorded.custom_patterns {
                if !pat.starts_with("[REDACTED:")
                    && Regex::new(&pat).is_ok()
                    && existing_patterns.insert(pat.clone())
                {
                    cfg.custom_patterns.push(pat);
                }
            }
        }
    }
    // The recorded config implies redaction was active for the sweep; ensure the
    // oracle runs even if the CLI invocation defaulted it off.
    if !cfg.secret_literals.is_empty() || !cfg.custom_patterns.is_empty() {
        cfg.enabled = true;
    }
}

/// Run the audit over `opts.dir`, returning a deterministic report.
///
/// Audits the operator's configured `secret_literals` and `custom_patterns`
/// (from `cfg.root.redaction`, unioned with the sweep's recorded manifest
/// config) alongside the built-in detectors. No artifact is modified.
pub fn run_redact_audit(cfg: &Config, opts: &AuditOpts) -> Result<AuditReport, Error> {
    if !opts.dir.is_dir() {
        return Err(Error::Config(ConfigError::Usage(format!(
            "redact-audit: '{}' is not a directory",
            opts.dir.display()
        ))));
    }

    let detectors = select_detectors(opts)?;
    // Audit with the union of the CLI/default `[redaction]` config and the
    // resolved config recorded in the sweep's own `manifest.json` (if present),
    // so a sweep run with custom literals/patterns is still caught when the
    // documented `redact-audit runs/my-sweep` is later run with defaults.
    let mut redaction_cfg = cfg.root.redaction.clone();
    merge_recorded_sweep_redaction(&opts.dir, &mut redaction_cfg);
    let configured = ConfiguredMatchers::from_cfg(&redaction_cfg)?;

    let baseline = match &opts.baseline {
        Some(path) => Some(load_baseline(path)?),
        None => None,
    };

    let mut files = Vec::new();
    let mut walk_errors = Vec::new();
    collect_files(&opts.dir, &opts.dir, &mut files, &mut walk_errors);
    files.sort();

    let mut acc = ScanAcc::default();
    acc.scan_errors.append(&mut walk_errors);
    for path in files {
        scan_one(&path, &opts.dir, &detectors, &configured, &mut acc);
    }

    finalize(&mut acc.findings);
    mark_new(&mut acc.findings, baseline.as_ref());
    acc.scan_errors.sort();

    let summary = summarize(&acc.findings);
    Ok(AuditReport {
        artifact_kind: "redact_audit".to_owned(),
        schema_version: REDACT_AUDIT_SCHEMA_VERSION,
        scanned_dir: opts.dir.display().to_string(),
        files_scanned: acc.files_scanned,
        lines_scanned: acc.lines_scanned,
        detectors: active_detector_ids(&detectors),
        findings: acc.findings,
        scan_errors: acc.scan_errors,
        summary,
    })
}

/// Scan a single artifact path (plain file or bundle) into `acc`.
fn scan_one(
    path: &Path,
    base: &Path,
    detectors: &[Detector],
    configured: &ConfiguredMatchers,
    acc: &mut ScanAcc,
) {
    let rel = display_relative(base, path);
    if is_bundle(path) {
        match scan_bundle(path, &rel, detectors, configured) {
            Ok((mut found, scanned, lines, mut member_errors)) => {
                acc.findings.append(&mut found);
                acc.files_scanned += scanned;
                acc.lines_scanned += lines;
                acc.scan_errors.append(&mut member_errors);
            }
            Err(msg) => acc.scan_errors.push(msg),
        }
    } else {
        match read_text(path) {
            Ok(text) => {
                acc.files_scanned += 1;
                acc.lines_scanned += line_count(&text);
                scan_text(&text, &rel, detectors, configured, &mut acc.findings);
            }
            Err(msg) => acc.scan_errors.push(msg),
        }
    }
}

/// Resolve the active detector set from `--detectors` / `--disable-entropy`.
fn select_detectors(opts: &AuditOpts) -> Result<Vec<Detector>, Error> {
    let all = detector_registry().map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "internal detector regex failed to compile: {e}"
        )))
    })?;

    let mut selected: Vec<Detector> = match &opts.detectors {
        None => all,
        Some(requested) => {
            let known: BTreeSet<&str> = detector_ids().into_iter().collect();
            for r in requested {
                if !known.contains(r.as_str()) {
                    let mut names: Vec<&str> = known.iter().copied().collect();
                    names.sort_unstable();
                    return Err(Error::Config(ConfigError::Usage(format!(
                        "unknown detector '{r}'; valid detectors: {}",
                        names.join(", ")
                    ))));
                }
            }
            let want: BTreeSet<&str> = requested.iter().map(String::as_str).collect();
            all.into_iter().filter(|d| want.contains(d.id)).collect()
        }
    };

    if opts.disable_entropy {
        selected.retain(|d| !matches!(d.kind, DetectorKind::Entropy));
    }
    Ok(selected)
}

/// Deduplicated detector ids for the report's `detectors` field, in order.
/// A family such as `aws` may have several underlying regex shapes; the report
/// lists each selectable id once.
fn active_detector_ids(detectors: &[Detector]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for d in detectors {
        if seen.insert(d.id) {
            out.push(d.id.to_owned());
        }
    }
    out
}

/// Recursively collect candidate artifact files (names sorted by the caller).
///
/// An unreadable directory is recorded in `errors` rather than skipped
/// silently: for a publish gate, an unreadable subtree means the scan is
/// incomplete and a "clean" verdict cannot be trusted (drives exit code 33).
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<PathBuf>, errors: &mut Vec<String>) {
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    // Canonical root used to keep the walk inside the scanned sweep: a directory
    // symlink pointing outside `base` must not be followed (see below). Fall back
    // to the literal path when canonicalization fails so a normal walk proceeds.
    let canonical_base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    collect_files_inner(base, &canonical_base, dir, out, errors, &mut visited);
}

/// Inner walk that guards against directory symlink loops by tracking the
/// canonicalized path of every directory already descended into. A symlinked
/// directory pointing back at an ancestor would otherwise recurse forever
/// (`path.is_dir()` follows symlinks); a revisit is skipped silently.
fn collect_files_inner(
    base: &Path,
    canonical_base: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
    errors: &mut Vec<String>,
    visited: &mut BTreeSet<PathBuf>,
) {
    // Use the canonical path as the loop-detection key; fall back to the literal
    // path when canonicalization fails (e.g. permissions) so the read_dir below
    // still produces a proper scan error.
    let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    // Keep the walk inside the sweep: a directory symlink whose canonical target
    // escapes the scanned root would otherwise pull in unrelated files (a home
    // or workspace tree), producing false publish-gate failures. Skip it
    // silently, consistent with the loop guard — out-of-tree files are not part
    // of the sweep, so not following the link does not make the scan incomplete.
    if key != *canonical_base && !key.starts_with(canonical_base) {
        return;
    }
    if !visited.insert(key) {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push(format!(
                "{}: cannot read directory: {e}",
                display_relative(base, dir)
            ));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // A DirEntry that cannot be read (I/O / permission race) means
                // a file or subtree is skipped; record it so an incomplete scan
                // cannot report clean.
                errors.push(format!(
                    "{}: cannot read directory entry: {e}",
                    display_relative(base, dir)
                ));
                continue;
            }
        };
        let path = entry.path();
        if path.is_dir() {
            collect_files_inner(base, canonical_base, &path, out, errors, visited);
        } else if is_audited_file(&path) {
            // Keep file symlinks inside the sweep too: an artifact-looking
            // symlink whose canonical target escapes the scanned root would let
            // the scan read (and fail the gate on) unrelated external files.
            // Only escaping symlinks fail this check — a regular file always
            // canonicalizes to within `canonical_base`; if canonicalization
            // fails, keep the entry so read_text below surfaces a real error.
            let escapes = std::fs::canonicalize(&path)
                .map(|c| c != *canonical_base && !c.starts_with(canonical_base))
                .unwrap_or(false);
            if !escapes {
                out.push(path);
            }
        }
    }
}

/// `true` for the artifact kinds the auditor scans.
fn is_audited_file(path: &Path) -> bool {
    const SUFFIXES: [&str; 7] = [
        ".traj.json",
        ".output.txt",
        ".patch",
        ".md",
        ".html",
        ".csv",
        ".mermaid",
    ];
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            if name == "redact_audit.json" {
                return false;
            }
            // First-class sweep/bundle artifacts named exactly. `results.json`
            // is the `sweep_results` summary; `trajectory.json` is the legacy
            // nested single-run layout; `manifest.json` and `annotations.json`
            // are bundle-only JSON that `bench bundle` redaction-checks before
            // archiving — all read across the repo and worth scanning by name.
            name == "evaluation.json"
                || name == "results.json"
                || name == "trajectory.json"
                || name == "manifest.json"
                || name == "annotations.json"
                || (name.starts_with("all_preds") && name.ends_with(".jsonl"))
                || SUFFIXES.iter().any(|s| name.ends_with(s))
                || is_bundle(path)
        })
}

/// `true` for gzip bundle archives. The issue names `bundle.tar.zst`, but the
/// harness produces gzip (`flate2`) archives — see `src/run/bundle.rs` — so the
/// real extensions are `.tar.gz` / `.tgz`.
fn is_bundle(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with(".tar.gz") || name.ends_with(".tgz"))
}

/// Read a file as UTF-8 (lossily), so binary noise never aborts a scan.
fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| format!("{}: cannot read file: {e}", path.display()))
}

/// Extract a `.tar.gz` bundle to memory and scan its audited members.
///
/// Returns `(findings, members_scanned, lines_scanned, member_errors)`. Both a
/// member whose body cannot be read *and* a corrupt tar entry encountered
/// mid-stream are recorded in `member_errors`; either way the findings already
/// collected from earlier members are preserved (findings-over-scan-errors).
/// The outer `Err` is reserved for a bundle that cannot even be opened or whose
/// index is unreadable before any member is seen.
type BundleScan = (Vec<Finding>, usize, usize, Vec<String>);

fn scan_bundle(
    path: &Path,
    rel: &str,
    detectors: &[Detector],
    configured: &ConfiguredMatchers,
) -> Result<BundleScan, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("{}: cannot open bundle: {e}", path.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    let entries = archive
        .entries()
        .map_err(|e| format!("{}: cannot read bundle entries: {e}", path.display()))?;

    let mut findings = Vec::new();
    let mut member_errors = Vec::new();
    let mut scanned = 0usize;
    let mut lines = 0usize;
    for entry in entries {
        // A corrupt entry mid-stream ends iteration, but keep what we already
        // found in earlier members rather than discarding the whole bundle.
        let mut entry = match entry {
            Ok(e) => e,
            Err(e) => {
                member_errors.push(format!("{}: corrupt bundle entry: {e}", path.display()));
                break;
            }
        };
        let inner = match entry.path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(e) => {
                // A member whose path metadata cannot be decoded cannot be
                // classified by `is_audited_file`. Record it instead of silently
                // dropping it, so a malformed bundle cannot report clean.
                member_errors.push(format!("{rel}: cannot read bundle member path: {e}"));
                continue;
            }
        };
        if !is_audited_file(Path::new(&inner)) || inner.ends_with(".tar.gz") {
            continue;
        }
        let member_label = format!("{rel}!{inner}");
        let mut bytes = Vec::new();
        if let Err(e) = entry.read_to_end(&mut bytes) {
            // A readable header with an unreadable body means the scan of this
            // member is incomplete; record it so the audit cannot report clean.
            member_errors.push(format!("{member_label}: cannot read bundle member: {e}"));
            continue;
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        scanned += 1;
        lines += line_count(&text);
        scan_text(&text, &member_label, detectors, configured, &mut findings);
    }
    Ok((findings, scanned, lines, member_errors))
}

/// A pre-filter candidate match.
struct RawMatch {
    start: usize,
    end: usize,
    detector_id: &'static str,
    match_class: &'static str,
    severity: Severity,
}

/// Collect built-in **structured** (regex) detector matches into `candidates`.
///
/// The entropy heuristic is handled separately so it can never shadow a
/// structured match: its greedy token can start earlier and span wider than a
/// precise regex hit, and a naive earliest-start overlap filter would otherwise
/// demote a high-severity key to `entropy_only`.
fn collect_detector_matches(text: &str, detectors: &[Detector], candidates: &mut Vec<RawMatch>) {
    for det in detectors {
        let DetectorKind::Regex { regex, group } = &det.kind else {
            continue;
        };
        for caps in regex.captures_iter(text) {
            let m = group.and_then(|g| caps.get(g)).or_else(|| caps.get(0));
            if let Some(m) = m {
                if m.start() < m.end() {
                    candidates.push(RawMatch {
                        start: m.start(),
                        end: m.end(),
                        detector_id: det.id,
                        match_class: det.match_class,
                        severity: det.severity,
                    });
                }
            }
        }
    }
}

/// Collect the operator-defined configured literals and custom patterns, audited
/// "in addition to" the built-in detectors.
///
/// These are scanned directly (not via `Redactor::check()`) so a configured
/// secret that also matches a built-in structured shape — e.g. a literal used as
/// `Bearer <literal>` — is still reported, instead of being filtered out by the
/// runtime redactor's internal overlap resolution.
fn collect_configured_matches(
    text: &str,
    configured: &ConfiguredMatchers,
    candidates: &mut Vec<RawMatch>,
) {
    for literal in &configured.literals {
        let mut from = 0usize;
        while let Some(off) = text[from..].find(literal.as_str()) {
            let start = from + off;
            let end = start + literal.len();
            candidates.push(RawMatch {
                start,
                end,
                detector_id: "configured_literal",
                match_class: "configured_literal",
                severity: Severity::High,
            });
            from = end;
        }
    }
    for re in &configured.patterns {
        for m in re.find_iter(text) {
            if m.start() < m.end() {
                candidates.push(RawMatch {
                    start: m.start(),
                    end: m.end(),
                    detector_id: "configured_pattern",
                    match_class: "configured_custom_pattern",
                    severity: Severity::High,
                });
            }
        }
    }
}

/// Map a runtime redactor source label to `(detector_id, match_class, severity)`.
///
/// `None` only for sources already reported verbatim elsewhere
/// (`literal`/`custom_pattern[N]` are handled by [`collect_configured_matches`]).
/// Every *structured* shape the runtime redactor masks is surfaced here so the
/// audit never misses a value that would have been redacted at write time, even
/// when the high-confidence registry's tighter length/charset misses it (the
/// deterministic overlap filter de-dupes when both fire on the same span).
fn oracle_source_kind(source: &str) -> Option<(&'static str, &'static str, Severity)> {
    match source {
        // Bearer tokens have no provider prefix and are missed by the registry.
        "structured:bearer" => Some(("redactor_bearer", "bearer_token", Severity::High)),
        // PEM blocks and provider API keys: high-confidence even via the oracle.
        "structured:pem" => Some(("redactor_pem", "pem_private_key", Severity::High)),
        "structured:github_token" => Some(("redactor_github", "github_pat", Severity::High)),
        "structured:api_key" => Some(("redactor_api_key", "api_key", Severity::High)),
        // Any sensitive `NAME=value` assignment the runtime redactor would mask.
        "structured:env_assignment" => Some((
            "redactor_env_assignment",
            "sensitive_env_assignment",
            Severity::Medium,
        )),
        _ if source.starts_with("env:") => {
            // An ambient sensitive env-var value found verbatim in an artifact.
            Some(("redactor_env_value", "sensitive_env_value", Severity::High))
        }
        _ => None,
    }
}

/// Use the configured runtime [`Redactor`] as a detection oracle so the audit
/// reports every shape it would have masked at write time — bearer tokens,
/// sensitive env-assignments, and ambient env-var literal values — that the
/// high-confidence registry does not already cover.
fn collect_redactor_oracle_matches(
    text: &str,
    configured: &ConfiguredMatchers,
    candidates: &mut Vec<RawMatch>,
) {
    let Some(redactor) = &configured.redactor else {
        return;
    };
    for m in redactor.check(text).matches {
        if m.start >= m.end {
            continue;
        }
        // An artifact that was already redacted at write time contains
        // `[REDACTED:…]` markers; the redactor re-matches the marker text (e.g.
        // a `DATABASE_PASSWORD=[REDACTED:…]` assignment). Reporting a *fully*
        // redacted value would fail the publish gate on correctly-redacted
        // output, so skip it — but a value that is only partially redacted
        // (marker + raw residue like `[REDACTED:…]hunter2`) still leaks and must
        // be flagged.
        if is_fully_redacted(&text[m.start..m.end]) {
            continue;
        }
        if let Some((detector_id, match_class, severity)) = oracle_source_kind(&m.source) {
            candidates.push(RawMatch {
                start: m.start,
                end: m.end,
                detector_id,
                match_class,
                severity,
            });
        }
    }
}

/// `true` when `value` is *fully* covered by existing runtime redaction markers
/// — i.e. it carries at least one `[REDACTED:…]` marker and, once every marker
/// is stripped, no raw secret-ish residue remains.
///
/// Already-redacted artifacts carry `[REDACTED:KIND:SIZE:HASH]` markers; treating
/// them as fresh leaks would fail the publish gate on correctly-redacted output.
/// But a *partially* redacted value such as
/// `DATABASE_PASSWORD=[REDACTED:…]hunter2` still contains raw secret material
/// (`hunter2`): the runtime redactor would re-mask the whole assignment value, so
/// the audit must still flag it rather than skip on the mere presence of a
/// marker. Residue is "secret-ish" if it contains any alphanumeric byte;
/// structural leftovers (quotes, whitespace) do not count.
fn is_fully_redacted(value: &str) -> bool {
    if !value.contains("[REDACTED:") {
        return false;
    }
    !strip_redaction_markers(value)
        .chars()
        .any(char::is_alphanumeric)
}

/// Remove every `[REDACTED:…]` marker substring from `value`, returning the
/// remaining (non-marker) text. An unterminated `[REDACTED:` consumes the rest.
fn strip_redaction_markers(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(idx) = rest.find("[REDACTED:") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx..];
        if let Some(close) = after.find(']') {
            rest = &after[close + 1..];
        } else {
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Walk JSON artifacts and flag values under sensitive keys (e.g. `password`,
/// `api_key`, `*_token`) regardless of value shape, mirroring the runtime
/// [`Redactor::redact_json_value`](crate::redaction::Redactor) structural pass.
/// Handles both whole-file JSON and JSONL; silent on non-JSON text.
///
/// Values are located by their *verbatim source bytes* via a lightweight
/// source-span scanner rather than via serde decode + re-serialization.
/// The earlier re-serialization approach decoded the value through serde and
/// then searched for the canonical re-encoded form, causing it to miss inputs
/// that used non-canonical escape sequences such as `hunter2` (unicode
/// escape for `h`) or `abc\/def` (escaped forward slash): serde would
/// round-trip both to their canonical forms (`hunter2`, `abc/def`), making
/// the source bytes unfindable with a simple `str::find`.
fn collect_json_sensitive_keys(text: &str, candidates: &mut Vec<RawMatch>) {
    let src = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();

    // Try whole-file JSON first.
    let mut pos = 0usize;
    json_skip_ws(src, &mut pos);
    if pos < src.len() {
        json_scan_value(src, &mut pos, false, &mut spans, 0);
    }
    json_skip_ws(src, &mut pos);

    if pos < src.len() {
        // Whole-file parse did not consume the full text — treat as JSONL.
        spans.clear();
        let mut line_byte_offset = 0usize;
        for line in text.split('\n') {
            let trim_prefix = line.len() - line.trim_start().len();
            let trimmed = &line[trim_prefix..];
            if !trimmed.is_empty() {
                let line_src = trimmed.as_bytes();
                let mut lpos = 0usize;
                json_scan_value(
                    line_src,
                    &mut lpos,
                    false,
                    &mut spans,
                    line_byte_offset + trim_prefix,
                );
            }
            line_byte_offset += line.len() + 1; // +1 for the '\n' separator
        }
    }

    for (start, end) in spans {
        if start >= end {
            continue; // empty string value
        }
        // Skip values fully covered by existing markers, but still flag a value
        // that mixes a marker with raw residue (e.g. `[REDACTED:…]hunter2`).
        if is_fully_redacted(&String::from_utf8_lossy(&src[start..end])) {
            continue;
        }
        candidates.push(RawMatch {
            start,
            end,
            detector_id: "json_sensitive_key",
            match_class: "sensitive_json_value",
            severity: Severity::Medium,
        });
    }
}

/// Advance `*pos` past ASCII whitespace in `src`.
fn json_skip_ws(src: &[u8], pos: &mut usize) {
    while *pos < src.len() && matches!(src[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

/// Scan a JSON string at `src[*pos]` (must be `"`).  Returns
/// `(content_start, content_end)` — positions *inside* the surrounding quotes
/// in `src`-relative coordinates — and advances `*pos` past the closing quote.
/// Returns `None` if `*pos` is not at `"` or the string is unterminated.
fn json_scan_string(src: &[u8], pos: &mut usize) -> Option<(usize, usize)> {
    if *pos >= src.len() || src[*pos] != b'"' {
        return None;
    }
    *pos += 1;
    let content_start = *pos;
    while *pos < src.len() {
        match src[*pos] {
            b'"' => {
                let content_end = *pos;
                *pos += 1;
                return Some((content_start, content_end));
            }
            b'\\' => {
                *pos += 1;
                if *pos < src.len() {
                    if src[*pos] == b'u' {
                        *pos = (*pos + 5).min(src.len()); // \uXXXX: skip u + 4 hex digits
                    } else {
                        *pos += 1; // single-char escape
                    }
                }
            }
            _ => *pos += 1,
        }
    }
    None // unterminated string
}

/// Decode JSON string escape sequences in `raw` (the bytes between the quotes).
/// Used only for key-name lookup — values are retained as verbatim source spans.
fn json_decode_key(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] == b'\\' && i + 1 < raw.len() {
            i += 1;
            match raw[i] {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'b' => out.push('\x08'),
                b'f' => out.push('\x0c'),
                b'u' if i + 4 < raw.len() => {
                    let hex = &raw[i + 1..i + 5];
                    if let Ok(s) = std::str::from_utf8(hex) {
                        if let Ok(n) = u16::from_str_radix(s, 16) {
                            if let Some(c) = char::from_u32(u32::from(n)) {
                                out.push(c);
                            }
                        }
                    }
                    i += 4; // outer loop adds 1 more → 5 total (u + 4 hex)
                }
                _ => out.push(raw[i] as char),
            }
        } else {
            out.push(raw[i] as char);
        }
        i += 1;
    }
    out
}

/// Scan any JSON value at `src[*pos]`.  Records content spans (in
/// original-text coordinates via `base_offset`) for string values that are
/// under a sensitive key.
fn json_scan_value(
    src: &[u8],
    pos: &mut usize,
    under_sensitive_key: bool,
    spans: &mut Vec<(usize, usize)>,
    base_offset: usize,
) {
    json_skip_ws(src, pos);
    if *pos >= src.len() {
        return;
    }
    match src[*pos] {
        b'"' => {
            if let Some((cs, ce)) = json_scan_string(src, pos) {
                if under_sensitive_key {
                    spans.push((base_offset + cs, base_offset + ce));
                }
            }
        }
        b'{' => json_scan_object(src, pos, under_sensitive_key, spans, base_offset),
        b'[' => json_scan_array(src, pos, spans, base_offset, under_sensitive_key),
        _ => json_skip_primitive(src, pos),
    }
}

/// Scan a JSON object `{ "key": value, … }`.
///
/// `under_sensitive_key` carries the enclosing context: when the object is
/// itself the value of a sensitive key, *every* nested value is sensitive,
/// mirroring the runtime `Redactor::redact_sensitive_value`, which recurses
/// through all values under a sensitive key regardless of inner field names
/// (e.g. `{"credentials":{"value":"…"}}`).
fn json_scan_object(
    src: &[u8],
    pos: &mut usize,
    under_sensitive_key: bool,
    spans: &mut Vec<(usize, usize)>,
    base_offset: usize,
) {
    if *pos >= src.len() || src[*pos] != b'{' {
        return;
    }
    *pos += 1;
    loop {
        json_skip_ws(src, pos);
        if *pos >= src.len() {
            break;
        }
        match src[*pos] {
            b'}' => {
                *pos += 1;
                break;
            }
            b',' => {
                *pos += 1;
                continue;
            }
            _ => {}
        }
        // Always consume the key string (advancing `pos`), then OR its own
        // sensitivity with the inherited context — short-circuiting on
        // `under_sensitive_key` would leave `pos` parked on the key.
        let key_is_sensitive = json_scan_string(src, pos).is_some_and(|(ks, ke)| {
            sensitive_json_key_kind(&json_decode_key(&src[ks..ke])).is_some()
        });
        let key_sensitive = under_sensitive_key || key_is_sensitive;
        json_skip_ws(src, pos);
        if *pos < src.len() && src[*pos] == b':' {
            *pos += 1;
        }
        json_scan_value(src, pos, key_sensitive, spans, base_offset);
    }
}

/// Scan a JSON array `[ value, … ]`.
fn json_scan_array(
    src: &[u8],
    pos: &mut usize,
    spans: &mut Vec<(usize, usize)>,
    base_offset: usize,
    under_sensitive_key: bool,
) {
    if *pos >= src.len() || src[*pos] != b'[' {
        return;
    }
    *pos += 1;
    loop {
        json_skip_ws(src, pos);
        if *pos >= src.len() {
            break;
        }
        match src[*pos] {
            b']' => {
                *pos += 1;
                break;
            }
            b',' => {
                *pos += 1;
                continue;
            }
            _ => {}
        }
        json_scan_value(src, pos, under_sensitive_key, spans, base_offset);
    }
}

/// Skip a JSON primitive (number, `true`, `false`, `null`).
fn json_skip_primitive(src: &[u8], pos: &mut usize) {
    while *pos < src.len()
        && !matches!(src[*pos], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
    {
        *pos += 1;
    }
}

/// Scan one text blob, appending findings for `file_label`.
///
/// Safety model: every kept match span is replaced with its marker to build a
/// single fully-redacted copy of `text`, and each finding's preview is derived
/// as a *slice of that redacted copy*. Because the redacted copy contains no raw
/// secret bytes anywhere, no preview can ever leak a raw secret — this holds
/// regardless of detector span precision.
fn scan_text(
    text: &str,
    file_label: &str,
    detectors: &[Detector],
    configured: &ConfiguredMatchers,
    out: &mut Vec<Finding>,
) {
    // Phase 1: structured detectors — built-in regex, configured literals /
    // custom patterns, the runtime-redactor oracle (bearer / env-assignment /
    // env-value), and the JSON sensitive-key walk. These always win over entropy.
    let mut structured: Vec<RawMatch> = Vec::new();
    collect_detector_matches(text, detectors, &mut structured);
    collect_configured_matches(text, configured, &mut structured);
    collect_redactor_oracle_matches(text, configured, &mut structured);
    collect_json_sensitive_keys(text, &mut structured);
    let kept_structured = filter_overlaps(structured);

    // Phase 2: the entropy heuristic only fills gaps the structured pass left,
    // so a greedy high-entropy token can never shadow a precise key match.
    let mut entropy = Vec::new();
    if let Some(det) = detectors
        .iter()
        .find(|d| matches!(d.kind, DetectorKind::Entropy))
    {
        collect_entropy_matches(text, det, &mut entropy);
        entropy.retain(|e| !kept_structured.iter().any(|s| overlaps(s, e)));
    }
    let kept_entropy = filter_overlaps(entropy);

    // Merge into a single set of non-overlapping spans in positional order.
    let mut all: Vec<RawMatch> = kept_structured;
    all.extend(kept_entropy);
    all.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));

    // Build the fully-redacted copy and record each marker's position within it.
    let mut redacted_text = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(all.len());
    for c in &all {
        redacted_text.push_str(&text[last..c.start]);
        let span_start = redacted_text.len();
        redacted_text.push_str(&marker(c.match_class, &text[c.start..c.end]));
        spans.push((span_start, redacted_text.len()));
        last = c.end;
    }
    redacted_text.push_str(&text[last..]);

    for (c, &(span_start, span_end)) in all.iter().zip(spans.iter()) {
        out.push(Finding {
            file: file_label.to_owned(),
            byte_offset: c.start,
            line: line_at(text, c.start),
            detector_id: c.detector_id.to_owned(),
            severity: c.severity,
            match_class: c.match_class.to_owned(),
            match_fingerprint: fingerprint(&text[c.start..c.end]),
            preview: preview_from_masked(&redacted_text, span_start, span_end),
            is_new: true,
        });
    }
}

/// Two candidate spans overlap when their byte ranges intersect.
fn overlaps(a: &RawMatch, b: &RawMatch) -> bool {
    a.start < b.end && b.start < a.end
}

/// Greedily drop overlapping candidates, preferring earlier start, then longer
/// span, then higher severity, then detector id — deterministic in all cases.
fn filter_overlaps(mut candidates: Vec<RawMatch>) -> Vec<RawMatch> {
    candidates.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| b.end.cmp(&a.end))
            .then_with(|| a.severity.cmp(&b.severity))
            .then_with(|| a.detector_id.cmp(b.detector_id))
    });
    let mut kept: Vec<RawMatch> = Vec::new();
    let mut next_available = 0usize;
    for c in candidates {
        if c.start < next_available {
            continue;
        }
        next_available = c.end;
        kept.push(c);
    }
    kept
}

/// Tokenize on secret-ish characters and flag high-entropy runs.
fn collect_entropy_matches(text: &str, det: &Detector, out: &mut Vec<RawMatch>) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if is_token_byte(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_token_byte(bytes[i]) {
                i += 1;
            }
            let token = &text[start..i];
            if token.len() >= ENTROPY_MIN_LEN && looks_random(token) {
                out.push(RawMatch {
                    start,
                    end: i,
                    detector_id: det.id,
                    match_class: det.match_class,
                    severity: det.severity,
                });
            }
        } else {
            i += 1;
        }
    }
}

const fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// Conservative randomness test: Shannon entropy >= 4.0 bits/char and at least
/// two of {lowercase, uppercase, digit} present (so prose and hex-only ids that
/// are usually benign don't dominate the noise budget).
fn looks_random(token: &str) -> bool {
    let mut counts = [0u32; 256];
    for &b in token.as_bytes() {
        counts[b as usize] += 1;
    }
    let len = token.len() as f64;
    let entropy: f64 = counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / len;
            -p * p.log2()
        })
        .sum();
    if entropy < 4.0 {
        return false;
    }
    let has_lower = token.bytes().any(|b| b.is_ascii_lowercase());
    let has_upper = token.bytes().any(|b| b.is_ascii_uppercase());
    let has_digit = token.bytes().any(|b| b.is_ascii_digit());
    u8::from(has_lower) + u8::from(has_upper) + u8::from(has_digit) >= 2
}

/// Number of lines in `text` (counts a trailing partial line).
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.bytes().filter(|&b| b == b'\n').count() + usize::from(!text.ends_with('\n'))
}

/// 1-based line number of byte offset `at`.
fn line_at(text: &str, at: usize) -> usize {
    1 + text.as_bytes()[..at.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// Build a one-line preview as a window of the already-redacted text.
///
/// `redacted` is the full text with every detected secret already replaced by
/// its marker, and `[span_start, span_end)` is this finding's marker span. The
/// window extends [`PREVIEW_CONTEXT`] bytes on each side of the marker. Because
/// `redacted` contains no raw secret bytes, the returned preview can never leak
/// a secret — by construction, not by detector precision.
fn preview_from_masked(redacted: &str, span_start: usize, span_end: usize) -> String {
    let win_start = floor_boundary(redacted, span_start.saturating_sub(PREVIEW_CONTEXT));
    let win_end = ceil_boundary(redacted, (span_end + PREVIEW_CONTEXT).min(redacted.len()));
    sanitize_line(&redacted[win_start..win_end])
}

/// Collapse control characters so the preview is a single, log-safe line.
fn sanitize_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn floor_boundary(text: &str, mut i: usize) -> usize {
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(text: &str, mut i: usize) -> usize {
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Deterministic, content-addressed marker mirroring the runtime scheme
/// `[REDACTED:KIND:SIZE:HASH]`. The hash is an **unsalted** digest so identical
/// secrets produce identical markers across runs (required for determinism and
/// `--baseline`).
fn marker(match_class: &str, raw: &str) -> String {
    format!(
        "[REDACTED:{match_class}:{}:{}]",
        size_class(raw),
        short_hash(raw, 8)
    )
}

/// Stable fingerprint of a raw secret (first 12 hex of unsalted SHA-256).
fn fingerprint(raw: &str) -> String {
    short_hash(raw, 12)
}

fn short_hash(raw: &str, n: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..n.min(digest.len())].to_owned()
}

/// Size bucket for a secret, matching the runtime redactor's classes.
fn size_class(value: &str) -> &'static str {
    match value.len() {
        0..=15 => "short",
        16..=63 => "medium",
        _ => "long",
    }
}

/// Sort findings into a deterministic order.
fn finalize(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.byte_offset.cmp(&b.byte_offset))
            .then_with(|| a.detector_id.cmp(&b.detector_id))
            .then_with(|| a.match_class.cmp(&b.match_class))
    });
}

/// Flag findings absent from the baseline (`is_new`).
fn mark_new(findings: &mut [Finding], baseline: Option<&BTreeSet<String>>) {
    if let Some(baseline) = baseline {
        for f in findings.iter_mut() {
            f.is_new = !baseline.contains(&finding_identity(f));
        }
    }
}

/// Stable identity tuple for baseline comparison (no offsets, so unrelated edits
/// elsewhere in a file don't re-flag a grandfathered finding).
fn finding_identity(f: &Finding) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        f.file, f.detector_id, f.match_class, f.match_fingerprint
    )
}

/// Load a previous report and build its finding-identity set.
fn load_baseline(path: &Path) -> Result<BTreeSet<String>, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(ConfigError::Usage(format!(
            "cannot read --baseline '{}': {e}",
            path.display()
        )))
    })?;
    let report: AuditReport = serde_json::from_str(&text).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "--baseline '{}' is not a valid redact_audit.json: {e}",
            path.display()
        )))
    })?;
    Ok(report.findings.iter().map(finding_identity).collect())
}

fn summarize(findings: &[Finding]) -> AuditSummary {
    let mut s = AuditSummary {
        total: findings.len(),
        ..AuditSummary::default()
    };
    for f in findings {
        match f.severity {
            Severity::High => s.high += 1,
            Severity::Medium => s.medium += 1,
            Severity::EntropyOnly => s.entropy_only += 1,
        }
        if f.is_new {
            s.new_findings += 1;
        }
    }
    s
}

/// Relative display path of `path` under `base`.
fn display_relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

// ── Formatters ──────────────────────────────────────────────────────────────

/// Serialize the report as pretty JSON (the public artifact contract).
pub fn format_json(report: &AuditReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

/// Render a human-readable summary table. Never prints raw secrets.
#[must_use]
pub fn format_human(report: &AuditReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "redact-audit: scanned {} file(s), {} line(s) in {}",
        report.files_scanned, report.lines_scanned, report.scanned_dir
    );

    if report.findings.is_empty() {
        out.push_str("no findings\n");
    } else {
        let _ = writeln!(out, "{} finding(s):\n", report.findings.len());
        for f in &report.findings {
            let tag = if f.is_new { "" } else { " (baseline)" };
            let _ = writeln!(
                out,
                "  {sev:<12} {class:<22} {file}:{line} (+{off}){tag}",
                sev = f.severity.label(),
                class = f.match_class,
                file = f.file,
                line = f.line,
                off = f.byte_offset,
            );
            let _ = writeln!(out, "      {}", f.preview);
        }
        out.push('\n');
    }

    let _ = writeln!(
        out,
        "summary: high={} medium={} entropy_only={} new={} total={}",
        report.summary.high,
        report.summary.medium,
        report.summary.entropy_only,
        report.summary.new_findings,
        report.summary.total,
    );

    if !report.scan_errors.is_empty() {
        let _ = writeln!(out, "\nscan errors ({}):", report.scan_errors.len());
        for e in &report.scan_errors {
            let _ = writeln!(out, "  {e}");
        }
    }

    out
}

/// Parse `--format` into an [`AuditFormat`].
pub fn parse_format(json_flag: bool, format: &str) -> Result<AuditFormat, Error> {
    if json_flag {
        return Ok(AuditFormat::Json);
    }
    match format {
        "json" => Ok(AuditFormat::Json),
        "human" | "" => Ok(AuditFormat::Human),
        other => Err(Error::Config(ConfigError::Invalid(format!(
            "--format '{other}' is not valid; use 'human' or 'json'"
        )))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::pedantic)]
    use super::*;

    fn default_cfg() -> Config {
        Config::defaults().unwrap()
    }

    fn audit(dir: &Path) -> AuditReport {
        run_redact_audit(
            &default_cfg(),
            &AuditOpts {
                dir: dir.to_path_buf(),
                detectors: None,
                disable_entropy: false,
                baseline: None,
            },
        )
        .unwrap()
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn detectors_catch_each_planted_class() {
        let dir = tempfile::tempdir().unwrap();
        // One planted secret per high/medium detector class.
        let planted = [
            (
                "aws.output.txt",
                "id=AKIAIOSFODNN7EXAMPLE done",
                "aws_access_key",
            ),
            (
                "gcp.output.txt",
                "key=AIzaABCDE12345ABCDE12345ABCDE12345FGHIJ end",
                "gcp_api_key",
            ),
            (
                "anthropic.output.txt",
                "k=sk-ant-api03-abcdefghij1234567890XYZ done",
                "anthropic_api_key",
            ),
            (
                "openai.output.txt",
                "k=sk-abcdefghij1234567890ABCDEFGH done",
                "openai_api_key",
            ),
            (
                "hf.output.txt",
                "k=hf_abcdefghijklmnopqrstuvwxyz12 done",
                "huggingface_token",
            ),
            (
                "gh.output.txt",
                "t=ghp_0123456789abcdefghijklmnopqrstuvwxyzA done",
                "github_pat",
            ),
            (
                "slack.output.txt",
                "t=xoxb-123456789012-abcdefABCDEF done",
                "slack_token",
            ),
            (
                "jwt.output.txt",
                "t=eyJhbGciOiJI.eyJzdWIiOiIx.SflKxwRJSME done",
                "jwt",
            ),
            (
                "azure.output.txt",
                "DefaultEndpointsProtocol=https;AccountKey=YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXowMTIzNDU2Nzg5QUJDREVGR0g=;",
                "azure_storage_key",
            ),
        ];
        for (name, body, _) in planted {
            write(dir.path(), name, body);
        }
        write(
            dir.path(),
            "pem.output.txt",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIBcap\n-----END RSA PRIVATE KEY-----\n",
        );

        let report = audit(dir.path());
        let classes: BTreeSet<&str> = report
            .findings
            .iter()
            .map(|f| f.match_class.as_str())
            .collect();
        for (_, _, class) in planted {
            assert!(classes.contains(class), "missing detector for {class}");
        }
        assert!(
            classes.contains("pem_private_key"),
            "missing pem detector; classes were {classes:?}"
        );
    }

    #[test]
    fn never_emits_raw_secret_in_report() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyzA";
        write(
            dir.path(),
            "leak.output.txt",
            &format!("token={secret} trailing"),
        );
        let report = audit(dir.path());
        let json = format_json(&report).unwrap();
        assert!(!json.contains(secret), "raw secret leaked into JSON report");
        assert!(
            !format_human(&report).contains(secret),
            "raw secret leaked into human output"
        );
        assert!(report.findings.iter().all(|f| !f.preview.contains(secret)));
    }

    #[test]
    fn clean_artifact_has_no_actionable_findings() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "hello.traj.json",
            r#"{"schema":"mini-swe-agent-1.1","outcome":"submitted","messages":[{"role":"user","content":"fix the bug in src/lib.rs"},{"role":"assistant","content":"I ran cargo test and all tests passed."}]}"#,
        );
        let report = audit(dir.path());
        assert_eq!(report.summary.high, 0, "{:?}", report.findings);
        assert_eq!(report.summary.medium, 0, "{:?}", report.findings);
    }

    #[test]
    fn exit_code_reflects_actionable_findings() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "leak.output.txt", "AKIAIOSFODNN7EXAMPLE");
        let report = audit(dir.path());
        assert_eq!(report.exit_code(), ExitCode::RedactAuditFindings);

        let clean = tempfile::tempdir().unwrap();
        write(clean.path(), "ok.output.txt", "nothing to see here\n");
        assert_eq!(audit(clean.path()).exit_code(), ExitCode::Success);
    }

    #[test]
    fn baseline_suppresses_known_findings() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "leak.output.txt", "AKIAIOSFODNN7EXAMPLE");
        let first = audit(dir.path());
        let baseline_path = dir.path().join("baseline.json");
        std::fs::write(&baseline_path, format_json(&first).unwrap()).unwrap();

        let second = run_redact_audit(
            &default_cfg(),
            &AuditOpts {
                dir: dir.path().to_path_buf(),
                detectors: None,
                disable_entropy: false,
                baseline: Some(baseline_path),
            },
        )
        .unwrap();
        assert!(second.findings.iter().all(|f| !f.is_new));
        assert_eq!(second.exit_code(), ExitCode::Success);
    }

    #[test]
    fn detector_selection_scopes_only_the_registry() {
        // `--detectors` / `--disable-entropy` tune the high-confidence regex
        // registry + entropy heuristic only. The runtime-redactor oracle is the
        // write-time-parity guarantee and ALWAYS runs, so an AWS key is still
        // caught (as `api_key`, via the oracle) even though only the `github`
        // registry detector was selected. The reported `detectors` list still
        // reflects the selected registry subset.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.output.txt", "AKIAIOSFODNN7EXAMPLE");
        let report = run_redact_audit(
            &default_cfg(),
            &AuditOpts {
                dir: dir.path().to_path_buf(),
                detectors: Some(vec!["github".to_owned()]),
                disable_entropy: true,
                baseline: None,
            },
        )
        .unwrap();
        assert_eq!(report.detectors, vec!["github".to_owned()]);
        // The oracle still flags the AWS key — the safety net is not gated.
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.detector_id == "redactor_api_key"),
            "oracle should still catch the AWS key: {:?}",
            report.findings
        );
    }

    #[test]
    fn registry_detector_selection_still_narrows_the_named_layer() {
        // With the oracle disabled (redaction off), `--detectors` narrows the
        // registry as before: only the selected provider detector fires.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.output.txt",
            "aws=AKIAIOSFODNN7EXAMPLE gh=ghp_0123456789abcdefghijklmnopqrstuvwxyz",
        );
        let mut cfg = Config::defaults().unwrap();
        cfg.root.redaction.enabled = false; // disable the oracle
        let report = run_redact_audit(
            &cfg,
            &AuditOpts {
                dir: dir.path().to_path_buf(),
                detectors: Some(vec!["github".to_owned()]),
                disable_entropy: true,
                baseline: None,
            },
        )
        .unwrap();
        let classes: BTreeSet<&str> = report
            .findings
            .iter()
            .map(|f| f.match_class.as_str())
            .collect();
        assert!(classes.contains("github_pat"), "github missed: {classes:?}");
        assert!(
            !classes.contains("aws_access_key"),
            "aws should be excluded when only github selected + oracle off: {classes:?}"
        );
    }

    #[test]
    fn unknown_detector_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_redact_audit(
            &default_cfg(),
            &AuditOpts {
                dir: dir.path().to_path_buf(),
                detectors: Some(vec!["nope".to_owned()]),
                disable_entropy: false,
                baseline: None,
            },
        );
        assert!(err.is_err());
    }

    #[test]
    fn entropy_only_does_not_fail_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        // A long random-looking token with no structured prefix.
        write(
            dir.path(),
            "blob.output.txt",
            "value = aZ9xQ2bW8kL4mN7pR1sT3vY6cE5dH0jF2gB4nM8qP1wA",
        );
        let report = audit(dir.path());
        // Whatever entropy finds, it must not push exit code to "findings".
        assert!(report.summary.high == 0 && report.summary.medium == 0);
        assert_eq!(report.exit_code(), ExitCode::Success);
    }

    // ── PR #557 review fixes ──────────────────────────────────────────────

    #[test]
    fn audits_patch_artifacts() {
        // Submitted `.patch` files are first-class, shareable sweep artifacts;
        // a secret in one must not slip the publish gate.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "run-1.patch",
            "+AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n",
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "aws_access_key"),
            "patch artifact not audited: {:?}",
            report.findings
        );
        assert_eq!(report.exit_code(), ExitCode::RedactAuditFindings);
    }

    #[test]
    fn detects_aws_secret_access_key_by_name() {
        // The 40-char secret key has no fixed prefix; anchor on the var name.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "env.output.txt",
            "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n",
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "aws_secret_access_key"),
            "aws secret key missed: {:?}",
            report.findings
        );
        assert_eq!(report.exit_code(), ExitCode::RedactAuditFindings);
        // The 40-char secret value must never appear verbatim.
        let json = format_json(&report).unwrap();
        assert!(!json.contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
    }

    #[test]
    fn detects_openai_key_with_separators() {
        // OpenAI keys may carry `_`/`-` in the body after the prefix.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "k.output.txt",
            "OPENAI_API_KEY=sk-proj-AbC0_dEf-GhI1jKlMnOpQrStUv\n",
        );
        let report = audit(dir.path());
        let openai: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.match_class == "openai_api_key")
            .collect();
        assert!(
            !openai.is_empty(),
            "openai key missed: {:?}",
            report.findings
        );
        assert_eq!(openai[0].severity, Severity::High);
    }

    #[test]
    fn unreadable_member_is_recorded_as_scan_error() {
        // A bundle whose declared member size exceeds its actual body fails on
        // read_to_end; that must be recorded, not silently skipped.
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle.tar.gz");
        {
            let f = std::fs::File::create(&bundle).unwrap();
            let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            // Hand-craft a tar header claiming 512 bytes but write no body.
            let mut header = tar::Header::new_gnu();
            header.set_path("inner.output.txt").unwrap();
            header.set_size(512);
            header.set_mode(0o644);
            header.set_cksum();
            enc.write_all(header.as_bytes()).unwrap();
            // No member body and no terminator → read_to_end fails.
            enc.finish().unwrap();
        }
        let report = audit(dir.path());
        assert!(
            !report.scan_errors.is_empty(),
            "truncated bundle member not recorded as scan error"
        );
        assert_eq!(report.exit_code(), ExitCode::RedactAuditScanError);
    }

    #[test]
    fn detector_ids_are_deduplicated() {
        // `aws` has two underlying regex shapes but is one selectable id.
        let ids = detector_ids();
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate ids: {ids:?}");
        assert!(ids.contains(&"aws"));
    }

    // ── PR #557 second-round review fixes ─────────────────────────────────

    #[test]
    fn audits_rerun_prediction_jsonl() {
        // Rerun sweeps write `all_preds.run-<k>.jsonl`, not just the aggregate.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "all_preds.run-1.jsonl",
            r#"{"model_patch":"+tok=ghp_0123456789abcdefghijklmnopqrstuvwxyz"}"#,
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "github_pat"),
            "rerun prediction jsonl not audited: {:?}",
            report.findings
        );
    }

    #[test]
    fn detects_classic_github_token_with_underscore() {
        // Classic token bodies may contain `_`, like the runtime redactor.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "t.output.txt",
            "token=ghp_ABCdef0123_456789_abcdef0123456789ABCD\n",
        );
        let report = audit(dir.path());
        let gh: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.match_class == "github_pat")
            .collect();
        assert!(
            !gh.is_empty(),
            "github token with `_` missed: {:?}",
            report.findings
        );
        assert_eq!(gh[0].severity, Severity::High);
    }

    #[test]
    fn configured_literal_not_shadowed_by_structured_shape() {
        // A configured literal used in a `Bearer <literal>` context must still
        // be reported, even though the runtime redactor would label that span
        // `structured:bearer` and drop the literal in its overlap resolution.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "Authorization: Bearer my-internal-literal-secret-token\n",
        );
        let mut cfg = Config::defaults().unwrap();
        cfg.root.redaction.enabled = true;
        cfg.root.redaction.secret_literals = vec!["my-internal-literal-secret-token".to_owned()];
        let report = run_redact_audit(
            &cfg,
            &AuditOpts {
                dir: dir.path().to_path_buf(),
                detectors: None,
                disable_entropy: true,
                baseline: None,
            },
        )
        .unwrap();
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "configured_literal"),
            "configured literal shadowed by structured shape: {:?}",
            report.findings
        );
    }

    #[test]
    fn configured_custom_pattern_is_audited() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "id=INTERNAL-TOKEN-abcdef123456\n",
        );
        let mut cfg = Config::defaults().unwrap();
        cfg.root.redaction.enabled = true;
        cfg.root.redaction.custom_patterns = vec![r"INTERNAL-TOKEN-[A-Za-z0-9]+".to_owned()];
        let report = run_redact_audit(
            &cfg,
            &AuditOpts {
                dir: dir.path().to_path_buf(),
                detectors: None,
                disable_entropy: true,
                baseline: None,
            },
        )
        .unwrap();
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "configured_custom_pattern"),
            "configured custom pattern not audited: {:?}",
            report.findings
        );
    }

    #[test]
    fn audits_results_json_and_legacy_trajectory_json() {
        // `results.json` (sweep summary) and the legacy nested `trajectory.json`
        // are first-class artifacts that must not be skipped by name.
        let dir = tempfile::tempdir().unwrap();
        let inst = dir.path().join("inst-1");
        std::fs::create_dir_all(&inst).unwrap();
        std::fs::write(
            dir.path().join("results.json"),
            r#"{"instances":[{"error":"failed: AKIAIOSFODNN7EXAMPLE"}]}"#,
        )
        .unwrap();
        std::fs::write(
            inst.join("trajectory.json"),
            r#"{"messages":[{"content":"tok=ghp_0123456789abcdefghijklmnopqrstuvwxyz"}]}"#,
        )
        .unwrap();

        let report = audit(dir.path());
        let files: BTreeSet<&str> = report.findings.iter().map(|f| f.file.as_str()).collect();
        assert!(
            files.iter().any(|f| f.ends_with("results.json")),
            "results.json not audited: {files:?}"
        );
        assert!(
            files.iter().any(|f| f.ends_with("trajectory.json")),
            "legacy trajectory.json not audited: {files:?}"
        );
    }

    // ── PR #557 third-round review: runtime-redactor parity oracle ────────

    #[test]
    fn oracle_catches_bearer_token() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "Authorization: Bearer abcdefghijklmnop1234\n",
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "bearer_token"),
            "bearer token missed: {:?}",
            report.findings
        );
        assert_eq!(report.exit_code(), ExitCode::RedactAuditFindings);
    }

    #[test]
    fn oracle_catches_generic_sensitive_env_assignment() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "DATABASE_PASSWORD=correcthorsebatterystaple\n",
        );
        let report = audit(dir.path());
        let m: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.match_class == "sensitive_env_assignment")
            .collect();
        assert!(
            !m.is_empty(),
            "env assignment missed: {:?}",
            report.findings
        );
        assert_eq!(m[0].severity, Severity::Medium);
        // The value must not leak; the var name (benign context) may remain.
        let json = format_json(&report).unwrap();
        assert!(!json.contains("correcthorsebatterystaple"));
    }

    #[test]
    fn oracle_ignores_benign_env_assignment() {
        // Non-sensitive names must not be flagged (false-positive budget).
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "PATH=/usr/bin:/bin\nHOME=/root\n",
        );
        let report = audit(dir.path());
        assert_eq!(report.summary.high, 0, "{:?}", report.findings);
        assert_eq!(report.summary.medium, 0, "{:?}", report.findings);
    }

    #[test]
    fn oracle_catches_sensitive_json_keys() {
        // Sensitive keys are flagged regardless of value shape; the value never
        // leaks into the report.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.traj.json",
            r#"{"password":"hunter2","api_key":"abcd1234abcd1234","note":"all 42 tests passed"}"#,
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_json_value"),
            "sensitive json key missed: {:?}",
            report.findings
        );
        let json = format_json(&report).unwrap();
        assert!(!json.contains("hunter2"), "json value leaked");
    }

    #[test]
    fn oracle_catches_ambient_env_value() {
        // A sensitive env value from the audit's own environment, appearing in
        // an artifact without its variable name, is still caught.
        // SAFETY: single-threaded test; restored immediately after the run.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "leaked: super-secret-ci-token-value-123\n",
        );
        // SAFETY: set/remove a process-local var in a serial unit test.
        unsafe {
            std::env::set_var("DATABASE_PASSWORD", "super-secret-ci-token-value-123");
        }
        let report = audit(dir.path());
        unsafe {
            std::env::remove_var("DATABASE_PASSWORD");
        }
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_env_value"),
            "ambient env value missed: {:?}",
            report.findings
        );
    }

    // ── PR #557 fourth-round review ───────────────────────────────────────

    #[test]
    fn oracle_catches_short_github_token() {
        // `ghp_` + 24-char body: below the registry's 36+ pattern but masked by
        // the runtime redactor (20+), so the oracle must still flag it.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "tok=ghp_short012345678901234567\n",
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "github_pat"),
            "short github token missed: {:?}",
            report.findings
        );
        assert_eq!(report.exit_code(), ExitCode::RedactAuditFindings);
    }

    #[test]
    fn json_sensitive_value_with_escapes_is_located() {
        // A value containing escapes (`\n`, `\"`) must be found by its escaped
        // source form and never leak its decoded bytes.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.traj.json", r#"{"password":"abc\ndef\"ghi"}"#);
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_json_value"),
            "escaped json value missed: {:?}",
            report.findings
        );
        let json = format_json(&report).unwrap();
        // Neither the escaped source nor the decoded value may appear.
        assert!(!json.contains(r"abc\ndef"), "escaped value leaked");
        assert!(!json.contains("def\"ghi"), "decoded value leaked");
    }

    #[test]
    fn json_non_canonical_encodings_are_located() {
        // `h` is the JSON unicode escape for ASCII `h`; the old
        // re-serialization approach decoded it to `hunter2` and then searched
        // for the canonical `"hunter2"`, which is absent from the source bytes
        // `"hunter2"`.  Similarly `\/` is a valid but non-canonical
        // escaped slash that round-trips to an unescaped `/`, so
        // `find("abc/def")` would fail on source bytes `abc\/def`.
        let dir = tempfile::tempdir().unwrap();
        // JSON source uses h (JSON unicode escape for 'h'); serde decodes
        // this to 'h' and re-serializes canonically, so the old approach would
        // search for `"hunter2"` which is absent from the source bytes
        // `"hunter2"`.
        write(
            dir.path(),
            "a.traj.json",
            "{\"password\":\"\\u0068unter2\"}",
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_json_value"),
            "unicode-escaped value missed: {:?}",
            report.findings
        );
        let json = format_json(&report).unwrap();
        assert!(!json.contains("hunter2"), "decoded unicode value leaked");
        assert!(!json.contains("unter2"), "partial unicode value leaked");

        // JSON source uses \/ (escaped forward slash).
        let dir2 = tempfile::tempdir().unwrap();
        write(
            dir2.path(),
            "b.traj.json",
            r#"{"api_key":"abc\/def\/secret"}"#,
        );
        let report2 = audit(dir2.path());
        assert!(
            report2
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_json_value"),
            "escaped-slash value missed: {:?}",
            report2.findings
        );
        let json2 = format_json(&report2).unwrap();
        assert!(
            !json2.contains("abc/def/secret"),
            "decoded slash value leaked"
        );
    }

    #[test]
    fn audits_bundle_manifest_and_annotations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{"model":{"name":"ghp_0123456789abcdefghijklmnopqrstuvwxyz"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("annotations.json"),
            r#"{"note":"token AKIAIOSFODNN7EXAMPLE in run"}"#,
        )
        .unwrap();
        let report = audit(dir.path());
        let files: BTreeSet<&str> = report.findings.iter().map(|f| f.file.as_str()).collect();
        assert!(
            files.contains("manifest.json"),
            "manifest not audited: {files:?}"
        );
        assert!(
            files.contains("annotations.json"),
            "annotations not audited: {files:?}"
        );
    }

    #[test]
    fn corrupt_bundle_member_preserves_earlier_findings() {
        // A readable member carrying a secret, followed by a member whose body
        // is shorter than its declared size (read_to_end fails). The earlier
        // finding must survive and the bad member must be a recorded scan error.
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle.tar.gz");
        {
            let f = std::fs::File::create(&bundle).unwrap();
            let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());

            // Member 1: valid, contains a secret.
            let body = b"tok=ghp_0123456789abcdefghijklmnopqrstuvwxyz\n";
            let mut h1 = tar::Header::new_gnu();
            h1.set_path("a.output.txt").unwrap();
            h1.set_size(body.len() as u64);
            h1.set_mode(0o644);
            h1.set_cksum();
            enc.write_all(h1.as_bytes()).unwrap();
            enc.write_all(body).unwrap();
            // pad member 1 to a 512-byte block
            let pad = (512 - body.len() % 512) % 512;
            enc.write_all(&vec![0u8; pad]).unwrap();

            // Member 2: header claims 512 bytes but body is absent → read fails.
            let mut h2 = tar::Header::new_gnu();
            h2.set_path("b.output.txt").unwrap();
            h2.set_size(512);
            h2.set_mode(0o644);
            h2.set_cksum();
            enc.write_all(h2.as_bytes()).unwrap();
            enc.finish().unwrap();
        }
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "github_pat"),
            "earlier-member finding dropped: {:?}",
            report.findings
        );
        assert!(
            !report.scan_errors.is_empty(),
            "corrupt member not recorded as scan error"
        );
        // Findings present → exit code reflects findings, not just scan error.
        assert_eq!(report.exit_code(), ExitCode::RedactAuditFindings);
    }

    #[test]
    fn loads_recorded_redaction_config_from_manifest() {
        // A sweep recorded a custom short literal in its manifest's resolved
        // config. Auditing with DEFAULTS (no --config) must still catch a value
        // that has no provider/entropy shape, because the recorded config is
        // merged in.
        let dir = tempfile::tempdir().unwrap();
        let resolved = "[redaction]\nenabled = true\nsecret_literals = [\"sw33t\"]\n";
        let manifest = serde_json::json!({
            "config": { "resolved": resolved }
        });
        std::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        write(dir.path(), "log.output.txt", "the password is sw33t today");

        // Default config has no such literal.
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "configured_literal"),
            "recorded-config literal not applied: {:?}",
            report.findings
        );
        // And the literal value itself is masked in the report.
        let json = format_json(&report).unwrap();
        assert!(!json.contains("sw33t"), "recorded literal leaked");
    }

    // ── PR #557 fifth-round review ────────────────────────────────────────

    #[test]
    fn jsonl_records_are_scanned_for_sensitive_keys() {
        // A multi-record .jsonl fails a whole-file parse; per-line parsing must
        // still flag a sensitive key in any record.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "all_preds.run-1.jsonl",
            "{\"instance_id\":\"a\",\"model_patch\":\"ok\"}\n{\"instance_id\":\"b\",\"api_key\":\"short-ci-secret\"}\n",
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_json_value"),
            "jsonl sensitive key missed: {:?}",
            report.findings
        );
        let json = format_json(&report).unwrap();
        assert!(!json.contains("short-ci-secret"), "jsonl value leaked");
    }

    #[test]
    fn recorded_config_is_read_from_results_json() {
        // Ordinary sweeps embed the resolved config inside results.json (no
        // standalone manifest.json). Auditing with defaults must still apply it.
        let dir = tempfile::tempdir().unwrap();
        let resolved = "[redaction]\nenabled = true\nsecret_literals = [\"zzliteral\"]\n";
        let results = serde_json::json!({
            "manifest": { "config": { "resolved": resolved } },
            "instances": [],
        });
        std::fs::write(
            dir.path().join("results.json"),
            serde_json::to_string(&results).unwrap(),
        )
        .unwrap();
        write(dir.path(), "log.output.txt", "value is zzliteral here");

        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "configured_literal"),
            "results.json recorded literal not applied: {:?}",
            report.findings
        );
        assert!(!format_json(&report).unwrap().contains("zzliteral"));
    }

    #[test]
    fn directory_symlink_loop_terminates() {
        // A directory symlink pointing back at an ancestor must not loop; the
        // real artifact is scanned once and the loop is skipped.
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        write(
            &sub,
            "leak.output.txt",
            "tok=ghp_0123456789abcdefghijklmnopqrstuvwxyz",
        );
        // Best-effort symlink; skip the assertion on platforms/sandboxes that
        // disallow it rather than failing the suite.
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(dir.path(), sub.join("loop")).is_ok();
        #[cfg(not(unix))]
        let linked = false;

        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "github_pat"),
            "real artifact missed: {:?}",
            report.findings
        );
        if linked {
            // The loop must not have inflated the scan with repeated descents.
            assert_eq!(
                report.files_scanned, 1,
                "symlink loop rescanned files: {}",
                report.files_scanned
            );
        }
    }

    // ── PR #557 sixth-round review ────────────────────────────────────────

    #[test]
    fn already_redacted_env_assignment_is_not_a_finding() {
        // A correctly-redacted artifact carries a marker as the assignment value;
        // it must not fail the publish gate.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "DATABASE_PASSWORD=[REDACTED:env_assignment:short:abc123def456]\n",
        );
        let report = audit(dir.path());
        assert_eq!(report.summary.high, 0, "{:?}", report.findings);
        assert_eq!(report.summary.medium, 0, "{:?}", report.findings);
        assert_eq!(report.exit_code(), ExitCode::Success);
    }

    #[test]
    fn already_redacted_json_value_is_not_a_finding() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.traj.json",
            r#"{"api_key":"[REDACTED:env_key:medium:deadbeef0000]"}"#,
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.match_class != "sensitive_json_value"),
            "marker flagged as sensitive value: {:?}",
            report.findings
        );
        assert_eq!(report.exit_code(), ExitCode::Success);
    }

    #[test]
    fn real_secret_under_sensitive_json_key_still_flagged() {
        // Guard against the marker filter being over-broad.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.traj.json",
            r#"{"api_key":"realhunter2value"}"#,
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_json_value"),
            "real json secret missed: {:?}",
            report.findings
        );
    }

    // ── PR #557 eighth-round review ───────────────────────────────────────

    #[test]
    fn partially_redacted_env_assignment_is_still_flagged() {
        // A marker plus raw residue (`hunter2`) still leaks; the mere presence
        // of a marker must not let it pass the gate.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "DATABASE_PASSWORD=[REDACTED:env_assignment:short:abc123def456]hunter2\n",
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_env_assignment"),
            "partially-redacted env value missed: {:?}",
            report.findings
        );
        assert_eq!(report.exit_code(), ExitCode::RedactAuditFindings);
        let json = format_json(&report).unwrap();
        assert!(!json.contains("hunter2"), "raw residue leaked");
    }

    #[test]
    fn partially_redacted_json_value_is_still_flagged() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.traj.json",
            r#"{"api_key":"[REDACTED:env_key:medium:deadbeef0000]leftoversecret"}"#,
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_json_value"),
            "partially-redacted json value missed: {:?}",
            report.findings
        );
        let json = format_json(&report).unwrap();
        assert!(!json.contains("leftoversecret"), "raw residue leaked");
    }

    #[test]
    fn sensitive_json_object_value_propagates_to_nested_fields() {
        // A sensitive key whose value is an object: every nested value must be
        // treated as sensitive, even when the inner field name is benign —
        // mirroring the runtime redactor's recursion.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.traj.json",
            r#"{"credentials":{"value":"short-ci-secret","note":"x"}}"#,
        );
        let report = audit(dir.path());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "sensitive_json_value"),
            "nested sensitive value missed: {:?}",
            report.findings
        );
        let json = format_json(&report).unwrap();
        assert!(!json.contains("short-ci-secret"), "nested value leaked");
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlink_escaping_sweep_is_not_followed() {
        // A directory symlink pointing outside the scanned sweep must not be
        // descended into — otherwise unrelated external files get scanned.
        let outside = tempfile::tempdir().unwrap();
        write(
            outside.path(),
            "leak.output.txt",
            "tok=ghp_0123456789abcdefghijklmnopqrstuvwxyz\n",
        );
        let sweep = tempfile::tempdir().unwrap();
        write(sweep.path(), "clean.output.txt", "all 42 tests passed\n");
        // Best-effort symlink; skip on sandboxes that disallow it.
        if std::os::unix::fs::symlink(outside.path(), sweep.path().join("external")).is_ok() {
            let report = audit(sweep.path());
            assert!(
                report.findings.is_empty(),
                "followed escaping symlink and scanned external files: {:?}",
                report.findings
            );
            assert_eq!(report.exit_code(), ExitCode::Success);
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_symlink_escaping_sweep_is_not_followed() {
        // An artifact-looking file symlink pointing outside the scanned sweep
        // must not be scanned — otherwise an unrelated external file's secrets
        // fail the publish gate.
        let outside = tempfile::tempdir().unwrap();
        write(
            outside.path(),
            "secret.output.txt",
            "tok=ghp_0123456789abcdefghijklmnopqrstuvwxyz\n",
        );
        let sweep = tempfile::tempdir().unwrap();
        write(sweep.path(), "clean.output.txt", "all 42 tests passed\n");
        // Best-effort symlink; skip on sandboxes that disallow it.
        if std::os::unix::fs::symlink(
            outside.path().join("secret.output.txt"),
            sweep.path().join("linked.output.txt"),
        )
        .is_ok()
        {
            let report = audit(sweep.path());
            assert!(
                report.findings.is_empty(),
                "followed escaping file symlink: {:?}",
                report.findings
            );
            assert_eq!(report.exit_code(), ExitCode::Success);
        }
    }

    #[test]
    fn recorded_enabled_turns_on_oracle_even_without_literals() {
        // Sweep recorded `redaction.enabled = true` but no recoverable literals
        // (they are stored already-redacted). With a CLI config that defaulted
        // redaction off, the oracle must still run on the strength of the
        // recorded `enabled` flag.
        let dir = tempfile::tempdir().unwrap();
        let resolved = "[redaction]\nenabled = true\n";
        std::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_string(&serde_json::json!({
                "config": { "resolved": resolved }
            }))
            .unwrap(),
        )
        .unwrap();
        write(
            dir.path(),
            "x.output.txt",
            "Authorization: Bearer abcdefghijklmnop1234\n",
        );

        let mut cfg = Config::defaults().unwrap();
        cfg.root.redaction.enabled = false; // CLI defaulted off
        let report = run_redact_audit(
            &cfg,
            &AuditOpts {
                dir: dir.path().to_path_buf(),
                detectors: None,
                disable_entropy: false,
                baseline: None,
            },
        )
        .unwrap();
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.match_class == "bearer_token"),
            "recorded enabled=true did not re-enable the oracle: {:?}",
            report.findings
        );
    }
}
