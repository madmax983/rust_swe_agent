//! `bench assert` — evaluate operator-declared SLO rules against a completed sweep.
//!
//! Reads sweep artifacts (results.json, evaluation.json) and evaluates each rule
//! against the extracted metrics. Writes assertions.json next to results.json.
//! See `docs/spec-assert.md` for the full contract.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
            Self::Eq => (observed - threshold).abs() < f64::EPSILON * 1000.0,
            Self::Ne => (observed - threshold).abs() >= f64::EPSILON * 1000.0,
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
        "resolved_rate" => {
            "evaluation.json: resolved instances / total instances".to_owned()
        }
        "resolved_count" => "evaluation.json: count of instances where resolved=true".to_owned(),
        "unresolved_count" => {
            "evaluation.json: count of instances where resolved=false".to_owned()
        }
        "errored_count" => "results.json: .errored".to_owned(),
        "total_cost_usd" => "results.json: .total_cost_usd".to_owned(),
        "mean_cost_per_instance_usd" => {
            "results.json: .total_cost_usd / .total".to_owned()
        }
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
            let failure_cat = cap_failure_category(param);
            format!("results.json: count of .instances[] where failure_category=\"{failure_cat}\"")
        }
        other => format!("unknown metric: {other}"),
    }
}

fn cap_failure_category(param: &str) -> &str {
    match param {
        "steps" => "step_limit",
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

fn extract_metric(metric: &ParsedMetric, artifacts: &Artifacts) -> Option<f64> {
    let results = &artifacts.results;
    match metric.base.as_str() {
        "resolved_rate" => {
            let eval = artifacts.evaluation.as_ref()?;
            let instances = eval["instances"].as_array()?;
            let total = instances.len();
            if total == 0 {
                return Some(0.0);
            }
            let resolved = instances
                .iter()
                .filter(|i| i["resolved"] == true)
                .count();
            Some(resolved as f64 / total as f64)
        }
        "resolved_count" => {
            let eval = artifacts.evaluation.as_ref()?;
            let instances = eval["instances"].as_array()?;
            let resolved = instances
                .iter()
                .filter(|i| i["resolved"] == true)
                .count();
            Some(resolved as f64)
        }
        "unresolved_count" => {
            let eval = artifacts.evaluation.as_ref()?;
            let instances = eval["instances"].as_array()?;
            let unresolved = instances
                .iter()
                .filter(|i| i["resolved"] == false)
                .count();
            Some(unresolved as f64)
        }
        "errored_count" => results["errored"].as_f64(),
        "total_cost_usd" => results["total_cost_usd"].as_f64(),
        "mean_cost_per_instance_usd" => {
            let total_cost = results["total_cost_usd"].as_f64()?;
            let total = results["total"].as_f64()?;
            if total == 0.0 {
                Some(0.0)
            } else {
                Some(total_cost / total)
            }
        }
        "cost_per_resolved_instance_usd" => {
            let total_cost = results["total_cost_usd"].as_f64()?;
            let eval = artifacts.evaluation.as_ref()?;
            let instances = eval["instances"].as_array()?;
            let resolved = instances
                .iter()
                .filter(|i| i["resolved"] == true)
                .count();
            if resolved == 0 {
                Some(f64::INFINITY)
            } else {
                Some(total_cost / resolved as f64)
            }
        }
        "mean_steps" => {
            let instances = results["instances"].as_array()?;
            let steps: Vec<f64> = instances
                .iter()
                .filter_map(|i| i["steps"].as_f64())
                .collect();
            if steps.is_empty() {
                return Some(0.0);
            }
            Some(steps.iter().sum::<f64>() / steps.len() as f64)
        }
        "p95_steps" => {
            let instances = results["instances"].as_array()?;
            let mut steps: Vec<f64> = instances
                .iter()
                .filter_map(|i| i["steps"].as_f64())
                .collect();
            if steps.is_empty() {
                return Some(0.0);
            }
            steps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let idx = ((steps.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
            let idx = idx.min(steps.len() - 1);
            Some(steps[idx])
        }
        "wallclock_total_s" => {
            let started = results["manifest"]["runtime"]["started_at_utc"].as_str()?;
            let finished = results["manifest"]["runtime"]["finished_at_utc"].as_str()?;
            let started_dt = chrono::DateTime::parse_from_rfc3339(started).ok()?;
            let finished_dt = chrono::DateTime::parse_from_rfc3339(finished).ok()?;
            let duration = finished_dt.signed_duration_since(started_dt);
            Some(duration.num_seconds() as f64)
        }
        "failure_category_count" => {
            let param = metric.param.as_deref().unwrap_or("");
            results["failures_by_category"][param]
                .as_f64()
                .or(Some(0.0))
        }
        "failure_category_share" => {
            let param = metric.param.as_deref().unwrap_or("");
            let count = results["failures_by_category"][param]
                .as_f64()
                .unwrap_or(0.0);
            let total = results["total"].as_f64()?;
            if total == 0.0 {
                Some(0.0)
            } else {
                Some(count / total)
            }
        }
        "at_cap_count" => {
            let param = metric.param.as_deref().unwrap_or("");
            let failure_cat = cap_failure_category(param);
            let instances = results["instances"].as_array()?;
            let count = instances
                .iter()
                .filter(|i| i["failure_category"].as_str() == Some(failure_cat))
                .count();
            Some(count as f64)
        }
        _ => None,
    }
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

// ── entry point ───────────────────────────────────────────────────────────────

pub fn run_assert(args: &AssertArgs) -> Result<AssertReport, Error> {
    // Validate: exactly one of --rules or --rule must be provided.
    if args.rules_file.is_none() && args.inline_rules.is_empty() {
        return Err(Error::Config(ConfigError::Usage(
            "supply exactly one of --rules <file> or one or more --rule <metric><op><threshold>"
                .to_owned(),
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

    // Load evaluation.json (optional; absence triggers skip/fail depending on flag).
    let evaluation_path = args.sweep.join("evaluation.json");
    let evaluation: Option<Value> = if evaluation_path.exists() {
        let text = fs::read_to_string(&evaluation_path)?;
        Some(serde_json::from_str(&text)?)
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
        .map(String::from)
        .unwrap_or_else(|| args.sweep.display().to_string());

    // Evaluate each rule.
    let mut records: Vec<RuleRecord> = Vec::new();

    for rule in &rules {
        let parsed_metric = parse_metric(&rule.metric)?;
        let source = metric_source(&parsed_metric);
        let source_field = metric_source_field_path(&parsed_metric);

        // Check if required artifact is available.
        let evaluation_needed = matches!(source, SourceArtifact::Evaluation | SourceArtifact::Both);
        if evaluation_needed && artifacts.evaluation.is_none() {
            let reason = "missing_artifact: evaluation.json".to_owned();
            if args.allow_missing_artifacts {
                // Skip (passed = null).
                records.push(RuleRecord {
                    name: rule.name.clone(),
                    metric: rule.metric.clone(),
                    op: rule.op.to_string(),
                    threshold: rule.threshold,
                    observed_value: None,
                    passed: None,
                    source_field_path: source_field,
                    reason_if_failed_or_skipped: Some(reason),
                });
            } else {
                // Fail-closed (passed = null, counted as failure for exit code).
                records.push(RuleRecord {
                    name: rule.name.clone(),
                    metric: rule.metric.clone(),
                    op: rule.op.to_string(),
                    threshold: rule.threshold,
                    observed_value: None,
                    passed: None,
                    source_field_path: source_field,
                    reason_if_failed_or_skipped: Some(reason),
                });
            }
            continue;
        }

        // Extract metric value.
        let observed = extract_metric(&parsed_metric, &artifacts);
        match observed {
            None => {
                let reason = format!("metric '{}' could not be extracted", rule.metric);
                records.push(RuleRecord {
                    name: rule.name.clone(),
                    metric: rule.metric.clone(),
                    op: rule.op.to_string(),
                    threshold: rule.threshold,
                    observed_value: None,
                    passed: Some(false),
                    source_field_path: source_field,
                    reason_if_failed_or_skipped: Some(reason),
                });
            }
            Some(val) => {
                let pass = rule.op.evaluate(val, rule.threshold);
                let reason = if pass {
                    None
                } else {
                    Some(format!(
                        "observed {val} {op} {threshold} is false",
                        op = rule.op.as_str(),
                        threshold = rule.threshold
                    ))
                };
                records.push(RuleRecord {
                    name: rule.name.clone(),
                    metric: rule.metric.clone(),
                    op: rule.op.to_string(),
                    threshold: rule.threshold,
                    observed_value: Some(val),
                    passed: Some(pass),
                    source_field_path: source_field,
                    reason_if_failed_or_skipped: reason,
                });
            }
        }
    }

    // Compute counts.
    let passed_count = records
        .iter()
        .filter(|r| r.passed == Some(true))
        .count();
    let failed_count = records
        .iter()
        .filter(|r| r.passed == Some(false))
        .count();
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
    let header = if artifact.skipped_count > 0 {
        format!(
            "bench assert: {} — {} passed, {} failed, {} skipped\n",
            args.sweep.display(),
            artifact.passed_count,
            artifact.failed_count,
            artifact.skipped_count
        )
    } else {
        format!(
            "bench assert: {} — {} passed, {} failed\n",
            args.sweep.display(),
            artifact.passed_count,
            artifact.failed_count
        )
    };
    out.push_str(&header);

    // Rule lines.
    for r in &artifact.rules {
        match r.passed {
            Some(true) => {
                if args.verbose {
                    out.push_str(&format!(
                        "PASSED {name}: {metric}{op}{threshold}: observed {val:.6} ({source})\n",
                        name = r.name,
                        metric = r.metric,
                        op = r.op,
                        threshold = r.threshold,
                        val = r.observed_value.unwrap_or(0.0),
                        source = r.source_field_path,
                    ));
                }
            }
            Some(false) => {
                out.push_str(&format!(
                    "FAILED {name}: {metric}{op}{threshold}: observed {val:.6} ({source})\n",
                    name = r.name,
                    metric = r.metric,
                    op = r.op,
                    threshold = r.threshold,
                    val = r.observed_value.unwrap_or(0.0),
                    source = r.source_field_path,
                ));
            }
            None => {
                out.push_str(&format!(
                    "SKIPPED {name}: {metric}: {reason}\n",
                    name = r.name,
                    metric = r.metric,
                    reason = r.reason_if_failed_or_skipped.as_deref().unwrap_or(""),
                ));
            }
        }
    }

    out
}
