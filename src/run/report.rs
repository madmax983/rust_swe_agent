//! `bench report`: produce a shareable markdown or HTML sweep summary.
//!
//! Reads a completed sweep directory (containing `results.json` and optionally
//! `evaluation.json`) and renders a self-contained report file. The report is
//! deterministic for a fixed input sweep.

#![allow(clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion, classify_json_value};
use crate::error::Error;
use crate::run::compare::{self, CompareReport, LoadedSweep, load_sweep};
use crate::run::evaluate::{BreakdownSelection, EvaluationResults, evaluation_path};
use crate::run::swebench::{
    InstanceResult, ProvenanceManifest, effective_runs, pass_at_1 as sweep_pass_at_1,
    resolved_count as sweep_resolved_count,
};
use crate::trajectory::Trajectory;

const NO_EVAL_MSG: &str = "_no evaluation data — run `bench evaluate` to populate_";
const EXCERPT_LEN: usize = 120;
const TOP_DELTA_LIST: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Markdown,
    Html,
}

#[derive(Debug, Clone)]
pub struct ReportArgs {
    pub sweep_dir: PathBuf,
    pub output: PathBuf,
    pub baseline: Option<PathBuf>,
    pub top_failures: usize,
    pub format: ReportFormat,
}

pub fn run(args: &ReportArgs) -> Result<(), Error> {
    let content = generate(args)?;
    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&args.output, content)?;
    Ok(())
}

fn generate(args: &ReportArgs) -> Result<String, Error> {
    let loaded = load_sweep(&args.sweep_dir)?;
    let eval = load_evaluation(&args.sweep_dir)?;

    // Sort instances deterministically by instance_id.
    let mut instances: Vec<&InstanceResult> = loaded.instances.values().collect();
    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let baseline_report = baseline_compare_report(args)?;

    let md = render_markdown(
        args,
        &loaded,
        &instances,
        eval.as_ref(),
        baseline_report.as_ref(),
    );
    match args.format {
        ReportFormat::Markdown => Ok(md),
        ReportFormat::Html => Ok(md_to_html(&md)),
    }
}

fn load_evaluation(sweep_dir: &Path) -> Result<Option<EvaluationResults>, Error> {
    let path = evaluation_path(sweep_dir);
    if !path.exists() {
        return Ok(None);
    }
    // File present: surface parse / IO / schema errors rather than silently
    // degrading to the missing-evaluation placeholder. A corrupt or
    // schema-incompatible evaluation.json must not produce a plausible-but-wrong
    // summary. We validate the artifact header (kind + schema version) via the
    // same `classify_json_value` path the rest of the sweep tooling uses, so a
    // wrong-kind or future-incompatible artifact is rejected even if the
    // remaining fields would happen to deserialize.
    let text = std::fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(
        &value,
        ArtifactKind::EvaluationResults,
        path.display().to_string(),
    )
    .map_err(|err| Error::Trajectory(err.to_string()))?;
    let eval: EvaluationResults = serde_json::from_value(value)?;
    Ok(Some(eval))
}

fn baseline_compare_report(args: &ReportArgs) -> Result<Option<CompareReport>, Error> {
    let Some(baseline) = args.baseline.clone() else {
        return Ok(None);
    };
    let report = compare::compute(&compare::CompareArgs {
        baseline,
        candidate: args.sweep_dir.clone(),
        format: compare::CompareFormat::Json,
        max_regressions: None,
        max_patch_size_regression_pct: None,
        breakdown: BreakdownSelection::none(),
        min_delta_pp: 0.05,
        cost_attribution: false,
        cost_attribution_min_delta_usd: 1.0,
    })?;
    Ok(Some(report))
}

// ── Markdown rendering ─────────────────────────────────────────────────────

fn render_markdown(
    args: &ReportArgs,
    loaded: &LoadedSweep,
    instances: &[&InstanceResult],
    eval: Option<&EvaluationResults>,
    baseline_report: Option<&CompareReport>,
) -> String {
    let model = loaded.manifest.as_ref().map(|m| m.model.name.as_str());
    let mut buf = String::new();
    md_header(&mut buf, loaded);
    md_provenance(&mut buf, loaded.manifest.as_ref(), instances);
    md_topline(&mut buf, instances, eval, model);
    md_failure_mix(&mut buf, instances, eval, model);
    md_top_failures(
        &mut buf,
        args.top_failures,
        instances,
        eval,
        model,
        &args.sweep_dir,
    );
    md_eval_section(&mut buf, eval);
    if let Some(report) = baseline_report {
        md_baseline_delta(&mut buf, report);
    }
    buf
}

fn md_header(buf: &mut String, loaded: &LoadedSweep) {
    let schema_ver = loaded
        .artifact
        .as_ref()
        .and_then(|a| a.version)
        .unwrap_or(ArtifactSchemaVersion::CURRENT);
    writeln!(buf, "# Sweep Report").ok();
    writeln!(buf).ok();
    writeln!(
        buf,
        "> Artifact schema version: v{schema_ver} | Generated by rust-swe-agent"
    )
    .ok();
    writeln!(buf).ok();
}

fn md_provenance(
    buf: &mut String,
    manifest: Option<&ProvenanceManifest>,
    instances: &[&InstanceResult],
) {
    writeln!(buf, "## Provenance").ok();
    writeln!(buf).ok();
    writeln!(buf, "| Field | Value |").ok();
    writeln!(buf, "|---|---|").ok();

    let Some(m) = manifest else {
        writeln!(buf, "| _provenance not available_ | — |").ok();
        writeln!(buf).ok();
        return;
    };

    writeln!(buf, "| Model | {} |", m.model.name).ok();
    if let Some(sha) = &m.harness.git_sha {
        writeln!(buf, "| Harness git SHA | {sha} |").ok();
    }
    writeln!(buf, "| Dataset | {} |", m.dataset.path).ok();
    if let Some(split) = &m.dataset.split {
        writeln!(buf, "| Dataset split | {split} |").ok();
    }
    if let Some(reproduced_from) = &m.reproduced_from {
        writeln!(buf, "| Reproduced from | {} |", reproduced_from.sweep_dir).ok();
    }
    writeln!(buf, "| Started | {} |", m.runtime.started_at_utc).ok();
    if let Some(finished) = &m.runtime.finished_at_utc {
        writeln!(buf, "| Finished | {finished} |").ok();
        if let (Ok(start), Ok(end)) = (
            chrono::DateTime::parse_from_rfc3339(&m.runtime.started_at_utc),
            chrono::DateTime::parse_from_rfc3339(finished),
        ) {
            let raw_secs = end.signed_duration_since(start).num_seconds().max(0);
            #[allow(clippy::cast_sign_loss)]
            let secs = raw_secs as u64;
            let h = secs / 3600;
            let min = (secs % 3600) / 60;
            let s = secs % 60;
            if h > 0 {
                writeln!(buf, "| Wallclock | {h}h {min}m {s}s |").ok();
            } else {
                writeln!(buf, "| Wallclock | {min}m {s}s |").ok();
            }
        }
    }

    let runs_per: u32 = instances.first().map_or(1, |i| effective_runs(i));
    let total_runs: u32 = instances.iter().map(|i| effective_runs(i)).sum();
    writeln!(buf, "| Runs per instance | {runs_per} |").ok();
    writeln!(buf, "| Total runs | {total_runs} |").ok();

    writeln!(buf).ok();
}

fn md_topline(
    buf: &mut String,
    instances: &[&InstanceResult],
    eval: Option<&EvaluationResults>,
    model: Option<&str>,
) {
    let total = instances.len();
    let resolved = instances.iter().filter(|i| is_resolved(i, eval)).count();
    let resolved_rate = pct(resolved, total);
    let pass_at_1 = pct(
        instances.iter().filter(|i| is_pass_at_1(i, eval)).count(),
        total,
    );
    let pass_at_k = resolved_rate;
    let total_cost: f64 = instances.iter().map(|i| cost_for(i, model)).sum();
    let cost_per_resolved = if resolved > 0 {
        total_cost / resolved as f64
    } else {
        f64::NAN
    };

    writeln!(buf, "## Top-Line Metrics").ok();
    writeln!(buf).ok();
    writeln!(buf, "| Metric | Value |").ok();
    writeln!(buf, "|---|---|").ok();
    writeln!(buf, "| Total instances | {total} |").ok();
    writeln!(buf, "| Resolved | {resolved} ({resolved_rate:.2}%) |").ok();
    writeln!(buf, "| Pass@1 | {pass_at_1:.2}% |").ok();
    writeln!(buf, "| Pass@k | {pass_at_k:.2}% |").ok();
    writeln!(buf, "| Total cost USD | ${total_cost:.4} |").ok();
    if cost_per_resolved.is_nan() || cost_per_resolved.is_infinite() {
        writeln!(buf, "| $/resolved instance | — |").ok();
    } else {
        writeln!(buf, "| $/resolved instance | ${cost_per_resolved:.4} |").ok();
    }

    let mean_lines = eval.and_then(|e| {
        let lines: Vec<u32> = e
            .instances
            .iter()
            .filter(|ie| ie.resolved)
            .filter_map(|ie| ie.patch_stats.as_ref())
            .map(|ps| ps.lines_added + ps.lines_removed)
            .collect();
        if lines.is_empty() {
            None
        } else {
            let total: u32 = lines.iter().sum();
            Some(f64::from(total) / lines.len() as f64)
        }
    });
    match mean_lines {
        Some(ml) => writeln!(buf, "| Mean lines changed (resolved) | {ml:.1} |").ok(),
        None => writeln!(buf, "| Mean lines changed (resolved) | {NO_EVAL_MSG} |").ok(),
    };
    writeln!(buf).ok();
}

fn md_failure_mix(
    buf: &mut String,
    instances: &[&InstanceResult],
    eval: Option<&EvaluationResults>,
    model: Option<&str>,
) {
    let total = instances.len();
    let total_cost: f64 = instances.iter().map(|i| cost_for(i, model)).sum();

    writeln!(buf, "## Failure Mix").ok();
    writeln!(buf).ok();
    writeln!(buf, "| Category | n | share% | cost_usd | cost% |").ok();
    writeln!(buf, "|---|---|---|---|---|").ok();

    // Resolved row first.
    let resolved_n = instances.iter().filter(|i| is_resolved(i, eval)).count();
    let resolved_cost: f64 = instances
        .iter()
        .filter(|i| is_resolved(i, eval))
        .map(|i| cost_for(i, model))
        .sum();
    let resolved_share = pct(resolved_n, total);
    let resolved_cost_share = cost_pct(resolved_cost, total_cost);
    writeln!(
        buf,
        "| resolved | {resolved_n} | {resolved_share:.2}% | ${resolved_cost:.4} | {resolved_cost_share:.2}% |"
    )
    .ok();

    // Group unresolved instances by category.
    let mut by_cat: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for inst in instances.iter().filter(|i| !is_resolved(i, eval)) {
        let cat = instance_category(inst, eval);
        let e = by_cat.entry(cat).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += cost_for(inst, model);
    }

    let mut rows: Vec<(String, usize, f64)> = by_cat
        .into_iter()
        .map(|(cat, (n, cost))| (cat, n, cost))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    for (cat, n, cost) in &rows {
        let share = pct(*n, total);
        let cost_share = cost_pct(*cost, total_cost);
        writeln!(
            buf,
            "| {cat} | {n} | {share:.2}% | ${cost:.4} | {cost_share:.2}% |"
        )
        .ok();
    }
    writeln!(buf).ok();
}

fn md_top_failures(
    buf: &mut String,
    top_n: usize,
    instances: &[&InstanceResult],
    eval: Option<&EvaluationResults>,
    model: Option<&str>,
    sweep_dir: &Path,
) {
    writeln!(buf, "## Top Failed Instances (top {top_n})").ok();
    writeln!(buf).ok();
    writeln!(
        buf,
        "| Instance | Category | Resolved/Total | Cost USD | Excerpt |"
    )
    .ok();
    writeln!(buf, "|---|---|---|---|---|").ok();

    let mut failed: Vec<&InstanceResult> = instances
        .iter()
        .copied()
        .filter(|i| !is_resolved(i, eval))
        .collect();
    failed.sort_by(|a, b| {
        cost_for(b, model)
            .partial_cmp(&cost_for(a, model))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.instance_id.cmp(&b.instance_id))
    });

    for inst in failed.iter().take(top_n) {
        let cat = instance_category(inst, eval);
        let (resolved_count, runs) = resolved_count_and_runs(inst, eval);
        let ratio = format!("{resolved_count}/{runs}");
        let cost = match inst.effective_cost_usd(model) {
            Some(c) => format!("${c:.4}"),
            None => "—".into(),
        };
        let excerpt = trajectory_excerpt(sweep_dir, &inst.instance_id);
        writeln!(
            buf,
            "| {} | {cat} | {ratio} | {cost} | {excerpt} |",
            inst.instance_id
        )
        .ok();
    }
    writeln!(buf).ok();
}

fn md_eval_section(buf: &mut String, eval: Option<&EvaluationResults>) {
    writeln!(buf, "## Evaluation").ok();
    writeln!(buf).ok();
    if eval.is_none() {
        writeln!(buf, "{NO_EVAL_MSG}").ok();
    } else {
        writeln!(
            buf,
            "_Evaluation data available. See failure mix table above for per-category breakdown._"
        )
        .ok();
    }
    writeln!(buf).ok();
}

fn md_baseline_delta(buf: &mut String, report: &CompareReport) {
    writeln!(buf, "## Delta vs Baseline").ok();
    writeln!(buf).ok();
    writeln!(buf, "Baseline: `{}`", report.baseline_dir.display()).ok();
    writeln!(buf, "Candidate: `{}`", report.candidate_dir.display()).ok();
    writeln!(buf).ok();
    writeln!(buf, "| Metric | Value |").ok();
    writeln!(buf, "|---|---|").ok();
    writeln!(
        buf,
        "| Baseline resolved | {} ({:.2}%) |",
        report.baseline_resolved,
        report.baseline_resolved_rate * 100.0
    )
    .ok();
    writeln!(
        buf,
        "| Candidate resolved | {} ({:.2}%) |",
        report.candidate_resolved,
        report.candidate_resolved_rate * 100.0
    )
    .ok();
    writeln!(
        buf,
        "| Resolved delta | {} ({:+.2}pp) |",
        report.resolved_delta,
        report.resolved_delta_rate * 100.0
    )
    .ok();
    writeln!(
        buf,
        "| Resolved delta 95% CI | [{:+.2}pp, {:+.2}pp] |",
        report.resolved_delta_ci95.lower * 100.0,
        report.resolved_delta_ci95.upper * 100.0
    )
    .ok();
    writeln!(buf, "| Within noise | {} |", report.within_noise).ok();
    writeln!(buf, "| Verdict | {:?} |", report.verdict).ok();
    writeln!(buf).ok();

    // Top regressions.
    writeln!(buf, "### Top Regressions").ok();
    writeln!(buf).ok();
    if report.regressions.is_empty() {
        writeln!(buf, "_None._").ok();
    } else {
        writeln!(buf, "| Instance | Baseline | Candidate |").ok();
        writeln!(buf, "|---|---|---|").ok();
        for t in report.regressions.iter().take(TOP_DELTA_LIST) {
            let base = t.baseline_outcome.as_deref().unwrap_or("—");
            let cand = t.candidate_outcome.as_deref().unwrap_or("—");
            writeln!(buf, "| {} | {base} | {cand} |", t.instance_id).ok();
        }
    }
    writeln!(buf).ok();

    // Top improvements (fail->pass transitions).
    writeln!(buf, "### Top Improvements").ok();
    writeln!(buf).ok();
    let improvements: Vec<_> = report
        .transitions
        .iter()
        .filter(|(k, _)| matches!(k, compare::TransitionKind::FailPass))
        .collect();
    let improvement_count = improvements.iter().map(|(_, v)| **v).sum::<usize>();
    if improvement_count == 0 {
        writeln!(buf, "_None._").ok();
    } else {
        writeln!(
            buf,
            "{improvement_count} instance(s) transitioned from fail to pass."
        )
        .ok();
    }
    writeln!(buf).ok();
}

// ── HTML rendering ─────────────────────────────────────────────────────────

fn md_to_html(md: &str) -> String {
    let mut buf = String::new();
    writeln!(buf, "<!DOCTYPE html>").ok();
    writeln!(buf, "<html lang=\"en\">").ok();
    writeln!(buf, "<head>").ok();
    writeln!(buf, "<meta charset=\"utf-8\">").ok();
    writeln!(
        buf,
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
    )
    .ok();
    writeln!(buf, "<title>Sweep Report</title>").ok();
    writeln!(buf, "<style>").ok();
    writeln!(
        buf,
        "body{{font-family:system-ui,sans-serif;max-width:1100px;margin:40px auto;padding:0 20px;line-height:1.5;color:#222}}"
    )
    .ok();
    writeln!(
        buf,
        "table{{border-collapse:collapse;width:100%;margin:1em 0}}th,td{{border:1px solid #ccc;padding:6px 12px;text-align:left}}th{{background:#f4f4f4}}"
    )
    .ok();
    writeln!(
        buf,
        "blockquote{{border-left:4px solid #aaa;margin:0;padding:0 1em;color:#555}}"
    )
    .ok();
    writeln!(
        buf,
        "code,pre{{background:#f6f8fa;border-radius:4px;padding:2px 6px}}"
    )
    .ok();
    writeln!(buf, "em{{font-style:italic;color:#666}}").ok();
    writeln!(buf, "h1,h2,h3{{margin-top:1.6em}}").ok();
    writeln!(buf, "</style>").ok();
    writeln!(buf, "</head>").ok();
    writeln!(buf, "<body>").ok();

    let mut in_table = false;
    let mut in_blockquote = false;
    for line in md.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            close_table(&mut buf, &mut in_table);
            close_blockquote(&mut buf, &mut in_blockquote);
            writeln!(buf, "<h1>{}</h1>", html_escape(rest)).ok();
        } else if let Some(rest) = line.strip_prefix("## ") {
            close_table(&mut buf, &mut in_table);
            close_blockquote(&mut buf, &mut in_blockquote);
            writeln!(buf, "<h2>{}</h2>", html_escape(rest)).ok();
        } else if let Some(rest) = line.strip_prefix("### ") {
            close_table(&mut buf, &mut in_table);
            writeln!(buf, "<h3>{}</h3>", html_escape(rest)).ok();
        } else if let Some(rest) = line.strip_prefix("> ") {
            if !in_blockquote {
                writeln!(buf, "<blockquote>").ok();
                in_blockquote = true;
            }
            writeln!(buf, "<p>{}</p>", html_escape(rest)).ok();
        } else if line.starts_with('|') {
            let cells = split_md_pipes(line);
            // Skip separator rows like |---|---|.
            if cells
                .iter()
                .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-'))
            {
                continue;
            }
            if in_table {
                writeln!(buf, "<tr>").ok();
                for cell in &cells {
                    write!(buf, "<td>{}</td>", render_md_inline(cell)).ok();
                }
                writeln!(buf, "</tr>").ok();
            } else {
                writeln!(buf, "<table>").ok();
                writeln!(buf, "<thead><tr>").ok();
                for cell in &cells {
                    write!(buf, "<th>{}</th>", html_escape(cell)).ok();
                }
                writeln!(buf, "</tr></thead>").ok();
                writeln!(buf, "<tbody>").ok();
                in_table = true;
            }
        } else {
            close_blockquote(&mut buf, &mut in_blockquote);
            close_table(&mut buf, &mut in_table);
            if line.is_empty() {
                writeln!(buf, "<br>").ok();
            } else {
                writeln!(buf, "<p>{}</p>", render_md_inline(line)).ok();
            }
        }
    }
    close_table(&mut buf, &mut in_table);
    close_blockquote(&mut buf, &mut in_blockquote);
    writeln!(buf, "</body>").ok();
    writeln!(buf, "</html>").ok();
    buf
}

fn close_table(buf: &mut String, in_table: &mut bool) {
    if *in_table {
        writeln!(buf, "</tbody></table>").ok();
        *in_table = false;
    }
}

fn close_blockquote(buf: &mut String, in_blockquote: &mut bool) {
    if *in_blockquote {
        writeln!(buf, "</blockquote>").ok();
        *in_blockquote = false;
    }
}

/// Split a markdown table row on `|`, respecting `\|` escapes so cells that
/// contain escaped pipes are preserved as a single cell. Drops the empty
/// leading/trailing cells that table rows produce.
fn split_md_pipes(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                if next == '|' {
                    current.push('|');
                } else {
                    current.push('\\');
                    current.push(next);
                }
            } else {
                current.push('\\');
            }
        } else if c == '|' {
            cells.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(c);
        }
    }
    cells.push(current.trim().to_owned());
    if cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render inline markdown emphasis for cell content. We deliberately do NOT
/// treat `_` as italic markers — instance ids like `django__django-001`
/// contain unbalanced underscores and would otherwise get mangled. Backticks
/// for code spans are preserved.
fn render_md_inline(s: &str) -> String {
    let escaped = html_escape(s);
    render_code_spans(&escaped)
}

fn render_code_spans(s: &str) -> String {
    let mut result = String::new();
    let mut open = false;
    for c in s.chars() {
        if c == '`' {
            if open {
                result.push_str("</code>");
            } else {
                result.push_str("<code>");
            }
            open = !open;
        } else {
            result.push(c);
        }
    }
    if open {
        result.push_str("</code>");
    }
    result
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn pct(n: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        n as f64 / total as f64 * 100.0
    }
}

fn cost_pct(cost: f64, total: f64) -> f64 {
    if total == 0.0 {
        0.0
    } else {
        cost / total * 100.0
    }
}

/// Sweep-level cost for an instance. Prefers the recorded `cost_usd` when
/// non-zero, otherwise falls back to a token-based estimate via
/// `InstanceResult::effective_cost_usd`. Sweeps that only populate token
/// counts (e.g. backends that don't return billing in the response) would
/// otherwise show `$0.0000` across every cost cell.
fn cost_for(inst: &InstanceResult, model: Option<&str>) -> f64 {
    inst.effective_cost_usd(model).unwrap_or(0.0)
}

/// Did this instance resolve? Prefers the evaluator verdict (a submitted patch
/// can be evaluated as unresolved); falls back to the sweep-row helpers from
/// `swebench` for non-evaluated rows. The sweep helpers handle legacy artifacts
/// where `runs == 0` and `resolved_count == 0` but the row is genuinely
/// submitted — direct field reads on `resolved_count` would mis-classify those
/// successful legacy runs as failures.
fn is_resolved(inst: &InstanceResult, eval: Option<&EvaluationResults>) -> bool {
    if let Some(ie) = eval_for(inst, eval) {
        ie.resolved_count > 0 || ie.resolved
    } else {
        sweep_resolved_count(inst) > 0
    }
}

/// Did the first run of this instance resolve? Same evaluator-first preference;
/// legacy-aware fallback for non-evaluated rows.
fn is_pass_at_1(inst: &InstanceResult, eval: Option<&EvaluationResults>) -> bool {
    if let Some(ie) = eval_for(inst, eval) {
        ie.pass_at_1
    } else {
        sweep_pass_at_1(inst)
    }
}

/// `(resolved_count, total_runs)` for display, preferring evaluator counts and
/// using the legacy-aware sweep helpers otherwise.
fn resolved_count_and_runs(inst: &InstanceResult, eval: Option<&EvaluationResults>) -> (u32, u32) {
    if let Some(ie) = eval_for(inst, eval) {
        let runs = if ie.runs > 0 {
            ie.runs
        } else {
            effective_runs(inst)
        };
        (ie.resolved_count, runs)
    } else {
        (sweep_resolved_count(inst), effective_runs(inst))
    }
}

fn eval_for<'a>(
    inst: &InstanceResult,
    eval: Option<&'a EvaluationResults>,
) -> Option<&'a crate::run::evaluate::InstanceEvaluation> {
    eval?
        .instances
        .iter()
        .find(|ie| ie.instance_id == inst.instance_id)
}

fn instance_category(inst: &InstanceResult, eval: Option<&EvaluationResults>) -> String {
    if let Some(e) = eval {
        if let Some(ie) = e
            .instances
            .iter()
            .find(|ie| ie.instance_id == inst.instance_id)
        {
            return format!("{:?}", ie.eval_exit_reason);
        }
    }
    inst.failure_category
        .map_or_else(|| "unknown".into(), |c| format!("{c:?}"))
}

fn trajectory_excerpt(sweep_dir: &Path, instance_id: &str) -> String {
    let nested = sweep_dir.join(instance_id).join("run-1.traj.json");
    let legacy = sweep_dir.join(format!("{instance_id}.traj.json"));
    let path = if nested.exists() {
        nested
    } else if legacy.exists() {
        legacy
    } else {
        return "_no trajectory_".into();
    };

    let Ok(text) = std::fs::read_to_string(&path) else {
        return "_no trajectory_".into();
    };
    let Ok(traj) = serde_json::from_str::<Trajectory>(&text) else {
        return "_no trajectory_".into();
    };

    let Some(msg) = traj.messages.iter().rev().find(|m| m.role == "assistant") else {
        return "_no assistant message_".into();
    };

    let text = msg.content.trim();
    let redacted = apply_redaction(text);
    truncate_to_120(&redacted)
}

fn apply_redaction(text: &str) -> String {
    use crate::redaction::{Redactor, surface};
    let cfg = crate::config::RedactionCfg::default();
    let redactor = Redactor::from_config_lossy(&cfg);
    redactor.redact_text(text, surface::EXPORT).text
}

fn truncate_to_120(s: &str) -> String {
    let s = s.replace('\n', " ").replace('|', "\\|");
    match s.char_indices().nth(EXCERPT_LEN) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]
    use super::*;

    #[test]
    fn pct_zero_total_returns_zero() {
        assert!((pct(5, 0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cost_pct_zero_total_returns_zero() {
        assert!((cost_pct(1.5, 0.0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn truncate_to_120_short_string_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_to_120(s), s);
    }

    #[test]
    fn truncate_to_120_long_string_truncated() {
        let s = "a".repeat(200);
        let truncated = truncate_to_120(&s);
        // EXCERPT_LEN chars + ellipsis = EXCERPT_LEN + 1
        assert_eq!(truncated.chars().count(), EXCERPT_LEN + 1);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn truncate_to_120_pipes_escaped() {
        let s = "foo | bar";
        assert!(truncate_to_120(s).contains("\\|"));
    }

    #[test]
    fn split_md_pipes_handles_escaped_pipe() {
        let line = "| a | b \\| c | d |";
        let cells = split_md_pipes(line);
        assert_eq!(cells, vec!["a", "b | c", "d"]);
    }

    #[test]
    fn split_md_pipes_drops_leading_trailing_empties() {
        let cells = split_md_pipes("| x | y |");
        assert_eq!(cells, vec!["x", "y"]);
    }

    #[test]
    fn render_md_inline_does_not_italicize_underscores() {
        // Instance ids with double underscores must not be mangled.
        let out = render_md_inline("django__django-001");
        assert_eq!(out, "django__django-001");
    }

    #[test]
    fn render_md_inline_renders_code_spans() {
        let out = render_md_inline("run `bench evaluate`");
        assert_eq!(out, "run <code>bench evaluate</code>");
    }

    #[test]
    fn render_md_inline_escapes_html() {
        let out = render_md_inline("<script>alert(1)</script>");
        assert!(out.contains("&lt;script&gt;"));
    }
}
