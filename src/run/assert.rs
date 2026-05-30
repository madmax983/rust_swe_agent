//! `bench assert` — evaluate operator-declared SLO rules against a completed sweep.
//!
//! Reads sweep artifacts (results.json, evaluation.json) and evaluates each rule
//! against the extracted metrics. Writes assertions.json next to results.json.
//! See `docs/spec-assert.md` for the full contract.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::error::{ConfigError, Error};

// ── args ─────────────────────────────────────────────────────────────────────

pub struct AssertArgs {
    pub sweep: PathBuf,
    pub rules_file: Option<PathBuf>,
    pub inline_rules: Vec<String>,
    pub verbose: bool,
    pub allow_missing_artifacts: bool,
}

// ── rule types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub metric: String,
    pub op: RuleOp,
    pub threshold: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleOp {
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
}

impl RuleOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    fn evaluate(self, observed: f64, threshold: f64) -> bool {
        match self {
            Self::Eq => {
                if observed.is_nan() || threshold.is_nan() {
                    false
                } else {
                    (observed - threshold).abs() < f64::EPSILON * 1000.0
                }
            }
            Self::Ne => {
                if observed.is_nan() || threshold.is_nan() {
                    true
                } else {
                    (observed - threshold).abs() >= f64::EPSILON * 1000.0
                }
            }
            Self::Lt => observed < threshold,
            Self::Le => observed <= threshold,
            Self::Gt => observed > threshold,
            Self::Ge => observed >= threshold,
        }
    }
}

impl std::fmt::Display for RuleOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── TOML rule file schema ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TomlRuleFile {
    rule: Vec<TomlRule>,
}

#[derive(Debug, Deserialize)]
struct TomlRule {
    name: String,
    metric: String,
    op: String,
    threshold: f64,
}

// ── metric vocabulary ─────────────────────────────────────────────────────────

/// Which source file a metric requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceArtifact {
    Results,
    Evaluation,
    Both,
}

/// Parsed metric: base name + optional bracket parameter.
#[derive(Debug, Clone)]
struct ParsedMetric {
    base: String,
    param: Option<String>,
}

/// Validate and parse a metric name.  Returns `Err` for unknown metrics.
fn parse_metric(raw: &str) -> Result<ParsedMetric, Error> {
    // Parametric metrics: base[param]
    if let Some(bracket) = raw.find('[') {
        if !raw.ends_with(']') {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "malformed parametric metric '{raw}': expected 'base[param]'"
            ))));
        }
        let base = raw[..bracket].to_owned();
        let param = raw[bracket + 1..raw.len() - 1].to_owned();
        // Validate base
        match base.as_str() {
            "failure_category_count" | "failure_category_share" | "at_cap_count" => {}
            other => {
                return Err(Error::Config(ConfigError::Invalid(format!(
                    "unknown parametric metric base '{other}'; see docs/spec-assert.md for the vocabulary"
                ))));
            }
        }
        if param.is_empty() {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "metric '{raw}' has empty parameter"
            ))));
        }
        // at_cap_count only defines three cap dimensions; reject typos as usage errors.
        if base == "at_cap_count" {
            match param.as_str() {
                "steps" | "cost" | "wallclock" => {}
                other => {
                    return Err(Error::Config(ConfigError::Invalid(format!(
                        "unknown parameter '{other}' for at_cap_count; valid values: steps, cost, wallclock"
                    ))));
                }
            }
        }
        return Ok(ParsedMetric {
            base,
            param: Some(param),
        });
    }

    // Scalar metrics
    match raw {
        "resolved_rate"
        | "resolved_count"
        | "unresolved_count"
        | "errored_count"
        | "total_cost_usd"
        | "mean_cost_per_instance_usd"
        | "cost_per_resolved_instance_usd"
        | "mean_steps"
        | "p95_steps"
        | "wallclock_total_s" => Ok(ParsedMetric {
            base: raw.to_owned(),
            param: None,
        }),
        other => Err(Error::Config(ConfigError::Invalid(format!(
            "unknown metric '{other}'; see docs/spec-assert.md for the v1 vocabulary"
        )))),
    }
}

fn metric_source(metric: &ParsedMetric) -> SourceArtifact {
    match metric.base.as_str() {
        "resolved_rate" | "resolved_count" | "unresolved_count" => SourceArtifact::Evaluation,
        "cost_per_resolved_instance_usd" => SourceArtifact::Both,
        _ => SourceArtifact::Results,
    }
}

fn metric_source_field_path(metric: &ParsedMetric) -> String {
    match metric.base.as_str() {
        "resolved_rate" => "evaluation.json: resolved instances / total instances".to_owned(),
        "resolved_count" => "evaluation.json: count of instances where resolved=true".to_owned(),
        "unresolved_count" => "evaluation.json: count of instances where resolved=false".to_owned(),
        "errored_count" => "results.json: .errored".to_owned(),
        "total_cost_usd" => "results.json: .total_cost_usd".to_owned(),
        "mean_cost_per_instance_usd" => "results.json: .total_cost_usd / .total".to_owned(),
        "cost_per_resolved_instance_usd" => {
            "results.json: .total_cost_usd / evaluation.json resolved count".to_owned()
        }
        "mean_steps" => "results.json: mean of .instances[].steps".to_owned(),
        "p95_steps" => "results.json: p95 of .instances[].steps".to_owned(),
        "wallclock_total_s" => {
            "results.json: .manifest.runtime.finished_at_utc - .manifest.runtime.started_at_utc"
                .to_owned()
        }
        "failure_category_count" => {
            let param = metric.param.as_deref().unwrap_or("?");
            format!("results.json: .failures_by_category.{param}")
        }
        "failure_category_share" => {
            let param = metric.param.as_deref().unwrap_or("?");
            format!("results.json: .failures_by_category.{param} / .total")
        }
        "at_cap_count" => {
            let param = metric.param.as_deref().unwrap_or("?");
            match param {
                "steps" => {
                    "results.json: count of .instances[] where failure_category=\"step_limit\""
                        .to_owned()
                }
                "cost" => "results.json: count of .instances[] where exit_reason=\"budget_halt\""
                    .to_owned(),
                "wallclock" => {
                    "results.json: count of .instances[] where exit_reason=\"wallclock_timeout\""
                        .to_owned()
                }
                other => {
                    format!("results.json: count of .instances[] where exit_reason=\"{other}\"")
                }
            }
        }
        other => format!("unknown metric: {other}"),
    }
}

/// Maps an `at_cap_count` parameter to the `exit_reason` value used in results.json
/// for cost and wallclock caps (which set exit_reason, not failure_category).
fn cap_exit_reason(param: &str) -> &str {
    match param {
        "cost" => "budget_halt",
        "wallclock" => "wallclock_timeout",
        other => other,
    }
}

// ── metric extraction ─────────────────────────────────────────────────────────

struct Artifacts {
    results: Value,
    evaluation: Option<Value>,
}

/// Convert a count to f64; clamps at u32::MAX so large-but-realistic sweeps
/// still work without precision-loss on platforms where usize == 64 bits.
fn count_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

fn compute_mean_steps(steps: &[f64]) -> f64 {
    if steps.is_empty() {
        return 0.0;
    }
    steps.iter().sum::<f64>() / count_f64(steps.len())
}

fn compute_p95_steps(steps: &[f64]) -> f64 {
    if steps.is_empty() {
        return 0.0;
    }
    let mut sorted = steps.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    // (n-1) linear interpolation matching NumPy/pandas default percentile method.
    // For [5,10,15,20]: pos = 0.95*3 = 2.85 → 15 + 0.85*(20-15) = 19.25.
    let pos = 0.95 * count_f64(n - 1);
    let lo = pos.floor();
    let frac = pos - lo;
    // pos is bounded to [0, n-2] by construction; cast is safe.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo_idx = (lo as usize).min(n - 2);
    sorted[lo_idx] + frac * (sorted[lo_idx + 1] - sorted[lo_idx])
}

fn extract_metric(metric: &ParsedMetric, artifacts: &Artifacts) -> Option<f64> {
    let results = &artifacts.results;
    match metric.base.as_str() {
        "resolved_rate" => extract_resolved_rate(artifacts, results),
        "resolved_count" => {
            let instances = artifacts.evaluation.as_ref()?["instances"].as_array()?;
            Some(count_f64(
                instances.iter().filter(|i| i["resolved"] == true).count(),
            ))
        }
        "unresolved_count" => {
            let instances = artifacts.evaluation.as_ref()?["instances"].as_array()?;
            Some(count_f64(
                instances.iter().filter(|i| i["resolved"] == false).count(),
            ))
        }
        "errored_count" => results["errored"].as_f64(),
        "total_cost_usd" => results["total_cost_usd"].as_f64(),
        "mean_cost_per_instance_usd" => {
            let total_cost = results["total_cost_usd"].as_f64()?;
            let total = results["total"].as_f64()?;
            (total > 0.0).then(|| total_cost / total).or(Some(0.0))
        }
        "cost_per_resolved_instance_usd" => {
            let total_cost = results["total_cost_usd"].as_f64()?;
            let instances = artifacts.evaluation.as_ref()?["instances"].as_array()?;
            let resolved = instances.iter().filter(|i| i["resolved"] == true).count();
            if resolved == 0 {
                Some(f64::INFINITY)
            } else {
                Some(total_cost / count_f64(resolved))
            }
        }
        "mean_steps" => {
            let steps = collect_steps(results)?;
            Some(compute_mean_steps(&steps))
        }
        "p95_steps" => {
            let steps = collect_steps(results)?;
            Some(compute_p95_steps(&steps))
        }
        "wallclock_total_s" => extract_wallclock(results),
        "failure_category_count" => {
            let param = metric.param.as_deref().unwrap_or("");
            Some(
                results["failures_by_category"][param]
                    .as_f64()
                    .unwrap_or(0.0),
            )
        }
        "failure_category_share" => {
            let param = metric.param.as_deref().unwrap_or("");
            let count = results["failures_by_category"][param]
                .as_f64()
                .unwrap_or(0.0);
            let total = results["total"].as_f64()?;
            (total > 0.0).then(|| count / total).or(Some(0.0))
        }
        "at_cap_count" => {
            let param = metric.param.as_deref().unwrap_or("");
            let instances = results["instances"].as_array()?;
            // Modern artifacts: steps caps use failure_category="step_limit"; cost/wallclock caps
            // use exit_reason. Legacy artifacts may only have exit_reason for step caps too.
            Some(count_f64(match param {
                "steps" => instances
                    .iter()
                    .filter(|i| {
                        i["failure_category"].as_str() == Some("step_limit")
                            || i["exit_reason"].as_str() == Some("step_limit")
                    })
                    .count(),
                other => {
                    let exit_reason = cap_exit_reason(other);
                    instances
                        .iter()
                        .filter(|i| i["exit_reason"].as_str() == Some(exit_reason))
                        .count()
                }
            }))
        }
        _ => None,
    }
}

fn extract_resolved_rate(artifacts: &Artifacts, results: &Value) -> Option<f64> {
    let instances = artifacts.evaluation.as_ref()?["instances"].as_array()?;
    // Use results["total"] so instances missing from evaluation.json count against
    // the denominator (spec requirement: trajectories missing from evaluation.json
    // count against the denominator).
    let total = results["total"].as_f64()?;
    if total == 0.0 {
        return Some(0.0);
    }
    let resolved = instances.iter().filter(|i| i["resolved"] == true).count();
    Some(count_f64(resolved) / total)
}

fn collect_steps(results: &Value) -> Option<Vec<f64>> {
    Some(
        results["instances"]
            .as_array()?
            .iter()
            .filter_map(|i| i["steps"].as_f64())
            .collect(),
    )
}

fn extract_wallclock(results: &Value) -> Option<f64> {
    let started = results["manifest"]["runtime"]["started_at_utc"].as_str()?;
    let finished = results["manifest"]["runtime"]["finished_at_utc"].as_str()?;
    let started_dt = chrono::DateTime::parse_from_rfc3339(started).ok()?;
    let finished_dt = chrono::DateTime::parse_from_rfc3339(finished).ok()?;
    let secs = finished_dt.signed_duration_since(started_dt).num_seconds();
    // Wallclock durations fit easily in f64; precision loss is irrelevant.
    #[allow(clippy::cast_precision_loss)]
    Some(secs as f64)
}

// ── rule parsing ──────────────────────────────────────────────────────────────

fn parse_op(op_str: &str) -> Result<RuleOp, Error> {
    match op_str {
        "==" => Ok(RuleOp::Eq),
        "!=" => Ok(RuleOp::Ne),
        "<" => Ok(RuleOp::Lt),
        "<=" => Ok(RuleOp::Le),
        ">" => Ok(RuleOp::Gt),
        ">=" => Ok(RuleOp::Ge),
        other => Err(Error::Config(ConfigError::Invalid(format!(
            "unknown operator '{other}'; valid ops: ==, !=, <, <=, >, >="
        )))),
    }
}

fn parse_inline_rule(raw: &str) -> Result<Rule, Error> {
    // Try multi-char ops first to avoid ambiguity (>= before >).
    let ops = [">=", "<=", "!=", "==", ">", "<"];
    for op_str in &ops {
        if let Some(pos) = raw.find(op_str) {
            let metric_raw = &raw[..pos];
            let threshold_raw = &raw[pos + op_str.len()..];
            // Validate metric
            let parsed_metric = parse_metric(metric_raw)?;
            let op = parse_op(op_str)?;
            let threshold: f64 = threshold_raw.parse().map_err(|_| {
                Error::Config(ConfigError::Invalid(format!(
                    "invalid threshold '{threshold_raw}' in rule '{raw}'"
                )))
            })?;
            let full_metric = if let Some(ref p) = parsed_metric.param {
                format!("{}[{}]", parsed_metric.base, p)
            } else {
                parsed_metric.base
            };
            return Ok(Rule {
                name: metric_raw.to_owned(),
                metric: full_metric,
                op,
                threshold,
            });
        }
    }
    Err(Error::Config(ConfigError::Invalid(format!(
        "cannot parse rule '{raw}': expected format 'metric<op>threshold' where op is one of ==, !=, <, <=, >, >="
    ))))
}

fn load_rules_from_file(path: &Path) -> Result<Vec<Rule>, Error> {
    let text = fs::read_to_string(path).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "cannot read rules file '{}': {e}",
            path.display()
        )))
    })?;
    let file: TomlRuleFile = toml::from_str(&text).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "malformed rules file '{}': {e}",
            path.display()
        )))
    })?;
    file.rule
        .into_iter()
        .map(|r| {
            // Validate metric
            let parsed = parse_metric(&r.metric)?;
            let op = parse_op(&r.op)?;
            let full_metric = if let Some(ref p) = parsed.param {
                format!("{}[{}]", parsed.base, p)
            } else {
                parsed.base
            };
            Ok(Rule {
                name: r.name,
                metric: full_metric,
                op,
                threshold: r.threshold,
            })
        })
        .collect()
}

// ── artifact schema ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct AssertionsArtifact {
    artifact_kind: &'static str,
    schema_version: SchemaVersion,
    generated_at: String,
    sweep_id: String,
    rules: Vec<RuleRecord>,
    passed: bool,
    passed_count: usize,
    failed_count: usize,
    skipped_count: usize,
}

#[derive(Debug, Serialize)]
struct SchemaVersion {
    major: u16,
    minor: u16,
}

#[derive(Debug, Serialize)]
struct RuleRecord {
    name: String,
    metric: String,
    op: String,
    threshold: f64,
    observed_value: Option<f64>,
    passed: Option<bool>,
    source_field_path: String,
    reason_if_failed_or_skipped: Option<String>,
}

// ── main output ───────────────────────────────────────────────────────────────

pub struct AssertReport {
    pub stdout: String,
    pub all_passed: bool,
}

// ── rule evaluation ───────────────────────────────────────────────────────────

fn evaluate_rules(rules: &[Rule], artifacts: &Artifacts) -> Result<Vec<RuleRecord>, Error> {
    let mut records: Vec<RuleRecord> = Vec::new();

    for rule in rules {
        let parsed_metric = parse_metric(&rule.metric)?;
        let source = metric_source(&parsed_metric);
        let source_field = metric_source_field_path(&parsed_metric);

        let evaluation_needed = matches!(source, SourceArtifact::Evaluation | SourceArtifact::Both);
        if evaluation_needed && artifacts.evaluation.is_none() {
            // Both fail-closed and --allow-missing-artifacts produce passed=null.
            // The distinction is in the all_passed calculation in run_assert.
            records.push(RuleRecord {
                name: rule.name.clone(),
                metric: rule.metric.clone(),
                op: rule.op.to_string(),
                threshold: rule.threshold,
                observed_value: None,
                passed: None,
                source_field_path: source_field,
                reason_if_failed_or_skipped: Some("missing_artifact: evaluation.json".to_owned()),
            });
            continue;
        }

        match extract_metric(&parsed_metric, artifacts) {
            None => {
                records.push(RuleRecord {
                    name: rule.name.clone(),
                    metric: rule.metric.clone(),
                    op: rule.op.to_string(),
                    threshold: rule.threshold,
                    observed_value: None,
                    passed: Some(false),
                    source_field_path: source_field,
                    reason_if_failed_or_skipped: Some(format!(
                        "metric '{}' could not be extracted",
                        rule.metric
                    )),
                });
            }
            Some(val) => {
                let pass = rule.op.evaluate(val, rule.threshold);
                // Non-finite values (∞, NaN) cannot be serialized as JSON numbers;
                // store None so the artifact is valid, but keep passed=Some(pass)
                // so downstream consumers don't confuse it with a skipped rule.
                let observed_value = if val.is_finite() { Some(val) } else { None };
                // Rust formats f64::INFINITY as "inf", NEG_INFINITY as "-inf"
                let reason = if pass {
                    None
                } else {
                    Some(format!(
                        "observed {val} {} {} is false",
                        rule.op.as_str(),
                        rule.threshold
                    ))
                };
                records.push(RuleRecord {
                    name: rule.name.clone(),
                    metric: rule.metric.clone(),
                    op: rule.op.to_string(),
                    threshold: rule.threshold,
                    observed_value,
                    passed: Some(pass),
                    source_field_path: source_field,
                    reason_if_failed_or_skipped: reason,
                });
            }
        }
    }
    Ok(records)
}

// ── artifact validation ───────────────────────────────────────────────────────

/// Delegate to the shared artifact classifier so that pre-versioning legacy
/// artifacts (missing both artifact_kind and schema_version) are accepted just
/// as they are by all other public readers in this codebase.
fn validate_artifact(v: &Value, kind: ArtifactKind, path: &Path) -> Result<(), Error> {
    classify_json_value(v, kind, path.display().to_string())
        .map(|_| ())
        .map_err(|e| Error::Config(ConfigError::Invalid(e.to_string())))
}

// ── entry point ───────────────────────────────────────────────────────────────

pub fn run_assert(args: &AssertArgs) -> Result<AssertReport, Error> {
    // Validate: exactly one of --rules or --rule must be provided.
    if args.rules_file.is_none() && args.inline_rules.is_empty() {
        return Err(Error::Config(ConfigError::Usage(
            "supply exactly one of --rules <file> or one or more --rule <metric><op><threshold>"
                .to_owned(),
        )));
    }
    if args.rules_file.is_some() && !args.inline_rules.is_empty() {
        return Err(Error::Config(ConfigError::Usage(
            "cannot supply both --rules and --rule; choose one".to_owned(),
        )));
    }

    // Load rules.
    let rules: Vec<Rule> = if let Some(ref path) = args.rules_file {
        load_rules_from_file(path)?
    } else {
        args.inline_rules
            .iter()
            .map(|r| parse_inline_rule(r))
            .collect::<Result<Vec<_>, _>>()?
    };

    if rules.is_empty() {
        return Err(Error::Config(ConfigError::Usage(
            "rule set is empty; provide at least one rule".to_owned(),
        )));
    }

    // Load results.json (always required).
    let results_path = args.sweep.join("results.json");
    if !results_path.exists() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "results.json not found in sweep directory: {}",
            args.sweep.display()
        ))));
    }
    let results_text = fs::read_to_string(&results_path)?;
    let results: Value = serde_json::from_str(&results_text)?;
    validate_artifact(&results, ArtifactKind::SweepResults, &results_path)?;

    // Load evaluation.json only when at least one rule actually needs it; a
    // stale or malformed evaluation artifact must not break results-only gates.
    let needs_evaluation = rules.iter().any(|rule| {
        parse_metric(&rule.metric)
            .map(|m| {
                matches!(
                    metric_source(&m),
                    SourceArtifact::Evaluation | SourceArtifact::Both
                )
            })
            .unwrap_or(false)
    });
    let evaluation_path = args.sweep.join("evaluation.json");
    let evaluation: Option<Value> = if needs_evaluation && evaluation_path.exists() {
        let text = fs::read_to_string(&evaluation_path)?;
        let v: Value = serde_json::from_str(&text)?;
        validate_artifact(&v, ArtifactKind::EvaluationResults, &evaluation_path)?;
        Some(v)
    } else {
        None
    };

    let artifacts = Artifacts {
        results,
        evaluation,
    };

    // Determine sweep_id from manifest or path.
    let sweep_id = artifacts.results["manifest"]["harness"]["git_sha"]
        .as_str()
        .map_or_else(|| args.sweep.display().to_string(), String::from);

    // Evaluate each rule.
    let records = evaluate_rules(&rules, &artifacts)?;

    // Compute counts.
    let passed_count = records.iter().filter(|r| r.passed == Some(true)).count();
    let failed_count = records.iter().filter(|r| r.passed == Some(false)).count();
    let skipped_count = records.iter().filter(|r| r.passed.is_none()).count();

    // Fail-closed: skipped counts as failure for the overall result.
    let all_passed = if args.allow_missing_artifacts {
        failed_count == 0
    } else {
        failed_count == 0 && skipped_count == 0
    };

    let artifact = AssertionsArtifact {
        artifact_kind: "assertions",
        schema_version: SchemaVersion { major: 1, minor: 0 },
        generated_at: Utc::now().to_rfc3339(),
        sweep_id,
        rules: records,
        passed: all_passed,
        passed_count,
        failed_count,
        skipped_count,
    };

    // Write assertions.json.
    let artifact_json = serde_json::to_string_pretty(&artifact)?;
    let out_path = args.sweep.join("assertions.json");
    fs::write(&out_path, &artifact_json)?;

    // Build stdout output.
    let stdout = build_stdout(&artifact, args);

    Ok(AssertReport { stdout, all_passed })
}

fn build_stdout(artifact: &AssertionsArtifact, args: &AssertArgs) -> String {
    let mut out = String::new();

    // Pytest-style header.
    if artifact.skipped_count > 0 {
        let _ = writeln!(
            out,
            "bench assert: {} — {} passed, {} failed, {} skipped",
            args.sweep.display(),
            artifact.passed_count,
            artifact.failed_count,
            artifact.skipped_count
        );
    } else {
        let _ = writeln!(
            out,
            "bench assert: {} — {} passed, {} failed",
            args.sweep.display(),
            artifact.passed_count,
            artifact.failed_count
        );
    }

    // Rule lines.
    for r in &artifact.rules {
        let val_str = |v: Option<f64>| v.map_or_else(|| "N/A".to_owned(), |f| format!("{f:.6}"));
        match r.passed {
            Some(true) => {
                if args.verbose {
                    let _ = writeln!(
                        out,
                        "PASSED {}: {}{}{}: observed {} ({})",
                        r.name,
                        r.metric,
                        r.op,
                        r.threshold,
                        val_str(r.observed_value),
                        r.source_field_path,
                    );
                }
            }
            Some(false) => {
                let reason_str = r
                    .reason_if_failed_or_skipped
                    .as_deref()
                    .map_or_else(String::new, |s| format!(" — {s}"));
                let _ = writeln!(
                    out,
                    "FAILED {}: {}{}{}: observed {} ({}){reason_str}",
                    r.name,
                    r.metric,
                    r.op,
                    r.threshold,
                    val_str(r.observed_value),
                    r.source_field_path,
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "SKIPPED {}: {}: {}",
                    r.name,
                    r.metric,
                    r.reason_if_failed_or_skipped.as_deref().unwrap_or(""),
                );
            }
        }
    }

    out
}
