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
    pub major: u16,
    pub minor: u16,
}

impl ArtifactSchemaVersion {
    pub const CURRENT: Self = Self {
        major: 1,
        minor: 10,
    };
    pub const LEGACY_PRE_VERSIONING: Self = Self { major: 0, minor: 0 };

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
    Trajectory,
    SweepResults,
    EvaluationResults,
    ForecastReport,
    CalibrationReport,
    PreflightReport,
    SwebenchPredictionsMetadata,
    BundleManifest,
    SweepHaltReport,
    RenderOnly,
    CacheStatsReport,
    LadderReport,
    SkillsPreview,
    AuditReport,
}

impl ArtifactKind {
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
            Self::CacheStatsReport => "cache_stats_report",
            Self::LadderReport => "ladder_report",
            Self::SkillsPreview => "skills_preview",
            Self::AuditReport => "audit_report",
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
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
}

impl ArtifactHeader {
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
    SupportedCurrent,
    SupportedLegacy,
}

impl CompatibilityClass {
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
    pub kind: ArtifactKind,
    pub version: Option<ArtifactSchemaVersion>,
    pub class: CompatibilityClass,
    pub warnings: Vec<String>,
}

impl ArtifactCompatibility {
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
pub enum ArtifactSchemaError {
    #[error("{path}: artifact kind mismatch: expected {expected}, found {found}")]
    KindMismatch {
        path: String,
        expected: ArtifactKind,
        found: ArtifactKind,
    },
    #[error(
        "{path}: unsupported future artifact schema for {kind}: version {version}; this binary supports major {supported_major}. Re-run with a newer max."
    )]
    UnsupportedFuture {
        path: String,
        kind: ArtifactKind,
        version: ArtifactSchemaVersion,
        supported_major: u16,
    },
    #[error("{path}: malformed artifact schema header: {message}")]
    MalformedHeader { path: String, message: String },
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
