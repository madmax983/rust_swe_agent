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
    /// `true` once the runner persisted a `.patch` artifact for this
    /// instance — even an empty diff. Submitted instances missing a
    /// patch indicate a capture failure (which downgrades `outcome` to
    /// `error`); non-submitted outcomes never write a patch.
    #[serde(default)]
    pub patch_present: bool,
    /// `true` if the captured patch had any content. Distinct from
    /// `patch_present`: a submitted instance always sets `patch_present`
    /// after a successful capture, but `non_empty_patch` only when the
    /// agent's working tree actually diverged from `base_commit`.
    #[serde(default)]
    pub non_empty_patch: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepResults {
    pub total: usize,
    pub submitted: usize,
    pub skipped: usize,
    pub errored: usize,
    /// Submitted instances whose captured patch had non-zero length.
    /// Equal to `submitted` minus the count of empty-diff submissions.
    pub with_patch: usize,
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
        let _ = writeln!(
            s,
            "With patch:         {} — non-empty diff against base_commit",
            self.with_patch
        );
        let _ = writeln!(
            s,
            "Skipped:            {} — trajectory already on disk",
            self.skipped
        );
        let _ = writeln!(s, "Submit rate:        {submit_rate_pct:.2}%");
        let _ = writeln!(s, "Prompt tokens:      {}", self.total_prompt_tokens);
        let _ = writeln!(s, "Completion tokens:  {}", self.total_completion_tokens);
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
    /// When true, tasks whose trajectory file already exists and parses as
    /// valid JSON are skipped before any agent (or Docker container, or
    /// model API call) is launched for them.
    pub resume: bool,
    /// Per-task deterministic responses, cloned into each spawned `MiniArgs`.
    /// Lets sweeps run end-to-end against a scripted model without network
    /// I/O — mainly useful for tests and local smoke checks.
    pub deterministic_responses: Option<Vec<String>>,
}

/// Path where `run_one` writes the trajectory for an instance. Centralized so
/// the resume-skip check stays in lockstep with the writer.
#[must_use]
pub fn trajectory_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    output_dir.join(format!("{instance_id}.traj.json"))
}

/// Path where the SWE-bench-style unified diff is written for an instance.
/// File presence is the resume-mode signal that the patch artifact was
/// captured for a previously-submitted run.
#[must_use]
pub fn patch_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    output_dir.join(format!("{instance_id}.patch"))
}

/// Path of the aggregated SWE-bench predictions file written at the end of
/// a sweep. One JSONL line per submitted instance, in the schema sb-cli
/// expects (`instance_id`, `model_patch`, `model_name_or_path`).
#[must_use]
pub fn predictions_path(output_dir: &std::path::Path) -> PathBuf {
    output_dir.join("all_preds.jsonl")
}

/// Inspect a trajectory path on disk. Returns `Some(info)` only when the file
/// exists *and* parses as valid trajectory JSON; truncated or corrupt files
/// (e.g. a mid-write crash) yield `None` so the task re-runs.
#[must_use]
pub fn existing_trajectory_info(
    output_dir: &std::path::Path,
    instance_id: &str,
) -> Option<crate::trajectory::TrajectoryInfo> {
    read_trajectory_info(&trajectory_path_for(output_dir, instance_id))
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
// The body is a single sequential pipeline (load → resume-skip → spawn →
// join → aggregate → emit). Splitting it would obscure the linear flow
// without yielding reusable pieces.
#[allow(clippy::too_many_lines)]
pub async fn run(args: SwebenchArgs) -> Result<SweepResults, Error> {
    std::fs::create_dir_all(&args.output_dir)?;

    let instances = load_dataset(&args.dataset_path)?;
    let total = instances.len();
    let sem = Arc::new(Semaphore::new(args.parallel.max(1)));
    let mut set = tokio::task::JoinSet::new();
    let mut skipped_results: Vec<InstanceResult> = Vec::new();

    for inst in instances {
        // Resume short-circuit: a valid on-disk trajectory + (when the run
        // was submitted) a patch file mean this task is fully archived
        // from a prior sweep. Skip it before we spawn — no semaphore slot,
        // no Docker container, no model API call.
        if args.resume {
            if let Some(info) = existing_trajectory_info(&args.output_dir, &inst.instance_id) {
                let patch_path = patch_path_for(&args.output_dir, &inst.instance_id);
                let needs_patch = info.outcome.as_deref() == Some(outcome::SUBMITTED);
                if !needs_patch || patch_path.exists() {
                    skipped_results.push(skipped_result_from_info(
                        &inst.instance_id,
                        &info,
                        &patch_path,
                    ));
                    continue;
                }
                tracing::info!(
                    instance = %inst.instance_id,
                    "resume: trajectory present but patch missing — re-running"
                );
            }
        }

        let permit_sem = Arc::clone(&sem);
        let output_dir = args.output_dir.clone();
        let cfg = args.config.clone();
        let deterministic = args.deterministic_responses.clone();
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
                        patch_present: false,
                        non_empty_patch: false,
                    };
                }
            };
            run_one(inst, output_dir, cfg, deterministic).await
        });
    }

    let mut results = skipped_results;
    let skipped = results.len();
    let mut submitted = 0;
    let mut errored = 0;
    let mut with_patch = 0;
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;
    // Skipped tasks are excluded from token totals: those API calls were
    // billed in the original sweep and shouldn't be counted again here.
    while let Some(j) = set.join_next().await {
        match j {
            Ok(r) => {
                match r.outcome.as_deref() {
                    Some(outcome::SUBMITTED) => submitted += 1,
                    Some(outcome::ERROR) => errored += 1,
                    _ => {}
                }
                if r.non_empty_patch {
                    with_patch += 1;
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
                    patch_present: false,
                    non_empty_patch: false,
                });
            }
        }
    }

    // Skipped instances loaded from disk also contribute to the patch
    // counter so resumed sweeps report cumulative `with_patch` correctly.
    for r in &results {
        if r.exit_reason == "skipped_resume" && r.non_empty_patch {
            with_patch += 1;
        }
    }

    write_predictions_file(&args.output_dir, &results, &args.config.root.model.name)?;

    let sweep = SweepResults {
        total,
        submitted,
        skipped,
        errored,
        with_patch,
        total_prompt_tokens: total_prompt,
        total_completion_tokens: total_completion,
        estimated_cost_usd: estimate_cost_usd(total_prompt, total_completion),
        instances: results,
    };
    let summary_path = args.output_dir.join("results.json");
    std::fs::write(&summary_path, serde_json::to_string_pretty(&sweep)?)?;

    Ok(sweep)
}

/// Write `all_preds.jsonl` containing one line per *submitted* instance
/// with a patch artifact on disk. Schema: `{instance_id, model_patch,
/// model_name_or_path}` — the minimum sb-cli accepts. Non-submitted and
/// patch-capture-failed instances are excluded by design so sb-cli
/// reports them as unresolved rather than misattributes a stale diff.
fn write_predictions_file(
    output_dir: &std::path::Path,
    results: &[InstanceResult],
    model_name: &str,
) -> Result<(), Error> {
    let path = predictions_path(output_dir);
    let mut text = String::new();
    for r in results {
        if r.outcome.as_deref() != Some(outcome::SUBMITTED) {
            continue;
        }
        if !r.patch_present {
            // A submitted-but-patch-missing instance only happens on a
            // resume that found the trajectory but no `.patch`; we already
            // re-queued it above so this branch is defensive.
            continue;
        }
        let patch_path = patch_path_for(output_dir, &r.instance_id);
        let model_patch = std::fs::read_to_string(&patch_path).unwrap_or_default();
        let line = serde_json::json!({
            "instance_id": r.instance_id,
            "model_patch": model_patch,
            "model_name_or_path": model_name,
        });
        text.push_str(&serde_json::to_string(&line)?);
        text.push('\n');
    }
    std::fs::write(&path, text)?;
    Ok(())
}

/// Build an `InstanceResult` for a task skipped via `--resume`. Mirrors what
/// `run_one` would have produced from the on-disk trajectory, with
/// `exit_reason = "skipped_resume"` so summaries can distinguish a fresh run
/// from a resumed one.
fn skipped_result_from_info(
    instance_id: &str,
    info: &crate::trajectory::TrajectoryInfo,
    patch_path: &std::path::Path,
) -> InstanceResult {
    let (prompt_tokens, completion_tokens) = info.token_usage.as_ref().map_or((None, None), |t| {
        (Some(t.prompt_tokens), Some(t.completion_tokens))
    });
    let (patch_present, non_empty_patch) = match std::fs::metadata(patch_path) {
        Ok(m) => (true, m.len() > 0),
        Err(_) => (false, false),
    };
    InstanceResult {
        instance_id: instance_id.to_owned(),
        exit_reason: "skipped_resume".into(),
        outcome: info.outcome.clone(),
        steps: info.steps,
        cost_usd: info.total_cost_usd,
        prompt_tokens,
        completion_tokens,
        duration_secs: info.duration_secs,
        error: None,
        patch_present,
        non_empty_patch,
    }
}

async fn run_one(
    inst: SweBenchInstance,
    output_dir: PathBuf,
    mut cfg: Config,
    deterministic_responses: Option<Vec<String>>,
) -> InstanceResult {
    let id = inst.instance_id.clone();
    let task = inst.problem_statement.clone().unwrap_or_default();
    // A scripted model implies a local-only sweep — `image` from the dataset
    // would otherwise force the Docker env, which is wrong for tests.
    if deterministic_responses.is_none() {
        if let Some(img) = &inst.image {
            cfg.root.environment.docker_image = Some(img.clone());
            cfg.root.environment.kind = crate::config::EnvKind::Docker;
        }
    }
    let workdir = PathBuf::from(cfg.root.environment.workdir.clone());
    let patch_path = patch_path_for(&output_dir, &id);
    let patch_capture = Some(crate::run::mini::PatchCaptureSpec {
        base_commit: inst.base_commit.clone(),
        workdir,
        patch_path: patch_path.clone(),
    });
    let args = crate::run::mini::MiniArgs {
        task,
        extra_context: None,
        config: cfg,
        output_dir: output_dir.clone(),
        trajectory_name: id.clone(),
        deterministic_responses,
        stream_addr: None,
        patch_capture,
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

    let (patch_present, non_empty_patch) = match std::fs::metadata(&patch_path) {
        Ok(m) => (true, m.len() > 0),
        Err(_) => (false, false),
    };

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
        patch_present,
        non_empty_patch,
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
            skipped: 3,
            errored: 1,
            with_patch: 3,
            total_prompt_tokens: 250_000,
            total_completion_tokens: 50_000,
            estimated_cost_usd: estimate_cost_usd(250_000, 50_000),
            instances: vec![],
        };
        let t = s.summary_table();
        assert!(t.contains("Total tasks:        10"));
        assert!(t.contains("Submitted:          4"));
        assert!(
            t.contains("With patch:         3 — non-empty diff against base_commit"),
            "missing with_patch row in: {t}"
        );
        assert!(
            t.contains("Skipped:            3 — trajectory already on disk"),
            "missing skipped row in: {t}"
        );
        assert!(t.contains("Submit rate:        40.00%"));
        assert!(t.contains("Total tokens:       300000"));
        assert!(t.contains("Estimated cost:     $1.5000"));
    }

    #[test]
    fn existing_trajectory_info_returns_some_for_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let traj = Trajectory {
            trajectory_format: crate::trajectory::FORMAT_VERSION.into(),
            info: crate::trajectory::TrajectoryInfo {
                outcome: Some(outcome::SUBMITTED.into()),
                exit_reason: Some("submitted".into()),
                steps: Some(2),
                ..Default::default()
            },
            messages: vec![],
        };
        let path = trajectory_path_for(dir.path(), "inst-1");
        std::fs::write(&path, serde_json::to_string(&traj).unwrap()).unwrap();

        let info = existing_trajectory_info(dir.path(), "inst-1").unwrap();
        assert_eq!(info.outcome.as_deref(), Some(outcome::SUBMITTED));
    }

    #[test]
    fn existing_trajectory_info_returns_none_for_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = trajectory_path_for(dir.path(), "inst-2");
        std::fs::write(&path, "{\"trajectory_format\": \"mini-swe-agent-1.").unwrap();
        assert!(existing_trajectory_info(dir.path(), "inst-2").is_none());
    }

    #[test]
    fn existing_trajectory_info_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(existing_trajectory_info(dir.path(), "no-such-id").is_none());
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
