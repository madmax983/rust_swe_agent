//! TDD tests for per-call sampling parameter recording in trajectories.
//! Issue #177: Record per-call sampling parameters in trajectories for honest measurement.
//!
//! RED → GREEN → REFACTOR cycle:
//!   (a) fresh trajectory carries `sampling` on every assistant step
//!   (b) legacy trajectory without field parses and surfaces as `sampling: null`
//!   (c) `bench compare` flags a sampling-only difference between two sweeps
//!   (d) fallback records the post-fallback model name in `sampling.model`
//!   (e) redaction masks a synthetic secret-shaped key in `sampling.extra`

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rust_swe_agent::agent::default::DefaultAgentBuilder;
use rust_swe_agent::env::RunRequest;
use rust_swe_agent::error::{EnvError, ModelError};
use rust_swe_agent::model::{
    Message, MessageExtra, Model, ModelResponse, ModelUsage, QueryOpts, SamplingParams,
};
use rust_swe_agent::redaction::Redactor;
use rust_swe_agent::run::compare::{CompareArgs, CompareFormat, compute as compare_compute};
use rust_swe_agent::run::evaluate::BreakdownSelection;
use rust_swe_agent::run::swebench::{InstanceResult, SweepResults};
use rust_swe_agent::trajectory::{
    FORMAT_VERSION, MessageRecord, Trajectory, TrajectoryInfo, outcome,
};
use rust_swe_agent::{
    Agent, Config, DeterministicModel, Environment, ExitReason, FallbackModel, RunResult,
};

mod support;

// ── shared helpers ───────────────────────────────────────────────────────────

#[derive(Clone)]
struct StaticEnv(RunResult);

#[async_trait]
impl Environment for StaticEnv {
    async fn run(&self, _req: RunRequest) -> Result<RunResult, EnvError> {
        Ok(self.0.clone())
    }
}

fn ok_env() -> Box<dyn Environment> {
    Box::new(StaticEnv(RunResult {
        stdout: "ok\n".into(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    }))
}

fn two_step_model() -> Arc<DeterministicModel> {
    Arc::new(DeterministicModel::new(vec![
        "```bash\necho hello\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
    ]))
}

fn minimal_cfg_with_temperature(temperature: f64) -> Config {
    Config::from_toml_str(&format!(
        r#"
[agent]
step_limit = 5

[model]
name = "deterministic"
temperature = {temperature}
max_tokens = 4096
"#
    ))
    .unwrap()
}

fn write_sweep_results(dir: &Path, instances: &[&str], passed: bool) {
    use rust_swe_agent::run::swebench::FilterSpec;
    let instance_results: Vec<InstanceResult> = instances
        .iter()
        .map(|id| InstanceResult {
            instance_id: (*id).to_owned(),
            exit_reason: if passed {
                "submitted".into()
            } else {
                "error".into()
            },
            outcome: Some(if passed {
                outcome::SUBMITTED.into()
            } else {
                outcome::ERROR.into()
            }),
            failure_category: None,
            steps: Some(2),
            cost_usd: Some(0.0),
            prompt_tokens: Some(10),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            completion_tokens: Some(5),
            duration_secs: Some(1.0),
            error: None,
            github_pr_error: None,
            patch_present: passed,
            non_empty_patch: passed,
            attempts: 1,
            retry_reasons: vec![],
            runs: 1,
            resolved_count: u32::from(passed),
            pass_at_1: passed,
            tests_run_before_submit: false,
            last_tests_passed: None,
            fallback_count: None,
            final_model: None,
            retry_id: None,
            previous_failure_category: None,
        })
        .collect();
    let submitted_count = instance_results.iter().filter(|r| r.pass_at_1).count();
    let errored_count = instance_results.iter().filter(|r| !r.pass_at_1).count();
    let total = instance_results.len();
    let sweep = SweepResults {
        total,
        submitted: submitted_count,
        skipped: 0,
        errored: errored_count,
        instances: instance_results,
        ..Default::default()
    };
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&sweep).unwrap(),
    )
    .unwrap();
}

fn write_traj_with_temperature(dir: &Path, instance_id: &str, temperature: f32) {
    let mut assistant_extra = MessageExtra::default();
    assistant_extra.sampling = Some(SamplingParams {
        model: "deterministic".to_owned(),
        temperature: Some(temperature),
        top_p: None,
        max_tokens: Some(4096),
        seed: None,
        extra: serde_json::Map::new(),
    });
    let traj = Trajectory {
        trajectory_format: FORMAT_VERSION.into(),
        info: TrajectoryInfo {
            outcome: Some(outcome::SUBMITTED.into()),
            model_name: Some("deterministic".into()),
            ..Default::default()
        },
        messages: vec![
            MessageRecord {
                role: "user".into(),
                content: "task".into(),
                extra: MessageExtra::default(),
            },
            MessageRecord {
                role: "assistant".into(),
                content: "response".into(),
                extra: assistant_extra,
            },
        ],
    };
    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
}

// ── test (a): fresh trajectory carries sampling on every assistant step ──────

#[tokio::test]
async fn fresh_trajectory_has_sampling_on_every_assistant_step() {
    let cfg = minimal_cfg_with_temperature(0.0);
    let model = two_step_model();
    let env = ok_env();

    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "solve the problem".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();

    let _ = agent.run().await.unwrap();
    let traj_json = agent.trajectory.to_json_pretty().unwrap();
    let traj: Trajectory = serde_json::from_str(&traj_json).unwrap();

    let assistant_messages: Vec<_> = traj
        .messages
        .iter()
        .filter(|m| m.role == "assistant")
        .collect();

    assert!(
        !assistant_messages.is_empty(),
        "trajectory should have assistant messages"
    );
    for msg in &assistant_messages {
        assert!(
            msg.extra.sampling.is_some(),
            "assistant message missing sampling block: {:?}",
            msg.content
        );
        let s = msg.extra.sampling.as_ref().unwrap();
        assert!(!s.model.is_empty(), "sampling.model must be non-empty");
    }
}

// ── test (b): legacy trajectory without field parses as null ─────────────────

#[test]
fn legacy_trajectory_without_sampling_parses_as_null() {
    let json = r#"{
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {},
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi there"},
            {"role": "user", "content": "Thanks"}
        ]
    }"#;

    let traj: Trajectory = serde_json::from_str(json)
        .expect("legacy trajectory without sampling field should parse successfully");

    for msg in &traj.messages {
        assert!(
            msg.extra.sampling.is_none(),
            "legacy message at role={} should have sampling=None (got Some)",
            msg.role
        );
    }
}

// ── test (c): bench compare flags sampling-only difference ───────────────────

#[test]
fn bench_compare_flags_sampling_only_difference() {
    let work = tempfile::tempdir().unwrap();
    let baseline_dir = work.path().join("baseline");
    let candidate_dir = work.path().join("candidate");
    std::fs::create_dir_all(&baseline_dir).unwrap();
    std::fs::create_dir_all(&candidate_dir).unwrap();

    // Both sweeps have the same outcome (pass) for the same instance.
    write_sweep_results(&baseline_dir, &["task1"], true);
    write_sweep_results(&candidate_dir, &["task1"], true);

    // Trajectory files differ only in temperature (sampling drift only).
    write_traj_with_temperature(&baseline_dir, "task1", 0.0);
    write_traj_with_temperature(&candidate_dir, "task1", 0.7);

    let args = CompareArgs {
        baseline: baseline_dir.clone(),
        candidate: candidate_dir.clone(),
        format: CompareFormat::Json,
        max_regressions: None,
        max_patch_size_regression_pct: None,
        breakdown: BreakdownSelection::none(),
        min_delta_pp: 0.0,
        cost_attribution: false,
        cost_attribution_min_delta_usd: 0.0,
        min_significance: None,
        regression_significance: None,
        allow_underpowered: true,
    };

    let report = compare_compute(&args).unwrap();

    let drift = report
        .sampling_drift
        .as_ref()
        .expect("compare should report sampling_drift when trajectories differ");

    assert!(
        drift.steps_drifted > 0,
        "expected sampling_drift.steps_drifted > 0, got {}",
        drift.steps_drifted
    );
    assert!(
        drift.example.is_some(),
        "sampling_drift should include a representative example"
    );
}

// ── test (d): fallback records post-fallback model in sampling ───────────────

struct AlwaysFail {
    name: String,
}

#[async_trait]
impl Model for AlwaysFail {
    fn name(&self) -> &str {
        &self.name
    }

    async fn query(
        &self,
        _messages: &[Message],
        _opts: &QueryOpts,
    ) -> Result<ModelResponse, rust_swe_agent::error::ModelError> {
        Err(ModelError::RateLimited(
            "scripted rate limit for test".into(),
        ))
    }
}

struct AlwaysOk {
    name: String,
}

#[async_trait]
impl Model for AlwaysOk {
    fn name(&self) -> &str {
        &self.name
    }

    async fn query(
        &self,
        _messages: &[Message],
        _opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        Ok(ModelResponse {
            content: "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
            usage: ModelUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_usd: Some(0.0),
            },
            raw: serde_json::Value::Null,
            responding_model: None,
            fallback_attempts: vec![],
        })
    }
}

#[tokio::test]
async fn fallback_records_post_fallback_model_in_sampling() {
    let primary = Box::new(AlwaysFail {
        name: "primary-model".into(),
    }) as Box<dyn Model>;
    let fallback = Box::new(AlwaysOk {
        name: "fallback-model".into(),
    }) as Box<dyn Model>;
    let chain = Arc::new(FallbackModel::new(vec![primary, fallback]));

    let cfg = Config::from_toml_str(
        r#"
[agent]
step_limit = 3

[model]
name = "primary-model"
max_tokens = 4096
"#,
    )
    .unwrap();

    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model: chain,
        env: ok_env(),
        task: "test fallback sampling".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();

    let _ = agent.run().await.unwrap();
    let traj_json = agent.trajectory.to_json_pretty().unwrap();
    let traj: Trajectory = serde_json::from_str(&traj_json).unwrap();

    let asst_messages: Vec<_> = traj
        .messages
        .iter()
        .filter(|m| m.role == "assistant")
        .collect();

    assert!(
        !asst_messages.is_empty(),
        "trajectory must have at least one assistant message"
    );

    for msg in &asst_messages {
        let sampling = msg
            .extra
            .sampling
            .as_ref()
            .expect("assistant message must have sampling block");
        assert_eq!(
            sampling.model, "fallback-model",
            "sampling.model should be the fallback model name, got {:?}",
            sampling.model
        );
    }
}

// ── test (e): redaction masks secret-shaped keys in sampling.extra ───────────

#[test]
fn redaction_masks_secret_shaped_extra_key_in_sampling_extra() {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "api_key".to_owned(),
        serde_json::json!("sk-super-secret-value-1234567890"),
    );
    extra.insert("safe_setting".to_owned(), serde_json::json!(42));

    let redactor = Redactor::default_enabled();
    let mut value = serde_json::Value::Object(extra.clone());
    redactor.redact_json_value(&mut value, "trajectory");

    let map = value.as_object().unwrap();

    let api_key_val = map["api_key"].as_str().unwrap();
    assert!(
        api_key_val.starts_with("[REDACTED"),
        "api_key value should be redacted, got: {api_key_val}"
    );

    assert_eq!(
        map["safe_setting"],
        serde_json::json!(42),
        "safe_setting should not be redacted"
    );
}
