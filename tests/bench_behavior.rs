//! `bench behavior`: surface agent read/edit/test mix by outcome.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

// ── unit tests for action class classification ────────────────────────────────

use rust_swe_agent::run::behavior::{ActionClass, classify_action, classify_turn};

#[test]
fn classify_read_commands() {
    assert_eq!(classify_action("cat file.py"), ActionClass::Read);
    assert_eq!(classify_action("head -20 file.py"), ActionClass::Read);
    assert_eq!(classify_action("tail -f log.txt"), ActionClass::Read);
    assert_eq!(classify_action("ls -la"), ActionClass::Read);
    assert_eq!(classify_action("wc -l file.txt"), ActionClass::Read);
}

#[test]
fn classify_search_commands() {
    assert_eq!(classify_action("grep -r 'def test' ."), ActionClass::Search);
    assert_eq!(classify_action("rg pattern src/"), ActionClass::Search);
    assert_eq!(classify_action("find . -name '*.py'"), ActionClass::Search);
}

#[test]
fn classify_write_commands() {
    assert_eq!(classify_action("sed -i 's/foo/bar/' file.py"), ActionClass::Write);
    assert_eq!(classify_action("awk '{print $1}' data.txt"), ActionClass::Write);
    assert_eq!(classify_action("patch -p1 < fix.patch"), ActionClass::Write);
    assert_eq!(classify_action("tee output.txt"), ActionClass::Write);
}

#[test]
fn classify_test_commands() {
    assert_eq!(classify_action("pytest tests/"), ActionClass::Test);
    assert_eq!(classify_action("cargo test"), ActionClass::Test);
    assert_eq!(classify_action("npm test"), ActionClass::Test);
    assert_eq!(classify_action("go test ./..."), ActionClass::Test);
    assert_eq!(classify_action("jest --coverage"), ActionClass::Test);
}

#[test]
fn classify_build_commands() {
    assert_eq!(classify_action("cargo build"), ActionClass::Build);
    assert_eq!(classify_action("cargo check"), ActionClass::Build);
    assert_eq!(classify_action("make all"), ActionClass::Build);
    assert_eq!(classify_action("cmake .."), ActionClass::Build);
    assert_eq!(classify_action("tsc --outDir dist"), ActionClass::Build);
}

#[test]
fn classify_nav_commands() {
    assert_eq!(classify_action("cd src/"), ActionClass::Nav);
    assert_eq!(classify_action("pwd"), ActionClass::Nav);
    assert_eq!(classify_action("which python"), ActionClass::Nav);
}

#[test]
fn classify_git_commands() {
    assert_eq!(classify_action("git status"), ActionClass::Git);
    assert_eq!(classify_action("git diff HEAD"), ActionClass::Git);
    assert_eq!(classify_action("git log --oneline"), ActionClass::Git);
}

#[test]
fn classify_unknown_heads_land_in_other() {
    assert_eq!(classify_action("myweirdtool --flag"), ActionClass::Other);
    assert_eq!(classify_action("anotherspecialtool --arg"), ActionClass::Other);
}

#[test]
fn classify_turn_applies_priority_order_test_beats_write() {
    // A turn with both test and write actions → test wins (higher priority)
    let class = classify_turn(&["sed -i 's/x/y/' f.py", "pytest tests/"]);
    assert_eq!(class, ActionClass::Test, "test should beat write in priority");
}

#[test]
fn classify_turn_applies_priority_write_beats_read() {
    let class = classify_turn(&["cat file.py", "sed -i 's/x/y/' f.py"]);
    assert_eq!(class, ActionClass::Write, "write should beat read in priority");
}

#[test]
fn classify_turn_applies_priority_search_beats_read() {
    let class = classify_turn(&["cat file.py", "grep -r pattern ."]);
    assert_eq!(class, ActionClass::Search, "search should beat read in priority");
}

#[test]
fn classify_turn_empty_actions_is_noop() {
    let class = classify_turn(&[]);
    assert_eq!(class, ActionClass::Noop, "empty actions should be noop");
}

#[test]
fn classify_turn_submit_only_is_noop() {
    let class = classify_turn(&["__SUBMIT__"]);
    assert_eq!(class, ActionClass::Noop, "__SUBMIT__ only should be noop");
}

#[test]
fn classify_turn_strips_sudo_prefix() {
    assert_eq!(classify_action("sudo sed -i 's/x/y/' f.py"), ActionClass::Write);
}

#[test]
fn classify_turn_strips_env_prefix() {
    assert_eq!(classify_action("env RUST_LOG=debug cargo test"), ActionClass::Test);
}

// ── CLI integration tests ─────────────────────────────────────────────────────

fn copy_main_behavior_fixture(dest: &Path) {
    let src = Path::new("tests/fixtures/behavior/main_sweep");
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
    }
}

fn copy_noop_behavior_fixture(dest: &Path) {
    let src = Path::new("tests/fixtures/behavior/noop_sweep");
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
    }
}

#[test]
fn cli_produces_text_output() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench behavior failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("bench behavior"), "{stdout}");
    assert!(stdout.contains("resolved"), "{stdout}");
    assert!(stdout.contains("unresolved"), "{stdout}");
}

#[test]
fn cli_writes_behavior_json_artifact() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench behavior failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let artifact_path = sweep.path().join("behavior.json");
    assert!(artifact_path.exists(), "behavior.json should be written");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifact_path).unwrap()).unwrap();

    assert!(report["sweep"].is_string(), "sweep field required");
    assert!(report["generated_at"].is_string(), "generated_at required");
    assert!(report["taxonomy_version"].is_number(), "taxonomy_version required");
    assert!(report["totals"].is_object(), "totals required");
    assert!(report["by_outcome"].is_object(), "by_outcome required");
    assert!(report["unclassified_heads"].is_object(), "unclassified_heads required");
}

#[test]
fn cli_json_format_emits_valid_json_to_stdout() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench behavior --format json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["by_outcome"].is_object());
    assert!(report["taxonomy_version"].is_number());
}

#[test]
fn edit_heavy_resolved_vs_read_heavy_unresolved_shape_diff() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    // Resolved bucket should be write-heavy
    let resolved = &report["by_outcome"]["resolved"];
    let write_share = resolved["write"]["share"].as_f64().unwrap_or(0.0);
    assert!(
        write_share > 0.1,
        "resolved bucket should have significant write share, got {write_share}"
    );

    // Unresolved bucket should be read-heavy
    let unresolved = &report["by_outcome"]["unresolved"];
    let read_share = unresolved["read"]["share"].as_f64().unwrap_or(0.0);
    assert!(
        read_share > 0.5,
        "unresolved bucket should be read-heavy, got {read_share}"
    );

    // comparisons should show write with positive delta (more in resolved than unresolved)
    let comparisons = report["comparisons"]["resolved_vs_unresolved"]
        .as_array()
        .unwrap();
    let write_entry = comparisons
        .iter()
        .find(|e| e["action_class"].as_str() == Some("write"))
        .expect("write class should appear in resolved_vs_unresolved comparison");
    let delta = write_entry["share_delta"].as_f64().unwrap();
    assert!(
        delta > 0.10,
        "write share delta should be ≥ 10pp, got {delta:.4}"
    );
}

#[test]
fn noop_only_sweep_reports_noop_share_one() {
    let sweep = tempfile::tempdir().unwrap();
    copy_noop_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench behavior on noop sweep failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let all_bucket = &report["by_outcome"]["all"];
    let noop_share = all_bucket["noop"]["share"].as_f64().unwrap_or(0.0);
    assert!(
        (noop_share - 1.0).abs() < 1e-9,
        "noop-only sweep should have noop share = 1.0, got {noop_share}"
    );
}

#[test]
fn unknown_heads_appear_in_unclassified_heads() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let unclassified = report["unclassified_heads"].as_object().unwrap();
    assert!(
        unclassified.contains_key("myweirdtool") || unclassified.contains_key("anotherspecialtool"),
        "unclassified_heads should contain unknown command heads, got: {unclassified:?}"
    );
}

#[test]
fn per_instance_flag_emits_per_instance_data() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
            "--per-instance",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bench behavior --per-instance failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let per_instance = report["per_instance"]
        .as_array()
        .expect("per_instance should be present when --per-instance is set");
    assert!(
        !per_instance.is_empty(),
        "per_instance array should not be empty"
    );

    let first = &per_instance[0];
    assert!(first["instance_id"].is_string(), "instance_id required in per-instance row");
    assert!(first["class_counts"].is_object(), "class_counts required in per-instance row");
}

#[test]
fn determinism_same_output_on_two_runs() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let run = |sweep_path: &str| -> serde_json::Value {
        let output = Command::new(binary_path())
            .args([
                "--log",
                "error",
                "bench",
                "behavior",
                "--sweep",
                sweep_path,
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let r1 = run(sweep.path().to_str().unwrap());
    let r2 = run(sweep.path().to_str().unwrap());

    // Compare everything except generated_at
    let strip_ts = |mut v: serde_json::Value| {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("generated_at");
        }
        v
    };

    assert_eq!(
        strip_ts(r1),
        strip_ts(r2),
        "bench behavior should be deterministic (modulo generated_at)"
    );
}

#[test]
fn bucket_filter_all_includes_all_instances() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--bucket",
            "all",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["by_outcome"]["all"].is_object());
}

#[test]
fn min_share_hides_low_share_classes() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    // With --min-share 0.9, most classes should be hidden in the text output
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--min-share",
            "0.9",
        ])
        .output()
        .unwrap();

    // Should succeed even if all classes are filtered
    assert!(
        output.status.success(),
        "bench behavior --min-share 0.9 failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_bucket_exits_with_error() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--bucket",
            "invalid_bucket",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "bench behavior --bucket invalid_bucket should fail"
    );
}

#[test]
fn missing_sweep_exits_with_error() {
    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            "/nonexistent/sweep/path",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "bench behavior with missing sweep should fail"
    );
}

#[test]
fn by_outcome_schema_includes_all_required_buckets() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let by_outcome = report["by_outcome"].as_object().unwrap();
    assert!(by_outcome.contains_key("resolved"), "resolved bucket required");
    assert!(by_outcome.contains_key("unresolved"), "unresolved bucket required");
    assert!(by_outcome.contains_key("errored"), "errored bucket required");
    assert!(by_outcome.contains_key("all"), "all bucket required");
}

#[test]
fn class_metrics_include_required_fields() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Find a class with turns in the all bucket
    let all_bucket = report["by_outcome"]["all"].as_object().unwrap();
    let first_class = all_bucket.values().next().expect("at least one class in all bucket");
    assert!(first_class["turn_count"].is_number(), "turn_count required");
    assert!(first_class["share"].is_number(), "share required");
    assert!(first_class["mean_turns_per_instance"].is_number(), "mean_turns_per_instance required");
    assert!(first_class["attributed_cost_usd"].is_number(), "attributed_cost_usd required");
}

#[test]
fn totals_include_all_classes_with_share_fields() {
    let sweep = tempfile::tempdir().unwrap();
    copy_main_behavior_fixture(sweep.path());

    let output = Command::new(binary_path())
        .args([
            "--log",
            "error",
            "bench",
            "behavior",
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let totals = report["totals"].as_object().unwrap();
    // Totals should have at least one class
    assert!(!totals.is_empty(), "totals should not be empty");
    let first = totals.values().next().unwrap();
    assert!(first["turn_count"].is_number(), "turn_count required in totals");
    assert!(first["share"].is_number(), "share required in totals");
}
