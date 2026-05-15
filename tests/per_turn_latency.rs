//! Per-turn wall-clock attribution: model / tool / harness latency on each
//! turn's `MessageExtra`, with totals surfaced through `bench inspect` and
//! distributional stats through `bench evaluate`. See issue #159.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rust_swe_agent::agent::default::DefaultAgentBuilder;
use rust_swe_agent::artifact::ArtifactSchemaVersion;
use rust_swe_agent::run::evaluate::EvaluationResults;
use rust_swe_agent::run::inspect::{InspectReport, render_text};
use rust_swe_agent::trajectory::Trajectory;
use rust_swe_agent::{
    Agent, Config, DeterministicModel, Environment, ExitReason, LocalEnvironment, Message,
    MessageExtra, Model, ModelResponse, ModelUsage, QueryOpts, ToolHookCfg,
};

// -- 1. Schema: MessageExtra carries the three latency fields.

#[test]
fn message_extra_supports_latency_fields_roundtrip() {
    let extra = MessageExtra {
        model_latency_ms: Some(1234),
        tool_latency_ms: Some(56),
        harness_overhead_ms: Some(78),
        ..MessageExtra::default()
    };
    let json = serde_json::to_string(&extra).unwrap();
    assert!(json.contains("\"model_latency_ms\":1234"), "{json}");
    assert!(json.contains("\"tool_latency_ms\":56"), "{json}");
    assert!(json.contains("\"harness_overhead_ms\":78"), "{json}");

    let back: MessageExtra = serde_json::from_str(&json).unwrap();
    assert_eq!(back.model_latency_ms, Some(1234));
    assert_eq!(back.tool_latency_ms, Some(56));
    assert_eq!(back.harness_overhead_ms, Some(78));
}

#[test]
fn message_extra_omits_latency_fields_when_none() {
    let extra = MessageExtra::default();
    let json = serde_json::to_string(&extra).unwrap();
    assert!(!json.contains("model_latency_ms"), "{json}");
    assert!(!json.contains("tool_latency_ms"), "{json}");
    assert!(!json.contains("harness_overhead_ms"), "{json}");
}

#[test]
fn legacy_trajectory_without_latency_fields_parses_cleanly() {
    // Old artifact: no latency fields anywhere. Must still parse with
    // omitted-as-unknown semantics.
    let json = r#"{
  "trajectory_format": "mini-swe-agent-1.2",
  "info": {},
  "messages": [{"role":"assistant","content":"hi","extra":{}}]
}"#;
    let traj: Trajectory = serde_json::from_str(json).unwrap();
    assert_eq!(traj.messages[0].extra.model_latency_ms, None);
    assert_eq!(traj.messages[0].extra.tool_latency_ms, None);
    assert_eq!(traj.messages[0].extra.harness_overhead_ms, None);
}

// -- 2. Artifact schema bump: 1.5 -> 1.6.

#[test]
fn artifact_schema_minor_bumped_for_latency_attribution() {
    assert_eq!(
        ArtifactSchemaVersion::CURRENT,
        ArtifactSchemaVersion::new(1, 7),
    );
}

// -- 3. Agent loop: a real (non-skip) model produces model_latency on
// assistant turns and tool_latency on observation turns; reconciliation
// holds within tolerance.

struct SlowModel {
    responses: Mutex<VecDeque<String>>,
    model_delay: Duration,
}

impl SlowModel {
    fn new(responses: impl IntoIterator<Item = String>, model_delay: Duration) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            model_delay,
        }
    }
}

#[async_trait]
impl Model for SlowModel {
    fn name(&self) -> &'static str {
        "slow-model"
    }
    async fn query(
        &self,
        _messages: &[Message],
        _opts: &QueryOpts,
    ) -> Result<ModelResponse, rust_swe_agent::ModelError> {
        tokio::time::sleep(self.model_delay).await;
        let content = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| rust_swe_agent::ModelError::Malformed("scripted: no more".into()))?;
        Ok(ModelResponse {
            content,
            usage: ModelUsage {
                cost_usd: Some(0.0),
                ..ModelUsage::default()
            },
            raw: serde_json::json!({}),
            responding_model: None,
            fallback_attempts: vec![],
        })
    }
}

#[tokio::test]
async fn assistant_turn_records_model_latency_when_model_is_real() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(SlowModel::new(
        vec![
            "```bash\nsleep 0.1 && echo done\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ],
        Duration::from_millis(60),
    ));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "latency".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let assistant_with_model_latency: Vec<u64> = agent
        .trajectory
        .messages
        .iter()
        .filter(|m| m.role == "assistant")
        .filter_map(|m| m.extra.model_latency_ms)
        .collect();
    assert!(
        !assistant_with_model_latency.is_empty(),
        "expected at least one assistant turn with model_latency_ms"
    );
    // The 60ms sleep should be measurable.
    assert!(
        assistant_with_model_latency.iter().any(|ms| *ms >= 40),
        "expected at least one model_latency_ms ≥ 40, got {assistant_with_model_latency:?}",
    );
}

#[tokio::test]
async fn observation_turn_records_tool_latency_when_bash_runs() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(SlowModel::new(
        vec![
            "```bash\nsleep 0.1\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ],
        Duration::from_millis(5),
    ));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "latency".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();
    agent.run().await.unwrap();

    let tool_latencies: Vec<u64> = agent
        .trajectory
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.extra.tool_latency_ms)
        .collect();
    assert!(
        tool_latencies.iter().any(|ms| *ms >= 80),
        "expected an observation with tool_latency_ms ≥ 80, got {tool_latencies:?}",
    );
}

#[tokio::test]
async fn deterministic_model_assistant_turn_omits_model_latency() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 3;

    let model = Arc::new(DeterministicModel::new(vec![
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfin\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "x".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();
    agent.run().await.unwrap();

    // For DeterministicModel, model_latency_ms is unmeasurable in a
    // meaningful sense — it must be omitted, not zeroed.
    for m in &agent.trajectory.messages {
        if m.role == "assistant" {
            assert!(
                m.extra.model_latency_ms.is_none(),
                "deterministic-model assistant turn must omit model_latency_ms; got {:?}",
                m.extra.model_latency_ms
            );
        }
    }
}

#[tokio::test]
async fn per_turn_stage_times_reconcile_to_duration_within_5_percent() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;

    let model = Arc::new(SlowModel::new(
        vec![
            "```bash\necho hi\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ],
        Duration::from_millis(20),
    ));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "rec".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();
    agent.run().await.unwrap();

    let duration_ms = agent.trajectory.info.duration_secs.unwrap_or(0.0) * 1000.0;
    let stage_sum_ms: u64 = agent
        .trajectory
        .messages
        .iter()
        .map(|m| {
            m.extra.model_latency_ms.unwrap_or(0)
                + m.extra.tool_latency_ms.unwrap_or(0)
                + m.extra.harness_overhead_ms.unwrap_or(0)
        })
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let stage_sum = stage_sum_ms as f64;
    let drift = (stage_sum - duration_ms).abs();
    assert!(
        drift <= duration_ms * 0.05 + 50.0, // tolerance + 50ms slack for short runs
        "stage sum {stage_sum}ms drifted from duration {duration_ms}ms by {drift}ms",
    );
}

#[tokio::test]
async fn slow_post_tool_hook_time_is_attributed_to_current_obs_turn() {
    // A slow post_tool_use hook runs *after* tool exec but before the
    // observation is recorded. Its wall-clock must land in the current
    // obs turn's harness_overhead_ms — not leak to the next turn (and
    // get lost entirely if the run terminates here).
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.post_tool_use = vec![ToolHookCfg {
        name: "slow-post".into(),
        command: "sleep 0.1".into(),
        timeout_secs: Some(5),
    }];

    let model = Arc::new(SlowModel::new(
        vec![
            "```bash\necho hi\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ],
        Duration::from_millis(2),
    ));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "post-hook".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();
    agent.run().await.unwrap();

    let obs_with_post_hook_time = agent
        .trajectory
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .find(|m| m.extra.harness_overhead_ms.unwrap_or(0) >= 80);
    assert!(
        obs_with_post_hook_time.is_some(),
        "slow post-hook (≥100ms) must show up as harness_overhead_ms (≥80) on the current obs turn; got {:?}",
        agent
            .trajectory
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| (m.extra.harness_overhead_ms, m.extra.tool_latency_ms))
            .collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn slow_pre_tool_hook_makes_harness_overhead_dominate_observation_turn() {
    // A slow pre-tool hook simulates harness-side time (rate-limit waits,
    // retries, redaction, env setup). The tool itself runs instantly and
    // the model is near-instant — so the observation turn's
    // harness_overhead_ms should outweigh both.
    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 5;
    cfg.root.agent.hooks.pre_tool_use = vec![ToolHookCfg {
        name: "slow-stub".into(),
        command: "sleep 0.1".into(),
        timeout_secs: Some(5),
    }];

    let model = Arc::new(SlowModel::new(
        vec![
            "```bash\necho hi\n```".into(),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ],
        Duration::from_millis(2),
    ));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "harness-dominant".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();
    agent.run().await.unwrap();

    let dominated_obs = agent
        .trajectory
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .find(|m| {
            let h = m.extra.harness_overhead_ms.unwrap_or(0);
            let t = m.extra.tool_latency_ms.unwrap_or(0);
            h >= 80 && h > t
        });
    assert!(
        dominated_obs.is_some(),
        "expected an observation turn where harness_overhead_ms (≥80) dominates tool_latency_ms; got {:?}",
        agent
            .trajectory
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| (m.extra.harness_overhead_ms, m.extra.tool_latency_ms))
            .collect::<Vec<_>>(),
    );
}

// -- 4. `bench inspect` exposes per-stage totals and shares.

#[test]
fn inspect_report_aggregates_stage_totals() {
    let mut traj = Trajectory::new();
    traj.info.duration_secs = Some(1.0);
    let mut a = Message::assistant("hi");
    a.extra.model_latency_ms = Some(400);
    a.extra.harness_overhead_ms = Some(20);
    traj.record_message(&a);
    let mut u = Message::user("obs");
    u.extra.tool_latency_ms = Some(500);
    u.extra.harness_overhead_ms = Some(30);
    traj.record_message(&u);

    let tmp = tempfile::tempdir().unwrap();
    let sweep = tmp.path();
    let traj_path = sweep.join("aa.traj.json");
    traj.save_pretty(&traj_path).unwrap();
    // Required by run::compare::load_sweep when walking the sweep, but inspect
    // can also load a specific .traj.json directly. We bypass load_sweep here
    // by using build_instance_report through public `run`.

    let args = rust_swe_agent::run::inspect::InspectArgs {
        sweep: sweep.to_path_buf(),
        instance: Some("aa".into()),
        filter: None,
        full: true,
        show_expected: false,
    };
    let out = rust_swe_agent::run::inspect::run(&args).unwrap();
    let report: &InspectReport = match &out {
        rust_swe_agent::run::inspect::InspectOutput::Instance(r) => r,
        rust_swe_agent::run::inspect::InspectOutput::Summary(_) => {
            panic!("expected instance report")
        }
    };
    assert_eq!(report.model_latency_ms_total, Some(400));
    assert_eq!(report.tool_latency_ms_total, Some(500));
    assert_eq!(report.harness_overhead_ms_total, Some(50));

    let text = render_text(&out);
    assert!(text.contains("model_ms"), "{text}");
    assert!(text.contains("tool_ms"), "{text}");
    assert!(text.contains("harness_ms"), "{text}");
}

#[test]
fn legacy_trajectory_inspect_renders_latency_unknown() {
    // A trajectory with no per-turn latency fields (pre-1.5) must render an
    // explicit `latency: unknown` marker in the inspect text header so
    // operators don't misread silence as zero.
    let traj_json = r#"{
  "trajectory_format": "mini-swe-agent-1.2",
  "info": {"task": "legacy", "duration_secs": 1.0},
  "messages": [
    {"role": "system", "content": "sys"},
    {"role": "user", "content": "task"}
  ]
}"#;
    let tmp = tempfile::tempdir().unwrap();
    let sweep = tmp.path();
    std::fs::write(sweep.join("legacy.traj.json"), traj_json).unwrap();

    let args = rust_swe_agent::run::inspect::InspectArgs {
        sweep: sweep.to_path_buf(),
        instance: Some("legacy".into()),
        filter: None,
        full: true,
        show_expected: false,
    };
    let out = rust_swe_agent::run::inspect::run(&args).unwrap();
    let text = render_text(&out);
    assert!(
        text.contains("latency:") && text.contains("unknown"),
        "expected `latency: unknown` line on legacy trajectory, got:\n{text}"
    );
}

// -- 5. `bench evaluate` exposes per-stage p50/p95 in EvaluationResults JSON.

#[test]
fn evaluation_results_carry_latency_summary_field() {
    let json = r#"{
        "instances": [],
        "behavioral": {
            "tests_run_before_submit_rate": 0.0,
            "resolved_rate_when_tests_run": 0.0,
            "resolved_rate_when_tests_skipped": 0.0
        },
        "latency_summary": {
            "model_latency_ms": {"p50": 100, "p95": 200},
            "tool_latency_ms": {"p50": 50, "p95": 80},
            "harness_overhead_ms": {"p50": 10, "p95": 30}
        }
    }"#;
    let parsed: EvaluationResults = serde_json::from_str(json).unwrap();
    let summary = parsed
        .latency_summary
        .as_ref()
        .expect("evaluation results must carry latency_summary");
    assert_eq!(summary.model_latency_ms.as_ref().unwrap().p50, 100);
    assert_eq!(summary.model_latency_ms.as_ref().unwrap().p95, 200);
    assert_eq!(summary.tool_latency_ms.as_ref().unwrap().p95, 80);
    assert_eq!(summary.harness_overhead_ms.as_ref().unwrap().p50, 10);
}
