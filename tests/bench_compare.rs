//! `bench compare`: end-to-end + CLI-binary integration tests.
//!
//! Covers the AC from issue #16:
//!   * subcommand exists in `--help`
//!   * regression list + transition matrix are correct
//!   * `--max-regressions` flips the process exit code
//!   * `--format json` emits a structured document
//!   * baseline missing newer fields (legacy) does not panic

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::run::swebench::{InstanceResult, SweepResults};
use rust_swe_agent::trajectory::{FailureCategory, TokenUsage, Trajectory, outcome};

fn binary_path() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo when running integration tests.
    // Fallback covers `cargo test --bin rust-swe-agent` invocations that
    // don't set it (rare in practice but harmless).
    std::env::var("CARGO_BIN_EXE_rust-swe-agent").map_or_else(
        |_| {
            let mut p = std::env::current_exe().unwrap();
            p.pop(); // tests/deps
            p.pop(); // debug
            p.push("rust-swe-agent");
            p
        },
        std::path::PathBuf::from,
    )
}

fn submitted(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(4),
        cost_usd: Some(0.05),
        prompt_tokens: Some(500),
        completion_tokens: Some(100),
        duration_secs: Some(8.0),
        error: None,
        patch_present: true,
        non_empty_patch: true,
        attempts: 1,
        retry_reasons: Vec::new(),
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
        completion_tokens: Some(200),
        duration_secs: Some(15.0),
        error: Some("stub".into()),
        patch_present: false,
        non_empty_patch: false,
        attempts: 1,
        retry_reasons: Vec::new(),
    }
}

fn write_results(dir: &Path, instances: Vec<InstanceResult>) {
    let sweep = SweepResults {
        total: instances.len(),
        submitted: instances
            .iter()
            .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
            .count(),
        skipped: 0,
        errored: instances
            .iter()
            .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
            .count(),
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        estimated_cost_usd: 0.0,
        retries: 0,
        retried_instances: 0,
        filter_spec: rust_swe_agent::run::swebench::FilterSpec::default(),
        manifest: None,
        cost_limit_usd: None,
        instances,
    };
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&sweep).unwrap(),
    )
    .unwrap();
}

fn write_diff_traj(
    dir: &Path,
    instance_id: &str,
    failure_category: Option<FailureCategory>,
    assistant: &str,
    command: &str,
    stdout: &str,
) {
    let mut t = Trajectory::new();
    t.info
        .other
        .insert("instance_id".into(), serde_json::json!(instance_id));
    t.info.outcome = Some(if failure_category.is_some() {
        outcome::ERROR.into()
    } else {
        outcome::SUBMITTED.into()
    });
    t.info.failure_category = failure_category;
    t.info.total_cost_usd = Some(0.10);
    t.info.token_usage = Some(TokenUsage {
        prompt_tokens: 10,
        completion_tokens: 5,
    });
    t.info.steps = Some(1);

    let mut asst = rust_swe_agent::model::Message::assistant(assistant);
    asst.extra.actions = Some(vec![command.into()]);
    t.record_message(&asst);

    let mut obs = rust_swe_agent::model::Message::user("tool result");
    obs.extra.other.insert(
        "run_result".into(),
        serde_json::json!({
            "stdout": stdout,
            "stderr": "",
            "exit_code": 0,
            "timed_out": false
        }),
    );
    t.record_message(&obs);

    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();
}

#[test]
fn help_lists_compare_subcommand() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("compare"),
        "expected `compare` in `bench --help`, got:\n{stdout}"
    );

    let out = Command::new(binary_path())
        .args(["bench", "compare", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--baseline",
        "--candidate",
        "--format",
        "--max-regressions",
        "--inspect-diff",
        "--emit-diff-script",
    ] {
        assert!(
            stdout.contains(flag),
            "expected `{flag}` in compare --help, got:\n{stdout}"
        );
    }
}

#[test]
fn compare_inspect_diff_sugar_renders_one_instance_diff() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results(baseline_dir.path(), vec![submitted("a")]);
    write_results(
        candidate_dir.path(),
        vec![errored("a", FailureCategory::StepLimit)],
    );
    write_diff_traj(
        baseline_dir.path(),
        "a",
        None,
        "```bash\npytest -q\n```",
        "pytest -q",
        "1 failed\n",
    );
    write_diff_traj(
        candidate_dir.path(),
        "a",
        Some(FailureCategory::StepLimit),
        "```bash\npytest tests\n```",
        "pytest tests",
        "2 failed\n",
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--inspect-diff",
            "a",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("=== bench inspect diff ==="), "{stdout}");
    assert!(stdout.contains("instance_id: a"), "{stdout}");
    assert!(stdout.contains("[step 0 - diverge]"), "{stdout}");
    assert!(stdout.contains("pytest tests"), "{stdout}");
}

#[test]
fn compare_emit_diff_script_executes_generated_script_for_all_regressions() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    let script = tempfile::NamedTempFile::new().unwrap();
    let script_path = script.path().to_path_buf();
    write_results(
        baseline_dir.path(),
        vec![
            submitted("regressed-one"),
            submitted("regressed-two"),
            submitted("stable"),
        ],
    );
    write_results(
        candidate_dir.path(),
        vec![
            errored("regressed-one", FailureCategory::StepLimit),
            errored("regressed-two", FailureCategory::ModelParse),
            submitted("stable"),
        ],
    );
    write_diff_traj(
        baseline_dir.path(),
        "regressed-one",
        None,
        "```bash\npytest -q\n```",
        "pytest -q",
        "1 failed\n",
    );
    write_diff_traj(
        candidate_dir.path(),
        "regressed-one",
        Some(FailureCategory::StepLimit),
        "```bash\npytest tests\n```",
        "pytest tests",
        "2 failed\n",
    );
    write_diff_traj(
        baseline_dir.path(),
        "regressed-two",
        None,
        "```bash\ncargo test\n```",
        "cargo test",
        "ok\n",
    );
    write_diff_traj(
        candidate_dir.path(),
        "regressed-two",
        Some(FailureCategory::ModelParse),
        "```bash\ncargo test --all\n```",
        "cargo test --all",
        "parse error\n",
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--emit-diff-script",
            script_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let script_text = std::fs::read_to_string(&script_path).unwrap();
    assert!(
        script_text.contains("bench inspect --diff"),
        "{script_text}"
    );
    assert!(
        script_text.contains("regressed-one.traj.json"),
        "{script_text}"
    );
    assert!(
        script_text.contains("regressed-two.traj.json"),
        "{script_text}"
    );
    assert!(!script_text.contains("stable.traj.json"), "{script_text}");

    let out = Command::new("sh").arg(&script_path).output().unwrap();
    assert!(
        out.status.success(),
        "generated script should exit zero; stdout:\n{}\nstderr:\n{}\nscript:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        script_text
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.matches("=== bench inspect diff ===").count(), 2);
    assert!(stdout.contains("instance_id: regressed-one"), "{stdout}");
    assert!(stdout.contains("instance_id: regressed-two"), "{stdout}");
    assert!(
        !stdout.contains("instance_id: stable"),
        "script should only inspect regressions:\n{stdout}"
    );
}

#[test]
fn cli_text_output_lists_regressions() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    write_results(
        baseline_dir.path(),
        vec![
            submitted("a"),
            submitted("b"),
            errored("c", FailureCategory::ModelApi),
        ],
    );
    write_results(
        candidate_dir.path(),
        vec![
            submitted("a"),
            errored("b", FailureCategory::StepLimit),
            submitted("c"),
        ],
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("=== bench compare ==="), "got: {stdout}");
    assert!(
        stdout.contains("Resolved:           2 -> 2 (+0)"),
        "got: {stdout}"
    );
    assert!(stdout.contains("pass->fail"), "got: {stdout}");
    assert!(stdout.contains("Regressions (1):"), "got: {stdout}");
    assert!(stdout.contains("- b"), "got: {stdout}");
    assert!(stdout.contains("category=step_limit"), "got: {stdout}");
}

#[test]
fn cli_json_output_is_machine_readable() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    write_results(baseline_dir.path(), vec![submitted("a"), submitted("b")]);
    write_results(
        candidate_dir.path(),
        vec![submitted("a"), errored("b", FailureCategory::AgentInternal)],
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("expected valid JSON; err={e}; got:\n{stdout}");
    });
    assert_eq!(v["regressions"].as_array().unwrap().len(), 1);
    assert_eq!(v["regressions"][0]["instance_id"], "b");
    assert_eq!(v["regressions"][0]["kind"], "pass_fail");
    assert_eq!(v["resolved_delta"], -1);
    assert_eq!(v["transitions"]["pass_fail"], 1);
}

#[test]
fn cli_max_regressions_flips_exit_code() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results(baseline_dir.path(), vec![submitted("a"), submitted("b")]);
    write_results(
        candidate_dir.path(),
        vec![
            errored("a", FailureCategory::StepLimit),
            errored("b", FailureCategory::StepLimit),
        ],
    );

    // Threshold 0 -> any regression fails the gate (2 regressions > 0).
    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--max-regressions",
            "0",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected non-zero exit when regressions > max; stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Threshold 5 -> 2 regressions <= 5; informational only, exit 0.
    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--max-regressions",
            "5",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected zero exit under threshold; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Unset -> always exit 0 even with regressions.
    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected zero exit when --max-regressions unset; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_legacy_baseline_without_failure_category_does_not_panic() {
    // Per AC: tolerate trajectories missing newer fields. We synthesize
    // a baseline `results.json` that pre-dates #15 — no `failure_category`
    // anywhere, and missing `failures_by_category`/`budget_halted`/
    // `with_patch` keys at the top level.
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    let legacy = serde_json::json!({
        "total": 2,
        "submitted": 1,
        "skipped": 0,
        "errored": 1,
        "instances": [
            {
                "instance_id": "a",
                "exit_reason": "submitted",
                "outcome": "submitted"
            },
            {
                "instance_id": "b",
                "exit_reason": "error",
                "outcome": "error"
            }
        ]
    });
    std::fs::write(
        baseline_dir.path().join("results.json"),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .unwrap();
    write_results(
        candidate_dir.path(),
        vec![errored("a", FailureCategory::StepLimit), submitted("b")],
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    // `a` was submitted in baseline, errored in candidate -> regression.
    // `b` was errored in baseline, submitted in candidate -> fail->pass.
    assert_eq!(v["regressions"].as_array().unwrap().len(), 1);
    assert_eq!(v["regressions"][0]["instance_id"], "a");
    assert_eq!(v["transitions"]["fail_pass"], 1);
}

#[test]
fn cli_disjoint_id_sets_bucketed_not_dropped() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results(
        baseline_dir.path(),
        vec![submitted("only_in_baseline"), submitted("shared")],
    );
    write_results(
        candidate_dir.path(),
        vec![submitted("only_in_candidate"), submitted("shared")],
    );
    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["transitions"]["pass_pass"], 1);
    assert_eq!(v["transitions"]["missing_present"], 1);
    assert_eq!(v["transitions"]["present_missing"], 1);
    assert_eq!(v["regressions"].as_array().unwrap().len(), 0);
}

#[test]
fn compare_gate_uses_evaluation_resolved_when_present() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results(baseline_dir.path(), vec![submitted("a"), submitted("b")]);
    write_results(candidate_dir.path(), vec![submitted("a"), submitted("b")]);

    let baseline_eval = serde_json::json!({
        "instances": [
            {"instance_id": "a", "resolved": true, "tests_passed": [], "tests_failed": [], "eval_exit_reason": "resolved"},
            {"instance_id": "b", "resolved": true, "tests_passed": [], "tests_failed": [], "eval_exit_reason": "resolved"}
        ]
    });
    let candidate_eval = serde_json::json!({
        "instances": [
            {"instance_id": "a", "resolved": false, "tests_passed": [], "tests_failed": [], "eval_exit_reason": "unresolved"},
            {"instance_id": "b", "resolved": false, "tests_passed": [], "tests_failed": [], "eval_exit_reason": "unresolved"}
        ]
    });
    std::fs::write(
        baseline_dir.path().join("evaluation.json"),
        serde_json::to_string_pretty(&baseline_eval).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate_dir.path().join("evaluation.json"),
        serde_json::to_string_pretty(&candidate_eval).unwrap(),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--max-regressions",
            "0",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected non-zero with resolved regressions; stdout:
{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["baseline_resolved"], 2);
    assert_eq!(v["candidate_resolved"], 0);
    assert_eq!(v["resolved_delta"], -2);
    assert_eq!(v["regressions"].as_array().unwrap().len(), 2);
}

#[test]
fn evaluate_none_backend_writes_evaluation_json() {
    let sweep_dir = tempfile::tempdir().unwrap();
    write_results(
        sweep_dir.path(),
        vec![submitted("a"), errored("b", FailureCategory::ModelApi)],
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "evaluate",
            "--sweep",
            sweep_dir.path().to_str().unwrap(),
            "--backend",
            "none",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let eval_path = sweep_dir.path().join("evaluation.json");
    assert!(eval_path.exists());
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(eval_path).unwrap()).unwrap();
    assert_eq!(v["instances"].as_array().unwrap().len(), 2);
}

#[test]
fn evaluate_breakdown_none_is_headline_only() {
    let sweep_dir = tempfile::tempdir().unwrap();
    write_results(sweep_dir.path(), vec![submitted("django__django-1")]);
    let out = Command::new(binary_path())
        .args([
            "bench",
            "evaluate",
            "--sweep",
            sweep_dir.path().to_str().unwrap(),
            "--backend",
            "none",
            "--breakdown",
            "none",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("resolved: 0"), "{stdout}");
    assert!(
        !stdout.contains("axis,bucket,n,resolved,resolved_rate"),
        "{stdout}"
    );
}

#[test]
fn compare_breakdown_json_includes_all_buckets_and_threshold_flag() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results(
        baseline_dir.path(),
        vec![submitted("django__django-1"), submitted("psf__requests-2")],
    );
    write_results(
        candidate_dir.path(),
        vec![
            errored("django__django-1", FailureCategory::StepLimit),
            submitted("psf__requests-2"),
        ],
    );
    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--format",
            "json",
            "--breakdown",
            "repo",
            "--breakdown-min-delta-pp",
            "5",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = v["breakdown_delta"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "expected both repos, got: {rows:?}");
    assert!(rows.iter().any(|r| {
        r["bucket_value"] == "django/django"
            && r["delta_resolved_rate"] == -1.0
            && r["exceeds_threshold"] == true
    }));
    assert!(rows.iter().any(|r| {
        r["bucket_value"] == "psf/requests"
            && r["delta_resolved_rate"] == 0.0
            && r["exceeds_threshold"] == false
    }));
}
