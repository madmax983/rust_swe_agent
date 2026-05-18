//! Integration: DefaultAgent + FallbackModel — verify fallback_summary in trajectory.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use maxwells_daemon::Agent;
use maxwells_daemon::agent::default::DefaultAgentBuilder;
use maxwells_daemon::error::EnvError;
use maxwells_daemon::{
    Config, Environment, FallbackModel, LocalEnvironment, Message, Model, ModelError,
    ModelResponse, ModelUsage, QueryOpts, RunRequest, RunResult,
};

// ── test helpers ─────────────────────────────────────────────────────────────

struct AlwaysTransient {
    name: String,
}

impl AlwaysTransient {
    fn new(name: &str) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait]
impl Model for AlwaysTransient {
    fn name(&self) -> &str {
        &self.name
    }
    async fn query(&self, _: &[Message], _: &QueryOpts) -> Result<ModelResponse, ModelError> {
        Err(ModelError::RateLimited("simulated 429".into()))
    }
}

struct FixedResponseModel {
    name: String,
    responses: Vec<String>,
    calls: AtomicU32,
}

impl FixedResponseModel {
    fn new(name: &str, responses: Vec<&str>) -> Self {
        Self {
            name: name.into(),
            responses: responses.into_iter().map(String::from).collect(),
            calls: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl Model for FixedResponseModel {
    fn name(&self) -> &str {
        &self.name
    }
    async fn query(&self, _: &[Message], _: &QueryOpts) -> Result<ModelResponse, ModelError> {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst) as usize;
        let content = self.responses.get(idx).cloned().unwrap_or_else(|| {
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfallback-final\n```".into()
        });
        Ok(ModelResponse {
            content,
            usage: ModelUsage::default(),
            raw: serde_json::json!({}),
            responding_model: None,
            fallback_attempts: Vec::new(),
        })
    }
}

struct FailFirstN {
    name: String,
    fail_for: u32,
    calls: AtomicU32,
    inner: Box<dyn Model>,
}

impl FailFirstN {
    fn new(name: &str, fail_for: u32, inner: Box<dyn Model>) -> Self {
        Self {
            name: name.into(),
            fail_for,
            calls: AtomicU32::new(0),
            inner,
        }
    }
}

#[async_trait]
impl Model for FailFirstN {
    fn name(&self) -> &str {
        &self.name
    }
    async fn query(&self, msgs: &[Message], opts: &QueryOpts) -> Result<ModelResponse, ModelError> {
        let c = self.calls.fetch_add(1, Ordering::SeqCst);
        if c < self.fail_for {
            Err(ModelError::RateLimited("simulated 429".into()))
        } else {
            self.inner.query(msgs, opts).await
        }
    }
}

// Simple environment that always returns ok for bash commands.
struct AlwaysOkEnv;

#[async_trait]
impl Environment for AlwaysOkEnv {
    async fn run(&self, _req: RunRequest) -> Result<RunResult, EnvError> {
        Ok(RunResult {
            stdout: "ok\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn fallback_summary_is_populated_on_fallback() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 3;

    // Primary: always rate-limited. Secondary: immediately submits.
    let model = Arc::new(FallbackModel::new(vec![
        Box::new(AlwaysTransient::new("primary-model")),
        Box::new(FixedResponseModel::new(
            "secondary-model",
            vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```"],
        )),
    ]));

    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(LocalEnvironment::new()),
        task: "test fallback".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    resume_from: None,
    }
    .build()
    .unwrap();

    agent.run().await.unwrap();

    let summary = agent
        .trajectory
        .info
        .fallback_summary
        .as_ref()
        .expect("fallback_summary should be present");
    assert!(
        summary.fallback_happened,
        "fallback_happened should be true"
    );
    assert_eq!(summary.primary_model, "primary-model");
    assert_eq!(summary.final_model, "secondary-model");
    assert!(summary.fallback_count > 0, "fallback_count should be > 0");
    assert!(!summary.all_failed, "all_failed should be false");
    assert!(
        summary
            .attempted_models
            .contains(&"primary-model".to_owned()),
        "primary should appear in attempted_models"
    );
    assert!(
        summary
            .attempted_models
            .contains(&"secondary-model".to_owned()),
        "secondary should appear in attempted_models"
    );
}

#[tokio::test]
async fn all_candidates_fail_sets_all_failed_flag_in_trajectory() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 3;

    // Both models always fail transiently → AllCandidatesFailed → agent error.
    let model = Arc::new(FallbackModel::new(vec![
        Box::new(AlwaysTransient::new("primary-model")),
        Box::new(AlwaysTransient::new("secondary-model")),
    ]));

    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(LocalEnvironment::new()),
        task: "test all-fail".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    resume_from: None,
    }
    .build()
    .unwrap();

    let _ = agent.run().await; // expect Err — all candidates fail
    // mini.rs calls finalize_run_metadata on error paths; simulate that here.
    agent.finalize_run_metadata("error");

    let summary = agent
        .trajectory
        .info
        .fallback_summary
        .as_ref()
        .expect("fallback_summary should be present even on all-fail");
    assert!(summary.all_failed, "all_failed should be true");
    assert!(
        !summary.failed_attempts.is_empty(),
        "failed_attempts should be populated"
    );
    assert_eq!(summary.primary_model, "primary-model");
}

#[tokio::test]
async fn multi_step_fallback_preserves_all_step_responders() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    // Primary fails only on its first call, then succeeds.
    // Step 1: primary fails → secondary responds (bash echo)
    // Step 2: primary succeeds → responds (submit)
    let primary = FailFirstN::new(
        "primary-model",
        1,
        Box::new(FixedResponseModel::new(
            "primary-model",
            vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nprimary-final\n```"],
        )),
    );
    let secondary = FixedResponseModel::new("secondary-model", vec!["```bash\necho step1\n```"]);

    let model = Arc::new(FallbackModel::new(vec![
        Box::new(primary),
        Box::new(secondary),
    ]));

    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(AlwaysOkEnv),
        task: "test multi-step responders".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    resume_from: None,
    }
    .build()
    .unwrap();

    agent.run().await.unwrap();

    let summary = agent
        .trajectory
        .info
        .fallback_summary
        .as_ref()
        .expect("fallback_summary should be present");

    // Step 1 had a fallback (secondary responded), step 2 used primary.
    assert!(summary.fallback_happened, "fallback occurred in step 1");
    assert_eq!(
        summary.final_model, "primary-model",
        "last step was answered by primary"
    );
    // Both primary (failed step 1) and secondary (responded step 1) and primary
    // (responded step 2) must all appear in attempted_models.
    assert!(
        summary
            .attempted_models
            .contains(&"primary-model".to_owned()),
        "primary should be in attempted_models: {:?}",
        summary.attempted_models
    );
    assert!(
        summary
            .attempted_models
            .contains(&"secondary-model".to_owned()),
        "secondary should be in attempted_models: {:?}",
        summary.attempted_models
    );
}

#[tokio::test]
async fn no_fallback_run_leaves_no_fallback_summary() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 3;

    // Single model, no fallback configured.
    let model = Arc::new(FixedResponseModel::new(
        "only-model",
        vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```"],
    ));

    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env: Box::new(LocalEnvironment::new()),
        task: "test no-fallback".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    resume_from: None,
    }
    .build()
    .unwrap();

    agent.run().await.unwrap();

    // Non-fallback run: no fallback_summary should be written.
    assert!(
        agent.trajectory.info.fallback_summary.is_none(),
        "single-model run should not write fallback_summary"
    );
}
