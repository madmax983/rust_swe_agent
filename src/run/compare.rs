//! `bench compare`: diff two completed sweep runs.
//!
//! Reads a baseline and candidate sweep output directory (each holding a
//! `results.json` written by `run::swebench::run`), joins per-instance
//! results by `instance_id`, and emits a transition matrix + regression
//! list. Optionally exits non-zero when regressions exceed a threshold,
//! enabling CI gating on prompt/harness changes.
//!
//! Read-only over existing artifacts: no model, env, or runtime
//! concurrency. The diff is deterministic given the same inputs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::swebench::{InstanceResult, SweepResults};
use crate::trajectory::{FailureCategory, Trajectory, outcome};

/// Output format for the compare report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareFormat {
    Text,
    Json,
}

#[derive(Debug, Clone)]
pub struct CompareArgs {
    pub baseline: PathBuf,
    pub candidate: PathBuf,
    pub format: CompareFormat,
    /// When `Some(n)`, the binary exits non-zero if regressed-task count
    /// strictly exceeds `n`. `None` is informational only.
    pub max_regressions: Option<usize>,
}

/// Per-task transition between baseline and candidate. `pass` is defined
/// as `outcome == "submitted"` AND `failure_category` is `None`, which
/// matches what `run::swebench::run_one` writes for a clean submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    PassPass,
    PassFail,
    FailPass,
    FailFail,
    MissingPresent,
    PresentMissing,
}

impl TransitionKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::PassPass => "pass->pass",
            Self::PassFail => "pass->fail",
            Self::FailPass => "fail->pass",
            Self::FailFail => "fail->fail",
            Self::MissingPresent => "missing->present",
            Self::PresentMissing => "present->missing",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskTransition {
    pub instance_id: String,
    pub kind: TransitionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_failure_category: Option<FailureCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_failure_category: Option<FailureCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_exit_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_exit_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompareReport {
    pub baseline_dir: PathBuf,
    pub candidate_dir: PathBuf,
    pub baseline_total: usize,
    pub candidate_total: usize,
    /// Counts of each transition kind. Always contains every variant,
    /// with zero for absent buckets — keeps downstream JSON consumers
    /// from having to special-case missing keys.
    pub transitions: BTreeMap<TransitionKind, usize>,
    pub baseline_resolved: usize,
    pub candidate_resolved: usize,
    pub resolved_delta: i64,
    pub baseline_total_cost_usd: f64,
    pub candidate_total_cost_usd: f64,
    pub cost_delta_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_mean_steps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_mean_steps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_steps_delta: Option<f64>,
    pub failure_category_baseline: BTreeMap<FailureCategory, usize>,
    pub failure_category_candidate: BTreeMap<FailureCategory, usize>,
    pub failure_category_delta: BTreeMap<FailureCategory, i64>,
    /// Tasks that passed in the baseline but failed in the candidate.
    /// This is the high-signal artifact for CI gating; sorted by
    /// `instance_id` for stable output.
    pub regressions: Vec<TaskTransition>,
}

impl CompareReport {
    #[must_use]
    pub fn regression_count(&self) -> usize {
        self.regressions.len()
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Compact human-readable table for terminal output.
    #[must_use]
    pub fn human_table(&self) -> String {
        let mut s = String::new();
        s.push_str("\n=== bench compare ===\n");
        let _ = writeln!(s, "Baseline:           {}", self.baseline_dir.display());
        let _ = writeln!(s, "Candidate:          {}", self.candidate_dir.display());
        let _ = writeln!(
            s,
            "Tasks (b/c/union):  {} / {} / {}",
            self.baseline_total,
            self.candidate_total,
            self.transitions.values().sum::<usize>()
        );
        let _ = writeln!(
            s,
            "Resolved:           {} -> {} ({:+})",
            self.baseline_resolved, self.candidate_resolved, self.resolved_delta
        );
        let _ = writeln!(
            s,
            "Total cost USD:     ${:.4} -> ${:.4} ({:+.4})",
            self.baseline_total_cost_usd, self.candidate_total_cost_usd, self.cost_delta_usd
        );
        match (
            self.baseline_mean_steps,
            self.candidate_mean_steps,
            self.mean_steps_delta,
        ) {
            (Some(b), Some(c), Some(d)) => {
                let _ = writeln!(s, "Mean steps:         {b:.2} -> {c:.2} ({d:+.2})");
            }
            _ => {
                s.push_str("Mean steps:         n/a\n");
            }
        }

        s.push_str("\nTransition matrix:\n");
        for kind in [
            TransitionKind::PassPass,
            TransitionKind::PassFail,
            TransitionKind::FailPass,
            TransitionKind::FailFail,
            TransitionKind::MissingPresent,
            TransitionKind::PresentMissing,
        ] {
            let n = self.transitions.get(&kind).copied().unwrap_or(0);
            let _ = writeln!(s, "  {:<18} {n}", kind.label());
        }

        let nonzero: Vec<(FailureCategory, i64)> = self
            .failure_category_delta
            .iter()
            .filter(|(_, v)| **v != 0)
            .map(|(k, v)| (*k, *v))
            .collect();
        if !nonzero.is_empty() {
            s.push_str("\nFailure category delta (candidate - baseline):\n");
            for (cat, d) in nonzero {
                let b = self
                    .failure_category_baseline
                    .get(&cat)
                    .copied()
                    .unwrap_or(0);
                let c = self
                    .failure_category_candidate
                    .get(&cat)
                    .copied()
                    .unwrap_or(0);
                let _ = writeln!(s, "  {:<14} {b} -> {c} ({d:+})", failure_label(cat));
            }
        }

        if self.regressions.is_empty() {
            s.push_str("\nRegressions:        none\n");
        } else {
            let _ = writeln!(s, "\nRegressions ({}):", self.regressions.len());
            for r in &self.regressions {
                let cat = r.candidate_failure_category.map_or("none", failure_label);
                let exit = r.candidate_exit_reason.as_deref().unwrap_or("?");
                let old = r.baseline_outcome.as_deref().unwrap_or("?");
                let new = r.candidate_outcome.as_deref().unwrap_or("?");
                let _ = writeln!(
                    s,
                    "  - {id}  {old} -> {new}  category={cat}  exit_reason={exit}",
                    id = r.instance_id
                );
            }
        }
        s
    }
}

/// Load all `InstanceResult`s from a sweep output directory.
///
/// Tries `results.json` first (the canonical end-of-sweep summary). Falls
/// back to scanning per-instance `*.traj.json` files when no `results.json`
/// exists, reconstructing minimal `InstanceResult`s. Tolerant of missing
/// newer fields: defaults flow through serde.
pub fn load_run(dir: &Path) -> Result<HashMap<String, InstanceResult>, Error> {
    let results_path = dir.join("results.json");
    if results_path.exists() {
        let text = std::fs::read_to_string(&results_path)?;
        let sweep: SweepResults = serde_json::from_str(&text)?;
        return Ok(sweep
            .instances
            .into_iter()
            .map(|r| (r.instance_id.clone(), r))
            .collect());
    }
    if !dir.exists() {
        return Err(Error::Trajectory(format!(
            "compare: directory does not exist: {}",
            dir.display()
        )));
    }

    let mut out = HashMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.ends_with(".traj.json") {
            continue;
        }
        let id = name_str.trim_end_matches(".traj.json").to_owned();
        let text = std::fs::read_to_string(entry.path())?;
        let traj: Trajectory = match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let info = traj.info;
        let (prompt_tokens, completion_tokens) =
            info.token_usage.as_ref().map_or((None, None), |t| {
                (Some(t.prompt_tokens), Some(t.completion_tokens))
            });
        out.insert(
            id.clone(),
            InstanceResult {
                instance_id: id,
                exit_reason: info.exit_reason.clone().unwrap_or_default(),
                outcome: info.outcome.clone(),
                failure_category: info.failure_category,
                steps: info.steps,
                cost_usd: info.total_cost_usd,
                prompt_tokens,
                completion_tokens,
                duration_secs: info.duration_secs,
                error: None,
                patch_present: false,
                non_empty_patch: false,
            },
        );
    }
    Ok(out)
}

/// Compute a `CompareReport` from two on-disk sweep directories.
pub fn compute(args: &CompareArgs) -> Result<CompareReport, Error> {
    let baseline = load_run(&args.baseline)?;
    let candidate = load_run(&args.candidate)?;
    Ok(diff(&args.baseline, &args.candidate, &baseline, &candidate))
}

/// Pure diff over two already-loaded id->result maps. Split out so tests
/// can drive it without touching the filesystem.
#[must_use]
pub fn diff<S: std::hash::BuildHasher>(
    baseline_dir: &Path,
    candidate_dir: &Path,
    baseline: &HashMap<String, InstanceResult, S>,
    candidate: &HashMap<String, InstanceResult, S>,
) -> CompareReport {
    let mut all_ids: BTreeSet<&str> = BTreeSet::new();
    all_ids.extend(baseline.keys().map(String::as_str));
    all_ids.extend(candidate.keys().map(String::as_str));

    let mut transitions: BTreeMap<TransitionKind, usize> = BTreeMap::new();
    for kind in [
        TransitionKind::PassPass,
        TransitionKind::PassFail,
        TransitionKind::FailPass,
        TransitionKind::FailFail,
        TransitionKind::MissingPresent,
        TransitionKind::PresentMissing,
    ] {
        transitions.insert(kind, 0);
    }
    let mut regressions: Vec<TaskTransition> = Vec::new();

    for id in &all_ids {
        let b = baseline.get(*id);
        let c = candidate.get(*id);
        let kind = classify(b, c);
        *transitions.entry(kind).or_insert(0) += 1;
        if matches!(kind, TransitionKind::PassFail) {
            regressions.push(TaskTransition {
                instance_id: (*id).to_owned(),
                kind,
                baseline_outcome: b.and_then(|r| r.outcome.clone()),
                candidate_outcome: c.and_then(|r| r.outcome.clone()),
                baseline_failure_category: b.and_then(|r| r.failure_category),
                candidate_failure_category: c.and_then(|r| r.failure_category),
                baseline_exit_reason: b.map(|r| r.exit_reason.clone()),
                candidate_exit_reason: c.map(|r| r.exit_reason.clone()),
            });
        }
    }

    let baseline_resolved = baseline.values().filter(|r| is_pass(r)).count();
    let candidate_resolved = candidate.values().filter(|r| is_pass(r)).count();

    let baseline_total_cost: f64 = baseline.values().filter_map(|r| r.cost_usd).sum();
    let candidate_total_cost: f64 = candidate.values().filter_map(|r| r.cost_usd).sum();

    let baseline_mean_steps = mean_steps(baseline);
    let candidate_mean_steps = mean_steps(candidate);
    let mean_steps_delta = match (baseline_mean_steps, candidate_mean_steps) {
        (Some(b), Some(c)) => Some(c - b),
        _ => None,
    };

    let failure_category_baseline = histogram(baseline);
    let failure_category_candidate = histogram(candidate);
    let mut failure_category_delta: BTreeMap<FailureCategory, i64> = BTreeMap::new();
    for cat in failure_category_baseline
        .keys()
        .chain(failure_category_candidate.keys())
    {
        let b = i64::try_from(failure_category_baseline.get(cat).copied().unwrap_or(0))
            .unwrap_or(i64::MAX);
        let c = i64::try_from(failure_category_candidate.get(cat).copied().unwrap_or(0))
            .unwrap_or(i64::MAX);
        failure_category_delta.insert(*cat, c - b);
    }

    CompareReport {
        baseline_dir: baseline_dir.to_path_buf(),
        candidate_dir: candidate_dir.to_path_buf(),
        baseline_total: baseline.len(),
        candidate_total: candidate.len(),
        transitions,
        baseline_resolved,
        candidate_resolved,
        resolved_delta: i64::try_from(candidate_resolved).unwrap_or(i64::MAX)
            - i64::try_from(baseline_resolved).unwrap_or(i64::MAX),
        baseline_total_cost_usd: baseline_total_cost,
        candidate_total_cost_usd: candidate_total_cost,
        cost_delta_usd: candidate_total_cost - baseline_total_cost,
        baseline_mean_steps,
        candidate_mean_steps,
        mean_steps_delta,
        failure_category_baseline,
        failure_category_candidate,
        failure_category_delta,
        regressions,
    }
}

fn classify(b: Option<&InstanceResult>, c: Option<&InstanceResult>) -> TransitionKind {
    match (b, c) {
        (None, Some(_)) => TransitionKind::MissingPresent,
        (None | Some(_), None) => TransitionKind::PresentMissing,
        (Some(b), Some(c)) => match (is_pass(b), is_pass(c)) {
            (true, true) => TransitionKind::PassPass,
            (true, false) => TransitionKind::PassFail,
            (false, true) => TransitionKind::FailPass,
            (false, false) => TransitionKind::FailFail,
        },
    }
}

fn is_pass(r: &InstanceResult) -> bool {
    r.outcome.as_deref() == Some(outcome::SUBMITTED) && r.failure_category.is_none()
}

fn mean_steps<S: std::hash::BuildHasher>(map: &HashMap<String, InstanceResult, S>) -> Option<f64> {
    let xs: Vec<u32> = map.values().filter_map(|r| r.steps).collect();
    if xs.is_empty() {
        return None;
    }
    let sum: u64 = xs.iter().map(|x| u64::from(*x)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum as f64 / xs.len() as f64;
    Some(mean)
}

fn histogram<S: std::hash::BuildHasher>(
    map: &HashMap<String, InstanceResult, S>,
) -> BTreeMap<FailureCategory, usize> {
    let mut out: BTreeMap<FailureCategory, usize> = BTreeMap::new();
    for r in map.values() {
        if let Some(cat) = r.failure_category {
            *out.entry(cat).or_insert(0) += 1;
        }
    }
    out
}

fn failure_label(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn submitted(id: &str) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "submitted".into(),
            outcome: Some(outcome::SUBMITTED.into()),
            failure_category: None,
            steps: Some(5),
            cost_usd: Some(0.10),
            prompt_tokens: Some(1000),
            completion_tokens: Some(200),
            duration_secs: Some(12.0),
            error: None,
            patch_present: true,
            non_empty_patch: true,
        }
    }

    fn errored(id: &str, cat: FailureCategory) -> InstanceResult {
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "error".into(),
            outcome: Some(outcome::ERROR.into()),
            failure_category: Some(cat),
            steps: Some(7),
            cost_usd: Some(0.20),
            prompt_tokens: Some(2000),
            completion_tokens: Some(400),
            duration_secs: Some(20.0),
            error: Some("boom".into()),
            patch_present: false,
            non_empty_patch: false,
        }
    }

    fn legacy_unknown(id: &str) -> InstanceResult {
        // Pre-#15 baseline: ERROR with no failure_category. Per AC,
        // the diff must classify this as a non-pass without panicking.
        InstanceResult {
            instance_id: id.into(),
            exit_reason: "error".into(),
            outcome: Some(outcome::ERROR.into()),
            failure_category: None,
            steps: None,
            cost_usd: None,
            prompt_tokens: None,
            completion_tokens: None,
            duration_secs: None,
            error: None,
            patch_present: false,
            non_empty_patch: false,
        }
    }

    fn map_of<I: IntoIterator<Item = InstanceResult>>(it: I) -> HashMap<String, InstanceResult> {
        it.into_iter().map(|r| (r.instance_id.clone(), r)).collect()
    }

    #[test]
    fn classifies_all_six_transitions() {
        // Baseline:  pp(pass), pf(pass), fp(fail), ff(fail), pm(pass)
        // Candidate: pp(pass), pf(fail), fp(pass), ff(fail), mp(pass)
        let baseline = map_of([
            submitted("pp"),
            submitted("pf"),
            errored("fp", FailureCategory::ModelApi),
            errored("ff", FailureCategory::ModelApi),
            submitted("pm"),
        ]);
        let candidate = map_of([
            submitted("pp"),
            errored("pf", FailureCategory::ModelApi),
            submitted("fp"),
            errored("ff", FailureCategory::AgentInternal),
            submitted("mp"),
        ]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.transitions[&TransitionKind::PassPass], 1);
        assert_eq!(r.transitions[&TransitionKind::PassFail], 1);
        assert_eq!(r.transitions[&TransitionKind::FailPass], 1);
        assert_eq!(r.transitions[&TransitionKind::FailFail], 1);
        assert_eq!(r.transitions[&TransitionKind::MissingPresent], 1);
        assert_eq!(r.transitions[&TransitionKind::PresentMissing], 1);
    }

    #[test]
    fn regressions_list_only_pass_fail() {
        let baseline = map_of([
            submitted("a"),
            submitted("b"),
            errored("c", FailureCategory::ModelApi),
        ]);
        let candidate = map_of([
            submitted("a"),
            errored("b", FailureCategory::StepLimit),
            errored("c", FailureCategory::ModelApi),
        ]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.regressions.len(), 1);
        assert_eq!(r.regressions[0].instance_id, "b");
        assert_eq!(
            r.regressions[0].candidate_failure_category,
            Some(FailureCategory::StepLimit)
        );
        assert_eq!(
            r.regressions[0].candidate_exit_reason.as_deref(),
            Some("error")
        );
    }

    #[test]
    fn aggregates_resolved_and_cost_deltas() {
        let baseline = map_of([
            submitted("a"),
            submitted("b"),
            errored("c", FailureCategory::ModelApi),
        ]);
        let candidate = map_of([submitted("a"), submitted("b"), submitted("c")]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.baseline_resolved, 2);
        assert_eq!(r.candidate_resolved, 3);
        assert_eq!(r.resolved_delta, 1);
        // baseline cost = 0.10+0.10+0.20 = 0.40; candidate = 0.30
        assert!((r.baseline_total_cost_usd - 0.40).abs() < 1e-9);
        assert!((r.candidate_total_cost_usd - 0.30).abs() < 1e-9);
        assert!((r.cost_delta_usd - (-0.10)).abs() < 1e-9);
    }

    #[test]
    fn legacy_baseline_without_failure_category_does_not_panic() {
        // Per AC: tolerate trajectories missing newer fields.
        let baseline = map_of([legacy_unknown("a"), submitted("b")]);
        let candidate = map_of([submitted("a"), errored("b", FailureCategory::ModelApi)]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.transitions[&TransitionKind::FailPass], 1);
        assert_eq!(r.transitions[&TransitionKind::PassFail], 1);
    }

    #[test]
    fn missing_and_extra_ids_are_bucketed_not_dropped() {
        let baseline = map_of([submitted("only_b")]);
        let candidate = map_of([submitted("only_c")]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        assert_eq!(r.transitions[&TransitionKind::PresentMissing], 1);
        assert_eq!(r.transitions[&TransitionKind::MissingPresent], 1);
        assert_eq!(r.regressions.len(), 0);
    }

    #[test]
    fn json_round_trip_via_results_json() {
        // End-to-end: write a synthetic results.json on disk, load with
        // `load_run`, compute diff, ensure structure is preserved.
        let dir_b = tempfile::tempdir().unwrap();
        let dir_c = tempfile::tempdir().unwrap();
        let baseline_sweep = SweepResults {
            total: 2,
            submitted: 1,
            skipped: 0,
            errored: 1,
            failures_by_category: BTreeMap::new(),
            budget_halted: 0,
            with_patch: 1,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            estimated_cost_usd: 0.0,
            cost_limit_usd: None,
            instances: vec![submitted("a"), errored("b", FailureCategory::ModelApi)],
        };
        let candidate_sweep = SweepResults {
            instances: vec![errored("a", FailureCategory::StepLimit), submitted("b")],
            ..baseline_sweep.clone()
        };
        std::fs::write(
            dir_b.path().join("results.json"),
            serde_json::to_string_pretty(&baseline_sweep).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir_c.path().join("results.json"),
            serde_json::to_string_pretty(&candidate_sweep).unwrap(),
        )
        .unwrap();

        let r = compute(&CompareArgs {
            baseline: dir_b.path().to_path_buf(),
            candidate: dir_c.path().to_path_buf(),
            format: CompareFormat::Json,
            max_regressions: None,
        })
        .unwrap();
        assert_eq!(r.regressions.len(), 1);
        assert_eq!(r.regressions[0].instance_id, "a");
        let json = r.to_json_pretty().unwrap();
        assert!(json.contains("\"pass_fail\""), "got: {json}");
        assert!(json.contains("\"regressions\""), "got: {json}");
    }

    #[test]
    fn human_table_lists_regressions_and_deltas() {
        let baseline = map_of([submitted("a"), submitted("b")]);
        let candidate = map_of([submitted("a"), errored("b", FailureCategory::StepLimit)]);
        let r = diff(Path::new("/b"), Path::new("/c"), &baseline, &candidate);
        let t = r.human_table();
        assert!(t.contains("=== bench compare ==="));
        assert!(t.contains("Resolved:           2 -> 1 (-1)"));
        assert!(t.contains("pass->fail"));
        assert!(t.contains("Regressions (1):"), "got:\n{t}");
        assert!(t.contains("- b"), "got:\n{t}");
        assert!(t.contains("category=step_limit"), "got:\n{t}");
    }

    #[test]
    fn falls_back_to_trajectory_files_when_no_results_json() {
        // Exercises the load_run fallback path used when an operator
        // points compare at a directory that only has per-task trajectories
        // (e.g. a sweep that crashed before writing results.json).
        use crate::trajectory::{FORMAT_VERSION, TrajectoryInfo};
        let dir = tempfile::tempdir().unwrap();
        let traj = Trajectory {
            trajectory_format: FORMAT_VERSION.into(),
            info: TrajectoryInfo {
                outcome: Some(outcome::SUBMITTED.into()),
                exit_reason: Some("submitted".into()),
                steps: Some(3),
                ..Default::default()
            },
            messages: vec![],
        };
        std::fs::write(
            dir.path().join("inst-1.traj.json"),
            serde_json::to_string_pretty(&traj).unwrap(),
        )
        .unwrap();
        let map = load_run(dir.path()).unwrap();
        assert_eq!(map.len(), 1);
        let r = map.get("inst-1").unwrap();
        assert_eq!(r.outcome.as_deref(), Some(outcome::SUBMITTED));
        assert_eq!(r.steps, Some(3));
    }
}
