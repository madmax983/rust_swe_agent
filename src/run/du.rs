//! `bench du`: report run-directory disk footprint and safely reclaim stale
//! sweep artifacts. Reads only on-disk artifacts (zero model calls, zero
//! network). See `docs/spec-disk-usage.md` for the full contract.

#![allow(clippy::cast_precision_loss)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error};
use crate::run::bundle::BUNDLE_MANIFEST_PATH;

// ── public data types ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DuArgs {
    pub root: PathBuf,
    pub in_progress_window: Duration,
    pub prune: Option<PruneRequest>,
}

#[derive(Debug, Clone)]
pub struct PruneRequest {
    pub apply: bool,
    pub older_than: Option<Duration>,
    pub keep_last: Option<usize>,
    pub incomplete_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Complete,
    Interrupted,
    Incomplete,
    InProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Trajectories,
    Evaluation,
    Bundles,
    PartialCheckpoints,
    Logs,
    Other,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CategoryBytes {
    pub trajectories: u64,
    pub evaluation: u64,
    pub bundles: u64,
    pub partial_checkpoints: u64,
    pub logs: u64,
    pub other: u64,
}

impl CategoryBytes {
    fn add(&mut self, category: Category, bytes: u64) {
        let slot = match category {
            Category::Trajectories => &mut self.trajectories,
            Category::Evaluation => &mut self.evaluation,
            Category::Bundles => &mut self.bundles,
            Category::PartialCheckpoints => &mut self.partial_checkpoints,
            Category::Logs => &mut self.logs,
            Category::Other => &mut self.other,
        };
        *slot += bytes;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepReport {
    pub id: String,
    /// Human-readable, JSON-safe path. On Unix this is a **lossy** UTF-8
    /// rendering (`Path::display`) — invalid byte sequences in a non-UTF-8
    /// directory name are replaced with U+FFFD. Never used for filesystem
    /// operations; see `path_buf`.
    pub path: String,
    /// The exact on-disk path, byte-for-byte. Not serialized (JSON cannot
    /// losslessly represent arbitrary non-UTF-8 paths); used for every
    /// actual filesystem operation (deletion, re-scan) so a non-UTF-8
    /// sweep name is never mistargeted via a lossy string round-trip.
    #[serde(skip)]
    pub path_buf: PathBuf,
    pub total_bytes: u64,
    pub lifecycle_state: LifecycleState,
    pub last_modified: String,
    pub last_modified_unix: u64,
    /// Sub-second remainder (0..1_000_000_000) of the same mtime as
    /// `last_modified_unix`. `last_modified_unix` alone truncates to whole
    /// seconds, which collapses distinct mtimes together on filesystems with
    /// sub-second resolution (common for artifacts finished within the same
    /// batch); `--keep-last` ranking and the pre-delete recency re-check
    /// both compare the `(last_modified_unix, last_modified_nanos)` pair so
    /// two sweeps modified in the same second still rank by actual recency
    /// rather than falling through to an alphabetical `id` tiebreak.
    pub last_modified_nanos: u32,
    pub age_seconds: u64,
    pub categories: CategoryBytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneSelectors {
    pub older_than_secs: Option<u64>,
    pub keep_last: Option<usize>,
    pub incomplete_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneEntry {
    pub id: String,
    /// See `SweepReport::path` — lossy, display-only.
    pub path: String,
    /// See `SweepReport::path_buf` — exact, used for filesystem operations.
    #[serde(skip)]
    pub path_buf: PathBuf,
    pub bytes: u64,
    pub lifecycle_state: LifecycleState,
    /// The `SweepReport::last_modified_unix`/`last_modified_nanos` this
    /// entry was ranked against when `--keep-last` retention was computed.
    /// Used by the pre-delete recheck in `build_prune_report` to detect that
    /// a candidate's own recency has changed since ranking — see that
    /// function's doc comment.
    pub last_modified_unix: u64,
    pub last_modified_nanos: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneReport {
    pub apply: bool,
    pub dry_run: bool,
    pub selectors: PruneSelectors,
    pub candidates: Vec<PruneEntry>,
    /// Sweeps skipped as unsafe to delete: either already `in_progress` at
    /// scan time, or caught in-progress by the immediately-before-delete
    /// re-check (see `build_prune_report`).
    pub protected: Vec<PruneEntry>,
    pub retained_by_keep_last: Vec<PruneEntry>,
    pub deleted: Vec<PruneEntry>,
    /// Candidates where `std::fs::remove_dir_all` itself failed (permission
    /// error, non-UTF-8 path mismatch, etc.). Also drives `blocked` — a
    /// failed deletion means the reclaim was not fully carried out, which is
    /// as much a reason to exit non-zero as an unsafe-to-delete candidate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deletion_failed: Vec<PruneEntry>,
    pub would_reclaim_bytes: u64,
    pub reclaimed_bytes: u64,
    /// `true` when `--apply` was requested and at least one sweep that
    /// otherwise matched the selectors was skipped (`protected`) or failed
    /// to delete (`deletion_failed`). Drives exit code 50
    /// (`disk_usage_prune_blocked`).
    pub blocked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuReport {
    pub generated_at: String,
    pub root: String,
    pub total_bytes: u64,
    pub unattributed_bytes: u64,
    pub sweeps: Vec<SweepReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<PruneReport>,
}

// ── public entry point ──────────────────────────────────────────────────────

pub fn run(args: &DuArgs) -> Result<DuReport, Error> {
    if !args.root.is_dir() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "bench du: --root {} is not a directory",
            args.root.display()
        ))));
    }

    let now = SystemTime::now();
    let mut sweeps: Vec<SweepReport> = Vec::new();
    let mut unattributed_bytes = 0u64;

    let entries = std::fs::read_dir(&args.root).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "bench du: cannot read --root {}: {e}",
            args.root.display()
        )))
    })?;

    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            sweeps.push(scan_sweep(&p, now, args.in_progress_window));
        } else if let Ok(meta) = entry.metadata() {
            unattributed_bytes += meta.len();
        }
    }

    sweeps.sort_by(|a, b| {
        b.total_bytes
            .cmp(&a.total_bytes)
            .then_with(|| a.id.cmp(&b.id))
    });

    let total_bytes: u64 = sweeps.iter().map(|s| s.total_bytes).sum::<u64>() + unattributed_bytes;

    let mut report = DuReport {
        generated_at: utc_now_iso8601(),
        root: args.root.display().to_string(),
        total_bytes,
        unattributed_bytes,
        sweeps,
        prune: None,
    };

    if let Some(prune_args) = &args.prune {
        report.prune = Some(build_prune_report(
            &report.sweeps,
            prune_args,
            args.in_progress_window,
        ));
    }

    Ok(report)
}

/// Evaluate selectors, then (with `--apply`) delete every resulting
/// candidate — re-checking each one immediately beforehand with a fresh,
/// single-sweep scan re-evaluated against the same selectors.
///
/// The initial full-`--root` scan that produced `sweeps` can be arbitrarily
/// stale by the time a given candidate is actually reached in the deletion
/// loop (a `--root` with many/large sweeps takes real time to scan — see
/// `docs/spec-disk-usage.md`'s Performance Notes). Two things can have
/// changed by then: the sweep can have become actively in-progress again (a
/// resumed/retried process checkpointing), or it can have simply *finished*
/// — no longer in-progress, but also no longer old enough for
/// `--older-than`, and no longer `incomplete`/`interrupted` for
/// `--incomplete-only` now that it has a fresh `results.json`. Both cases
/// re-scan to `protected` rather than deleted. Re-scanning right before
/// deleting narrows the race window from "the whole scan" down to "one
/// directory stat", though it cannot eliminate it entirely without a
/// lock/PID mechanism this codebase does not have — see the "Lifecycle
/// states" caveat in `docs/spec-disk-usage.md`. `--keep-last`'s full N-most-
/// recent ranking is not recomputed here — it ranks a sweep against every
/// *other* sweep's recency, not a static property of the sweep itself, so a
/// single-sweep re-check can't fully redo it in isolation — but a candidate
/// whose *own* recency changed since it was ranked (e.g. it finished and
/// wrote a fresh `results.json`, which could well put it inside the
/// retained window now) is conservatively protected rather than risked.
pub fn build_prune_report(
    sweeps: &[SweepReport],
    prune_args: &PruneRequest,
    in_progress_window: Duration,
) -> PruneReport {
    let selectors = PruneSelectors {
        older_than_secs: prune_args.older_than.map(|d| d.as_secs()),
        keep_last: prune_args.keep_last,
        incomplete_only: prune_args.incomplete_only,
    };
    let eval = evaluate_prune_candidates(sweeps, &selectors);

    let would_reclaim_bytes: u64 = eval.candidates.iter().map(|c| c.bytes).sum();
    let mut deleted: Vec<PruneEntry> = Vec::new();
    let mut deletion_failed: Vec<PruneEntry> = Vec::new();
    let mut protected = eval.protected;
    let mut reclaimed_bytes = 0u64;

    if prune_args.apply {
        for candidate in &eval.candidates {
            let recheck = scan_sweep(&candidate.path_buf, SystemTime::now(), in_progress_window);
            if recheck.lifecycle_state == LifecycleState::InProgress {
                eprintln!(
                    "warning: bench du: {} became in-progress since the initial scan; skipping deletion",
                    candidate.path
                );
                protected.push(candidate.clone());
                continue;
            }
            // A sweep that isn't in-progress can still have changed enough
            // to no longer match the selectors that made it a candidate in
            // the first place — most notably, a sweep that *finished*
            // between the initial scan and this recheck: its fresh mtime
            // means it no longer satisfies --older-than, and its fresh
            // `results.json` means it's no longer `incomplete`/`interrupted`
            // for --incomplete-only. Deleting it anyway would reclaim a
            // just-completed sweep the operator's own selectors say to keep.
            if !matches_age_and_incomplete_selectors(&recheck, &selectors) {
                eprintln!(
                    "warning: bench du: {} no longer matches the prune selectors since the initial scan (now {}); skipping deletion",
                    candidate.path,
                    lifecycle_label(recheck.lifecycle_state)
                );
                protected.push(candidate.clone());
                continue;
            }
            // --keep-last ranks a sweep against every other sweep's recency,
            // so it can't be re-evaluated in isolation the way age/lifecycle
            // can (see the function doc comment). But if THIS candidate's
            // own recency changed since it was ranked — e.g. it finished and
            // wrote a fresh results.json while a large --root was still
            // being pruned — the "not among the N most recent" verdict that
            // made it a candidate is stale and can no longer be trusted:
            // conservatively protect it rather than risk deleting a sweep
            // that would now rank inside --keep-last's protected window.
            if selectors.keep_last.is_some()
                && (recheck.last_modified_unix, recheck.last_modified_nanos)
                    != (candidate.last_modified_unix, candidate.last_modified_nanos)
            {
                eprintln!(
                    "warning: bench du: {} was modified since the --keep-last ranking was computed; skipping deletion",
                    candidate.path
                );
                protected.push(candidate.clone());
                continue;
            }
            match std::fs::remove_dir_all(&candidate.path_buf) {
                Ok(()) => {
                    reclaimed_bytes += candidate.bytes;
                    deleted.push(candidate.clone());
                }
                Err(e) => {
                    eprintln!(
                        "warning: bench du: could not delete {}: {e}",
                        candidate.path
                    );
                    deletion_failed.push(candidate.clone());
                }
            }
        }
    }

    let blocked = prune_args.apply && (!protected.is_empty() || !deletion_failed.is_empty());

    PruneReport {
        apply: prune_args.apply,
        dry_run: !prune_args.apply,
        selectors,
        candidates: eval.candidates,
        protected,
        retained_by_keep_last: eval.retained,
        deleted,
        deletion_failed,
        would_reclaim_bytes,
        reclaimed_bytes,
        blocked,
    }
}

// ── rendering ────────────────────────────────────────────────────────────────

pub fn render_text(report: &DuReport) -> String {
    use std::fmt::Write as _;

    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let mut out = String::new();
    let _ = writeln!(out, "\n=== bench du ===");
    let _ = writeln!(out, "Root: {}", report.root);
    let _ = writeln!(out, "Generated: {}", report.generated_at);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Total: {} across {} sweep(s) (+ {} unattributed)",
        human_bytes(report.total_bytes),
        report.sweeps.len(),
        human_bytes(report.unattributed_bytes)
    );

    let _ = writeln!(out, "\nSweeps (ranked by size):");
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "sweep",
            "bytes",
            "lifecycle",
            "trajectories",
            "partial_ckpt",
            "evaluation",
            "bundles",
            "logs",
            "other",
        ]);
    for s in &report.sweeps {
        table.add_row(vec![
            s.id.clone(),
            s.total_bytes.to_string(),
            lifecycle_label(s.lifecycle_state).to_owned(),
            s.categories.trajectories.to_string(),
            s.categories.partial_checkpoints.to_string(),
            s.categories.evaluation.to_string(),
            s.categories.bundles.to_string(),
            s.categories.logs.to_string(),
            s.categories.other.to_string(),
        ]);
    }
    let _ = writeln!(out, "{table}");

    if let Some(prune) = &report.prune {
        render_prune_text(&mut out, prune);
    }

    out
}

fn render_prune_text(out: &mut String, prune: &PruneReport) {
    use std::fmt::Write as _;

    let _ = writeln!(out, "\n=== prune ===");
    if prune.dry_run {
        let _ = writeln!(out, "DRY RUN — nothing deleted (pass --apply to reclaim)");
    }
    let _ = writeln!(
        out,
        "Candidates: {} ({} would be reclaimed)",
        prune.candidates.len(),
        human_bytes(prune.would_reclaim_bytes)
    );
    for c in &prune.candidates {
        let _ = writeln!(
            out,
            "  - {} ({}, {})",
            c.id,
            human_bytes(c.bytes),
            lifecycle_label(c.lifecycle_state)
        );
    }
    if !prune.protected.is_empty() {
        let _ = writeln!(
            out,
            "Protected (not confirmed idle within --in-progress-window, skipped): {}",
            prune.protected.len()
        );
        for c in &prune.protected {
            let _ = writeln!(out, "  - {} ({})", c.id, human_bytes(c.bytes));
        }
    }
    if !prune.retained_by_keep_last.is_empty() {
        let _ = writeln!(
            out,
            "Retained by --keep-last: {}",
            prune.retained_by_keep_last.len()
        );
    }
    if prune.apply {
        let _ = writeln!(
            out,
            "Deleted: {} ({} reclaimed)",
            prune.deleted.len(),
            human_bytes(prune.reclaimed_bytes)
        );
    }
    if !prune.deletion_failed.is_empty() {
        let _ = writeln!(out, "Deletion failed: {}", prune.deletion_failed.len());
        for c in &prune.deletion_failed {
            let _ = writeln!(out, "  - {} ({})", c.id, human_bytes(c.bytes));
        }
    }
    if prune.blocked {
        let _ = writeln!(
            out,
            "\nBLOCKED: at least one matching sweep was skipped as not confirmed idle, or failed to delete."
        );
    }
}

fn lifecycle_label(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Complete => "complete",
        LifecycleState::Interrupted => "interrupted",
        LifecycleState::Incomplete => "incomplete",
        LifecycleState::InProgress => "in_progress",
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_idx = 0;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{bytes} {}", UNITS[unit_idx])
    } else {
        format!("{value:.2} {}", UNITS[unit_idx])
    }
}

// ── pure helpers (unit-testable) ────────────────────────────────────────────

/// Classify a sweep's lifecycle state.
///
/// `has_fresh_partial_checkpoint` takes priority over everything else: a
/// sweep with a recently-touched partial checkpoint is being actively
/// written to right now (by `mini`/`bench swebench`/`bench retry`), even if
/// a stale `results.json` from an earlier run already exists in the same
/// directory. Never safe to delete.
#[must_use]
pub fn classify_lifecycle(
    has_results_json: bool,
    has_partial_checkpoint: bool,
    has_fresh_partial_checkpoint: bool,
) -> LifecycleState {
    if has_fresh_partial_checkpoint {
        LifecycleState::InProgress
    } else if has_results_json {
        LifecycleState::Complete
    } else if has_partial_checkpoint {
        LifecycleState::Interrupted
    } else {
        LifecycleState::Incomplete
    }
}

/// Classify a single file into an artifact category by name, given whether
/// it is a partial trajectory checkpoint (`None` when the file could not be
/// parsed, or is not a `.traj.json` file at all).
///
/// Matching is intentionally case-sensitive: every artifact filename this
/// codebase writes is lowercase by convention (see `docs/spec-disk-usage.md`).
#[must_use]
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub fn classify_category(file_name: &str, traj_partial: Option<bool>) -> Category {
    if file_name.ends_with(".traj.json") {
        return if traj_partial == Some(true) {
            Category::PartialCheckpoints
        } else {
            Category::Trajectories
        };
    }
    if file_name == "evaluation.json" || file_name.ends_with(".evaluation.json") {
        return Category::Evaluation;
    }
    if file_name.ends_with(".tar.gz")
        || file_name.ends_with(".tgz")
        || file_name.ends_with(".zip")
        || file_name == BUNDLE_MANIFEST_PATH
    {
        return Category::Bundles;
    }
    if file_name.ends_with(".log") || file_name == "events.jsonl" {
        return Category::Logs;
    }
    Category::Other
}

/// Parse an `--older-than`/age selector: `<N>d`, `<N>h`, `<N>m`, `<N>s`, or a
/// plain integer (seconds).
pub fn parse_age_selector(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration (expected e.g. `7d`, `24h`, `30m`, `45s`, or a plain integer number of seconds)".to_owned());
    }
    let last = s.chars().last().unwrap_or(' ');
    let (num_part, multiplier) = if last.is_ascii_alphabetic() {
        let multiplier = match last {
            's' => 1u64,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            other => {
                return Err(format!(
                    "invalid duration suffix `{other}` in `{s}` (expected s/m/h/d)"
                ));
            }
        };
        (&s[..s.len() - 1], multiplier)
    } else {
        (s, 1u64)
    };
    let n: u64 = num_part.parse().map_err(|_| {
        format!(
            "invalid duration `{s}` (expected e.g. `7d`, `24h`, `30m`, `45s`, or a plain integer number of seconds)"
        )
    })?;
    Ok(Duration::from_secs(n.saturating_mul(multiplier)))
}

/// Independent recursive byte-size walk used to cross-check the categorized
/// scan's `total_bytes` (see `docs/spec-disk-usage.md`). Skips symlinks —
/// the same convention `scan_sweep` uses — so the two totals reconcile
/// exactly.
#[must_use]
pub fn recursive_size_walk(root: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            total += recursive_size_walk(&p);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// Whether `sweep` matches the non-ranking selectors (`--older-than`,
/// `--incomplete-only`). Deliberately excludes `--keep-last`, which ranks a
/// sweep against every *other* sweep's recency rather than testing a static
/// property of the sweep itself — not something a single-sweep re-check (see
/// `build_prune_report`) can re-evaluate in isolation. Also excludes the
/// `in_progress` check itself: callers combine this with a separate
/// lifecycle-state check so the two reasons a sweep is skipped (never
/// matched vs. no longer matches after a state change) can be told apart.
fn matches_age_and_incomplete_selectors(sweep: &SweepReport, selectors: &PruneSelectors) -> bool {
    let matches_age = selectors
        .older_than_secs
        .is_none_or(|secs| sweep.age_seconds >= secs);
    let matches_incomplete = !selectors.incomplete_only
        || matches!(
            sweep.lifecycle_state,
            LifecycleState::Incomplete | LifecycleState::Interrupted
        );
    matches_age && matches_incomplete
}

pub struct PruneEvaluation {
    pub candidates: Vec<PruneEntry>,
    pub protected: Vec<PruneEntry>,
    pub retained: Vec<PruneEntry>,
}

/// Pure selector evaluation over already-scanned sweeps. An in-progress
/// sweep is never a candidate: if it otherwise matches every given
/// selector it is reported `protected` instead (driving the blocked exit
/// code); if it never matched, it is silently excluded like any other
/// non-matching sweep.
#[must_use]
pub fn evaluate_prune_candidates(
    sweeps: &[SweepReport],
    selectors: &PruneSelectors,
) -> PruneEvaluation {
    let mut by_recency: Vec<&SweepReport> = sweeps.iter().collect();
    by_recency.sort_by(|a, b| {
        (b.last_modified_unix, b.last_modified_nanos)
            .cmp(&(a.last_modified_unix, a.last_modified_nanos))
            .then_with(|| a.id.cmp(&b.id))
    });
    let retained_ids: HashSet<&str> = match selectors.keep_last {
        Some(n) => by_recency.iter().take(n).map(|s| s.id.as_str()).collect(),
        None => HashSet::new(),
    };

    let mut candidates = Vec::new();
    let mut protected = Vec::new();
    let mut retained = Vec::new();

    for s in sweeps {
        let kept_by_keep_last = retained_ids.contains(s.id.as_str());

        if !matches_age_and_incomplete_selectors(s, selectors) {
            continue;
        }
        if kept_by_keep_last {
            retained.push(entry_for(s));
            continue;
        }
        if s.lifecycle_state == LifecycleState::InProgress {
            protected.push(entry_for(s));
        } else {
            candidates.push(entry_for(s));
        }
    }

    let by_bytes_desc_id_asc =
        |a: &PruneEntry, b: &PruneEntry| b.bytes.cmp(&a.bytes).then_with(|| a.id.cmp(&b.id));
    candidates.sort_by(by_bytes_desc_id_asc);
    protected.sort_by(by_bytes_desc_id_asc);
    retained.sort_by(by_bytes_desc_id_asc);

    PruneEvaluation {
        candidates,
        protected,
        retained,
    }
}

fn entry_for(s: &SweepReport) -> PruneEntry {
    PruneEntry {
        id: s.id.clone(),
        path: s.path.clone(),
        path_buf: s.path_buf.clone(),
        bytes: s.total_bytes,
        lifecycle_state: s.lifecycle_state,
        last_modified_unix: s.last_modified_unix,
        last_modified_nanos: s.last_modified_nanos,
    }
}

// ── internals ────────────────────────────────────────────────────────────────

fn scan_sweep(path: &Path, now: SystemTime, in_progress_window: Duration) -> SweepReport {
    // Lossy on non-UTF-8 names, but unlike a blanket `unwrap_or_default()`
    // this doesn't collapse every non-UTF-8 name to the same empty string —
    // two differently-named non-UTF-8 sweeps stay distinguishable for the
    // `--keep-last` retained-set membership check in `evaluate_prune_candidates`.
    let id = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut categories = CategoryBytes::default();
    let mut total_bytes = 0u64;
    // Only seeded from file mtimes; falls back to the sweep directory's own
    // mtime below when the sweep contains no files at all. Must NOT be
    // seeded from the directory's mtime up front — that mtime reflects
    // directory creation time, not content activity, and would shadow
    // legitimately old file mtimes (breaking --older-than / age selectors).
    let mut newest_file_mtime: Option<SystemTime> = None;
    let mut has_partial_checkpoint = false;
    let mut has_fresh_partial_checkpoint = false;

    walk_sweep(path, &mut |file_path, size, mtime| {
        total_bytes += size;
        if newest_file_mtime.is_none_or(|current| mtime > current) {
            newest_file_mtime = Some(mtime);
        }
        let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let traj_partial = if file_name.ends_with(".traj.json") {
            parse_traj_partial(file_path)
        } else {
            None
        };
        let category = classify_category(file_name, traj_partial);
        categories.add(category, size);

        if category == Category::PartialCheckpoints {
            has_partial_checkpoint = true;
            let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
            if age <= in_progress_window {
                has_fresh_partial_checkpoint = true;
            }
        }
    });

    let last_modified = newest_file_mtime.unwrap_or_else(|| fs_mtime(path).unwrap_or(UNIX_EPOCH));

    // Must agree with walk_sweep's symlink-skipping convention: a symlinked
    // `results.json` contributes zero bytes to the byte walk (symlinks are
    // skipped there), so it must not independently flip lifecycle to
    // `complete` via a stat call that silently follows the symlink.
    let results_path = path.join("results.json");
    let has_results_json = !results_path.is_symlink() && results_path.is_file();
    let lifecycle_state = classify_lifecycle(
        has_results_json,
        has_partial_checkpoint,
        has_fresh_partial_checkpoint,
    );

    let age_seconds = if let Ok(d) = now.duration_since(last_modified) {
        d.as_secs()
    } else {
        // A future mtime (clock skew, NFS/container drift, a stray
        // `touch -d future`) would otherwise clamp age to 0 and silently
        // make this sweep un-prunable via --older-than forever.
        eprintln!(
            "warning: bench du: {} has a file with an mtime in the future relative to this host's clock; treating age as 0s (clock skew?)",
            path.display()
        );
        0
    };
    let since_epoch = last_modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let last_modified_unix = since_epoch.as_secs();
    let last_modified_nanos = since_epoch.subsec_nanos();

    SweepReport {
        id,
        path: path.display().to_string(),
        path_buf: path.to_path_buf(),
        total_bytes,
        lifecycle_state,
        last_modified: rfc3339(last_modified),
        last_modified_unix,
        last_modified_nanos,
        age_seconds,
        categories,
    }
}

/// Recursively visits every regular file under `dir` (skipping symlinks),
/// invoking `visit(path, size_bytes, mtime)` for each. Unreadable
/// subdirectories are skipped silently via the shared
/// `agent_runs::read_dir_or_empty` primitive, the same one
/// `agent_runs::walk_children` uses.
fn walk_sweep(dir: &Path, visit: &mut impl FnMut(&Path, u64, SystemTime)) {
    for entry in crate::run::agent_runs::read_dir_or_empty(dir) {
        let p = entry.path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            walk_sweep(&p, visit);
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        visit(&p, meta.len(), mtime);
    }
}

/// Deserializes only the `info.partial` field via a buffered reader instead
/// of reading the whole file into a `String` and building a generic
/// `serde_json::Value` DOM — trajectory files can carry large message
/// histories and tool outputs that this check has no use for.
fn parse_traj_partial(path: &Path) -> Option<bool> {
    #[derive(Deserialize)]
    struct TrajInfo {
        info: Option<Info>,
    }
    #[derive(Deserialize)]
    struct Info {
        partial: Option<bool>,
    }

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let traj: TrajInfo = serde_json::from_reader(reader).ok()?;
    traj.info?.partial
}

fn fs_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    chrono::DateTime::<chrono::Utc>::from_timestamp(i64::try_from(secs).unwrap_or(i64::MAX), 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
