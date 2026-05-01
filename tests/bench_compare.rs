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
        runs: 1,
        resolved_count: 1,
        pass_at_1: true,
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
        runs: 1,
        resolved_count: 0,
        pass_at_1: false,
    }
}

fn rerun_result(id: &str, runs: u32, resolved_count: u32) -> InstanceResult {
    let mut r = if resolved_count > 0 {
        submitted(id)
    } else {
        errored(id, FailureCategory::StepLimit)
    };
    r.runs = runs;
    r.resolved_count = resolved_count;
    r.pass_at_1 = resolved_count > 0;
    r
}

fn write_results(dir: &Path, instances: Vec<InstanceResult>) {
    write_results_with_filter_spec(
        dir,
        instances,
        rust_swe_agent::run::swebench::FilterSpec::default(),
    );
}

fn write_results_with_filter_spec(
    dir: &Path,
    instances: Vec<InstanceResult>,
    filter_spec: rust_swe_agent::run::swebench::FilterSpec,
) {
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
        pass_at_k: if instances.is_empty() {
            0.0
        } else {
            let passed = instances.iter().filter(|r| r.resolved_count > 0).count();
            f64::from(u32::try_from(passed).unwrap())
                / f64::from(u32::try_from(instances.len()).unwrap())
        },
        filter_spec,
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

fn write_wallclock_timeout_results_json(dir: &Path, id: &str) {
    let payload = serde_json::json!({
        "total": 1,
        "submitted": 0,
        "skipped": 0,
        "errored": 1,
        "failures_by_category": {"wallclock_timeout": 1},
        "budget_halted": 0,
        "with_patch": 0,
        "total_prompt_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "retries": 0,
        "retried_instances": 0,
        "pass_at_k": 0.0,
        "filter_spec": {},
        "cost_limit_usd": null,
        "instances": [{
            "instance_id": id,
            "exit_reason": "wallclock_timeout",
            "outcome": "error",
            "failure_category": "wallclock_timeout",
            "steps": 1,
            "duration_secs": 2.0,
            "patch_present": false,
            "non_empty_patch": false,
            "attempts": 1,
            "retry_reasons": [],
            "runs": 1,
            "resolved_count": 0,
            "pass_at_1": false
        }]
    });
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&payload).unwrap(),
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

fn write_run_traj(
    dir: &Path,
    instance_id: &str,
    run_index: u32,
    failure_category: Option<FailureCategory>,
    cost_usd: Option<f64>,
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
    t.info.total_cost_usd = cost_usd;
    t.info.token_usage = Some(TokenUsage {
        prompt_tokens: 10,
        completion_tokens: 5,
    });
    t.info.steps = Some(1);

    let traj_path =
        rust_swe_agent::run::swebench::trajectory_path_for_run(dir, instance_id, run_index);
    std::fs::create_dir_all(traj_path.parent().unwrap()).unwrap();
    std::fs::write(traj_path, serde_json::to_string_pretty(&t).unwrap()).unwrap();
}

fn write_evaluation_json(dir: &Path, value: &serde_json::Value) {
    std::fs::write(
        dir.join("evaluation.json"),
        serde_json::to_string_pretty(value).unwrap(),
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
        "--cost-attribution",
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
fn compare_treats_wallclock_timeout_as_ordinary_failure_transition() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    write_wallclock_timeout_results_json(baseline_dir.path(), "slow");
    write_results(candidate_dir.path(), vec![submitted("slow")]);
    let report =
        rust_swe_agent::run::compare::compute(&rust_swe_agent::run::compare::CompareArgs {
            baseline: baseline_dir.path().to_path_buf(),
            candidate: candidate_dir.path().to_path_buf(),
            format: rust_swe_agent::run::compare::CompareFormat::Json,
            max_regressions: None,
            breakdown: rust_swe_agent::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: true,
            cost_attribution_min_delta_usd: 1.0,
        })
        .unwrap();
    assert_eq!(
        report
            .transitions
            .get(&rust_swe_agent::run::compare::TransitionKind::FailPass),
        Some(&1)
    );
    assert!(report.regressions.is_empty());

    write_results(baseline_dir.path(), vec![submitted("slow")]);
    write_wallclock_timeout_results_json(candidate_dir.path(), "slow");
    let report =
        rust_swe_agent::run::compare::compute(&rust_swe_agent::run::compare::CompareArgs {
            baseline: baseline_dir.path().to_path_buf(),
            candidate: candidate_dir.path().to_path_buf(),
            format: rust_swe_agent::run::compare::CompareFormat::Json,
            max_regressions: None,
            breakdown: rust_swe_agent::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: true,
            cost_attribution_min_delta_usd: 1.0,
        })
        .unwrap();
    assert_eq!(
        report
            .transitions
            .get(&rust_swe_agent::run::compare::TransitionKind::PassFail),
        Some(&1)
    );
    assert_eq!(report.regressions.len(), 1);
    assert_eq!(
        report.regressions[0].candidate_exit_reason.as_deref(),
        Some("wallclock_timeout")
    );
}

#[test]
fn cli_max_regressions_flips_exit_code() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    let ids = (0..10).map(|i| format!("case-{i}")).collect::<Vec<_>>();
    write_results(
        baseline_dir.path(),
        ids.iter().map(|id| submitted(id)).collect(),
    );
    write_results(
        candidate_dir.path(),
        ids.iter()
            .map(|id| errored(id, FailureCategory::StepLimit))
            .collect(),
    );

    // Threshold 0 -> decisive regressions fail the gate.
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

    // Threshold 20 -> 10 regressions <= 20; informational only, exit 0.
    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--max-regressions",
            "20",
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
    let ids = (0..10).map(|i| format!("case-{i}")).collect::<Vec<_>>();
    write_results(
        baseline_dir.path(),
        ids.iter().map(|id| submitted(id)).collect(),
    );
    write_results(
        candidate_dir.path(),
        ids.iter().map(|id| submitted(id)).collect(),
    );

    let baseline_eval = serde_json::json!({
        "instances": ids.iter().map(|id| serde_json::json!({
            "instance_id": id,
            "resolved": true,
            "tests_passed": [],
            "tests_failed": [],
            "eval_exit_reason": "resolved"
        })).collect::<Vec<_>>()
    });
    let candidate_eval = serde_json::json!({
        "instances": ids.iter().map(|id| serde_json::json!({
            "instance_id": id,
            "resolved": false,
            "tests_passed": [],
            "tests_failed": [],
            "eval_exit_reason": "unresolved"
        })).collect::<Vec<_>>()
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
    assert_eq!(v["baseline_resolved"], 10);
    assert_eq!(v["candidate_resolved"], 0);
    assert_eq!(v["resolved_delta"], -10);
    assert_eq!(v["regressions"].as_array().unwrap().len(), 10);
}

#[test]
fn compare_json_marks_small_rerun_delta_as_within_noise() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results(baseline_dir.path(), vec![rerun_result("a", 10, 8)]);
    write_results(candidate_dir.path(), vec![rerun_result("a", 10, 7)]);

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
            "--max-regressions",
            "0",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "CI crossing zero must not fail the gate; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["baseline_runs"], 10);
    assert_eq!(v["candidate_runs"], 10);
    assert_eq!(v["baseline_resolved"], 8);
    assert_eq!(v["candidate_resolved"], 7);
    assert_eq!(v["resolved_delta"], -1);
    assert_eq!(v["within_noise"], true);
    assert_eq!(v["verdict"], "within_noise");
    assert!(v["resolved_delta_ci95"]["lower"].as_f64().unwrap() < 0.0);
    assert!(v["resolved_delta_ci95"]["upper"].as_f64().unwrap() > 0.0);
}

#[test]
fn compare_gate_fails_only_when_rerun_ci_is_below_zero() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results(baseline_dir.path(), vec![rerun_result("a", 10, 10)]);
    write_results(candidate_dir.path(), vec![rerun_result("a", 10, 0)]);

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
        "strong negative CI should fail the regression gate; stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Verdict:            regression"),
        "{stdout}"
    );
    assert!(stdout.contains("Within noise:       false"), "{stdout}");
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("resolved: 0"), "{stdout}");
    assert!(stdout.contains("resolved_rate: 0.0000"), "{stdout}");
    assert!(stdout.contains("pass@1: 0.0000"), "{stdout}");
    assert!(stdout.contains("pass@k: 0.0000"), "{stdout}");
    assert!(
        stdout.contains("bucket,n,total_usd,mean_usd,share_pct"),
        "{stdout}"
    );

    let eval_path = sweep_dir.path().join("evaluation.json");
    assert!(eval_path.exists());
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(eval_path).unwrap()).unwrap();
    assert_eq!(v["instances"].as_array().unwrap().len(), 2);
    assert!(v["cost_attribution"].is_array(), "{v:?}");
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
fn evaluate_cost_attribution_off_matches_legacy_stdout() {
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
            "--cost-attribution",
            "off",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = "\
resolved: 0\n\
resolved_rate: 0.0000\n\
pass@1: 0.0000\n\
pass@k: 0.0000\n\
axis,bucket,n,resolved,resolved_rate\n\
repo,unknown,2,0,0.0000\n\
failure_category,model_api,1,0,0.0000\n\
failure_category,none,1,0,0.0000\n";
    assert_eq!(stdout, expected);

    let eval_path = sweep_dir.path().join("evaluation.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(eval_path).unwrap()).unwrap();
    assert!(v.get("cost_attribution").is_none(), "{v:?}");
}

#[test]
fn evaluate_cost_attribution_uses_per_run_trajectories_for_reruns() {
    let sweep_dir = tempfile::tempdir().unwrap();
    let mut aggregate = errored("task-a", FailureCategory::StepLimit);
    aggregate.runs = 2;
    aggregate.cost_usd = Some(0.30);
    write_results(sweep_dir.path(), vec![aggregate]);
    write_run_traj(
        sweep_dir.path(),
        "task-a",
        1,
        Some(FailureCategory::StepLimit),
        Some(0.10),
    );
    write_run_traj(
        sweep_dir.path(),
        "task-a",
        2,
        Some(FailureCategory::ModelApi),
        Some(0.20),
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("step_limit,1,0.1000,0.1000,33.33"),
        "{stdout}"
    );
    assert!(
        stdout.contains("model_api,1,0.2000,0.2000,66.67"),
        "{stdout}"
    );
    assert!(stdout.contains("TOTAL,2,0.3000,0.1500,100.00"), "{stdout}");

    let eval_path = sweep_dir.path().join("evaluation.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(eval_path).unwrap()).unwrap();
    let rows = v["cost_attribution"].as_array().unwrap();
    assert!(
        rows.iter().any(|row| {
            row["bucket"] == "step_limit" && row["n"] == 1 && row["total_usd"] == 0.1
        })
    );
    assert!(
        rows.iter().any(|row| {
            row["bucket"] == "model_api" && row["n"] == 1 && row["total_usd"] == 0.2
        })
    );
}

#[test]
fn evaluate_cost_attribution_ignores_stale_trajectories_outside_current_sweep() {
    let sweep_dir = tempfile::tempdir().unwrap();
    write_results(
        sweep_dir.path(),
        vec![errored("task-a", FailureCategory::StepLimit)],
    );
    write_run_traj(
        sweep_dir.path(),
        "task-a",
        1,
        Some(FailureCategory::StepLimit),
        Some(0.10),
    );
    write_run_traj(
        sweep_dir.path(),
        "stale-task",
        1,
        Some(FailureCategory::ModelApi),
        None,
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("step_limit,1,0.1000,0.1000,100.00"),
        "{stdout}"
    );
    assert!(stdout.contains("TOTAL,1,0.1000,0.1000,100.00"), "{stdout}");
    assert!(!stdout.contains("model_api,1"), "{stdout}");
    assert!(
        !stdout.contains("warning: cost attribution missing usd_cost"),
        "{stdout}"
    );

    let eval_path = sweep_dir.path().join("evaluation.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(eval_path).unwrap()).unwrap();
    let rows = v["cost_attribution"].as_array().unwrap();
    assert!(
        rows.iter().any(|row| {
            row["bucket"] == "model_api" && row["n"] == 0 && row["total_usd"] == 0.0
        }),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| { row["bucket"] == "TOTAL" && row["n"] == 1 && row["total_usd"] == 0.1 })
    );
}

#[test]
fn compare_cost_attribution_warns_when_dataset_subsets_differ() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results_with_filter_spec(
        baseline_dir.path(),
        vec![submitted("a"), errored("b", FailureCategory::StepLimit)],
        rust_swe_agent::run::swebench::FilterSpec {
            original_count: 10,
            selected_count: 2,
            instance_ids: Some(vec!["a".into(), "b".into()]),
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
        },
    );
    write_results_with_filter_spec(
        candidate_dir.path(),
        vec![errored("a", FailureCategory::ModelApi), submitted("c")],
        rust_swe_agent::run::swebench::FilterSpec {
            original_count: 10,
            selected_count: 2,
            instance_ids: Some(vec!["a".into(), "c".into()]),
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
        },
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Cost attribution delta:"), "{stdout}");
    assert!(
        stdout.contains("totals are not directly comparable"),
        "{stdout}"
    );
    assert_eq!(
        stdout.matches("totals are not directly comparable").count(),
        1,
        "{stdout}"
    );
    assert_eq!(
        stdout.matches("dataset subset differs").count(),
        1,
        "{stdout}"
    );
}

#[test]
fn compare_cost_attribution_prefers_evaluation_json_table_over_aggregate_rows() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    let mut baseline = errored("task-a", FailureCategory::StepLimit);
    baseline.runs = 2;
    baseline.cost_usd = Some(0.30);
    write_results(baseline_dir.path(), vec![baseline]);

    let mut candidate = errored("task-a", FailureCategory::ModelApi);
    candidate.runs = 2;
    candidate.cost_usd = Some(0.30);
    write_results(candidate_dir.path(), vec![candidate]);

    write_evaluation_json(
        baseline_dir.path(),
        &serde_json::json!({
            "instances": [{
                "instance_id": "task-a",
                "resolved": false,
                "runs": 2,
                "resolved_count": 0,
                "pass_at_1": false,
                "tests_passed": [],
                "tests_failed": [],
                "eval_exit_reason": "unresolved"
            }],
            "cost_attribution": [
                {"bucket": "model_api", "n": 1, "total_usd": 0.2, "mean_usd": 0.2, "share_pct": 66.67},
                {"bucket": "step_limit", "n": 1, "total_usd": 0.1, "mean_usd": 0.1, "share_pct": 33.33},
                {"bucket": "TOTAL", "n": 2, "total_usd": 0.3, "mean_usd": 0.15, "share_pct": 100.0}
            ]
        }),
    );
    write_evaluation_json(
        candidate_dir.path(),
        &serde_json::json!({
            "instances": [{
                "instance_id": "task-a",
                "resolved": false,
                "runs": 2,
                "resolved_count": 0,
                "pass_at_1": false,
                "tests_passed": [],
                "tests_failed": [],
                "eval_exit_reason": "unresolved"
            }],
            "cost_attribution": [
                {"bucket": "agent_internal", "n": 1, "total_usd": 0.25, "mean_usd": 0.25, "share_pct": 83.33},
                {"bucket": "model_api", "n": 1, "total_usd": 0.05, "mean_usd": 0.05, "share_pct": 16.67},
                {"bucket": "TOTAL", "n": 2, "total_usd": 0.3, "mean_usd": 0.15, "share_pct": 100.0}
            ]
        }),
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
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = v["cost_attribution_delta"].as_array().unwrap();
    assert!(
        rows.iter().any(|row| {
            row["bucket"] == "step_limit"
                && row["n_baseline"] == 1
                && row["total_usd_baseline"] == 0.1
                && row["n_candidate"] == 0
                && row["total_usd_candidate"] == 0.0
        }),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| {
            row["bucket"] == "model_api"
                && row["n_baseline"] == 1
                && row["total_usd_baseline"] == 0.2
                && row["n_candidate"] == 1
                && row["total_usd_candidate"] == 0.05
        }),
        "{rows:?}"
    );
    assert!(rows.iter().all(|row| row["bucket"] != "TOTAL"), "{rows:?}");
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
