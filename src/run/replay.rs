//! `bench replay` runner — re-runs an agent against a recorded trajectory's
//! scripted responses and, when fingerprints are present, validates that the
//! agent is asking the model the same questions it asked originally.
//!
//! # Exit-code contract
//!
//! | Code | Meaning |
//! |------|---------|
//! | 0    | Replay completed without drift. |
//! | 9    | Prompt drift: at least one step's input fingerprint did not match. |
//! | 10   | Scripted responses exhausted before the agent finished. |
//! | 2    | Usage error: trajectory has no fingerprints and `--allow-unfingerprinted` was not passed. |
//! | 1    | Other internal error. |
//!
//! See `docs/spec-replay.md` for the full contract.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::agent::{Agent, DefaultAgent, default::DefaultAgentBuilder};
use crate::config::{Config, EnvKind};
#[cfg(feature = "docker")]
use crate::env::DockerEnvironment;
use crate::env::{Environment, LocalEnvironment};
use crate::error::{Error, ModelError};
use crate::fingerprint::{canonical_json, cap_canonical, compute_input_fingerprint};
use crate::model::{DeterministicModel, Message, Model, ModelResponse, QueryOpts};
use crate::redaction::Redactor;
use crate::trajectory::Trajectory;

/// Filename written to `output_dir` when drift is detected.
pub const DRIFT_REPORT_FILENAME: &str = "replay-drift.json";

/// Default byte cap applied to the unified diff string in each drift step.
pub const DEFAULT_DRIFT_CAP_BYTES: usize = 8 * 1024;

/// Byte cap applied when storing `input_canonical` in trajectory assistant messages.
/// Exported so `DefaultAgent` can reference a single source of truth.
pub const CANONICAL_CAP_BYTES: usize = 64 * 1024;

// ── public API ────────────────────────────────────────────────────────────────

pub struct ReplayArgs {
    pub trajectory_path: PathBuf,
    pub config: Config,
    pub output_dir: PathBuf,
    pub trajectory_name: Option<String>,
    /// When `true`, replaying a trajectory that has no fingerprints on its
    /// assistant messages emits a warning but succeeds. When `false` (default),
    /// an unfingerprinted trajectory causes a non-zero exit.
    pub allow_unfingerprinted: bool,
    /// When `true`, replay suppresses only prompt-drift failures, runs to
    /// completion, writes a full drift report, and exits 0. Configuration
    /// errors (unfingerprinted legacy, exhausted responses) are still propagated.
    pub report_only: bool,
    /// Maximum bytes of the unified diff string included per drift step.
    pub drift_cap_bytes: usize,
}

/// One step that exhibited fingerprint drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftStep {
    /// 0-based index of the model-query step where drift was detected.
    pub step_index: usize,
    /// Fingerprint stored in the cassette trajectory.
    pub recorded_fingerprint: String,
    /// Fingerprint computed from the actual messages during this replay.
    pub actual_fingerprint: String,
    /// Unified diff (`--- recorded` / `+++ actual`) of the pretty-printed
    /// canonical inputs, capped to `drift_cap_bytes` with a `[truncated]`
    /// marker if cut. Empty when no stored canonical is available.
    pub unified_diff: String,
    /// `true` if the diff was truncated **or** if the recorded canonical was
    /// already truncated when stored in the cassette (best-effort diff).
    pub diff_truncated: bool,
}

/// Structured drift report written to `{output_dir}/replay-drift.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftReport {
    pub steps: Vec<DriftStep>,
}

// ── cassette extraction ───────────────────────────────────────────────────────

struct CassetteEntry {
    response: String,
    fingerprint: Option<String>,
    canonical: Option<String>,
    canonical_truncated: bool,
}

type CassetteVecs = (
    Vec<String>,
    Vec<Option<String>>,
    Vec<Option<(String, bool)>>,
);

fn extract_cassette(trajectory: &Trajectory) -> Vec<CassetteEntry> {
    trajectory
        .messages
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| {
            let mc = m.extra.other.get("model_call");
            let fingerprint = mc
                .and_then(|v| v.get("input_fingerprint"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let canonical = mc
                .and_then(|v| v.get("input_canonical"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let canonical_truncated = mc
                .and_then(|v| v.get("input_canonical_truncated"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            CassetteEntry {
                response: m.content.clone(),
                fingerprint,
                canonical,
                canonical_truncated,
            }
        })
        .collect()
}

fn unzip_cassette(cassette: Vec<CassetteEntry>) -> CassetteVecs {
    cassette.into_iter().fold(
        (Vec::new(), Vec::new(), Vec::new()),
        |(mut rs, mut fps, mut cs), e| {
            let canon = e.canonical.map(|c| (c, e.canonical_truncated));
            rs.push(e.response);
            fps.push(e.fingerprint);
            cs.push(canon);
            (rs, fps, cs)
        },
    )
}

// ── main entry point ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
pub async fn run(args: ReplayArgs) -> Result<(), Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    // 1. Load original trajectory.
    let file_content = std::fs::read_to_string(&args.trajectory_path)?;
    let orig_trajectory: Trajectory = serde_json::from_str(&file_content).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "Invalid trajectory JSON: {e}"
        )))
    })?;

    // 2. Extract assistant messages — responses, expected fingerprints, recorded canonicals.
    let cassette = extract_cassette(&orig_trajectory);

    if cassette.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "No assistant messages found in trajectory to replay".into(),
        )));
    }

    let (responses, expected_fps, expected_canonicals) = unzip_cassette(cassette);

    // Validate fingerprint coverage before any environment setup so that legacy
    // cassettes are rejected immediately, without starting Docker containers or
    // connecting to MCP servers.
    if !args.allow_unfingerprinted {
        if let Some(pos) = expected_fps.iter().position(Option::is_none) {
            return Err(Error::Model(ModelError::ReplayUnfingerprintedLegacy(pos)));
        }
    }

    let task = orig_trajectory
        .info
        .task
        .unwrap_or_else(|| "Replayed Task".into());
    let traj_name = args
        .trajectory_name
        .unwrap_or_else(|| crate::run::mini::slugify(&task));

    // 3. Build fingerprint-checking model.
    // A dedicated Redactor (same config as the agent's) applies TRAJECTORY
    // redaction before fingerprinting — needed so secrets still present in
    // extra_context/skills at replay time are hashed identically to how they
    // were hashed during recording.
    let fp_redactor = Redactor::from_config(&args.config.root.redaction).map_err(|err| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "Invalid redaction config: {err}"
        )))
    })?;
    let drift_steps: Arc<Mutex<Vec<DriftStep>>> = Arc::new(Mutex::new(Vec::new()));
    let model: Arc<dyn Model> = Arc::new(FingerprintCheckingModel {
        inner: DeterministicModel::new(responses),
        redactor: fp_redactor,
        expected_fps,
        expected_canonicals,
        step: Mutex::new(0),
        allow_unfingerprinted: args.allow_unfingerprinted,
        report_only: args.report_only,
        drift_cap_bytes: args.drift_cap_bytes,
        drift_steps: Arc::clone(&drift_steps),
    });

    // 4. Build environment and agent.
    let env = build_env(&args.config).await?;
    let tool_providers = crate::tool::discover_mcp_servers(
        env.as_ref(),
        &args.config.root.agent.mcp_servers,
        args.config.root.agent.tool_hook_timeout_secs,
        None,
    )
    .await?;

    let resolved_skills = crate::skills::resolve_for_task(&args.config.root.skills, &task, None)?;

    let mut replay_config = args.config.clone();
    replay_config.root.agent.detect_stagnation = false;
    let mut agent: DefaultAgent = DefaultAgentBuilder {
        config: replay_config,
        model,
        env,
        task,
        extra_context: resolved_skills.merged_extra_context,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build_with_tool_providers(tool_providers)?;
    resolved_skills
        .active_skills
        .record_redacted_provenance(&mut agent.trajectory.info, &agent.redactor)?;

    // 5. Run the replay.
    tracing::info!(?args.trajectory_path, "starting replay mode");
    let run_result = agent.run().await;

    // 6. Collect any drift steps recorded during the run.
    let collected = {
        drift_steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    };

    // 7. Write drift report when there is drift; remove any stale report when
    //    there is none, so reused CI output directories don't show old results.
    let drift_report_path = args.output_dir.join(DRIFT_REPORT_FILENAME);
    if !collected.is_empty() {
        let report = DriftReport { steps: collected };
        write_drift_report(&args.output_dir, &report)?;
    } else if drift_report_path.exists() {
        std::fs::remove_file(&drift_report_path)?;
    }

    // 8. In --report-only mode suppress only prompt-drift failures; propagate
    //    all other errors (usage errors, exhausted responses, I/O failures, …)
    //    so operators see an honest exit code for structural problems.
    if args.report_only {
        let is_drift_only = matches!(&run_result, Err(Error::Model(ModelError::ReplayDrift(_))));
        if is_drift_only || run_result.is_ok() {
            if let Ok(ref exit) = run_result {
                save_outputs(&agent, &args.output_dir, &traj_name, exit)?;
            } else {
                let traj_path = args.output_dir.join(format!("{traj_name}.traj.json"));
                agent.trajectory.save_pretty(&traj_path)?;
            }
            return Ok(());
        }
        // Non-drift error: fall through and propagate below.
    }

    // 9. Fail-fast mode (or report-only with a non-drift error): propagate.
    let exit = run_result?;
    save_outputs(&agent, &args.output_dir, &traj_name, &exit)?;
    tracing::info!("replay trajectory written");
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn save_outputs(
    agent: &DefaultAgent,
    output_dir: &Path,
    traj_name: &str,
    exit: &crate::agent::ExitReason,
) -> Result<(), Error> {
    let traj_path = output_dir.join(format!("{traj_name}.traj.json"));
    agent.trajectory.save_pretty(&traj_path)?;

    if let crate::agent::ExitReason::Submitted { final_output } = exit {
        let out_path = output_dir.join(format!("{traj_name}.output.txt"));
        std::fs::write(out_path, final_output)?;
    }
    Ok(())
}

fn write_drift_report(output_dir: &Path, report: &DriftReport) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(report)?;
    let path = output_dir.join(DRIFT_REPORT_FILENAME);
    std::fs::write(&path, json)?;

    eprintln!(
        "replay-drift: {} step(s) with prompt divergence",
        report.steps.len()
    );
    for s in &report.steps {
        eprintln!(
            "  step {}: recorded={} actual={}",
            s.step_index, s.recorded_fingerprint, s.actual_fingerprint
        );
    }
    Ok(())
}

/// Pretty-print a compact canonical JSON string for human-readable diffing.
/// Falls back to the raw string if parsing fails.
fn pretty_canonical(compact: &str) -> String {
    serde_json::from_str::<serde_json::Value>(compact)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| compact.to_owned())
}

/// Produce a unified diff (`--- recorded` / `+++ actual`) between two
/// pretty-printed canonical strings, capped to `cap` bytes.
///
/// The header is only written when there are actual differences.
/// `recorded_was_truncated` causes `diff_truncated` to be `true` even if the
/// diff itself fits within `cap`.
fn make_unified_diff(
    recorded: &str,
    actual: &str,
    recorded_was_truncated: bool,
    cap: usize,
) -> (String, bool) {
    let recorded_pretty = pretty_canonical(recorded);
    let actual_pretty = pretty_canonical(actual);

    let diff = TextDiff::from_lines(&recorded_pretty, &actual_pretty);
    let mut out = String::new();
    let mut header_written = false;
    for group in diff.grouped_ops(3) {
        if !header_written {
            out.push_str("--- recorded\n+++ actual\n");
            header_written = true;
        }
        for op in &group {
            for change in diff.iter_changes(op) {
                let prefix = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                out.push_str(prefix);
                out.push_str(change.value());
                if change.missing_newline() {
                    out.push('\n');
                }
            }
        }
    }

    let (capped, diff_cap_hit) = cap_canonical(&out, cap);
    (capped, diff_cap_hit || recorded_was_truncated)
}

// ── FingerprintCheckingModel ──────────────────────────────────────────────────

/// Wraps `DeterministicModel` and enforces fingerprint integrity before each
/// scripted response is consumed.
struct FingerprintCheckingModel {
    inner: DeterministicModel,
    /// Redactor used to apply TRAJECTORY-surface redaction to messages before
    /// fingerprinting — mirrors the same redaction applied on the recording side
    /// so that raw secrets in extra_context/skills produce the same canonical
    /// form as they did when the trajectory was recorded.
    redactor: Redactor,
    /// Stored fingerprints from the cassette trajectory, one per assistant
    /// message in order. `None` means the original was recorded without
    /// fingerprints (legacy trajectory).
    expected_fps: Vec<Option<String>>,
    /// Stored canonical JSON from the cassette, paired with a truncation flag.
    /// `None` means the original was recorded without canonical storage (legacy).
    expected_canonicals: Vec<Option<(String, bool)>>,
    step: Mutex<usize>,
    allow_unfingerprinted: bool,
    report_only: bool,
    drift_cap_bytes: usize,
    drift_steps: Arc<Mutex<Vec<DriftStep>>>,
}

#[async_trait]
impl Model for FingerprintCheckingModel {
    fn name(&self) -> &'static str {
        "fingerprint-checking-replay"
    }

    fn skip_latency_telemetry(&self) -> bool {
        // Replay drives a `DeterministicModel` for responses; the wall-clock
        // here reflects fingerprint-checking + scripted lookup, not real
        // model latency. Omit `model_latency_ms` so inspect/evaluate can't
        // mistake replay runs for fast real-model runs (#159).
        true
    }

    async fn query(
        &self,
        messages: &[Message],
        opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        let step = *self
            .step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Mirror the recording-side fingerprinting: apply TRAJECTORY-surface
        // redaction first (so raw secrets in extra_context/skills hash the same
        // as the recorded [REDACTED:...] markers), then normalize the per-run
        // salt from every marker so hashes are stable across runs.
        // Use redact_text_scratch so this pass does not inflate the run's
        // redaction-count telemetry.
        let normalized: Vec<Message> = messages
            .iter()
            .map(|m| {
                let mut m2 = m.clone();
                let redacted = self.redactor.redact_text_scratch(&m.content);
                m2.content = crate::fingerprint::normalize_redaction_markers(&redacted);
                m2
            })
            .collect();
        let actual_fp = compute_input_fingerprint(&normalized);

        match self.expected_fps.get(step) {
            Some(None) => {
                // Legacy trajectory — no fingerprint stored.
                if !self.allow_unfingerprinted {
                    return Err(ModelError::ReplayUnfingerprintedLegacy(step));
                }
                tracing::warn!(
                    step,
                    "replaying step without stored fingerprint (--allow-unfingerprinted active)"
                );
            }
            Some(Some(expected)) if actual_fp.hex != *expected => {
                // Fingerprint mismatch — prompt drift.
                let actual_canonical = canonical_json(&normalized);
                let (recorded_canonical_raw, recorded_was_truncated) = self
                    .expected_canonicals
                    .get(step)
                    .and_then(Option::as_ref)
                    .map_or(("", false), |(c, t)| (c.as_str(), *t));
                // The stored canonical uses hashed markers ([REDACTED:k:s:HASH]).
                // Normalize it before diffing so per-run salts don't appear as
                // spurious differences in the drift report.
                let recorded_canonical_normalized =
                    crate::fingerprint::normalize_redaction_markers(recorded_canonical_raw);

                let (unified_diff, diff_truncated) = make_unified_diff(
                    &recorded_canonical_normalized,
                    &actual_canonical,
                    recorded_was_truncated,
                    self.drift_cap_bytes,
                );

                {
                    let mut guard = self
                        .drift_steps
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(DriftStep {
                        step_index: step,
                        recorded_fingerprint: expected.clone(),
                        actual_fingerprint: actual_fp.hex.clone(),
                        unified_diff,
                        diff_truncated,
                    });
                }

                if !self.report_only {
                    // Fail-fast: do NOT consume the scripted response.
                    return Err(ModelError::ReplayDrift(step));
                }
                // Report-only: fall through and consume the response.
            }
            Some(Some(_)) | None => {
                // Fingerprint matches or step is out of expected range;
                // proceed normally (inner model handles out-of-range via
                // ScriptedResponsesExhausted).
            }
        }

        // Advance step counter only after the fingerprint check passes.
        *self
            .step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = step + 1;

        // Translate the generic scripted-model exhaustion into the
        // replay-specific variant so that `ExitCode::from_error` maps it to
        // `ReplayResponseExhausted` (10) only in replay context.
        self.inner.query(messages, opts).await.map_err(|e| match e {
            ModelError::ResponsesExhausted(n) => ModelError::ScriptedResponsesExhausted(n),
            other => other,
        })
    }
}

// ── environment helpers ───────────────────────────────────────────────────────

async fn build_env(cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    match cfg.root.environment.kind {
        EnvKind::Local => Ok(Box::new(LocalEnvironment::new())),
        EnvKind::Docker => build_docker_env(cfg).await,
    }
}

#[cfg(feature = "docker")]
async fn build_docker_env(cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    let image = cfg.root.environment.docker_image.clone().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "environment.kind=docker requires environment.docker_image".into(),
        ))
    })?;
    let wd = PathBuf::from(cfg.root.environment.workdir.clone());
    let env = DockerEnvironment::start(image, wd).await?;
    Ok(Box::new(env))
}

#[cfg(not(feature = "docker"))]
#[allow(clippy::unused_async)]
async fn build_docker_env(_cfg: &Config) -> Result<Box<dyn Environment>, Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "docker support not compiled in — rebuild with --features docker".into(),
    )))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::MessageExtra;
    use crate::trajectory::MessageRecord;
    use tempfile::tempdir;

    fn make_simple_traj() -> Trajectory {
        Trajectory {
            trajectory_format: "mini-swe-agent-1.1".into(),
            info: crate::trajectory::TrajectoryInfo {
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
            fork_lineage: None,
        }
    }

    #[tokio::test]
    async fn test_replay_mode_allow_unfingerprinted() {
        let dir = tempdir().unwrap();
        let traj = make_simple_traj();
        let dummy_path = dir.path().join("input.traj.json");
        traj.save_pretty(&dummy_path).unwrap();

        let cfg = Config::defaults().unwrap();
        let args = ReplayArgs {
            trajectory_path: dummy_path,
            config: cfg,
            output_dir: dir.path().to_path_buf(),
            trajectory_name: Some("replayed-run".into()),
            allow_unfingerprinted: true,
            report_only: false,
            drift_cap_bytes: DEFAULT_DRIFT_CAP_BYTES,
        };

        run(args).await.unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();

        assert!(
            entries
                .iter()
                .any(|e| e.file_name().to_string_lossy() == "replayed-run.traj.json")
        );
        assert!(
            entries
                .iter()
                .any(|e| e.file_name().to_string_lossy() == "replayed-run.output.txt")
        );
        assert!(!dir.path().join(DRIFT_REPORT_FILENAME).exists());
    }

    #[tokio::test]
    async fn clean_replay_removes_stale_drift_report() {
        let dir = tempdir().unwrap();
        let traj = make_simple_traj();
        let dummy_path = dir.path().join("input.traj.json");
        traj.save_pretty(&dummy_path).unwrap();

        // Pre-plant a stale drift report from a previous run.
        let stale_path = dir.path().join(DRIFT_REPORT_FILENAME);
        std::fs::write(&stale_path, r#"{"steps":[]}"#).unwrap();
        assert!(stale_path.exists(), "precondition: stale report exists");

        let cfg = Config::defaults().unwrap();
        let args = ReplayArgs {
            trajectory_path: dummy_path,
            config: cfg,
            output_dir: dir.path().to_path_buf(),
            trajectory_name: Some("clean-run".into()),
            allow_unfingerprinted: true,
            report_only: false,
            drift_cap_bytes: DEFAULT_DRIFT_CAP_BYTES,
        };
        run(args).await.unwrap();

        assert!(
            !stale_path.exists(),
            "clean replay must remove the stale drift report"
        );
    }

    #[test]
    fn make_unified_diff_shows_changes() {
        let recorded = r#"[{"content":"hello","role":"user"}]"#;
        let actual = r#"[{"content":"world","role":"user"}]"#;
        let (diff, truncated) = make_unified_diff(recorded, actual, false, DEFAULT_DRIFT_CAP_BYTES);
        assert!(!truncated);
        assert!(
            diff.starts_with("--- recorded\n+++ actual\n"),
            "diff must start with header"
        );
        assert!(diff.contains("hello"), "diff must show removed text");
        assert!(diff.contains("world"), "diff must show added text");
    }

    #[test]
    fn make_unified_diff_identical_inputs_is_empty() {
        let canon = r#"[{"content":"hi","role":"user"}]"#;
        let (diff, truncated) = make_unified_diff(canon, canon, false, DEFAULT_DRIFT_CAP_BYTES);
        assert!(!truncated);
        // All lines are equal — grouped_ops(3) produces no hunks for identical input.
        assert!(diff.is_empty(), "no diff expected for identical inputs");
    }

    #[test]
    fn make_unified_diff_truncates_when_over_cap() {
        let long = "x".repeat(100);
        let recorded = format!(r#"[{{"content":"{long}","role":"user"}}]"#);
        let actual = format!(r#"[{{"content":"{long}z","role":"user"}}]"#);
        let (diff, truncated) = make_unified_diff(&recorded, &actual, false, 10);
        assert!(truncated, "diff must be marked truncated when over cap");
        assert!(diff.ends_with("[truncated]"));
    }

    #[test]
    fn make_unified_diff_recorded_truncated_flag_propagates() {
        let canon = r#"[{"content":"hi","role":"user"}]"#;
        let (_, truncated) = make_unified_diff(canon, canon, true, DEFAULT_DRIFT_CAP_BYTES);
        assert!(
            truncated,
            "diff_truncated must be true when recorded canonical was truncated"
        );
    }
}
