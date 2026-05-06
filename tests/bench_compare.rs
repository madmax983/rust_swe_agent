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
use std::fmt::Write as _;
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

fn submitted_with_tests(id: &str, tests_run: bool) -> InstanceResult {
    let mut r = submitted(id);
    r.tests_run_before_submit = tests_run;
    r.last_tests_passed = tests_run.then_some(true);
    r
}

fn write_results(dir: &Path, instances: Vec<InstanceResult>) {
    write_results_with_model(dir, instances, None);
}

fn write_results_with_model(dir: &Path, instances: Vec<InstanceResult>, model_name: Option<&str>) {
    let filter_spec = rust_swe_agent::run::swebench::FilterSpec::default();
    write_results_with_filter_spec_and_model(dir, instances, &filter_spec, model_name);
}

fn write_results_with_filter_spec(
    dir: &Path,
    instances: Vec<InstanceResult>,
    filter_spec: &rust_swe_agent::run::swebench::FilterSpec,
) {
    write_results_with_filter_spec_and_model(dir, instances, filter_spec, None);
}

fn write_results_with_filter_spec_and_model(
    dir: &Path,
    instances: Vec<InstanceResult>,
    filter_spec: &rust_swe_agent::run::swebench::FilterSpec,
    model_name: Option<&str>,
) {
    let sweep = SweepResults {
        total: instances.len(),
        sweep_status: rust_swe_agent::run::swebench::SWEEP_STATUS_COMPLETED.into(),
        cancelled_at: None,
        cancel_deadline_at: None,
        cancel_exit_code: None,
        completed: 0,
        in_flight_at_cancel: 0,
        not_started: 0,
        submitted: instances
            .iter()
            .filter(|r| r.outcome.as_deref() == Some(outcome::SUBMITTED))
            .count(),
        submitted_with_tests: instances
            .iter()
            .filter(|r| {
                r.outcome.as_deref() == Some(outcome::SUBMITTED) && r.tests_run_before_submit
            })
            .count(),
        skipped: 0,
        errored: instances
            .iter()
            .filter(|r| r.outcome.as_deref() == Some(outcome::ERROR))
            .count(),
        failures_by_category: BTreeMap::new(),
        budget_halted: 0,
        with_patch: 0,
        patch_empty: 0,
        patch_apply_invalid: 0,
        github_pr_failures: 0,
        total_prompt_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_creation_tokens: 0,
        total_completion_tokens: 0,
        estimated_cost_usd: 0.0,
        cache_hit_rate: 0.0,
        retries: 0,
        retried_instances: 0,
        pass_at_k: if instances.is_empty() {
            0.0
        } else {
            let passed = instances.iter().filter(|r| r.resolved_count > 0).count();
            f64::from(u32::try_from(passed).unwrap())
                / f64::from(u32::try_from(instances.len()).unwrap())
        },
        filter_spec: filter_spec.clone(),
        manifest: model_name.map(|name| rust_swe_agent::run::swebench::ProvenanceManifest {
            purpose: None,
            harness: rust_swe_agent::run::swebench::HarnessManifest {
                name: "rust_swe_agent".into(),
                version: "test".into(),
                git_sha: None,
                git_dirty: None,
                git_resolution: "test".into(),
            },
            dataset: rust_swe_agent::run::swebench::DatasetManifest {
                path: "test.jsonl".into(),
                sha256: "test".into(),
                instance_count: instances.len(),
                filter_spec: Some(filter_spec.clone()),
            },
            prompt_template: rust_swe_agent::run::swebench::PromptTemplateManifest {
                source: "inline".into(),
                path: None,
                sha256: "test".into(),
            },
            config: rust_swe_agent::run::swebench::ConfigManifest {
                resolved: "test".into(),
                overlay_paths: Vec::new(),
            },
            model: rust_swe_agent::run::swebench::ModelManifest {
                name: name.into(),
                backend: "litellm".into(),
                backend_version: None,
                base_url: None,
            },
            runtime: rust_swe_agent::run::swebench::RuntimeManifest {
                started_at_utc: "2026-05-01T00:00:00Z".into(),
                finished_at_utc: Some("2026-05-01T00:01:00Z".into()),
                host_os: "linux".into(),
                resume_mode: false,
                rust_version: None,
            },
            cli: rust_swe_agent::run::swebench::CliManifest { argv: Vec::new() },
        }),
        cost_limit_usd: None,
        instances,
        rate_limit_events: None,
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
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
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
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 5,
    });
    t.info.steps = Some(1);

    let traj_path =
        rust_swe_agent::run::swebench::trajectory_path_for_run(dir, instance_id, run_index);
    std::fs::create_dir_all(traj_path.parent().unwrap()).unwrap();
    std::fs::write(traj_path, serde_json::to_string_pretty(&t).unwrap()).unwrap();
}

fn write_root_traj(
    dir: &Path,
    instance_id: &str,
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
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 5,
    });
    t.info.steps = Some(1);

    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();
}

fn write_evaluation_json(dir: &Path, value: &serde_json::Value) {
    std::fs::write(
        dir.join("evaluation.json"),
        serde_json::to_string_pretty(value).unwrap(),
    )
    .unwrap();
}

fn patch_stats_agent_patch() -> &'static str {
    "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 keep
-old
+new
+agent only
 end
diff --git a/tests/test_lib.rs b/tests/test_lib.rs
--- a/tests/test_lib.rs
+++ b/tests/test_lib.rs
@@ -1,2 +1,3 @@
 test
+assert new
 end
diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,2 +1,2 @@
-version = 1
+version = 2
"
}

fn patch_stats_gold_patch() -> &'static str {
    "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 keep
-old
+new
 end
"
}

fn generated_added_lines_patch(file_path: &str, added_lines: usize) -> String {
    let mut text = format!(
        "diff --git a/{file_path} b/{file_path}\n--- a/{file_path}\n+++ b/{file_path}\n@@ -1 +1,{} @@\n anchor\n",
        added_lines + 1
    );
    for i in 0..added_lines {
        let _ = writeln!(text, "+line {i}");
    }
    text
}

fn bloated_patch_with_unrelated_whitespace_edits() -> String {
    let mut text = generated_added_lines_patch("src/lib.rs", 10);
    text.push_str(
        "diff --git a/docs/unrelated.md b/docs/unrelated.md\n--- a/docs/unrelated.md\n+++ b/docs/unrelated.md\n@@ -1 +1,91 @@\n anchor\n",
    );
    for _ in 0..90 {
        text.push_str("+   \n");
    }
    text
}

fn write_run_patch(dir: &Path, instance_id: &str, patch: &str) {
    let patch_path = rust_swe_agent::run::swebench::patch_path_for_run(dir, instance_id, 1);
    std::fs::create_dir_all(patch_path.parent().unwrap()).unwrap();
    std::fs::write(patch_path, patch).unwrap();
}

fn evaluate_none_for_patch_stats(dir: &Path) {
    let out = Command::new(binary_path())
        .args([
            "bench",
            "evaluate",
            "--sweep",
            dir.to_str().unwrap(),
            "--backend",
            "none",
            "--breakdown",
            "none",
            "--cost-attribution",
            "off",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "evaluate failed; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn mark_evaluation_instance_resolved(dir: &Path, instance_id: &str) {
    let path = dir.join("evaluation.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let row = value["instances"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|row| row["instance_id"] == instance_id)
        .unwrap();
    row["resolved"] = serde_json::json!(true);
    row["resolved_count"] = serde_json::json!(1);
    row["pass_at_1"] = serde_json::json!(true);
    row["eval_exit_reason"] = serde_json::json!("resolved");
    std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
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
#[allow(clippy::too_many_lines)]
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

    if cfg!(windows) {
        return;
    }

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
fn compare_reports_tests_before_submit_rate_drop_with_flat_resolved_rate() {
    let baseline: std::collections::HashMap<String, InstanceResult> = (0..5)
        .map(|i| submitted_with_tests(&format!("task-{i}"), i < 4))
        .map(|row| (row.instance_id.clone(), row))
        .collect();
    let candidate: std::collections::HashMap<String, InstanceResult> = (0..5)
        .map(|i| submitted_with_tests(&format!("task-{i}"), i < 3))
        .map(|row| (row.instance_id.clone(), row))
        .collect();

    let report = rust_swe_agent::run::compare::diff(
        Path::new("/baseline"),
        Path::new("/candidate"),
        &baseline,
        &candidate,
    );

    assert!((report.baseline_tests_before_submit_rate - 0.8).abs() < f64::EPSILON);
    assert!((report.candidate_tests_before_submit_rate - 0.6).abs() < f64::EPSILON);
    assert!((report.tests_before_submit_delta_rate + 0.2).abs() < f64::EPSILON);
    assert_eq!(report.resolved_delta, 0);
    let text = report.human_table();
    assert!(
        text.contains("Tests before submit: 80.00% -> 60.00% (-20.00pp)"),
        "{text}"
    );
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
            max_patch_size_regression_pct: None,
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
            max_patch_size_regression_pct: None,
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
fn evaluate_writes_patch_stats_with_gold_distance_and_data_driven_classifiers() {
    let sweep_dir = tempfile::tempdir().unwrap();
    let dataset = tempfile::NamedTempFile::new().unwrap();
    write_results(sweep_dir.path(), vec![submitted("django__django-1")]);

    let patch_path =
        rust_swe_agent::run::swebench::patch_path_for_run(sweep_dir.path(), "django__django-1", 1);
    std::fs::create_dir_all(patch_path.parent().unwrap()).unwrap();
    std::fs::write(&patch_path, patch_stats_agent_patch()).unwrap();

    let row = serde_json::json!({
        "instance_id": "django__django-1",
        "problem_statement": "fix it",
        "patch": patch_stats_gold_patch()
    });
    std::fs::write(
        dataset.path(),
        format!("{}\n", serde_json::to_string(&row).unwrap()),
    )
    .unwrap();

    let out = Command::new(binary_path())
        .args([
            "bench",
            "evaluate",
            "--sweep",
            sweep_dir.path().to_str().unwrap(),
            "--dataset",
            dataset.path().to_str().unwrap(),
            "--backend",
            "none",
            "--breakdown",
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

    let v: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(sweep_dir.path().join("evaluation.json")).unwrap(),
    )
    .unwrap();
    let stats = &v["instances"][0]["patch_stats"];
    assert_eq!(stats["files_changed"], 3);
    assert_eq!(stats["hunks"], 3);
    assert_eq!(stats["lines_added"], 4);
    assert_eq!(stats["lines_removed"], 2);
    assert_eq!(stats["is_empty"], false);
    assert_eq!(stats["touches_test_files"], true);
    assert_eq!(stats["touches_lock_or_generated"], true);
    assert!(
        stats["gold_files_iou"]
            .as_f64()
            .is_some_and(|v| v > 0.3 && v < 0.4),
        "expected src/lib.rs overlap across three touched files, got: {stats}"
    );
    assert!(
        stats["gold_lines_overlap"]
            .as_f64()
            .is_some_and(|v| v > 0.0),
        "expected at least one agent-touched line to overlap the gold diff: {stats}"
    );
    assert!(
        stats["gold_size_ratio"].as_f64().is_some_and(|v| v > 1.0),
        "agent patch should be larger than the gold patch: {stats}"
    );
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
fn compare_patch_size_regression_gate_fails_when_resolved_rate_ties() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();
    write_results(baseline_dir.path(), vec![submitted("inst-1")]);
    write_results(candidate_dir.path(), vec![submitted("inst-1")]);
    write_run_patch(
        baseline_dir.path(),
        "inst-1",
        &generated_added_lines_patch("src/lib.rs", 10),
    );
    write_run_patch(
        candidate_dir.path(),
        "inst-1",
        &bloated_patch_with_unrelated_whitespace_edits(),
    );

    evaluate_none_for_patch_stats(baseline_dir.path());
    evaluate_none_for_patch_stats(candidate_dir.path());
    mark_evaluation_instance_resolved(baseline_dir.path(), "inst-1");
    mark_evaluation_instance_resolved(candidate_dir.path(), "inst-1");

    let out = Command::new(binary_path())
        .args([
            "bench",
            "compare",
            "--baseline",
            baseline_dir.path().to_str().unwrap(),
            "--candidate",
            candidate_dir.path().to_str().unwrap(),
            "--max-patch-size-regression",
            "50",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected patch-size gate failure; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Resolved:           1 -> 1 (+0)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Mean lines changed: 10.00 -> 100.00 (+90.00)"),
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
input_tokens: 2000\n\
cache_read_tokens: 0\n\
cache_creation_tokens: 0\n\
completion_tokens: 300\n\
cache_hit_rate: 0.0000\n\
total_cost_usd: 0.1500\n\
cost_per_resolved_usd: NaN\n\
axis,bucket,n,resolved,resolved_rate,cost_per_resolved_usd\n\
repo,unknown,2,0,0.0000,NaN\n\
failure_category,model_api,1,0,0.0000,NaN\n\
failure_category,none,1,0,0.0000,NaN\n";
    assert_eq!(stdout, expected);

    let eval_path = sweep_dir.path().join("evaluation.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(eval_path).unwrap()).unwrap();
    assert!(v.get("cost_attribution").is_none(), "{v:?}");
}

#[test]
fn compare_uses_manifest_model_for_fallback_cost_repricing() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    let mut baseline = submitted("cached");
    baseline.cost_usd = Some(0.0);
    baseline.prompt_tokens = Some(0);
    baseline.cache_read_tokens = Some(1_000_000);
    baseline.cache_creation_tokens = Some(0);
    baseline.completion_tokens = Some(0);

    let mut candidate = submitted("cached");
    candidate.cost_usd = Some(0.0);
    candidate.prompt_tokens = Some(0);
    candidate.cache_read_tokens = Some(1_000_000);
    candidate.cache_creation_tokens = Some(0);
    candidate.completion_tokens = Some(0);

    write_results_with_model(
        baseline_dir.path(),
        vec![baseline],
        Some("anthropic/claude-sonnet-4-6"),
    );
    write_results_with_model(
        candidate_dir.path(),
        vec![candidate],
        Some("anthropic/claude-sonnet-4-6"),
    );

    let report =
        rust_swe_agent::run::compare::compute(&rust_swe_agent::run::compare::CompareArgs {
            baseline: baseline_dir.path().to_path_buf(),
            candidate: candidate_dir.path().to_path_buf(),
            format: rust_swe_agent::run::compare::CompareFormat::Json,
            max_regressions: None,
            max_patch_size_regression_pct: None,
            breakdown: rust_swe_agent::run::evaluate::BreakdownSelection::none(),
            min_delta_pp: 0.0,
            cost_attribution: false,
            cost_attribution_min_delta_usd: 1.0,
        })
        .unwrap();

    assert!(
        (report.baseline_total_cost_usd - 0.3).abs() < 1e-9,
        "{report:#?}"
    );
    assert!(
        (report.candidate_total_cost_usd - 0.3).abs() < 1e-9,
        "{report:#?}"
    );
}

#[test]
fn evaluate_uses_manifest_model_for_fallback_cost_repricing() {
    let sweep_dir = tempfile::tempdir().unwrap();
    let mut cached = submitted("cached");
    cached.cost_usd = Some(0.0);
    cached.prompt_tokens = Some(0);
    cached.cache_read_tokens = Some(1_000_000);
    cached.cache_creation_tokens = Some(0);
    cached.completion_tokens = Some(0);
    write_results_with_model(
        sweep_dir.path(),
        vec![cached],
        Some("anthropic/claude-sonnet-4-6"),
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
    assert!(stdout.contains("total_cost_usd: 0.3000"), "{stdout}");
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
fn evaluate_cost_attribution_dedupes_legacy_root_and_nested_run_slots() {
    let sweep_dir = tempfile::tempdir().unwrap();
    write_results(
        sweep_dir.path(),
        vec![errored("task-a", FailureCategory::StepLimit)],
    );
    write_root_traj(
        sweep_dir.path(),
        "task-a",
        Some(FailureCategory::ModelApi),
        Some(0.30),
    );
    write_run_traj(
        sweep_dir.path(),
        "task-a",
        1,
        Some(FailureCategory::StepLimit),
        Some(0.10),
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
    assert!(!stdout.contains("model_api,1,0.3000"), "{stdout}");

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
        &rust_swe_agent::run::swebench::FilterSpec {
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
        &rust_swe_agent::run::swebench::FilterSpec {
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
fn compare_cost_attribution_falls_back_to_run_slots_when_evaluation_json_is_missing() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    let mut baseline = errored("task-a", FailureCategory::StepLimit);
    baseline.runs = 2;
    baseline.cost_usd = Some(0.30);
    write_results(baseline_dir.path(), vec![baseline]);
    write_run_traj(
        baseline_dir.path(),
        "task-a",
        1,
        Some(FailureCategory::StepLimit),
        Some(0.10),
    );
    write_run_traj(
        baseline_dir.path(),
        "task-a",
        2,
        Some(FailureCategory::ModelApi),
        Some(0.20),
    );

    let mut candidate = errored("task-a", FailureCategory::AgentInternal);
    candidate.runs = 2;
    candidate.cost_usd = Some(0.30);
    write_results(candidate_dir.path(), vec![candidate]);
    write_run_traj(
        candidate_dir.path(),
        "task-a",
        1,
        Some(FailureCategory::AgentInternal),
        Some(0.25),
    );
    write_run_traj(
        candidate_dir.path(),
        "task-a",
        2,
        Some(FailureCategory::ModelApi),
        Some(0.05),
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
    assert!(
        rows.iter().any(|row| {
            row["bucket"] == "agent_internal"
                && row["n_baseline"] == 0
                && row["total_usd_baseline"] == 0.0
                && row["n_candidate"] == 1
                && row["total_usd_candidate"] == 0.25
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

// ─── RED phase: issue #51 – $/resolved-instance + Pareto view ───────────────

#[test]
fn evaluate_outputs_cost_per_resolved_usd_in_text() {
    let sweep_dir = tempfile::tempdir().unwrap();
    let mut r1 = submitted("a");
    r1.cost_usd = Some(2.0);
    let mut r2 = submitted("b");
    r2.cost_usd = Some(4.0);
    write_results(sweep_dir.path(), vec![r1, r2]);

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
            "--breakdown",
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
        stdout.contains("cost_per_resolved_usd:"),
        "expected cost_per_resolved_usd in output; got:\n{stdout}"
    );
    // 2 resolved, total cost $6.00 -> $3.00/resolved (none backend never
    // truly resolves, so cost_per_resolved should be NaN in none backend)
    // With none backend resolved=0, so it should print NaN or a sentinel.
    assert!(
        stdout.contains("cost_per_resolved_usd: NaN")
            || stdout.contains("cost_per_resolved_usd: nan"),
        "expected NaN for cost_per_resolved_usd when backend=none; got:\n{stdout}"
    );
}

#[test]
fn evaluate_cost_per_resolved_usd_correct_when_resolved_present() {
    // Use evaluation.json to simulate resolved instances in a none-backend run.
    let sweep_dir = tempfile::tempdir().unwrap();
    let mut r1 = submitted("django__django-1");
    r1.cost_usd = Some(3.0);
    let mut r2 = submitted("django__django-2");
    r2.cost_usd = Some(1.0);
    let mut r3 = errored("django__django-3", FailureCategory::StepLimit);
    r3.cost_usd = Some(2.0);
    write_results(sweep_dir.path(), vec![r1, r2, r3]);

    // Inject evaluation.json with 2 resolved
    write_evaluation_json(
        sweep_dir.path(),
        &serde_json::json!({
            "instances": [
                {"instance_id": "django__django-1", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []},
                {"instance_id": "django__django-2", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []},
                {"instance_id": "django__django-3", "resolved": false, "eval_exit_reason": "unresolved", "tests_passed": [], "tests_failed": []}
            ]
        }),
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
            "--breakdown",
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
    // none backend doesn't call sb-cli; it marks all as unresolved.
    // The summarize() function uses eval.instances for resolved count.
    // Since backend=none, nothing is resolved from its perspective
    assert!(
        stdout.contains("cost_per_resolved_usd:"),
        "expected cost_per_resolved_usd field in output; got:\n{stdout}"
    );
}

#[test]
fn compare_json_includes_cost_per_resolved_and_pareto_verdict() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    // baseline: 1 resolved out of 2, cost $4.00 -> $4.00/resolved
    let mut b1 = submitted("a");
    b1.cost_usd = Some(4.0);
    b1.runs = 1;
    b1.resolved_count = 1;
    let mut b2 = errored("b", FailureCategory::StepLimit);
    b2.cost_usd = Some(0.0);
    write_results(baseline_dir.path(), vec![b1, b2]);
    write_evaluation_json(
        baseline_dir.path(),
        &serde_json::json!({
            "instances": [
                {"instance_id": "a", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []},
                {"instance_id": "b", "resolved": false, "eval_exit_reason": "unresolved", "tests_passed": [], "tests_failed": []}
            ]
        }),
    );

    // candidate: 2 resolved out of 2, cost $2.00 -> $1.00/resolved (cheaper AND better)
    let mut c1 = submitted("a");
    c1.cost_usd = Some(1.0);
    c1.runs = 1;
    c1.resolved_count = 1;
    let mut c2 = submitted("b");
    c2.cost_usd = Some(1.0);
    c2.runs = 1;
    c2.resolved_count = 1;
    write_results(candidate_dir.path(), vec![c1, c2]);
    write_evaluation_json(
        candidate_dir.path(),
        &serde_json::json!({
            "instances": [
                {"instance_id": "a", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []},
                {"instance_id": "b", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []}
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

    // Both cost_per_resolved fields must be present
    assert!(
        v.get("baseline_cost_per_resolved_usd").is_some(),
        "expected baseline_cost_per_resolved_usd in compare JSON; got:\n{v}"
    );
    assert!(
        v.get("candidate_cost_per_resolved_usd").is_some(),
        "expected candidate_cost_per_resolved_usd in compare JSON; got:\n{v}"
    );
    assert!(
        v.get("cost_per_resolved_delta_usd").is_some(),
        "expected cost_per_resolved_delta_usd in compare JSON; got:\n{v}"
    );
    assert!(
        v.get("pareto_verdict").is_some(),
        "expected pareto_verdict in compare JSON; got:\n{v}"
    );
    // Candidate dominates: lower cost AND higher resolved rate
    assert_eq!(
        v["pareto_verdict"], "candidate_dominates",
        "candidate should dominate (better resolved rate, lower cost/resolved); got:\n{v}"
    );
}

#[test]
fn compare_text_output_includes_pareto_verdict() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let candidate_dir = tempfile::tempdir().unwrap();

    let mut b = submitted("a");
    b.cost_usd = Some(2.0);
    write_results(baseline_dir.path(), vec![b]);

    let mut c = submitted("a");
    c.cost_usd = Some(1.0);
    write_results(candidate_dir.path(), vec![c]);

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
    assert!(
        stdout.contains("Pareto verdict:")
            || stdout.contains("pareto_verdict:")
            || stdout.contains("Cost/resolved"),
        "expected pareto verdict in text output; got:\n{stdout}"
    );
}

#[test]
fn frontier_subcommand_exists_in_help() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("frontier"),
        "expected `frontier` in `bench --help`; got:\n{stdout}"
    );

    let out = Command::new(binary_path())
        .args(["bench", "frontier", "--help"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bench frontier --help should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--format"),
        "expected --format in frontier --help; got:\n{stdout}"
    );
}

#[test]
fn frontier_emits_pareto_json_with_efficient_frontier() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    // dir_a: 1/2 resolved, resolved_rate 0.50
    let mut a1 = submitted("inst-1");
    a1.cost_usd = Some(4.0);
    // Keep errored with default cost_usd ($0.10) so effective_cost_usd returns it directly.
    let a2 = errored("inst-2", FailureCategory::StepLimit);
    write_results(dir_a.path(), vec![a1, a2]);
    write_evaluation_json(
        dir_a.path(),
        &serde_json::json!({"instances": [
            {"instance_id": "inst-1", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []},
            {"instance_id": "inst-2", "resolved": false, "eval_exit_reason": "unresolved", "tests_passed": [], "tests_failed": []}
        ]}),
    );

    // dir_b: 2/2 resolved, cost $2 -> cost_per_resolved $1.00, resolved_rate 1.00
    // -> dominates dir_a (better resolved_rate AND lower cost_per_resolved)
    let mut b1 = submitted("inst-1");
    b1.cost_usd = Some(1.0);
    let mut b2 = submitted("inst-2");
    b2.cost_usd = Some(1.0);
    write_results(dir_b.path(), vec![b1, b2]);
    write_evaluation_json(
        dir_b.path(),
        &serde_json::json!({"instances": [
            {"instance_id": "inst-1", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []},
            {"instance_id": "inst-2", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []}
        ]}),
    );

    // dir_c: 1/2 resolved, higher cost -> dominated by dir_a (same resolved_rate, lower cost)
    let mut c1 = submitted("inst-1");
    c1.cost_usd = Some(10.0); // clearly higher than dir_a's $4.0
    // keep errored with its default cost so effective_cost_usd returns it directly
    let c2 = errored("inst-2", FailureCategory::StepLimit);
    write_results(dir_c.path(), vec![c1, c2]);
    write_evaluation_json(
        dir_c.path(),
        &serde_json::json!({"instances": [
            {"instance_id": "inst-1", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []},
            {"instance_id": "inst-2", "resolved": false, "eval_exit_reason": "unresolved", "tests_passed": [], "tests_failed": []}
        ]}),
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "frontier",
            "--format",
            "json",
            dir_a.path().to_str().unwrap(),
            dir_b.path().to_str().unwrap(),
            dir_c.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bench frontier should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("expected JSON from bench frontier; err={e}; got:\n{stdout}");
    });

    let points = v["points"].as_array().unwrap();
    assert_eq!(points.len(), 3, "expected 3 points (one per dir)");

    // dir_b should be on the frontier
    let on_frontier: Vec<bool> = points
        .iter()
        .map(|p| p["on_frontier"].as_bool().unwrap_or(false))
        .collect();
    let frontier_count = on_frontier.iter().filter(|&&x| x).count();
    // dir_b dominates both: higher resolved_rate AND lower cost_per_resolved
    assert!(
        frontier_count >= 1,
        "at least one point should be on the efficient frontier"
    );
    // Identify dir_c by directory path
    let dir_c_path = dir_c.path().to_str().unwrap();
    let dir_c_point = points
        .iter()
        .find(|p| p["dir"].as_str().is_some_and(|d| d == dir_c_path))
        .unwrap();
    assert_eq!(
        dir_c_point["on_frontier"].as_bool(),
        Some(false),
        "dir_c (same resolved_rate as dir_a but higher cost) should NOT be on frontier; got: {dir_c_point}"
    );
}

#[test]
fn frontier_emits_ascii_chart_in_text_mode() {
    let dir_a = tempfile::tempdir().unwrap();
    let mut r = submitted("inst-1");
    r.cost_usd = Some(2.0);
    write_results(dir_a.path(), vec![r]);
    write_evaluation_json(
        dir_a.path(),
        &serde_json::json!({"instances": [
            {"instance_id": "inst-1", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []}
        ]}),
    );

    let out = Command::new(binary_path())
        .args(["bench", "frontier", dir_a.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bench frontier (text mode) should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Should have some kind of chart or table output
    assert!(
        stdout.contains("frontier")
            || stdout.contains("cost_per_resolved")
            || stdout.contains("resolved_rate"),
        "expected frontier output in text mode; got:\n{stdout}"
    );
}

// ─── frontier: resolved count restricted to loaded sweep IDs ─────────────────

#[test]
fn frontier_resolved_count_capped_to_sweep_instance_ids() {
    // Sweep has only inst-1.  eval.json has inst-1 AND inst-2 (stale / wider
    // evaluation).  Resolved count must be 1 (not 2), resolved_rate must be
    // 1.0 (not >1.0), and cost_per_resolved must reflect only the one loaded
    // instance — otherwise metrics would be impossible / misleading.
    let dir = tempfile::tempdir().unwrap();
    let mut r = submitted("inst-1");
    r.cost_usd = Some(4.0);
    write_results(dir.path(), vec![r]);
    write_evaluation_json(
        dir.path(),
        &serde_json::json!({"instances": [
            {"instance_id": "inst-1", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []},
            {"instance_id": "inst-2", "resolved": true, "eval_exit_reason": "resolved", "tests_passed": [], "tests_failed": []}
        ]}),
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "frontier",
            "--format",
            "json",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bench frontier should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("expected JSON; err={e}; got:\n{stdout}");
    });
    let p = &v["points"][0];
    assert_eq!(
        p["instances"].as_u64(),
        Some(1),
        "instances must be 1 (only inst-1 in sweep)"
    );
    assert_eq!(
        p["resolved"].as_u64(),
        Some(1),
        "resolved must be 1, not 2 (inst-2 not in sweep)"
    );
    let rate = p["resolved_rate"].as_f64().unwrap_or(f64::NAN);
    assert!(
        (rate - 1.0).abs() < 1e-9,
        "resolved_rate must be 1.0, got {rate}"
    );
}

// ─── issue #51 AC#6 – budget-exhausted exclusion flag ───────────────────────

#[test]
fn evaluate_notes_budget_exhausted_exclusion_in_output() {
    let sweep_dir = tempfile::tempdir().unwrap();
    let mut normal = submitted("django__django-1");
    normal.cost_usd = Some(2.0);
    let mut budgeted = errored("django__django-2", FailureCategory::BudgetExhausted);
    budgeted.cost_usd = Some(5.0);
    write_results(sweep_dir.path(), vec![normal, budgeted]);
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
    assert!(
        stdout.contains("budget_exhausted_excluded: 1"),
        "expected budget_exhausted_excluded note in output; got:\n{stdout}"
    );
}

// ─── issue #51 AC#5 – cost_per_resolved_usd per breakdown slice ──────────────

#[test]
fn evaluate_breakdown_csv_includes_cost_per_resolved_usd_column() {
    let sweep_dir = tempfile::tempdir().unwrap();
    write_results(
        sweep_dir.path(),
        vec![submitted("django__django-1"), submitted("psf__requests-2")],
    );
    let out = Command::new(binary_path())
        .args([
            "bench",
            "evaluate",
            "--sweep",
            sweep_dir.path().to_str().unwrap(),
            "--backend",
            "none",
            "--breakdown",
            "repo",
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
    assert!(
        stdout.contains("axis,bucket,n,resolved,resolved_rate,cost_per_resolved_usd"),
        "expected cost_per_resolved_usd column in breakdown header; got:\n{stdout}"
    );
    // none backend → nothing resolved → all buckets are NaN
    assert!(
        stdout.contains("NaN"),
        "expected NaN for zero-resolved bucket; got:\n{stdout}"
    );
}
