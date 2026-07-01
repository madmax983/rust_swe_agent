//! `agent annotate` — attach a human verdict and notes to a trajectory sidecar (issue #539).
//!
//! Writes `<id>.annotation.json` next to the trajectory file without mutating the
//! trajectory. Supports reading back the annotation with `--show`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::error::{ConfigError, Error};
use crate::redaction::{Redactor, surface};

// ── public opts ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Correct,
    Incorrect,
    Partial,
    Unsure,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Correct => "correct",
            Self::Incorrect => "incorrect",
            Self::Partial => "partial",
            Self::Unsure => "unsure",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "correct" => Some(Self::Correct),
            "incorrect" => Some(Self::Incorrect),
            "partial" => Some(Self::Partial),
            "unsure" => Some(Self::Unsure),
            _ => None,
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Verdict {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Verdict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown verdict `{s}`; expected one of: correct, incorrect, partial, unsure"
            ))
        })
    }
}

#[derive(Debug, Clone)]
pub struct StepNoteInput {
    pub step: usize,
    pub note: String,
}

pub struct AnnotateOpts {
    pub trajectory_path: PathBuf,
    pub verdict: Verdict,
    pub failure_category: Option<String>,
    pub note: Option<String>,
    pub step_notes: Vec<StepNoteInput>,
    pub force: bool,
}

pub struct ShowOpts {
    pub trajectory_path: PathBuf,
    pub format: AnnotateFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotateFormat {
    Text,
    Json,
}

// ── sidecar schema ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepNote {
    pub step: usize,
    pub note: String,
}

/// Sidecar annotation file schema (`<id>.annotation.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryAnnotation {
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
    /// Instance id derived from the trajectory path (or `--instance-id` override).
    pub instance_id: String,
    /// Path of the trajectory as supplied by the operator.
    pub trajectory_path: String,
    /// Full SHA-256 hex digest of the trajectory file bytes at annotation time.
    pub trajectory_sha256: String,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub step_notes: Vec<StepNote>,
    pub annotated_at: String,
}

impl TrajectoryAnnotation {
    fn new(
        instance_id: String,
        trajectory_path: String,
        trajectory_sha256: String,
        verdict: Verdict,
        failure_category: Option<String>,
        note: Option<String>,
        step_notes: Vec<StepNote>,
    ) -> Self {
        Self {
            artifact_kind: ArtifactKind::TrajectoryAnnotation,
            schema_version: ArtifactSchemaVersion::CURRENT,
            instance_id,
            trajectory_path,
            trajectory_sha256,
            verdict,
            failure_category,
            note,
            step_notes,
            annotated_at: now_utc(),
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Derive sidecar path from trajectory path:
/// `foo/bar.traj.json` → `foo/bar.annotation.json`
/// `foo/bar.json` → `foo/bar.annotation.json`
pub fn sidecar_path(traj_path: &Path) -> PathBuf {
    let stem = traj_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let new_name = if let Some(rest) = stem.strip_suffix(".traj.json") {
        format!("{rest}.annotation.json")
    } else if let Some(rest) = stem.strip_suffix(".json") {
        format!("{rest}.annotation.json")
    } else {
        format!("{stem}.annotation.json")
    };

    traj_path.with_file_name(new_name)
}

/// Derive instance_id from trajectory path:
/// `<dir>/<id>.traj.json` → `<id>`
/// `<dir>/<id>/run-k.traj.json` → `<id>`
/// `<dir>/<id>/trajectory.json` → `<id>`
///
/// Conventional sweep layouts name the per-run file `run-k.traj.json` or
/// `trajectory.json` inside a `<id>/` directory; for those the parent directory
/// name is the instance id. Any other `<id>.traj.json` filename is a flat
/// standalone trajectory and its stem is the instance id.
pub fn instance_id_from_path(traj_path: &Path) -> String {
    let stem = traj_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // Nested sweep layout: the per-run filename carries no instance identity,
    // so the parent directory name is the instance id. The conventional names
    // are `trajectory.json` and `run-<number>.traj.json` (see
    // `load_all_trajectories_for_instance`); a flat file like
    // `run-my-instance.traj.json` does NOT match and stays flat.
    let is_nested_run_file = stem == "trajectory.json" || is_run_k_traj(stem);
    if is_nested_run_file {
        if let Some(parent) = traj_path.parent() {
            if let Some(dir_name) = parent.file_name().and_then(|n| n.to_str()) {
                if !dir_name.is_empty() && dir_name != "." {
                    return dir_name.to_owned();
                }
            }
        }
    }

    // Flat layout: `<id>.traj.json` (or any other `.json`) → stem is the id.
    if let Some(id) = stem.strip_suffix(".traj.json") {
        if !id.is_empty() {
            return id.to_owned();
        }
    }
    if let Some(id) = stem.strip_suffix(".json") {
        if !id.is_empty() {
            return id.to_owned();
        }
    }

    // Last resort: parent directory name, else the raw stem.
    if let Some(parent) = traj_path.parent() {
        if let Some(dir_name) = parent.file_name().and_then(|n| n.to_str()) {
            if !dir_name.is_empty() && dir_name != "." {
                return dir_name.to_owned();
            }
        }
    }

    stem.to_owned()
}

/// True for the conventional nested per-run filename `run-<number>.traj.json`.
fn is_run_k_traj(stem: &str) -> bool {
    stem.strip_prefix("run-")
        .and_then(|rest| rest.strip_suffix(".traj.json"))
        .is_some_and(|num| !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in &digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Current UTC timestamp as a seconds-precision RFC 3339 / ISO 8601 string,
/// matching the convention used across the harness (e.g. `src/annotation.rs`).
fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ── write path ────────────────────────────────────────────────────────────────

/// Minimal trajectory shape — only the fields needed to validate the file and
/// count messages. Avoids allocating all message content for large trajectories
/// (can be tens of MB).
#[derive(Deserialize)]
struct TrajectoryMin {
    artifact_kind: ArtifactKind,
    messages: Vec<serde::de::IgnoredAny>,
}

pub fn run_annotate(opts: &AnnotateOpts) -> Result<(), Error> {
    // 1. Read and parse the trajectory (validates it is a parseable trajectory).
    let traj_bytes = std::fs::read(&opts.trajectory_path).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "agent annotate: cannot read `{}`: {e}",
            opts.trajectory_path.display()
        )))
    })?;

    let traj: TrajectoryMin = serde_json::from_slice(&traj_bytes).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "agent annotate: `{}` is not a parseable trajectory: {e}",
            opts.trajectory_path.display()
        )))
    })?;
    if traj.artifact_kind != ArtifactKind::Trajectory {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "agent annotate: `{}` is not a trajectory artifact",
            opts.trajectory_path.display()
        ))));
    }

    // 2. Validate step-note indices.
    let step_count = traj.messages.len();
    for sn in &opts.step_notes {
        if sn.step >= step_count {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "agent annotate: --step-note index {} is out of range; \
                 trajectory has {} message(s) (valid indices: 0..{})",
                sn.step,
                step_count,
                step_count.saturating_sub(1)
            ))));
        }
    }

    // 3. Check for existing sidecar.
    let sidecar = sidecar_path(&opts.trajectory_path);
    if sidecar.exists() && !opts.force {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "agent annotate: annotation already exists at `{}`; \
             use --force to overwrite",
            sidecar.display()
        ))));
    }

    // 4. Apply redaction to free-text fields.
    let redactor = Redactor::default_enabled();
    let failure_category = opts
        .failure_category
        .as_deref()
        .map(|c| redactor.redact_text(c, surface::EXPORT).text);
    let note = opts
        .note
        .as_deref()
        .map(|n| redactor.redact_text(n, surface::EXPORT).text);
    let step_notes: Vec<StepNote> = opts
        .step_notes
        .iter()
        .map(|sn| StepNote {
            step: sn.step,
            note: redactor.redact_text(&sn.note, surface::EXPORT).text,
        })
        .collect();

    // 5. Build and write the annotation.
    let instance_id = instance_id_from_path(&opts.trajectory_path);
    let trajectory_sha256 = sha256_hex(&traj_bytes);
    let trajectory_path_str = opts.trajectory_path.display().to_string();

    let annotation = TrajectoryAnnotation::new(
        instance_id,
        trajectory_path_str,
        trajectory_sha256,
        opts.verdict,
        failure_category,
        note,
        step_notes,
    );

    let json = serde_json::to_string_pretty(&annotation)?;
    let temp_sidecar = sidecar.with_extension("tmp");
    std::fs::write(&temp_sidecar, json)?;
    std::fs::rename(&temp_sidecar, &sidecar)?;

    Ok(())
}

// ── read / --show path ────────────────────────────────────────────────────────

pub fn run_show(opts: &ShowOpts) -> Result<TrajectoryAnnotation, Error> {
    let sidecar = sidecar_path(&opts.trajectory_path);
    if !sidecar.exists() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "agent annotate: no annotation found at `{}`",
            sidecar.display()
        ))));
    }

    let raw = std::fs::read_to_string(&sidecar).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "agent annotate: cannot read annotation `{}`: {e}",
            sidecar.display()
        )))
    })?;

    // Validate artifact_kind and schema_version before deserializing the payload.
    let val: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "agent annotate: annotation at `{}` is malformed JSON: {e}",
            sidecar.display()
        )))
    })?;
    crate::artifact::classify_json_value(
        &val,
        ArtifactKind::TrajectoryAnnotation,
        sidecar.display().to_string(),
    )
    .map_err(|e| Error::Config(ConfigError::Invalid(e.to_string())))?;

    let mut ann: TrajectoryAnnotation = serde_json::from_value(val).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "agent annotate: annotation at `{}` is malformed: {e}",
            sidecar.display()
        )))
    })?;

    // Apply redaction to free-text on read/emit.
    let redactor = Redactor::default_enabled();
    if let Some(ref mut c) = ann.failure_category {
        let redacted = redactor.redact_text(c, surface::EXPORT).text;
        *c = redacted;
    }
    if let Some(ref mut n) = ann.note {
        let redacted = redactor.redact_text(n, surface::EXPORT).text;
        *n = redacted;
    }
    for sn in &mut ann.step_notes {
        sn.note = redactor.redact_text(&sn.note, surface::EXPORT).text;
    }

    Ok(ann)
}

// ── text renderers ────────────────────────────────────────────────────────────

use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};

pub fn render_write_text(traj_path: &Path, verdict: Verdict, sidecar: &Path) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "annotate: wrote {} verdict for `{}` → {}",
        verdict,
        instance_id_from_path(traj_path),
        sidecar.display()
    );
    s
}

pub fn render_show_text(ann: &TrajectoryAnnotation) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["Field", "Value"]);

    table.add_row(vec!["Instance ID".to_string(), ann.instance_id.clone()]);
    table.add_row(vec!["Verdict".to_string(), ann.verdict.to_string()]);

    if let Some(fc) = ann.failure_category.as_deref() {
        table.add_row(vec!["Failure Category".to_string(), fc.to_string()]);
    }
    if let Some(note) = ann.note.as_deref() {
        table.add_row(vec!["Note".to_string(), note.to_string()]);
    }

    for sn in &ann.step_notes {
        table.add_row(vec![format!("Step [{}]", sn.step), sn.note.clone()]);
    }

    table.add_row(vec![
        "Annotated At".to_string(),
        ann.annotated_at.clone(),
    ]);
    table.add_row(vec![
        "Trajectory SHA256".to_string(),
        ann.trajectory_sha256.clone(),
    ]);

    let mut s = String::new();
    let _ = writeln!(s, "=== annotation: {} ===", ann.instance_id);
    let _ = writeln!(s, "{table}");
    s
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn sidecar_path_strips_traj_json() {
        let p = Path::new("/runs/foo__bar.traj.json");
        assert_eq!(
            sidecar_path(p),
            PathBuf::from("/runs/foo__bar.annotation.json")
        );
    }

    #[test]
    fn sidecar_path_handles_plain_json() {
        let p = Path::new("/runs/trajectory.json");
        assert_eq!(
            sidecar_path(p),
            PathBuf::from("/runs/trajectory.annotation.json")
        );
    }

    #[test]
    fn instance_id_from_flat_path() {
        let p = Path::new("/runs/django__django-12345.traj.json");
        assert_eq!(instance_id_from_path(p), "django__django-12345");
    }

    #[test]
    fn instance_id_from_nested_path() {
        let p = Path::new("/runs/django__django-12345/run-1.traj.json");
        assert_eq!(instance_id_from_path(p), "django__django-12345");
    }

    #[test]
    fn instance_id_from_nested_trajectory_json() {
        let p = Path::new("/runs/my-instance/trajectory.json");
        assert_eq!(instance_id_from_path(p), "my-instance");
    }

    #[test]
    fn instance_id_flat_file_named_run_prefix_uses_stem() {
        // A flat standalone trajectory whose stem starts with "run-" but is not
        // the conventional `run-<number>` form must use its own stem, not the
        // parent directory.
        let p = Path::new("/runs/run-my-instance.traj.json");
        assert_eq!(instance_id_from_path(p), "run-my-instance");
    }

    #[test]
    fn instance_id_nested_run_number_uses_parent_dir() {
        // The conventional `run-<number>.traj.json` per-run file resolves to the
        // parent directory name.
        let p = Path::new("/runs/django__django-1/run-12.traj.json");
        assert_eq!(instance_id_from_path(p), "django__django-1");
    }

    #[test]
    fn verdict_parse_roundtrip() {
        for v in ["correct", "incorrect", "partial", "unsure"] {
            let parsed = Verdict::parse(v).expect("parse");
            assert_eq!(parsed.as_str(), v);
        }
    }

    #[test]
    fn verdict_parse_unknown_returns_none() {
        assert!(Verdict::parse("bad-value").is_none());
    }

    #[test]
    fn step_note_out_of_range_returns_error() {
        // We need a real trajectory for this — use a minimal inline one.
        let traj_json = serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.3",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 12},
            "info": {"task": "t"},
            "messages": [{"role": "assistant", "content": "hi"}]
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let traj_path = dir.path().join("t.traj.json");
        std::fs::write(&traj_path, serde_json::to_string(&traj_json).unwrap()).unwrap();

        let opts = AnnotateOpts {
            trajectory_path: traj_path,
            verdict: Verdict::Correct,
            failure_category: None,
            note: None,
            step_notes: vec![StepNoteInput {
                step: 5,
                note: "oob".into(),
            }],
            force: false,
        };
        let result = run_annotate(&opts);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains('5') || msg.contains("range"), "msg: {msg}");
    }

    #[test]
    fn force_false_refuses_existing_sidecar() {
        let traj_json = serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.3",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 12},
            "info": {"task": "t"},
            "messages": [{"role": "assistant", "content": "hi"}]
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let traj_path = dir.path().join("t.traj.json");
        std::fs::write(&traj_path, serde_json::to_string(&traj_json).unwrap()).unwrap();

        let opts = AnnotateOpts {
            trajectory_path: traj_path,
            verdict: Verdict::Correct,
            failure_category: None,
            note: None,
            step_notes: vec![],
            force: false,
        };
        run_annotate(&opts).expect("first write");

        let opts2 = AnnotateOpts {
            force: false,
            ..opts
        };
        let result = run_annotate(&opts2);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("force"));
    }
}
