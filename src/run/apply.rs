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
//! 5. `git apply --check [--3way]` must succeed (exit 28 on rejection).
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
    let patch_text = std::fs::read_to_string(&patch_path)?;

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
    let (files_changed, lines_added, lines_removed) = parse_diff_stats(&patch_text);
    let target_sha = git_head_sha(&git_root);

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
    // patch_path is canonicalized (absolute); git_root avoids silent skips
    // when --target is a repo subdirectory.
    apply_patch(&git_root, &patch_path, opts.three_way, &files_changed)?;

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
        PatchSelector::PatchFile(p) => (p.clone(), None, false),
        PatchSelector::TrajectoryFile(traj_path) => {
            let patch_path = sibling_patch_of_trajectory(traj_path);
            let redacted = trajectory_has_patch_submission_redaction(traj_path);
            (patch_path, Some(traj_path.clone()), redacted)
        }
        PatchSelector::SweepInstance { sweep, instance } => {
            // Try layouts in priority order matching bundle.rs find_patch_path_for_run:
            // 1. Canonical nested: <sweep>/<instance>/run-1.{patch,traj.json}
            let nested_patch = sweep.join(instance).join("run-1.patch");
            let nested_traj = sweep.join(instance).join("run-1.traj.json");
            // 2. Bundle layout: <sweep>/patches/<instance>.patch + trajectories/<instance>.traj.json
            let bundle_patch = sweep.join("patches").join(format!("{instance}.patch"));
            let bundle_traj = sweep
                .join("trajectories")
                .join(format!("{instance}.traj.json"));
            // 3. Legacy flat: <sweep>/<instance>.{patch,traj.json}
            let legacy_patch = sweep.join(format!("{instance}.patch"));
            let legacy_traj = sweep.join(format!("{instance}.traj.json"));

            let (patch_path, traj_path) = if nested_patch.exists() || nested_traj.exists() {
                (nested_patch, nested_traj)
            } else if bundle_patch.exists() || bundle_traj.exists() {
                (bundle_patch, bundle_traj)
            } else {
                (legacy_patch, legacy_traj)
            };

            let redacted = trajectory_has_patch_submission_redaction(&traj_path);
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
    let stem = traj_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();

    let base = if let Some(s) = stem.strip_suffix(".traj.json") {
        s.to_owned()
    } else if let Some(s) = stem.strip_suffix(".json") {
        s.to_owned()
    } else {
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
    let Ok(text) = std::fs::read_to_string(traj_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
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
    // Legacy secret_leak_detected marker
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
        .is_ok_and(|o| o.status.success())
}

/// Return the absolute path of the git worktree root containing `dir`.
fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_owned()))
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<String> = stdout
        .split('\0')
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            // Skip old-name fields from rename/copy entries — they have no
            // "XY " status prefix (byte 2 is not a space).
            if l.as_bytes().get(2) != Some(&b' ') {
                return None;
            }
            let rel = l.get(3..)?.trim();
            if rel.is_empty() {
                return None;
            }
            let abs = git_root.join(rel);
            // Canonicalize for comparison; fall back to raw path if it doesn't
            // exist yet (e.g. untracked file whose parent isn't resolved).
            let abs_canon = std::fs::canonicalize(&abs).unwrap_or(abs);
            let excluded = exclude_abs.iter().any(|ex| {
                let ex_canon = std::fs::canonicalize(ex).unwrap_or_else(|_| ex.to_path_buf());
                abs_canon == ex_canon
            });
            if excluded { None } else { Some(rel.to_owned()) }
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

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let msg = if stdout.is_empty() {
            stderr
        } else {
            format!("{stderr}\n{stdout}")
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
/// unescaped. For renames, both the old (`a/`) and new (`b/`) paths are
/// included in the returned file list so that `--3way` cleanup can unstage
/// both the deletion and the addition sides of the rename.
fn parse_diff_stats(patch_text: &str) -> (Vec<String>, i64, i64) {
    let mut files: Vec<String> = Vec::new();
    let mut added: i64 = 0;
    let mut removed: i64 = 0;
    let mut in_hunk = false;
    // Whether the current file section opened with a "diff --git" header.
    let mut saw_git_header = false;
    // b/ (new) name set by "diff --git" or "+++ "; held for rename detection.
    let mut pending_b: Option<String> = None;
    // a/ (old) name set by "--- "; compared with b/ to detect renames.
    let mut pending_a: Option<String> = None;

    for line in patch_text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // New file section; always terminates any active hunk.
            in_hunk = false;
            saw_git_header = true;
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
            if !saw_git_header {
                // Plain unified-diff fallback (no "diff --git" seen).
                if let Some(ref name) = b_name {
                    if !files.contains(name) {
                        files.push(name.clone());
                    }
                }
                pending_b.clone_from(&b_name);
            }
            // Rename detection: if a/ ≠ b/, the old name is also affected
            // (git stages its deletion; cleanup must unstage that too).
            let effective_b = pending_b.as_ref().or(b_name.as_ref());
            if let (Some(a), Some(b)) = (&pending_a, effective_b) {
                if a != b && !files.contains(a) {
                    files.push(a.clone());
                }
            }
            saw_git_header = false;
            pending_a = None;
            pending_b = None;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            // Old-file header outside a hunk: track for rename detection.
            pending_a = diff_header_path(rest, "a/");
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        }
    }

    (files, added, removed)
}

/// Extract the `b/` path from the tail of a `diff --git a/… b/…` line.
///
/// Handles the unquoted format (`a/foo b/bar`) and git's C-string-quoted
/// format (`"a/foo bar" "b/bar baz"`).  Uses `rfind` for the unquoted case so
/// that an `a/` path containing the substring ` b/` is less likely to confuse
/// the split point.
fn git_diff_b_path(rest: &str) -> Option<String> {
    if rest.starts_with('"') {
        // Quoted: '"a/…" "b/…"'
        let sep = rest.find(" \"b/")?;
        let b_start = sep + 4; // skip ' "b/'
        let b_end = rest[b_start..].find('"').map(|i| b_start + i)?;
        let name = unescape_c_string(&rest[b_start..b_end]);
        if name.is_empty() { None } else { Some(name) }
    } else {
        // Unquoted: rfind keeps the b/ path intact when a/ contains " b/".
        let pos = rest.rfind(" b/")?;
        let name = rest[pos + 3..].trim().to_owned();
        if name.is_empty() { None } else { Some(name) }
    }
}

/// Extract a file path from a `---` or `+++` diff header line.
///
/// `prefix` is `"a/"` or `"b/"`.  Handles git's C-string-quoted format and
/// strips trailing timestamps (plain-diff `\t<datetime>` suffix).  Returns
/// `None` for `/dev/null` and empty paths.
fn diff_header_path(s: &str, prefix: &str) -> Option<String> {
    if let Some(without_open) = s.strip_prefix('"') {
        // Quoted: '"prefix/path"'
        let end = without_open.find('"')?;
        let inner = &without_open[..end];
        let name = unescape_c_string(inner.strip_prefix(prefix).unwrap_or(inner));
        if name.is_empty() || name == "/dev/null" {
            None
        } else {
            Some(name)
        }
    } else {
        let path = if let Some(stripped) = s.strip_prefix(prefix) {
            stripped
        } else {
            // Plain-diff path without git's a/b convention (e.g. "diff -ru old
            // new" produces "+++ new/f1"). Strip one leading component to match
            // git apply's default -p1 strip level so the reported path matches
            // what git actually applied.
            s.find('/').map_or(s, |i| &s[i + 1..])
        };
        let path = path.split('\t').next().unwrap_or(path).trim();
        if path.is_empty() || path == "/dev/null" {
            None
        } else {
            Some(path.to_owned())
        }
    }
}

/// Unescape a git C-string (the content between double-quotes, without them).
fn unescape_c_string(s: &str) -> String {
    let bytes = unescape_c_string_bytes(s.as_bytes());
    String::from_utf8_lossy(&bytes).into_owned()
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

/// Return a map of `path → (mode, sha1)` for files among `files` that have
/// genuinely staged changes (index ≠ HEAD) before the apply runs.
///
/// Two-step:
/// 1. `git diff --cached --name-only -- <files>` → only paths with real staged
///    edits (unlike `git ls-files --stage` which returns every tracked file).
/// 2. `git ls-files --stage -- <staged>` → capture mode + sha1 so they can
///    be restored via `update-index --cacheinfo` if the apply fails.
fn pre_staged_entries(
    git_root: &Path,
    files: &[String],
) -> std::collections::HashMap<String, (String, String)> {
    let mut result = std::collections::HashMap::new();

    // Step 1: which of the affected files actually have staged edits?
    let mut diff_cmd = Command::new("git");
    diff_cmd.args(["diff", "--cached", "--name-only", "--"]);
    for f in files {
        diff_cmd.arg(format!(":(literal){f}"));
    }
    diff_cmd.current_dir(git_root);
    let Ok(diff_output) = diff_cmd.output() else {
        return result;
    };
    let diff_text = String::from_utf8_lossy(&diff_output.stdout);
    let staged_names: Vec<String> = diff_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if staged_names.is_empty() {
        return result;
    }

    // Step 2: record mode + sha1 for each genuinely staged file.
    let mut ls_cmd = Command::new("git");
    ls_cmd.args(["ls-files", "--stage", "--"]);
    for f in &staged_names {
        ls_cmd.arg(format!(":(literal){f}"));
    }
    ls_cmd.current_dir(git_root);
    let Ok(ls_output) = ls_cmd.output() else {
        return result;
    };
    let ls_text = String::from_utf8_lossy(&ls_output.stdout);
    for line in ls_text.lines() {
        // Format: "<mode> <sha1> <stage>\t<path>"
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let mut meta_parts = meta.split(' ');
        let (Some(mode), Some(sha), Some(stage)) =
            (meta_parts.next(), meta_parts.next(), meta_parts.next())
        else {
            continue;
        };
        if stage != "0" {
            continue; // skip unmerged entries (stages 1–3)
        }
        result.insert(path.to_owned(), (mode.to_owned(), sha.to_owned()));
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
    affected_files: &[String],
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
                let lit = format!(":(literal){f}");
                // Clear any unmerged index entries for this path so that
                // subsequent checkout commands are not refused.
                let _ = Command::new("git")
                    .args(["reset", "HEAD", "--", &lit])
                    .current_dir(git_root)
                    .output();
                if let Some((mode, sha)) = pre_staged.get(f) {
                    // Restore the pre-apply staged state, then check out that
                    // version into the working tree.
                    let cacheinfo = format!("{mode},{sha},{f}");
                    let _ = Command::new("git")
                        .args(["update-index", "--cacheinfo", &cacheinfo])
                        .current_dir(git_root)
                        .output();
                    // checkout-index takes literal file names (not pathspecs).
                    let _ = Command::new("git")
                        .args(["checkout-index", "--force", "--", f])
                        .current_dir(git_root)
                        .output();
                } else {
                    // No pre-existing staged edit: restore working tree from HEAD.
                    let _ = Command::new("git")
                        .args(["checkout", "--", &lit])
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
                .args(["reset", "HEAD", "--", &format!(":(literal){f}")])
                .current_dir(git_root)
                .output();
            if let Some((mode, sha)) = pre_staged.get(f) {
                let cacheinfo = format!("{mode},{sha},{f}");
                let _ = Command::new("git")
                    .args(["update-index", "--cacheinfo", &cacheinfo])
                    .current_dir(git_root)
                    .output();
            }
        }
    }
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
