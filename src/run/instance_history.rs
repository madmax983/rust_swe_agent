//! `bench instance-history`: join historical sweeps on instance_id and report
//! per-instance resolution history, stability class, and flip provenance.
//!
//! Read-only over existing artifacts: no model, env, or runtime changes.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::compare::load_sweep;

// ── output format ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryFormat {
    Text,
    Json,
}

impl std::str::FromStr for HistoryFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!("unknown format {other:?}; expected text or json")),
        }
    }
}

// ── stability classification ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StabilityClass {
    StableWin,
    StableLoss,
    Flipper,
    UnstableMinorityWin,
    UnstableMinorityLoss,
}

impl StabilityClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StableWin => "stable_win",
            Self::StableLoss => "stable_loss",
            Self::Flipper => "flipper",
            Self::UnstableMinorityWin => "unstable_minority_win",
            Self::UnstableMinorityLoss => "unstable_minority_loss",
        }
    }
}

impl std::fmt::Display for StabilityClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify an instance by its resolved_count / total_runs ratio.
///
/// With the default `stable_threshold = 1.0`:
///   - N/N (rate=1.0) → `stable_win`
///   - 0/N (rate=0.0) → `stable_loss`
///   - anything in between → `flipper`
///
/// With a relaxed threshold T < 1.0, the middle band is subdivided:
///   - rate > T          → `stable_win`
///   - rate < (1-T)      → `stable_loss`
///   - rate > 0.5        → `unstable_minority_win`
///   - rate < 0.5        → `unstable_minority_loss`
///   - rate == 0.5 exactly → `flipper`
pub fn classify_stability(resolved: u32, total: u32, stable_threshold: f64) -> StabilityClass {
    if total == 0 || resolved == 0 {
        return StabilityClass::StableLoss;
    }
    if resolved >= total {
        return StabilityClass::StableWin;
    }
    // 0 < resolved < total: not perfectly stable at either extreme
    if stable_threshold >= 1.0 {
        // Default: everything between 0/N and N/N is a flipper
        return StabilityClass::Flipper;
    }
    let rate = resolved as f64 / total as f64;
    if rate > stable_threshold {
        StabilityClass::StableWin
    } else if rate < 1.0 - stable_threshold {
        StabilityClass::StableLoss
    } else if rate > 0.5 {
        StabilityClass::UnstableMinorityWin
    } else if rate < 0.5 {
        StabilityClass::UnstableMinorityLoss
    } else {
        StabilityClass::Flipper
    }
}

// ── per-sweep outcome record ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepOutcome {
    pub sweep_id: String,
    pub sweep_path: String,
    pub finished_at: Option<String>,
    pub resolved: bool,
    pub errored: bool,
}

// ── flip event ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlipEvent {
    pub from_sweep: String,
    pub to_sweep: String,
    /// `"win→loss"` or `"loss→win"`.
    pub direction: String,
}

// ── per-instance row ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceHistoryRow {
    pub instance_id: String,
    pub resolved_count: u32,
    pub total_runs: u32,
    pub resolved_rate: f64,
    pub stability_class: StabilityClass,
    /// Ordered by `finished_at` ascending (ties broken by `sweep_id` lex).
    pub sweep_outcomes: Vec<SweepOutcome>,
    pub flip_events: Vec<FlipEvent>,
    pub last_flip: Option<FlipEvent>,
}

// ── partial coverage ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialCoverage {
    pub instance_id: String,
    pub sweeps_seen_in: Vec<String>,
    pub sweeps_missing_from: Vec<String>,
}

// ── stability counts ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StabilityCounts {
    pub stable_win: usize,
    pub stable_loss: usize,
    pub flipper: usize,
    pub unstable_minority_win: usize,
    pub unstable_minority_loss: usize,
}

impl StabilityCounts {
    fn increment(&mut self, class: StabilityClass) {
        match class {
            StabilityClass::StableWin => self.stable_win += 1,
            StabilityClass::StableLoss => self.stable_loss += 1,
            StabilityClass::Flipper => self.flipper += 1,
            StabilityClass::UnstableMinorityWin => self.unstable_minority_win += 1,
            StabilityClass::UnstableMinorityLoss => self.unstable_minority_loss += 1,
        }
    }
}

// ── top-level report ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceHistoryReport {
    pub generated_at: String,
    pub tool_version: String,
    pub sweep_count: usize,
    pub intersection_size: usize,
    pub stability_counts: StabilityCounts,
    /// `flipper_count / intersection_size`, or 0 when intersection is empty.
    pub flipper_share: f64,
    /// Content hash of sorted intersection `instance_id` list.
    pub dataset_signature: String,
    pub partial_coverage_count: usize,
    pub skipped_sweeps: Vec<String>,
    pub instances: Vec<InstanceHistoryRow>,
    pub partial_coverage: Vec<PartialCoverage>,
}

// ── args ──────────────────────────────────────────────────────────────────────

pub struct InstanceHistoryArgs {
    pub sweeps: Vec<PathBuf>,
    pub stable_threshold: f64,
    pub require_full_coverage: bool,
    pub max_partial_share: Option<f64>,
    pub format: HistoryFormat,
    pub output: PathBuf,
    pub top: usize,
    pub focus: bool,
    pub class_filter: Option<StabilityClass>,
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn dataset_signature(ids: &BTreeSet<&str>) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for id in ids {
        id.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn compute_flip_events(outcomes: &[SweepOutcome]) -> Vec<FlipEvent> {
    let mut events = Vec::new();
    for window in outcomes.windows(2) {
        let prev = &window[0];
        let next = &window[1];
        if prev.resolved != next.resolved {
            let direction = if prev.resolved {
                "win→loss".to_owned()
            } else {
                "loss→win".to_owned()
            };
            events.push(FlipEvent {
                from_sweep: prev.sweep_id.clone(),
                to_sweep: next.sweep_id.clone(),
                direction,
            });
        }
    }
    events
}

// ── compute ───────────────────────────────────────────────────────────────────

pub fn compute(args: &InstanceHistoryArgs) -> Result<InstanceHistoryReport, Error> {
    // ── load sweeps, skipping those without results.json ─────────────────────
    struct LoadedEntry {
        sweep_id: String,
        sweep_path: String,
        finished_at: Option<String>,
        instances: HashMap<String, bool>, // instance_id → resolved
    }

    let mut loaded: Vec<LoadedEntry> = Vec::new();
    let mut skipped_sweeps: Vec<String> = Vec::new();

    for sweep_path in &args.sweeps {
        let results_path = sweep_path.join("results.json");
        if !results_path.exists() {
            skipped_sweeps.push(sweep_path.display().to_string());
            continue;
        }
        match load_sweep(sweep_path) {
            Ok(ls) => {
                let finished_at = ls
                    .manifest
                    .as_ref()
                    .and_then(|m| m.runtime.finished_at_utc.clone());
                let sweep_id = sweep_path
                    .file_name()
                    .map_or_else(|| sweep_path.display().to_string(), |n| n.to_string_lossy().into_owned());
                let instances: HashMap<String, bool> = ls
                    .instances
                    .iter()
                    .map(|(id, r)| (id.clone(), r.resolved_count > 0))
                    .collect();
                loaded.push(LoadedEntry {
                    sweep_id,
                    sweep_path: sweep_path.display().to_string(),
                    finished_at,
                    instances,
                });
            }
            Err(_) => {
                skipped_sweeps.push(sweep_path.display().to_string());
            }
        }
    }

    if loaded.len() < 2 {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "instance-history: need ≥ 2 valid sweeps (each with a readable results.json)".into(),
        )));
    }

    // ── compute intersection ──────────────────────────────────────────────────
    let all_ids: BTreeSet<String> = loaded
        .iter()
        .flat_map(|e| e.instances.keys().cloned())
        .collect();

    let sweep_count = loaded.len();

    let mut intersection_ids: BTreeSet<&str> = all_ids.iter().map(String::as_str).collect();
    for entry in &loaded {
        intersection_ids.retain(|id| entry.instances.contains_key(*id));
    }

    // ── partial coverage ──────────────────────────────────────────────────────
    let partial_ids: BTreeSet<&str> = all_ids
        .iter()
        .map(String::as_str)
        .filter(|id| !intersection_ids.contains(id))
        .collect();

    let mut partial_coverage: Vec<PartialCoverage> = partial_ids
        .iter()
        .map(|id| {
            let seen: Vec<String> = loaded
                .iter()
                .filter(|e| e.instances.contains_key(*id))
                .map(|e| e.sweep_id.clone())
                .collect();
            let missing: Vec<String> = loaded
                .iter()
                .filter(|e| !e.instances.contains_key(*id))
                .map(|e| e.sweep_id.clone())
                .collect();
            PartialCoverage {
                instance_id: (*id).to_owned(),
                sweeps_seen_in: seen,
                sweeps_missing_from: missing,
            }
        })
        .collect();
    partial_coverage.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    // ── check --require-full-coverage ─────────────────────────────────────────
    if args.require_full_coverage {
        let total_unique = all_ids.len();
        let partial_share = if total_unique == 0 {
            0.0
        } else {
            partial_ids.len() as f64 / total_unique as f64
        };
        let max_share = args.max_partial_share.unwrap_or(0.0);
        if partial_share > max_share || intersection_ids.is_empty() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "instance-history: --require-full-coverage failed: {:.1}% partial (limit {:.1}%)",
                partial_share * 100.0,
                max_share * 100.0,
            ))));
        }
    }

    // ── build per-instance rows ───────────────────────────────────────────────
    // Sort loaded sweeps by finished_at ascending, tie-break by sweep_id lex.
    let mut sweep_order: Vec<usize> = (0..loaded.len()).collect();
    sweep_order.sort_by(|&a, &b| {
        let fa = loaded[a].finished_at.as_deref().unwrap_or("");
        let fb = loaded[b].finished_at.as_deref().unwrap_or("");
        fa.cmp(fb).then_with(|| loaded[a].sweep_id.cmp(&loaded[b].sweep_id))
    });

    let mut rows: Vec<InstanceHistoryRow> = intersection_ids
        .iter()
        .map(|&id| {
            let mut sweep_outcomes: Vec<SweepOutcome> = sweep_order
                .iter()
                .map(|&idx| {
                    let e = &loaded[idx];
                    let resolved = e.instances.get(id).copied().unwrap_or(false);
                    SweepOutcome {
                        sweep_id: e.sweep_id.clone(),
                        sweep_path: e.sweep_path.clone(),
                        finished_at: e.finished_at.clone(),
                        resolved,
                        errored: !resolved,
                    }
                })
                .collect();

            // Ensure deterministic ordering within ties.
            sweep_outcomes.sort_by(|a, b| {
                let fa = a.finished_at.as_deref().unwrap_or("");
                let fb = b.finished_at.as_deref().unwrap_or("");
                fa.cmp(fb).then_with(|| a.sweep_id.cmp(&b.sweep_id))
            });

            let resolved_count = sweep_outcomes.iter().filter(|o| o.resolved).count() as u32;
            let total_runs = sweep_outcomes.len() as u32;
            let resolved_rate = if total_runs == 0 {
                0.0
            } else {
                resolved_count as f64 / total_runs as f64
            };
            let stability_class =
                classify_stability(resolved_count, total_runs, args.stable_threshold);
            let flip_events = compute_flip_events(&sweep_outcomes);
            let last_flip = flip_events.last().cloned();

            InstanceHistoryRow {
                instance_id: id.to_owned(),
                resolved_count,
                total_runs,
                resolved_rate,
                stability_class,
                sweep_outcomes,
                flip_events,
                last_flip,
            }
        })
        .collect();

    // ── rank rows by operator value ───────────────────────────────────────────
    // Order: flippers (most-balanced first, then by last-flip recency), then
    // unstable_minority_win, unstable_minority_loss, stable_loss, stable_win.
    fn class_rank(c: StabilityClass) -> u8 {
        match c {
            StabilityClass::Flipper => 0,
            StabilityClass::UnstableMinorityWin => 1,
            StabilityClass::UnstableMinorityLoss => 2,
            StabilityClass::StableLoss => 3,
            StabilityClass::StableWin => 4,
        }
    }

    rows.sort_by(|a, b| {
        let ra = class_rank(a.stability_class);
        let rb = class_rank(b.stability_class);
        ra.cmp(&rb)
            .then_with(|| {
                // Within flippers: most-balanced (|rate - 0.5| ascending) first
                let ba = (a.resolved_rate - 0.5_f64).abs();
                let bb = (b.resolved_rate - 0.5_f64).abs();
                ba.partial_cmp(&bb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    // ── compute stability counts ──────────────────────────────────────────────
    let mut stability_counts = StabilityCounts::default();
    for row in &rows {
        stability_counts.increment(row.stability_class);
    }

    let intersection_size = intersection_ids.len();
    let flipper_share = if intersection_size == 0 {
        0.0
    } else {
        stability_counts.flipper as f64 / intersection_size as f64
    };

    let dataset_signature = dataset_signature(&intersection_ids);

    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    Ok(InstanceHistoryReport {
        generated_at,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        sweep_count,
        intersection_size,
        stability_counts,
        flipper_share,
        dataset_signature,
        partial_coverage_count: partial_coverage.len(),
        skipped_sweeps,
        instances: rows,
        partial_coverage,
    })
}

// ── text rendering ────────────────────────────────────────────────────────────

pub fn render_text(
    report: &InstanceHistoryReport,
    top: usize,
    focus: bool,
    class_filter: Option<StabilityClass>,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "instance-history: {sweeps} sweeps, {size} instances in intersection",
        sweeps = report.sweep_count,
        size = report.intersection_size,
    );
    let _ = writeln!(
        out,
        "  stable_win={sw}  stable_loss={sl}  flipper={f}  minority_win={mw}  minority_loss={ml}",
        sw = report.stability_counts.stable_win,
        sl = report.stability_counts.stable_loss,
        f = report.stability_counts.flipper,
        mw = report.stability_counts.unstable_minority_win,
        ml = report.stability_counts.unstable_minority_loss,
    );
    let _ = writeln!(out, "  flipper_share={:.1}%", report.flipper_share * 100.0);
    if !report.skipped_sweeps.is_empty() {
        let _ = writeln!(out, "  skipped: {}", report.skipped_sweeps.join(", "));
    }
    let _ = writeln!(out);

    let filter_class = if focus { Some(StabilityClass::Flipper) } else { class_filter };

    let rows: Vec<&InstanceHistoryRow> = report
        .instances
        .iter()
        .filter(|r| filter_class.is_none() || filter_class == Some(r.stability_class))
        .take(top)
        .collect();

    if rows.is_empty() {
        let _ = writeln!(out, "(no instances match the current filter)");
        return out;
    }

    let _ = writeln!(
        out,
        "{:<50}  {:<22}  {:>7}  {:>6}  {}",
        "instance_id", "stability_class", "resolved", "rate", "last_flip"
    );
    let _ = writeln!(out, "{}", "-".repeat(110));

    for row in &rows {
        let last_flip = row
            .last_flip
            .as_ref()
            .map_or_else(|| "-".to_owned(), |f| f.direction.clone());
        let _ = writeln!(
            out,
            "{:<50}  {:<22}  {:>4}/{:<2}  {:>5.1}%  {}",
            truncate(&row.instance_id, 50),
            row.stability_class,
            row.resolved_count,
            row.total_runs,
            row.resolved_rate * 100.0,
            last_flip,
        );
    }

    out
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

// ── write output ──────────────────────────────────────────────────────────────

pub fn write_output(report: &InstanceHistoryReport, path: &Path) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(report)?;
    if path.to_str() == Some("-") {
        println!("{json}");
    } else {
        std::fs::write(path, &json)?;
    }
    Ok(())
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_default_threshold() {
        assert_eq!(classify_stability(10, 10, 1.0), StabilityClass::StableWin);
        assert_eq!(classify_stability(0, 10, 1.0), StabilityClass::StableLoss);
        assert_eq!(classify_stability(7, 10, 1.0), StabilityClass::Flipper);
        assert_eq!(classify_stability(1, 10, 1.0), StabilityClass::Flipper);
        assert_eq!(classify_stability(9, 10, 1.0), StabilityClass::Flipper);
    }

    #[test]
    fn classify_relaxed_threshold() {
        // rate=0.7 at T=0.9: 0.7 not >0.9, 0.7 not <0.1, 0.7 > 0.5 → minority_win
        assert_eq!(
            classify_stability(7, 10, 0.9),
            StabilityClass::UnstableMinorityWin
        );
        // rate=0.9 (9/10) at T=0.9: 0.9 not > 0.9 → minority_win
        assert_eq!(
            classify_stability(9, 10, 0.9),
            StabilityClass::UnstableMinorityWin
        );
        // rate=1.0 at T=0.9 → StableWin (N/N always wins)
        assert_eq!(classify_stability(10, 10, 0.9), StabilityClass::StableWin);
        // rate=0.0 at T=0.9 → StableLoss (0/N always loses)
        assert_eq!(classify_stability(0, 10, 0.9), StabilityClass::StableLoss);
        // rate=0.3 at T=0.9: 0.3 not > 0.9, 0.3 not < 0.1, 0.3 < 0.5 → minority_loss
        assert_eq!(
            classify_stability(3, 10, 0.9),
            StabilityClass::UnstableMinorityLoss
        );
    }

    #[test]
    fn flip_events_ordering() {
        let outcomes = vec![
            SweepOutcome {
                sweep_id: "a".into(),
                sweep_path: "a".into(),
                finished_at: Some("2026-05-01T00:00:00Z".into()),
                resolved: true,
                errored: false,
            },
            SweepOutcome {
                sweep_id: "b".into(),
                sweep_path: "b".into(),
                finished_at: Some("2026-05-02T00:00:00Z".into()),
                resolved: false,
                errored: true,
            },
            SweepOutcome {
                sweep_id: "c".into(),
                sweep_path: "c".into(),
                finished_at: Some("2026-05-03T00:00:00Z".into()),
                resolved: true,
                errored: false,
            },
        ];
        let flips = compute_flip_events(&outcomes);
        assert_eq!(flips.len(), 2);
        assert_eq!(flips[0].direction, "win→loss");
        assert_eq!(flips[1].direction, "loss→win");
    }
}
