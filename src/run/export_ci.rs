//! `bench export-ci`: convert a completed sweep to JUnit XML and/or GitHub Actions annotations.
//!
//! Reads only on-disk artifacts (no model calls, no network). Applies the
//! existing secret-redaction guarantee to all text excerpts before writing
//! any output.

#![allow(clippy::cast_precision_loss)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::config::RedactionCfg;
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::swebench::{InstanceResult, SweepResults, trajectory_path_for};
use crate::trajectory::FailureCategory;

const EXCERPT_MAX_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportCiFormat {
    Junit,
    GithubAnnotations,
    Both,
}

#[derive(Debug, Clone)]
pub struct ExportCiArgs {
    pub sweep_dir: PathBuf,
    pub format: ExportCiFormat,
    /// Override output path for JUnit XML. Defaults to `<sweep_dir>/junit.xml`.
    pub output: Option<PathBuf>,
}

/// Return value from `run`: aggregate counts used by the CLI dispatch to emit
/// the integrity-violation exit code without duplicating logic.
#[derive(Debug, Clone)]
pub struct ExportCiResult {
    pub integrity_violation: bool,
    pub xml_tests: usize,
    pub xml_failures: usize,
    pub xml_errors: usize,
    pub json_total: usize,
    pub json_failures: usize,
    pub json_errors: usize,
}

// ── Optional triage data ───────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
struct TriageClusterMin {
    failure_category: String,
    #[allow(dead_code)]
    signature_summary: String,
    instance_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct TriageReportMin {
    clusters: Vec<TriageClusterMin>,
}

fn load_triage(sweep_dir: &Path) -> Option<HashMap<String, String>> {
    let path = sweep_dir.join("triage.json");
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let report: TriageReportMin = serde_json::from_str(&text).ok()?;
    let mut map = HashMap::new();
    for cluster in report.clusters {
        for id in cluster.instance_ids {
            map.entry(id)
                .or_insert_with(|| cluster.failure_category.clone());
        }
    }
    Some(map)
}

// ── Entry point ────────────────────────────────────────────────────────────

pub fn run(args: &ExportCiArgs) -> Result<ExportCiResult, Error> {
    let sweep = load_results(&args.sweep_dir)?;
    let triage_categories = load_triage(&args.sweep_dir);

    let redactor = build_redactor(&sweep);

    let instances: Vec<&InstanceResult> = {
        let mut v: Vec<&InstanceResult> = sweep.instances.iter().collect();
        v.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        v
    };

    let agg = compute_aggregates(&sweep, &instances);

    let result = ExportCiResult {
        integrity_violation: integrity_mismatch(&agg, &sweep),
        xml_tests: agg.tests,
        xml_failures: agg.failures,
        xml_errors: agg.errors,
        json_total: sweep.total,
        json_failures: agg.json_failures_expected,
        json_errors: sweep.errored,
    };

    match args.format {
        ExportCiFormat::Junit => {
            let junit_path = junit_output_path(args);
            let xml = render_junit(
                &instances,
                &agg,
                triage_categories.as_ref(),
                &redactor,
                &args.sweep_dir,
            );
            write_output(&junit_path, &xml)?;
        }
        ExportCiFormat::GithubAnnotations => {
            let annotations = render_annotations(
                &instances,
                triage_categories.as_ref(),
                &redactor,
                &args.sweep_dir,
            );
            print!("{annotations}");
        }
        ExportCiFormat::Both => {
            let junit_path = junit_output_path(args);
            let xml = render_junit(
                &instances,
                &agg,
                triage_categories.as_ref(),
                &redactor,
                &args.sweep_dir,
            );
            write_output(&junit_path, &xml)?;
            let annotations = render_annotations(
                &instances,
                triage_categories.as_ref(),
                &redactor,
                &args.sweep_dir,
            );
            print!("{annotations}");
        }
    }

    Ok(result)
}

// ── Aggregate computation ──────────────────────────────────────────────────

struct Aggregates {
    tests: usize,
    /// JUnit `failures` = submitted-but-unresolved instances.
    failures: usize,
    /// JUnit `errors` = errored instances.
    errors: usize,
    /// Expected `failures` from results.json for integrity check.
    json_failures_expected: usize,
    time_secs: f64,
}

fn compute_aggregates(sweep: &SweepResults, instances: &[&InstanceResult]) -> Aggregates {
    use crate::run::swebench::resolved_count;
    use crate::trajectory::outcome;

    let tests = instances.len();
    let errors = instances
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
        .count();
    let resolved = instances.iter().filter(|r| resolved_count(r) > 0).count();
    let failures = tests.saturating_sub(resolved).saturating_sub(errors);
    let json_failures_expected = sweep
        .submitted
        .saturating_sub(instances.iter().filter(|r| resolved_count(r) > 0).count());
    let time_secs: f64 = instances.iter().filter_map(|r| r.duration_secs).sum();

    Aggregates {
        tests,
        failures,
        errors,
        json_failures_expected,
        time_secs,
    }
}

fn integrity_mismatch(agg: &Aggregates, sweep: &SweepResults) -> bool {
    agg.tests != sweep.total || agg.errors != sweep.errored
}

// ── JUnit XML rendering ────────────────────────────────────────────────────

fn render_junit(
    instances: &[&InstanceResult],
    agg: &Aggregates,
    triage: Option<&HashMap<String, String>>,
    redactor: &Redactor,
    sweep_dir: &Path,
) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        out,
        "<testsuites tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\">",
        agg.tests, agg.failures, agg.errors, agg.time_secs,
    );
    let _ = writeln!(
        out,
        "  <testsuite name=\"swe-bench\" tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\">",
        agg.tests, agg.failures, agg.errors, agg.time_secs,
    );

    for inst in instances {
        let classname = parse_classname(&inst.instance_id);
        let time = inst.duration_secs.unwrap_or(0.0);
        let traj_rel = trajectory_relative_path(sweep_dir, &inst.instance_id);

        if is_resolved(inst) {
            let _ = writeln!(
                out,
                "    <testcase name=\"{}\" classname=\"{}\" time=\"{:.3}\"/>",
                xml_attr(&inst.instance_id),
                xml_attr(&classname),
                time,
            );
        } else {
            let failure_msg = failure_message(inst, triage);
            let excerpt = build_excerpt(inst, redactor);
            let _ = writeln!(
                out,
                "    <testcase name=\"{}\" classname=\"{}\" time=\"{:.3}\" file=\"{}\">",
                xml_attr(&inst.instance_id),
                xml_attr(&classname),
                time,
                xml_attr(&traj_rel),
            );
            let _ = writeln!(
                out,
                "      <failure message=\"{}\">{}</failure>",
                xml_attr(&failure_msg),
                xml_text(&excerpt),
            );
            out.push_str("    </testcase>\n");
        }
    }

    out.push_str("  </testsuite>\n");
    out.push_str("</testsuites>\n");
    out
}

// ── GitHub Annotations rendering ──────────────────────────────────────────

fn render_annotations(
    instances: &[&InstanceResult],
    triage: Option<&HashMap<String, String>>,
    redactor: &Redactor,
    sweep_dir: &Path,
) -> String {
    let mut out = String::new();
    for inst in instances {
        if is_resolved(inst) {
            continue;
        }
        let traj_rel = trajectory_relative_path(sweep_dir, &inst.instance_id);
        let failure_msg = failure_message(inst, triage);
        let excerpt = build_excerpt(inst, redactor);
        // GitHub Actions workflow command format:
        //   ::error title=INSTANCE_ID,file=PATH::MESSAGE
        let message = if excerpt.is_empty() {
            failure_msg.clone()
        } else {
            format!("{failure_msg}: {excerpt}")
        };
        let message = message.replace('\n', " ").replace('\r', "");
        let _ = writeln!(
            out,
            "::error title={},file={}::{}",
            inst.instance_id, traj_rel, message,
        );
    }
    out
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn load_results(sweep_dir: &Path) -> Result<SweepResults, Error> {
    use crate::artifact::{ArtifactKind, classify_json_value};

    let results_path = sweep_dir.join("results.json");
    let text = std::fs::read_to_string(&results_path).map_err(|e| {
        Error::Trajectory(format!(
            "bench export-ci: cannot read {}: {e}",
            results_path.display()
        ))
    })?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Trajectory(format!("bench export-ci: malformed results.json: {e}")))?;
    classify_json_value(
        &value,
        ArtifactKind::SweepResults,
        results_path.display().to_string(),
    )
    .map_err(|e| Error::Trajectory(e.to_string()))?;
    let results: SweepResults = serde_json::from_value(value).map_err(|e| {
        Error::Trajectory(format!("bench export-ci: cannot deserialize results: {e}"))
    })?;
    Ok(results)
}

fn build_redactor(sweep: &SweepResults) -> Redactor {
    let mut cfg = RedactionCfg::default();
    if let Some(manifest) = &sweep.manifest {
        let resolved = &manifest.config.resolved;
        if let Ok(parsed) = toml::from_str::<ResolvedConfigForRedaction>(resolved) {
            if let Some(rc) = parsed.redaction {
                cfg.secret_literals.extend(rc.secret_literals);
                cfg.custom_patterns.extend(rc.custom_patterns);
            }
        }
    }
    Redactor::from_config_lossy(&cfg)
}

#[derive(serde::Deserialize)]
struct ResolvedConfigForRedaction {
    #[serde(default)]
    redaction: Option<RedactionFields>,
}

#[derive(serde::Deserialize)]
struct RedactionFields {
    #[serde(default)]
    secret_literals: Vec<String>,
    #[serde(default)]
    custom_patterns: Vec<String>,
}

fn is_resolved(inst: &InstanceResult) -> bool {
    crate::run::swebench::is_resolved_instance_result(inst)
}

fn failure_message(inst: &InstanceResult, triage: Option<&HashMap<String, String>>) -> String {
    // Prefer triage cluster category for the message.
    if let Some(triage_map) = triage {
        if let Some(cat) = triage_map.get(&inst.instance_id) {
            return cat.clone();
        }
    }
    // Fall back to the instance's own failure_category.
    if let Some(cat) = inst.failure_category {
        return failure_category_label(cat).to_owned();
    }
    "unresolved".to_owned()
}

fn build_excerpt(inst: &InstanceResult, redactor: &Redactor) -> String {
    let raw = inst
        .error
        .as_deref()
        .unwrap_or("")
        .chars()
        .take(EXCERPT_MAX_CHARS)
        .collect::<String>();
    if raw.is_empty() {
        return String::new();
    }
    redactor.redact_text(&raw, surface::EXPORT).text
}

fn failure_category_label(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::BudgetExhausted => "budget_exhausted",
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::AgentStagnation => "agent_stagnation",
        FailureCategory::HistoryCompactionFailed => "history_compaction_failed",
        FailureCategory::ReadOnlyViolation => "read_only_violation",
        FailureCategory::Unknown => "unknown",
    }
}

/// Parse JUnit classname from SWE-bench instance ID.
///
/// `django__django-12345` → `django.django`
/// Falls back to the full instance ID for non-standard formats.
fn parse_classname(instance_id: &str) -> String {
    let Some((org, rest)) = instance_id.split_once("__") else {
        return instance_id.to_owned();
    };
    // Strip trailing numeric issue number: `-\d+` suffix.
    let repo = strip_issue_number(rest);
    format!("{org}.{repo}")
}

fn strip_issue_number(s: &str) -> &str {
    // Find the last `-` followed only by digits.
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i > 0 && i < bytes.len() && bytes[i - 1] == b'-' {
        &s[..i - 1]
    } else {
        s
    }
}

/// Return the trajectory path relative to the sweep directory.
fn trajectory_relative_path(sweep_dir: &Path, instance_id: &str) -> String {
    let abs = trajectory_path_for(sweep_dir, instance_id);
    if let Ok(rel) = abs.strip_prefix(sweep_dir) {
        rel.to_string_lossy().into_owned()
    } else {
        // Legacy flat path fallback.
        format!("{instance_id}.traj.json")
    }
}

fn junit_output_path(args: &ExportCiArgs) -> PathBuf {
    args.output
        .clone()
        .unwrap_or_else(|| args.sweep_dir.join("junit.xml"))
}

fn write_output(path: &Path, content: &str) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, content)?;
    Ok(())
}

// ── XML helpers ────────────────────────────────────────────────────────────

/// Escape a string for use in an XML attribute value (double-quoted).
fn xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Escape a string for use as XML text content.
fn xml_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_classname_standard() {
        assert_eq!(parse_classname("django__django-12345"), "django.django");
    }

    #[test]
    fn parse_classname_hyphenated_repo() {
        assert_eq!(
            parse_classname("scikit-learn__scikit-learn-12345"),
            "scikit-learn.scikit-learn"
        );
    }

    #[test]
    fn parse_classname_no_double_underscore_fallback() {
        assert_eq!(parse_classname("model-a-1"), "model-a-1");
    }

    #[test]
    fn xml_attr_escapes_specials() {
        assert_eq!(xml_attr("a<b>c\"d&e"), "a&lt;b&gt;c&quot;d&amp;e");
    }

    #[test]
    fn xml_text_escapes_specials() {
        assert_eq!(xml_text("a<b>c&d"), "a&lt;b&gt;c&amp;d");
    }

    #[test]
    fn strip_issue_number_strips_suffix() {
        assert_eq!(strip_issue_number("django-12345"), "django");
        assert_eq!(strip_issue_number("scikit-learn-12345"), "scikit-learn");
        assert_eq!(strip_issue_number("no-number"), "no-number");
    }

    #[test]
    fn artifact_integrity_violation_exit_code_is_22() {
        assert_eq!(
            crate::exit_code::ExitCode::ArtifactIntegrityViolation.as_i32(),
            22
        );
        assert_eq!(
            crate::exit_code::ExitCode::ArtifactIntegrityViolation.outcome_class(),
            "artifact_integrity_violation"
        );
    }
}
