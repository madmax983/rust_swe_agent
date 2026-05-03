//! Config loading: start from the embedded default TOML, then overlay any
//! user-specified TOML on top via recursive merge. `extends: <path>` inside
//! a config pulls in a parent first (same merge rule).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::ConfigError;

pub mod schema;

pub use schema::{AgentCfg, AgentKind, EnvCfg, EnvKind, ModelCfg, PromptCfg, RootCfg, SweepCfg};

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
        let root: RootCfg =
            serde_json::from_value(v.clone()).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        Ok(Self { root, raw: v })
    }

    /// Load a user TOML, resolving `extends:` chains. Defaults are the base.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let defaults = toml_to_json(DEFAULT_TOML)?;
        let user = load_with_extends(path, 0)?;
        let merged = recursive_merge(defaults, user);
        let root: RootCfg = serde_json::from_value(merged.clone())
            .map_err(|e| ConfigError::Invalid(e.to_string()))?;
        Ok(Self { root, raw: merged })
    }

    /// Construct from a TOML string, starting from defaults. Used in tests.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let defaults = toml_to_json(DEFAULT_TOML)?;
        let user = toml_to_json(s)?;
        let merged = recursive_merge(defaults, user);
        let root: RootCfg = serde_json::from_value(merged.clone())
            .map_err(|e| ConfigError::Invalid(e.to_string()))?;
        Ok(Self { root, raw: merged })
    }
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
    fn recursive_merge_replaces_arrays() {
        let base = serde_json::json!({"xs": [1, 2, 3]});
        let overlay = serde_json::json!({"xs": [9]});
        let merged = recursive_merge(base, overlay);
        assert_eq!(merged, serde_json::json!({"xs": [9]}));
    }

    #[test]
    fn config_load_resolves_extends() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let parent_path = dir.path().join("parent.toml");
        let mut parent_file = std::fs::File::create(&parent_path).unwrap();
        writeln!(
            parent_file,
            "[agent]\nstep_limit = 99\n[model]\nname = \"parent-model\""
        )
        .unwrap();

        let child_path = dir.path().join("child.toml");
        let mut child_file = std::fs::File::create(&child_path).unwrap();
        writeln!(
            child_file,
            "extends = \"parent.toml\"\n[model]\nname = \"child-model\""
        )
        .unwrap();

        let c = Config::load(&child_path).unwrap();
        assert_eq!(c.root.agent.step_limit, 99); // from parent
        assert_eq!(c.root.model.name, "child-model"); // overridden by child

        // The raw object should not have extends
        assert!(c.raw.get("extends").is_none());
    }

    #[test]
    fn config_load_not_found() {
        let c = Config::load(Path::new("/does/not/exist/ever.toml"));
        assert!(matches!(c, Err(ConfigError::NotFound(_))));
    }

    #[test]
    fn config_load_exceeds_depth() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let a_path = dir.path().join("a.toml");
        let b_path = dir.path().join("b.toml");

        let mut a_file = std::fs::File::create(&a_path).unwrap();
        writeln!(a_file, "extends = \"b.toml\"").unwrap();

        let mut b_file = std::fs::File::create(&b_path).unwrap();
        writeln!(b_file, "extends = \"a.toml\"").unwrap();

        let err = Config::load(&a_path).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::IncludeDepthExceeded(MAX_INCLUDE_DEPTH)
        ));
    }

    #[test]
    fn resolve_relative_handles_absolute_and_relative() {
        let base = PathBuf::from("/etc/swe/config.toml");

        let abs_target = "/var/lib/other.toml";
        let pb_abs = resolve_relative(&base, abs_target);
        assert_eq!(pb_abs, PathBuf::from(abs_target));

        let rel_target = "sibling.toml";
        let pb_rel = resolve_relative(&base, rel_target);
        assert_eq!(pb_rel, PathBuf::from("/etc/swe/sibling.toml"));
    }

    #[test]
    fn resolve_relative_handles_base_without_parent() {
        let base = PathBuf::from("/");
        let rel_target = "sibling.toml";
        let pb_rel = resolve_relative(&base, rel_target);
        assert_eq!(pb_rel, PathBuf::from("sibling.toml"));
    }
}
