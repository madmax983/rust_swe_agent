//! `agent apply` — safely apply a captured `.patch` artifact to a working tree.
//!
//! Safety gates (in evaluation order):
//! 1. Patch selector must be unambiguous (exit 2 on ambiguity).
//! 2. Target must be a git working tree (exit 2 before any mutation).
//! 3. Working tree must be clean unless `--allow-dirty` (exit 30).
//!    The selected patch file and trajectory file are excluded from the dirty check.
//! 4. Patch must not contain `[REDACTED:…]` markers, and the source
//!    trajectory (when available) must not record patch-submission
//!    redaction, unless `--allow-redacted` (exit 29).
//! 5. `git apply --check [--3way]` must succeed (exit 29 on rejection).
//! 6. On `--dry-run`, stop here and report what would change (exit 0).
//! 7. Apply the patch; write `apply-report.json`.

use std::ffi::OsString;
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
    /// Sweep output directory + instance ID. Tries the canonical nested
    /// layout `<sweep>/<instance>/run-1.patch` first, then falls back to
    /// the legacy flat path `<sweep>/<instance>.patch`.
    SweepInstance { sweep: PathBuf, instance: String },
}

/// Options for a single `agent apply` invocation.
#[allow(clippy::struct_excessive_bools)]
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
    /// written next to (in the parent of) the target directory.
    pub report_path: Option<PathBuf>,
}

/// Schema-versioned report written on every non-error outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
                write!(
                    f,
                    "target '{}' is not inside a git working tree",
                    p.display()
                )
            }
            Self::DirtyTree(paths) => {
                write!(
                    f,
                    "target tree has uncommitted changes: {}",
                    paths.join(", ")
                )
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
#[allow(clippy::too_many_lines)]
pub fn run_agent_apply(opts: AgentApplyOpts) -> Result<ApplyReport, ApplyError> {
    // ── 1. Resolve patch path and optionally check trajectory redaction ────────
    let (patch_path, traj_path_opt, trajectory_redacted) =
        resolve_patch_and_redaction(&opts.selector);

    // Canonicalize the patch path so that relative paths are anchored to the
    // caller's CWD before any `current_dir()` changes in git subprocess calls.
    let patch_path = std::fs::canonicalize(&patch_path).unwrap_or(patch_path);

    // ── 2. Verify target is a git working tree ────────────────────────────────
    if !is_git_tree(&opts.target) {
        return Err(ApplyError::NotGitTree(opts.target));
    }

    // ── 3. Resolve the git worktree root ─────────────────────────────────────
    // All git apply commands must run from the worktree root. Running from a
    // subdirectory causes git apply to silently skip patches for paths outside
    // that subdirectory (it prints "Skipped patch" and exits 0).
    let git_root = git_toplevel(&opts.target).unwrap_or_else(|| opts.target.clone());

    // ── 4. Compute default report destination ────────────────────────────────
    // Anchor on the worktree root, not the target directory: if --target is a
    // subdirectory the target's parent() is inside the repo, which would write
    // the report (and create a directory) inside the checkout on dry-run.
    let report_dest = opts.report_path.clone().unwrap_or_else(|| {
        git_root
            .parent()
            .unwrap_or(&git_root)
            .join("apply-report.json")
    });

    // ── 5. Dirty-tree gate ────────────────────────────────────────────────────
    // Exclude the patch file AND the trajectory file (when using the
    // trajectory or sweep selectors) so that an untracked artifact bundle
    // sitting inside the target repo doesn't trip the gate.
    if !opts.allow_dirty {
        let traj_canon = traj_path_opt
            .as_deref()
            .map(|tp| std::fs::canonicalize(tp).unwrap_or_else(|_| tp.to_owned()));

        let mut exclude: Vec<&Path> = vec![patch_path.as_path()];
        if let Some(ref tc) = traj_canon {
            exclude.push(tc.as_path());
        }

        let dirty = dirty_paths_excluding(&git_root, &opts.target, &exclude)?;
        if !dirty.is_empty() {
            return Err(ApplyError::DirtyTree(dirty));
        }
    }

    // ── 6. Read patch content ─────────────────────────────────────────────────
    // Read as raw bytes so that patches touching files with non-UTF-8 content
    // don't fail with InvalidData before the redaction or apply gates run.
    let patch_bytes = std::fs::read(&patch_path)?;
    let patch_text = String::from_utf8_lossy(&patch_bytes);

    // ── 7. Redaction gate ─────────────────────────────────────────────────────
    if !opts.allow_redacted {
        let patch_has_markers = patch_text.contains(REDACTION_MARKER);
        if patch_has_markers || trajectory_redacted {
            return Err(ApplyError::RedactedRefused);
        }
    }

    // ── 8. Empty-patch fast path ──────────────────────────────────────────────
    if patch_text.trim().is_empty() {
        let target_sha = git_head_sha(&git_root);
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
        write_report(&report_dest, &report)?;
        return Ok(report);
    }

    // ── 9. git apply --check [--3way] ─────────────────────────────────────────
    // Pass --3way to the preflight check when requested so that patches that
    // only succeed via three-way merge are not falsely rejected here.
    if let Err(msg) = git_apply_check(&git_root, &patch_path, opts.three_way) {
        return Err(ApplyError::CheckFailed(msg));
    }

    // ── 10. Collect diff stats from patch text ────────────────────────────────
    // files_os: raw OsString paths used for git cleanup (preserves non-UTF-8).
    // files_changed: lossy-string version written to the JSON report.
    let (files_os, lines_added, lines_removed) = parse_diff_stats(&patch_text);
    let files_changed: Vec<String> = files_os
        .iter()
        .map(|f| f.to_string_lossy().into_owned())
        .collect();
    let target_sha = git_head_sha(&git_root);

    // ── 10.5. Pre-apply overlap check ───────────────────────────────────────
    // Run for both dry-run and wet-run so that a report path that overlaps
    // the patch output is caught before any write (including the dry-run
    // report) leaves the tree in an unexpected state.
    {
        let rp_parent = report_dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let report_abs = std::fs::canonicalize(rp_parent)
            .or_else(|_| std::path::absolute(rp_parent))
            .ok()
            .map(|p| p.join(report_dest.file_name().unwrap_or_default()));
        if let Some(ref report_abs) = report_abs {
            for f in &files_os {
                let file_abs = git_root.join(f);
                // Direct conflict: the patch touches exactly the report file.
                if file_abs == *report_abs {
                    return Err(ApplyError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!(
                            "report path '{}' overlaps a file in the patch; \
                             use --report to choose a different destination",
                            report_dest.display()
                        ),
                    )));
                }
                // Ancestor conflict: the patch creates a regular file at a path
                // that is a parent directory of report_dest. write_report would
                // then fail with NotADirectory after the patch has been applied.
                if report_abs.starts_with(&file_abs) {
                    return Err(ApplyError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotADirectory,
                        format!(
                            "report path '{}' is under '{}' which the patch adds as a file; \
                             use --report to choose a different destination",
                            report_dest.display(),
                            file_abs.display()
                        ),
                    )));
                }
            }
        }
    }

    // ── 11. Dry-run: report without mutating ──────────────────────────────────
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
        write_report(&report_dest, &report)?;
        return Ok(report);
    }

    // ── 12. Apply the patch ───────────────────────────────────────────────────
    // Preflight the report destination *before* mutating the tree so that a
    // bad report path (unwritable directory, missing parent, etc.) fails
    // cleanly instead of leaving a modified-but-unreported checkout.
    preflight_report_path(&report_dest)?;

    // patch_path is canonicalized (absolute); git_root avoids silent skips
    // when --target is a repo subdirectory.
    apply_patch(&git_root, &patch_path, opts.three_way, &files_os)?;

    // ── 14. Write report ──────────────────────────────────────────────────────
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
    write_report(&report_dest, &report)?;

    Ok(report)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve the patch `PathBuf` and the trajectory `PathBuf` (if any) from the
/// selector, and check whether the source trajectory recorded patch-submission
/// redaction.
///
/// Returns `(patch_path, trajectory_path_opt, trajectory_recorded_redaction)`.
fn resolve_patch_and_redaction(selector: &PatchSelector) -> (PathBuf, Option<PathBuf>, bool) {
    match selector {
        PatchSelector::PatchFile(p) => {
            // For a direct --patch selector, also probe the sibling trajectory
            // so that redaction recorded only in metadata cannot be bypassed by
            // choosing --patch instead of --trajectory.
            let sibling_traj = sibling_trajectory_of_patch(p);
            let redacted = sibling_traj
                .as_deref()
                .is_some_and(trajectory_has_patch_submission_redaction);
            (p.clone(), sibling_traj, redacted)
        }
        PatchSelector::TrajectoryFile(traj_path) => {
            let patch_path = sibling_patch_of_trajectory(traj_path);
            let redacted = trajectory_has_patch_submission_redaction(traj_path);
            (patch_path, Some(traj_path.clone()), redacted)
        }
        PatchSelector::SweepInstance { sweep, instance } => {
            // Select patch by existence to avoid choosing a nonexistent patch
            // just because the trajectory for that layout is present.
            let nested_patch = sweep.join(instance).join("run-1.patch");
            let bundle_patch = sweep.join("patches").join(format!("{instance}.patch"));
            let legacy_patch = sweep.join(format!("{instance}.patch"));

            let patch_path = if nested_patch.exists() {
                nested_patch
            } else if bundle_patch.exists() {
                bundle_patch
            } else {
                legacy_patch
            };

            // Resolve trajectory independently; also check "trajectory.json"
            // (the legacy nested name used by some sweep versions) before
            // falling back to the bundle / flat layouts.
            let nested_traj = sweep.join(instance).join("run-1.traj.json");
            let nested_traj_legacy = sweep.join(instance).join("trajectory.json");
            let bundle_traj = sweep
                .join("trajectories")
                .join(format!("{instance}.traj.json"));
            let legacy_traj = sweep.join(format!("{instance}.traj.json"));

            // Check ALL candidate trajectories: if any records patch_submission
            // redaction the patch is refused. A stale clean nested trajectory
            // must not shadow redaction in the bundle trajectory that actually
            // corresponds to the selected patch.
            let redacted = [
                &nested_traj,
                &nested_traj_legacy,
                &bundle_traj,
                &legacy_traj,
            ]
            .iter()
            .filter(|p| p.exists())
            .any(|p| trajectory_has_patch_submission_redaction(p));

            // For reporting purposes, pick the first existing trajectory.
            let traj_path = if nested_traj.exists() {
                nested_traj
            } else if nested_traj_legacy.exists() {
                nested_traj_legacy
            } else if bundle_traj.exists() {
                bundle_traj
            } else {
                legacy_traj
            };

            (patch_path, Some(traj_path), redacted)
        }
    }
}

/// Derive the `.patch` sibling of a `.traj.json` trajectory file.
///
/// `task.traj.json` → `task.patch`
/// `task.json`      → `task.patch`
/// `task`           → `task.patch`
fn sibling_patch_of_trajectory(traj_path: &Path) -> PathBuf {
    let file_name = traj_path.file_name().unwrap_or_default();

    // Strip the ".traj.json" / ".json" suffix using raw bytes so non-UTF-8
    // filenames are not corrupted before the sibling patch path is built.
    #[cfg(unix)]
    let (base, patch_name): (OsString, OsString) = {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = file_name.as_bytes();
        let base_bytes = if bytes.ends_with(b".traj.json") {
            &bytes[..bytes.len() - 10]
        } else if bytes.ends_with(b".json") {
            &bytes[..bytes.len() - 5]
        } else {
            bytes
        };
        let mut pname = OsString::from_vec(base_bytes.to_vec());
        pname.push(".patch");
        (OsString::from_vec(base_bytes.to_vec()), pname)
    };
    #[cfg(not(unix))]
    let (base, patch_name): (OsString, OsString) = {
        let stem = file_name.to_string_lossy();
        let base_str = if let Some(s) = stem.strip_suffix(".traj.json") {
            s.to_owned()
        } else if let Some(s) = stem.strip_suffix(".json") {
            s.to_owned()
        } else {
            stem.into_owned()
        };
        let pname = OsString::from(format!("{base_str}.patch"));
        (OsString::from(base_str), pname)
    };
    let _ = base; // used only in some cfg branches

    let traj_dir = traj_path.parent().unwrap_or_else(|| Path::new("."));

    // Primary: sibling .patch in the same directory (most common).
    let sibling = traj_dir.join(&patch_name);
    if sibling.exists() {
        return sibling;
    }

    // Bundle layout: trajectories/<id>.traj.json → ../patches/<id>.patch.
    // Only apply this fallback when the trajectory lives inside a directory
    // named "trajectories" so we don't accidentally pick up an unrelated
    // patches/<base>.patch in non-bundle layouts.
    if traj_dir.file_name() == Some(std::ffi::OsStr::new("trajectories")) {
        if let Some(bundle_root) = traj_dir.parent() {
            let bundle_patch = bundle_root.join("patches").join(&patch_name);
            if bundle_patch.exists() {
                return bundle_patch;
            }
        }
    }

    // Fall back to the sibling path (may not exist; caller handles I/O error).
    sibling
}

/// Derive the `.traj.json` sibling of a `.patch` file, returning `Some` only
/// if the trajectory file actually exists on disk.
///
/// `task.patch` → `task.traj.json`
fn sibling_trajectory_of_patch(patch_path: &Path) -> Option<PathBuf> {
    let file_name = patch_path.file_name()?;

    // Strip the ".patch" suffix using raw bytes so non-UTF-8 filenames are
    // preserved and the resulting sibling path is correct on Unix.
    #[cfg(unix)]
    let traj_name: OsString = {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = file_name.as_bytes();
        let base = if bytes.ends_with(b".patch") {
            &bytes[..bytes.len() - 6]
        } else {
            bytes
        };
        let mut name = OsString::from_vec(base.to_vec());
        name.push(".traj.json");
        name
    };
    #[cfg(not(unix))]
    let traj_name: OsString = {
        let s = file_name.to_string_lossy();
        let base = s.strip_suffix(".patch").unwrap_or(&s);
        OsString::from(format!("{base}.traj.json"))
    };

    // Primary: same-directory sibling (most common).
    let sibling = patch_path.with_file_name(&traj_name);
    if sibling.exists() {
        return Some(sibling);
    }

    // Bundle layout: patches/<id>.patch → ../trajectories/<id>.traj.json.
    // Only apply this fallback when the patch actually lives in a directory
    // named "patches" to avoid matching an unrelated trajectories/ subtree
    // in non-bundle workspaces.
    if patch_path.parent().and_then(|p| p.file_name()) == Some(std::ffi::OsStr::new("patches")) {
        if let Some(bundle_root) = patch_path.parent().and_then(|p| p.parent()) {
            let bundle_traj = bundle_root.join("trajectories").join(&traj_name);
            if bundle_traj.exists() {
                return Some(bundle_traj);
            }
        }
    }

    None
}

/// Return `true` if the trajectory file at `traj_path` records at least one
/// redaction event on the `patch_submission` surface.
fn trajectory_has_patch_submission_redaction(traj_path: &Path) -> bool {
    let text = match std::fs::read_to_string(traj_path) {
        Ok(t) => t,
        // Missing trajectory → no redaction metadata recorded.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        // Unreadable trajectory → fail closed (treat as redacted).
        Err(_) => return true,
    };
    // Malformed trajectory → fail closed.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return true;
    };
    if let Some(counts) = value
        .pointer("/info/redaction/counts")
        .and_then(|v| v.as_array())
    {
        for entry in counts {
            if entry.get("surface").and_then(|s| s.as_str()) == Some("patch_submission") {
                return true;
            }
        }
    }
    // Legacy secret_leak_detected marker (direct path).
    if value
        .pointer("/info/secret_leak_detected/surface")
        .and_then(|s| s.as_str())
        == Some("patch_submission")
    {
        return true;
    }
    // Legacy secret_leak_detected marker (stored under info/other by some mini versions).
    if value
        .pointer("/info/other/secret_leak_detected/surface")
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
        .is_ok_and(|o| o.status.success())
}

/// Return the absolute path of the git worktree root containing `dir`.
fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    // Trim trailing newline from raw bytes before converting so that
    // non-UTF-8 directory names in the path are not corrupted.
    let mut bytes = output.stdout;
    while bytes.last().copied() == Some(b'\n') || bytes.last().copied() == Some(b'\r') {
        bytes.pop();
    }
    Some(PathBuf::from(bytes_to_os_string(bytes)))
}

/// Return dirty paths, excluding any paths that resolve to an entry in
/// `exclude_abs`. The patch file and trajectory file are excluded so that an
/// untracked artifact bundle inside the target does not trip the clean-tree gate.
fn dirty_paths_excluding(
    git_root: &Path,
    dir: &Path,
    exclude_abs: &[&Path],
) -> Result<Vec<String>, ApplyError> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "-z", "--untracked-files=all"])
        .current_dir(dir)
        .output()?;
    // --porcelain -z: records are NUL-terminated, paths are never C-string-
    // quoted (unlike the default line-based format). Rename/copy entries emit
    // "XY new\0old\0"; the old-path field has no "XY " status prefix so we
    // detect and skip it to avoid treating it as an additional dirty file.
    // Work at the byte level so non-UTF-8 filenames survive the comparison
    // with exclude_abs without being corrupted by from_utf8_lossy.
    let paths: Vec<String> = output
        .stdout
        .split(|&b| b == 0)
        .filter(|l| !l.trim_ascii().is_empty())
        .filter_map(|l| {
            // Skip old-name fields from rename/copy entries — they have no
            // "XY " status prefix (byte 2 is not a space).
            if l.get(2) != Some(&b' ') {
                return None;
            }
            let rel_bytes = l.get(3..)?.trim_ascii();
            if rel_bytes.is_empty() {
                return None;
            }
            let rel_os = bytes_to_os_string(rel_bytes.to_vec());
            let abs = git_root.join(&rel_os);
            // Canonicalize for comparison; fall back to raw path if it doesn't
            // exist yet (e.g. untracked file whose parent isn't resolved).
            let abs_canon = std::fs::canonicalize(&abs).unwrap_or(abs);
            let excluded = exclude_abs.iter().any(|ex| {
                let ex_canon = std::fs::canonicalize(ex).unwrap_or_else(|_| ex.to_path_buf());
                abs_canon == ex_canon
            });
            if excluded {
                None
            } else {
                // Lossy conversion is acceptable here: these strings are
                // only used in the DirtyTree error message for display.
                Some(rel_os.to_string_lossy().into_owned())
            }
        })
        .collect();
    Ok(paths)
}

/// Run `git apply --check [--3way]` and return `Ok(())` on success.
/// Passing `three_way` ensures patches that only apply via three-way merge
/// are not falsely rejected during the preflight check.
fn git_apply_check(apply_dir: &Path, patch_path: &Path, three_way: bool) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.args(["apply", "--check"]);
    if three_way {
        cmd.arg("--3way");
    }
    cmd.arg(patch_path).current_dir(apply_dir);
    let output = cmd.output().map_err(|e| e.to_string())?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    if output.status.success() {
        // When --3way is active, git apply --check can exit 0 even though the
        // real apply would produce conflict markers. git's conflict diagnostic
        // ends the line with " with conflicts." — check each line's suffix so
        // a filename like "foo with conflicts." ("Applied patch ... cleanly.")
        // doesn't trigger a false-positive.
        let ends_with_conflicts = |s: &str| s.lines().any(|l| l.ends_with(" with conflicts."));
        if three_way && (ends_with_conflicts(&stdout) || ends_with_conflicts(&stderr)) {
            let combined = format!("{}\n{}", stderr.trim(), stdout.trim());
            return Err(combined.trim().to_owned());
        }
        Ok(())
    } else {
        let stderr_s = stderr.trim().to_owned();
        let stdout_s = stdout.trim().to_owned();
        let msg = if stdout_s.is_empty() {
            stderr_s
        } else {
            format!("{stderr_s}\n{stdout_s}")
        };
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
///
/// Handles both `diff --git` extended headers and plain `---`/`+++` headers
/// (e.g. from `diff -u`). Quoted filenames (git C-string escaping) are
/// unescaped to raw bytes (preserved as `OsString`) so that non-UTF-8 paths
/// survive the round-trip to git cleanup commands. For renames, both the old
/// (`a/`) and new (`b/`) paths are included so that `--3way` cleanup can
/// unstage both the deletion and the addition sides of the rename.
fn parse_diff_stats(patch_text: &str) -> (Vec<OsString>, i64, i64) {
    let mut files: Vec<OsString> = Vec::new();
    let mut added: i64 = 0;
    let mut removed: i64 = 0;
    let mut in_hunk = false;
    // Whether the current file section opened with a "diff --git" header.
    let mut saw_git_header = false;
    // b/ (new) name set by "diff --git" or "+++ "; held for rename detection.
    let mut pending_b: Option<OsString> = None;
    // a/ (old) name set by "--- "; compared with b/ to detect renames.
    let mut pending_a: Option<OsString> = None;
    // True when the current section is a copy (not a rename). For copies the
    // source file is untouched, so it must not be added to the affected list.
    let mut is_copy = false;

    for line in patch_text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // New file section; flush any rename that had no "+++" pair
            // (e.g. 100% similarity renames produce no hunk lines).
            if !is_copy {
                let effective_b = pending_b.as_ref();
                if let (Some(a), Some(b)) = (&pending_a, effective_b) {
                    if a != b && !files.contains(a) {
                        files.push(a.clone());
                    }
                }
            }
            in_hunk = false;
            saw_git_header = true;
            is_copy = false;
            pending_a = None;
            pending_b = git_diff_b_path(rest);
            if let Some(ref name) = pending_b {
                if !files.contains(name) {
                    files.push(name.clone());
                }
            }
        } else if in_hunk {
            // Count added/removed hunk lines. Also detect "--- " as the start
            // of a new file section in multi-file plain unified diffs: without
            // a "diff --git" header, "--- " reliably signals the next file.
            // (Edge case: a removed content line whose original starts with
            // "-- " is indistinguishable; it is accepted as a known trade-off.)
            if let Some(rest) = line.strip_prefix("--- ") {
                in_hunk = false;
                pending_a = diff_header_path(rest, "a/");
            } else if line.starts_with('+') && !line.starts_with("+++") {
                added += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                removed += 1;
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            // New-file header outside a hunk: end of the header pair.
            let b_name = diff_header_path(rest, "b/");
            if saw_git_header {
                // The +++ path is parsed unambiguously (just strip "b/") while
                // the diff --git header's rfind(" b/") can split incorrectly
                // when the filename contains the literal substring " b/". When
                // the two disagree, replace the wrongly-split git-header entry
                // in `files` with the correctly-parsed +++ path.
                if let Some(ref b) = b_name {
                    match &pending_b {
                        Some(old) if old != b => {
                            if let Some(pos) = files.iter().rposition(|f| f == old) {
                                files[pos].clone_from(b);
                            }
                            pending_b.clone_from(&b_name);
                        }
                        None => {
                            if !files.contains(b) {
                                files.push(b.clone());
                            }
                            pending_b.clone_from(&b_name);
                        }
                        _ => {}
                    }
                }
            } else {
                // Plain unified-diff fallback (no "diff --git" seen).
                if let Some(ref name) = b_name {
                    if !files.contains(name) {
                        files.push(name.clone());
                    }
                } else if let Some(ref a) = pending_a {
                    // +++ /dev/null: plain deletion; record the old path.
                    if !files.contains(a) {
                        files.push(a.clone());
                    }
                }
                pending_b.clone_from(&b_name);
            }
            // Rename detection: if a/ ≠ b/, the old name is also affected
            // (git stages its deletion; cleanup must unstage that too).
            // Skip this for copies: the source file is unchanged by the patch.
            let effective_b = pending_b.as_ref().or(b_name.as_ref());
            if !is_copy {
                if let (Some(a), Some(b)) = (&pending_a, effective_b) {
                    if a != b && !files.contains(a) {
                        files.push(a.clone());
                    }
                }
            }
            is_copy = false;
            saw_git_header = false;
            pending_a = None;
            pending_b = None;
        } else if line.starts_with("copy from ") || line.starts_with("copy to ") {
            is_copy = true;
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            // 100% similarity renames have no "---"/"+++" pair; capture the
            // old path here so the section-end flush above can record it.
            pending_a = Some(OsString::from(rest));
        } else if let Some(rest) = line.strip_prefix("--- ") {
            // Old-file header outside a hunk: track for rename detection.
            pending_a = diff_header_path(rest, "a/");
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        }
    }

    // Flush the final section: a trailing 100% rename has no following
    // "diff --git" to trigger the section-start flush.
    if !is_copy {
        let effective_b = pending_b.as_ref();
        if let (Some(a), Some(b)) = (&pending_a, effective_b) {
            if a != b && !files.contains(a) {
                files.push(a.clone());
            }
        }
    }

    (files, added, removed)
}

/// Extract the `b/` path from the tail of a `diff --git a/… b/…` line.
///
/// Handles the unquoted format (`a/foo b/bar`) and git's C-string-quoted
/// format (`"a/foo bar" "b/bar baz"`).  Uses `rfind` for the unquoted case so
/// that an `a/` path containing the substring ` b/` is less likely to confuse
/// the split point.  Returns an `OsString` to preserve non-UTF-8 bytes that
/// survive git's C-string octal-escape encoding.
fn git_diff_b_path(rest: &str) -> Option<OsString> {
    if rest.starts_with('"') {
        // Quoted: '"a/…" "b/…"'
        let sep = rest.find(" \"b/")?;
        let b_start = sep + 4; // skip ' "b/'
        let b_end = find_unescaped_quote(&rest[b_start..]).map(|i| b_start + i)?;
        let name = unescape_c_string_os(&rest[b_start..b_end]);
        if name.is_empty() { None } else { Some(name) }
    } else {
        // Unquoted: rfind keeps the b/ path intact when a/ contains " b/".
        let pos = rest.rfind(" b/")?;
        let name = rest[pos + 3..].trim();
        if name.is_empty() {
            None
        } else {
            Some(OsString::from(name))
        }
    }
}

/// Extract a file path from a `---` or `+++` diff header line.
///
/// `prefix` is `"a/"` or `"b/"`.  Handles git's C-string-quoted format and
/// strips trailing timestamps (plain-diff `\t<datetime>` suffix).  Returns
/// `None` for `/dev/null` and empty paths.  Returns an `OsString` to preserve
/// non-UTF-8 bytes encoded via C-string octal escapes.
fn diff_header_path(s: &str, prefix: &str) -> Option<OsString> {
    if let Some(without_open) = s.strip_prefix('"') {
        // Quoted: '"prefix/path"'
        let end = find_unescaped_quote(without_open)?;
        let inner = &without_open[..end];
        let name = unescape_c_string_os(inner.strip_prefix(prefix).unwrap_or(inner));
        if name.is_empty() { None } else { Some(name) }
    } else {
        // Strip trailing tab+timestamp before any other checks. Plain unified
        // diffs include a TAB+datetime after the path (e.g. "--- /dev/null\t
        // 2024-01-01 00:00:00 +0000"). Use only the tab split — do not trim()
        // the result since filenames may legitimately end with a space.
        let s = s.split('\t').next().unwrap_or(s);
        // `/dev/null` is git's sentinel for new-file / deleted-file patches.
        if s == "/dev/null" {
            return None;
        }
        let path = if let Some(stripped) = s.strip_prefix(prefix) {
            stripped
        } else {
            // Plain-diff path without git's a/b convention (e.g. "diff -ru old
            // new" produces "+++ new/f1"). Strip one leading component to match
            // git apply's default -p1 strip level so the reported path matches
            // what git actually applied.
            s.find('/').map_or(s, |i| &s[i + 1..])
        };
        if path.is_empty() || path == "/dev/null" {
            None
        } else {
            Some(OsString::from(path))
        }
    }
}

/// Unescape a git C-string and return the raw bytes as an `OsString`.
///
/// On Unix the byte sequence is preserved exactly; on other platforms the
/// bytes are converted through `from_utf8_lossy` (non-UTF-8 paths are
/// extremely rare outside Unix).
fn unescape_c_string_os(s: &str) -> OsString {
    let bytes = unescape_c_string_bytes(s.as_bytes());
    bytes_to_os_string(bytes)
}

/// Build an `OsString` from a raw byte vec, preserving non-UTF-8 bytes on Unix.
fn bytes_to_os_string(bytes: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Find the index of the first unescaped `"` in `s`.
///
/// A `"` preceded by a backslash (itself part of a C-string escape sequence)
/// is not a closing quote. Octal escapes (`\nnn`) are also stepped over so
/// their digits are not mistaken for a quote.
fn find_unescaped_quote(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some(i),
            b'\\' => {
                i += 1;
                if i < bytes.len() && bytes[i] >= b'0' && bytes[i] <= b'7' {
                    // Octal escape: skip up to 2 more octal digits.
                    i += 1;
                    for _ in 0..2 {
                        if i < bytes.len() && bytes[i] >= b'0' && bytes[i] <= b'7' {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                } else if i < bytes.len() {
                    i += 1; // single-char escape (e.g. \", \\, \t)
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Byte-level C-string unescape as used by git for quoting special filenames.
fn unescape_c_string_bytes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] != b'\\' || i + 1 >= input.len() {
            out.push(input[i]);
            i += 1;
            continue;
        }
        i += 1;
        match input[i] {
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            b'"' => {
                out.push(b'"');
                i += 1;
            }
            b'\\' => {
                out.push(b'\\');
                i += 1;
            }
            b'a' => {
                out.push(b'\x07');
                i += 1;
            }
            b'b' => {
                out.push(b'\x08');
                i += 1;
            }
            b'f' => {
                out.push(b'\x0C');
                i += 1;
            }
            b'v' => {
                out.push(b'\x0B');
                i += 1;
            }
            d @ b'0'..=b'7' => {
                // Octal escape: git uses \nnn for non-ASCII bytes in path names.
                let mut val = u32::from(d - b'0');
                i += 1;
                for _ in 0..2 {
                    if i < input.len() && input[i] >= b'0' && input[i] <= b'7' {
                        val = val * 8 + u32::from(input[i] - b'0');
                        i += 1;
                    } else {
                        break;
                    }
                }
                #[allow(clippy::cast_possible_truncation)]
                out.push(val as u8); // max octal git produces is \377 = 255
            }
            _ => {
                out.push(b'\\');
                out.push(input[i]);
                i += 1;
            }
        }
    }
    out
}

/// Build a `:(literal)<path>` pathspec arg as an `OsString`.
fn literal_pathspec(f: &OsString) -> OsString {
    let mut arg = OsString::from(":(literal)");
    arg.push(f.as_os_str());
    arg
}

/// Return a map of `path → (mode, sha1)` for files among `files` that have
/// genuinely staged changes (index ≠ HEAD) before the apply runs.
///
/// Two-step:
/// 1. `git diff --cached --name-only -z -- <files>` → only paths with real
///    staged edits (unlike `git ls-files --stage` which returns every tracked
///    file). `-z` output is NUL-terminated so paths with special bytes are
///    never C-string-quoted, and we preserve them as raw `OsString` values.
/// 2. `git ls-files --stage -z -- <staged>` → capture mode + sha1 so they
///    can be restored via `update-index --cacheinfo` if the apply fails.
fn pre_staged_entries(
    git_root: &Path,
    files: &[OsString],
) -> std::collections::HashMap<OsString, (String, String)> {
    let mut result = std::collections::HashMap::new();

    // Step 1: which of the affected files actually have staged edits?
    // Use -z so git emits NUL-terminated raw paths — never C-string-quoted.
    let mut diff_cmd = Command::new("git");
    diff_cmd.args(["diff", "--cached", "--name-only", "-z", "--"]);
    for f in files {
        diff_cmd.arg(literal_pathspec(f));
    }
    diff_cmd.current_dir(git_root);
    let Ok(diff_output) = diff_cmd.output() else {
        return result;
    };
    // Split on NUL; each segment is a raw path (preserved as OsString).
    let staged_os: Vec<OsString> = diff_output
        .stdout
        .split(|&b| b == b'\0')
        .filter(|s| !s.is_empty())
        .map(|s| bytes_to_os_string(s.to_vec()))
        .collect();
    if staged_os.is_empty() {
        return result;
    }

    // Step 2: record mode + sha1 for each genuinely staged file.
    // Use -z so git emits NUL-terminated records; format per record:
    // "<mode> <sha1> <stage>\t<path>\0"
    let mut ls_cmd = Command::new("git");
    ls_cmd.args(["ls-files", "--stage", "-z", "--"]);
    for f in &staged_os {
        ls_cmd.arg(literal_pathspec(f));
    }
    ls_cmd.current_dir(git_root);
    let Ok(ls_output) = ls_cmd.output() else {
        return result;
    };
    for record in ls_output.stdout.split(|&b| b == b'\0') {
        if record.is_empty() {
            continue;
        }
        // Each record: "<mode> <sha1> <stage>\t<path>"
        let Some(tab_pos) = record.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let meta_str = String::from_utf8_lossy(&record[..tab_pos]);
        let path_os = bytes_to_os_string(record[tab_pos + 1..].to_vec());
        let mut meta_parts = meta_str.split(' ');
        let (Some(mode), Some(sha), Some(stage)) =
            (meta_parts.next(), meta_parts.next(), meta_parts.next())
        else {
            continue;
        };
        if stage != "0" {
            continue; // skip unmerged entries (stages 1–3)
        }
        result.insert(path_os, (mode.to_owned(), sha.to_owned()));
    }
    result
}

/// Run `git apply [--3way]` from `git_root` and return `Ok(())` on success.
///
/// `affected_files` is the list of paths the patch touches (from
/// `parse_diff_stats`). It drives two `--3way`-specific corrections that both
/// use the pre-apply index state to avoid clobbering pre-existing staged edits:
///
/// - **Failure restore**: `git apply --check --3way` can exit 0 when the real
///   merge has conflicts, leaving conflict markers and unmerged index entries.
///   For every affected file: clear unmerged stages with `git reset HEAD --`,
///   then for files that had pre-existing staged edits restore their index
///   entry via `git update-index --cacheinfo` and working tree via
///   `git checkout-index --force`; for files that were not staged restore
///   the working tree with `git checkout --`.
///
/// - **Success unstage**: `git apply --3way` stages successful merges in the
///   index. For each affected file that had no pre-apply staged entry: reset
///   it to unstaged. This gives the same "modified working tree only"
///   semantics as regular `git apply`, while still preserving any pre-existing
///   staged edits in affected paths.
fn apply_patch(
    git_root: &Path,
    patch_path: &Path,
    three_way: bool,
    affected_files: &[OsString],
) -> Result<(), ApplyError> {
    // Record which affected files have genuinely staged changes (index ≠ HEAD)
    // before we touch anything, capturing their mode + sha1 for restoration.
    let pre_staged = if three_way && !affected_files.is_empty() {
        pre_staged_entries(git_root, affected_files)
    } else {
        std::collections::HashMap::new()
    };

    let mut cmd = Command::new("git");
    cmd.arg("apply");
    if three_way {
        cmd.arg("--3way");
    }
    cmd.arg(patch_path).current_dir(git_root);
    let output = cmd.output()?;

    if !output.status.success() {
        if three_way {
            for f in affected_files {
                // Use :(literal) to prevent git from treating file names that
                // contain pathspec metacharacters (e.g. '*') as globs.
                // Clear any unmerged index entries for this path so that
                // subsequent checkout commands are not refused.
                let _ = Command::new("git")
                    .args(["reset", "HEAD", "--"])
                    .arg(literal_pathspec(f))
                    .current_dir(git_root)
                    .output();
                if let Some((mode, sha)) = pre_staged.get(f) {
                    // Restore the pre-apply staged state, then check out that
                    // version into the working tree.
                    // Use three-arg --cacheinfo form so the path is an OsStr
                    // arg rather than embedded in a comma-delimited string.
                    let _ = Command::new("git")
                        .args(["update-index", "--cacheinfo", mode, sha])
                        .arg(f)
                        .current_dir(git_root)
                        .output();
                    // checkout-index takes literal file names (not pathspecs).
                    let _ = Command::new("git")
                        .args(["checkout-index", "--force", "--"])
                        .arg(f)
                        .current_dir(git_root)
                        .output();
                } else {
                    // No pre-existing staged edit: restore working tree from HEAD.
                    let _ = Command::new("git")
                        .args(["checkout", "--"])
                        .arg(literal_pathspec(f))
                        .current_dir(git_root)
                        .output();
                }
            }
        }
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(ApplyError::CheckFailed(msg));
    }

    // Restore the pre-apply index state for every affected file.
    // git apply --3way stages the merged result; we always clear that so the
    // apply behaves like regular git apply (working-tree only). Then for files
    // that had pre-existing staged edits, re-apply the original cached entry
    // so the caller's staged work is exactly preserved.
    if three_way {
        for f in affected_files {
            let _ = Command::new("git")
                .args(["reset", "HEAD", "--"])
                .arg(literal_pathspec(f))
                .current_dir(git_root)
                .output();
            if let Some((mode, sha)) = pre_staged.get(f) {
                let _ = Command::new("git")
                    .args(["update-index", "--cacheinfo", mode, sha])
                    .arg(f)
                    .current_dir(git_root)
                    .output();
            }
        }
    }
    Ok(())
}

/// Verify the report destination is writable *before* mutating the tree.
///
/// Probes the report path for write access before `apply_patch` runs.
///
/// Does NOT create missing parent directories — that is deferred to
/// `write_report` after the patch succeeds, so a missing parent can never
/// conflict with a path the patch adds. Parent creation here would leave
/// behind untracked directories if the apply subsequently fails.
fn preflight_report_path(path: &Path) -> Result<(), ApplyError> {
    // Reject immediately if the report path itself is already a directory;
    // write_report would fail with IsADirectory after mutating the tree.
    if path.is_dir() {
        return Err(ApplyError::Io(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            format!("report path is a directory: {}", path.display()),
        )));
    }
    // Walk up to the nearest existing ancestor to place the probe file,
    // since missing intermediate directories have not been created yet.
    // Treat an empty parent (relative path with no directory component) as
    // "." so that e.g. `--report out/report.json` (where `out` is missing)
    // continues walking rather than breaking out of the loop early.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut probe_dir = parent;
    while !probe_dir.exists() {
        probe_dir = match probe_dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            Some(_) => Path::new("."), // empty parent → fall back to cwd
            None => break,
        };
    }
    // If the report file already exists, verify it is writable directly
    // rather than relying on a sibling probe. A read-only existing report
    // would pass the sibling probe but fail in write_report after apply.
    if path.is_file() {
        std::fs::OpenOptions::new().write(true).open(path)?;
    }
    let probe = probe_dir.join(format!(".apply-probe-{}.tmp", std::process::id()));
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Serialize and write the report JSON to `path`, creating parent directories
/// as needed. Creating directories here (after a successful apply) avoids
/// materializing directories that could conflict with paths the patch adds.
fn write_report(path: &Path, report: &ApplyReport) -> Result<(), ApplyError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| ApplyError::Io(std::io::Error::other(e.to_string())))?;
    std::fs::write(path, json)?;
    Ok(())
}

// ── CLI helpers (public for cli/mod.rs) ───────────────────────────────────────

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
        ApplyError::CheckFailed(_) => crate::exit_code::ExitCode::ApplyCheckFailed.outcome_class(),
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
