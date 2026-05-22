//! `bench fork` runner — re-runs a prefix from a parent trajectory under $0 cost
//! and prompt/drift verification, then switches to live model calls at step N.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sha2::Digest;

use crate::agent::{Agent, DefaultAgent, default::DefaultAgentBuilder};
use crate::cli::args::ForkCmd;
use crate::config::{Config, EnvKind, McpServerCfg};
#[cfg(feature = "docker")]
use crate::env::DockerEnvironment;
use crate::env::{Environment, LocalEnvironment};
use crate::error::{Error, ModelError};
use crate::fingerprint::{canonical_json, cap_canonical, compute_input_fingerprint};
use crate::model::litellm::LitellmBackend;
use crate::model::{
    DeterministicModel, FallbackModel, Message, Model, ModelResponse, ModelUsage, QueryOpts,
};
use crate::redaction::Redactor;
use crate::run::reproduce::{
    DriftSeverity, compare_manifests, filter_hard_drifts, load_manifest_from_sweep,
};
use crate::trajectory::{ForkLineage, Trajectory};

/// Hybrid model that replays a prefix using deterministic responses with
/// fingerprint verification at strictly $0 cost, and delegates to a live
/// backend model starting from step N.
pub struct ForkingModel {
    pub inner_deterministic: DeterministicModel,
    pub live_model: Arc<dyn Model>,
    pub fork_step: usize,
    pub redactor: Redactor,
    /// Stored fingerprints from the parent trajectory.
    pub expected_fps: Vec<Option<String>>,
    /// Stored canonical JSON from the parent trajectory.
    pub expected_canonicals: Vec<Option<(String, bool)>>,
    pub step: Mutex<usize>,
    pub allow_unfingerprinted: bool,
    pub drift_cap_bytes: usize,
    pub drift_steps: Arc<Mutex<Vec<crate::run::replay::DriftStep>>>,
}

#[async_trait]
impl Model for ForkingModel {
    fn name(&self) -> &'static str {
        "forking-model"
    }

    fn skip_latency_telemetry(&self) -> bool {
        // Since we are hybrid, we let telemetry reflect the live backend
        // starting from step N, but the deterministic replay prefix doesn't have latency.
        false
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

        if step < self.fork_step {
            // Apply prompt fingerprint check during the prefix phase
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
                    if !self.allow_unfingerprinted {
                        return Err(ModelError::ReplayUnfingerprintedLegacy(step));
                    }
                    tracing::warn!(
                        step,
                        "fork prefix: replaying step without stored fingerprint (--allow-unfingerprinted active)"
                    );
                }
                Some(Some(expected)) if actual_fp.hex != *expected => {
                    // Fingerprint mismatch — prompt drift!
                    let actual_canonical = canonical_json(&normalized);
                    let (recorded_canonical_raw, recorded_was_truncated) = self
                        .expected_canonicals
                        .get(step)
                        .and_then(Option::as_ref)
                        .map_or(("", false), |(c, t)| (c.as_str(), *t));
                    let recorded_canonical_normalized =
                        crate::fingerprint::normalize_redaction_markers(recorded_canonical_raw);

                    let (unified_diff, diff_truncated) = make_unified_diff_for_fork(
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
                        guard.push(crate::run::replay::DriftStep {
                            step_index: step,
                            recorded_fingerprint: expected.clone(),
                            actual_fingerprint: actual_fp.hex.clone(),
                            unified_diff,
                            diff_truncated,
                        });
                    }

                    // For fork, prompt drift mismatch at/before step N-1 causes immediate exit 9.
                    return Err(ModelError::ReplayDrift(step));
                }
                _ => {}
            }

            // Advance step counter only after checking fingerprint.
            *self
                .step
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = step + 1;

            let mut resp = self
                .inner_deterministic
                .query(messages, opts)
                .await
                .map_err(|e| match e {
                    ModelError::ResponsesExhausted(n) => ModelError::ScriptedResponsesExhausted(n),
                    other => other,
                })?;

            // Strictly $0 cost attribute to the prefix.
            resp.usage = ModelUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_usd: Some(0.0),
            };

            Ok(resp)
        } else {
            // Live phase! Delegate to the live model.
            *self
                .step
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = step + 1;

            self.live_model.query(messages, opts).await
        }
    }
}

fn make_unified_diff_for_fork(
    recorded: &str,
    actual: &str,
    recorded_was_truncated: bool,
    cap: usize,
) -> (String, bool) {
    let recorded_pretty = pretty_canonical(recorded);
    let actual_pretty = pretty_canonical(actual);

    let diff = similar::TextDiff::from_lines(&recorded_pretty, &actual_pretty);
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
                    similar::ChangeTag::Delete => "-",
                    similar::ChangeTag::Insert => "+",
                    similar::ChangeTag::Equal => " ",
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

fn pretty_canonical(compact: &str) -> String {
    serde_json::from_str::<serde_json::Value>(compact)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| compact.to_owned())
}

// Extract cassette entry helper (similar to replay.rs)
struct CassetteEntry {
    response: String,
    fingerprint: Option<String>,
    canonical: Option<String>,
    canonical_truncated: bool,
}

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

#[allow(clippy::too_many_lines, clippy::unwrap_used, clippy::cast_possible_truncation)]
pub async fn run(args: ForkCmd) -> Result<(), Error> {
    // 1. Locate and parse the parent trajectory.
    let (parent_trajectory, parent_trajectory_sha256) = {
        let single_candidates = [
            args.sweep.join(format!("{}.traj.json", args.instance)),
            args.sweep.join(&args.instance).join("trajectory.json"),
            args.sweep
                .join("trajectories")
                .join(format!("{}.traj.json", args.instance)),
        ];
        let mut found = None;
        for path in &single_candidates {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(traj) = serde_json::from_slice::<Trajectory>(&bytes) {
                    found = Some((traj, bytes));
                    break;
                }
            }
        }

        let (traj, bytes) = found.ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "No trajectory found for instance {} under sweep directory {}",
                args.instance,
                args.sweep.display()
            )))
        })?;

        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        let hash = hasher.finalize();
        let mut sha256 = String::with_capacity(64);
        for b in hash {
            use std::fmt::Write as _;
            let _ = write!(sha256, "{b:02x}");
        }
        (traj, sha256)
    };

    // Extract expected fingerprints and scripted responses from cassette
    let cassette = extract_cassette(&parent_trajectory);
    let parent_steps = cassette.len() as u32;

    // Validate bounds:
    if args.from_step >= parent_steps {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "Fork step {} is out of bounds (parent trajectory has only {} steps)",
            args.from_step, parent_steps
        ))));
    }

    let mut responses = Vec::new();
    let mut expected_fps = Vec::new();
    let mut expected_canonicals = Vec::new();

    for (i, entry) in cassette.into_iter().enumerate() {
        if (i as u32) < args.from_step {
            responses.push(entry.response);
            expected_fps.push(entry.fingerprint);
            expected_canonicals.push(entry.canonical.map(|c| (c, entry.canonical_truncated)));
        } else {
            break;
        }
    }

    if !args.allow_unfingerprinted {
        if let Some(pos) = expected_fps.iter().position(Option::is_none) {
            return Err(Error::Model(ModelError::ReplayUnfingerprintedLegacy(pos)));
        }
    }

    // 2. Perform environment-drift checks from reproducible sweeps.
    let drift_records = if let Ok(source_manifest) = load_manifest_from_sweep(&args.sweep) {
        let mut current_manifest = source_manifest.clone();
        current_manifest.harness.git_sha = current_git_sha();
        current_manifest.harness.git_dirty = None;
        current_manifest.runtime.started_at_utc = chrono::Utc::now().to_rfc3339();
        current_manifest.runtime.finished_at_utc = None;
        current_manifest.runtime.host_os = std::env::consts::OS.into();
        current_manifest.runtime.rust_version = current_rust_version();

        let all_drifts = compare_manifests(&source_manifest, &current_manifest);

        // Warn on soft drifts
        for d in all_drifts
            .iter()
            .filter(|d| d.severity == DriftSeverity::Soft)
        {
            tracing::warn!(field = %d.field, "fork environment soft drift: {}", d.message);
        }

        // Abort on unwhitelisted hard drifts. Since there's no CLI override for white-listing drifts here,
        // we block blocking drifts.
        let blocking = filter_hard_drifts(&all_drifts, &[]);
        if !blocking.is_empty() {
            let reasons: Vec<String> = blocking.iter().map(|d| d.message.clone()).collect();
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "fork: hard environment drift detected:\n  {}",
                reasons.join("\n  ")
            ))));
        }

        all_drifts
    } else {
        Vec::new()
    };

    // 3. Build the configuration for the agent, incorporating the tail overrides.
    let mut config = Config::defaults()?;

    // Config overlays:
    if let Some(ref model_override) = args.model {
        config.root.model.name = model_override.clone();
    } else if let Some(ref parent_model) = parent_trajectory.info.model_name {
        config.root.model.name = parent_model.clone();
    }

    if let Some(ref prompt_file) = args.system_prompt_file {
        let content = std::fs::read_to_string(prompt_file)?;
        config.root.prompts.system = content;
    } else {
        // Fallback to default or empty if not set
    }

    if let Some(limit) = args.step_limit {
        config.root.agent.step_limit = limit;
    } else {
        config.root.agent.step_limit = parent_steps.max(50);
    }

    if let Some(budget) = args.per_task_budget_usd {
        config.root.agent.per_task_budget_usd = Some(budget);
    }

    if !args.mcp_servers.is_empty() {
        config.root.agent.mcp_servers = args
            .mcp_servers
            .iter()
            .map(|cmd| McpServerCfg {
                command: cmd.clone(),
                timeout_secs: None,
            })
            .collect();
    } else if let Some(ref mcp_config_path) = args.mcp_config {
        // Standard MCP config JSON loader compatibility if specified
        if let Ok(text) = std::fs::read_to_string(mcp_config_path) {
            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&text) {
                if let Some(serde_json::Value::Object(mcp_servers)) = map.get("mcpServers") {
                    let mut servers = Vec::new();
                    for (_name, val) in mcp_servers {
                        if let Some(cmd) = val.get("command").and_then(|c| c.as_str()) {
                            servers.push(McpServerCfg {
                                command: cmd.to_owned(),
                                timeout_secs: None,
                            });
                        }
                    }
                    config.root.agent.mcp_servers = servers;
                }
            }
        }
    }

    // 4. Construct the ForkingModel.
    let fp_redactor = Redactor::from_config(&config.root.redaction).map_err(|err| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "Invalid redaction config: {err}"
        )))
    })?;

    let live_model = build_live_model(&config);
    let drift_steps = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(ForkingModel {
        inner_deterministic: DeterministicModel::new(responses),
        live_model,
        fork_step: args.from_step as usize,
        redactor: fp_redactor,
        expected_fps,
        expected_canonicals,
        step: Mutex::new(0),
        allow_unfingerprinted: args.allow_unfingerprinted,
        drift_cap_bytes: crate::run::replay::DEFAULT_DRIFT_CAP_BYTES,
        drift_steps: Arc::clone(&drift_steps),
    });

    // 5. Setup the execution environment and Agent.
    let env = build_env(&config).await?;
    let task = parent_trajectory
        .info
        .task
        .clone()
        .unwrap_or_else(|| "Forked task".to_owned());

    let tool_providers = crate::tool::discover_mcp_servers(
        env.as_ref(),
        &config.root.agent.mcp_servers,
        config.root.agent.tool_hook_timeout_secs,
        None,
    )
    .await?;

    let resolved_skills = crate::skills::resolve_for_task(&config.root.skills, &task, None)?;

    let mut agent: DefaultAgent = DefaultAgentBuilder {
        config: config.clone(),
        model,
        env,
        task: task.clone(),
        extra_context: resolved_skills.merged_extra_context,
        renderer: None,
        stream: None,
        resume_from: None,
    }
    .build_with_tool_providers(tool_providers)?;

    resolved_skills
        .active_skills
        .record_redacted_provenance(&mut agent.trajectory.info, &agent.redactor)?;

    // Start with empty/zeroed initial cost because prefix is strictly $0 cost
    agent.trajectory.info.actual_cost_usd = Some(0.0);
    agent.trajectory.info.total_cost_usd = Some(0.0);

    // Save outputs target
    let traj_name = format!("{}-fork", args.instance);
    std::fs::create_dir_all(&args.output)?;
    let checkpoint_path = args.output.join(format!("{traj_name}.traj.json"));
    agent.checkpoint_path = Some(checkpoint_path.clone());

    // 6. Run the agent.
    tracing::info!("starting bench fork at step {}", args.from_step);
    let run_result = agent.run().await;

    // Report if any drift steps occurred during the deterministic prefix (though we exit 9 early if mismatch occurs)
    let collected_drifts = drift_steps.lock().unwrap().clone();
    if !collected_drifts.is_empty() {
        tracing::warn!("Replay drift detected during deterministic prefix phase");
    }

    // 7. Embed the ForkLineage block.
    let mut tail_overrides = std::collections::BTreeMap::new();
    if let Some(ref m) = args.model {
        tail_overrides.insert("model".to_owned(), serde_json::Value::String(m.clone()));
    }
    if let Some(ref p) = args.system_prompt_file {
        tail_overrides.insert(
            "system_prompt_file".to_owned(),
            serde_json::Value::String(p.to_string_lossy().into_owned()),
        );
    }
    if let Some(lim) = args.step_limit {
        tail_overrides.insert(
            "step_limit".to_owned(),
            serde_json::Value::Number(lim.into()),
        );
    }
    if let Some(budget) = args.per_task_budget_usd {
        tail_overrides.insert(
            "per_task_budget_usd".to_owned(),
            serde_json::Value::Number(serde_json::Number::from_f64(budget).unwrap()),
        );
    }
    if !args.mcp_servers.is_empty() {
        tail_overrides.insert(
            "mcp_servers".to_owned(),
            serde_json::Value::Array(
                args.mcp_servers
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(ref p) = args.mcp_config {
        tail_overrides.insert(
            "mcp_config".to_owned(),
            serde_json::Value::String(p.to_string_lossy().into_owned()),
        );
    }

    let lineage = ForkLineage {
        parent_sweep_path: args.sweep.to_string_lossy().into_owned(),
        parent_instance_id: args.instance.clone(),
        parent_trajectory_sha256,
        fork_step: args.from_step,
        tail_overrides,
    };

    agent.trajectory.fork_lineage = Some(lineage);

    // Embed environmental drifts in the `provenance` metadata block if present
    if !drift_records.is_empty() {
        agent.trajectory.info.other.insert(
            "provenance".to_owned(),
            serde_json::to_value(&drift_records).unwrap_or(serde_json::Value::Null),
        );
    }

    // Check exit reason and handle errors
    let exit_reason = run_result?;

    // Save final results
    let final_traj_path = args.output.join(format!("{traj_name}.traj.json"));
    agent.trajectory.save_pretty(&final_traj_path)?;

    if let crate::agent::ExitReason::Submitted { final_output } = &exit_reason {
        let out_path = args.output.join(format!("{traj_name}.output.txt"));
        std::fs::write(out_path, final_output)?;
    }

    tracing::info!(
        "bench fork run completed successfully; trajectory saved to {}",
        final_traj_path.display()
    );
    Ok(())
}

fn build_live_model(cfg: &Config) -> Arc<dyn Model> {
    let primary =
        LitellmBackend::new(cfg.root.model.name.clone()).with_max_tokens(cfg.root.model.max_tokens);
    if cfg.root.model.fallback_models.is_empty() {
        return Arc::new(primary);
    }
    let mut models: Vec<Box<dyn Model>> = vec![Box::new(primary)];
    for name in &cfg.root.model.fallback_models {
        models.push(Box::new(
            LitellmBackend::new(name.clone()).with_max_tokens(cfg.root.model.max_tokens),
        ));
    }
    Arc::new(FallbackModel::new(models))
}

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

fn current_git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn current_rust_version() -> Option<String> {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
