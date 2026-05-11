//! TDD tests for replay prompt-drift detection (issue #155).
//!
//! RED phase: written before implementation. Tests cover all four acceptance
//! scenarios from the issue:
//!   (a) identical replay → exit 0, no drift report
//!   (b) one tampered fingerprint → exit code ReplayPromptDrift, drift JSON valid
//!   (c) legacy trajectory (no fingerprints) → refused without flag, accepted with flag
//!   (d) --report-only with two divergences → exit 0 and both steps listed

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use rust_swe_agent::{
    config::Config,
    exit_code::ExitCode,
    fingerprint::{InputFingerprint, canonical_json, compute_input_fingerprint},
    model::{Message, MessageExtra},
    run::replay::{ReplayArgs, run as replay_run},
    trajectory::{MessageRecord, Trajectory, TrajectoryInfo},
};
use tempfile::tempdir;

// ── helpers ────────────────────────────────────────────────────────────────

/// Build a minimal 2-step trajectory without fingerprints (simulates legacy).
fn make_legacy_trajectory(dir: &Path) -> std::path::PathBuf {
    let traj = Trajectory {
        trajectory_format: "mini-swe-agent-1.1".into(),
        info: TrajectoryInfo {
            task: Some("dummy task".into()),
            ..Default::default()
        },
        messages: vec![
            MessageRecord {
                role: "user".into(),
                content: "dummy task".into(),
                extra: MessageExtra::default(),
            },
            MessageRecord {
                role: "assistant".into(),
                content: "```bash\necho hi\n```".into(),
                extra: MessageExtra::default(),
            },
            MessageRecord {
                role: "user".into(),
                content: "hi".into(),
                extra: MessageExtra::default(),
            },
            MessageRecord {
                role: "assistant".into(),
                content: "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nhi\n```".into(),
                extra: MessageExtra::default(),
            },
        ],
    };
    let path = dir.join("legacy.traj.json");
    traj.save_pretty(&path).unwrap();
    path
}

/// Run pass-1 replay (allow_unfingerprinted=true) to produce a fingerprinted
/// trajectory, then return the path to that output trajectory.
async fn record_fingerprinted_trajectory(dir: &Path) -> std::path::PathBuf {
    let legacy = make_legacy_trajectory(dir);
    let args = ReplayArgs {
        trajectory_path: legacy,
        config: Config::defaults().unwrap(),
        output_dir: dir.to_path_buf(),
        trajectory_name: Some("recorded".into()),
        allow_unfingerprinted: true,
        report_only: false,
        drift_cap_bytes: 8192,
    };
    replay_run(args).await.expect("pass-1 recording should succeed");
    dir.join("recorded.traj.json")
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[test]
fn compute_fingerprint_is_stable() {
    let msgs = vec![Message::user("hello"), Message::assistant("world")];
    let fp1 = compute_input_fingerprint(&msgs);
    let fp2 = compute_input_fingerprint(&msgs);
    assert_eq!(fp1.hex, fp2.hex);
    assert_eq!(fp1.canonical_size, fp2.canonical_size);
}

#[test]
fn different_messages_produce_different_fingerprints() {
    let fp_a = compute_input_fingerprint(&[Message::user("hello")]);
    let fp_b = compute_input_fingerprint(&[Message::user("world")]);
    assert_ne!(fp_a.hex, fp_b.hex);
}

#[test]
fn fingerprint_hex_is_16_chars() {
    let fp = compute_input_fingerprint(&[Message::user("x")]);
    assert_eq!(fp.hex.len(), 16, "fingerprint should be 16 hex chars (8 bytes)");
    assert!(fp.hex.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn canonical_json_keys_are_sorted() {
    let msgs = vec![Message::user("hello")];
    let json = canonical_json(&msgs);
    // "content" should appear before "role" (alphabetical order)
    let content_pos = json.find("\"content\"").unwrap();
    let role_pos = json.find("\"role\"").unwrap();
    assert!(content_pos < role_pos, "canonical JSON must have sorted keys");
}

#[test]
fn canonical_json_excludes_extra_metadata() {
    let mut msg = Message::user("hello");
    msg.extra.cost = Some(1.23);
    msg.extra.timestamp = Some("2026-01-01".into());
    let json = canonical_json(&[msg]);
    assert!(!json.contains("cost"), "canonical JSON must not include cost");
    assert!(
        !json.contains("timestamp"),
        "canonical JSON must not include timestamp"
    );
}

#[test]
fn replay_prompt_drift_exit_code_is_9() {
    assert_eq!(ExitCode::ReplayPromptDrift.as_i32(), 9);
    assert_eq!(ExitCode::ReplayPromptDrift.outcome_class(), "replay_prompt_drift");
}

#[test]
fn replay_response_exhausted_exit_code_is_10() {
    assert_eq!(ExitCode::ReplayResponseExhausted.as_i32(), 10);
    assert_eq!(
        ExitCode::ReplayResponseExhausted.outcome_class(),
        "replay_response_exhausted"
    );
}

// ── integration tests ──────────────────────────────────────────────────────

/// (a) Identical replay → exit 0, no drift file.
#[tokio::test]
async fn identical_replay_exits_zero_no_drift_file() {
    let dir = tempdir().unwrap();

    // Pass 1: produce a trajectory with fingerprints.
    let fp_traj = record_fingerprinted_trajectory(dir.path()).await;

    // Pass 2: replay the fingerprinted trajectory — fingerprints must match.
    let args = ReplayArgs {
        trajectory_path: fp_traj,
        config: Config::defaults().unwrap(),
        output_dir: dir.path().to_path_buf(),
        trajectory_name: Some("pass2".into()),
        allow_unfingerprinted: false,
        report_only: false,
        drift_cap_bytes: 8192,
    };
    let result = replay_run(args).await;
    assert!(result.is_ok(), "identical replay should succeed: {result:?}");

    // No drift report should be written.
    assert!(
        !dir.path().join("replay-drift.json").exists(),
        "no drift file expected on clean replay"
    );
}

/// (b) One tampered fingerprint → exit code ReplayPromptDrift (9), drift JSON valid.
#[tokio::test]
async fn prompt_drift_exits_drift_code_at_step_zero() {
    let dir = tempdir().unwrap();

    // Pass 1: produce fingerprinted trajectory.
    let fp_traj = record_fingerprinted_trajectory(dir.path()).await;

    // Tamper with the fingerprint of the first assistant message.
    let content = std::fs::read_to_string(&fp_traj).unwrap();
    let mut traj: Trajectory = serde_json::from_str(&content).unwrap();
    let tampered = "deadbeef00000000";
    for msg in &mut traj.messages {
        if msg.role == "assistant" {
            if let Some(mc) = msg.extra.other.get_mut("model_call") {
                mc["input_fingerprint"] = serde_json::json!(tampered);
            }
            break; // only first assistant message
        }
    }
    traj.save_pretty(&fp_traj).unwrap();

    // Pass 2: replay should detect drift.
    let args = ReplayArgs {
        trajectory_path: fp_traj,
        config: Config::defaults().unwrap(),
        output_dir: dir.path().to_path_buf(),
        trajectory_name: Some("drifted".into()),
        allow_unfingerprinted: false,
        report_only: false,
        drift_cap_bytes: 8192,
    };
    let result = replay_run(args).await;
    assert!(result.is_err(), "should fail on drift");
    assert_eq!(
        ExitCode::from_error(&result.unwrap_err()),
        ExitCode::ReplayPromptDrift
    );

    // Drift report must be written with correct content.
    let drift_path = dir.path().join("replay-drift.json");
    assert!(drift_path.exists(), "drift report should be written");
    let drift: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&drift_path).unwrap()).unwrap();
    let steps = drift["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "exactly one drift step");
    assert_eq!(steps[0]["step_index"].as_u64().unwrap(), 0);
    assert_eq!(
        steps[0]["recorded_fingerprint"].as_str().unwrap(),
        tampered
    );
    assert!(!steps[0]["actual_fingerprint"].as_str().unwrap().is_empty());
    // unified_diff must be present (may be empty string if no stored canonical)
    assert!(
        steps[0]["unified_diff"].as_str().is_some(),
        "unified_diff field must be present in drift step"
    );
    assert!(
        steps[0]["diff_truncated"].as_bool().is_some(),
        "diff_truncated field must be present"
    );
}

/// (c-1) Legacy trajectory without fingerprints is refused without --allow-unfingerprinted.
#[tokio::test]
async fn legacy_trajectory_refused_without_flag() {
    let dir = tempdir().unwrap();
    let traj = make_legacy_trajectory(dir.path());

    let args = ReplayArgs {
        trajectory_path: traj,
        config: Config::defaults().unwrap(),
        output_dir: dir.path().to_path_buf(),
        trajectory_name: Some("no-flag".into()),
        allow_unfingerprinted: false, // must refuse
        report_only: false,
        drift_cap_bytes: 8192,
    };
    let result = replay_run(args).await;
    assert!(
        result.is_err(),
        "should fail without --allow-unfingerprinted"
    );
    // No drift report for this case (it's a configuration error, not drift)
}

/// (c-2) Legacy trajectory accepted with --allow-unfingerprinted, exit 0.
#[tokio::test]
async fn legacy_trajectory_accepted_with_flag() {
    let dir = tempdir().unwrap();
    let traj = make_legacy_trajectory(dir.path());

    let args = ReplayArgs {
        trajectory_path: traj,
        config: Config::defaults().unwrap(),
        output_dir: dir.path().to_path_buf(),
        trajectory_name: Some("with-flag".into()),
        allow_unfingerprinted: true, // should succeed
        report_only: false,
        drift_cap_bytes: 8192,
    };
    let result = replay_run(args).await;
    assert!(
        result.is_ok(),
        "--allow-unfingerprinted should succeed: {result:?}"
    );
    assert!(
        !dir.path().join("replay-drift.json").exists(),
        "no drift file for unfingerprinted run"
    );
}

/// (d) --report-only with two divergences exits 0 and lists both steps.
#[tokio::test]
async fn report_only_with_two_divergences_exits_zero() {
    let dir = tempdir().unwrap();

    // Pass 1: produce fingerprinted trajectory.
    let fp_traj = record_fingerprinted_trajectory(dir.path()).await;

    // Tamper with BOTH assistant fingerprints.
    let content = std::fs::read_to_string(&fp_traj).unwrap();
    let mut traj: Trajectory = serde_json::from_str(&content).unwrap();
    let mut idx = 0usize;
    for msg in &mut traj.messages {
        if msg.role == "assistant" {
            if let Some(mc) = msg.extra.other.get_mut("model_call") {
                mc["input_fingerprint"] = serde_json::json!(format!("deadbeef0000000{idx}"));
            }
            idx += 1;
        }
    }
    traj.save_pretty(&fp_traj).unwrap();

    // Pass 2: --report-only must exit 0 despite two divergences.
    let args = ReplayArgs {
        trajectory_path: fp_traj,
        config: Config::defaults().unwrap(),
        output_dir: dir.path().to_path_buf(),
        trajectory_name: Some("report-run".into()),
        allow_unfingerprinted: false,
        report_only: true, // key flag
        drift_cap_bytes: 8192,
    };
    let result = replay_run(args).await;
    assert!(result.is_ok(), "--report-only should exit 0: {result:?}");

    // Both divergent steps must appear in the report.
    let drift_path = dir.path().join("replay-drift.json");
    assert!(drift_path.exists(), "drift report must be written in --report-only mode");
    let drift: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&drift_path).unwrap()).unwrap();
    let steps = drift["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2, "both divergent steps must be listed");
}

// ── InputFingerprint is a real type ────────────────────────────────────────

#[test]
fn input_fingerprint_fields_are_accessible() {
    let fp: InputFingerprint = compute_input_fingerprint(&[Message::user("hi")]);
    assert!(!fp.hex.is_empty());
    assert!(fp.canonical_size > 0);
}
