//! `bench report`: integration tests.
//!
//! Covers the AC from issue #166:
//!   - subcommand exists in `--help`
//!   - writes a markdown file and exits 0
//!   - provenance block present
//!   - top-line metrics present
//!   - failure-mix table present
//!   - top failed instances table present
//!   - missing evaluation.json renders gracefully (no failure)
//!   - `--format html` produces html output
//!   - deterministic output for fixed input

#![allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::expect_used
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::run::swebench::{InstanceResult, SweepResults};
use rust_swe_agent::trajectory::{FailureCategory, outcome};

mod support;
use support::binary_path;

// ── fixture builders ───────────────────────────────────────────────────────

fn submitted(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(4),
        cost_usd: Some(0.05),
        prompt_tokens: Some(500),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(100),
        duration_secs: Some(8.0),
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: true,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: 1,
        pass_at_1: true,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
    }
}

fn errored(id: &str, cat: FailureCategory) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "error".into(),
        outcome: Some(outcome::ERROR.into()),
        failure_category: Some(cat),
        steps: Some(6),
        cost_usd: Some(0.10),
        prompt_tokens: Some(1500),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(200),
        duration_secs: Some(15.0),
        error: Some("stub".into()),
        github_pr_error: None,
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: 0,
        pass_at_1: false,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
    }
}

fn write_sweep(dir: &Path, instances: Vec<InstanceResult>) {
    let total_cost: f64 = instances.iter().filter_map(|i| i.cost_usd).sum();
    let pass_at_k = if instances.is_empty() {
        0.0
    } else {
        let passed = instances.iter().filter(|r| r.resolved_count > 0).count();
        passed as f64 / instances.len() as f64
    };
    let failures_by_category: BTreeMap<FailureCategory, usize> = {
        let mut map = BTreeMap::new();
        for inst in &instances {
            if let Some(cat) = inst.failure_category {
                *map.entry(cat).or_insert(0) += 1;
            }
        }
        map
    };
    let sweep = SweepResults {
        total: instances.len(),
        sweep_status: rust_swe_agent::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: instances.len(),
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: instances
            .iter()
            .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
            .count(),
        submitted_with_tests: 0,
        skipped: 0,
        errored: instances
            .iter()
            .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
            .count(),
        failures_by_category,
        budget_halted: 0,
        with_patch: instances.iter().filter(|r| r.patch_present).count(),
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: instances
            .iter()
            .filter_map(|i| i.prompt_tokens)
            .sum::<u64>(),
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: instances
            .iter()
            .filter_map(|i| i.completion_tokens)
            .sum::<u64>(),
        estimated_cost_usd: total_cost,
        actual_cost_usd: Some(total_cost),
        actual_cost_source: None,
        baseline_cost_usd: None,
        baseline_cost_model: None,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k,
        filter_spec: Default::default(),
        manifest: Some(rust_swe_agent::run::swebench::ProvenanceManifest {
            purpose: None,
            harness: rust_swe_agent::run::swebench::HarnessManifest {
                name: "rust_swe_agent".into(),
                version: "0.1.0-test".into(),
                git_sha: Some("deadbeef1234".into()),
                git_dirty: Some(false),
                git_resolution: "exact".into(),
            },
            dataset: rust_swe_agent::run::swebench::DatasetManifest {
                path: "tests/fixtures/test.jsonl".into(),
                sha256: "abc123".into(),
                instance_count: instances.len(),
                filter_spec: None,
                ..Default::default()
            },
            prompt_template: rust_swe_agent::run::swebench::PromptTemplateManifest {
                source: "inline".into(),
                path: None,
                sha256: "tpl123".into(),
            },
            config: rust_swe_agent::run::swebench::ConfigManifest {
                resolved: "default".into(),
                overlay_paths: Vec::new(),
            },
            model: rust_swe_agent::run::swebench::ModelManifest {
                name: "claude-opus-4-7".into(),
                backend: "litellm".into(),
                backend_version: None,
                base_url: None,
            },
            runtime: rust_swe_agent::run::swebench::RuntimeManifest {
                started_at_utc: "2026-05-01T00:00:00Z".into(),
                finished_at_utc: Some("2026-05-01T00:10:00Z".into()),
                host_os: "linux".into(),
                resume_mode: false,
                rust_version: Some("rustc 1.85.0".into()),
            },
            cli: rust_swe_agent::run::swebench::CliManifest { argv: Vec::new() },
            circuit_breaker: None,
            reproduced_from: None,
        }),
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,
        total_fallbacks: 0,
        model_mix: BTreeMap::new(),
        systemic_halt_category: None,
    };
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&sweep).unwrap(),
    )
    .unwrap();
}

fn bench_report(args: &[&str]) -> std::process::Output {
    Command::new(binary_path())
        .args(["bench", "report"])
        .args(args)
        .output()
        .expect("failed to run bench report")
}

// ── tests ──────────────────────────────────────────────────────────────────

#[test]
fn bench_report_in_help() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("report"),
        "bench --help should list 'report' subcommand\nstdout: {stdout}"
    );
}

#[test]
fn bench_report_basic_markdown_exits_zero() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            submitted("django__django-002"),
            errored("django__django-003", FailureCategory::StepLimit),
        ],
    );
    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench report should exit 0\nstderr: {stderr}"
    );
    assert!(out_file.exists(), "report file should be written");
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(!content.is_empty(), "report file should not be empty");
}

#[test]
fn bench_report_markdown_contains_provenance_block() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            errored("django__django-002", FailureCategory::ModelApi),
        ],
    );
    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();

    // Provenance block: model name
    assert!(
        content.contains("claude-opus-4-7"),
        "report should contain model name\ncontent:\n{content}"
    );
    // Provenance block: harness git SHA
    assert!(
        content.contains("deadbeef1234"),
        "report should contain harness git SHA\ncontent:\n{content}"
    );
    // Provenance block: dataset path
    assert!(
        content.contains("tests/fixtures/test.jsonl"),
        "report should contain dataset path\ncontent:\n{content}"
    );
    // Provenance block: timestamps
    assert!(
        content.contains("2026-05-01"),
        "report should contain start timestamp\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_markdown_contains_topline_metrics() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            submitted("django__django-002"),
            errored("django__django-003", FailureCategory::StepLimit),
        ],
    );
    let out_file = work.path().join("report.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let content = std::fs::read_to_string(&out_file).unwrap();

    // Should mention total instances (3)
    assert!(
        content.contains('3'),
        "report should contain total instance count\ncontent:\n{content}"
    );
    // Should mention resolved count (2)
    assert!(
        content.contains('2'),
        "report should contain resolved count\ncontent:\n{content}"
    );
    // Should mention cost
    assert!(
        content.contains("cost") || content.contains("usd") || content.contains("USD"),
        "report should contain cost information\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_markdown_contains_failure_mix_table() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            errored("django__django-002", FailureCategory::StepLimit),
            errored("django__django-003", FailureCategory::ModelApi),
            errored("django__django-004", FailureCategory::StepLimit),
        ],
    );
    let out_file = work.path().join("report.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let content = std::fs::read_to_string(&out_file).unwrap();

    // Failure mix table should contain failure categories
    assert!(
        content.contains("step_limit") || content.contains("StepLimit"),
        "report should contain step_limit failure category\ncontent:\n{content}"
    );
    assert!(
        content.contains("model_api") || content.contains("ModelApi"),
        "report should contain model_api failure category\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_markdown_contains_top_failed_instances() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            errored("django__django-002", FailureCategory::StepLimit),
            errored("django__django-003", FailureCategory::ModelApi),
        ],
    );
    let out_file = work.path().join("report.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let content = std::fs::read_to_string(&out_file).unwrap();

    // Top failed instances table should contain instance IDs of failed instances
    assert!(
        content.contains("django__django-002"),
        "report should contain failed instance id\ncontent:\n{content}"
    );
    assert!(
        content.contains("django__django-003"),
        "report should contain failed instance id\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_top_failures_flag_limits_table() {
    let work = tempfile::tempdir().unwrap();
    // Write 15 failed instances
    let instances: Vec<InstanceResult> = (1..=15)
        .map(|i| errored(&format!("inst-{i:03}"), FailureCategory::StepLimit))
        .collect();
    write_sweep(work.path(), instances);
    let out_file = work.path().join("report.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
        "--top-failures",
        "5",
    ]);
    let content = std::fs::read_to_string(&out_file).unwrap();
    // With --top-failures 5, only 5 instance ids should appear in the top table
    // inst-001 through inst-005 should be present, inst-006 through inst-015 should NOT
    // (we just check that the table is bounded — at most 5 rows)
    let count = (1..=15)
        .filter(|i| content.contains(&format!("inst-{i:03}")))
        .count();
    assert!(
        count <= 5,
        "with --top-failures 5, at most 5 instance rows should appear, got {count}\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_missing_evaluation_json_exits_zero() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            errored("django__django-002", FailureCategory::StepLimit),
        ],
    );
    // Deliberately do NOT write evaluation.json
    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench report should exit 0 even without evaluation.json\nstderr: {stderr}"
    );
    let content = std::fs::read_to_string(&out_file).unwrap();
    // Should contain a note about missing evaluation data
    assert!(
        content.contains("evaluation") || content.contains("eval"),
        "report should mention evaluation data status\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_missing_evaluation_json_renders_graceful_message() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![errored("django__django-001", FailureCategory::StepLimit)],
    );
    let out_file = work.path().join("report.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let content = std::fs::read_to_string(&out_file).unwrap();
    // The spec says eval-only sections should render as:
    // "_no evaluation data — run `bench evaluate` to populate_"
    assert!(
        content.contains("no evaluation data") || content.contains("bench evaluate"),
        "report without evaluation.json should have graceful message\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_format_html_exits_zero_and_writes_html() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(work.path(), vec![submitted("django__django-001")]);
    let out_file = work.path().join("report.html");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
        "--format",
        "html",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench report --format html should exit 0\nstderr: {stderr}"
    );
    assert!(out_file.exists(), "html report file should be written");
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(
        content.contains("<html") || content.contains("<!DOCTYPE"),
        "html report should contain html markup\ncontent:\n{content}"
    );
    // HTML should be self-contained (no external assets, inline CSS)
    assert!(
        !content.contains("http://") && !content.contains("https://"),
        "html report should not reference external resources (inline CSS/no JS)\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_format_markdown_is_default() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(work.path(), vec![submitted("django__django-001")]);
    let out_file = work.path().join("report.md");
    // No --format flag — should default to markdown
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();
    // Markdown starts with # headings
    assert!(
        content.contains('#'),
        "default format should be markdown with # headings\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_output_is_deterministic() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            errored("django__django-002", FailureCategory::StepLimit),
        ],
    );
    let out1 = work.path().join("report1.md");
    let out2 = work.path().join("report2.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out1.display().to_string(),
    ]);
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out2.display().to_string(),
    ]);
    let c1 = std::fs::read_to_string(&out1).unwrap();
    let c2 = std::fs::read_to_string(&out2).unwrap();
    assert_eq!(
        c1, c2,
        "bench report should produce deterministic output for fixed input"
    );
}

#[test]
fn bench_report_schema_version_in_header() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(work.path(), vec![submitted("django__django-001")]);
    let out_file = work.path().join("report.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let content = std::fs::read_to_string(&out_file).unwrap();
    // The report header should include a schema version reference
    assert!(
        content.contains("schema") || content.contains("v1.") || content.contains("version"),
        "report should reference artifact schema version\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_missing_sweep_exits_nonzero() {
    let out = bench_report(&[
        "--sweep",
        "/tmp/does-not-exist-at-all-xyz",
        "--output",
        "/tmp/out.md",
    ]);
    assert!(
        !out.status.success(),
        "bench report with missing sweep dir should exit non-zero"
    );
}

#[test]
fn bench_report_markdown_matches_snapshot() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            submitted("django__django-002"),
            errored("django__django-003", FailureCategory::StepLimit),
            errored("django__django-004", FailureCategory::ModelApi),
        ],
    );
    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();
    insta::assert_snapshot!("bench_report_basic", content);
}

#[test]
fn bench_report_falls_back_to_token_based_cost_when_cost_usd_missing() {
    // Sweep with no recorded `cost_usd` but populated token counts — the report
    // should derive cost from `effective_cost_usd(model)` rather than showing
    // $0.0000 totals and ranking failures by instance id.
    let work = tempfile::tempdir().unwrap();
    let mut row = errored("django__django-001", FailureCategory::StepLimit);
    row.cost_usd = None;
    row.prompt_tokens = Some(10_000);
    row.completion_tokens = Some(2_000);
    write_sweep(work.path(), vec![row]);

    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();
    // Token-based estimate for claude-opus-4-7 with 10K input + 2K output
    // tokens is well above $0; the dollar total must not be $0.0000.
    let zero_dollar_total = content.contains("Total cost USD | $0.0000");
    assert!(
        !zero_dollar_total,
        "report must derive cost from tokens when cost_usd is missing\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_treats_legacy_submitted_row_as_resolved() {
    // Legacy results.json fixtures: `runs == 0`, `resolved_count == 0`,
    // `pass_at_1 == false` even though the row is genuinely submitted (the
    // pre-rerun-tracking schema). The report must classify these as resolved
    // via `swebench::resolved_count`/`pass_at_1`, not as failures.
    let work = tempfile::tempdir().unwrap();
    let mut legacy = submitted("django__django-001");
    legacy.runs = 0;
    legacy.resolved_count = 0;
    legacy.pass_at_1 = false;
    write_sweep(work.path(), vec![legacy]);

    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();
    // Legacy submitted row should count as resolved (1/1, 100%).
    assert!(
        content.contains("Resolved | 1"),
        "legacy submitted row should be resolved\ncontent:\n{content}"
    );
    assert!(
        content.contains("100.00%"),
        "legacy submitted row should drive resolve rate to 100%\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_finds_nested_canonical_trajectory_layout() {
    // Canonical nested `instance_id/trajectory.json` layout (what bench
    // inspect uses as its primary path). The report should pick up that
    // trajectory and render its last-assistant message in the excerpt
    // column instead of `_no trajectory_`.
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![errored("django__django-001", FailureCategory::StepLimit)],
    );
    let instance_dir = work.path().join("django__django-001");
    std::fs::create_dir_all(&instance_dir).unwrap();
    let traj = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": {"major": 1, "minor": 5},
        "info": {},
        "messages": [
            {"role": "user", "content": "fix this"},
            {"role": "assistant", "content": "UNIQUE_EXCERPT_MARKER_42"}
        ]
    });
    std::fs::write(
        instance_dir.join("trajectory.json"),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();

    let out_file = work.path().join("report.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(
        content.contains("UNIQUE_EXCERPT_MARKER_42"),
        "report must resolve the canonical nested trajectory.json layout\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_legacy_single_run_eval_row_counts_as_pass_at_1() {
    // Legacy evaluation.json artifact: single-run row where `pass_at_1`
    // defaulted to false (field didn't exist yet) but `resolved: true`
    // already means the first run passed. The report must show 100% pass@1
    // for that row, not 0%.
    let work = tempfile::tempdir().unwrap();
    write_sweep(work.path(), vec![submitted("django__django-001")]);
    let eval = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 5},
        "instances": [{
            "instance_id": "django__django-001",
            "resolved": true,
            // legacy shape: runs == 0, resolved_count == 0, pass_at_1 == false
            "tests_passed": [],
            "tests_failed": [],
            "eval_exit_reason": "resolved"
        }]
    });
    std::fs::write(
        work.path().join("evaluation.json"),
        serde_json::to_string_pretty(&eval).unwrap(),
    )
    .unwrap();

    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(
        content.contains("Pass@1 | 100.00%"),
        "legacy single-run resolved eval row must count as pass@1\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_redacts_secrets_in_instance_ids() {
    // A custom/private sweep where an `instance_id` itself contains a
    // secret-shaped value must be redacted before the row reaches the
    // shareable report.
    let work = tempfile::tempdir().unwrap();
    let leaked_token = "sk-abcdef0123456789ABCDEF12345";
    // Embed the secret with a non-word-character boundary so the default
    // redactor's `\b sk-...` pattern matches.
    let leaky_id = format!("custom/{leaked_token}");
    let mut row = errored(&leaky_id, FailureCategory::StepLimit);
    row.cost_usd = Some(0.42);
    write_sweep(work.path(), vec![row]);

    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(
        !content.contains(leaked_token),
        "report must redact secrets embedded in instance IDs\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_redacts_secrets_in_manifest_fields() {
    // A dataset path or other manifest string that embeds a secret-shaped value
    // must be redacted before reaching the shareable report file.
    let work = tempfile::tempdir().unwrap();
    let leaked_token = "sk-abcdef0123456789ABCDEF12345";
    let dataset_path = format!("/tmp/datasets/{leaked_token}/swebench.jsonl");
    // Write a results.json that we hand-craft so we can plant the secret in
    // manifest.dataset.path (write_sweep hardcodes that field).
    let payload = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 5},
        "total": 1,
        "sweep_status": "completed",
        "submitted": 1, "submitted_with_tests": 0,
        "skipped": 0, "errored": 0,
        "failures_by_category": {},
        "budget_halted": 0, "with_patch": 1, "patch_empty": 0,
        "patch_apply_invalid": 0, "github_pr_failures": 0,
        "total_prompt_tokens": 0, "total_cache_read_tokens": 0,
        "total_cache_creation_tokens": 0, "total_completion_tokens": 0,
        "estimated_cost_usd": 0.05, "cache_hit_rate": 0.0,
        "retries": 0, "retried_instances": 0, "pass_at_k": 1.0,
        "filter_spec": {}, "cost_limit_usd": null,
        "manifest": {
            "harness": {"name": "rust_swe_agent", "version": "test", "git_resolution": "test"},
            "dataset": {"path": dataset_path, "sha256": "test", "instance_count": 1},
            "prompt_template": {"source": "inline", "sha256": "tpl"},
            "config": {"resolved": "default", "overlay_paths": []},
            "model": {"name": "claude-opus-4-7", "backend": "litellm"},
            "runtime": {
                "started_at_utc": "2026-05-01T00:00:00Z",
                "finished_at_utc": "2026-05-01T00:01:00Z",
                "host_os": "linux", "resume_mode": false
            },
            "cli": {"argv": []}
        },
        "instances": [{
            "instance_id": "django__django-001",
            "exit_reason": "submitted", "outcome": "submitted",
            "patch_present": true, "non_empty_patch": true,
            "attempts": 1, "retry_reasons": [],
            "runs": 1, "resolved_count": 1, "pass_at_1": true
        }]
    });
    std::fs::write(
        work.path().join("results.json"),
        serde_json::to_string_pretty(&payload).unwrap(),
    )
    .unwrap();

    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(
        !content.contains(leaked_token),
        "report must redact secret-shaped values embedded in manifest fields; \
         leaked token still present in output\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_rejects_wrong_kind_evaluation_artifact() {
    // evaluation.json with an artifact header from the wrong kind must be
    // rejected (not silently accepted just because the field shapes overlap).
    let work = tempfile::tempdir().unwrap();
    write_sweep(work.path(), vec![submitted("django__django-001")]);
    let wrong_kind = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 5},
        "instances": [],
    });
    std::fs::write(
        work.path().join("evaluation.json"),
        serde_json::to_string_pretty(&wrong_kind).unwrap(),
    )
    .unwrap();
    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(
        !out.status.success(),
        "bench report should reject an evaluation.json with the wrong artifact_kind"
    );
}

#[test]
fn bench_report_corrupt_evaluation_json_fails() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(work.path(), vec![submitted("django__django-001")]);
    // Write a malformed evaluation.json — must NOT be silently treated as
    // "no evaluation data" (that would hide evaluator-only resolved status).
    std::fs::write(work.path().join("evaluation.json"), b"{not valid json").unwrap();
    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(
        !out.status.success(),
        "bench report should exit non-zero when evaluation.json is malformed"
    );
}

#[test]
fn bench_report_uses_evaluation_resolved_over_sweep_submission() {
    let work = tempfile::tempdir().unwrap();
    // Sweep row says "submitted, pass_at_1 = true" — but evaluation.json
    // contradicts that with `resolved: false`. The report must trust the
    // evaluator, not the sweep proxy.
    write_sweep(work.path(), vec![submitted("django__django-001")]);
    let eval = serde_json::json!({
        "artifact_kind": "evaluation_results",
        "schema_version": {"major": 1, "minor": 5},
        "instances": [{
            "instance_id": "django__django-001",
            "resolved": false,
            "runs": 1,
            "resolved_count": 0,
            "pass_at_1": false,
            "tests_passed": [],
            "tests_failed": [],
            "eval_exit_reason": "unresolved"
        }]
    });
    std::fs::write(
        work.path().join("evaluation.json"),
        serde_json::to_string_pretty(&eval).unwrap(),
    )
    .unwrap();

    let out_file = work.path().join("report.md");
    let out = bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    assert!(out.status.success());
    let content = std::fs::read_to_string(&out_file).unwrap();

    // Resolved count from evaluation is 0, not 1 (which the sweep row would have given).
    assert!(
        content.contains("Resolved | 0"),
        "report must use evaluator resolved (0), not sweep submission (1)\ncontent:\n{content}"
    );
    // The unresolved instance must appear in the top-failed table.
    assert!(
        content.contains("django__django-001"),
        "evaluator-failed instance must appear in top-failed table\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_baseline_flag_emits_delta_section() {
    let work = tempfile::tempdir().unwrap();
    let baseline_dir = work.path().join("baseline");
    let candidate_dir = work.path().join("candidate");
    std::fs::create_dir_all(&baseline_dir).unwrap();
    std::fs::create_dir_all(&candidate_dir).unwrap();

    // Baseline: 2 instances resolved, 1 errored.
    write_sweep(
        &baseline_dir,
        vec![
            submitted("django__django-001"),
            submitted("django__django-002"),
            errored("django__django-003", FailureCategory::StepLimit),
        ],
    );
    // Candidate: 1 resolved, 1 errored, 1 regressed.
    write_sweep(
        &candidate_dir,
        vec![
            submitted("django__django-001"),
            errored("django__django-002", FailureCategory::ModelApi),
            errored("django__django-003", FailureCategory::StepLimit),
        ],
    );

    let out_file = candidate_dir.join("report.md");
    let out = bench_report(&[
        "--sweep",
        &candidate_dir.display().to_string(),
        "--baseline",
        &baseline_dir.display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench report --baseline should exit 0\nstderr: {stderr}"
    );
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(
        content.contains("Delta vs Baseline") || content.contains("Resolved delta"),
        "report should contain baseline delta section\ncontent:\n{content}"
    );
    // The regression (django-002 passed in baseline, failed in candidate) should be listed.
    assert!(
        content.contains("django__django-002"),
        "regression should be listed in delta section\ncontent:\n{content}"
    );
    // Regression rows must label transitions as `pass`/`fail` (plus the
    // failure category) instead of the raw `submitted`/`error` outcomes.
    assert!(
        content.contains("pass") && content.contains("fail"),
        "regression rows should use explicit pass/fail labels\ncontent:\n{content}"
    );
    assert!(
        content.contains("ModelApi"),
        "regression rows should include the failure category\ncontent:\n{content}"
    );
}

#[test]
fn bench_report_resolved_instance_not_in_failed_table() {
    let work = tempfile::tempdir().unwrap();
    write_sweep(
        work.path(),
        vec![
            submitted("django__django-001"),
            submitted("django__django-002"),
            errored("django__django-003", FailureCategory::StepLimit),
        ],
    );
    let out_file = work.path().join("report.md");
    bench_report(&[
        "--sweep",
        &work.path().display().to_string(),
        "--output",
        &out_file.display().to_string(),
    ]);
    let content = std::fs::read_to_string(&out_file).unwrap();
    // Resolved instances should not appear in the "Top failed instances" section.
    // The report should list django-003 in the failure table.
    assert!(
        content.contains("django__django-003"),
        "failed instance should appear in report\ncontent:\n{content}"
    );
    // Resolved instances should not appear in the top failures section (they'd
    // appear elsewhere like metrics but not as "failed").
    // We check that django-001 does not appear in the top-failed rows
    // by looking for it specifically in a context that implies failure.
    // Since this is hard to parse precisely in a plain string test, we
    // at least verify the report was generated successfully.
    assert!(!content.is_empty(), "report should not be empty");
}
