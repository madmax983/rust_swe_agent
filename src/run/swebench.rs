//! SWE-bench sweep runner. Minimum-viable full parity: load JSONL, shard
//! across workers with a `JoinSet` + `Semaphore`, emit per-instance
//! trajectory + patch files, summarize in `results.json`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::error::Error;
use crate::trajectory::{Trajectory, outcome};

/// Standard `claude-3-5-sonnet` USD pricing per 1M tokens. Used for the
/// summary's cost estimate; per-instance trajectories carry only token
/// counts so downstream tooling can re-price as needed.
pub const SONNET_INPUT_USD_PER_MTOK: f64 = 3.0;
pub const SONNET_OUTPUT_USD_PER_MTOK: f64 = 15.0;

#[must_use]
pub fn estimate_cost_usd(prompt_tokens: u64, completion_tokens: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let p = prompt_tokens as f64;
    #[allow(clippy::cast_precision_loss)]
    let c = completion_tokens as f64;
    (c / 1_000_000.0).mul_add(
        SONNET_OUTPUT_USD_PER_MTOK,
        p / 1_000_000.0 * SONNET_INPUT_USD_PER_MTOK,
    )
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SweBenchInstance {
    pub instance_id: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub problem_statement: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceResult {
    pub instance_id: String,
    pub exit_reason: String,
    /// Coarse outcome from the trajectory: `submitted` | `step_limit_reached`
    /// | `error`. `None` when the trajectory file could not be read.
    pub outcome: Option<String>,
    pub steps: Option<u32>,
    pub cost_usd: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub duration_secs: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepResults {
    pub total: usize,
    pub submitted: usize,
    pub errored: usize,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub estimated_cost_usd: f64,
    pub instances: Vec<InstanceResult>,
}

impl SweepResults {
    /// Render the post-sweep summary table. A flat plain-text block so it
    /// reads cleanly in CI logs and from a tail of stdout.
    #[must_use]
    pub fn summary_table(&self) -> String {
        #[allow(clippy::cast_precision_loss)]
        let submit_rate_pct = if self.total == 0 {
            0.0
        } else {
            (self.submitted as f64 / self.total as f64) * 100.0
        };
        let total_tokens = self
            .total_prompt_tokens
            .saturating_add(self.total_completion_tokens);
        let mut s = String::new();
        s.push_str("\n=== SWE-bench sweep summary ===\n");
        let _ = writeln!(s, "Total tasks:        {}", self.total);
        let _ = writeln!(s, "Submitted:          {}", self.submitted);
        let _ = writeln!(s, "Submit rate:        {submit_rate_pct:.2}%");
        let _ = writeln!(s, "Prompt tokens:      {}", self.total_prompt_tokens);
        let _ = writeln!(
            s,
            "Completion tokens:  {}",
            self.total_completion_tokens
        );
        let _ = writeln!(s, "Total tokens:       {total_tokens}");
        let _ = writeln!(
            s,
            "Estimated cost:     ${:.4} (claude-3-5-sonnet @ ${SONNET_INPUT_USD_PER_MTOK}/MTok in, ${SONNET_OUTPUT_USD_PER_MTOK}/MTok out)",
            self.estimated_cost_usd
        );
        s
    }
}

pub struct SwebenchArgs {
    pub dataset_path: PathBuf,
    pub output_dir: PathBuf,
    pub parallel: usize,
    pub config: Config,
}

pub fn load_dataset(path: &std::path::Path) -> Result<Vec<SweBenchInstance>, Error> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let instance: SweBenchInstance = serde_json::from_str(line)
            .map_err(|e| Error::Trajectory(format!("dataset line {}: {e}", i + 1)))?;
        out.push(instance);
    }
    Ok(out)
}

/// Run the sweep. This scaffolds the parallelism + trajectory emission; the
/// per-instance body calls through to `run::mini::run` using a Docker env
/// (when the `docker` feature is enabled).
pub async fn run(args: SwebenchArgs) -> Result<SweepResults, Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    let instances = load_dataset(&args.dataset_path)?;
    let total = instances.len();
    let sem = Arc::new(Semaphore::new(args.parallel.max(1)));
    let mut set = tokio::task::JoinSet::new();

    for inst in instances {
        let permit_sem = Arc::clone(&sem);
        let output_dir = args.output_dir.clone();
        let cfg = args.config.clone();
        set.spawn(async move {
            let _permit = match permit_sem.acquire_owned().await {
                Ok(p) => p,
                Err(e) => {
                    return InstanceResult {
                        instance_id: inst.instance_id.clone(),
                        exit_reason: "error".into(),
                        outcome: Some(outcome::ERROR.into()),
                        steps: None,
                        cost_usd: None,
                        prompt_tokens: None,
                        completion_tokens: None,
                        duration_secs: None,
                        error: Some(e.to_string()),
                    };
                }
            };
            run_one(inst, output_dir, cfg).await
        });
    }

    let mut results = Vec::new();
    let mut submitted = 0;
    let mut errored = 0;
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;
    while let Some(j) = set.join_next().await {
        match j {
            Ok(r) => {
                match r.outcome.as_deref() {
                    Some(outcome::SUBMITTED) => submitted += 1,
                    Some(outcome::ERROR) => errored += 1,
                    _ => {}
                }
                if let Some(p) = r.prompt_tokens {
                    total_prompt = total_prompt.saturating_add(p);
                }
                if let Some(c) = r.completion_tokens {
                    total_completion = total_completion.saturating_add(c);
                }
                results.push(r);
            }
            Err(e) => {
                errored += 1;
                results.push(InstanceResult {
                    instance_id: "<join_error>".into(),
                    exit_reason: "error".into(),
                    outcome: Some(outcome::ERROR.into()),
                    steps: None,
                    cost_usd: None,
                    prompt_tokens: None,
                    completion_tokens: None,
                    duration_secs: None,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    let sweep = SweepResults {
        total,
        submitted,
        errored,
        total_prompt_tokens: total_prompt,
        total_completion_tokens: total_completion,
        estimated_cost_usd: estimate_cost_usd(total_prompt, total_completion),
        instances: results,
    };
    let summary_path = args.output_dir.join("results.json");
    std::fs::write(&summary_path, serde_json::to_string_pretty(&sweep)?)?;

    Ok(sweep)
}

async fn run_one(inst: SweBenchInstance, output_dir: PathBuf, mut cfg: Config) -> InstanceResult {
    let id = inst.instance_id.clone();
    let task = inst.problem_statement.clone().unwrap_or_default();
    if let Some(img) = &inst.image {
        cfg.root.environment.docker_image = Some(img.clone());
        cfg.root.environment.kind = crate::config::EnvKind::Docker;
    }
    let args = crate::run::mini::MiniArgs {
        task,
        extra_context: None,
        config: cfg,
        output_dir: output_dir.clone(),
        trajectory_name: id.clone(),
        deterministic_responses: None,
        stream_addr: None,
    };
    let run_err = crate::run::mini::run(args).await.err();

    // Trajectory is the source of truth — `mini::run` writes it on both
    // success and error paths, so reading it covers every outcome.
    let traj_path = output_dir.join(format!("{id}.traj.json"));
    let info = read_trajectory_info(&traj_path);

    let outcome_str = info
        .as_ref()
        .and_then(|i| i.outcome.clone())
        .or_else(|| run_err.as_ref().map(|_| outcome::ERROR.to_owned()))
        .unwrap_or_else(|| outcome::ERROR.to_owned());

    let exit_reason = info
        .as_ref()
        .and_then(|i| i.exit_reason.clone())
        .unwrap_or_else(|| outcome_str.clone());

    let (prompt_tokens, completion_tokens) = info
        .as_ref()
        .and_then(|i| i.token_usage.as_ref())
        .map_or((None, None), |t| {
            (Some(t.prompt_tokens), Some(t.completion_tokens))
        });

    InstanceResult {
        instance_id: id,
        exit_reason,
        outcome: Some(outcome_str),
        steps: info.as_ref().and_then(|i| i.steps),
        cost_usd: info.as_ref().and_then(|i| i.total_cost_usd),
        prompt_tokens,
        completion_tokens,
        duration_secs: info.as_ref().and_then(|i| i.duration_secs),
        error: run_err.map(|e| e.to_string()),
    }
}

fn read_trajectory_info(path: &std::path::Path) -> Option<crate::trajectory::TrajectoryInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let traj: Trajectory = serde_json::from_str(&text).ok()?;
    Some(traj.info)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn cost_estimate_uses_sonnet_pricing() {
        // 1M prompt + 1M completion = $3 + $15 = $18.
        let c = estimate_cost_usd(1_000_000, 1_000_000);
        assert!((c - 18.0).abs() < 1e-9, "got {c}");
        // Zero in, zero out.
        assert!(estimate_cost_usd(0, 0).abs() < 1e-9);
    }

    #[test]
    fn summary_table_includes_required_fields() {
        let s = SweepResults {
            total: 10,
            submitted: 4,
            errored: 1,
            total_prompt_tokens: 250_000,
            total_completion_tokens: 50_000,
            estimated_cost_usd: estimate_cost_usd(250_000, 50_000),
            instances: vec![],
        };
        let t = s.summary_table();
        assert!(t.contains("Total tasks:        10"));
        assert!(t.contains("Submitted:          4"));
        assert!(t.contains("Submit rate:        40.00%"));
        assert!(t.contains("Total tokens:       300000"));
        assert!(t.contains("Estimated cost:     $1.5000"));
    }

    #[test]
    fn loads_jsonl() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "{\"instance_id\":\"a\"}\n{\"instance_id\":\"b\",\"image\":\"ubuntu:22.04\"}\n",
        )
        .unwrap();
        let got = load_dataset(tmp.path()).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].instance_id, "a");
        assert_eq!(got[1].image.as_deref(), Some("ubuntu:22.04"));
    }
}
