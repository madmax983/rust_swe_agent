//! TDD tests for per-call sampling parameter recording in trajectories.
//! Issue #177: Record per-call sampling parameters in trajectories for honest measurement.
//!
//! RED → GREEN → REFACTOR cycle:
//!   (a) fresh trajectory carries `sampling` on every assistant step
//!   (b) legacy trajectory without field parses and surfaces as `sampling: null`
//!   (c) `bench compare` flags a sampling-only difference between two sweeps
//!   (d) fallback records the post-fallback model name in `sampling.model`
//!   (e) redaction masks a synthetic secret-shaped key in `sampling.extra`

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use maxwells_daemon::agent::default::DefaultAgentBuilder;
use maxwells_daemon::env::RunRequest;
use maxwells_daemon::error::{EnvError, ModelError};
use maxwells_daemon::model::{
    Message, MessageExtra, Model, ModelResponse, ModelUsage, QueryOpts, SamplingParams,
};
use maxwells_daemon::redaction::Redactor;
use maxwells_daemon::run::compare::{CompareArgs, CompareFormat, compute as compare_compute};
use maxwells_daemon::run::evaluate::BreakdownSelection;
use maxwells_daemon::run::swebench::{InstanceResult, SweepResults};
use maxwells_daemon::trajectory::{
    FORMAT_VERSION, MessageRecord, Trajectory, TrajectoryInfo, outcome,
};
use maxwells_daemon::{Agent, Config, DeterministicModel, Environment, FallbackModel, RunResult};

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
            trace_id: None,
            context_pressure: Default::default(),
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
    let assistant_extra = MessageExtra {
        sampling: Some(SamplingParams {
            model: "deterministic".to_owned(),
            temperature: Some(temperature),
            top_p: None,
            max_tokens: Some(4096),
            seed: None,
            extra: serde_json::Map::new(),
        }),
        ..MessageExtra::default()
    };
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
        fork_lineage: None,
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
        resume_from: None,
        read_only: false,
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
        baseline: baseline_dir,
        candidate: candidate_dir,
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
        flake_report: None,
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
    ) -> Result<ModelResponse, maxwells_daemon::error::ModelError> {
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
        resume_from: None,
        read_only: false,
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

// ── unit coverage helpers ─────────────────────────────────────────────────────

#[test]
fn sampling_params_summary_line_all_fields() {
    let s = SamplingParams {
        model: "claude-opus-4-7".into(),
        temperature: Some(0.5),
        top_p: Some(0.9),
        max_tokens: Some(4096),
        seed: Some(42),
        extra: serde_json::Map::new(),
    };
    let line = s.summary_line();
    assert!(line.contains("model=claude-opus-4-7"), "{line}");
    assert!(line.contains("temp=0.5"), "{line}");
    assert!(line.contains("top_p=0.9"), "{line}");
    assert!(line.contains("max_tokens=4096"), "{line}");
    assert!(line.contains("seed=42"), "{line}");
}

#[test]
fn sampling_params_summary_line_minimal() {
    let s = SamplingParams {
        model: "deterministic".into(),
        temperature: None,
        top_p: None,
        max_tokens: None,
        seed: None,
        extra: serde_json::Map::new(),
    };
    assert_eq!(s.summary_line(), "model=deterministic");
}

#[test]
fn sampling_drift_block_as_drift_field_soft() {
    use maxwells_daemon::run::reproduce::{DriftSeverity, SamplingDriftBlock};
    let block = SamplingDriftBlock {
        instances_drifted: 2,
        steps_drifted: 5,
    };
    let field = block.as_drift_field(false);
    assert_eq!(field.field, "sampling");
    assert_eq!(field.severity, DriftSeverity::Soft);
    assert!(field.message.contains("5 step(s)"), "{}", field.message);
    assert!(field.message.contains("2 instance(s)"), "{}", field.message);
}

#[test]
fn sampling_drift_block_as_drift_field_hard() {
    use maxwells_daemon::run::reproduce::{DriftSeverity, SamplingDriftBlock};
    let block = SamplingDriftBlock {
        instances_drifted: 1,
        steps_drifted: 3,
    };
    let field = block.as_drift_field(true);
    assert_eq!(field.severity, DriftSeverity::Hard);
}

#[test]
fn bench_compare_text_output_includes_sampling_drift() {
    let work = tempfile::tempdir().unwrap();
    let baseline_dir = work.path().join("baseline");
    let candidate_dir = work.path().join("candidate");
    std::fs::create_dir_all(&baseline_dir).unwrap();
    std::fs::create_dir_all(&candidate_dir).unwrap();

    write_sweep_results(&baseline_dir, &["task1"], true);
    write_sweep_results(&candidate_dir, &["task1"], true);
    write_traj_with_temperature(&baseline_dir, "task1", 0.0);
    write_traj_with_temperature(&candidate_dir, "task1", 0.9);

    let args = maxwells_daemon::run::compare::CompareArgs {
        baseline: baseline_dir,
        candidate: candidate_dir,
        format: maxwells_daemon::run::compare::CompareFormat::Text,
        max_regressions: None,
        max_patch_size_regression_pct: None,
        breakdown: maxwells_daemon::run::evaluate::BreakdownSelection::none(),
        min_delta_pp: 0.0,
        cost_attribution: false,
        cost_attribution_min_delta_usd: 0.0,
        min_significance: None,
        regression_significance: None,
        allow_underpowered: true,
        flake_report: None,
    };
    let report = compare_compute(&args).unwrap();
    let text = report.human_table();
    assert!(
        text.contains("Sampling drift"),
        "text output should mention sampling drift, got:\n{text}"
    );
}

#[test]
fn load_all_trajectories_finds_multi_run_files() {
    let work = tempfile::tempdir().unwrap();
    let instance_id = "my_instance";
    let inst_dir = work.path().join(instance_id);
    std::fs::create_dir_all(&inst_dir).unwrap();

    // write_traj_with_temperature writes "{id}.traj.json" in the given dir.
    // Place run-1 and run-2 inside the instance subdirectory.
    write_traj_with_temperature(&inst_dir, "run-1", 0.1);
    write_traj_with_temperature(&inst_dir, "run-2", 0.2);

    let trajs =
        maxwells_daemon::trajectory::load_all_trajectories_for_instance(work.path(), instance_id);
    assert_eq!(trajs.len(), 2, "should load both run-1 and run-2");
}

#[test]
fn inspect_redaction_masks_sampling_extra_secret_key() {
    use maxwells_daemon::run::inspect::redact_trajectory_for_inspect;
    use maxwells_daemon::trajectory::{MessageRecord, Trajectory, TrajectoryInfo};

    let mut secret_extra = serde_json::Map::new();
    secret_extra.insert("api_key".into(), serde_json::json!("sk-1234567890abcdef"));
    secret_extra.insert("window".into(), serde_json::json!(8192));

    let mut traj = Trajectory {
        trajectory_format: maxwells_daemon::trajectory::FORMAT_VERSION.into(),
        info: TrajectoryInfo::default(),
        messages: vec![MessageRecord {
            role: "assistant".into(),
            content: "ok".into(),
            extra: maxwells_daemon::model::MessageExtra {
                sampling: Some(SamplingParams {
                    model: "m".into(),
                    temperature: None,
                    top_p: None,
                    max_tokens: None,
                    seed: None,
                    extra: secret_extra,
                }),
                ..Default::default()
            },
        }],
        fork_lineage: None,
    };

    let redactor = Redactor::default_enabled();
    let changed = redact_trajectory_for_inspect(&mut traj, &redactor);
    assert!(changed, "should report redaction happened");

    let sampling = traj.messages[0].extra.sampling.as_ref().unwrap();
    let key_val = sampling.extra["api_key"].as_str().unwrap();
    assert!(
        key_val.starts_with("[REDACTED"),
        "api_key should be redacted in sampling.extra, got: {key_val}"
    );
    assert_eq!(
        sampling.extra["window"],
        serde_json::json!(8192),
        "non-secret key should be unchanged"
    );
}

// ── test (f): Some vs None sampling counts as drift ──────────────────────────

fn write_legacy_traj(dir: &Path, instance_id: &str) {
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
                extra: MessageExtra::default(), // no sampling block
            },
        ],
        fork_lineage: None,
    };
    std::fs::write(
        dir.join(format!("{instance_id}.traj.json")),
        serde_json::to_string_pretty(&traj).unwrap(),
    )
    .unwrap();
}

#[test]
fn bench_compare_flags_sampling_presence_vs_absence_as_drift() {
    let work = tempfile::tempdir().unwrap();
    let baseline_dir = work.path().join("baseline");
    let candidate_dir = work.path().join("candidate");
    std::fs::create_dir_all(&baseline_dir).unwrap();
    std::fs::create_dir_all(&candidate_dir).unwrap();

    write_sweep_results(&baseline_dir, &["task1"], true);
    write_sweep_results(&candidate_dir, &["task1"], true);
    // Baseline has a sampling block; candidate is a legacy trajectory without one.
    write_traj_with_temperature(&baseline_dir, "task1", 0.0);
    write_legacy_traj(&candidate_dir, "task1");

    let args = maxwells_daemon::run::compare::CompareArgs {
        baseline: baseline_dir,
        candidate: candidate_dir,
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
        flake_report: None,
    };
    let report = compare_compute(&args).unwrap();
    let drift = report
        .sampling_drift
        .as_ref()
        .expect("should detect sampling drift when one side is legacy");
    assert!(
        drift.steps_drifted > 0,
        "Some(params) vs null should count as drift, got steps_drifted={}",
        drift.steps_drifted
    );
}
