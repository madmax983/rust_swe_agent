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

use crate::agent::{Agent, DefaultAgent, default::DefaultAgentBuilder};
use crate::config::{Config, EnvKind};
#[cfg(feature = "docker")]
use crate::env::DockerEnvironment;
use crate::env::{Environment, LocalEnvironment};
use crate::error::{Error, ModelError};
use crate::fingerprint::{canonical_json, compute_input_fingerprint};
use crate::model::{DeterministicModel, Message, Model, ModelResponse, QueryOpts};
use crate::trajectory::Trajectory;

/// Filename written to `output_dir` when drift is detected.
pub const DRIFT_REPORT_FILENAME: &str = "replay-drift.json";

/// Default byte cap for the actual-canonical snippet in a drift report.
pub const DEFAULT_DRIFT_CAP_BYTES: usize = 8 * 1024;

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
    /// When `true`, replay runs to completion despite any fingerprint drift,
    /// writes a full drift report, and exits 0. When `false` (default), replay
    /// stops at the first drift and exits with code 9.
    pub report_only: bool,
    /// Maximum bytes of the actual canonical JSON to include per drift step.
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
    /// First `drift_cap_bytes` of the actual canonical input JSON.
    pub actual_canonical_snippet: String,
    /// `true` if `actual_canonical_snippet` was truncated.
    pub truncated: bool,
}

/// Structured drift report written to `{output_dir}/replay-drift.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftReport {
    pub steps: Vec<DriftStep>,
}

// ── main entry point ──────────────────────────────────────────────────────────

pub async fn run(args: ReplayArgs) -> Result<(), Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    // 1. Load original trajectory.
    let file_content = std::fs::read_to_string(&args.trajectory_path)?;
    let orig_trajectory: Trajectory = serde_json::from_str(&file_content).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "Invalid trajectory JSON: {e}"
        )))
    })?;

    // 2. Extract assistant messages — responses + expected fingerprints.
    let (responses, expected_fps): (Vec<String>, Vec<Option<String>>) = orig_trajectory
        .messages
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| {
            let content = m.content.clone();
            let fp = m
                .extra
                .other
                .get("model_call")
                .and_then(|mc| mc.get("input_fingerprint"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            (content, fp)
        })
        .unzip();

    if responses.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "No assistant messages found in trajectory to replay".into(),
        )));
    }

    let task = orig_trajectory
        .info
        .task
        .unwrap_or_else(|| "Replayed Task".into());
    let traj_name = args
        .trajectory_name
        .unwrap_or_else(|| crate::run::mini::slugify(&task));

    // 3. Build fingerprint-checking model.
    let drift_steps: Arc<Mutex<Vec<DriftStep>>> = Arc::new(Mutex::new(Vec::new()));
    let model: Arc<dyn Model> = Arc::new(FingerprintCheckingModel {
        inner: DeterministicModel::new(responses),
        expected_fps,
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

    let mut agent: DefaultAgent = DefaultAgentBuilder {
        config: args.config.clone(),
        model,
        env,
        task,
        extra_context: resolved_skills.merged_extra_context,
        renderer: None,
        stream: None,
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

    // 7. Write drift report to disk and stderr when there is drift.
    if !collected.is_empty() {
        let report = DriftReport { steps: collected };
        write_drift_report(&args.output_dir, &report)?;
    }

    // 8. In --report-only mode always exit 0 (drift was collected above).
    if args.report_only {
        // Best-effort trajectory save even when the run had errors.
        if let Ok(ref exit) = run_result {
            save_outputs(&agent, &args.output_dir, &traj_name, exit)?;
        } else {
            // Save whatever trajectory we managed to produce.
            let traj_path = args.output_dir.join(format!("{traj_name}.traj.json"));
            agent.trajectory.save_pretty(&traj_path)?;
        }
        return Ok(());
    }

    // 9. Fail-fast mode: propagate errors from the run.
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

fn truncate_canonical(canonical: &str, cap: usize) -> (String, bool) {
    if canonical.len() <= cap {
        return (canonical.to_owned(), false);
    }
    let mut end = cap;
    while end > 0 && !canonical.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!("{}[truncated]", &canonical[..end]),
        true,
    )
}

// ── FingerprintCheckingModel ──────────────────────────────────────────────────

/// Wraps `DeterministicModel` and enforces fingerprint integrity before each
/// scripted response is consumed.
struct FingerprintCheckingModel {
    inner: DeterministicModel,
    /// Stored fingerprints from the cassette trajectory, one per assistant
    /// message in order. `None` means the original was recorded without
    /// fingerprints (legacy trajectory).
    expected_fps: Vec<Option<String>>,
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

    async fn query(
        &self,
        messages: &[Message],
        opts: &QueryOpts,
    ) -> Result<ModelResponse, ModelError> {
        let step = *self
            .step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let actual_fp = compute_input_fingerprint(messages);

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
                let canonical = canonical_json(messages);
                let (snippet, truncated) =
                    truncate_canonical(&canonical, self.drift_cap_bytes);
                {
                    let mut guard = self
                        .drift_steps
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(DriftStep {
                        step_index: step,
                        recorded_fingerprint: expected.clone(),
                        actual_fingerprint: actual_fp.hex.clone(),
                        actual_canonical_snippet: snippet,
                        truncated,
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

        self.inner.query(messages, opts).await
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

        assert!(entries
            .iter()
            .any(|e| e.file_name().to_string_lossy() == "replayed-run.traj.json"));
        assert!(entries
            .iter()
            .any(|e| e.file_name().to_string_lossy() == "replayed-run.output.txt"));
        assert!(!dir.path().join(DRIFT_REPORT_FILENAME).exists());
    }

    #[test]
    fn truncate_canonical_under_cap_is_unchanged() {
        let (s, truncated) = truncate_canonical("hello", 10);
        assert_eq!(s, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_canonical_over_cap_adds_marker() {
        let (s, truncated) = truncate_canonical("hello world", 5);
        assert!(s.ends_with("[truncated]"));
        assert!(truncated);
    }
}
