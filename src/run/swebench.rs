//! SWE-bench sweep runner. Minimum-viable full parity: load JSONL, shard
//! across workers with a `JoinSet` + `Semaphore`, emit per-instance
//! trajectory + patch files, summarize in `results.json`.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::error::Error;

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
    pub steps: Option<u32>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepResults {
    pub total: usize,
    pub submitted: usize,
    pub errored: usize,
    pub instances: Vec<InstanceResult>,
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
                        steps: None,
                        cost_usd: None,
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
    while let Some(j) = set.join_next().await {
        match j {
            Ok(r) => {
                if r.exit_reason == "submitted" {
                    submitted += 1;
                } else if r.exit_reason == "error" {
                    errored += 1;
                }
                results.push(r);
            }
            Err(e) => {
                errored += 1;
                results.push(InstanceResult {
                    instance_id: "<join_error>".into(),
                    exit_reason: "error".into(),
                    steps: None,
                    cost_usd: None,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    let sweep = SweepResults {
        total,
        submitted,
        errored,
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
    match crate::run::mini::run(args).await {
        Ok(()) => InstanceResult {
            instance_id: id,
            exit_reason: "submitted".into(),
            steps: None,
            cost_usd: None,
            error: None,
        },
        Err(e) => InstanceResult {
            instance_id: id,
            exit_reason: "error".into(),
            steps: None,
            cost_usd: None,
            error: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

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
