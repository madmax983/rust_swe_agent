//! `agent artifact-check` — zero-cost structural conformance gate for artifact files.
//!
//! Issue #534. Validates one or more artifact files (or directories, recursively)
//! against the Artifact Contract defined in `docs/artifact-contract.md`. Reports
//! the resolved `artifact_kind`, `schema_version`, a conformance verdict, and the
//! specific missing or malformed required fields. No model call is made and no
//! network I/O is performed.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::artifact::ArtifactSchemaVersion;
use crate::error::Error;

/// Schema version for the `validation_report` JSON artifact emitted by this command.
const ARTIFACT_CHECK_SCHEMA_VERSION: ArtifactSchemaVersion = ArtifactSchemaVersion::new(1, 0);

// ── Source ────────────────────────────────────────────────────────────────────

/// Input source for `agent artifact-check`.
pub enum ArtifactCheckSource {
    /// One or more explicit file or directory paths. Directories are scanned
    /// recursively for `*.json` files.
    Paths(Vec<PathBuf>),
}

// ── Verdict ───────────────────────────────────────────────────────────────────

/// Conformance verdict for a single checked artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceVerdict {
    /// All required fields present; `schema_version` is within supported major.
    Valid,
    /// All required fields present but `schema_version` is an older supported
    /// minor (within the same major). Readers warn and may default missing fields.
    ValidWithWarnings,
    /// One or more required fields are missing or the header is malformed.
    Invalid,
    /// Both `artifact_kind` and `schema_version` are absent; this is a
    /// pre-versioning legacy artifact. Surfaced as a warning (not hard error)
    /// under default mode; becomes a failure under `--strict`.
    LegacyUnversioned,
    /// `schema_version.major` exceeds the harness-supported major. This is
    /// always a failure regardless of `--strict`.
    UnsupportedMajor,
}

impl ConformanceVerdict {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::ValidWithWarnings => "valid_with_warnings",
            Self::Invalid => "invalid",
            Self::LegacyUnversioned => "legacy_unversioned",
            Self::UnsupportedMajor => "unsupported_major",
        }
    }

    /// Returns true when this verdict is always a hard failure (regardless of
    /// `--strict`).
    #[must_use]
    pub fn is_hard_failure(&self) -> bool {
        matches!(self, Self::Invalid | Self::UnsupportedMajor)
    }

    /// Returns true when this verdict is only a failure under `--strict`.
    #[must_use]
    pub fn is_strict_failure(&self) -> bool {
        matches!(self, Self::LegacyUnversioned | Self::ValidWithWarnings)
    }
}

// ── Per-artifact result ───────────────────────────────────────────────────────

/// Validation result for a single artifact file.
#[derive(Debug, Clone)]
pub struct ArtifactResult {
    /// Resolved path to the file.
    pub path: PathBuf,
    /// Resolved `artifact_kind` string, or `None` for legacy unversioned artifacts.
    pub artifact_kind: Option<String>,
    /// `schema_version` formatted as `"major.minor"`, or `None` for legacy unversioned.
    pub schema_version: Option<String>,
    /// Conformance verdict for this artifact.
    pub verdict: ConformanceVerdict,
    /// Required fields that are missing or malformed (empty when not `invalid`).
    pub missing_fields: Vec<String>,
    /// Human-readable warning messages (e.g., older minor version).
    pub warnings: Vec<String>,
}

// ── Options ───────────────────────────────────────────────────────────────────

/// Options passed to [`run_artifact_check`].
pub struct ArtifactCheckOpts {
    /// Where to read artifacts from.
    pub source: ArtifactCheckSource,
    /// When true, `legacy_unversioned` and `valid_with_warnings` are treated as
    /// failures in addition to `invalid` and `unsupported_major`.
    pub strict: bool,
}

// ── Output ────────────────────────────────────────────────────────────────────

/// The output of an `artifact-check` run.
pub struct ArtifactCheckOutput {
    /// Per-artifact validation results.
    pub results: Vec<ArtifactResult>,
    /// Whether `--strict` mode was active.
    pub strict: bool,
}

impl ArtifactCheckOutput {
    /// Returns true when any result counts as a failure (given the `strict` flag).
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.results
            .iter()
            .any(|r| r.verdict.is_hard_failure() || (self.strict && r.verdict.is_strict_failure()))
    }

    /// Counts by verdict.
    #[must_use]
    pub fn verdict_counts(&self) -> VerdictCounts {
        let mut counts = VerdictCounts::default();
        for r in &self.results {
            match r.verdict {
                ConformanceVerdict::Valid => counts.valid += 1,
                ConformanceVerdict::ValidWithWarnings => counts.valid_with_warnings += 1,
                ConformanceVerdict::Invalid => counts.invalid += 1,
                ConformanceVerdict::LegacyUnversioned => counts.legacy_unversioned += 1,
                ConformanceVerdict::UnsupportedMajor => counts.unsupported_major += 1,
            }
        }
        counts
    }
}

/// Summary counts by verdict.
#[derive(Debug, Default, Serialize)]
pub struct VerdictCounts {
    pub valid: usize,
    pub valid_with_warnings: usize,
    pub invalid: usize,
    pub legacy_unversioned: usize,
    pub unsupported_major: usize,
}

// ── Core function ─────────────────────────────────────────────────────────────

/// Validate each artifact file described by `opts` against the Artifact Contract.
///
/// No model calls, no network I/O. Files in directories are discovered
/// recursively.
///
/// # Errors
/// Returns `Err` only on unrecoverable I/O errors (e.g., the specified path
/// does not exist). Individual per-file parse failures are returned as
/// `ConformanceVerdict::Invalid` rather than propagating as `Err`.
pub fn run_artifact_check(opts: &ArtifactCheckOpts) -> Result<ArtifactCheckOutput, Error> {
    let (paths, dir_errors) = collect_paths(&opts.source);
    let mut results = Vec::with_capacity(paths.len() + dir_errors.len());
    for (dir_path, msg) in dir_errors {
        results.push(ArtifactResult {
            path: dir_path,
            artifact_kind: None,
            schema_version: None,
            verdict: ConformanceVerdict::Invalid,
            missing_fields: vec![],
            warnings: vec![msg],
        });
    }
    for path in paths {
        results.push(validate_file(&path));
    }
    Ok(ArtifactCheckOutput {
        results,
        strict: opts.strict,
    })
}

// ── Path collection ───────────────────────────────────────────────────────────

fn collect_paths(source: &ArtifactCheckSource) -> (Vec<PathBuf>, Vec<(PathBuf, String)>) {
    let ArtifactCheckSource::Paths(input_paths) = source;
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for path in input_paths {
        if path.is_dir() {
            collect_json_recursive(path, &mut out, &mut errors);
        } else {
            out.push(path.clone());
        }
    }
    (out, errors)
}

fn collect_json_recursive(
    dir: &std::path::Path,
    out: &mut Vec<PathBuf>,
    errors: &mut Vec<(PathBuf, String)>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            errors.push((dir.to_owned(), format!("cannot read directory: {e}")));
            return;
        }
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        // Use file_type() (does not follow symlinks) to avoid infinite recursion
        // if the directory tree contains symlink cycles.
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            collect_json_recursive(&path, out, errors);
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

// ── File validation ───────────────────────────────────────────────────────────

fn validate_file(path: &std::path::Path) -> ArtifactResult {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return ArtifactResult {
                path: path.to_owned(),
                artifact_kind: None,
                schema_version: None,
                verdict: ConformanceVerdict::Invalid,
                missing_fields: vec![],
                warnings: vec![format!("cannot read file: {e}")],
            };
        }
    };

    let value: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return ArtifactResult {
                path: path.to_owned(),
                artifact_kind: None,
                schema_version: None,
                verdict: ConformanceVerdict::Invalid,
                missing_fields: vec![],
                warnings: vec![format!("JSON parse error: {e}")],
            };
        }
    };

    validate_value(path, &value)
}

/// Parsed artifact header extracted from a JSON value.
struct ParsedHeader {
    kind_str: String,
    version: ArtifactSchemaVersion,
}

/// Parse the artifact header fields, returning `Ok(ParsedHeader)` or an
/// early `ArtifactResult` that should be returned directly.
///
/// The `Err` variant is boxed to keep the `Result` size in check (clippy
/// `result_large_err`).
fn parse_header(
    path: &std::path::Path,
    value: &Value,
) -> Result<ParsedHeader, Box<ArtifactResult>> {
    if !value.is_object() {
        return Err(Box::new(ArtifactResult {
            path: path.to_owned(),
            artifact_kind: None,
            schema_version: None,
            verdict: ConformanceVerdict::Invalid,
            missing_fields: vec![],
            warnings: vec!["JSON value is not an object".into()],
        }));
    }

    let kind_val = value.get("artifact_kind");
    let version_val = value.get("schema_version");

    if kind_val.is_none() && version_val.is_none() {
        return Err(Box::new(ArtifactResult {
            path: path.to_owned(),
            artifact_kind: None,
            schema_version: None,
            verdict: ConformanceVerdict::LegacyUnversioned,
            missing_fields: vec![],
            warnings: vec![
                "pre-versioning legacy artifact: artifact_kind and schema_version absent".into(),
            ],
        }));
    }

    let (Some(kind_val), Some(version_val)) = (kind_val, version_val) else {
        return Err(Box::new(ArtifactResult {
            path: path.to_owned(),
            artifact_kind: None,
            schema_version: None,
            verdict: ConformanceVerdict::Invalid,
            missing_fields: vec![],
            warnings: vec![
                "artifact_kind and schema_version must both be present or both absent".into(),
            ],
        }));
    };

    let Some(kind_str) = kind_val.as_str().map(str::to_owned) else {
        return Err(Box::new(ArtifactResult {
            path: path.to_owned(),
            artifact_kind: None,
            schema_version: None,
            verdict: ConformanceVerdict::Invalid,
            missing_fields: vec![],
            warnings: vec!["artifact_kind must be a string".into()],
        }));
    };

    match serde_json::from_value(version_val.clone()) {
        Ok(version) => Ok(ParsedHeader { kind_str, version }),
        Err(e) => Err(Box::new(ArtifactResult {
            path: path.to_owned(),
            artifact_kind: Some(kind_str),
            schema_version: None,
            verdict: ConformanceVerdict::Invalid,
            missing_fields: vec![],
            warnings: vec![format!("schema_version malformed: {e}")],
        })),
    }
}

fn validate_value(path: &std::path::Path, value: &Value) -> ArtifactResult {
    let ParsedHeader { kind_str, version } = match parse_header(path, value) {
        Ok(h) => h,
        Err(early) => return *early,
    };

    let version_str = version.to_string();

    if version.major > ArtifactSchemaVersion::CURRENT.major {
        return ArtifactResult {
            path: path.to_owned(),
            artifact_kind: Some(kind_str),
            schema_version: Some(version_str),
            verdict: ConformanceVerdict::UnsupportedMajor,
            missing_fields: vec![],
            warnings: vec![format!(
                "schema_version.major {} exceeds supported major {}",
                version.major,
                ArtifactSchemaVersion::CURRENT.major
            )],
        };
    }

    let current = ArtifactSchemaVersion::CURRENT;
    let is_older_minor = version.major == current.major && version.minor < current.minor;
    let is_newer_minor = version.major == current.major && version.minor > current.minor;

    let required = required_fields_for_kind(&kind_str);
    let mut missing_fields: Vec<String> = required
        .iter()
        .filter(|f| value.get(f).is_none())
        .map(|f| (*f).to_owned())
        .collect();

    if !missing_fields.is_empty() {
        missing_fields.sort();
        return ArtifactResult {
            path: path.to_owned(),
            artifact_kind: Some(kind_str),
            schema_version: Some(version_str),
            verdict: ConformanceVerdict::Invalid,
            missing_fields,
            warnings: vec![],
        };
    }

    let mut warnings = Vec::new();
    let verdict = if is_older_minor {
        warnings.push(format!(
            "schema_version {version_str} is an older minor than current {current}; readers may default missing fields",
        ));
        ConformanceVerdict::ValidWithWarnings
    } else if is_newer_minor {
        warnings.push(format!(
            "schema_version {version_str} is a newer minor than current {current}; this harness version may not know all fields",
        ));
        ConformanceVerdict::ValidWithWarnings
    } else {
        ConformanceVerdict::Valid
    };

    ArtifactResult {
        path: path.to_owned(),
        artifact_kind: Some(kind_str),
        schema_version: Some(version_str),
        verdict,
        missing_fields: vec![],
        warnings,
    }
}

/// Returns the required payload fields (beyond the standard header) for a given
/// artifact kind string, as enumerated in `docs/artifact-contract.md`.
///
/// Unknown kinds get no additional required fields — the header check is still
/// applied. Unknown additive fields within major 1 are intentionally ignored per
/// the contract reader policy.
fn required_fields_for_kind(kind: &str) -> &'static [&'static str] {
    match kind {
        "trajectory" => &["trajectory_format", "info", "messages"],
        "sweep_results" => &[
            "total",
            "submitted",
            "skipped",
            "errored",
            "failures_by_category",
            "instances",
        ],
        "evaluation_results" => &["instances"],
        "forecast_report" => &[
            "calibration",
            "per_instance",
            "forecast",
            "resolution_rate",
            "threshold",
        ],
        "calibration_report" => &[
            "forecast_path",
            "results_path",
            "verdict",
            "comparability",
            "metrics",
            "per_instance",
        ],
        "preflight_report" => &["mode", "checks"],
        "swebench_predictions_metadata" => &[
            "predictions_file",
            "aggregate",
            "row_count",
            "swebench_evaluator_compatible",
        ],
        "bundle_manifest" => &[
            "source_sweep_dir",
            "source_manifest_hash",
            "harness_git_sha",
            "bundle_generated_at",
            "instance_scope",
            "files",
        ],
        "cache_stats_report" => &[
            "sweep",
            "generated_at",
            "cache_disabled",
            "sweep_totals",
            "instances",
        ],
        _ => &[],
    }
}

// ── Formatters ────────────────────────────────────────────────────────────────

/// Format as a human-readable table summarising counts by verdict and listing
/// each artifact with its verdict and any issues.
#[must_use]
pub fn format_text(output: &ArtifactCheckOutput) -> String {
    use std::fmt::Write as _;

    let counts = output.verdict_counts();
    let total = output.results.len();
    let mut out = String::new();

    let _ = writeln!(
        out,
        "artifact-check: {total} artifact(s) — valid: {}, valid_with_warnings: {}, invalid: {}, legacy_unversioned: {}, unsupported_major: {}",
        counts.valid,
        counts.valid_with_warnings,
        counts.invalid,
        counts.legacy_unversioned,
        counts.unsupported_major,
    );

    if total == 0 {
        return out;
    }

    out.push('\n');
    let _ = writeln!(
        out,
        "{:<60}  {:<24}  {:<12}  {:<14}  issues",
        "path", "artifact_kind", "schema_version", "verdict"
    );
    let _ = writeln!(
        out,
        "{:-<60}  {:-<24}  {:-<12}  {:-<14}  ------",
        "", "", "", ""
    );

    for r in &output.results {
        let path_str = r.path.display().to_string();
        let kind_str = r.artifact_kind.as_deref().unwrap_or("(unknown)");
        let ver_str = r.schema_version.as_deref().unwrap_or("(none)");
        let issues = if r.missing_fields.is_empty() {
            if r.warnings.is_empty() {
                String::new()
            } else {
                r.warnings.join("; ")
            }
        } else {
            format!("missing: {}", r.missing_fields.join(", "))
        };
        let _ = writeln!(
            out,
            "{:<60}  {:<24}  {:<12}  {:<14}  {}",
            path_str,
            kind_str,
            ver_str,
            r.verdict.as_str(),
            issues,
        );
    }

    out
}

/// Format as a schema-versioned JSON `validation_report` artifact.
///
/// # Errors
/// Returns a [`serde_json::Error`] if serialisation fails (should not happen
/// for well-formed inputs).
pub fn format_json(output: &ArtifactCheckOutput) -> Result<serde_json::Value, serde_json::Error> {
    let counts = output.verdict_counts();

    let results: Vec<serde_json::Value> = output
        .results
        .iter()
        .map(|r| {
            serde_json::json!({
                "path": r.path.display().to_string(),
                "artifact_kind": r.artifact_kind,
                "schema_version": r.schema_version,
                "verdict": r.verdict.as_str(),
                "missing_fields": r.missing_fields,
                "warnings": r.warnings,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "artifact_kind": "validation_report",
        "schema_version": ARTIFACT_CHECK_SCHEMA_VERSION,
        "summary": {
            "total": output.results.len(),
            "valid": counts.valid,
            "valid_with_warnings": counts.valid_with_warnings,
            "invalid": counts.invalid,
            "legacy_unversioned": counts.legacy_unversioned,
            "unsupported_major": counts.unsupported_major,
        },
        "results": results,
    }))
}
