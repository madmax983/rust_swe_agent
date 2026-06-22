//! Integration tests for `max agent annotate` (issue #539).
//!
//! Tests are organized by Acceptance Criteria from the issue.
//! Each test copies the fixture to an isolated tempdir to avoid concurrency races.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

mod support;
use support::binary_path;

const FIXTURE: &str = "tests/fixtures/agent_annotate/sample.traj.json";

// ── helpers ───────────────────────────────────────────────────────────────────

/// Copy the fixture trajectory to a fresh temp dir and return the temp dir + path.
fn isolated_traj() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let traj_path = dir.path().join("sample.traj.json");
    std::fs::copy(FIXTURE, &traj_path).expect("copy fixture");
    (dir, traj_path)
}

fn annotation_path_for(traj: &Path) -> PathBuf {
    let name = traj.file_name().unwrap().to_str().unwrap();
    let ann_name = name.replace(".traj.json", ".annotation.json");
    traj.with_file_name(ann_name)
}

fn run_annotate(traj: &Path, extra_args: &[&str]) -> std::process::Output {
    Command::new(binary_path())
        .args(["agent", "annotate"])
        .arg(traj)
        .args(extra_args)
        .output()
        .expect("failed to run binary")
}

fn assert_success(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── AC1: sidecar written, trajectory untouched ────────────────────────────────

#[test]
fn ac1_sidecar_written_trajectory_byte_identical() {
    let (_dir, traj) = isolated_traj();
    let before = std::fs::read(&traj).expect("read traj before");

    let out = run_annotate(&traj, &["--verdict", "correct"]);
    assert_success(&out);

    let after = std::fs::read(&traj).expect("read traj after");
    assert_eq!(before, after, "trajectory was mutated");

    let ann_path = annotation_path_for(&traj);
    assert!(
        ann_path.exists(),
        "annotation sidecar not written: {}",
        ann_path.display()
    );
}

// ── AC2: all flag values captured ─────────────────────────────────────────────

#[test]
fn ac2_verdict_failure_category_note_step_notes_captured() {
    let (_dir, traj) = isolated_traj();

    let out = run_annotate(
        &traj,
        &[
            "--verdict",
            "incorrect",
            "--failure-category",
            "patch_apply_invalid",
            "--note",
            "agent edited the wrong file",
            "--step-note",
            "0=first step went wrong",
            "--step-note",
            "2=last step also bad",
        ],
    );
    assert_success(&out);

    let ann_path = annotation_path_for(&traj);
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ann_path).unwrap())
            .expect("annotation is valid JSON");

    assert_eq!(json["verdict"], "incorrect");
    assert_eq!(json["failure_category"], "patch_apply_invalid");
    assert_eq!(json["note"], "agent edited the wrong file");

    let step_notes = json["step_notes"].as_array().expect("step_notes is array");
    assert_eq!(step_notes.len(), 2);
    assert_eq!(step_notes[0]["step"], 0);
    assert_eq!(step_notes[0]["note"], "first step went wrong");
    assert_eq!(step_notes[1]["step"], 2);
    assert_eq!(step_notes[1]["note"], "last step also bad");
}

// ── AC3: schema versioning and fingerprint ────────────────────────────────────

#[test]
fn ac3_schema_version_artifact_kind_instance_id_fingerprint_present() {
    let (_dir, traj) = isolated_traj();

    let out = run_annotate(&traj, &["--verdict", "partial"]);
    assert_success(&out);

    let ann_path = annotation_path_for(&traj);
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ann_path).unwrap()).unwrap();

    assert_eq!(
        json["artifact_kind"], "trajectory_annotation",
        "artifact_kind wrong"
    );
    let sv = &json["schema_version"];
    assert!(
        sv["major"].as_u64().unwrap() >= 1,
        "schema_version.major missing"
    );
    assert!(sv["minor"].is_number(), "schema_version.minor missing");

    assert!(
        json["instance_id"].is_string() && !json["instance_id"].as_str().unwrap().is_empty(),
        "instance_id missing or empty"
    );

    let fp = json["trajectory_sha256"]
        .as_str()
        .expect("trajectory_sha256 missing");
    assert_eq!(fp.len(), 64, "sha256 should be 64 hex chars, got {fp}");

    // Verify fingerprint matches actual file.
    let traj_bytes = std::fs::read(&traj).unwrap();
    let digest = Sha256::digest(&traj_bytes);
    let mut expected_hex = String::with_capacity(64);
    for b in &digest {
        use std::fmt::Write as _;
        let _ = write!(expected_hex, "{b:02x}");
    }
    assert_eq!(fp, expected_hex, "trajectory_sha256 does not match file digest");
}

// ── AC4: validation exits non-zero with clear messages ───────────────────────

#[test]
fn ac4_step_note_out_of_range_exits_nonzero() {
    let (_dir, traj) = isolated_traj();

    // trajectory has 3 messages (indices 0,1,2); index 99 is out of range
    let out = run_annotate(&traj, &["--verdict", "correct", "--step-note", "99=bad"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for out-of-range step-note"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("99") || stderr.contains("range") || stderr.contains("out"),
        "error message should mention the index or range: {stderr}"
    );
}

#[test]
fn ac4_bad_verdict_exits_nonzero() {
    let (_dir, traj) = isolated_traj();

    let out = run_annotate(&traj, &["--verdict", "totally-wrong-value"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for bad verdict"
    );
}

#[test]
fn ac4_non_trajectory_file_exits_nonzero() {
    let out = Command::new(binary_path())
        .args(["agent", "annotate", "Cargo.toml", "--verdict", "correct"])
        .output()
        .expect("failed to run binary");
    assert!(
        !out.status.success(),
        "expected non-zero exit for non-trajectory file"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parse")
            || stderr.contains("trajectory")
            || stderr.contains("Cargo.toml"),
        "error message unclear: {stderr}"
    );
}

// ── AC5: re-run behavior with --force ─────────────────────────────────────────

#[test]
fn ac5_second_run_without_force_exits_nonzero() {
    let (_dir, traj) = isolated_traj();

    let out1 = run_annotate(&traj, &["--verdict", "correct"]);
    assert_success(&out1);

    let out2 = run_annotate(&traj, &["--verdict", "incorrect"]);
    assert!(
        !out2.status.success(),
        "second run without --force should fail"
    );
    let stderr = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr.contains("force") || stderr.contains("already"),
        "error should mention --force: {stderr}"
    );
}

#[test]
fn ac5_second_run_with_force_overwrites_deterministically() {
    let (_dir, traj) = isolated_traj();

    let out1 = run_annotate(&traj, &["--verdict", "correct"]);
    assert_success(&out1);

    let out2 = run_annotate(&traj, &["--verdict", "incorrect", "--force"]);
    assert_success(&out2);

    let ann_path = annotation_path_for(&traj);
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ann_path).unwrap()).unwrap();
    assert_eq!(json["verdict"], "incorrect", "verdict should be updated");
}

// ── AC6: --show read path ─────────────────────────────────────────────────────

#[test]
fn ac6_show_prints_existing_annotation_as_text() {
    let (_dir, traj) = isolated_traj();

    let out_write = run_annotate(&traj, &["--verdict", "unsure", "--note", "needs review"]);
    assert_success(&out_write);

    let out_show = Command::new(binary_path())
        .args(["agent", "annotate"])
        .arg(&traj)
        .arg("--show")
        .output()
        .expect("failed to run binary");
    assert!(
        out_show.status.success(),
        "show should exit 0\nstderr: {}",
        String::from_utf8_lossy(&out_show.stderr)
    );

    let stdout = String::from_utf8_lossy(&out_show.stdout);
    assert!(
        stdout.contains("unsure"),
        "--show text output should contain verdict: {stdout}"
    );
}

#[test]
fn ac6_show_format_json_emits_valid_json() {
    let (_dir, traj) = isolated_traj();

    let out_write = run_annotate(&traj, &["--verdict", "partial"]);
    assert_success(&out_write);

    let out_show = Command::new(binary_path())
        .args(["agent", "annotate"])
        .arg(&traj)
        .args(["--show", "--format", "json"])
        .output()
        .expect("failed to run binary");
    assert!(out_show.status.success());

    let stdout = String::from_utf8_lossy(&out_show.stdout);
    let _: serde_json::Value =
        serde_json::from_str(&stdout).expect("--show --format json should be valid JSON");
}

#[test]
fn ac6_show_no_annotation_exits_nonzero() {
    let (_dir, traj) = isolated_traj();

    let out = Command::new(binary_path())
        .args(["agent", "annotate"])
        .arg(&traj)
        .arg("--show")
        .output()
        .expect("failed to run binary");
    assert!(
        !out.status.success(),
        "show without annotation should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no annotation")
            || stderr.contains("not found")
            || stderr.contains("does not exist"),
        "error should say annotation is missing: {stderr}"
    );
}

// ── AC7: redaction applied to free-text fields ────────────────────────────────

#[test]
fn ac7_note_with_secret_is_redacted_in_sidecar() {
    let (_dir, traj) = isolated_traj();

    // Use a bearer-token pattern the default redactor catches.
    let secret_note = "token=ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA and some note";

    let out = run_annotate(&traj, &["--verdict", "incorrect", "--note", secret_note]);
    assert_success(&out);

    let ann_path = annotation_path_for(&traj);
    let raw = std::fs::read_to_string(&ann_path).unwrap();
    assert!(
        !raw.contains("ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        "raw secret should not appear in sidecar JSON: {raw}"
    );
    assert!(
        raw.contains("REDACTED") || raw.contains("note"),
        "redacted note field should be present"
    );
}

#[test]
fn ac7_failure_category_with_secret_is_redacted() {
    let (_dir, traj) = isolated_traj();

    let secret_cat = "leaked-ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    let out = run_annotate(
        &traj,
        &["--verdict", "incorrect", "--failure-category", secret_cat],
    );
    assert_success(&out);

    let ann_path = annotation_path_for(&traj);
    let raw = std::fs::read_to_string(&ann_path).unwrap();
    assert!(
        !raw.contains("ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        "raw secret should not appear in failure_category: {raw}"
    );
}
