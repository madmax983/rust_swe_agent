import re

with open("src/run/inspect.rs", "r") as f:
    content = f.read()

start_idx = content.find("fn render_instance_text(report: &InspectReport) -> String {")
end_idx = content.find("    for step in &report.steps {", start_idx)

replacement = """fn render_instance_text(report: &InspectReport) -> String {
    let mut s = String::new();
    let color = std::io::stdout().is_terminal();
    s.push_str("\\n=== bench inspect ===\\n");

    let mut table = Table::new();
    table.load_preset(UTF8_FULL).apply_modifier(UTF8_ROUND_CORNERS);

    table.add_row(vec!["instance_id", report.instance_id.as_deref().unwrap_or("?")]);
    table.add_row(vec!["model", report.model.as_deref().unwrap_or("?")]);
    table.add_row(vec!["outcome", report.outcome.as_deref().unwrap_or("?")]);
    table.add_row(vec!["failure_category", report.failure_category.map_or("none", failure_label)]);
    table.add_row(vec!["total_cost_usd", &report.total_cost_usd.map_or_else(|| "?".into(), |v| format!("{v:.6}"))]);

    if report.actual_cost_usd.is_some() || report.actual_cost_source.is_some() {
        let source = report
            .actual_cost_source
            .map_or_else(|| "unknown".to_owned(), |source| source.to_string());
        table.add_row(vec![
            "actual_cost_usd",
            &format!("{} ({source})", report.actual_cost_usd.map_or_else(|| "?".into(), |v| format!("{v:.6}")))
        ]);
    }
    if report.baseline_cost_usd.is_some() || report.baseline_cost_model.is_some() {
        let model = report
            .baseline_cost_model
            .as_deref()
            .unwrap_or("claude-3-5-sonnet");
        table.add_row(vec![
            "baseline_cost_usd",
            &format!("{} ({model})", report.baseline_cost_usd.map_or_else(|| "?".into(), |v| format!("{v:.6}")))
        ]);
    }
    table.add_row(vec![
        "tokens",
        &render_token_summary(
            report.prompt_tokens,
            report.input_tokens,
            report.cache_read_tokens,
            report.cache_creation_tokens,
            report.completion_tokens,
        )
    ]);

    if let Some(r) = report.resolved {
        table.add_row(vec!["resolved", &r.to_string()]);
    }

    let mut patch_stats = String::new();
    write_patch_stats_lines(&mut patch_stats, report.patch_stats.as_ref());
    if !patch_stats.is_empty() {
        table.add_row(vec!["patch_stats", patch_stats.trim()]);
    }

    let submitted_without_tests = report.outcome.as_deref()
        == Some(crate::trajectory::outcome::SUBMITTED)
        && !report.tests_run_before_submit;
    table.add_row(vec![
        "tests",
        &format!(
            "count={} last_exit_code={} last_passed={} submitted_without_tests={submitted_without_tests}",
            report.test_invocations_count,
            report.last_test_exit_code.map_or_else(|| "?".into(), |code| code.to_string()),
            report.last_tests_passed.map_or_else(|| "?".into(), |passed| passed.to_string()),
        )
    ]);

    if let Some(fb) = &report.fallback_summary {
        table.add_row(vec![
            "fallback",
            &format!("happened={} count={} primary={} final={}",
            fb.fallback_happened, fb.fallback_count, fb.primary_model, fb.final_model,
            )
        ]);
        if !fb.attempted_models.is_empty() {
            table.add_row(vec!["fallback_chain", &fb.attempted_models.join(" → ")]);
        }
    }

    if report.verification_status.is_some() || !report.verification_results.is_empty() {
        table.add_row(vec![
            "verification",
            &format!("status={} checks={}",
            report.verification_status.as_deref().unwrap_or("unverified"),
            report.verification_results.len())
        ]);
        let mut verification_details = String::new();
        for r in &report.verification_results {
            let timeout_note = if r.timed_out { " (timed_out)" } else { "" };
            let _ = writeln!(
                verification_details,
                "  [{}] passed={} exit_code={} duration={}ms{}",
                r.name, r.passed, r.exit_code, r.duration_ms, timeout_note,
            );
        }
        if !verification_details.is_empty() {
            table.add_row(vec!["verification details", verification_details.trim()]);
        }
    }
    for w in &report.warnings {
        table.add_row(vec!["warning", w]);
    }

    s.push_str(&table.to_string());
    s.push('\\n');

"""

new_content = content[:start_idx] + replacement + content[end_idx:]

with open("src/run/inspect.rs", "w") as f:
    f.write(new_content)
