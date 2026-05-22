//! Tests for history bounding (issue #167).
//!
//! Verifies `history_keep_last_observations` and `history_max_input_tokens`
//! elide old observations in the prompt while keeping the full content in the
//! trajectory.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use async_trait::async_trait;
use maxwells_daemon::agent::default::DefaultAgentBuilder;
use maxwells_daemon::error::EnvError;
use maxwells_daemon::{
    Agent, Config, DeterministicModel, Environment, ExitReason, FailureCategory, RunRequest,
    RunResult,
};

/// An environment that always returns a fixed-size synthetic observation.
struct SyntheticEnv {
    stdout: String,
}

impl SyntheticEnv {
    fn new_with_size(size_bytes: usize) -> Self {
        Self {
            stdout: "x".repeat(size_bytes),
        }
    }
}

#[async_trait]
impl Environment for SyntheticEnv {
    async fn run(&self, _req: RunRequest) -> Result<RunResult, EnvError> {
        Ok(RunResult {
            stdout: self.stdout.clone(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }
}

fn make_responses(n_bash: usize) -> Vec<String> {
    let mut r: Vec<String> = (0..n_bash)
        .map(|i| format!("```bash\necho step{i}\n```"))
        .collect();
    r.push("COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into());
    r
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1 (RED): keep_last_observations elides older observations from the
// model-visible prompt while keeping the full content in the trajectory.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn keep_last_observations_elides_older_from_model_prompt() {
    const N_BASH: usize = 5;
    const KEEP: usize = 2;

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 20;
    cfg.root.agent.history_keep_last_observations = Some(KEEP);
    cfg.root.agent.observation_max_bytes = 64 * 1024;

    let model = Arc::new(DeterministicModel::new(make_responses(N_BASH)));
    let env: Box<dyn Environment> = Box::new(SyntheticEnv::new_with_size(1024));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model: model.clone(),
        env,
        task: "test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(
        matches!(exit, ExitReason::Submitted { .. }),
        "expected submitted, got {exit:?}"
    );

    let inputs = model.recorded_inputs();
    let last_query = inputs.last().unwrap();

    // The model prompt must contain elision markers for steps older than KEEP.
    let elided_count = last_query
        .iter()
        .filter(|m| m.content.contains("[history-elided:"))
        .count();

    assert!(
        elided_count >= N_BASH - KEEP - 1,
        "expected at least {} elision markers in final prompt, got {}",
        N_BASH - KEEP - 1,
        elided_count,
    );

    // The most recent KEEP observations must be full (not markers).
    let user_msgs: Vec<_> = last_query
        .iter()
        .filter(|m| {
            matches!(
                m.role,
                maxwells_daemon::model::Role::User | maxwells_daemon::model::Role::Tool
            )
        })
        .collect();

    // The last KEEP user messages (excluding instance msg at front) should not
    // be elision markers.
    let recent: Vec<_> = user_msgs.iter().rev().take(KEEP).collect();
    for msg in &recent {
        assert!(
            !msg.content.contains("[history-elided:"),
            "a recent observation should not be elided: {}",
            &msg.content[..msg.content.len().min(80)]
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 (RED): trajectory records the full, untruncated observation even when
// the model only sees an elision marker. Steps that triggered elision carry
// `history_elided: true` and a positive `history_bytes_elided`.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn trajectory_records_full_content_and_elision_metadata() {
    const N_BASH: usize = 4;
    const KEEP: usize = 1;
    const OBS_SIZE: usize = 500;

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 20;
    cfg.root.agent.history_keep_last_observations = Some(KEEP);
    cfg.root.agent.observation_max_bytes = 64 * 1024;

    let model = Arc::new(DeterministicModel::new(make_responses(N_BASH)));
    let env: Box<dyn Environment> = Box::new(SyntheticEnv::new_with_size(OBS_SIZE));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    agent.run().await.unwrap();

    let traj = &agent.trajectory;
    // Trajectory must hold full content (no markers).
    let obs_messages: Vec<_> = traj
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .skip(1) // skip initial instance message
        .collect();

    for msg in &obs_messages {
        assert!(
            !msg.content.contains("[history-elided:"),
            "trajectory should contain full content, got marker: {}",
            &msg.content[..msg.content.len().min(80)]
        );
    }

    // Steps that became elided in the model-visible prompt must have
    // history_elided = true and history_bytes_elided > 0.
    let elided_steps: Vec<_> = obs_messages
        .iter()
        .filter(|m| {
            m.extra
                .other
                .get("history_elided")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .collect();

    assert!(
        elided_steps.len() >= N_BASH - KEEP,
        "expected at least {} steps with history_elided=true, got {}",
        N_BASH - KEEP,
        elided_steps.len()
    );

    for step in &elided_steps {
        let bytes_elided = step
            .extra
            .other
            .get("history_bytes_elided")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        assert!(
            bytes_elided > 0,
            "history_bytes_elided must be > 0 on elided steps"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3 (RED): regression — with both flags unset, no elision markers appear.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn no_elision_markers_when_flags_unset() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 20;
    assert!(cfg.root.agent.history_keep_last_observations.is_none());
    assert!(cfg.root.agent.history_max_input_tokens.is_none());

    let model = Arc::new(DeterministicModel::new(make_responses(5)));
    let env: Box<dyn Environment> = Box::new(SyntheticEnv::new_with_size(512));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model: model.clone(),
        env,
        task: "test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    agent.run().await.unwrap();

    for (qi, query) in model.recorded_inputs().iter().enumerate() {
        for msg in query {
            assert!(
                !msg.content.contains("[history-elided:"),
                "query {qi} unexpectedly contains an elision marker: {}",
                &msg.content[..msg.content.len().min(80)]
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4 (RED): history_max_input_tokens bounds the byte-length of the prompt
// sent to the model (1 token ≈ 4 bytes approximation).
// Uses 30+ steps with 5 KB synthetic observations — the cap must hold for every
// single query, not just the final one.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn max_input_tokens_bounds_prompt_bytes() {
    const N_BASH: usize = 32;
    const OBS_BYTES: usize = 5 * 1024; // 5 KB per observation
    // Cap at 8 000 tokens ≈ 32 000 bytes.
    // Each 5 KB observation would grow the prompt well beyond this cap without
    // elision, so the history-bounding logic must kick in every step.
    const MAX_TOKENS: u64 = 8_000;
    const MAX_BYTES_APPROX: usize = 8_000 * 4; // MAX_TOKENS * 4 bytes-per-token approximation

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 50;
    cfg.root.agent.history_max_input_tokens = Some(MAX_TOKENS);
    cfg.root.agent.observation_max_bytes = 64 * 1024;

    let model = Arc::new(DeterministicModel::new(make_responses(N_BASH)));
    let env: Box<dyn Environment> = Box::new(SyntheticEnv::new_with_size(OBS_BYTES));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model: model.clone(),
        env,
        task: "test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    agent.run().await.unwrap();

    let inputs = model.recorded_inputs();
    // We ran 32 bash steps + 1 submit, so the model should have received at
    // least 30 queries.
    assert!(
        inputs.len() >= 30,
        "expected ≥30 model queries, got {}",
        inputs.len()
    );

    for (qi, query) in inputs.iter().enumerate() {
        let total_bytes: usize = query.iter().map(|m| m.content.len()).sum();
        assert!(
            total_bytes <= MAX_BYTES_APPROX,
            "query {qi}: total bytes {total_bytes} exceeds cap {MAX_BYTES_APPROX}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5 (RED): when even the most-recent single observation exceeds the cap,
// the run exits with failure_category = history_compaction_failed.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn history_compaction_failed_when_irreducible() {
    // Tiny cap: 10 tokens ≈ 40 bytes. A 500-byte observation cannot fit.
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 20;
    cfg.root.agent.history_max_input_tokens = Some(10);
    cfg.root.agent.observation_max_bytes = 64 * 1024;

    let model = Arc::new(DeterministicModel::new(make_responses(10)));
    let env: Box<dyn Environment> = Box::new(SyntheticEnv::new_with_size(500));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();

    assert!(
        !matches!(exit, ExitReason::Submitted { .. }),
        "should not submit when cap is irreducibly too low"
    );

    assert_eq!(
        agent.trajectory.info.failure_category,
        Some(FailureCategory::HistoryCompactionFailed),
        "expected failure_category = history_compaction_failed"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6 (GREEN): when both flags are set, the more aggressive (more elision)
// rule wins — the model sees fewer observations than keep_last alone would allow.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn both_flags_compose_taking_more_aggressive() {
    const N_BASH: usize = 6;
    // Observations are 512 bytes each.
    // keep_last=4 alone would elide 2 candidates (keep 4 total obs).
    // With max_tokens=400 → max_bytes=1600:
    //   sys+inst+assistants ≈ 200+100+6*25 = ~450 bytes
    //   remaining budget for obs ≈ 1150 bytes → fits ~2 full observations
    // So max_tokens forces ≥ 4 elisions, while keep_last=4 forces only 2.
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 20;
    cfg.root.agent.history_keep_last_observations = Some(4);
    cfg.root.agent.history_max_input_tokens = Some(400); // ~1600 bytes
    cfg.root.agent.observation_max_bytes = 64 * 1024;

    let model = Arc::new(DeterministicModel::new(make_responses(N_BASH)));
    let env: Box<dyn Environment> = Box::new(SyntheticEnv::new_with_size(512));
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model: model.clone(),
        env,
        task: "test".into(),
        extra_context: None,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    agent.run().await.unwrap();

    let inputs = model.recorded_inputs();
    let last_query = inputs.last().unwrap();

    // keep_last=4 alone would elide 2 out of 5 candidates.
    // The token cap must force at least 3 elisions (strictly more than 2).
    let elision_count = last_query
        .iter()
        .filter(|m| m.content.contains("[history-elided:"))
        .count();

    let keep_last_only_elision = N_BASH - 4; // = 2
    assert!(
        elision_count > keep_last_only_elision,
        "token cap should force more elision than keep_last=4 alone; \
         expected elision_count > {keep_last_only_elision}, got {elision_count}"
    );
}
