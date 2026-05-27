//! `bench self-check` — score the agent's own test verdict against the evaluator.
//!
//! Reads existing sweep artifacts (`*.traj.json` + `evaluation.json`) and computes
//! a 3×2 confusion matrix, precision/recall, Brier score, and calibration delta.
//! Zero-cost: no model calls, no container calls.
//!
//! # Exit codes
//! * 0 — success
//! * 2 — `evaluation.json` missing (`Error::Config` → `UsageError`)
//! * 3 — trajectory predates the `tests_run_before_submit` field added in #46
//!        (`Error::Preflight` → `PreflightFailure`)
//! * 1 — internal I/O or JSON parse error

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error};

// ── public arg struct ─────────────────────────────────────────────────────────

/// Arguments for `bench self-check`.
#[derive(Debug, Clone)]
pub struct SelfCheckArgs {
    /// Completed sweep directory containing `*.traj.json` files and
    /// `evaluation.json`.
    pub sweep_dir: PathBuf,
    /// Output format: `"text"` (default) or `"json"`.
    pub format: String,
    /// Show this many false-positive / false-negative IDs in text output.
    /// `0` suppresses the lists. The `SelfCheckReport` always stores all IDs.
    pub list: usize,
    /// When `true`, populate `by_repo` with per-repository `ConfusionCounts`.
    pub by_repo: bool,
}

// ── report structs ────────────────────────────────────────────────────────────

/// 3×2 confusion matrix.
///
/// Rows represent the agent's self-verdict (`last_tests_passed`), columns
/// represent the external evaluator verdict (`resolved`).
///
/// ```text
///                      | resolved=true | resolved=false
/// tests_passed = true  |  passed_resolved (TP) | passed_unresolved (FP)
/// tests_passed = false |  failed_resolved (FN) | failed_unresolved (TN)
/// tests_passed = None  |  none_resolved        | none_unresolved
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ConfusionCounts {
    /// True positives: agent said "passed", evaluator said "resolved".
    pub passed_resolved: u32,
    /// False positives: agent said "passed", evaluator said "not resolved".
    pub passed_unresolved: u32,
    /// False negatives: agent said "failed", evaluator said "resolved".
    pub failed_resolved: u32,
    /// True negatives: agent said "failed", evaluator said "not resolved".
    pub failed_unresolved: u32,
    /// Agent reported no test result; evaluator said "resolved".
    pub none_resolved: u32,
    /// Agent reported no test result; evaluator said "not resolved".
    pub none_unresolved: u32,
}

/// Calibration metrics derived from the confusion matrix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SelfCheckMetrics {
    /// Fraction of "passed" calls that were actually resolved.
    /// `None` when the denominator (TP + FP) is zero.
    pub precision: Option<f64>,
    /// Fraction of resolved instances that were called "passed".
    /// `None` when the denominator (TP + FN) is zero.
    pub recall: Option<f64>,
    /// Mean-squared-error treating `last_tests_passed` as a probability
    /// (`true=1.0`, `false=0.0`, `None=0.5`) compared to `resolved`.
    /// Rounded to 3 decimal places.
    pub brier_score: f64,
}

/// Top-level self-check report — the return value of [`run`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfCheckReport {
    /// Schema identifier for machine-readable consumers.
    pub schema: String,
    /// 3×2 confusion matrix across all instances.
    pub confusion: ConfusionCounts,
    /// Derived calibration metrics.
    pub metrics: SelfCheckMetrics,
    /// Instance IDs where `last_tests_passed=true` but `resolved=false`
    /// (false positives), sorted lexicographically.
    pub false_positives: Vec<String>,
    /// Instance IDs where `last_tests_passed=false` but `resolved=true`
    /// (false negatives), sorted lexicographically.
    pub false_negatives: Vec<String>,
    /// Overall resolved rate across all instances
    /// (`total_resolved / total_instances`).
    pub base_rate: f64,
    /// Number of instances excluded from precision/recall because
    /// `last_tests_passed` was `None`.
    pub n_excluded_none: u32,
    /// `precision − base_rate`. Positive means the agent's "passed" signal
    /// is more reliable than a random baseline.
    /// `None` when precision is undefined.
    pub calibration_delta: Option<f64>,
    /// Per-repository breakdown (only populated when `args.by_repo = true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_repo: Option<BTreeMap<String, ConfusionCounts>>,
}

// ── main entry point ──────────────────────────────────────────────────────────

/// Run the self-check and return a [`SelfCheckReport`].
///
/// # Errors
/// * `Error::Config` (exit 2) — `evaluation.json` is absent or malformed.
/// * `Error::Preflight` (exit 3) — a trajectory predates the `tests_run_before_submit`
///   field introduced in harness issue #46.
/// * `Error::Io` / `Error::Json` (exit 1) — unexpected I/O or parse failure.
pub fn run(args: &SelfCheckArgs) -> Result<SelfCheckReport, Error> {
    // ── 1. Load evaluation.json ───────────────────────────────────────────────
    let eval_path = args.sweep_dir.join("evaluation.json");
    if !eval_path.exists() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "evaluation.json not found in {}; run `bench evaluate` first to produce it",
            args.sweep_dir.display()
        ))));
    }

    let eval_text = std::fs::read_to_string(&eval_path)?;
    let eval_json: serde_json::Value = serde_json::from_str(&eval_text)?;

    let instances = eval_json["instances"].as_array().ok_or_else(|| {
        Error::Config(ConfigError::Invalid(
            "evaluation.json is missing the 'instances' array".into(),
        ))
    })?;

    // ── 2. Iterate instances ──────────────────────────────────────────────────
    let mut confusion = ConfusionCounts::default();
    let mut false_positives: Vec<String> = Vec::new();
    let mut false_negatives: Vec<String> = Vec::new();
    let mut brier_sum = 0.0_f64;
    let total = instances.len();
    let mut total_resolved = 0_u32;
    let mut by_repo_map: BTreeMap<String, ConfusionCounts> = BTreeMap::new();

    for inst in instances {
        let instance_id = inst["instance_id"].as_str().ok_or_else(|| {
            Error::Config(ConfigError::Invalid(
                "an instance in evaluation.json is missing 'instance_id'".into(),
            ))
        })?;

        let resolved = inst["resolved"].as_bool().ok_or_else(|| {
            Error::Config(ConfigError::Invalid(format!(
                "instance '{instance_id}' in evaluation.json is missing 'resolved'"
            )))
        })?;

        if resolved {
            total_resolved += 1;
        }

        // ── 3. Load trajectory and check schema ───────────────────────────────
        let traj_path = find_trajectory_path(&args.sweep_dir, instance_id)?;
        let (tests_run_field_present, last_tests_passed) = load_trajectory_info(&traj_path)?;

        if !tests_run_field_present {
            return Err(Error::Preflight(format!(
                "trajectory for '{instance_id}' predates #46: the 'tests_run_before_submit' \
                 field is absent; self-check requires sweeps run with harness ≥ #46"
            )));
        }

        // ── 4. Brier score contribution ───────────────────────────────────────
        // f_i = agent's implied probability that the patch was correct:
        //   tests_passed=true  → 1.0
        //   tests_passed=false → 0.0
        //   tests_passed=None  → 0.5 (maximum-uncertainty prior)
        let f_i: f64 = match last_tests_passed {
            Some(true) => 1.0,
            Some(false) => 0.0,
            None => 0.5,
        };
        let y_i: f64 = if resolved { 1.0 } else { 0.0 };
        brier_sum += (f_i - y_i).powi(2);

        // ── 5. Confusion matrix cell ──────────────────────────────────────────
        let repo_key = parse_repo_from_instance_id(instance_id);

        match last_tests_passed {
            Some(true) => {
                if resolved {
                    confusion.passed_resolved += 1;
                } else {
                    confusion.passed_unresolved += 1;
                    false_positives.push(instance_id.to_owned());
                }
            }
            Some(false) => {
                if resolved {
                    confusion.failed_resolved += 1;
                    false_negatives.push(instance_id.to_owned());
                } else {
                    confusion.failed_unresolved += 1;
                }
            }
            None => {
                if resolved {
                    confusion.none_resolved += 1;
                } else {
                    confusion.none_unresolved += 1;
                }
            }
        }

        // ── 6. Per-repo cell (optional) ───────────────────────────────────────
        if args.by_repo {
            if let Some(repo) = repo_key {
                let entry = by_repo_map.entry(repo).or_default();
                match last_tests_passed {
                    Some(true) => {
                        if resolved {
                            entry.passed_resolved += 1;
                        } else {
                            entry.passed_unresolved += 1;
                        }
                    }
                    Some(false) => {
                        if resolved {
                            entry.failed_resolved += 1;
                        } else {
                            entry.failed_unresolved += 1;
                        }
                    }
                    None => {
                        if resolved {
                            entry.none_resolved += 1;
                        } else {
                            entry.none_unresolved += 1;
                        }
                    }
                }
            }
        }
    }

    // ── 7. Derive aggregate metrics ───────────────────────────────────────────
    false_positives.sort();
    false_negatives.sort();

    let tp = f64::from(confusion.passed_resolved);
    let fp = f64::from(confusion.passed_unresolved);
    let fn_ = f64::from(confusion.failed_resolved);

    let precision = if tp + fp > 0.0 {
        Some(tp / (tp + fp))
    } else {
        None
    };
    let recall = if tp + fn_ > 0.0 {
        Some(tp / (tp + fn_))
    } else {
        None
    };

    #[allow(clippy::cast_precision_loss)]
    let brier_score = if total > 0 {
        round3(brier_sum / total as f64)
    } else {
        0.0
    };

    #[allow(clippy::cast_precision_loss)]
    let base_rate = if total > 0 {
        f64::from(total_resolved) / total as f64
    } else {
        0.0
    };

    let calibration_delta = precision.map(|p| p - base_rate);
    let n_excluded_none = confusion.none_resolved + confusion.none_unresolved;

    let by_repo = if args.by_repo {
        Some(by_repo_map)
    } else {
        None
    };

    Ok(SelfCheckReport {
        schema: "bench-self-check/1".to_owned(),
        confusion,
        metrics: SelfCheckMetrics {
            precision,
            recall,
            brier_score,
        },
        false_positives,
        false_negatives,
        base_rate,
        n_excluded_none,
        calibration_delta,
        by_repo,
    })
}

// ── text renderer ─────────────────────────────────────────────────────────────

/// Render a [`SelfCheckReport`] as human-readable text.
///
/// `list_n` controls how many false-positive / false-negative instance IDs to
/// show. Pass `0` to suppress those sections entirely.
#[must_use]
pub fn render_text(report: &SelfCheckReport, list_n: usize) -> String {
    let mut out = String::new();

    out.push_str("=== Bench Self-Check: Agent Verdict vs. Evaluator ===\n\n");

    // ── Confusion matrix ──────────────────────────────────────────────────────
    out.push_str("Confusion Matrix (rows: last_tests_passed, cols: resolved)\n");
    out.push_str(
        "                    Resolved  Unresolved\n",
    );
    out.push_str(&format!(
        "  tests passed=true  {:>8}  {:>10}\n",
        report.confusion.passed_resolved, report.confusion.passed_unresolved
    ));
    out.push_str(&format!(
        "  tests passed=false {:>8}  {:>10}\n",
        report.confusion.failed_resolved, report.confusion.failed_unresolved
    ));
    out.push_str(&format!(
        "  tests passed=None  {:>8}  {:>10}\n",
        report.confusion.none_resolved, report.confusion.none_unresolved
    ));
    out.push('\n');

    // ── Metrics ───────────────────────────────────────────────────────────────
    out.push_str("Metrics:\n");
    match report.metrics.precision {
        Some(p) => out.push_str(&format!("  Precision:          {p:.3}\n")),
        None => out.push_str("  Precision:          N/A (no passed rows)\n"),
    }
    match report.metrics.recall {
        Some(r) => out.push_str(&format!("  Recall:             {r:.3}\n")),
        None => out.push_str("  Recall:             N/A (no resolved rows)\n"),
    }
    out.push_str(&format!(
        "  Brier score:        {:.3}\n",
        report.metrics.brier_score
    ));
    out.push_str(&format!(
        "  Base rate:          {:.3}\n",
        report.base_rate
    ));
    match report.calibration_delta {
        Some(d) => out.push_str(&format!("  Calibration delta:  {d:+.3}\n")),
        None => out.push_str("  Calibration delta:  N/A\n"),
    }
    out.push_str(&format!(
        "  Excluded (None):    {}\n",
        report.n_excluded_none
    ));
    out.push('\n');

    // ── False positives ───────────────────────────────────────────────────────
    if list_n > 0 && !report.false_positives.is_empty() {
        let shown = report.false_positives.len().min(list_n);
        out.push_str(&format!(
            "False positives (tests_passed=true, resolved=false) — first {shown}:\n"
        ));
        for id in report.false_positives.iter().take(list_n) {
            out.push_str(&format!("  {id}\n"));
        }
        out.push('\n');
    }

    // ── False negatives ───────────────────────────────────────────────────────
    if list_n > 0 && !report.false_negatives.is_empty() {
        let shown = report.false_negatives.len().min(list_n);
        out.push_str(&format!(
            "False negatives (tests_passed=false, resolved=true) — first {shown}:\n"
        ));
        for id in report.false_negatives.iter().take(list_n) {
            out.push_str(&format!("  {id}\n"));
        }
        out.push('\n');
    }

    // ── Per-repository breakdown ──────────────────────────────────────────────
    if let Some(by_repo) = &report.by_repo {
        out.push_str("Per-repository breakdown:\n");
        for (repo, c) in by_repo {
            out.push_str(&format!(
                "  {repo}: TP={} FP={} FN={} TN={} None+R={} None+U={}\n",
                c.passed_resolved,
                c.passed_unresolved,
                c.failed_resolved,
                c.failed_unresolved,
                c.none_resolved,
                c.none_unresolved
            ));
        }
    }

    out
}

// ── private helpers ───────────────────────────────────────────────────────────

/// Round `v` to 3 decimal places.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Locate the trajectory file for `instance_id` inside `sweep_dir`.
///
/// Tries the canonical path pattern `{sweep_dir}/{instance_id}.traj.json`.
fn find_trajectory_path(sweep_dir: &Path, instance_id: &str) -> Result<PathBuf, Error> {
    let candidate = sweep_dir.join(format!("{instance_id}.traj.json"));
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(Error::Config(ConfigError::Invalid(format!(
        "trajectory file not found for instance '{instance_id}' in {}; \
         expected '{instance_id}.traj.json'",
        sweep_dir.display()
    ))))
}

/// Load the two fields we need from a trajectory JSON file.
///
/// Returns `(tests_run_field_present, last_tests_passed)`.
///
/// `tests_run_field_present` is `true` when `info.tests_run_before_submit` key
/// exists in the raw JSON (even if its value is `false`). A missing key signals a
/// pre-#46 trajectory.
fn load_trajectory_info(path: &Path) -> Result<(bool, Option<bool>), Error> {
    let text = std::fs::read_to_string(path)?;
    let raw: serde_json::Value = serde_json::from_str(&text)?;

    let info = &raw["info"];
    let tests_run_field_present = info.get("tests_run_before_submit").is_some();

    // `last_tests_passed` is absent from JSON when None (skip_serializing_if),
    // and present as a bool otherwise.
    let last_tests_passed = info
        .get("last_tests_passed")
        .and_then(serde_json::Value::as_bool);

    Ok((tests_run_field_present, last_tests_passed))
}

/// Parse a SWE-bench `owner__repo-NNNNN` instance ID into `"owner/repo"`.
///
/// Returns `None` when the ID does not match the expected format.
fn parse_repo_from_instance_id(instance_id: &str) -> Option<String> {
    let (owner, rest) = instance_id.split_once("__")?;
    let (repo, _suffix) = rest.rsplit_once('-')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repo_round_trips() {
        assert_eq!(
            parse_repo_from_instance_id("django__django-12345"),
            Some("django/django".to_owned())
        );
        assert_eq!(
            parse_repo_from_instance_id("sympy__sympy-99999"),
            Some("sympy/sympy".to_owned())
        );
    }

    #[test]
    fn parse_repo_rejects_bad_format() {
        assert!(parse_repo_from_instance_id("no-double-underscore").is_none());
        assert!(parse_repo_from_instance_id("__-missing-owner").is_none());
    }

    #[test]
    fn round3_rounds_correctly() {
        assert_eq!(round3(0.0), 0.0);
        assert_eq!(round3(0.25), 0.25);
        assert!((round3(3.75 / 13.0) - 0.288).abs() < 1e-9);
    }
}
