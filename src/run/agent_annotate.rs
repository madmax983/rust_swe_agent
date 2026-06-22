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
use crate::trajectory::Trajectory;

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
pub fn instance_id_from_path(traj_path: &Path) -> String {
    let stem = traj_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    if let Some(id) = stem.strip_suffix(".traj.json") {
        // root layout: <id>.traj.json
        if id != "trajectory" && !id.starts_with("run-") {
            return id.to_owned();
        }
    }

    // nested layout: <dir>/<id>/run-k.traj.json or <id>/trajectory.json
    if let Some(parent) = traj_path.parent() {
        if let Some(dir_name) = parent.file_name().and_then(|n| n.to_str()) {
            if !dir_name.is_empty() && dir_name != "." {
                return dir_name.to_owned();
            }
        }
    }

    stem.to_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in &digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

fn now_utc() -> String {
    // Use file mtime or fallback to a fixed-format timestamp.
    // We don't depend on chrono; use std::time.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as ISO 8601 UTC: YYYY-MM-DDTHH:MM:SSZ
    let s = secs;
    let sec = s % 60;
    let min = (s / 60) % 60;
    let hour = (s / 3600) % 24;
    let days = s / 86400;
    // Days since epoch → date (Gregorian calendar)
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Simple Gregorian algorithm (valid for years >= 1970)
    let mut year = 1970u64;
    loop {
        let leap = is_leap(year);
        let days_in_year = if leap { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut month = 1u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

// ── write path ────────────────────────────────────────────────────────────────

pub fn run_annotate(opts: &AnnotateOpts) -> Result<(), Error> {
    // 1. Read and parse the trajectory (validates it is a parseable trajectory).
    let traj_bytes = std::fs::read(&opts.trajectory_path).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "agent annotate: cannot read `{}`: {e}",
            opts.trajectory_path.display()
        )))
    })?;
    let traj: Trajectory = serde_json::from_slice(&traj_bytes).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "agent annotate: `{}` is not a parseable trajectory: {e}",
            opts.trajectory_path.display()
        )))
    })?;

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
        opts.failure_category.clone(),
        note,
        step_notes,
    );

    let json = serde_json::to_string_pretty(&annotation)?;
    std::fs::write(&sidecar, json)?;

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

    let mut ann: TrajectoryAnnotation = serde_json::from_str(&raw).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "agent annotate: annotation at `{}` is malformed: {e}",
            sidecar.display()
        )))
    })?;

    // Apply redaction to free-text on read/emit.
    let redactor = Redactor::default_enabled();
    if let Some(n) = ann.note.take() {
        ann.note = Some(redactor.redact_text(&n, surface::EXPORT).text);
    }
    for sn in &mut ann.step_notes {
        sn.note = redactor.redact_text(&sn.note, surface::EXPORT).text;
    }

    Ok(ann)
}

// ── text renderers ────────────────────────────────────────────────────────────

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
    let mut s = String::new();
    let _ = writeln!(s, "=== annotation: {} ===", ann.instance_id);
    let _ = writeln!(s, "  verdict          : {}", ann.verdict);
    if let Some(fc) = ann.failure_category.as_deref() {
        let _ = writeln!(s, "  failure_category : {fc}");
    }
    if let Some(note) = ann.note.as_deref() {
        let _ = writeln!(s, "  note             : {note}");
    }
    for sn in &ann.step_notes {
        let _ = writeln!(s, "  step[{}]          : {}", sn.step, sn.note);
    }
    let _ = writeln!(s, "  annotated_at     : {}", ann.annotated_at);
    let _ = writeln!(s, "  trajectory_sha256: {}", ann.trajectory_sha256);
    s
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::unwrap_err_used)]
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
            step_notes: vec![StepNoteInput { step: 5, note: "oob".into() }],
            force: false,
        };
        let result = run_annotate(&opts);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("5") || msg.contains("range"), "msg: {msg}");
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
            trajectory_path: traj_path.clone(),
            verdict: Verdict::Correct,
            failure_category: None,
            note: None,
            step_notes: vec![],
            force: false,
        };
        run_annotate(&opts).expect("first write");

        let opts2 = AnnotateOpts { force: false, ..opts };
        let result = run_annotate(&opts2);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("force"));
    }
}
