//! `bench test-progress`: per-test partial-credit scoring across a sweep.
//!
//! Consumes existing evaluator output (`evaluation.json`) and dataset JSONL
//! (`dataset.jsonl`) to compute partial-credit scores, verdict buckets, and
//! hot-failing-test aggregations for every instance in a sweep.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::compare::{load_evaluation_results_checked, load_sweep};
use crate::run::evaluate::{EvalExitReason, InstanceEvaluation};
use crate::run::swebench::SweBenchInstance;

// ── public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TestProgressArgs {
    pub sweep_dir: PathBuf,
    pub format: String,
    pub bucket: Option<String>,
    pub hot_tests_n: usize,
    pub filter: Option<String>,
    pub min_tests: usize,
}

/// Verdict bucket for a single instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictBucket {
    Resolved,
    PartialProgress,
    NoProgress,
    Regressed,
    EvaluatorUnavailable,
}

impl VerdictBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::PartialProgress => "partial_progress",
            Self::NoProgress => "no_progress",
            Self::Regressed => "regressed",
            Self::EvaluatorUnavailable => "evaluator_unavailable",
        }
    }
}

impl std::fmt::Display for VerdictBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-instance computed metrics (before verdict assignment).
#[derive(Debug, Clone)]
pub struct InstanceTestMetrics {
    pub fail_to_pass_total: usize,
    pub fail_to_pass_passed: usize,
    pub fail_to_pass_ratio: f64,
    pub pass_to_pass_total: usize,
    pub pass_to_pass_regressed: usize,
    pub pass_to_pass_regressed_ratio: f64,
    pub partial_credit_score: f64,
}

impl InstanceTestMetrics {
    #[must_use]
    pub fn verdict_bucket(&self) -> VerdictBucket {
        // A resolved instance always scores exactly 1.0 (all FAIL_TO_PASS passed,
        // no PASS_TO_PASS regressed). We check the score value, not the resolved field,
        // so that the formula is the single source of truth.
        #[allow(clippy::float_cmp)]
        if (self.partial_credit_score - 1.0).abs() < f64::EPSILON
            && (self.fail_to_pass_ratio - 1.0).abs() < f64::EPSILON
            && self.pass_to_pass_regressed_ratio < f64::EPSILON
        {
            return VerdictBucket::Resolved;
        }
        if self.partial_credit_score > 0.0 {
            VerdictBucket::PartialProgress
        } else if self.partial_credit_score < 0.0 {
            VerdictBucket::Regressed
        } else {
            VerdictBucket::NoProgress
        }
    }
}

/// Row in the `per_instance` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerInstanceRow {
    pub instance_id: String,
    pub verdict_bucket: String,
    pub partial_credit_score: f64,
    pub fail_to_pass: FailToPassDetail,
    pub pass_to_pass: PassToPassDetail,
    pub outcome: String,
    /// True when the instance is excluded from sweep means by `--min-tests`.
    #[serde(default)]
    pub excluded_from_means: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailToPassDetail {
    pub total: usize,
    pub passed_count: usize,
    pub passed_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassToPassDetail {
    pub total: usize,
    pub regressed_count: usize,
    pub regressed_ratio: f64,
}

/// Entry in `hot_failing_tests` / `hot_regressed_tests`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotTestEntry {
    pub test_name: String,
    pub instance_count: usize,
}

/// Sweep-level totals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestProgressTotals {
    pub instance_count: usize,
    pub per_bucket: BTreeMap<String, u64>,
    pub per_bucket_share: BTreeMap<String, f64>,
    pub evaluator_unavailable_count: u64,
    pub mean_partial_credit_score: f64,
    pub mean_fail_to_pass_passed_ratio: f64,
    pub mean_pass_to_pass_regressed_ratio: f64,
}

/// The full test-progress report artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestProgressReport {
    pub schema_version: u32,
    pub generated_at: String,
    pub sweep: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_signature: Option<String>,
    pub totals: TestProgressTotals,
    pub per_instance: Vec<PerInstanceRow>,
    pub hot_failing_tests: Vec<HotTestEntry>,
    pub hot_regressed_tests: Vec<HotTestEntry>,
    pub redaction_applied: bool,
}

impl TestProgressReport {
    pub const SCHEMA_VERSION: u32 = 1;
}

// ── public computation functions ──────────────────────────────────────────────

/// Compute `partial_credit_score = clamp(pass_ratio - regress_ratio, -1.0, 1.0)`.
#[must_use]
pub fn compute_partial_credit_score(
    fail_to_pass_passed_ratio: f64,
    pass_to_pass_regressed_ratio: f64,
) -> f64 {
    (fail_to_pass_passed_ratio - pass_to_pass_regressed_ratio).clamp(-1.0, 1.0)
}

/// Compute per-instance metrics given the expected test lists and evaluator output.
#[must_use]
pub fn compute_instance_metrics(
    fail_to_pass: &[String],
    pass_to_pass: &[String],
    tests_passed: &[String],
    tests_failed: &[String],
) -> InstanceTestMetrics {
    let passed_set: HashSet<&str> = tests_passed.iter().map(String::as_str).collect();
    let failed_set: HashSet<&str> = tests_failed.iter().map(String::as_str).collect();

    let ftp_total = fail_to_pass.len();
    let ftp_passed = fail_to_pass
        .iter()
        .filter(|t| passed_set.contains(t.as_str()))
        .count();

    let ptp_total = pass_to_pass.len();
    let ptp_regressed = pass_to_pass
        .iter()
        .filter(|t| failed_set.contains(t.as_str()))
        .count();

    #[allow(clippy::cast_precision_loss)]
    let ftp_ratio = if ftp_total > 0 {
        ftp_passed as f64 / ftp_total as f64
    } else {
        1.0
    };
    #[allow(clippy::cast_precision_loss)]
    let ptp_regressed_ratio = if ptp_total > 0 {
        ptp_regressed as f64 / ptp_total as f64
    } else {
        0.0
    };

    let score = compute_partial_credit_score(ftp_ratio, ptp_regressed_ratio);

    InstanceTestMetrics {
        fail_to_pass_total: ftp_total,
        fail_to_pass_passed: ftp_passed,
        fail_to_pass_ratio: ftp_ratio,
        pass_to_pass_total: ptp_total,
        pass_to_pass_regressed: ptp_regressed,
        pass_to_pass_regressed_ratio: ptp_regressed_ratio,
        partial_credit_score: score,
    }
}

// ── public command runner ─────────────────────────────────────────────────────

pub fn run(args: &TestProgressArgs) -> Result<TestProgressReport, Error> {
    let mut report = build_report(args)?;
    report.generated_at = utc_now_iso8601();
    if args.filter.is_none() {
        let output_path = args.sweep_dir.join("test-progress.json");
        let file = std::fs::File::create(&output_path)?;
        serde_json::to_writer_pretty(file, &report)?;
    }
    Ok(report)
}

/// Try to load test-progress.json from a sweep dir for `bench compare` integration.
/// Returns `None` when the file is absent or unreadable.
#[allow(clippy::similar_names)]
pub fn test_progress_compare_section(baseline: &Path, candidate: &Path) -> Option<String> {
    let b_path = baseline.join("test-progress.json");
    let c_path = candidate.join("test-progress.json");

    if !b_path.exists() || !c_path.exists() {
        return None;
    }

    let b_text = std::fs::read_to_string(&b_path).ok()?;
    let c_text = std::fs::read_to_string(&c_path).ok()?;
    let b: TestProgressReport = serde_json::from_str(&b_text).ok()?;
    let c: TestProgressReport = serde_json::from_str(&c_text).ok()?;

    #[allow(clippy::similar_names)]
    let mean_score_delta = c.totals.mean_partial_credit_score - b.totals.mean_partial_credit_score;
    #[allow(clippy::similar_names)]
    let mean_ftp_ratio_delta =
        c.totals.mean_fail_to_pass_passed_ratio - b.totals.mean_fail_to_pass_passed_ratio;
    #[allow(clippy::similar_names)]
    let mean_ptp_ratio_delta =
        c.totals.mean_pass_to_pass_regressed_ratio - b.totals.mean_pass_to_pass_regressed_ratio;

    let mut out = String::from("\n--- Test progress delta ---\n");
    let _ = writeln!(
        out,
        "mean_partial_credit_score: {:.4} -> {:.4} ({:+.4})",
        b.totals.mean_partial_credit_score, c.totals.mean_partial_credit_score, mean_score_delta
    );
    let _ = writeln!(
        out,
        "mean_fail_to_pass_passed_ratio: {:.4} -> {:.4} ({:+.4})",
        b.totals.mean_fail_to_pass_passed_ratio,
        c.totals.mean_fail_to_pass_passed_ratio,
        mean_ftp_ratio_delta
    );
    let _ = writeln!(
        out,
        "mean_pass_to_pass_regressed_ratio: {:.4} -> {:.4} ({:+.4})",
        b.totals.mean_pass_to_pass_regressed_ratio,
        c.totals.mean_pass_to_pass_regressed_ratio,
        mean_ptp_ratio_delta
    );

    // Per-bucket count deltas
    let all_buckets = [
        "resolved",
        "partial_progress",
        "no_progress",
        "regressed",
        "evaluator_unavailable",
    ];
    out.push_str("\nBucket counts (baseline -> candidate):\n");
    for bucket in all_buckets {
        let b_count = b.totals.per_bucket.get(bucket).copied().unwrap_or(0);
        let c_count = c.totals.per_bucket.get(bucket).copied().unwrap_or(0);
        #[allow(clippy::cast_possible_wrap)]
        let delta = c_count as i64 - b_count as i64;
        let _ = writeln!(out, "  {bucket}: {b_count} -> {c_count} ({delta:+})");
    }

    Some(out)
}

pub fn render_text(report: &TestProgressReport, bucket_filter: Option<&str>) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let mut out = String::new();
    out.push_str("\n=== bench test-progress ===\n");
    let _ = writeln!(out, "Sweep: {}", report.sweep);
    let _ = writeln!(out, "Instances: {}", report.totals.instance_count);

    // Summary line
    let _ = writeln!(
        out,
        "mean_partial_credit_score: {:.4}  (ftp_passed: {:.4}  ptp_regressed: {:.4})",
        report.totals.mean_partial_credit_score,
        report.totals.mean_fail_to_pass_passed_ratio,
        report.totals.mean_pass_to_pass_regressed_ratio
    );
    out.push('\n');

    // Bucket counts
    out.push_str("Verdict bucket counts:\n");
    for (bucket, count) in &report.totals.per_bucket {
        let share = report
            .totals
            .per_bucket_share
            .get(bucket)
            .copied()
            .unwrap_or(0.0);
        let _ = writeln!(out, "  {bucket}: {count} ({:.1}%)", share * 100.0);
    }
    out.push('\n');

    // Per-instance table
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            "instance_id",
            "verdict_bucket",
            "score",
            "ftp_passed",
            "ptp_regressed",
        ]);

    for row in &report.per_instance {
        if let Some(filter) = bucket_filter {
            if row.verdict_bucket != filter {
                continue;
            }
        }
        table.add_row(vec![
            row.instance_id.clone(),
            row.verdict_bucket.clone(),
            format!("{:.4}", row.partial_credit_score),
            format!(
                "{}/{}",
                row.fail_to_pass.passed_count, row.fail_to_pass.total
            ),
            format!(
                "{}/{}",
                row.pass_to_pass.regressed_count, row.pass_to_pass.total
            ),
        ]);
    }
    out.push_str(&table.to_string());
    out.push('\n');

    // Hot failing tests
    if !report.hot_failing_tests.is_empty() {
        out.push_str("\nHot failing FAIL_TO_PASS tests:\n");
        let mut t = Table::new();
        t.load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec!["rank", "test_name", "instance_count"]);
        for (idx, entry) in report.hot_failing_tests.iter().enumerate() {
            t.add_row(vec![
                (idx + 1).to_string(),
                entry.test_name.clone(),
                entry.instance_count.to_string(),
            ]);
        }
        out.push_str(&t.to_string());
        out.push('\n');
    }

    // Hot regressed tests
    if !report.hot_regressed_tests.is_empty() {
        out.push_str("\nHot regressed PASS_TO_PASS tests:\n");
        let mut t = Table::new();
        t.load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec!["rank", "test_name", "instance_count"]);
        for (idx, entry) in report.hot_regressed_tests.iter().enumerate() {
            t.add_row(vec![
                (idx + 1).to_string(),
                entry.test_name.clone(),
                entry.instance_count.to_string(),
            ]);
        }
        out.push_str(&t.to_string());
        out.push('\n');
    }

    if report.totals.evaluator_unavailable_count > 0 {
        let _ = writeln!(
            out,
            "\nNote: {} instance(s) had evaluator_unavailable verdict and are excluded from means.",
            report.totals.evaluator_unavailable_count
        );
    }

    out
}

// ── internal implementation ───────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn build_report(args: &TestProgressArgs) -> Result<TestProgressReport, Error> {
    let valid_buckets = [
        "resolved",
        "partial_progress",
        "no_progress",
        "regressed",
        "evaluator_unavailable",
    ];
    if let Some(b) = &args.bucket {
        if !valid_buckets.contains(&b.as_str()) {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "test-progress: unknown --bucket `{b}`; valid values: {}",
                valid_buckets.join(", ")
            ))));
        }
    }

    let sweep = load_sweep(&args.sweep_dir)?;
    let evaluation = load_evaluation_results_checked(&args.sweep_dir)?;

    // Map instance_id → InstanceEvaluation
    let eval_map: HashMap<String, InstanceEvaluation> = evaluation
        .as_ref()
        .map(|ev| {
            ev.results
                .instances
                .iter()
                .map(|i| (i.instance_id.clone(), i.clone()))
                .collect()
        })
        .unwrap_or_default();

    // Load dataset JSONL for FAIL_TO_PASS / PASS_TO_PASS expected test lists
    let dataset_map = load_dataset_jsonl(&args.sweep_dir);

    // Build resolved set
    let resolved_set: HashSet<String> = evaluation
        .as_ref()
        .map(|ev| {
            ev.results
                .instances
                .iter()
                .filter(|i| i.resolved)
                .map(|i| i.instance_id.clone())
                .collect()
        })
        .unwrap_or_default();

    // Compute per-instance rows in deterministic order
    let mut sorted_ids: Vec<String> = sweep.instances.keys().cloned().collect();
    sorted_ids.sort();

    // Apply instance-level filter if present
    let mut instance_rows: Vec<PerInstanceRow> = Vec::new();
    for id in &sorted_ids {
        let instance = &sweep.instances[id];
        let is_resolved = resolved_set.contains(id);

        if let Some(filter) = &args.filter {
            if !matches_filter(instance, Some(is_resolved), filter)? {
                continue;
            }
        }

        let eval = eval_map.get(id);
        let row = build_instance_row(id, instance, eval, &dataset_map, args.min_tests);
        instance_rows.push(row);
    }

    // Sort: ascending partial_credit_score, ties by instance_id
    instance_rows.sort_by(|a, b| {
        a.partial_credit_score
            .partial_cmp(&b.partial_credit_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    // Aggregate bucket counts
    let all_bucket_names = [
        "resolved",
        "partial_progress",
        "no_progress",
        "regressed",
        "evaluator_unavailable",
    ];
    let mut per_bucket: BTreeMap<String, u64> = all_bucket_names
        .iter()
        .map(|&k| (k.to_owned(), 0u64))
        .collect();
    for row in &instance_rows {
        *per_bucket.entry(row.verdict_bucket.clone()).or_default() += 1;
    }

    let total = instance_rows.len();
    #[allow(clippy::cast_precision_loss)]
    let per_bucket_share: BTreeMap<String, f64> = per_bucket
        .iter()
        .map(|(k, &v)| {
            let share = if total > 0 {
                v as f64 / total as f64
            } else {
                0.0
            };
            (k.clone(), share)
        })
        .collect();

    let unavailable_count = per_bucket
        .get("evaluator_unavailable")
        .copied()
        .unwrap_or(0);

    // Compute means over eligible instances
    let eligible: Vec<&PerInstanceRow> = instance_rows
        .iter()
        .filter(|r| r.verdict_bucket != "evaluator_unavailable" && !r.excluded_from_means)
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let mean_pcs = if eligible.is_empty() {
        0.0
    } else {
        eligible.iter().map(|r| r.partial_credit_score).sum::<f64>() / eligible.len() as f64
    };
    #[allow(clippy::cast_precision_loss)]
    let mean_ftp = if eligible.is_empty() {
        0.0
    } else {
        eligible
            .iter()
            .map(|r| r.fail_to_pass.passed_ratio)
            .sum::<f64>()
            / eligible.len() as f64
    };
    #[allow(clippy::cast_precision_loss)]
    let mean_ptp_regressed = if eligible.is_empty() {
        0.0
    } else {
        eligible
            .iter()
            .map(|r| r.pass_to_pass.regressed_ratio)
            .sum::<f64>()
            / eligible.len() as f64
    };

    // hot_failing_tests: FAIL_TO_PASS tests that remained failing, ranked by instance count.
    // A test is "still failing" when it is absent from tests_passed — matching the definition
    // used by compute_instance_metrics — so tests not mentioned in tests_failed but also not
    // present in tests_passed are correctly counted here.
    let mut failing_test_counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in &instance_rows {
        if row.verdict_bucket == "evaluator_unavailable" || row.verdict_bucket == "resolved" {
            continue;
        }
        if let Some(eval) = eval_map.get(&row.instance_id) {
            if let Some(inst) = dataset_map.get(&row.instance_id) {
                let ftp_list = get_test_list(inst, "FAIL_TO_PASS");
                let passed_set: HashSet<&str> =
                    eval.tests_passed.iter().map(String::as_str).collect();
                for ftp_test in &ftp_list {
                    if !passed_set.contains(ftp_test.as_str()) {
                        *failing_test_counts.entry(ftp_test.clone()).or_default() += 1;
                    }
                }
            }
        }
    }
    let mut hot_failing_tests: Vec<HotTestEntry> = failing_test_counts
        .into_iter()
        .map(|(test_name, instance_count)| HotTestEntry {
            test_name,
            instance_count,
        })
        .collect();
    hot_failing_tests.sort_by(|a, b| {
        b.instance_count
            .cmp(&a.instance_count)
            .then_with(|| a.test_name.cmp(&b.test_name))
    });
    hot_failing_tests.truncate(args.hot_tests_n);

    // hot_regressed_tests: PASS_TO_PASS tests that regressed, ranked by instance count
    let mut regressed_test_counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in &instance_rows {
        if row.verdict_bucket == "evaluator_unavailable" {
            continue;
        }
        if let Some(eval) = eval_map.get(&row.instance_id) {
            if let Some(inst) = dataset_map.get(&row.instance_id) {
                let ptp_list = get_test_list(inst, "PASS_TO_PASS");
                let ptp_set: HashSet<&str> = ptp_list.iter().map(String::as_str).collect();
                for failed in &eval.tests_failed {
                    if ptp_set.contains(failed.as_str()) {
                        *regressed_test_counts.entry(failed.clone()).or_default() += 1;
                    }
                }
            }
        }
    }
    let mut hot_regressed_tests: Vec<HotTestEntry> = regressed_test_counts
        .into_iter()
        .map(|(test_name, instance_count)| HotTestEntry {
            test_name,
            instance_count,
        })
        .collect();
    hot_regressed_tests.sort_by(|a, b| {
        b.instance_count
            .cmp(&a.instance_count)
            .then_with(|| a.test_name.cmp(&b.test_name))
    });
    hot_regressed_tests.truncate(args.hot_tests_n);

    // Compute sweep_signature from manifest hash if available
    let sweep_signature = sweep
        .manifest
        .as_ref()
        .and_then(|m| serde_json::to_string(m).ok())
        .map(|s| format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(s.as_bytes())));

    // Redact test names in hot lists before persisting
    let redactor = Redactor::default_enabled();
    let mut redaction_applied = false;
    for entry in &mut hot_failing_tests {
        let outcome = redactor.redact_text(&entry.test_name, surface::INSPECT);
        if outcome.redacted {
            entry.test_name = outcome.text;
            redaction_applied = true;
        }
    }
    for entry in &mut hot_regressed_tests {
        let outcome = redactor.redact_text(&entry.test_name, surface::INSPECT);
        if outcome.redacted {
            entry.test_name = outcome.text;
            redaction_applied = true;
        }
    }

    let totals = TestProgressTotals {
        instance_count: total,
        per_bucket,
        per_bucket_share,
        evaluator_unavailable_count: unavailable_count,
        mean_partial_credit_score: mean_pcs,
        mean_fail_to_pass_passed_ratio: mean_ftp,
        mean_pass_to_pass_regressed_ratio: mean_ptp_regressed,
    };

    Ok(TestProgressReport {
        schema_version: TestProgressReport::SCHEMA_VERSION,
        generated_at: String::new(),
        sweep: args.sweep_dir.display().to_string(),
        sweep_signature,
        totals,
        per_instance: instance_rows,
        hot_failing_tests,
        hot_regressed_tests,
        redaction_applied,
    })
}

#[allow(clippy::too_many_lines)]
fn build_instance_row(
    instance_id: &str,
    instance: &crate::run::swebench::InstanceResult,
    eval: Option<&InstanceEvaluation>,
    dataset_map: &HashMap<String, SweBenchInstance>,
    min_tests: usize,
) -> PerInstanceRow {
    // Determine if evaluator data is available for per-test scoring
    let is_unavailable = match eval {
        None => true,
        Some(e) => matches!(
            e.eval_exit_reason,
            EvalExitReason::PatchApplyFailed
                | EvalExitReason::EvalError
                | EvalExitReason::SkippedNoPatch
        ),
    };

    let outcome = instance
        .outcome
        .clone()
        .unwrap_or_else(|| instance.exit_reason.clone());

    if is_unavailable {
        return PerInstanceRow {
            instance_id: instance_id.to_owned(),
            verdict_bucket: VerdictBucket::EvaluatorUnavailable.as_str().to_owned(),
            partial_credit_score: 0.0,
            fail_to_pass: FailToPassDetail {
                total: 0,
                passed_count: 0,
                passed_ratio: 0.0,
            },
            pass_to_pass: PassToPassDetail {
                total: 0,
                regressed_count: 0,
                regressed_ratio: 0.0,
            },
            outcome,
            excluded_from_means: false,
        };
    }

    let Some(eval) = eval else {
        return PerInstanceRow {
            instance_id: instance_id.to_owned(),
            verdict_bucket: VerdictBucket::EvaluatorUnavailable.as_str().to_owned(),
            partial_credit_score: 0.0,
            fail_to_pass: FailToPassDetail {
                total: 0,
                passed_count: 0,
                passed_ratio: 0.0,
            },
            pass_to_pass: PassToPassDetail {
                total: 0,
                regressed_count: 0,
                regressed_ratio: 0.0,
            },
            outcome,
            excluded_from_means: false,
        };
    };

    // Get expected test lists from dataset
    let (fail_to_pass, pass_to_pass) = if let Some(inst) = dataset_map.get(instance_id) {
        (
            get_test_list(inst, "FAIL_TO_PASS"),
            get_test_list(inst, "PASS_TO_PASS"),
        )
    } else {
        (vec![], vec![])
    };

    // Unresolved instance with no dataset entry: cannot compute meaningful score
    if !eval.resolved && fail_to_pass.is_empty() && pass_to_pass.is_empty() {
        return PerInstanceRow {
            instance_id: instance_id.to_owned(),
            verdict_bucket: VerdictBucket::EvaluatorUnavailable.as_str().to_owned(),
            partial_credit_score: 0.0,
            fail_to_pass: FailToPassDetail {
                total: 0,
                passed_count: 0,
                passed_ratio: 0.0,
            },
            pass_to_pass: PassToPassDetail {
                total: 0,
                regressed_count: 0,
                regressed_ratio: 0.0,
            },
            outcome,
            excluded_from_means: false,
        };
    }

    // For resolved instances, all FAIL_TO_PASS passed and nothing regressed
    let (tests_passed, tests_failed) = if eval.resolved {
        // Use evaluator data if non-empty, otherwise synthesize from dataset lists
        if !eval.tests_passed.is_empty() || !eval.tests_failed.is_empty() {
            (eval.tests_passed.clone(), eval.tests_failed.clone())
        } else {
            // Synthesize: all FAIL_TO_PASS + PASS_TO_PASS are in tests_passed
            let mut passed = fail_to_pass.clone();
            passed.extend_from_slice(&pass_to_pass);
            (passed, vec![])
        }
    } else {
        (eval.tests_passed.clone(), eval.tests_failed.clone())
    };

    // Unresolved instance with no per-test data from evaluator: cannot score
    if !eval.resolved && tests_passed.is_empty() && tests_failed.is_empty() {
        return PerInstanceRow {
            instance_id: instance_id.to_owned(),
            verdict_bucket: VerdictBucket::EvaluatorUnavailable.as_str().to_owned(),
            partial_credit_score: 0.0,
            fail_to_pass: FailToPassDetail {
                total: 0,
                passed_count: 0,
                passed_ratio: 0.0,
            },
            pass_to_pass: PassToPassDetail {
                total: 0,
                regressed_count: 0,
                regressed_ratio: 0.0,
            },
            outcome,
            excluded_from_means: false,
        };
    }

    let metrics =
        compute_instance_metrics(&fail_to_pass, &pass_to_pass, &tests_passed, &tests_failed);

    let total_tests = fail_to_pass.len() + pass_to_pass.len();
    let excluded_from_means = min_tests > 0 && total_tests < min_tests;

    let bucket = if eval.resolved {
        VerdictBucket::Resolved
    } else {
        metrics.verdict_bucket()
    };

    PerInstanceRow {
        instance_id: instance_id.to_owned(),
        verdict_bucket: bucket.as_str().to_owned(),
        partial_credit_score: if eval.resolved {
            1.0
        } else {
            metrics.partial_credit_score
        },
        fail_to_pass: FailToPassDetail {
            total: metrics.fail_to_pass_total,
            passed_count: metrics.fail_to_pass_passed,
            passed_ratio: metrics.fail_to_pass_ratio,
        },
        pass_to_pass: PassToPassDetail {
            total: metrics.pass_to_pass_total,
            regressed_count: metrics.pass_to_pass_regressed,
            regressed_ratio: metrics.pass_to_pass_regressed_ratio,
        },
        outcome,
        excluded_from_means,
    }
}

fn get_test_list(inst: &SweBenchInstance, key: &str) -> Vec<String> {
    let Some(val) = inst.other.get(key) else {
        return vec![];
    };
    let items: Vec<serde_json::Value> = if let Some(arr) = val.as_array() {
        arr.clone()
    } else if let Some(s) = val.as_str() {
        serde_json::from_str(s).unwrap_or_default()
    } else {
        vec![]
    };
    items
        .iter()
        .filter_map(|v| v.as_str().map(ToOwned::to_owned))
        .collect()
}

fn load_dataset_jsonl(sweep_dir: &Path) -> HashMap<String, SweBenchInstance> {
    let dataset_path = sweep_dir.join("dataset.jsonl");
    if !dataset_path.exists() {
        return HashMap::new();
    }
    let Ok(text) = std::fs::read_to_string(&dataset_path) else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(inst) = serde_json::from_str::<SweBenchInstance>(line) {
            map.insert(inst.instance_id.clone(), inst);
        }
    }
    map
}

fn matches_filter(
    instance: &crate::run::swebench::InstanceResult,
    resolved: Option<bool>,
    filter: &str,
) -> Result<bool, Error> {
    let Some((key, value)) = filter.split_once('=') else {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "test-progress: --filter expects key=value (e.g. resolved=true)".into(),
        )));
    };
    let (key, value) = (key.trim(), value.trim());
    match key {
        "resolved" => {
            if value != "true" && value != "false" {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "test-progress: resolved filter must be `true` or `false`".into(),
                )));
            }
            Ok(resolved == Some(value == "true"))
        }
        "failure_category" => Ok(instance
            .failure_category
            .is_some_and(|fc| crate::run::command_stats::failure_category_label(fc) == value)),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "test-progress: unsupported filter key `{other}`; supported: `resolved`, `failure_category`"
        )))),
    }
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
