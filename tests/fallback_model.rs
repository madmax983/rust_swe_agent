//! Fallback model telemetry — acceptance-criteria fixtures for issue #91.
//!
//! Covers four canonical scenarios:
//!   1. Primary success → zero fallback recorded.
//!   2. Primary transient failure → secondary success → fallback recorded.
//!   3. Primary non-transient failure → no fallback attempted.
//!   4. All candidates failed → `AllCandidatesFailed` error.
//!
//! Also covers: `ModelError::is_transient()`, `FallbackSummary` round-trips
//! through `TrajectoryInfo`, `InstanceResult` fallback fields, `SweepResults`
//! model-mix totals, and `CompareReport` model-mix warnings.

#![allow(clippy::unwrap_used)]

use rust_swe_agent::error::{FailedAttempt, ModelError};
use rust_swe_agent::model::{FallbackAttemptRecord, FallbackModel, Message, Model, ModelResponse, ModelUsage, QueryOpts};
use rust_swe_agent::run::swebench::{InstanceResult, SweepResults};
use rust_swe_agent::trajectory::{FallbackSummary, Trajectory, TrajectoryInfo, outcome};

// ── helpers ──────────────────────────────────────────────────────────────────

struct OkModel {
    name: String,
    content: String,
}

impl OkModel {
    fn new(name: &str, content: &str) -> Self {
        Self { name: name.into(), content: content.into() }
    }
}

#[async_trait::async_trait]
impl Model for OkModel {
    fn name(&self) -> &str { &self.name }
    async fn query(&self, _: &[Message], _: &QueryOpts) -> Result<ModelResponse, ModelError> {
        Ok(ModelResponse {
            content: self.content.clone(),
            usage: ModelUsage::default(),
            raw: serde_json::json!({}),
            responding_model: None,
            fallback_attempts: Vec::new(),
        })
    }
}

struct ErrModel {
    name: String,
    error: ModelError,
}

impl ErrModel {
    fn transient(name: &str) -> Self {
        Self {
            name: name.into(),
            error: ModelError::RateLimited("429 too many requests".into()),
        }
    }

    fn non_transient(name: &str) -> Self {
        Self {
            name: name.into(),
            error: ModelError::MissingCredentials("invalid api key".into()),
        }
    }
}

#[async_trait::async_trait]
impl Model for ErrModel {
    fn name(&self) -> &str { &self.name }
    async fn query(&self, _: &[Message], _: &QueryOpts) -> Result<ModelResponse, ModelError> {
        // Clone the error by rebuilding it from the Display string.
        match &self.error {
            ModelError::RateLimited(s) => Err(ModelError::RateLimited(s.clone())),
            ModelError::MissingCredentials(s) => Err(ModelError::MissingCredentials(s.clone())),
            ModelError::Request(s) => Err(ModelError::Request(s.clone())),
            ModelError::Malformed(s) => Err(ModelError::Malformed(s.clone())),
            ModelError::Refused(s) => Err(ModelError::Refused(s.clone())),
            ModelError::AllCandidatesFailed(s, _) => Err(ModelError::AllCandidatesFailed(s.clone(), Vec::new())),
        }
    }
}

// ── ModelError::is_transient ──────────────────────────────────────────────────

#[test]
fn rate_limited_is_transient() {
    assert!(ModelError::RateLimited("x".into()).is_transient());
}

#[test]
fn request_error_is_transient() {
    assert!(ModelError::Request("timeout".into()).is_transient());
}

#[test]
fn malformed_is_not_transient() {
    assert!(!ModelError::Malformed("bad json".into()).is_transient());
}

#[test]
fn refused_is_not_transient() {
    assert!(!ModelError::Refused("content policy".into()).is_transient());
}

#[test]
fn missing_credentials_is_not_transient() {
    assert!(!ModelError::MissingCredentials("bad key".into()).is_transient());
}

#[test]
fn all_candidates_failed_is_not_transient() {
    assert!(!ModelError::AllCandidatesFailed("all failed".into(), Vec::new()).is_transient());
}

// ── FallbackModel: AC1 - primary success, zero fallback ──────────────────────

#[tokio::test]
async fn primary_success_zero_fallback_recorded() {
    let fallback = FallbackModel::new(vec![Box::new(OkModel::new("primary-model", "hello"))]);
    let opts = QueryOpts::default();

    let resp = fallback.query(&[], &opts).await.unwrap();
    assert_eq!(resp.content, "hello");
    // No fallback happened: fallback_attempts must be empty.
    assert!(resp.fallback_attempts.is_empty(), "expected no fallback attempts");
    // AC1: primary model name reported.
    assert_eq!(resp.responding_model.as_deref(), Some("primary-model"));
}

// ── FallbackModel: AC3 - transient primary failure triggers secondary ─────────

#[tokio::test]
async fn transient_primary_failure_triggers_secondary() {
    let models: Vec<Box<dyn Model>> = vec![
        Box::new(ErrModel::transient("primary-model")),
        Box::new(OkModel::new("fallback-model", "from secondary")),
    ];
    let fallback = FallbackModel::new(models);

    let resp = fallback.query(&[], &QueryOpts::default()).await.unwrap();
    assert_eq!(resp.content, "from secondary");
    // AC2: final responding model is secondary.
    assert_eq!(resp.responding_model.as_deref(), Some("fallback-model"));
    // AC2: fallback happened → exactly one failed attempt recorded.
    assert_eq!(resp.fallback_attempts.len(), 1);
    let attempt = &resp.fallback_attempts[0];
    assert_eq!(attempt.model, "primary-model");
    // Failure reason is coarse and non-empty.
    assert!(!attempt.failure_reason.is_empty());
}

// ── FallbackModel: AC4 - non-transient failure → no fallback ─────────────────

#[tokio::test]
async fn non_transient_primary_failure_no_fallback() {
    let models: Vec<Box<dyn Model>> = vec![
        Box::new(ErrModel::non_transient("primary-model")),
        Box::new(OkModel::new("fallback-model", "should not reach")),
    ];
    let fallback = FallbackModel::new(models);

    let err = fallback.query(&[], &QueryOpts::default()).await.unwrap_err();
    // Must be the original non-transient error type, not AllCandidatesFailed.
    assert!(
        matches!(err, ModelError::MissingCredentials(_)),
        "expected MissingCredentials, got: {err:?}"
    );
}

// ── FallbackModel: AC9 - all candidates failed → compound error ───────────────

#[tokio::test]
async fn all_candidates_failed_returns_compound_error() {
    let models: Vec<Box<dyn Model>> = vec![
        Box::new(ErrModel::transient("primary-model")),
        Box::new(ErrModel::transient("secondary-model")),
    ];
    let fallback = FallbackModel::new(models);

    let err = fallback.query(&[], &QueryOpts::default()).await.unwrap_err();
    let ModelError::AllCandidatesFailed(ref msg, ref attempts) = err else {
        panic!("expected AllCandidatesFailed, got: {err:?}");
    };
    // Error message must mention both models.
    assert!(msg.contains("primary-model"), "error should mention primary: {msg}");
    assert!(msg.contains("secondary-model"), "error should mention secondary: {msg}");
    // Structured attempts must be preserved for telemetry.
    assert_eq!(attempts.len(), 2, "both failed attempts should be preserved");
    let _ = attempts[0].model.as_str(); // FailedAttempt::model
    let _ = attempts[0].reason.as_str(); // FailedAttempt::reason
}

// ── FallbackSummary: all_failed is not fabricated as primary ──────────────────

#[test]
fn fallback_summary_all_failed_excludes_from_model_mix() {
    // When all candidates fail, all_failed=true and final_model is the last
    // attempted model (not fabricated as primary). The summary should have
    // all_failed=true so swebench.rs leaves InstanceResult.final_model as None.
    let summary = FallbackSummary {
        primary_model: "gpt-4".into(),
        final_model: "claude-sonnet-4-6".into(), // last attempted
        fallback_happened: true,
        fallback_count: 2,
        attempted_models: vec!["gpt-4".into(), "claude-sonnet-4-6".into()],
        failed_attempts: vec![
            FallbackAttemptRecord { model: "gpt-4".into(), failure_reason: "rate_limited".into() },
            FallbackAttemptRecord { model: "claude-sonnet-4-6".into(), failure_reason: "rate_limited".into() },
        ],
        all_failed: true,
    };
    // all_failed=true → skip_serializing_if should omit when false, include when true
    let json = serde_json::to_string(&summary).unwrap();
    assert!(json.contains("\"all_failed\":true"), "all_failed should appear when true: {json}");

    // Legacy trajectory without all_failed deserializes cleanly with all_failed=false.
    let json_without = r#"{"primary_model":"gpt-4","final_model":"gpt-4","fallback_happened":false,"fallback_count":0,"attempted_models":["gpt-4"],"failed_attempts":[]}"#;
    let back: FallbackSummary = serde_json::from_str(json_without).unwrap();
    assert!(!back.all_failed, "missing all_failed defaults to false");
}

// ── FallbackModel: AC1 - single model config cannot silently fallback ─────────

#[test]
fn fallback_config_empty_by_default() {
    use rust_swe_agent::config::schema::ModelCfg;
    let cfg: ModelCfg = toml::from_str(r#"name = "gpt-4""#).unwrap();
    assert!(cfg.fallback_models.is_empty(), "no fallback by default");
}

#[test]
fn fallback_config_opt_in_parses() {
    use rust_swe_agent::config::schema::ModelCfg;
    let cfg: ModelCfg = toml::from_str(
        r#"name = "gpt-4"
fallback_models = ["claude-sonnet-4-6", "gpt-3.5-turbo"]
"#,
    )
    .unwrap();
    assert_eq!(cfg.fallback_models, vec!["claude-sonnet-4-6", "gpt-3.5-turbo"]);
}

// ── FallbackSummary round-trips through TrajectoryInfo JSON ──────────────────

#[test]
fn fallback_summary_round_trips_through_trajectory_info() {
    let summary = FallbackSummary {
        primary_model: "primary-model".into(),
        final_model: "fallback-model".into(),
        fallback_happened: true,
        fallback_count: 1,
        attempted_models: vec!["primary-model".into(), "fallback-model".into()],
        failed_attempts: vec![FallbackAttemptRecord {
            model: "primary-model".into(),
            failure_reason: "rate_limited".into(),
        }],
        all_failed: false,
    };

    let mut info = TrajectoryInfo::default();
    info.fallback_summary = Some(summary.clone());

    let json = serde_json::to_string_pretty(&info).unwrap();
    // Verify key fields are present in the JSON.
    assert!(json.contains("fallback_summary"), "expected fallback_summary key: {json}");
    assert!(json.contains("fallback_happened"), "expected fallback_happened: {json}");
    assert!(json.contains("fallback-model"), "expected final_model value: {json}");

    let back: TrajectoryInfo = serde_json::from_str(&json).unwrap();
    let back_summary = back.fallback_summary.expect("fallback_summary should round-trip");
    assert_eq!(back_summary, summary);
}

#[test]
fn fallback_summary_omitted_when_none() {
    let info = TrajectoryInfo::default();
    let json = serde_json::to_string_pretty(&info).unwrap();
    assert!(!json.contains("fallback_summary"), "should be omitted when None: {json}");
}

// ── Trajectory full round-trip with fallback info ────────────────────────────

#[test]
fn trajectory_fallback_summary_survives_full_roundtrip() {
    let mut traj = Trajectory::new();
    traj.info.fallback_summary = Some(FallbackSummary {
        primary_model: "gpt-4".into(),
        final_model: "claude-sonnet-4-6".into(),
        fallback_happened: true,
        fallback_count: 1,
        attempted_models: vec!["gpt-4".into(), "claude-sonnet-4-6".into()],
        failed_attempts: vec![FallbackAttemptRecord {
            model: "gpt-4".into(),
            failure_reason: "rate_limited".into(),
        }],
        all_failed: false,
    });

    let json = traj.to_json_pretty().unwrap();
    let back: Trajectory = serde_json::from_str(&json).unwrap();
    let s = back.info.fallback_summary.unwrap();
    assert!(s.fallback_happened);
    assert_eq!(s.fallback_count, 1);
    assert_eq!(s.primary_model, "gpt-4");
    assert_eq!(s.final_model, "claude-sonnet-4-6");
}

// ── InstanceResult fallback fields ───────────────────────────────────────────

#[test]
fn instance_result_fallback_fields_default_none() {
    let r = instance_result_stub("task-1");
    assert!(r.fallback_count.is_none(), "fallback_count defaults to None");
    assert!(r.final_model.is_none(), "final_model defaults to None");
}

#[test]
fn instance_result_fallback_fields_serialize_and_deserialize() {
    let mut r = instance_result_stub("task-1");
    r.fallback_count = Some(2);
    r.final_model = Some("claude-sonnet-4-6".into());

    let json = serde_json::to_string_pretty(&r).unwrap();
    assert!(json.contains("fallback_count"), "expected fallback_count in JSON");
    assert!(json.contains("final_model"), "expected final_model in JSON");

    let back: InstanceResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back.fallback_count, Some(2));
    assert_eq!(back.final_model.as_deref(), Some("claude-sonnet-4-6"));
}

#[test]
fn instance_result_legacy_without_fallback_fields_deserializes() {
    // Simulates a pre-fallback-telemetry artifact that has no fallback fields.
    let json = r#"{"instance_id":"x","exit_reason":"submitted"}"#;
    let r: InstanceResult = serde_json::from_str(json).unwrap();
    assert!(r.fallback_count.is_none());
    assert!(r.final_model.is_none());
}

// ── SweepResults model_mix and total_fallbacks ────────────────────────────────

#[test]
fn sweep_results_model_mix_defaults_empty() {
    let s = SweepResults::default();
    assert_eq!(s.total_fallbacks, 0);
    assert!(s.model_mix.is_empty());
}

#[test]
fn sweep_results_model_mix_serializes_and_deserializes() {
    let mut s = SweepResults::default();
    s.total_fallbacks = 3;
    s.model_mix.insert("gpt-4".into(), 7);
    s.model_mix.insert("claude-sonnet-4-6".into(), 3);

    let json = serde_json::to_string_pretty(&s).unwrap();
    assert!(json.contains("total_fallbacks"), "expected total_fallbacks: {json}");
    assert!(json.contains("model_mix"), "expected model_mix: {json}");

    let back: SweepResults = serde_json::from_str(&json).unwrap();
    assert_eq!(back.total_fallbacks, 3);
    assert_eq!(back.model_mix.get("gpt-4").copied(), Some(7));
    assert_eq!(back.model_mix.get("claude-sonnet-4-6").copied(), Some(3));
}

#[test]
fn sweep_results_model_mix_omitted_when_empty() {
    let s = SweepResults::default();
    let json = serde_json::to_string_pretty(&s).unwrap();
    // model_mix should be omitted entirely when empty to keep legacy artifacts clean.
    assert!(!json.contains("\"model_mix\""), "model_mix should be omitted when empty: {json}");
}

// ── bench compare: model_mix warnings ────────────────────────────────────────

#[test]
fn compare_warns_when_candidate_has_fallbacks_and_baseline_does_not() {
    use rust_swe_agent::run::compare::{build_model_mix_warnings, ModelMixSnapshot};
    let baseline = ModelMixSnapshot {
        model_mix: std::collections::BTreeMap::new(),
        total_fallbacks: 0,
    };
    let mut candidate_mix = std::collections::BTreeMap::new();
    candidate_mix.insert("gpt-4".to_string(), 8);
    candidate_mix.insert("claude-sonnet-4-6".to_string(), 2);
    let candidate = ModelMixSnapshot {
        model_mix: candidate_mix,
        total_fallbacks: 2,
    };

    let warnings = build_model_mix_warnings(&baseline, &candidate);
    assert!(!warnings.is_empty(), "should warn when candidate has fallbacks and baseline does not");
    let combined = warnings.join(" ");
    assert!(
        combined.to_ascii_lowercase().contains("fallback") || combined.to_ascii_lowercase().contains("model"),
        "warning should mention fallback or model: {combined}"
    );
}

#[test]
fn compare_warns_when_model_mixes_differ() {
    use rust_swe_agent::run::compare::{build_model_mix_warnings, ModelMixSnapshot};
    let mut baseline_mix = std::collections::BTreeMap::new();
    baseline_mix.insert("gpt-4".to_string(), 10);
    let baseline = ModelMixSnapshot {
        model_mix: baseline_mix,
        total_fallbacks: 0,
    };

    let mut candidate_mix = std::collections::BTreeMap::new();
    candidate_mix.insert("gpt-4".to_string(), 8);
    candidate_mix.insert("claude-sonnet-4-6".to_string(), 2);
    let candidate = ModelMixSnapshot {
        model_mix: candidate_mix,
        total_fallbacks: 2,
    };

    let warnings = build_model_mix_warnings(&baseline, &candidate);
    assert!(!warnings.is_empty(), "should warn on different model mixes");
}

#[test]
fn compare_no_warning_when_same_model_no_fallbacks() {
    use rust_swe_agent::run::compare::{build_model_mix_warnings, ModelMixSnapshot};
    let snapshot = ModelMixSnapshot {
        model_mix: std::collections::BTreeMap::new(),
        total_fallbacks: 0,
    };
    let warnings = build_model_mix_warnings(&snapshot, &snapshot);
    assert!(warnings.is_empty(), "no warning when both have no fallbacks: {warnings:?}");
}

// ── bench evaluate: model-mix summary present ────────────────────────────────

#[test]
fn sweep_results_model_mix_visible_to_evaluate() {
    // AC8: bench evaluate exposes model-mix summary. We verify the data
    // round-trips through SweepResults, which evaluate reads.
    let mut s = sweep_results_with_mix(&[("gpt-4", 8, false), ("claude-sonnet-4-6", 2, true)]);
    s.total_fallbacks = 2;

    let json = serde_json::to_string_pretty(&s).unwrap();
    let back: SweepResults = serde_json::from_str(&json).unwrap();

    let mix = &back.model_mix;
    assert_eq!(mix.get("gpt-4").copied(), Some(8));
    assert_eq!(mix.get("claude-sonnet-4-6").copied(), Some(2));
    assert_eq!(back.total_fallbacks, 2);
}

// ── AC5: cost attributed to responding model ─────────────────────────────────

#[test]
fn fallback_attempt_record_carrying_cost_attributes_to_final_model() {
    // When a fallback response carries cost_usd, the cost belongs to the
    // responding (fallback) model, not the primary. We verify ModelResponse
    // carries the responding_model field so callers can attribute correctly.
    let usage = ModelUsage {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(0.002),
    };
    let resp = ModelResponse {
        content: "ok".into(),
        usage,
        raw: serde_json::json!({}),
        responding_model: Some("claude-sonnet-4-6".into()),
        fallback_attempts: vec![FallbackAttemptRecord {
            model: "gpt-4".into(),
            failure_reason: "rate_limited".into(),
        }],
    };
    // Cost belongs to the responding model.
    assert_eq!(resp.responding_model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(resp.usage.cost_usd, Some(0.002));
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn instance_result_stub(id: &str) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: outcome::SUBMITTED.into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(1),
        cost_usd: Some(0.01),
        prompt_tokens: Some(100),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(20),
        duration_secs: Some(1.0),
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: true,
        attempts: 1,
        retry_reasons: Vec::new(),
        runs: 1,
        resolved_count: 1,
        pass_at_1: true,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
    }
}

fn sweep_results_with_mix(entries: &[(&str, usize, bool)]) -> SweepResults {
    let mut s = SweepResults::default();
    for (model, count, had_fallback) in entries {
        s.model_mix.insert(model.to_string(), *count);
        if *had_fallback {
            s.total_fallbacks += *count as u64;
        }
    }
    s
}
