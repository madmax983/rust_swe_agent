//! Zero-cost static preview of which skills will activate for one or more
//! tasks (issue #337).
//!
//! Wraps the existing `SkillRegistry::resolve_candidates` entry point, adds
//! byte-cost and cap-hit accounting, and applies the trajectory redactor to
//! all string output so secret literals never appear verbatim.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion};
use crate::config::Config;
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::skills::{SkillActivationReason, SkillRegistry, SkillResolveRequest};

// ── Public output types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEntryPreview {
    pub name: String,
    pub reason: SkillActivationReason,
    pub sha256_prefix: String,
    pub bytes: usize,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPreview {
    pub task_hash: String,
    pub active_skills: Vec<SkillEntryPreview>,
    pub total_bytes_injected: usize,
    pub max_active_cap_hit: bool,
    pub dropped_count: usize,
    pub merged_extra_context_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsPreviewSummary {
    pub task_count: usize,
    pub unique_skills_activated: usize,
    pub p50_bytes_per_task: usize,
    pub p95_bytes_per_task: usize,
    pub tasks_hitting_max_active: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsPreviewReport {
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
    pub tasks: Vec<TaskPreview>,
    pub summary: SkillsPreviewSummary,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub struct SkillsPreviewArgs {
    pub tasks: Vec<String>,
    pub config: Config,
}

/// Outcome returned alongside the report so the caller can choose the exit code.
pub enum PreviewOutcome {
    Clean(SkillsPreviewReport),
    Warning(SkillsPreviewReport, Vec<String>),
}

/// Disabled / unconfigured output (not a report; printed directly and we exit 0).
pub enum PreviewResult {
    Disabled(String),
    Report(PreviewOutcome),
}

pub fn preview(args: &SkillsPreviewArgs) -> Result<PreviewResult, Error> {
    let cfg = &args.config.root.skills;

    if !cfg.enabled {
        return Ok(PreviewResult::Disabled("skills disabled".into()));
    }
    if cfg.paths.is_empty() {
        return Ok(PreviewResult::Disabled("no skill paths configured".into()));
    }

    let redactor = Redactor::from_config_lossy(&args.config.root.redaction);

    let paths = cfg
        .paths
        .iter()
        .map(crate::skills::expand_skill_path_pub)
        .collect::<Vec<_>>();
    let registry = SkillRegistry::scan_paths(paths)?;

    let mut task_previews = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for task in &args.tasks {
        let task_preview = build_task_preview(task, &registry, cfg, &redactor, &mut warnings)?;
        task_previews.push(task_preview);
    }

    let summary = build_summary(&task_previews);
    let report = SkillsPreviewReport {
        artifact_kind: ArtifactKind::SkillsPreview,
        schema_version: ArtifactSchemaVersion::CURRENT,
        tasks: task_previews,
        summary,
    };

    if warnings.is_empty() {
        Ok(PreviewResult::Report(PreviewOutcome::Clean(report)))
    } else {
        Ok(PreviewResult::Report(PreviewOutcome::Warning(
            report, warnings,
        )))
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn build_task_preview(
    task: &str,
    registry: &SkillRegistry,
    cfg: &crate::config::SkillCfg,
    redactor: &Redactor,
    warnings: &mut Vec<String>,
) -> Result<TaskPreview, Error> {
    let task_hash = sha256_prefix_12(task.as_bytes());

    let request = SkillResolveRequest {
        task,
        auto_load: cfg.auto_load,
        max_active: cfg.max_active,
    };
    let all_candidates = registry.resolve_candidates(request);
    let total_candidates = all_candidates.len();
    let active_count = total_candidates.min(cfg.max_active);
    let dropped_count = total_candidates.saturating_sub(active_count);
    let cap_hit = dropped_count > 0;

    if cap_hit {
        warnings.push(format!(
            "task {task_hash}: max_active cap hit ({active_count} active, {dropped_count} dropped)"
        ));
    }

    let mut active_skills = Vec::new();
    let mut total_bytes_injected: usize = 0;

    for (reason, manifest) in all_candidates.into_iter().take(active_count) {
        let file_bytes = std::fs::read_to_string(&manifest.path)?;
        let byte_len = file_bytes.len();
        total_bytes_injected += byte_len;

        let sha256_prefix = sha256_prefix_12(file_bytes.as_bytes());

        if manifest.version.is_none() {
            warnings.push(format!("skill '{}' has no version field", manifest.name));
        }

        if reason == SkillActivationReason::AutoMatch && cfg.auto_load {
            warnings.push(format!(
                "skill '{}' activated via auto_match (consider using explicit mention for sweep determinism)",
                manifest.name
            ));
        }

        let raw_path = manifest.path.display().to_string();
        let redacted_path = redactor.redact_text(&raw_path, surface::TRAJECTORY).text;
        let redacted_name = redactor
            .redact_text(&manifest.name, surface::TRAJECTORY)
            .text;

        active_skills.push(SkillEntryPreview {
            name: redacted_name,
            reason,
            sha256_prefix,
            bytes: byte_len,
            path: PathBuf::from(redacted_path),
            version: manifest.version.clone(),
        });
    }

    // merged_extra_context_bytes: size delta — skills contribute `total_bytes_injected`
    // bytes on top of any base context. We report the raw injected bytes here.
    let merged_extra_context_bytes = total_bytes_injected;

    Ok(TaskPreview {
        task_hash,
        active_skills,
        total_bytes_injected,
        max_active_cap_hit: cap_hit,
        dropped_count,
        merged_extra_context_bytes,
    })
}

fn build_summary(tasks: &[TaskPreview]) -> SkillsPreviewSummary {
    let task_count = tasks.len();
    let tasks_hitting_max_active = tasks.iter().filter(|t| t.max_active_cap_hit).count();

    let mut all_names = BTreeSet::new();
    for t in tasks {
        for s in &t.active_skills {
            all_names.insert(s.name.clone());
        }
    }
    let unique_skills_activated = all_names.len();

    let mut bytes_per_task: Vec<usize> = tasks.iter().map(|t| t.total_bytes_injected).collect();
    bytes_per_task.sort_unstable();

    let p50_bytes_per_task = percentile(&bytes_per_task, 50);
    let p95_bytes_per_task = percentile(&bytes_per_task, 95);

    SkillsPreviewSummary {
        task_count,
        unique_skills_activated,
        p50_bytes_per_task,
        p95_bytes_per_task,
        tasks_hitting_max_active,
    }
}

fn percentile(sorted: &[usize], pct: usize) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() * pct).saturating_sub(1) / 100;
    sorted[idx.min(sorted.len() - 1)]
}

fn sha256_prefix_12(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(12);
    for byte in &digest {
        let _ = write!(hex, "{byte:02x}");
        if hex.len() >= 12 {
            break;
        }
    }
    hex[..12].to_owned()
}

// ── Formatting helpers ────────────────────────────────────────────────────────

pub fn format_text(report: &SkillsPreviewReport, redactor: &Redactor) -> String {
    let mut out = String::from("=== agent skills-preview (no model call made) ===\n");

    for task in &report.tasks {
        let _ = write!(out, "\ntask_hash: {}\n", task.task_hash);
        if task.active_skills.is_empty() {
            out.push_str("  (no skills activated)\n");
        } else {
            for skill in &task.active_skills {
                let reason_str = match skill.reason {
                    SkillActivationReason::ExplicitMention => "explicit",
                    SkillActivationReason::AutoMatch => "auto",
                };
                let path_str = skill.path.display().to_string();
                let _ = writeln!(
                    out,
                    "  {} | {} | {} | {} bytes | {}",
                    skill.name, reason_str, skill.sha256_prefix, skill.bytes, path_str,
                );
            }
        }
        let _ = writeln!(out, "  total_bytes_injected: {}", task.total_bytes_injected);
        let _ = writeln!(
            out,
            "  max_active_cap_hit: {} (dropped: {})",
            task.max_active_cap_hit, task.dropped_count
        );
        let _ = writeln!(
            out,
            "  merged_extra_context_bytes: {}",
            task.merged_extra_context_bytes
        );
    }

    let _ = writeln!(
        out,
        "\n--- summary ---\n\
         task_count: {}\n\
         unique_skills_activated: {}\n\
         p50_bytes_per_task: {}\n\
         p95_bytes_per_task: {}\n\
         tasks_hitting_max_active: {}",
        report.summary.task_count,
        report.summary.unique_skills_activated,
        report.summary.p50_bytes_per_task,
        report.summary.p95_bytes_per_task,
        report.summary.tasks_hitting_max_active,
    );

    redactor.redact_text(&out, surface::TRAJECTORY).text
}

/// Parse a task file: one task per line, `#`-prefixed lines ignored.
pub fn read_task_file(path: &std::path::Path) -> Result<Vec<String>, Error> {
    let content = std::fs::read_to_string(path)?;
    Ok(content
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::Config;

    fn write_skill(root: &Path, name: &str, content: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    fn skill_config(skill_root: &Path) -> Config {
        let skill_path = skill_root.display().to_string().replace('\\', "\\\\");
        Config::from_toml_str(&format!(
            "[skills]\nenabled = true\nauto_load = true\npaths = [\"{skill_path}\"]\n"
        ))
        .unwrap()
    }

    #[test]
    fn preview_disabled_returns_disabled() {
        let cfg = Config::from_toml_str("[skills]\nenabled = false\n").unwrap();
        let result = preview(&SkillsPreviewArgs {
            tasks: vec!["fix bug".into()],
            config: cfg,
        })
        .unwrap();
        assert!(matches!(result, PreviewResult::Disabled(_)));
    }

    #[test]
    fn preview_no_paths_returns_disabled() {
        let cfg = Config::from_toml_str("[skills]\nenabled = true\npaths = []\n").unwrap();
        let result = preview(&SkillsPreviewArgs {
            tasks: vec!["fix bug".into()],
            config: cfg,
        })
        .unwrap();
        assert!(matches!(result, PreviewResult::Disabled(_)));
    }

    #[test]
    fn preview_report_has_task_entry() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            temp.path(),
            "rust-router",
            "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
        );
        let cfg = skill_config(temp.path());
        let result = preview(&SkillsPreviewArgs {
            tasks: vec!["fix Rust borrow checker".into()],
            config: cfg,
        })
        .unwrap();
        let report = match result {
            PreviewResult::Report(PreviewOutcome::Clean(r) | PreviewOutcome::Warning(r, _)) => r,
            PreviewResult::Disabled(_) => panic!("expected report"),
        };
        assert_eq!(report.tasks.len(), 1);
        assert_eq!(report.tasks[0].task_hash.len(), 12);
    }

    #[test]
    fn preview_cap_hit_triggers_warning() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a-rust", "b-rust", "c-rust"] {
            write_skill(
                temp.path(),
                name,
                &format!(
                    "---\nname: {name}\ndescription: Use for Rust code.\nversion: 1.0\n---\n\n# {name}\n"
                ),
            );
        }
        let skill_path = temp.path().display().to_string().replace('\\', "\\\\");
        let cfg = Config::from_toml_str(&format!(
            "[skills]\nenabled = true\nauto_load = true\nmax_active = 1\npaths = [\"{skill_path}\"]\n"
        ))
        .unwrap();
        let result = preview(&SkillsPreviewArgs {
            tasks: vec!["fix Rust code here".into()],
            config: cfg,
        })
        .unwrap();
        assert!(matches!(
            result,
            PreviewResult::Report(PreviewOutcome::Warning(_, _))
        ));
    }

    #[test]
    fn read_task_file_ignores_comment_lines() {
        let temp = tempfile::tempdir().unwrap();
        let f = temp.path().join("tasks.txt");
        fs::write(&f, "# comment\ntask one\n\ntask two\n").unwrap();
        let tasks = read_task_file(&f).unwrap();
        assert_eq!(tasks, vec!["task one", "task two"]);
    }

    #[test]
    fn sha256_prefix_12_is_12_chars() {
        let h = sha256_prefix_12(b"hello");
        assert_eq!(h.len(), 12);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
