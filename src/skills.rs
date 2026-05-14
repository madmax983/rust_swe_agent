//! Harness-level agent skill discovery and activation.
//!
//! The registry is private context owned by the harness: it scans skill
//! manifests, resolves the relevant subset for a task, and loads only those
//! selected skill bodies into model-visible context.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::SkillCfg;
use crate::error::{ConfigError, Error};
use crate::redaction::{Redactor, surface};
use crate::trajectory::TrajectoryInfo;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip)]
    normalized_name: String,
    #[serde(skip)]
    search_tokens: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    manifests: Vec<SkillManifest>,
}

/// Indicates how a specific skill was selected for inclusion in the context.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillActivationReason {
    /// The skill was explicitly named in the agent's instructions or task context.
    ExplicitMention,
    /// The skill was automatically selected based on heuristics (like repository patterns).
    AutoMatch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub content: String,
    pub sha256: String,
    pub activation_reason: SkillActivationReason,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveSkillSet {
    pub skills: Vec<ActiveSkill>,
}

#[derive(Debug, Clone, Copy)]
pub struct SkillResolveRequest<'a> {
    pub task: &'a str,
    pub auto_load: bool,
    pub max_active: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveSkillManifest {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub sha256: String,
    pub activation_reason: SkillActivationReason,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSkillContext {
    pub active_skills: ActiveSkillSet,
    pub merged_extra_context: Option<String>,
}

impl SkillRegistry {
    pub fn scan_paths(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self, Error> {
        let mut skill_files = Vec::new();
        for path in paths {
            if !path.exists() {
                continue;
            }
            collect_skill_files(&path, &mut skill_files)?;
        }
        skill_files.sort();

        let mut manifests = Vec::new();
        let mut seen_names = BTreeSet::new();
        for path in skill_files {
            let text = std::fs::read_to_string(&path)?;
            let frontmatter = parse_frontmatter(&path, &text)?;
            let name = required_frontmatter(&path, &frontmatter, "name")?;
            let description = required_frontmatter(&path, &frontmatter, "description")?;
            let manifest = SkillManifest {
                normalized_name: normalize_search_text(&name),
                search_tokens: searchable_tokens(&name, &description),
                name,
                description,
                version: frontmatter.get("version").cloned(),
                path,
            };
            if !seen_names.insert(manifest.name.clone()) {
                return Err(Error::Config(ConfigError::Invalid(format!(
                    "duplicate skill name `{}`",
                    manifest.name
                ))));
            }
            manifests.push(manifest);
        }

        Ok(Self { manifests })
    }

    pub fn manifests(&self) -> &[SkillManifest] {
        &self.manifests
    }

    pub fn resolve(&self, request: SkillResolveRequest<'_>) -> Result<ActiveSkillSet, Error> {
        if request.max_active == 0 {
            return Ok(ActiveSkillSet::default());
        }

        let mut selected = Vec::new();
        let mut seen = BTreeSet::new();
        let normalized_task = normalize_search_text(request.task);
        let task_tokens = tokenize(&normalized_task)
            .filter(|token| !is_stopword(token))
            .collect::<BTreeSet<_>>();

        for manifest in &self.manifests {
            if mentioned_explicitly(&normalized_task, manifest) {
                selected.push(load_active_skill(
                    manifest,
                    SkillActivationReason::ExplicitMention,
                )?);
                seen.insert(manifest.name.clone());
                if selected.len() >= request.max_active {
                    return Ok(ActiveSkillSet { skills: selected });
                }
            }
        }

        if request.auto_load {
            let mut scored = self
                .manifests
                .iter()
                .filter(|manifest| !seen.contains(&manifest.name))
                .filter_map(|manifest| {
                    let score = match_score(&task_tokens, manifest);
                    (score >= 2).then_some((score, manifest))
                })
                .collect::<Vec<_>>();
            scored.sort_by(|(score_a, manifest_a), (score_b, manifest_b)| {
                score_b
                    .cmp(score_a)
                    .then_with(|| manifest_a.name.cmp(&manifest_b.name))
            });

            for (_, manifest) in scored {
                selected.push(load_active_skill(
                    manifest,
                    SkillActivationReason::AutoMatch,
                )?);
                if selected.len() >= request.max_active {
                    break;
                }
            }
        }

        Ok(ActiveSkillSet { skills: selected })
    }
}

impl ActiveSkillSet {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn render_context(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut rendered = String::from(
            "Active agent skills:\nThe harness selected these instructions for this task. Follow them when relevant.\n",
        );
        for skill in &self.skills {
            rendered.push_str("\n---\n");
            rendered.push_str("Skill: ");
            rendered.push_str(&skill.name);
            rendered.push('\n');
            rendered.push_str("Activation: ");
            rendered.push_str(match skill.activation_reason {
                SkillActivationReason::ExplicitMention => "explicit_mention",
                SkillActivationReason::AutoMatch => "auto_match",
            });
            rendered.push_str("\n\n");
            rendered.push_str(skill.content.trim());
            rendered.push('\n');
        }
        rendered
    }

    pub fn merge_extra_context(&self, extra_context: Option<String>) -> Option<String> {
        let skill_context = self.render_context();
        if skill_context.is_empty() {
            return extra_context;
        }
        Some(match extra_context {
            Some(extra) if !extra.trim().is_empty() => format!("{extra}\n\n{skill_context}"),
            _ => skill_context,
        })
    }

    pub fn provenance(&self) -> Vec<ActiveSkillManifest> {
        self.skills
            .iter()
            .map(|skill| ActiveSkillManifest {
                name: skill.name.clone(),
                description: skill.description.clone(),
                path: skill.path.clone(),
                sha256: skill.sha256.clone(),
                activation_reason: skill.activation_reason,
            })
            .collect()
    }

    pub fn redacted_provenance_value(
        &self,
        redactor: &Redactor,
    ) -> Result<serde_json::Value, Error> {
        let mut value = serde_json::to_value(self.provenance())?;
        redactor.redact_json_value(&mut value, surface::TRAJECTORY);
        Ok(value)
    }

    pub fn record_redacted_provenance(
        &self,
        info: &mut TrajectoryInfo,
        redactor: &Redactor,
    ) -> Result<(), Error> {
        if self.is_empty() {
            return Ok(());
        }
        info.other.insert(
            "active_skills".into(),
            self.redacted_provenance_value(redactor)?,
        );
        Ok(())
    }
}

pub fn resolve_for_task(
    cfg: &SkillCfg,
    task: &str,
    extra_context: Option<String>,
) -> Result<ResolvedSkillContext, Error> {
    if !cfg.enabled || cfg.paths.is_empty() {
        return Ok(ResolvedSkillContext {
            active_skills: ActiveSkillSet::default(),
            merged_extra_context: extra_context,
        });
    }

    let paths = cfg.paths.iter().map(expand_skill_path).collect::<Vec<_>>();
    let registry = SkillRegistry::scan_paths(paths)?;
    let active_skills = registry.resolve(SkillResolveRequest {
        task,
        auto_load: cfg.auto_load,
        max_active: cfg.max_active,
    })?;
    let merged_extra_context = active_skills.merge_extra_context(extra_context);
    Ok(ResolvedSkillContext {
        active_skills,
        merged_extra_context,
    })
}

fn collect_skill_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), Error> {
    if path.is_file() {
        if path
            .file_name()
            .is_some_and(|name| name == OsStr::new("SKILL.md"))
        {
            files.push(path.to_path_buf());
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_skill_files(&entry.path(), files)?;
        } else if file_type.is_file() && entry.file_name() == OsStr::new("SKILL.md") {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn parse_frontmatter(path: &Path, text: &str) -> Result<BTreeMap<String, String>, Error> {
    let Some(rest) = text.strip_prefix("---") else {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "skill `{}` missing YAML frontmatter",
            path.display()
        ))));
    };
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .ok_or_else(|| {
            Error::Config(ConfigError::Invalid(format!(
                "skill `{}` has malformed frontmatter start",
                path.display()
            )))
        })?;
    let Some((frontmatter, _body)) = split_frontmatter(rest) else {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "skill `{}` missing closing frontmatter fence",
            path.display()
        ))));
    };

    Ok(parse_frontmatter_fields(frontmatter))
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    for marker in ["\n---\r\n", "\n---\n"] {
        if let Some(pos) = text.find(marker) {
            let body_start = pos + marker.len();
            return Some((&text[..pos], &text[body_start..]));
        }
    }
    if let Some(pos) = text
        .strip_suffix("\n---")
        .map(|_| text.len() - "\n---".len())
    {
        return Some((&text[..pos], ""));
    }
    None
}

fn parse_frontmatter_fields(frontmatter: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut lines = frontmatter.lines().peekable();

    while let Some(line) = lines.next() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.chars().next().is_some_and(char::is_whitespace) {
            continue;
        }
        let Some((key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_owned();
        let mut value = strip_trailing_comment(raw_value.trim());
        if value.starts_with('"') && !ends_with_unescaped_quote(&value) {
            while let Some(next) = lines.peek() {
                let next_value = strip_trailing_comment(next.trim_end());
                value.push('\n');
                value.push_str(&next_value);
                let done = ends_with_unescaped_quote(next_value.trim());
                let _ = lines.next();
                if done {
                    break;
                }
            }
        }
        fields.insert(key, unquote_scalar(&value));
    }

    fields
}

fn strip_trailing_comment(value: &str) -> String {
    let mut quote = None;
    let mut escaped = false;

    for (idx, ch) in value.char_indices() {
        match (quote, ch) {
            (Some('"'), '\\') if !escaped => {
                escaped = true;
                continue;
            }
            (Some(active), _) if ch == active && !escaped => quote = None,
            (None, '"' | '\'') => quote = Some(ch),
            (None, '#') => return value[..idx].trim_end().to_owned(),
            _ => {}
        }
        escaped = false;
    }

    value.trim_end().to_owned()
}

fn required_frontmatter(
    path: &Path,
    frontmatter: &BTreeMap<String, String>,
    key: &str,
) -> Result<String, Error> {
    frontmatter
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| {
            Error::Config(ConfigError::Invalid(format!(
                "skill `{}` missing required `{key}` frontmatter",
                path.display()
            )))
        })
}

fn unquote_scalar(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

fn ends_with_unescaped_quote(value: &str) -> bool {
    let mut backslashes = 0usize;
    for ch in value.chars().rev().skip(1) {
        if ch == '\\' {
            backslashes += 1;
        } else {
            break;
        }
    }
    value.ends_with('"') && backslashes % 2 == 0
}

fn mentioned_explicitly(normalized_task: &str, manifest: &SkillManifest) -> bool {
    let normalized_name = &manifest.normalized_name;
    ["$", "@", "/"].iter().any(|marker| {
        contains_explicit_mention(normalized_task, &format!("{marker}{normalized_name}"))
    })
}

fn contains_explicit_mention(normalized_task: &str, needle: &str) -> bool {
    let mut search_from = 0;
    while let Some(offset) = normalized_task[search_from..].find(needle) {
        let start = search_from + offset;
        let end = start + needle.len();
        if is_explicit_mention_start_boundary(normalized_task[..start].chars().next_back())
            && is_explicit_mention_end_boundary(normalized_task[end..].chars().next())
        {
            return true;
        }
        search_from = end;
    }
    false
}

fn is_explicit_mention_start_boundary(previous: Option<char>) -> bool {
    match previous {
        None => true,
        Some(ch) => ch.is_whitespace(),
    }
}

fn is_explicit_mention_end_boundary(next: Option<char>) -> bool {
    match next {
        None => true,
        Some(ch) => ch.is_whitespace(),
    }
}

fn match_score(task_tokens: &BTreeSet<String>, manifest: &SkillManifest) -> usize {
    task_tokens.intersection(&manifest.search_tokens).count()
}

fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split_whitespace().map(ToOwned::to_owned)
}

fn searchable_tokens(name: &str, description: &str) -> BTreeSet<String> {
    let search_text = normalize_search_text(&format!("{name} {description}"));
    tokenize(&search_text)
        .filter(|token| !is_stopword(token))
        .collect()
}

fn normalize_search_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '$' || ch == '@' || ch == '/' {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "and"
            | "for"
            | "the"
            | "this"
            | "that"
            | "use"
            | "uses"
            | "with"
            | "work"
            | "when"
            | "should"
            | "must"
            | "all"
            | "any"
    )
}

fn load_active_skill(
    manifest: &SkillManifest,
    activation_reason: SkillActivationReason,
) -> Result<ActiveSkill, Error> {
    let full_text = std::fs::read_to_string(&manifest.path)?;
    let content = skill_body(&full_text)
        .map(str::trim)
        .filter(|body| !body.is_empty())
        .unwrap_or_else(|| full_text.trim())
        .to_owned();
    Ok(ActiveSkill {
        name: manifest.name.clone(),
        description: manifest.description.clone(),
        path: manifest.path.clone(),
        sha256: sha256_hex(full_text.as_bytes()),
        activation_reason,
        content,
    })
}

fn skill_body(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let (_, body) = split_frontmatter(rest)?;
    Some(body)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn expand_skill_path(path: &String) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(path)
}
