//! `bench grep`: search trajectory messages across a sweep by regex.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::compare::load_sweep;
use crate::trajectory::Trajectory;

#[derive(Debug, Clone)]
pub struct GrepArgs {
    pub sweep_dir: PathBuf,
    pub pattern: String,
    pub roles: Vec<String>,
    pub field: String,
    pub instance_ids: Option<Vec<String>>,
    pub exclude_instance_ids: Option<Vec<String>>,
    pub outcomes: Vec<String>,
    pub context_chars: usize,
    pub max_matches_per_instance: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatch {
    pub instance_id: String,
    pub turn_index: usize,
    pub role: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepReport {
    pub sweep: String,
    pub pattern: String,
    pub matches: Vec<GrepMatch>,
    pub instances_scanned: usize,
}

const VALID_FIELDS: &[&str] = &["content", "actions"];

pub fn run(args: &GrepArgs) -> Result<GrepReport, Error> {
    let re = Regex::new(&args.pattern).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "grep: invalid regex `{}`: {e}",
            args.pattern
        )))
    })?;

    if !VALID_FIELDS.contains(&args.field.as_str()) {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "grep: unsupported --field `{}`; supported: content, actions",
            args.field
        ))));
    }

    let redactor = Redactor::default_enabled();
    let sweep = load_sweep(&args.sweep_dir).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "grep: failed to load sweep `{}`: {e}",
            args.sweep_dir.display()
        )))
    })?;

    let include_set: Option<HashSet<&str>> = args
        .instance_ids
        .as_ref()
        .map(|ids| ids.iter().map(String::as_str).collect());

    let exclude_set: HashSet<&str> = args
        .exclude_instance_ids
        .as_ref()
        .map(|ids| ids.iter().map(String::as_str).collect())
        .unwrap_or_default();

    let outcome_filter: Option<HashSet<&str>> = if args.outcomes.is_empty() {
        None
    } else {
        Some(args.outcomes.iter().map(String::as_str).collect())
    };

    let role_filter: Option<HashSet<&str>> = if args.roles.is_empty() {
        None
    } else {
        Some(args.roles.iter().map(String::as_str).collect())
    };

    let mut sorted_ids: Vec<String> = sweep.instances.keys().cloned().collect();
    sorted_ids.sort();

    let mut all_matches: Vec<GrepMatch> = Vec::new();
    let mut instances_scanned = 0usize;

    for instance_id in &sorted_ids {
        if !instance_id_is_safe(instance_id) {
            tracing::warn!("bench grep: skipping unsafe instance id {instance_id:?}");
            continue;
        }
        if let Some(ref inc) = include_set {
            if !inc.contains(instance_id.as_str()) {
                continue;
            }
        }
        if exclude_set.contains(instance_id.as_str()) {
            continue;
        }

        let instance = &sweep.instances[instance_id];
        if let Some(ref outcomes) = outcome_filter {
            let inst_outcome = instance.outcome.as_deref().unwrap_or("");
            if !outcomes.contains(inst_outcome) {
                continue;
            }
        }

        instances_scanned += 1;
        let instance_matches = search_instance(
            instance_id,
            &args.sweep_dir,
            &re,
            &redactor,
            role_filter.as_ref(),
            args,
        );
        all_matches.extend(instance_matches);
    }

    Ok(GrepReport {
        sweep: args.sweep_dir.display().to_string(),
        pattern: args.pattern.clone(),
        matches: all_matches,
        instances_scanned,
    })
}

use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};

pub fn render_text(report: &GrepReport) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["Instance ID", "Turn", "Role", "Snippet"]);

    for m in &report.matches {
        let snippet = m.snippet.replace(['\n', '\r', '\t'], " ");
        table.add_row(vec![
            m.instance_id.clone(),
            m.turn_index.to_string(),
            m.role.clone(),
            snippet,
        ]);
    }

    let mut out = String::new();
    let _ = writeln!(out, "{table}");
    out
}

pub fn render_json_lines(report: &GrepReport) -> Result<String, serde_json::Error> {
    let mut lines = Vec::with_capacity(report.matches.len());
    for m in &report.matches {
        lines.push(serde_json::to_string(m)?);
    }
    Ok(lines.join("\n"))
}

// ── internal ──────────────────────────────────────────────────────────────────

fn search_instance(
    instance_id: &str,
    sweep_dir: &Path,
    re: &Regex,
    redactor: &Redactor,
    role_filter: Option<&HashSet<&str>>,
    args: &GrepArgs,
) -> Vec<GrepMatch> {
    let mut instance_matches: Vec<GrepMatch> = Vec::new();

    'traj: for path in resolve_trajectory_paths(sweep_dir, instance_id) {
        let trajectory = match load_trajectory(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("bench grep: skipping {}: {e}", path.display());
                continue;
            }
        };

        for (turn_index, message) in trajectory.messages.iter().enumerate() {
            if let Some(roles) = role_filter {
                if !roles.contains(message.role.as_str()) {
                    continue;
                }
            }

            let raw = match args.field.as_str() {
                "actions" => message
                    .extra
                    .actions
                    .as_ref()
                    .map(|a| a.join("\n"))
                    .unwrap_or_default(),
                _ => message.content.clone(),
            };

            let text = redactor.redact_text(&raw, surface::INSPECT).text;

            for mat in re.find_iter(&text) {
                let snippet = extract_snippet(&text, mat.start(), mat.end(), args.context_chars);
                instance_matches.push(GrepMatch {
                    instance_id: instance_id.to_owned(),
                    turn_index,
                    role: message.role.clone(),
                    snippet,
                });
                if let Some(max) = args.max_matches_per_instance {
                    if instance_matches.len() >= max {
                        break 'traj;
                    }
                }
            }
        }
    }

    instance_matches
}

fn extract_snippet(text: &str, match_start: usize, match_end: usize, context: usize) -> String {
    let raw_start = match_start.saturating_sub(context);
    let raw_end = match_end.saturating_add(context).min(text.len());
    let start = char_boundary_floor(text, raw_start);
    let end = char_boundary_ceil(text, raw_end);
    text[start..end].to_owned()
}

fn char_boundary_floor(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    (0..=pos)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

fn char_boundary_ceil(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    (pos..=s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len())
}

fn load_trajectory(path: &Path) -> Result<Trajectory, Error> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string())
        .map_err(|err| Error::Trajectory(err.to_string()))?;
    serde_json::from_value(value).map_err(Into::into)
}

fn resolve_trajectory_paths(sweep: &Path, instance_id: &str) -> Vec<PathBuf> {
    let nested = sweep.join(instance_id).join("trajectory.json");
    if nested.exists() {
        return vec![nested];
    }

    let instance_dir = sweep.join(instance_id);
    if instance_dir.is_dir() {
        // Collect all run-*.traj.json files by reading the directory rather than
        // probing sequentially — avoids false negatives when slots aren't contiguous.
        let mut run_paths: Vec<PathBuf> = std::fs::read_dir(&instance_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("run-") && n.ends_with(".traj.json"))
            })
            .collect();
        run_paths.sort_by_key(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("run-"))
                .and_then(|n| n.strip_suffix(".traj.json"))
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        if !run_paths.is_empty() {
            return run_paths;
        }
    }

    let flat = sweep.join(format!("{instance_id}.traj.json"));
    if flat.exists() {
        return vec![flat];
    }

    let bundled = sweep
        .join("trajectories")
        .join(format!("{instance_id}.traj.json"));
    if bundled.exists() {
        vec![bundled]
    } else {
        vec![]
    }
}

/// Returns true when `id` is a single safe path component (no separators, no `..`).
fn instance_id_is_safe(id: &str) -> bool {
    use std::path::{Component, Path};
    let mut components = Path::new(id).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn extract_snippet_full_context() {
        let text = "hello ImportError world";
        let start = text.find("ImportError").unwrap();
        let end = start + "ImportError".len();
        let snippet = extract_snippet(text, start, end, 100);
        assert_eq!(snippet, text);
    }

    #[test]
    fn extract_snippet_narrow_context() {
        let text = "hello ImportError world";
        let start = text.find("ImportError").unwrap();
        let end = start + "ImportError".len();
        let snippet = extract_snippet(text, start, end, 3);
        assert!(snippet.contains("ImportError"));
        assert!(snippet.len() < text.len());
    }

    #[test]
    fn extract_snippet_at_start() {
        let text = "ImportError: something";
        let end = "ImportError".len();
        let snippet = extract_snippet(text, 0, end, 5);
        assert!(snippet.starts_with("ImportError"));
    }

    #[test]
    fn extract_snippet_at_end() {
        let text = "something ImportError";
        let start = text.find("ImportError").unwrap();
        let snippet = extract_snippet(text, start, text.len(), 5);
        assert!(snippet.ends_with("ImportError"));
    }

    #[test]
    fn char_boundary_floor_aligns_correctly() {
        let s = "héllo"; // 'é' is 2 bytes at position 1
        assert_eq!(char_boundary_floor(s, 0), 0);
        assert_eq!(char_boundary_floor(s, 1), 1); // 'é' starts at 1
        assert_eq!(char_boundary_floor(s, 2), 1); // mid-'é', floor to 1
    }

    #[test]
    fn extract_snippet_huge_context_does_not_overflow() {
        let text = "hello ImportError world";
        let start = text.find("ImportError").unwrap();
        let end = start + "ImportError".len();
        // usize::MAX context must not overflow or panic
        let snippet = extract_snippet(text, start, end, usize::MAX);
        assert_eq!(snippet, text);
    }

    #[test]
    fn char_boundary_ceil_aligns_correctly() {
        let s = "héllo";
        assert_eq!(char_boundary_ceil(s, 0), 0);
        assert_eq!(char_boundary_ceil(s, 1), 1);
        assert_eq!(char_boundary_ceil(s, 2), 3); // mid-'é', ceil to 3 ('l' start)
    }

    #[test]
    fn instance_id_is_safe_accepts_normal_ids() {
        assert!(instance_id_is_safe("django__django-1234"));
        assert!(instance_id_is_safe("instance-a"));
        assert!(instance_id_is_safe("my.instance"));
    }

    #[test]
    fn instance_id_is_safe_rejects_path_traversal() {
        assert!(!instance_id_is_safe("../evil"));
        assert!(!instance_id_is_safe("foo/bar"));
        assert!(!instance_id_is_safe(".."));
        assert!(!instance_id_is_safe("/absolute"));
        assert!(!instance_id_is_safe("a/b/c"));
    }

    #[test]
    fn resolve_trajectory_paths_handles_noncontiguous_run_slots() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("my-instance");
        std::fs::create_dir(&instance_dir).unwrap();
        // run-1 and run-3 exist; run-2 is missing (gap)
        std::fs::write(instance_dir.join("run-1.traj.json"), "{}").unwrap();
        std::fs::write(instance_dir.join("run-3.traj.json"), "{}").unwrap();

        let paths = resolve_trajectory_paths(dir.path(), "my-instance");
        assert_eq!(
            paths.len(),
            2,
            "both run files should be found despite the gap"
        );
        assert!(paths.iter().any(|p| p.ends_with("run-1.traj.json")));
        assert!(paths.iter().any(|p| p.ends_with("run-3.traj.json")));
    }

    #[test]
    fn resolve_trajectory_paths_sorts_runs_numerically() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join("my-instance");
        std::fs::create_dir(&instance_dir).unwrap();
        // Without numeric sort, run-10 would precede run-2 lexicographically.
        for i in [1u32, 2, 9, 10] {
            std::fs::write(instance_dir.join(format!("run-{i}.traj.json")), "{}").unwrap();
        }

        let paths = resolve_trajectory_paths(dir.path(), "my-instance");
        assert_eq!(paths.len(), 4);
        let names: Vec<&str> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "run-1.traj.json",
                "run-2.traj.json",
                "run-9.traj.json",
                "run-10.traj.json"
            ]
        );
    }
}
