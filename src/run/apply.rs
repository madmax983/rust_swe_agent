//! `agent apply` — safely apply a captured `.patch` artifact to a working tree.
//!
//! Safety gates (in evaluation order):
//! 1. Patch selector must be unambiguous (exit 2 on ambiguity).
//! 2. Target must be a git working tree (exit 2 before any mutation).
//! 3. Working tree must be clean unless `--allow-dirty` (exit 30).
//! 4. Patch must not contain `[REDACTED:…]` markers, and the source
//!    trajectory (when available) must not record patch-submission
//!    redaction, unless `--allow-redacted` (exit 29).
//! 5. `git apply --check` must succeed (exit 28 on rejection).
//! 6. On `--dry-run`, stop here and report what would change (exit 0).
//! 7. Apply the patch; write `apply-report.json`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};

/// The `[REDACTED:` prefix written by the harness redactor.
const REDACTION_MARKER: &str = "[REDACTED:";

// ── Public types ──────────────────────────────────────────────────────────────

/// How the source patch file is located.
pub enum PatchSelector {
    /// Direct path to a `.patch` file.
    PatchFile(PathBuf),
    /// Path to a `.traj.json` file; the sibling `.patch` is derived by
    /// replacing the `.traj.json` extension with `.patch`.
    TrajectoryFile(PathBuf),
    /// Sweep output directory + instance ID; patch is at
    /// `<sweep>/<instance>.patch`.
    SweepInstance {
        sweep: PathBuf,
        instance: String,
    },
}

/// Options for a single `agent apply` invocation.
pub struct AgentApplyOpts {
    pub selector: PatchSelector,
    /// Git working tree to apply into. Defaults to CWD at the call site.
    pub target: PathBuf,
    /// Skip the redaction-corruption gate.
    pub allow_redacted: bool,
    /// Skip the clean-tree gate.
    pub allow_dirty: bool,
    /// Run `git apply --check` but do not mutate the tree.
    pub dry_run: bool,
    /// Delegate to `git apply --3way` for fuzzy application.
    pub three_way: bool,
    /// Where to write `apply-report.json`. When `None`, the report is
    /// written next to the target directory as `apply-report.json`.
    pub report_path: Option<PathBuf>,
}

/// Schema-versioned report written on every non-error outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplyReport {
    pub schema_version: ArtifactSchemaVersion,
    pub artifact_kind: ArtifactKind,
    pub source_patch_path: String,
    pub target_git_sha: Option<String>,
    pub files_changed: Vec<String>,
    pub lines_added: i64,
    pub lines_removed: i64,
    /// `"passed"` | `"empty"` | `"failed"`
    pub check_result: String,
    pub applied: bool,
    pub dry_run: bool,
}

/// Error variants returned by [`run_agent_apply`].
#[derive(Debug)]
pub enum ApplyError {
    /// The target path exists but is not inside a git working tree (exit 2).
    NotGitTree(PathBuf),
    /// The working tree has uncommitted changes; `--allow-dirty` overrides
    /// (exit 30).
    DirtyTree(Vec<String>),
    /// The patch (or its source trajectory) contains redaction markers; use
    /// `--allow-redacted` to override (exit 29).
    RedactedRefused,
    /// `git apply --check` rejected the patch; the message holds the trimmed
    /// stderr from git (exit 28).
    CheckFailed(String),
    /// An I/O or subprocess error that is not one of the above (exit 1).
    Io(std::io::Error),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGitTree(p) => {
                write!(f, "target '{}' is not inside a git working tree", p.display())
            }
            Self::DirtyTree(paths) => {
                write!(f, "target tree has uncommitted changes: {}", paths.join(", "))
            }
            Self::RedactedRefused => write!(
                f,
                "patch contains [REDACTED:…] markers or the source trajectory \
                 recorded redaction on the patch-submission surface; use \
                 --allow-redacted to override"
            ),
            Self::CheckFailed(msg) => write!(f, "git apply --check failed: {msg}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl From<std::io::Error> for ApplyError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Apply a captured patch to a working tree with the configured safety gates.
///
/// Returns the populated [`ApplyReport`] on success (including dry-run and
/// empty-patch cases). On any gate failure the tree is left byte-for-byte
/// unchanged.
pub fn run_agent_apply(opts: AgentApplyOpts) -> Result<ApplyReport, ApplyError> {
    // ── 1. Resolve patch path and optionally load the sibling trajectory ──────
    let (patch_path, trajectory_redacted) = resolve_patch_and_redaction(&opts.selector)?;

    // ── 2. Verify target is a git working tree ────────────────────────────────
    if !is_git_tree(&opts.target) {
        return Err(ApplyError::NotGitTree(opts.target.clone()));
    }

    // ── 3. Dirty-tree gate ────────────────────────────────────────────────────
    if !opts.allow_dirty {
        let dirty = dirty_paths(&opts.target)?;
        if !dirty.is_empty() {
            return Err(ApplyError::DirtyTree(dirty));
        }
    }

    // ── 4. Read patch content ─────────────────────────────────────────────────
    let patch_text = std::fs::read_to_string(&patch_path)?;

    // ── 5. Redaction gate ─────────────────────────────────────────────────────
    if !opts.allow_redacted {
        let patch_has_markers = patch_text.contains(REDACTION_MARKER);
        if patch_has_markers || trajectory_redacted {
            return Err(ApplyError::RedactedRefused);
        }
    }

    // ── 6. Empty-patch fast path ──────────────────────────────────────────────
    if patch_text.trim().is_empty() {
        let target_sha = git_head_sha(&opts.target);
        let report = ApplyReport {
            schema_version: ArtifactSchemaVersion::CURRENT,
            artifact_kind: ArtifactKind::ApplyReport,
            source_patch_path: patch_path.to_string_lossy().into_owned(),
            target_git_sha: target_sha,
            files_changed: vec![],
            lines_added: 0,
            lines_removed: 0,
            check_result: "empty".into(),
            applied: false,
            dry_run: opts.dry_run,
        };
        if let Some(rp) = &opts.report_path {
            write_report(rp, &report)?;
        }
        return Ok(report);
    }

    // ── 7. git apply --check ──────────────────────────────────────────────────
    let check_result = git_apply_check(&opts.target, &patch_path);
    match check_result {
        Err(msg) => return Err(ApplyError::CheckFailed(msg)),
        Ok(()) => {}
    }

    // ── 8. Collect diff stats from patch text ─────────────────────────────────
    let (files_changed, lines_added, lines_removed) = parse_diff_stats(&patch_text);
    let target_sha = git_head_sha(&opts.target);

    // ── 9. Dry-run: report without mutating ──────────────────────────────────
    if opts.dry_run {
        let report = ApplyReport {
            schema_version: ArtifactSchemaVersion::CURRENT,
            artifact_kind: ArtifactKind::ApplyReport,
            source_patch_path: patch_path.to_string_lossy().into_owned(),
            target_git_sha: target_sha,
            files_changed,
            lines_added,
            lines_removed,
            check_result: "passed".into(),
            applied: false,
            dry_run: true,
        };
        if let Some(rp) = &opts.report_path {
            write_report(rp, &report)?;
        }
        return Ok(report);
    }

    // ── 10. Apply the patch ───────────────────────────────────────────────────
    let mut cmd = Command::new("git");
    cmd.arg("apply");
    if opts.three_way {
        cmd.arg("--3way");
    }
    cmd.arg(&patch_path).current_dir(&opts.target);
    let output = cmd.output()?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(ApplyError::CheckFailed(msg));
    }

    // ── 11. Write report ──────────────────────────────────────────────────────
    let report = ApplyReport {
        schema_version: ArtifactSchemaVersion::CURRENT,
        artifact_kind: ArtifactKind::ApplyReport,
        source_patch_path: patch_path.to_string_lossy().into_owned(),
        target_git_sha: target_sha,
        files_changed,
        lines_added,
        lines_removed,
        check_result: "passed".into(),
        applied: true,
        dry_run: false,
    };

    let report_dest = opts.report_path.unwrap_or_else(|| {
        opts.target.join("apply-report.json")
    });
    write_report(&report_dest, &report)?;

    Ok(report)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve the patch `PathBuf` from the selector and check whether the source
/// trajectory recorded patch-submission redaction.
///
/// Returns `(patch_path, trajectory_recorded_redaction)`.
fn resolve_patch_and_redaction(
    selector: &PatchSelector,
) -> Result<(PathBuf, bool), ApplyError> {
    match selector {
        PatchSelector::PatchFile(p) => Ok((p.clone(), false)),
        PatchSelector::TrajectoryFile(traj_path) => {
            // Derive sibling .patch path
            let patch_path = sibling_patch_of_trajectory(traj_path);
            let redacted = trajectory_has_patch_submission_redaction(traj_path);
            Ok((patch_path, redacted))
        }
        PatchSelector::SweepInstance { sweep, instance } => {
            let patch_path = sweep.join(format!("{instance}.patch"));
            // Try to load the sibling trajectory for redaction info
            let traj_path = sweep.join(format!("{instance}.traj.json"));
            let redacted = trajectory_has_patch_submission_redaction(&traj_path);
            Ok((patch_path, redacted))
        }
    }
}

/// Derive the `.patch` sibling of a `.traj.json` trajectory file.
///
/// `task.traj.json` → `task.patch`
/// `task.json`      → `task.patch`
/// `task`           → `task.patch`
fn sibling_patch_of_trajectory(traj_path: &Path) -> PathBuf {
    let stem = traj_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();

    // Strip known double-extension ".traj.json" first
    let base = if let Some(s) = stem.strip_suffix(".traj.json") {
        s.to_owned()
    } else if let Some(s) = stem.strip_suffix(".json") {
        s.to_owned()
    } else {
        // No recognised extension; keep as-is
        stem.into_owned()
    };

    traj_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{base}.patch"))
}

/// Return `true` if the trajectory file at `traj_path` records at least one
/// redaction event on the `patch_submission` surface.
fn trajectory_has_patch_submission_redaction(traj_path: &Path) -> bool {
    let text = match std::fs::read_to_string(traj_path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // Check info.redaction.counts[*].surface == "patch_submission"
    if let Some(counts) = value
        .pointer("/info/redaction/counts")
        .and_then(|v| v.as_array())
    {
        for entry in counts {
            if entry
                .get("surface")
                .and_then(|s| s.as_str())
                == Some("patch_submission")
            {
                return true;
            }
        }
    }
    // Also catch the legacy secret_leak_detected marker in info.other
    if value
        .pointer("/info/secret_leak_detected/surface")
        .and_then(|s| s.as_str())
        == Some("patch_submission")
    {
        return true;
    }
    false
}

/// Return `true` if `dir` is inside a git working tree.
fn is_git_tree(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Return the list of paths that make the working tree "dirty":
/// modified tracked files, staged changes, and untracked files.
fn dirty_paths(dir: &Path) -> Result<Vec<String>, ApplyError> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<String> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            // Porcelain format: "XY filename"
            l.get(3..).unwrap_or(l).trim().to_owned()
        })
        .collect();
    Ok(paths)
}

/// Run `git apply --check` and return `Ok(())` if the patch can be applied,
/// or `Err(rejected_hunks_message)` if it cannot.
fn git_apply_check(dir: &Path, patch_path: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .args(["apply", "--check"])
        .arg(patch_path)
        .current_dir(dir)
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let msg = if stdout.is_empty() { stderr } else { format!("{stderr}\n{stdout}") };
        Err(msg)
    }
}

/// Get the current HEAD SHA of the git repo at `dir`.
fn git_head_sha(dir: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Parse a unified diff to extract `(files_changed, lines_added, lines_removed)`.
fn parse_diff_stats(patch_text: &str) -> (Vec<String>, i64, i64) {
    let mut files: Vec<String> = Vec::new();
    let mut added: i64 = 0;
    let mut removed: i64 = 0;
    let mut in_hunk = false;

    for line in patch_text.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
            // Extract filename from "diff --git a/foo b/foo"
            if let Some(b_part) = line.strip_prefix("diff --git ").and_then(|s| {
                // Find " b/" to get the new filename
                s.find(" b/").map(|i| &s[i + 3..])
            }) {
                let name = b_part.trim().to_owned();
                if !files.contains(&name) {
                    files.push(name);
                }
            }
        } else if line.starts_with("+++ ") {
            in_hunk = false;
        } else if line.starts_with("--- ") {
            // nothing
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        } else if in_hunk {
            if line.starts_with('+') && !line.starts_with("+++") {
                added += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                removed += 1;
            }
        }
    }

    (files, added, removed)
}

/// Serialize and write the report JSON to `path`.
fn write_report(path: &Path, report: &ApplyReport) -> Result<(), ApplyError> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| ApplyError::Io(std::io::Error::other(e.to_string())))?;
    std::fs::write(path, json)?;
    Ok(())
}

// ── CLI message helpers (public so cli/mod.rs can use them) ───────────────────

/// Human-readable one-liner for `run_agent_apply` errors, for printing to
/// stderr from the CLI layer.
pub fn error_message(e: &ApplyError) -> String {
    e.to_string()
}

/// Short outcome label written to stderr on non-zero exits.
pub fn error_outcome_class(e: &ApplyError) -> &'static str {
    match e {
        ApplyError::NotGitTree(_) => crate::exit_code::ExitCode::UsageError.outcome_class(),
        ApplyError::DirtyTree(_) => {
            crate::exit_code::ExitCode::ApplyDirtyTreeRefused.outcome_class()
        }
        ApplyError::RedactedRefused => {
            crate::exit_code::ExitCode::ApplyRedactedRefused.outcome_class()
        }
        ApplyError::CheckFailed(_) => {
            crate::exit_code::ExitCode::ApplyCheckFailed.outcome_class()
        }
        ApplyError::Io(_) => crate::exit_code::ExitCode::InternalError.outcome_class(),
    }
}

/// Map an [`ApplyError`] to its exit code.
pub fn exit_code_for(e: &ApplyError) -> crate::exit_code::ExitCode {
    match e {
        ApplyError::NotGitTree(_) => crate::exit_code::ExitCode::UsageError,
        ApplyError::DirtyTree(_) => crate::exit_code::ExitCode::ApplyDirtyTreeRefused,
        ApplyError::RedactedRefused => crate::exit_code::ExitCode::ApplyRedactedRefused,
        ApplyError::CheckFailed(_) => crate::exit_code::ExitCode::ApplyCheckFailed,
        ApplyError::Io(_) => crate::exit_code::ExitCode::InternalError,
    }
}
