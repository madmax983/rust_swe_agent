//! Machine-readable single-run result for `max mini --result-format json`.
//!
//! See `docs/spec-mini-result-format.md` for the full schema contract.

use std::path::Path;

use serde::Serialize;

use crate::artifact::ArtifactKind;
use crate::exit_code::ExitCode;
use crate::redaction::{Redactor, surface};
use crate::trajectory::{FailureCategory, TokenUsage, TrajectoryInfo};

/// Compact, schema-versioned single-run result emitted to stdout by
/// `max mini --result-format json`.
///
/// The struct is flattened with an `ArtifactHeader` at serialisation time via
/// `crate::artifact::to_string_pretty`, which prepends `artifact_kind` and
/// `schema_version` into the top-level JSON object.
#[derive(Debug, Serialize)]
pub struct MiniResult {
    /// High-level run outcome string from `trajectory.info.outcome`
    /// (e.g. `"submitted"`, `"error"`, `"step_limit_reached"`).
    pub outcome: Option<String>,
    /// Numeric process exit code this run would produce.
    pub exit_code: i32,
    /// Stable string outcome-class label (e.g. `"success"`, `"verification_failure"`).
    pub exit_outcome_class: String,
    /// Total cost of the run in USD (actual + baseline). `null` when not captured.
    pub total_cost_usd: Option<f64>,
    /// Number of agent steps taken.
    pub steps: Option<u32>,
    /// Prompt (input) tokens consumed. `null` when not captured.
    pub input_tokens: Option<u64>,
    /// Completion (output) tokens consumed. `null` when not captured.
    pub output_tokens: Option<u64>,
    /// Absolute path of the written `.traj.json` trajectory file.
    pub trajectory_path: String,
    /// Absolute path of the written `.patch` file, or `null` when no patch
    /// was captured (no `--github-pr` / `--open-pr` flags).
    pub patch_path: Option<String>,
    /// Machine-readable failure category. `null` on successful runs.
    pub failure_category: Option<FailureCategory>,
}

impl MiniResult {
    /// Build a `MiniResult` from a completed `TrajectoryInfo` and associated metadata.
    ///
    /// * `info` — the `.info` block of the just-written trajectory.
    /// * `exit` — the `ExitCode` that the process will exit with.
    /// * `traj_path` — absolute path to the `.traj.json` artifact.
    /// * `patch_path` — path to the `.patch` artifact, if it was written.
    pub fn from_trajectory_info(
        info: &TrajectoryInfo,
        exit: ExitCode,
        traj_path: &Path,
        patch_path: Option<&Path>,
    ) -> Self {
        let (input_tokens, output_tokens) = split_tokens(info.token_usage.as_ref());
        Self {
            outcome: info.outcome.clone(),
            exit_code: exit.as_i32(),
            exit_outcome_class: exit.outcome_class().to_owned(),
            total_cost_usd: info.total_cost_usd,
            steps: info.steps,
            input_tokens,
            output_tokens,
            trajectory_path: traj_path.display().to_string(),
            patch_path: patch_path.map(|p| p.display().to_string()),
            failure_category: info.failure_category,
        }
    }

    /// Serialize to indented JSON, redacting all string fields through the
    /// active `Redactor` policy (consistent with `agent skills-preview --format json`).
    ///
    /// Numbers, booleans, and nested structure keys are never altered.
    pub fn to_redacted_json(&self, redactor: &Redactor) -> Result<String, serde_json::Error> {
        let raw = crate::artifact::to_string_pretty(ArtifactKind::MiniResult, self)?;
        let mut value: serde_json::Value = serde_json::from_str(&raw)?;
        redactor.redact_json_value(&mut value, surface::TRAJECTORY);
        serde_json::to_string_pretty(&value)
    }
}

fn split_tokens(usage: Option<&TokenUsage>) -> (Option<u64>, Option<u64>) {
    match usage {
        Some(u) => (Some(u.prompt_tokens), Some(u.completion_tokens)),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use crate::config::RedactionCfg;
    use crate::trajectory::{TokenUsage, TrajectoryInfo};

    use super::*;

    fn make_info(outcome: &str, steps: u32, cost: f64) -> TrajectoryInfo {
        TrajectoryInfo {
            outcome: Some(outcome.to_owned()),
            steps: Some(steps),
            total_cost_usd: Some(cost),
            token_usage: Some(TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn from_trajectory_info_maps_fields_correctly() {
        let info = make_info("submitted", 3, 0.01);
        let traj = PathBuf::from("/tmp/foo.traj.json");
        let patch = PathBuf::from("/tmp/foo.patch");

        let result = MiniResult::from_trajectory_info(
            &info,
            ExitCode::Success,
            &traj,
            Some(patch.as_path()),
        );

        assert_eq!(result.outcome.as_deref(), Some("submitted"));
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.exit_outcome_class, "success");
        assert_eq!(result.total_cost_usd, Some(0.01));
        assert_eq!(result.steps, Some(3));
        assert_eq!(result.input_tokens, Some(10));
        assert_eq!(result.output_tokens, Some(5));
        assert_eq!(result.trajectory_path, "/tmp/foo.traj.json");
        assert_eq!(result.patch_path.as_deref(), Some("/tmp/foo.patch"));
        assert!(result.failure_category.is_none());
    }

    #[test]
    fn from_trajectory_info_null_patch_when_no_patch() {
        let info = make_info("submitted", 1, 0.0);
        let traj = PathBuf::from("/tmp/foo.traj.json");

        let result = MiniResult::from_trajectory_info(&info, ExitCode::Success, &traj, None);
        assert!(result.patch_path.is_none());
    }

    #[test]
    fn to_redacted_json_is_valid_json_with_required_keys() {
        let info = make_info("submitted", 2, 0.005);
        let traj = PathBuf::from("/tmp/test.traj.json");
        let result = MiniResult::from_trajectory_info(&info, ExitCode::Success, &traj, None);

        let redactor = Redactor::disabled();
        let json_str = result.to_redacted_json(&redactor).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert!(parsed.is_object());
        assert_eq!(parsed["artifact_kind"].as_str(), Some("mini_result"));
        assert!(parsed["schema_version"].is_object());
        assert!(parsed["outcome"].is_string());
        assert!(parsed["exit_code"].is_number());
        assert!(parsed["exit_outcome_class"].is_string());
        assert!(parsed.get("total_cost_usd").is_some());
        assert!(parsed.get("steps").is_some());
        assert!(parsed.get("input_tokens").is_some());
        assert!(parsed.get("output_tokens").is_some());
        assert!(parsed["trajectory_path"].is_string());
        assert!(parsed.get("patch_path").is_some());
        assert!(parsed.get("failure_category").is_some());
    }

    #[test]
    fn to_redacted_json_redacts_sensitive_trajectory_path() {
        let info = make_info("submitted", 1, 0.0);
        let traj = PathBuf::from("/home/alice/supersecrettoken/run.traj.json");
        let result = MiniResult::from_trajectory_info(&info, ExitCode::Success, &traj, None);

        let cfg = RedactionCfg {
            enabled: true,
            secret_literals: vec!["supersecrettoken".to_owned()],
            ..Default::default()
        };
        let redactor = Redactor::from_config(&cfg).unwrap();
        let json_str = result.to_redacted_json(&redactor).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        let traj_path_val = parsed["trajectory_path"].as_str().unwrap();
        assert!(
            !traj_path_val.contains("supersecrettoken"),
            "secret must be redacted from trajectory_path; got: {traj_path_val}"
        );
        assert!(
            traj_path_val.contains("[REDACTED:"),
            "expected [REDACTED:...] marker; got: {traj_path_val}"
        );
    }

    #[test]
    fn split_tokens_none_when_no_usage() {
        let (input, output) = split_tokens(None);
        assert!(input.is_none());
        assert!(output.is_none());
    }

    #[test]
    fn split_tokens_extracts_prompt_and_completion() {
        let usage = TokenUsage {
            prompt_tokens: 42,
            completion_tokens: 7,
            cache_read_tokens: 1,
            cache_creation_tokens: 2,
        };
        let (input, output) = split_tokens(Some(&usage));
        assert_eq!(input, Some(42));
        assert_eq!(output, Some(7));
    }
}
