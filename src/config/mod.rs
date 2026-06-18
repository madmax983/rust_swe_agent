//! Config loading: start from the embedded default TOML, then overlay any
//! user-specified TOML on top via recursive merge. `extends: <path>` inside
//! a config pulls in a parent first (same merge rule).

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use crate::error::ConfigError;

pub mod schema;

pub use schema::{
    AgentCfg, AgentKind, EnvCfg, EnvKind, McpServerCfg, ModelCfg, NetworkMode, PromptCfg,
    RedactionCfg, RootCfg, SkillCfg, SweepCfg, ToolCfg, ToolHookCfg, ToolHooksCfg,
};

const DEFAULT_TOML: &str = include_str!("defaults/default.toml");
const MAX_INCLUDE_DEPTH: usize = 16;

#[derive(Debug, Clone)]
pub struct Config {
    pub root: RootCfg,
    /// The raw merged config as JSON-shaped value. Templates can be resolved
    /// against fields we don't know about.
    pub raw: Value,
}

impl Config {
    /// Load the embedded default config only.
    pub fn defaults() -> Result<Self, ConfigError> {
        let v = toml_to_json(DEFAULT_TOML)?;
        Self::from_merged_value(v)
    }

    /// Load a user TOML, resolving `extends:` chains. Defaults are the base.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let defaults = toml_to_json(DEFAULT_TOML)?;
        let user = load_with_extends(path, 0)?;
        let merged = recursive_merge(defaults, user);
        Self::from_merged_value(merged)
    }

    /// Construct from a TOML string, starting from defaults. Used in tests.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let defaults = toml_to_json(DEFAULT_TOML)?;
        let user = toml_to_json(s)?;
        let merged = recursive_merge(defaults, user);
        Self::from_merged_value(merged)
    }

    fn from_merged_value(merged: Value) -> Result<Self, ConfigError> {
        let root: RootCfg = serde_json::from_value(merged.clone())
            .map_err(|e| ConfigError::Invalid(e.to_string()))?;
        validate_root(&root)?;
        Ok(Self { root, raw: merged })
    }
}

fn validate_root(root: &RootCfg) -> Result<(), ConfigError> {
    validate_agent_tools(root)?;
    validate_mcp_servers(root)?;
    validate_skills(root)?;
    for pattern in &root.agent.test_command_patterns {
        Regex::new(pattern).map_err(|err| {
            ConfigError::Invalid(format!(
                "invalid agent.test_command_patterns regex {pattern:?}: {err}"
            ))
        })?;
    }
    for pattern in &root.redaction.custom_patterns {
        Regex::new(pattern).map_err(|err| {
            ConfigError::Invalid(format!(
                "invalid redaction.custom_patterns regex {pattern:?}: {err}"
            ))
        })?;
    }
    Ok(())
}

fn validate_skills(root: &RootCfg) -> Result<(), ConfigError> {
    if root.skills.enabled && root.skills.max_active == 0 {
        return Err(ConfigError::Invalid(
            "skills.max_active must be greater than 0 when skills.enabled=true".into(),
        ));
    }
    if root.skills.paths.iter().any(|path| path.trim().is_empty()) {
        return Err(ConfigError::Invalid(
            "skills.paths entries cannot be empty".into(),
        ));
    }
    Ok(())
}

fn validate_mcp_servers(root: &RootCfg) -> Result<(), ConfigError> {
    for server in &root.agent.mcp_servers {
        if server.command.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "agent.mcp_servers command cannot be empty".into(),
            ));
        }
    }
    Ok(())
}

fn validate_agent_tools(root: &RootCfg) -> Result<(), ConfigError> {
    let mut seen = std::collections::BTreeSet::new();
    for tool in &root.agent.tools {
        crate::tool::validate_tool_name(&tool.name).map_err(|err| {
            ConfigError::Invalid(format!("invalid agent.tools name {:?}: {err}", tool.name))
        })?;
        if tool.name == crate::tool::BASH_TOOL_NAME {
            return Err(ConfigError::Invalid(
                "agent.tools cannot redefine built-in tool `bash`".into(),
            ));
        }
        if !seen.insert(tool.name.clone()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate agent.tools entry {:?}",
                tool.name
            )));
        }
    }
    Ok(())
}

fn load_with_extends(path: &Path, depth: usize) -> Result<Value, ConfigError> {
    if depth >= MAX_INCLUDE_DEPTH {
        return Err(ConfigError::IncludeDepthExceeded(MAX_INCLUDE_DEPTH));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|_| ConfigError::NotFound(path.display().to_string()))?;
    let v = toml_to_json(&text)?;

    if let Some(parent_path) = v.get("extends").and_then(Value::as_str) {
        let parent_pb = resolve_relative(path, parent_path);
        let parent = load_with_extends(&parent_pb, depth + 1)?;
        let mut merged = recursive_merge(parent, v);
        // Strip the `extends` pointer once resolved — it has no runtime meaning.
        if let Value::Object(m) = &mut merged {
            m.remove("extends");
        }
        Ok(merged)
    } else {
        Ok(v)
    }
}

fn resolve_relative(base: &Path, target: &str) -> PathBuf {
    let target_pb = PathBuf::from(target);
    if target_pb.is_absolute() {
        target_pb
    } else {
        base.parent()
            .map_or_else(|| target_pb.clone(), |p| p.join(target))
    }
}

fn toml_to_json(s: &str) -> Result<Value, ConfigError> {
    let toml_val: toml::Value = toml::from_str(s)?;
    serde_json::to_value(toml_val).map_err(|e| ConfigError::Invalid(e.to_string()))
}

/// Deep merge: `overlay` wins for non-object leaves; object keys recurse.
/// Arrays are replaced wholesale — mirrors mini-swe-agent's Python merge.
pub fn recursive_merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(mut b), Value::Object(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(existing) => recursive_merge(existing, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            Value::Object(b)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn defaults_parse() {
        let c = Config::defaults().unwrap();
        assert_eq!(c.root.agent.step_limit, 50);
        assert_eq!(c.root.model.name, "claude-opus-4-7");
        assert!(matches!(c.root.environment.kind, EnvKind::Local));
    }

    #[test]
    fn user_overrides_default() {
        let toml = r#"
[agent]
step_limit = 10

[model]
name = "claude-sonnet-4-6"
"#;
        let c = Config::from_toml_str(toml).unwrap();
        assert_eq!(c.root.agent.step_limit, 10);
        assert_eq!(c.root.model.name, "claude-sonnet-4-6");
        // Unchanged default survives.
        assert_eq!(c.root.model.max_tokens, 4096);
    }

    #[test]
    fn recursive_merge_leaves_prefer_overlay() {
        let base = serde_json::json!({"a": {"x": 1, "y": 2}, "b": 3});
        let overlay = serde_json::json!({"a": {"y": 20, "z": 30}, "c": 4});
        let merged = recursive_merge(base, overlay);
        assert_eq!(
            merged,
            serde_json::json!({"a": {"x": 1, "y": 20, "z": 30}, "b": 3, "c": 4})
        );
    }

    #[test]
    fn invalid_toml_returns_toml_error() {
        let err = Config::from_toml_str("[[[ not valid toml").unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)));
    }

    #[test]
    fn invalid_test_command_pattern_returns_config_error() {
        let err = Config::from_toml_str("[agent]\ntest_command_patterns = [\"(\"]").unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(
            err.to_string().contains("agent.test_command_patterns"),
            "{err}"
        );
    }

    #[test]
    fn invalid_agent_tool_names_return_config_error() {
        let err = Config::from_toml_str("[[agent.tools]]\nname = \"bash\"\ncommand = \"echo no\"")
            .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(err.to_string().contains("built-in tool `bash`"), "{err}");

        let err =
            Config::from_toml_str("[[agent.tools]]\nname = \"bad name\"\ncommand = \"echo no\"")
                .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(
            err.to_string().contains("invalid agent.tools name"),
            "{err}"
        );
    }

    #[test]
    fn recursive_merge_replaces_arrays() {
        let base = serde_json::json!({"xs": [1, 2, 3]});
        let overlay = serde_json::json!({"xs": [9]});
        let merged = recursive_merge(base, overlay);
        assert_eq!(merged, serde_json::json!({"xs": [9]}));
    }
}
