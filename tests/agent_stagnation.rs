//! Agent stagnation detection: TDD tests for issue #157.
//!
//! Tests drive the full default-agent loop using DeterministicModel and
//! LocalEnvironment. All tests must be deterministic and reproduce reliably.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use maxwells_daemon::agent::default::DefaultAgentBuilder;
use maxwells_daemon::trajectory::FailureCategory;
use maxwells_daemon::{
    Agent, Config, DeterministicModel, Environment, ExitReason, LocalEnvironment,
};

// ── helpers ──────────────────────────────────────────────────────────────────

fn build_agent(
    cfg: Config,
    responses: Vec<String>,
) -> maxwells_daemon::agent::default::DefaultAgent {
    let model = Arc::new(DeterministicModel::new(responses));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "stagnation test task".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build()
    .unwrap()
}

fn ls_responses(n: usize) -> Vec<String> {
    (0..n).map(|_| "```bash\nls\n```".into()).collect()
}

// ── test (a) ──────────────────────────────────────────────────────────────────
// Four identical `ls` actions in eight steps trips at step 4 and writes
// `agent_stagnation` with the right hash and indices.

#[tokio::test]
async fn stagnation_trips_at_step_4_for_repeated_ls() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 40;

    let mut agent = build_agent(cfg, ls_responses(40));
    let exit = agent.run().await.unwrap();

    assert!(
        matches!(exit, ExitReason::AgentStagnation { .. }),
        "expected AgentStagnation, got: {exit:?}"
    );
    assert_eq!(
        agent.trajectory.info.failure_category,
        Some(FailureCategory::AgentStagnation)
    );
    assert_eq!(
        agent.trajectory.info.exit_reason.as_deref(),
        Some("agent_stagnation")
    );
    // Default K=4: trips after 4 identical steps (step indices 0..3, steps=4 after increment)
    assert_eq!(agent.steps, 4, "should halt at step 4");

    let info = agent
        .trajectory
        .info
        .other
        .get("stagnation")
        .expect("trajectory must carry info.stagnation");
    assert_eq!(info["count"], 4, "count should be K=4");
    assert_eq!(info["window"], 8, "window should be W=8");

    let indices: Vec<u32> =
        serde_json::from_value(info["step_indices"].clone()).expect("step_indices must be array");
    assert_eq!(indices, vec![0, 1, 2, 3]);

    let hash = info["action_hash"]
        .as_str()
        .expect("action_hash must be string");
    assert!(!hash.is_empty(), "hash must not be empty");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash must be hex: {hash}"
    );
}

// ── test (b) ──────────────────────────────────────────────────────────────────
// Three-action rotation does NOT trip with K=4, W=8.
// In any W=8 window a 3-cycle yields at most ⌈8/3⌉=3 occurrences per action,
// which is below K=4, so the detector must not fire.

#[tokio::test]
async fn no_stagnation_for_three_action_rotation() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 12;

    let responses: Vec<String> = (0..12)
        .map(|i| match i % 3 {
            0 => "```bash\nls\n```".into(),
            1 => "```bash\ncat /dev/null\n```".into(),
            _ => "```bash\necho hello\n```".into(),
        })
        .collect();

    let mut agent = build_agent(cfg, responses);
    let exit = agent.run().await.unwrap();

    assert!(
        matches!(exit, ExitReason::StepLimit { .. }),
        "expected StepLimit (no stagnation), got: {exit:?}"
    );
}

// ── test (c) ──────────────────────────────────────────────────────────────────
// Raising `--stagnation-repeat-threshold` to 5 means a run with only 4 ls
// steps reaches the step limit instead of tripping at step 4.

#[tokio::test]
async fn raising_threshold_to_5_runs_to_step_limit() {
    let mut cfg = Config::defaults().unwrap();
    // step_limit=4 means the run exhausts 4 steps (indices 0-3).
    // With K=5, the detector needs 5 identical to trip — 4 identical is not enough.
    cfg.root.agent.step_limit = 4;
    cfg.root.agent.stagnation_repeat_threshold = 5;

    let mut agent = build_agent(cfg, ls_responses(4));
    let exit = agent.run().await.unwrap();

    assert!(
        matches!(exit, ExitReason::StepLimit { .. }),
        "expected StepLimit with K=5 and only 4 steps, got: {exit:?}"
    );
}

// ── test (d) ──────────────────────────────────────────────────────────────────
// `--detect-stagnation=false` disables the detector entirely — even with many
// repeated ls actions, the run reaches the step limit.

#[tokio::test]
async fn disabled_detection_reaches_step_limit() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 8;
    cfg.root.agent.detect_stagnation = false;

    let mut agent = build_agent(cfg, ls_responses(8));
    let exit = agent.run().await.unwrap();

    assert!(
        matches!(exit, ExitReason::StepLimit { .. }),
        "expected StepLimit with detection disabled, got: {exit:?}"
    );
}

// ── test (e) ──────────────────────────────────────────────────────────────────
// When the agent trips after producing some work, the trajectory is intact and
// the stagnation info is present (patch-preservation contract).

#[tokio::test]
async fn stagnation_trajectory_is_coherent_after_earlier_work() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 40;

    // First two steps do "real" work, then the agent stagnates with ls.
    let mut responses: Vec<String> = vec![
        "```bash\necho useful-work\n```".into(),
        "```bash\necho more-work\n```".into(),
    ];
    responses.extend(ls_responses(20));

    let mut agent = build_agent(cfg, responses);
    let exit = agent.run().await.unwrap();

    assert!(
        matches!(exit, ExitReason::AgentStagnation { .. }),
        "expected AgentStagnation, got: {exit:?}"
    );
    // Trajectory is fully finalized: steps, failure_category, and stagnation info present.
    assert!(
        agent.trajectory.info.steps.is_some(),
        "steps must be recorded"
    );
    assert_eq!(
        agent.trajectory.info.failure_category,
        Some(FailureCategory::AgentStagnation)
    );
    assert!(
        agent.trajectory.info.other.contains_key("stagnation"),
        "stagnation info must be present"
    );
    // No panic, no corruption — the trajectory is usable for patch capture.
}

// ── test (f) ──────────────────────────────────────────────────────────────────
// The canonicalization rule treats `ls`, `ls  ` (extra spaces), and `ls;` as the
// same action — they all hash to the same value.

#[test]
fn canonicalization_treats_variants_as_identical() {
    use maxwells_daemon::stagnation::canonicalize_action;

    let base = canonicalize_action("ls");
    assert_eq!(
        base,
        canonicalize_action("ls  "),
        "trailing spaces must be stripped"
    );
    assert_eq!(
        base,
        canonicalize_action("ls;"),
        "trailing semicolon must be stripped"
    );
    assert_eq!(
        base,
        canonicalize_action("  ls  "),
        "leading/trailing spaces must be stripped"
    );
    assert_eq!(
        base,
        canonicalize_action("  ls  ;"),
        "leading/trailing spaces + semicolon must all be stripped"
    );
    // Internal whitespace collapse: "ls   -la" → "ls -la"
    assert_eq!(
        canonicalize_action("ls   -la"),
        canonicalize_action("ls -la"),
        "internal whitespace must be collapsed"
    );
    // Different commands must differ
    assert_ne!(
        canonicalize_action("ls"),
        canonicalize_action("cat foo"),
        "different actions must differ"
    );
}

// ── config validation test ────────────────────────────────────────────────────
// W >= K is enforced; W < K should produce a config error.

#[test]
fn stagnation_config_rejects_window_smaller_than_threshold() {
    use maxwells_daemon::agent::default::DefaultAgentBuilder;

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.stagnation_repeat_threshold = 8;
    cfg.root.agent.stagnation_window = 4; // invalid: W < K

    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let result = DefaultAgentBuilder {
        config: cfg,
        model: Arc::new(DeterministicModel::new(vec![])),
        env,
        task: "validation test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build();

    assert!(result.is_err(), "should reject W < K config");
}

// ── config validation: zero threshold/window ──────────────────────────────────

#[test]
fn stagnation_config_rejects_zero_threshold() {
    use maxwells_daemon::agent::default::DefaultAgentBuilder;

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.stagnation_repeat_threshold = 0; // invalid

    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let result = DefaultAgentBuilder {
        config: cfg,
        model: Arc::new(DeterministicModel::new(vec![])),
        env,
        task: "validation test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build();

    assert!(result.is_err(), "should reject threshold = 0");
}

#[test]
fn stagnation_config_rejects_zero_window() {
    use maxwells_daemon::agent::default::DefaultAgentBuilder;

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.stagnation_window = 0; // invalid

    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let result = DefaultAgentBuilder {
        config: cfg,
        model: Arc::new(DeterministicModel::new(vec![])),
        env,
        task: "validation test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build();

    assert!(result.is_err(), "should reject window = 0");
}
