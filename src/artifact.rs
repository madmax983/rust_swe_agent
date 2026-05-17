//! Version metadata for user-facing run artifacts.
//!
//! Artifact readers validate the small typed header before deserializing the
//! larger payload. That lets old artifacts load deliberately and makes future
//! breaking schemas fail before metric code starts doing math on the wrong
//! shape. Grim, but cheaper than haunted analytics.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Current schema version for all artifact families introduced by this
/// contract. Major bumps are breaking; minor bumps must remain additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ArtifactSchemaVersion {
    /// The major version component (incremented for breaking changes).
    pub major: u16,
    /// The minor version component (incremented for backwards-compatible additions).
    pub minor: u16,
}

impl ArtifactSchemaVersion {
    /// The current schema version used by this build of `rust_swe_agent`.
    pub const CURRENT: Self = Self { major: 1, minor: 7 };
    /// A fallback version used for legacy trajectory files lacking a version stamp.
    pub const LEGACY_PRE_VERSIONING: Self = Self { major: 0, minor: 0 };

    /// Creates a new `ArtifactSchemaVersion` from major and minor components.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

impl Default for ArtifactSchemaVersion {
    fn default() -> Self {
        Self::CURRENT
    }
}

impl fmt::Display for ArtifactSchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Closed set of durable artifact families covered by the compatibility
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A step-by-step record of an agent's run on a single task.
    Trajectory,
    /// The aggregated results of a bulk sweep over multiple tasks.
    SweepResults,
    /// The output of an evaluation phase comparing predictions to true labels.
    EvaluationResults,
    /// A predictive report estimating costs and durations for a potential sweep.
    ForecastReport,
    /// Results from calibrating environment overhead limits.
    CalibrationReport,
    /// The output of a preflight environment integrity check.
    PreflightReport,
    /// Metadata specifically associated with SWE-bench formatting predictions.
    SwebenchPredictionsMetadata,
    /// A manifest describing the contents of a compiled bundle.
    BundleManifest,
    /// A report generated when a sweep is prematurely halted (e.g., due to errors).
    SweepHaltReport,
    /// Used internally when generating visual rendering outputs without saving state.
    RenderOnly,
}

impl ArtifactKind {
    /// Returns a standardized string label for the artifact kind.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trajectory => "trajectory",
            Self::SweepResults => "sweep_results",
            Self::EvaluationResults => "evaluation_results",
            Self::ForecastReport => "forecast_report",
            Self::CalibrationReport => "calibration_report",
            Self::PreflightReport => "preflight_report",
            Self::SwebenchPredictionsMetadata => "swebench_predictions_metadata",
            Self::BundleManifest => "bundle_manifest",
            Self::SweepHaltReport => "sweep_halt_report",
            Self::RenderOnly => "render_only",
        }
    }
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Standard top-level artifact metadata fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactHeader {
    /// The kind of artifact this header describes.
    pub artifact_kind: ArtifactKind,
    /// The schema version governing the format of this artifact.
    pub schema_version: ArtifactSchemaVersion,
}

impl ArtifactHeader {
    /// Creates a new `ArtifactHeader` stamped with the provided kind and the current version.
    #[must_use]
    pub const fn current(kind: ArtifactKind) -> Self {
        Self {
            artifact_kind: kind,
            schema_version: ArtifactSchemaVersion::CURRENT,
        }
    }
}

/// Compatibility class for a detected artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityClass {
    /// The artifact perfectly matches the current schema version.
    SupportedCurrent,
    /// The artifact is from an older schema version but can be safely processed.
    SupportedLegacy,
}

impl CompatibilityClass {
    /// Returns a standardized string label for the compatibility class.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SupportedCurrent => "supported-current",
            Self::SupportedLegacy => "supported-legacy",
        }
    }
}

/// Result of classifying a specific artifact payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactCompatibility {
    /// The identified kind of the artifact.
    pub kind: ArtifactKind,
    /// The schema version of the artifact, if explicitly stated.
    pub version: Option<ArtifactSchemaVersion>,
    /// The resulting compatibility classification.
    pub class: CompatibilityClass,
    /// Any warnings generated during classification (e.g., unrecognized fields).
    pub warnings: Vec<String>,
}

impl ArtifactCompatibility {
    /// Returns a combined string representing the artifact kind and its version,
    /// e.g. "trajectory@1.7".
    #[must_use]
    pub fn identity_label(&self) -> String {
        let version = self.version.map_or_else(
            || "legacy-pre-versioning".to_owned(),
            |version| version.to_string(),
        );
        format!("{}@{}", self.kind, version)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
/// Errors resulting from incompatible or malformed artifact headers.
pub enum ArtifactSchemaError {
    /// The artifact kind found in the file does not match the expected kind.
    #[error("{path}: artifact kind mismatch: expected {expected}, found {found}")]
    KindMismatch {
        /// The path to the failing file.
        path: String,
        /// The kind we were trying to load.
        expected: ArtifactKind,
        /// The kind actually found in the file.
        found: ArtifactKind,
    },
    /// The artifact belongs to a major schema version newer than what this binary supports.
    #[error(
        "{path}: unsupported future artifact schema for {kind}: version {version}; this binary supports major {supported_major}. Re-run with a newer rust-swe-agent."
    )]
    UnsupportedFuture {
        /// The path to the failing file.
        path: String,
        /// The artifact kind.
        kind: ArtifactKind,
        /// The unsupported future version found in the file.
        version: ArtifactSchemaVersion,
        /// The highest major version supported by this binary.
        supported_major: u16,
    },
    /// The artifact header exists but is malformed or missing required fields.
    #[error("{path}: malformed artifact schema header: {message}")]
    MalformedHeader {
        /// The path to the failing file.
        path: String,
        /// A specific description of what was malformed.
        message: String,
    },
}

/// Classify a decoded JSON artifact against the expected artifact family.
///
/// Missing `artifact_kind` and `schema_version` means a pre-versioning legacy
/// artifact. Unknown additive fields in a supported major are intentionally
/// ignored by the classifier and by the payload deserializers.
pub fn classify_json_value(
    value: &Value,
    expected: ArtifactKind,
    path: impl Into<String>,
) -> Result<ArtifactCompatibility, ArtifactSchemaError> {
    let path = path.into();
    let kind_value = value.get("artifact_kind");
    let version_value = value.get("schema_version");

    if kind_value.is_none() && version_value.is_none() {
        return Ok(ArtifactCompatibility {
            kind: expected,
            version: None,
            class: CompatibilityClass::SupportedLegacy,
            warnings: vec![format!(
                "{path}: supported-legacy artifact: detected pre-versioning legacy {expected}; defaulting artifact_kind={expected} schema_version={}",
                ArtifactSchemaVersion::LEGACY_PRE_VERSIONING
            )],
        });
    }

    let (Some(kind_value), Some(version_value)) = (kind_value, version_value) else {
        return Err(ArtifactSchemaError::MalformedHeader {
            path,
            message: "artifact_kind and schema_version must be present together".into(),
        });
    };

    let found_kind: ArtifactKind = serde_json::from_value(kind_value.clone()).map_err(|err| {
        ArtifactSchemaError::MalformedHeader {
            path: path.clone(),
            message: format!("artifact_kind: {err}"),
        }
    })?;
    if found_kind != expected {
        return Err(ArtifactSchemaError::KindMismatch {
            path,
            expected,
            found: found_kind,
        });
    }

    let version: ArtifactSchemaVersion =
        serde_json::from_value(version_value.clone()).map_err(|err| {
            ArtifactSchemaError::MalformedHeader {
                path: path.clone(),
                message: format!("schema_version: {err}"),
            }
        })?;
    if version.major > ArtifactSchemaVersion::CURRENT.major {
        return Err(ArtifactSchemaError::UnsupportedFuture {
            path,
            kind: found_kind,
            version,
            supported_major: ArtifactSchemaVersion::CURRENT.major,
        });
    }

    let mut warnings = Vec::new();
    let class = if version.major == ArtifactSchemaVersion::CURRENT.major
        && version.minor == ArtifactSchemaVersion::CURRENT.minor
    {
        CompatibilityClass::SupportedCurrent
    } else if version.major == ArtifactSchemaVersion::CURRENT.major {
        if version.minor > ArtifactSchemaVersion::CURRENT.minor {
            warnings.push(format!(
                "{path}: supported-current artifact: {found_kind} schema_version {version} has a newer additive minor than this binary's {}; ignoring unknown additive fields",
                ArtifactSchemaVersion::CURRENT
            ));
            CompatibilityClass::SupportedCurrent
        } else {
            warnings.push(format!(
                "{path}: supported-legacy artifact: {found_kind} schema_version {version}; this binary defaults missing fields and ignores removed fields as documented"
            ));
            CompatibilityClass::SupportedLegacy
        }
    } else {
        warnings.push(format!(
            "{path}: supported-legacy artifact: {found_kind} schema_version {version}; this binary defaults missing fields and ignores removed fields as documented"
        ));
        CompatibilityClass::SupportedLegacy
    };

    Ok(ArtifactCompatibility {
        kind: found_kind,
        version: Some(version),
        class,
        warnings,
    })
}

/// Serialize a payload with the standard artifact header flattened into the
/// top-level JSON object.
pub fn to_string_pretty<T>(kind: ArtifactKind, payload: &T) -> Result<String, serde_json::Error>
where
    T: Serialize,
{
    serde_json::to_string_pretty(&VersionedArtifact {
        header: ArtifactHeader::current(kind),
        payload,
    })
}

/// Serialize a payload with the standard artifact header flattened into the
/// top-level JSON object, streaming directly to a writer.
pub fn to_writer_pretty<W, T>(
    writer: W,
    kind: ArtifactKind,
    payload: &T,
) -> Result<(), serde_json::Error>
where
    W: std::io::Write,
    T: Serialize,
{
    serde_json::to_writer_pretty(
        writer,
        &VersionedArtifact {
            header: ArtifactHeader::current(kind),
            payload,
        },
    )
}

#[derive(Serialize)]
struct VersionedArtifact<'a, T>
where
    T: Serialize,
{
    #[serde(flatten)]
    header: ArtifactHeader,
    #[serde(flatten)]
    payload: &'a T,
}
