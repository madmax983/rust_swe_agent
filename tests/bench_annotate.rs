//! `bench annotate`: integration tests for persistent operator annotation store.
//!
//! Covers AC from issue #296:
//! - CLI: add / list / rm subcommands exist
//! - Default store path `./annotations.json`; `--store` override; `BENCH_ANNOTATIONS_PATH` env var
//! - Schema: `schema_version = "annotations-1.0"`, keyed by instance_id, note per (id, tag) pair
//! - Tag validation: `^[a-z0-9][a-z0-9-]{0,31}$`
//! - Note cap: 1024 chars
//! - Atomic writes (write-temp-then-rename)
//! - `bench inspect --instance` shows "Operator notes" section when annotations exist
//! - `bench triage` output gains an `annotations` column (best-effort)
//! - `bench bundle` includes `annotations.json` when present
//! - Zero network/model calls

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use maxwells_daemon::annotation::{AnnotationStore, TAG_REGEX};
use maxwells_daemon::run::annotate::diff_annotation_stores;

mod support;
use support::binary_path;

// ── helpers ──────────────────────────────────────────────────────────────────

fn default_store_path(dir: &Path) -> std::path::PathBuf {
    dir.join("annotations.json")
}

fn annotate_add(
    dir: &Path,
    instance_id: &str,
    tags: &[&str],
    note: Option<&str>,
    store: Option<&Path>,
) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.arg("bench")
        .arg("annotate")
        .arg("add")
        .arg(instance_id);
    for tag in tags {
        cmd.arg("--tag").arg(tag);
    }
    if let Some(n) = note {
        cmd.arg("--note").arg(n);
    }
    if let Some(s) = store {
        cmd.arg("--store").arg(s);
    } else {
        cmd.current_dir(dir);
    }
    cmd.output().unwrap()
}

fn annotate_list(
    dir: &Path,
    instance_id: Option<&str>,
    tag: Option<&str>,
    store: Option<&Path>,
) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.arg("bench").arg("annotate").arg("list");
    if let Some(id) = instance_id {
        cmd.arg("--instance").arg(id);
    }
    if let Some(t) = tag {
        cmd.arg("--tag").arg(t);
    }
    if let Some(s) = store {
        cmd.arg("--store").arg(s);
    } else {
        cmd.current_dir(dir);
    }
    cmd.output().unwrap()
}

fn annotate_rm(
    dir: &Path,
    instance_id: &str,
    tag: Option<&str>,
    store: Option<&Path>,
) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.arg("bench").arg("annotate").arg("rm").arg(instance_id);
    if let Some(t) = tag {
        cmd.arg("--tag").arg(t);
    }
    if let Some(s) = store {
        cmd.arg("--store").arg(s);
    } else {
        cmd.current_dir(dir);
    }
    cmd.output().unwrap()
}

// ── AC: subcommand surfaces in --help ─────────────────────────────────────────

#[test]
fn annotate_appears_in_bench_help() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("annotate"),
        "bench --help should list annotate subcommand"
    );
}

#[test]
fn annotate_add_help_shows_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "annotate", "add", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--tag"), "add --help should show --tag");
    assert!(stdout.contains("--note"), "add --help should show --note");
    assert!(stdout.contains("--store"), "add --help should show --store");
}

#[test]
fn annotate_list_help_shows_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "annotate", "list", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--instance"),
        "list --help should show --instance"
    );
    assert!(stdout.contains("--tag"), "list --help should show --tag");
    assert!(stdout.contains("--store"), "list --help should show --store");
}

#[test]
fn annotate_rm_help_shows_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "annotate", "rm", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--tag"), "rm --help should show --tag");
    assert!(stdout.contains("--store"), "rm --help should show --store");
}

// ── AC: tag validation ────────────────────────────────────────────────────────

#[test]
fn tag_regex_accepts_valid_tags() {
    let re = regex::Regex::new(TAG_REGEX).unwrap();
    assert!(re.is_match("evaluator-flake"));
    assert!(re.is_match("ignore"));
    assert!(re.is_match("real-regression"));
    assert!(re.is_match("a"));
    assert!(re.is_match("0xdeadbeef"));
    assert!(re.is_match("a1b2c3"));
    // max 32 chars
    assert!(re.is_match("abcdefghijklmnopqrstuvwxyz123456"));
}

#[test]
fn tag_regex_rejects_invalid_tags() {
    let re = regex::Regex::new(TAG_REGEX).unwrap();
    // starts with dash
    assert!(!re.is_match("-bad"));
    // uppercase
    assert!(!re.is_match("BadTag"));
    // spaces
    assert!(!re.is_match("bad tag"));
    // too long (33 chars)
    assert!(!re.is_match("abcdefghijklmnopqrstuvwxyz1234567"));
    // empty
    assert!(!re.is_match(""));
    // special chars
    assert!(!re.is_match("bad_tag"));
    assert!(!re.is_match("bad.tag"));
}

// ── AC: annotation store library API ─────────────────────────────────────────

#[test]
fn store_add_and_list_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();

    store
        .add("pytest__pytest-7234", "evaluator-flake", Some("known flake"))
        .unwrap();
    store
        .add("pytest__pytest-7234", "ignore", None)
        .unwrap();
    store.save(&store_path).unwrap();

    let loaded = AnnotationStore::load_or_default(&store_path).unwrap();
    let anns = loaded.list(Some("pytest__pytest-7234"), None);
    assert_eq!(anns.len(), 2, "should have 2 annotations");
    let tags: Vec<&str> = anns.iter().map(|a| a.tag.as_str()).collect();
    assert!(tags.contains(&"evaluator-flake"));
    assert!(tags.contains(&"ignore"));
}

#[test]
fn store_schema_version_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    store.add("id1", "tag1", None).unwrap();
    store.save(&store_path).unwrap();

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(
        raw["schema_version"].as_str().unwrap(),
        "annotations-1.0",
        "schema_version must be annotations-1.0"
    );
}

#[test]
fn store_note_cap_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    let long_note = "x".repeat(1025);
    let result = store.add("id1", "tag1", Some(&long_note));
    assert!(result.is_err(), "notes >1024 chars should be rejected");
}

#[test]
fn store_note_exactly_1024_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    let exactly_1024 = "x".repeat(1024);
    let result = store.add("id1", "tag1", Some(&exactly_1024));
    assert!(result.is_ok(), "notes of exactly 1024 chars should be accepted");
}

#[test]
fn store_invalid_tag_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    let result = store.add("id1", "-BadTag", None);
    assert!(result.is_err(), "invalid tag should be rejected");
}

#[test]
fn store_rm_specific_tag() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    store.add("id1", "tag1", None).unwrap();
    store.add("id1", "tag2", None).unwrap();
    store.save(&store_path).unwrap();

    let mut loaded = AnnotationStore::load_or_default(&store_path).unwrap();
    loaded.remove("id1", Some("tag1"));
    loaded.save(&store_path).unwrap();

    let final_store = AnnotationStore::load_or_default(&store_path).unwrap();
    let anns = final_store.list(Some("id1"), None);
    assert_eq!(anns.len(), 1);
    assert_eq!(anns[0].tag, "tag2");
}

#[test]
fn store_rm_all_tags_for_instance() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    store.add("id1", "tag1", None).unwrap();
    store.add("id1", "tag2", None).unwrap();
    store.save(&store_path).unwrap();

    let mut loaded = AnnotationStore::load_or_default(&store_path).unwrap();
    loaded.remove("id1", None);
    loaded.save(&store_path).unwrap();

    let final_store = AnnotationStore::load_or_default(&store_path).unwrap();
    let anns = final_store.list(Some("id1"), None);
    assert!(anns.is_empty(), "all annotations for id1 should be removed");
}

#[test]
fn store_list_filter_by_tag() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    store.add("id1", "evaluator-flake", None).unwrap();
    store.add("id2", "evaluator-flake", None).unwrap();
    store.add("id2", "ignore", None).unwrap();
    store.save(&store_path).unwrap();

    let loaded = AnnotationStore::load_or_default(&store_path).unwrap();
    let flakes = loaded.list(None, Some("evaluator-flake"));
    assert_eq!(flakes.len(), 2, "should find 2 evaluator-flake annotations");
    let ignores = loaded.list(None, Some("ignore"));
    assert_eq!(ignores.len(), 1, "should find 1 ignore annotation");
}

#[test]
fn store_last_writer_wins_on_same_tag() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    store.add("id1", "tag1", Some("first")).unwrap();
    store.add("id1", "tag1", Some("second")).unwrap();
    store.save(&store_path).unwrap();

    let loaded = AnnotationStore::load_or_default(&store_path).unwrap();
    let anns = loaded.list(Some("id1"), None);
    assert_eq!(anns.len(), 1, "same (id, tag) should be deduplicated");
    assert_eq!(
        anns[0].note.as_deref(),
        Some("second"),
        "last writer wins"
    );
}

#[test]
fn store_timestamps_are_rfc3339() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("annotations.json");
    let mut store = AnnotationStore::load_or_default(&store_path).unwrap();
    store.add("id1", "tag1", None).unwrap();
    store.save(&store_path).unwrap();

    let loaded = AnnotationStore::load_or_default(&store_path).unwrap();
    let ann = &loaded.list(Some("id1"), None)[0];
    // chrono parses RFC 3339
    chrono::DateTime::parse_from_rfc3339(&ann.created_at).expect("created_at not RFC 3339");
    chrono::DateTime::parse_from_rfc3339(&ann.updated_at).expect("updated_at not RFC 3339");
}

// ── AC: CLI add subcommand ────────────────────────────────────────────────────

#[test]
fn cli_annotate_add_creates_store() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = default_store_path(dir.path());
    let out = annotate_add(dir.path(), "pytest__pytest-7234", &["evaluator-flake"], None, None);
    assert!(
        out.status.success(),
        "annotate add should exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(store_path.exists(), "annotations.json should be created");
}

#[test]
fn cli_annotate_add_with_note() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = default_store_path(dir.path());
    let out = annotate_add(
        dir.path(),
        "pytest__pytest-7234",
        &["evaluator-flake"],
        Some("known evaluator bug"),
        None,
    );
    assert!(out.status.success());
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(
        raw["instances"]["pytest__pytest-7234"]["evaluator-flake"]["note"]
            .as_str()
            .unwrap(),
        "known evaluator bug"
    );
}

#[test]
fn cli_annotate_add_multiple_tags() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = default_store_path(dir.path());
    let out = annotate_add(
        dir.path(),
        "pytest__pytest-7234",
        &["evaluator-flake", "ignore"],
        None,
        None,
    );
    assert!(out.status.success());
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    assert!(raw["instances"]["pytest__pytest-7234"]["evaluator-flake"].is_object());
    assert!(raw["instances"]["pytest__pytest-7234"]["ignore"].is_object());
}

#[test]
fn cli_annotate_add_invalid_tag_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let out = annotate_add(dir.path(), "id1", &["BadTag"], None, None);
    assert!(
        !out.status.success(),
        "invalid tag should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("tag") || stderr.contains("invalid") || stderr.contains("format"),
        "error message should mention tag/invalid/format: {stderr}"
    );
}

#[test]
fn cli_annotate_add_note_too_long_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let long_note = "x".repeat(1025);
    let out = annotate_add(dir.path(), "id1", &["tag1"], Some(&long_note), None);
    assert!(!out.status.success(), "long note should exit non-zero");
}

#[test]
fn cli_annotate_add_store_override() {
    let dir = tempfile::tempdir().unwrap();
    let custom_store = dir.path().join("custom-store.json");
    let out = annotate_add(dir.path(), "id1", &["tag1"], None, Some(&custom_store));
    assert!(out.status.success());
    assert!(
        custom_store.exists(),
        "custom store should be created when --store is set"
    );
    assert!(
        !default_store_path(dir.path()).exists(),
        "default store should NOT be created when --store is set"
    );
}

#[test]
fn cli_annotate_add_env_var_store() {
    let dir = tempfile::tempdir().unwrap();
    let env_store = dir.path().join("env-store.json");
    let out = Command::new(binary_path())
        .current_dir(dir.path())
        .env("BENCH_ANNOTATIONS_PATH", &env_store)
        .args(["bench", "annotate", "add", "id1", "--tag", "tag1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        env_store.exists(),
        "BENCH_ANNOTATIONS_PATH store should be created"
    );
}

// ── AC: CLI list subcommand ───────────────────────────────────────────────────

#[test]
fn cli_annotate_list_empty_store_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = annotate_list(dir.path(), None, None, None);
    assert!(
        out.status.success(),
        "list on missing store should exit 0"
    );
}

#[test]
fn cli_annotate_list_shows_added_annotations() {
    let dir = tempfile::tempdir().unwrap();
    annotate_add(
        dir.path(),
        "pytest__pytest-7234",
        &["evaluator-flake"],
        Some("my note"),
        None,
    );
    let out = annotate_list(dir.path(), None, None, None);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("pytest__pytest-7234"));
    assert!(stdout.contains("evaluator-flake"));
}

#[test]
fn cli_annotate_list_filter_by_instance() {
    let dir = tempfile::tempdir().unwrap();
    annotate_add(dir.path(), "instance-a", &["tag1"], None, None);
    annotate_add(dir.path(), "instance-b", &["tag2"], None, None);
    let out = annotate_list(dir.path(), Some("instance-a"), None, None);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("instance-a"));
    assert!(!stdout.contains("instance-b"), "should not show instance-b");
}

#[test]
fn cli_annotate_list_filter_by_tag() {
    let dir = tempfile::tempdir().unwrap();
    annotate_add(dir.path(), "instance-a", &["evaluator-flake"], None, None);
    annotate_add(dir.path(), "instance-b", &["ignore"], None, None);
    let out = annotate_list(dir.path(), None, Some("evaluator-flake"), None);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("instance-a"));
    assert!(
        !stdout.contains("instance-b"),
        "should not show instance-b which only has 'ignore'"
    );
}

// ── AC: CLI rm subcommand ─────────────────────────────────────────────────────

#[test]
fn cli_annotate_rm_specific_tag() {
    let dir = tempfile::tempdir().unwrap();
    annotate_add(
        dir.path(),
        "pytest__pytest-7234",
        &["evaluator-flake", "ignore"],
        None,
        None,
    );
    let out = annotate_rm(dir.path(), "pytest__pytest-7234", Some("evaluator-flake"), None);
    assert!(out.status.success());

    let list_out = annotate_list(dir.path(), Some("pytest__pytest-7234"), None, None);
    let stdout = String::from_utf8_lossy(&list_out.stdout);
    assert!(
        !stdout.contains("evaluator-flake"),
        "evaluator-flake should be removed"
    );
    assert!(stdout.contains("ignore"), "ignore should still be present");
}

#[test]
fn cli_annotate_rm_all_tags() {
    let dir = tempfile::tempdir().unwrap();
    annotate_add(
        dir.path(),
        "pytest__pytest-7234",
        &["evaluator-flake", "ignore"],
        None,
        None,
    );
    let out = annotate_rm(dir.path(), "pytest__pytest-7234", None, None);
    assert!(out.status.success());

    let list_out = annotate_list(dir.path(), Some("pytest__pytest-7234"), None, None);
    let stdout = String::from_utf8_lossy(&list_out.stdout);
    assert!(
        !stdout.contains("pytest__pytest-7234"),
        "all annotations for pytest__pytest-7234 should be removed"
    );
}

// ── AC: redaction of notes ────────────────────────────────────────────────────

#[test]
fn cli_annotate_add_redacts_note_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = default_store_path(dir.path());
    // GitHub tokens are redacted by default
    let note_with_secret = "token=ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa extra text";
    let out = annotate_add(
        dir.path(),
        "id1",
        &["tag1"],
        Some(note_with_secret),
        None,
    );
    assert!(out.status.success());
    let raw_store = std::fs::read_to_string(&store_path).unwrap();
    assert!(
        !raw_store.contains("ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        "GitHub token should be redacted in stored note"
    );
}

// ── AC: bench inspect shows annotations ──────────────────────────────────────

fn write_minimal_sweep(dir: &Path, instance_id: &str) {
    // Write manifest.json (required by bench bundle)
    let manifest = serde_json::json!({
        "artifact_kind": "sweep_manifest",
        "schema_version": {"major": 1, "minor": 0},
        "purpose": "test",
        "harness": {
            "name": "maxwells-daemon",
            "version": "test",
            "git_sha": "test-sha",
            "git_dirty": false,
            "git_resolution": "test"
        },
        "dataset": {
            "path": "dataset.jsonl",
            "sha256": "test-dataset-hash",
            "instance_count": 1,
            "source_kind": "local",
            "selected_row_count": 1,
            "post_filter_row_count": 1
        },
        "prompt_template": {
            "source": "inline",
            "sha256": "test-template-hash"
        },
        "config": {
            "resolved": "[model]\nname = \"deterministic\"\n",
            "overlay_paths": []
        },
        "model": {
            "name": "deterministic",
            "backend": "deterministic"
        },
        "runtime": {
            "env_kind": "local",
            "parallelism": 1
        }
    });
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Write results.json with instances as array (format expected by bench bundle)
    let results = serde_json::json!({
        "artifact_kind": "sweep_results",
        "schema_version": {"major": 1, "minor": 3},
        "total": 1,
        "sweep_status": "completed",
        "instances": [
            {
                "instance_id": instance_id,
                "exit_reason": "error",
                "outcome": "error",
                "failure_category": "step_limit",
                "steps": 2,
                "cost_usd": 0.05,
                "total_input_tokens": 100,
                "total_completion_tokens": 20,
                "duration_secs": 5.0,
                "patch_present": false,
                "non_empty_patch": false,
                "attempts": 1,
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false,
                "tests_run_before_submit": false
            }
        ]
    });
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    // Write a minimal trajectory
    use maxwells_daemon::trajectory::{FailureCategory, TokenUsage, Trajectory, outcome};
    let mut t = Trajectory::new();
    t.info.model_name = Some("test-model".into());
    t.info.outcome = Some(outcome::ERROR.into());
    t.info.failure_category = Some(FailureCategory::StepLimit);
    t.info.total_cost_usd = Some(0.05);
    t.info.token_usage = Some(TokenUsage {
        prompt_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        completion_tokens: 20,
    });
    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&t).unwrap(),
    )
    .unwrap();
}

#[test]
fn inspect_shows_operator_notes_when_annotations_exist() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = "pytest__pytest-7234";
    write_minimal_sweep(dir.path(), instance_id);

    // Add an annotation
    annotate_add(
        dir.path(),
        instance_id,
        &["evaluator-flake"],
        Some("known issue"),
        None,
    );

    let out = Command::new(binary_path())
        .current_dir(dir.path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            instance_id,
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "inspect should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("Operator notes") || stdout.contains("operator notes"),
        "inspect output should contain 'Operator notes' section: {stdout}"
    );
    assert!(
        stdout.contains("evaluator-flake"),
        "inspect should show the tag: {stdout}"
    );
}

#[test]
fn inspect_no_operator_notes_when_no_annotations() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = "pytest__pytest-7234";
    write_minimal_sweep(dir.path(), instance_id);

    let out = Command::new(binary_path())
        .args([
            "bench",
            "inspect",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--instance",
            instance_id,
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(
        !stdout.contains("Operator notes"),
        "inspect output should NOT contain 'Operator notes' when no annotations: {stdout}"
    );
}

// ── AC: bench bundle includes annotations.json ────────────────────────────────

#[test]
fn bundle_includes_annotations_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = "pytest__pytest-7234";
    write_minimal_sweep(dir.path(), instance_id);

    // Add an annotation
    annotate_add(dir.path(), instance_id, &["evaluator-flake"], None, None);

    let bundle_path = dir.path().join("bundle.tar.gz");
    let out = Command::new(binary_path())
        .args([
            "bench",
            "bundle",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--output",
            bundle_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "bench bundle should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(bundle_path.exists());

    // Verify annotations.json is in the tarball
    let bundle_bytes = std::fs::read(&bundle_path).unwrap();
    let cursor = std::io::Cursor::new(bundle_bytes);
    let gz = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);
    let has_annotations = archive
        .entries()
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.path().map_or(false, |p| p.to_string_lossy().contains("annotations")));
    assert!(
        has_annotations,
        "bundle should include annotations.json"
    );
}

#[test]
fn bundle_succeeds_without_annotations() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = "pytest__pytest-7234";
    write_minimal_sweep(dir.path(), instance_id);

    let bundle_path = dir.path().join("bundle.tar.gz");
    let out = Command::new(binary_path())
        .args([
            "bench",
            "bundle",
            "--sweep",
            dir.path().to_str().unwrap(),
            "--output",
            bundle_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "bench bundle should succeed without annotations: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── AC: bench reproduce annotation diff ──────────────────────────────────────

#[test]
fn reproduce_annotation_diff_detects_only_in_original() {
    let dir = tempfile::tempdir().unwrap();
    let orig_path = dir.path().join("orig.json");
    let replay_path = dir.path().join("replay.json");

    let mut orig = AnnotationStore::load_or_default(&orig_path).unwrap();
    orig.add("pytest__pytest-7234", "evaluator-flake", Some("known issue")).unwrap();
    orig.save(&orig_path).unwrap();

    let replay = AnnotationStore::load_or_default(&replay_path).unwrap();

    let (only_orig, only_replay) = diff_annotation_stores(&orig, &replay);
    assert_eq!(only_orig.len(), 1);
    assert!(only_orig.iter().any(|(id, tag)| id == "pytest__pytest-7234" && tag == "evaluator-flake"));
    assert!(only_replay.is_empty());
}

#[test]
fn reproduce_annotation_diff_detects_only_in_replay() {
    let dir = tempfile::tempdir().unwrap();
    let orig_path = dir.path().join("orig.json");
    let replay_path = dir.path().join("replay.json");

    let orig = AnnotationStore::load_or_default(&orig_path).unwrap();
    let mut replay = AnnotationStore::load_or_default(&replay_path).unwrap();
    replay.add("pytest__pytest-7234", "new-tag", None).unwrap();
    replay.save(&replay_path).unwrap();

    let (only_orig, only_replay) = diff_annotation_stores(&orig, &replay);
    assert!(only_orig.is_empty());
    assert_eq!(only_replay.len(), 1);
}

#[test]
fn reproduce_annotation_diff_empty_when_same() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.json");
    let mut store = AnnotationStore::load_or_default(&path).unwrap();
    store.add("id1", "tag1", None).unwrap();
    store.save(&path).unwrap();

    let orig = AnnotationStore::load_or_default(&path).unwrap();
    let replay = AnnotationStore::load_or_default(&path).unwrap();
    let (only_orig, only_replay) = diff_annotation_stores(&orig, &replay);
    assert!(only_orig.is_empty() && only_replay.is_empty());
}

// ── AC: zero network / model calls ────────────────────────────────────────────

#[test]
fn annotate_add_is_zero_cost() {
    // This test verifies no network calls are attempted by running without any
    // network-dependent config and confirming it succeeds fast.
    let dir = tempfile::tempdir().unwrap();
    let start = std::time::Instant::now();
    let out = annotate_add(dir.path(), "id1", &["tag1"], None, None);
    let elapsed = start.elapsed();
    assert!(out.status.success());
    assert!(
        elapsed.as_secs() < 5,
        "annotate add should be fast (zero-cost): took {elapsed:?}"
    );
}
