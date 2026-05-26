//! Post-hoc unified-diff scoring for submitted patches.

use std::collections::BTreeSet;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error};

const DEFAULT_CLASSIFIERS_TOML: &str = include_str!("../../data/patch_classifiers.toml");

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionClass {
    ProdOnly,
    Mixed,
    TestOnly,
    Empty,
}

impl SubmissionClass {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ProdOnly => "prod_only",
            Self::Mixed => "mixed",
            Self::TestOnly => "test_only",
            Self::Empty => "empty",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PatchStats {
    pub files_changed: u32,
    pub hunks: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub is_empty: bool,
    pub touches_test_files: bool,
    pub touches_lock_or_generated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gold_files_iou: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gold_lines_overlap: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gold_size_ratio: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_class: Option<SubmissionClass>,
}

impl PatchStats {
    #[must_use]
    pub fn lines_changed(&self) -> u32 {
        self.lines_added.saturating_add(self.lines_removed)
    }
}

#[derive(Debug, Clone)]
pub struct PatchClassifiers {
    test_files: Vec<GlobMatcher>,
    lock_or_generated_files: Vec<GlobMatcher>,
}

impl PatchClassifiers {
    pub fn from_default_toml() -> Result<Self, Error> {
        Self::from_toml_str(DEFAULT_CLASSIFIERS_TOML)
    }

    pub fn from_toml_str(text: &str) -> Result<Self, Error> {
        let config: PatchClassifierConfig =
            toml::from_str(text).map_err(|e| Error::Config(ConfigError::Toml(e.to_string())))?;
        Ok(Self {
            test_files: compile_globs(&config.test_files)?,
            lock_or_generated_files: compile_globs(&config.lock_or_generated_files)?,
        })
    }

    pub fn is_test_file(&self, file: &str) -> bool {
        self.test_files.iter().any(|glob| glob.matches(file))
    }

    fn touches_test_file(&self, files: &BTreeSet<String>) -> bool {
        files.iter().any(|file| self.is_test_file(file))
    }

    fn touches_lock_or_generated(&self, files: &BTreeSet<String>) -> bool {
        files.iter().any(|file| {
            self.lock_or_generated_files
                .iter()
                .any(|glob| glob.matches(file))
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct PatchClassifierConfig {
    #[serde(default)]
    test_files: Vec<String>,
    #[serde(default)]
    lock_or_generated_files: Vec<String>,
}

#[derive(Debug, Clone)]
struct GlobMatcher {
    regex: Regex,
}

impl GlobMatcher {
    fn new(glob: &str) -> Result<Self, Error> {
        let pattern = glob_to_regex(glob);
        let regex = Regex::new(&pattern).map_err(|e| {
            Error::Config(ConfigError::Invalid(format!(
                "invalid patch classifier glob `{glob}`: {e}"
            )))
        })?;
        Ok(Self { regex })
    }

    fn matches(&self, path: &str) -> bool {
        self.regex.is_match(&normalize_path(path))
    }
}

fn compile_globs(globs: &[String]) -> Result<Vec<GlobMatcher>, Error> {
    globs.iter().map(|glob| GlobMatcher::new(glob)).collect()
}

fn glob_to_regex(glob: &str) -> String {
    let chars: Vec<char> = normalize_path(glob).chars().collect();
    let mut out = String::from("^");
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') && chars.get(i + 2) == Some(&'/') => {
                out.push_str("(?:.*/)?");
                i += 3;
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                out.push_str(".*");
                i += 2;
            }
            '*' => {
                out.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            c => {
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out.push('$');
    out
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TouchedLine {
    file: String,
    side: DiffSide,
    line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DiffSide {
    Old,
    New,
}

#[derive(Debug, Clone, Default)]
struct ParsedPatch {
    files: BTreeSet<String>,
    hunks: u32,
    lines_added: u32,
    lines_removed: u32,
    touched_lines: BTreeSet<TouchedLine>,
}

impl ParsedPatch {
    fn lines_changed(&self) -> u32 {
        self.lines_added.saturating_add(self.lines_removed)
    }
}

#[must_use]
pub fn score_patch(
    patch_text: &str,
    classifiers: &PatchClassifiers,
    gold_patch: Option<&str>,
) -> PatchStats {
    let parsed = parse_unified_diff(patch_text);
    let submission_class = if parsed.files.is_empty() {
        SubmissionClass::Empty
    } else {
        let test_count = parsed
            .files
            .iter()
            .filter(|f| classifiers.is_test_file(f))
            .count();
        if test_count == parsed.files.len() {
            SubmissionClass::TestOnly
        } else if test_count == 0 {
            SubmissionClass::ProdOnly
        } else {
            SubmissionClass::Mixed
        }
    };

    let mut stats = PatchStats {
        files_changed: saturating_u32(parsed.files.len()),
        hunks: parsed.hunks,
        lines_added: parsed.lines_added,
        lines_removed: parsed.lines_removed,
        is_empty: parsed.files.is_empty() && parsed.lines_changed() == 0,
        touches_test_files: classifiers.touches_test_file(&parsed.files),
        touches_lock_or_generated: classifiers.touches_lock_or_generated(&parsed.files),
        gold_files_iou: None,
        gold_lines_overlap: None,
        gold_size_ratio: None,
        submission_class: Some(submission_class),
    };

    if let Some(gold) = gold_patch {
        let gold = parse_unified_diff(gold);
        stats.gold_files_iou = Some(files_iou(&parsed.files, &gold.files));
        stats.gold_lines_overlap = Some(lines_overlap(&parsed.touched_lines, &gold.touched_lines));
        stats.gold_size_ratio = Some(size_ratio(parsed.lines_changed(), gold.lines_changed()));
    }

    stats
}

fn parse_unified_diff(text: &str) -> ParsedPatch {
    let mut parsed = ParsedPatch::default();
    let mut current_file: Option<String> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut in_hunk = false;

    for line in text.lines() {
        if let Some(file) = parse_diff_git_file(line) {
            parsed.files.insert(file.clone());
            current_file = Some(file);
            in_hunk = false;
            continue;
        }
        if let Some(file) = parse_file_marker(line, "+++ ") {
            if file != "/dev/null" {
                parsed.files.insert(file.clone());
                current_file = Some(file);
            }
            continue;
        }
        if let Some(file) = parse_file_marker(line, "--- ") {
            if current_file.is_none() && file != "/dev/null" {
                parsed.files.insert(file.clone());
                current_file = Some(file);
            }
            continue;
        }
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            parsed.hunks = parsed.hunks.saturating_add(1);
            old_line = old_start;
            new_line = new_start;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        let Some(file) = current_file.as_ref() else {
            continue;
        };
        match line.as_bytes().first().copied() {
            Some(b'+') => {
                parsed.lines_added = parsed.lines_added.saturating_add(1);
                parsed.touched_lines.insert(TouchedLine {
                    file: file.clone(),
                    side: DiffSide::New,
                    line: new_line,
                });
                new_line = new_line.saturating_add(1);
            }
            Some(b'-') => {
                parsed.lines_removed = parsed.lines_removed.saturating_add(1);
                parsed.touched_lines.insert(TouchedLine {
                    file: file.clone(),
                    side: DiffSide::Old,
                    line: old_line,
                });
                old_line = old_line.saturating_add(1);
            }
            Some(b' ') => {
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
            }
            _ => {}
        }
    }

    parsed
}

fn parse_diff_git_file(line: &str) -> Option<String> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "diff" || parts.next()? != "--git" {
        return None;
    }
    let old = parts.next()?;
    let new = parts.next()?;
    let selected = if new == "/dev/null" { old } else { new };
    Some(strip_diff_prefix(selected))
}

fn parse_file_marker(line: &str, prefix: &str) -> Option<String> {
    line.strip_prefix(prefix)
        .and_then(|rest| rest.split_whitespace().next())
        .map(strip_diff_prefix)
}

fn strip_diff_prefix(path: &str) -> String {
    let trimmed = path.trim_matches('"');
    let stripped = trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed);
    normalize_path(stripped)
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    if !line.starts_with("@@ ") {
        return None;
    }
    let mut old_start = None;
    let mut new_start = None;
    for token in line.split_whitespace() {
        if let Some(raw) = token.strip_prefix('-') {
            old_start = parse_hunk_start(raw);
        } else if let Some(raw) = token.strip_prefix('+') {
            new_start = parse_hunk_start(raw);
        }
        if old_start.is_some() && new_start.is_some() {
            break;
        }
    }
    Some((old_start?, new_start?))
}

fn parse_hunk_start(raw: &str) -> Option<u32> {
    raw.split(',').next()?.parse::<u32>().ok()
}

#[allow(clippy::cast_precision_loss)]
fn files_iou(agent: &BTreeSet<String>, gold: &BTreeSet<String>) -> f32 {
    let intersection = agent.intersection(gold).count();
    let union = agent.union(gold).count();
    if union == 0 {
        1.0
    } else {
        intersection as f32 / union as f32
    }
}

#[allow(clippy::cast_precision_loss)]
fn lines_overlap(agent: &BTreeSet<TouchedLine>, gold: &BTreeSet<TouchedLine>) -> f32 {
    if agent.is_empty() {
        return 0.0;
    }
    let intersection = agent.intersection(gold).count();
    intersection as f32 / agent.len() as f32
}

#[allow(clippy::cast_precision_loss)]
fn size_ratio(agent_lines_changed: u32, gold_lines_changed: u32) -> f32 {
    if gold_lines_changed == 0 {
        if agent_lines_changed == 0 { 1.0 } else { 0.0 }
    } else {
        agent_lines_changed as f32 / gold_lines_changed as f32
    }
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use crate::error::Error;

    use super::{PatchClassifiers, score_patch};

    #[test]
    fn classifier_toml_drives_generated_detection() -> Result<(), Error> {
        let classifiers = PatchClassifiers::from_toml_str(
            r#"
test_files = []
lock_or_generated_files = ["**/*.snap"]
"#,
        )?;
        let stats = score_patch(
            "diff --git a/src/view.snap b/src/view.snap\n--- a/src/view.snap\n+++ b/src/view.snap\n@@ -1 +1 @@\n-old\n+new\n",
            &classifiers,
            None,
        );
        assert!(stats.touches_lock_or_generated);
        Ok(())
    }

    #[test]
    fn test_submission_class_scoring() -> Result<(), Error> {
        use super::SubmissionClass;

        let classifiers = PatchClassifiers::from_toml_str(
            r#"
test_files = ["tests/**/*.rs", "src/test_helpers.rs"]
lock_or_generated_files = []
"#,
        )?;

        // 1. Prod only patch
        let patch_prod = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let stats = score_patch(patch_prod, &classifiers, None);
        assert_eq!(stats.submission_class, Some(SubmissionClass::ProdOnly));

        // 2. Test only patch
        let patch_test = "diff --git a/tests/test_lib.rs b/tests/test_lib.rs\n--- a/tests/test_lib.rs\n+++ b/tests/test_lib.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let stats = score_patch(patch_test, &classifiers, None);
        assert_eq!(stats.submission_class, Some(SubmissionClass::TestOnly));

        // 3. Mixed patch
        let patch_mixed = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/tests/test_lib.rs b/tests/test_lib.rs\n--- a/tests/test_lib.rs\n+++ b/tests/test_lib.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let stats = score_patch(patch_mixed, &classifiers, None);
        assert_eq!(stats.submission_class, Some(SubmissionClass::Mixed));

        // 4. No hunks but contains files (e.g. rename, mode change, metadata-only)
        let patch_nonempty = "diff --git a/src/lib.rs b/src/lib.rs\n";
        let stats = score_patch(patch_nonempty, &classifiers, None);
        assert_eq!(stats.submission_class, Some(SubmissionClass::ProdOnly));

        // 5. Completely empty patch (zero files, zero hunks)
        let patch_empty = "";
        let stats = score_patch(patch_empty, &classifiers, None);
        assert_eq!(stats.submission_class, Some(SubmissionClass::Empty));

        Ok(())
    }
}
