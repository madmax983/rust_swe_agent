//! Tests for `bench evaluator-selftest` — the gold-patch preflight command.
//!
//! Three fixture instances:
//!   1. `resolved-instance`  — has a non-empty `patch` field → resolved
//!   2. `no-patch-instance`  — `patch` field absent → `gold_patch_missing` errored
//!   3. `fail-instance`      — `patch` field present but evaluator wired to fail → unresolved

#![allow(clippy::unwrap_used)]

use rust_swe_agent::run::evaluator_selftest::{
    SelftestArgs, SelftestExitStatus, run as run_selftest,
};
use std::path::PathBuf;

fn write_fixture_dataset(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("dataset.jsonl");
    let content = concat!(
        "{\"instance_id\":\"resolved-instance\",\"patch\":\"diff --git a/x.py b/x.py\\n--- a/x.py\\n+++ b/x.py\\n@@ -1 +1 @@\\n-old\\n+new\\n\"}\n",
        "{\"instance_id\":\"no-patch-instance\"}\n",
        "{\"instance_id\":\"fail-instance\",\"patch\":\"diff --git a/y.py b/y.py\\n--- a/y.py\\n+++ b/y.py\\n@@ -1 +1 @@\\n-a\\n+b\\n\",\"selftest_force_fail\":true}\n",
    );
    std::fs::write(&path, content).unwrap();
    path
}

fn write_resolved_only_dataset(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("dataset_resolved.jsonl");
    let content = "{\"instance_id\":\"gold-ok\",\"patch\":\"diff --git a/f.py b/f.py\\n--- a/f.py\\n+++ b/f.py\\n@@ -1 +1 @@\\n-x\\n+y\\n\"}\n";
    std::fs::write(&path, content).unwrap();
    path
}

fn write_no_patch_dataset(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("dataset_nopatch.jsonl");
    let content = "{\"instance_id\":\"missing-patch\"}\n";
    std::fs::write(&path, content).unwrap();
    path
}

/// Construct a `SelftestArgs` with `none` backend defaults, overriding only
/// the dataset path and output dir. Tests that need other fields set them
/// explicitly after calling this helper.
fn make_args(dataset_path: PathBuf, output_dir: PathBuf) -> SelftestArgs {
    SelftestArgs {
        dataset_path,
        output_dir,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        format: "text".into(),
        backend: "none".into(),
        sb_subset: "swe-bench-m".into(),
        sb_split: "dev".into(),
        timeout_per_instance: 600,
        parallel: 4,
    }
}

// ── core result struct tests ──────────────────────────────────────────────────

#[test]
fn missing_patch_field_recorded_as_gold_patch_missing() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_no_patch_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    let selftest_out = result.output;
    assert_eq!(selftest_out.instances.len(), 1);
    let inst = &selftest_out.instances[0];
    assert_eq!(inst.instance_id, "missing-patch");
    assert!(!inst.resolved, "missing patch must not be resolved");
    assert_eq!(
        inst.evaluator_exit_reason, "gold_patch_missing",
        "reason must be gold_patch_missing"
    );
}

#[test]
fn empty_patch_string_treated_as_missing() {
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("dataset.jsonl");
    std::fs::write(&path, "{\"instance_id\":\"empty-patch\",\"patch\":\"\"}\n").unwrap();
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(path, output));

    let inst = &result.output.instances[0];
    assert_eq!(inst.evaluator_exit_reason, "gold_patch_missing");
    assert!(!inst.resolved);
}

#[test]
fn nonempty_patch_marks_resolved() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    let inst = &result.output.instances[0];
    assert!(inst.resolved, "non-empty gold patch must be resolved");
    assert_eq!(inst.evaluator_exit_reason, "resolved");
}

#[test]
fn evaluator_duration_ms_is_recorded() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    // Duration is always recorded (may be 0 in test, but key must be present).
    let inst = &result.output.instances[0];
    let _ = inst.evaluator_duration_ms; // field must exist
}

// ── JSON artifact tests ───────────────────────────────────────────────────────

#[test]
fn json_artifact_written_to_output_dir() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    run_selftest(make_args(dataset, output.clone()));

    let artifact_path = output.join("evaluator_selftest.json");
    assert!(
        artifact_path.exists(),
        "evaluator_selftest.json must be written"
    );
}

#[test]
fn json_artifact_has_required_schema_fields() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    run_selftest(make_args(dataset, output.clone()));

    let text = std::fs::read_to_string(output.join("evaluator_selftest.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let obj = v.as_object().unwrap();

    assert!(obj.contains_key("schema_version"), "missing schema_version");
    assert!(obj.contains_key("dataset_path"), "missing dataset_path");
    assert!(obj.contains_key("dataset_sha256"), "missing dataset_sha256");
    assert!(obj.contains_key("timestamp_utc"), "missing timestamp_utc");
    assert!(
        obj.contains_key("evaluator_backend"),
        "missing evaluator_backend"
    );
    assert!(obj.contains_key("instances"), "missing instances");
    assert!(obj.contains_key("totals"), "missing totals");

    // totals sub-fields
    let totals = obj.get("totals").unwrap().as_object().unwrap();
    assert!(totals.contains_key("instances_total"));
    assert!(totals.contains_key("instances_resolved"));
    assert!(totals.contains_key("instances_unresolved"));
    assert!(totals.contains_key("instances_errored"));

    // per-instance fields
    let instances = obj.get("instances").unwrap().as_array().unwrap();
    assert!(!instances.is_empty());
    let first = instances[0].as_object().unwrap();
    assert!(first.contains_key("instance_id"));
    assert!(first.contains_key("resolved"));
    assert!(first.contains_key("evaluator_exit_reason"));
    assert!(first.contains_key("evaluator_duration_ms"));
}

#[test]
fn json_artifact_dataset_path_matches_input() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    run_selftest(make_args(dataset, output.clone()));

    let text = std::fs::read_to_string(output.join("evaluator_selftest.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let dataset_path_in_json = v["dataset_path"].as_str().unwrap();
    assert!(
        dataset_path_in_json.contains("dataset_resolved.jsonl"),
        "dataset_path in JSON must reference the input file, got: {dataset_path_in_json}"
    );
}

// ── totals tests ─────────────────────────────────────────────────────────────

#[test]
fn totals_match_per_instance_results() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    let totals = &result.output.totals;
    assert_eq!(totals.instances_total, 3);
    // resolved-instance → resolved
    // no-patch-instance → errored (gold_patch_missing)
    // fail-instance     → errored (evaluator_failed counts as infrastructure error)
    assert_eq!(totals.instances_resolved, 1);
    assert_eq!(totals.instances_errored, 2);
    assert_eq!(totals.instances_unresolved, 0);
    assert_eq!(
        totals.instances_resolved + totals.instances_unresolved + totals.instances_errored,
        totals.instances_total
    );
}

// ── exit status tests ─────────────────────────────────────────────────────────

#[test]
fn exit_status_ok_when_all_resolved() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    assert_eq!(
        result.exit_status,
        SelftestExitStatus::AllResolved,
        "all resolved → AllResolved exit status"
    );
}

#[test]
fn exit_status_nonzero_when_any_unresolved() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    assert_ne!(
        result.exit_status,
        SelftestExitStatus::AllResolved,
        "mixed results → non-AllResolved exit status"
    );
}

#[test]
fn exit_status_nonzero_for_errored_only() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_no_patch_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    assert_ne!(result.exit_status, SelftestExitStatus::AllResolved);
}

// ── stdout format tests ───────────────────────────────────────────────────────

#[test]
fn stdout_text_contains_headline() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    assert!(
        result.stdout.contains("evaluator self-test"),
        "headline must contain 'evaluator self-test', got: {}",
        result.stdout
    );
    assert!(
        result.stdout.contains("1/1 resolved"),
        "headline must show resolved count, got: {}",
        result.stdout
    );
}

#[test]
fn stdout_text_lists_non_resolved_instances() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(make_args(dataset, output));

    // The non-resolved instances should appear in the output.
    assert!(
        result.stdout.contains("no-patch-instance"),
        "stdout must list errored instance: {}",
        result.stdout
    );
    assert!(
        result.stdout.contains("fail-instance"),
        "stdout must list failed instance: {}",
        result.stdout
    );
    // The resolved instance should NOT appear in the non-resolved table.
    // (It only appears in the headline count.)
}

#[test]
fn stdout_json_format_emits_json_only() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(SelftestArgs {
        format: "json".into(),
        ..make_args(dataset, output)
    });

    let parsed: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert!(parsed.is_object(), "JSON output must be an object");
}

// ── slicing tests ─────────────────────────────────────────────────────────────

#[test]
fn limit_restricts_instance_count() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(SelftestArgs {
        limit: Some(1),
        ..make_args(dataset, output)
    });

    assert_eq!(
        result.output.instances.len(),
        1,
        "--limit 1 must process exactly 1 instance"
    );
    assert_eq!(result.output.totals.instances_total, 1);
}

#[test]
fn instance_ids_filter_works() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(SelftestArgs {
        instance_ids: Some("resolved-instance".into()),
        ..make_args(dataset, output)
    });

    assert_eq!(result.output.instances.len(), 1);
    assert_eq!(result.output.instances[0].instance_id, "resolved-instance");
}

#[test]
fn instance_ids_at_file_filter_works() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    // Write a file containing the instance IDs to select.
    let ids_file = work.path().join("ids.txt");
    std::fs::write(&ids_file, "resolved-instance\n").unwrap();

    let result = run_selftest(SelftestArgs {
        instance_ids: Some(format!("@{}", ids_file.display())),
        ..make_args(dataset, output)
    });

    assert_eq!(result.output.instances.len(), 1);
    assert_eq!(result.output.instances[0].instance_id, "resolved-instance");
}

// ── determinism test ─────────────────────────────────────────────────────────

#[test]
fn two_runs_produce_identical_json_modulo_timestamp_and_duration() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());

    let out1 = work.path().join("run1");
    let out2 = work.path().join("run2");
    std::fs::create_dir_all(&out1).unwrap();
    std::fs::create_dir_all(&out2).unwrap();

    run_selftest(make_args(dataset.clone(), out1.clone()));
    run_selftest(make_args(dataset, out2.clone()));

    let json1: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out1.join("evaluator_selftest.json")).unwrap(),
    )
    .unwrap();
    let json2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out2.join("evaluator_selftest.json")).unwrap(),
    )
    .unwrap();

    // Compare all fields except timestamp_utc and evaluator_duration_ms.
    let normalize = |v: serde_json::Value| -> serde_json::Value {
        let mut obj = v.as_object().unwrap().clone();
        obj.remove("timestamp_utc");
        // Normalize per-instance durations to 0.
        if let Some(instances) = obj.get_mut("instances") {
            for inst in instances.as_array_mut().unwrap() {
                if let Some(o) = inst.as_object_mut() {
                    o.insert(
                        "evaluator_duration_ms".into(),
                        serde_json::Value::Number(0.into()),
                    );
                }
            }
        }
        serde_json::Value::Object(obj)
    };

    assert_eq!(
        normalize(json1),
        normalize(json2),
        "two runs must produce identical JSON (modulo timestamp and duration)"
    );
}

// ── fail-instance handling ────────────────────────────────────────────────────

#[test]
fn fail_instance_exit_reason_surfaced_in_stdout() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    let result = run_selftest(SelftestArgs {
        instance_ids: Some("fail-instance".into()),
        ..make_args(dataset, output)
    });

    let inst = &result.output.instances[0];
    assert!(!inst.resolved, "fail-instance must not be resolved");
    // The exit reason should be non-empty and surfaced in stdout.
    assert!(
        !inst.evaluator_exit_reason.is_empty(),
        "exit reason must be non-empty"
    );
    assert!(
        result.stdout.contains(&inst.evaluator_exit_reason),
        "exit reason must appear in stdout: {}",
        result.stdout
    );
}

#[test]
fn fail_instance_exit_reason_in_json() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_fixture_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    run_selftest(SelftestArgs {
        instance_ids: Some("fail-instance".into()),
        ..make_args(dataset, output.clone())
    });

    let text = std::fs::read_to_string(output.join("evaluator_selftest.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let instances = v["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    let reason = instances[0]["evaluator_exit_reason"].as_str().unwrap();
    assert!(
        !reason.is_empty(),
        "exit_reason must be non-empty in JSON artifact"
    );
    assert_ne!(reason, "gold_patch_missing");
    assert_ne!(reason, "resolved");
}

// ── backend field in artifact ─────────────────────────────────────────────────

#[test]
fn evaluator_backend_field_reflects_none_backend() {
    let work = tempfile::tempdir().unwrap();
    let dataset = write_resolved_only_dataset(work.path());
    let output = work.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    run_selftest(make_args(dataset, output.clone()));

    let text = std::fs::read_to_string(output.join("evaluator_selftest.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["evaluator_backend"].as_str().unwrap(), "none");
}
