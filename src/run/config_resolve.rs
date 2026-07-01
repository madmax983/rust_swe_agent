//! Zero-cost `agent config resolve` — print the fully-resolved effective
//! run configuration annotated with provenance (issue #500).
//!
//! No model calls are made; no network I/O is performed. Secret-bearing
//! string values are redacted using the configured redactor, consistent with
//! `agent env preview`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::error::ConfigError;
use crate::redaction::Redactor;

// ── Public args ───────────────────────────────────────────────────────────────

/// Arguments passed to [`run_config_resolve`].
#[derive(Debug, Clone)]
pub struct ConfigResolveArgs {
    pub config: Option<PathBuf>,
    /// Explicit `--model` flag value; `None` means the flag was not passed.
    pub model_flag: Option<String>,
    /// Explicit `--step-limit` flag value; `None` means the flag was not passed.
    pub step_limit_flag: Option<u32>,
    pub observation_max_bytes_flag: Option<usize>,
    pub observation_head_ratio_flag: Option<f64>,
    pub per_task_budget_usd_flag: Option<f64>,
    /// Mirrors `mini --hide-budget-from-agent`.
    pub hide_budget_from_agent_flag: bool,
    /// Mirrors `mini --env local|docker`.
    pub env_flag: Option<String>,
    /// Mirrors `mini --workdir` (path already validated/canonicalized by caller).
    pub workdir_flag: Option<PathBuf>,
    /// Mirrors `mini --detect-stagnation`.
    pub detect_stagnation_flag: Option<bool>,
    /// Mirrors `mini --stagnation-repeat-threshold`.
    pub stagnation_repeat_threshold_flag: Option<u32>,
    /// Mirrors `mini --stagnation-window`.
    pub stagnation_window_flag: Option<u32>,
}

// ── Output types ──────────────────────────────────────────────────────────────

/// Which precedence layer set this config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvenanceLayer {
    /// Built-in compiled default (no file or flag override).
    Default,
    /// Set by the `--config` TOML file (or its `extends:` chain).
    File,
    /// Set by an environment variable override.
    Env,
    /// Set by an explicit CLI flag on this invocation.
    Flag,
}

/// A single resolved scalar config field annotated with its provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedField {
    pub key: String,
    pub value: Value,
    pub layer: ProvenanceLayer,
}

/// A detected clap-default override hazard.
///
/// Emitted when a `--config` file sets a field that a clap default in
/// `mini` or `bench swebench` will silently overwrite unless the
/// corresponding flag is also passed explicitly on that invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideHazard {
    pub field: String,
    /// The value your config file intended.
    pub file_value: Value,
    /// The clap default that will win in `mini`/`bench swebench`.
    pub clap_default_value: Value,
    /// Which subcommands are affected by this hazard.
    pub commands_affected: Vec<String>,
    pub message: String,
}

/// The complete resolved config report returned by [`run_config_resolve`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigResolveReport {
    pub schema_version: u32,
    /// Resolved scalar fields in deterministic order.
    pub fields: Vec<ResolvedField>,
    /// Clap-default override hazards; empty ⟹ no silent overrides detected.
    pub hazards: Vec<OverrideHazard>,
    /// `true` when `hazards` is non-empty.
    pub has_hazards: bool,
}

// ── Clap-default constants ────────────────────────────────────────────────────

/// Clap default for `--model` in `mini` and `bench swebench`.
pub(crate) const CLAP_DEFAULT_MODEL: &str = "claude-opus-4-7";
/// Clap default for `--step-limit` in `bench swebench`.
const CLAP_DEFAULT_STEP_LIMIT: u64 = 50;

// ── Main function ─────────────────────────────────────────────────────────────

/// Resolve the effective run configuration with per-field provenance.
///
/// Performs no model calls and no network I/O. Secret-bearing string values
/// are redacted through the configured `[redaction]` policy.
pub fn run_config_resolve(args: &ConfigResolveArgs) -> Result<ConfigResolveReport, ConfigError> {
    let defaults_cfg = Config::defaults()?;
    // Serialize the typed root so serde-default fields (e.g. detect_stagnation=true)
    // are included even when absent from default.toml.
    let defaults_json = serde_json::to_value(&defaults_cfg.root)
        .map_err(|e| ConfigError::Invalid(e.to_string()))?;

    // Reuse the already-loaded defaults when no config file is provided.
    let merged_cfg = match &args.config {
        Some(path) => Config::load(path)?,
        None => defaults_cfg,
    };
    let merged_json =
        serde_json::to_value(&merged_cfg.root).map_err(|e| ConfigError::Invalid(e.to_string()))?;

    let redactor = Redactor::from_config_lossy(&merged_cfg.root.redaction);

    // Mirror DefaultAgent's stagnation validation: resolve effective values
    // (flag overrides config), then check bounds only when detection is enabled.
    let effective_detect = args
        .detect_stagnation_flag
        .unwrap_or(merged_cfg.root.agent.detect_stagnation);
    if effective_detect {
        let k = args
            .stagnation_repeat_threshold_flag
            .unwrap_or(merged_cfg.root.agent.stagnation_repeat_threshold);
        let w = args
            .stagnation_window_flag
            .unwrap_or(merged_cfg.root.agent.stagnation_window);
        if k == 0 {
            return Err(ConfigError::Invalid(
                "--stagnation-repeat-threshold must be >= 1".into(),
            ));
        }
        if w == 0 {
            return Err(ConfigError::Invalid(
                "--stagnation-window must be >= 1".into(),
            ));
        }
        if w < k {
            return Err(ConfigError::Invalid(format!(
                "--stagnation-window ({w}) must be >= --stagnation-repeat-threshold ({k})"
            )));
        }
    }

    let fields = build_resolved_fields(&defaults_json, &merged_json, args, &redactor);
    let hazards = detect_hazards(&merged_json, args, &redactor);
    let has_hazards = !hazards.is_empty();

    Ok(ConfigResolveReport {
        schema_version: 1,
        fields,
        hazards,
        has_hazards,
    })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

struct FieldDef {
    path: &'static [&'static str],
    key: &'static str,
}

const SCALAR_FIELDS: &[FieldDef] = &[
    FieldDef {
        path: &["model", "name"],
        key: "model.name",
    },
    FieldDef {
        path: &["model", "max_tokens"],
        key: "model.max_tokens",
    },
    FieldDef {
        path: &["model", "temperature"],
        key: "model.temperature",
    },
    FieldDef {
        path: &["agent", "step_limit"],
        key: "agent.step_limit",
    },
    FieldDef {
        path: &["agent", "per_task_budget_usd"],
        key: "agent.per_task_budget_usd",
    },
    FieldDef {
        path: &["agent", "cost_limit_usd"],
        key: "agent.cost_limit_usd",
    },
    FieldDef {
        path: &["agent", "hide_budget_from_agent"],
        key: "agent.hide_budget_from_agent",
    },
    FieldDef {
        path: &["agent", "observation_max_bytes"],
        key: "agent.observation_max_bytes",
    },
    FieldDef {
        path: &["agent", "observation_head_ratio"],
        key: "agent.observation_head_ratio",
    },
    FieldDef {
        path: &["agent", "detect_stagnation"],
        key: "agent.detect_stagnation",
    },
    FieldDef {
        path: &["agent", "stagnation_repeat_threshold"],
        key: "agent.stagnation_repeat_threshold",
    },
    FieldDef {
        path: &["agent", "stagnation_window"],
        key: "agent.stagnation_window",
    },
    FieldDef {
        path: &["agent", "parse_error_retries"],
        key: "agent.parse_error_retries",
    },
    FieldDef {
        path: &["environment", "kind"],
        key: "environment.kind",
    },
    FieldDef {
        path: &["environment", "timeout_secs"],
        key: "environment.timeout_secs",
    },
    FieldDef {
        path: &["environment", "workdir"],
        key: "environment.workdir",
    },
    FieldDef {
        path: &["environment", "network_mode"],
        key: "environment.network_mode",
    },
];

fn build_resolved_fields(
    defaults_json: &Value,
    merged_json: &Value,
    args: &ConfigResolveArgs,
    redactor: &Redactor,
) -> Vec<ResolvedField> {
    SCALAR_FIELDS
        .iter()
        .map(|field_def| {
            let default_val = get_nested(defaults_json, field_def.path)
                .cloned()
                .unwrap_or(Value::Null);
            let merged_val = get_nested(merged_json, field_def.path)
                .cloned()
                .unwrap_or(Value::Null);

            if let Some(flag_val) = get_flag_override(field_def.key, args) {
                ResolvedField {
                    key: field_def.key.to_string(),
                    value: redact_value(flag_val, redactor),
                    layer: ProvenanceLayer::Flag,
                }
            } else if merged_val != default_val {
                ResolvedField {
                    key: field_def.key.to_string(),
                    value: redact_value(merged_val, redactor),
                    layer: ProvenanceLayer::File,
                }
            } else {
                ResolvedField {
                    key: field_def.key.to_string(),
                    value: redact_value(default_val, redactor),
                    layer: ProvenanceLayer::Default,
                }
            }
        })
        .collect()
}

/// Map a dot-key config field name to any CLI flag override value.
fn get_flag_override(key: &str, args: &ConfigResolveArgs) -> Option<Value> {
    match key {
        "model.name" => args.model_flag.as_ref().map(|v| Value::String(v.clone())),
        "agent.step_limit" => args.step_limit_flag.map(|v| Value::Number(v.into())),
        "agent.observation_max_bytes" => args
            .observation_max_bytes_flag
            .map(|v| Value::Number(v.into())),
        "agent.observation_head_ratio" => args
            .observation_head_ratio_flag
            .and_then(|v| serde_json::Number::from_f64(v).map(Value::Number)),
        "agent.per_task_budget_usd" => args
            .per_task_budget_usd_flag
            .and_then(|v| serde_json::Number::from_f64(v).map(Value::Number)),
        "agent.hide_budget_from_agent" => {
            if args.hide_budget_from_agent_flag {
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        "agent.detect_stagnation" => args.detect_stagnation_flag.map(Value::Bool),
        "agent.stagnation_repeat_threshold" => args
            .stagnation_repeat_threshold_flag
            .map(|v| Value::Number(v.into())),
        "agent.stagnation_window" => args.stagnation_window_flag.map(|v| Value::Number(v.into())),
        "environment.kind" => args.env_flag.as_ref().map(|v| Value::String(v.clone())),
        "environment.workdir" => args
            .workdir_flag
            .as_ref()
            .map(|v| Value::String(v.to_string_lossy().into_owned())),
        _ => None,
    }
}

fn detect_hazards(
    merged_json: &Value,
    args: &ConfigResolveArgs,
    redactor: &Redactor,
) -> Vec<OverrideHazard> {
    let mut hazards = Vec::new();

    // Hazard 1: model.name
    // `mini` and `bench swebench` unconditionally write back their --model
    // clap default, so a config-file model.name is silently ignored unless
    // --model is also passed on the run invocation.
    let merged_model = get_nested(merged_json, &["model", "name"])
        .and_then(Value::as_str)
        .unwrap_or(CLAP_DEFAULT_MODEL);

    if merged_model != CLAP_DEFAULT_MODEL && args.model_flag.is_none() {
        let redacted_model = redactor.redact_text(merged_model, "config_resolve").text;
        hazards.push(OverrideHazard {
            field: "model.name".to_string(),
            file_value: Value::String(redacted_model.clone()),
            clap_default_value: Value::String(CLAP_DEFAULT_MODEL.to_string()),
            commands_affected: vec![
                "mini".to_string(),
                "bench swebench".to_string(),
                "bench rehearsal".to_string(),
                "bench forecast".to_string(),
                "bench doctor".to_string(),
                "agent stability".to_string(),
                "agent best-of".to_string(),
                "agent suite".to_string(),
            ],
            message: format!(
                "Config file sets model.name='{redacted_model}' but 'mini', \
                 'bench swebench', 'bench rehearsal', 'bench forecast', \
                 'bench doctor', 'agent stability', 'agent best-of', and \
                 'agent suite' unconditionally apply the clap default \
                 '{CLAP_DEFAULT_MODEL}' when --model is not explicitly passed; \
                 your config-file value is silently ignored."
            ),
        });
    }

    // Hazard 2: agent.step_limit
    // `bench swebench` has --step-limit with a clap default of 50; a
    // config-file step_limit is silently ignored unless --step-limit is passed.
    let merged_step = get_nested(merged_json, &["agent", "step_limit"])
        .and_then(Value::as_u64)
        .unwrap_or(CLAP_DEFAULT_STEP_LIMIT);

    if merged_step != CLAP_DEFAULT_STEP_LIMIT && args.step_limit_flag.is_none() {
        hazards.push(OverrideHazard {
            field: "agent.step_limit".to_string(),
            file_value: Value::Number(merged_step.into()),
            clap_default_value: Value::Number(CLAP_DEFAULT_STEP_LIMIT.into()),
            commands_affected: vec![
                "bench swebench".to_string(),
                "bench rehearsal".to_string(),
                "bench forecast".to_string(),
                "bench doctor".to_string(),
            ],
            message: format!(
                "Config file sets agent.step_limit={merged_step} but 'bench swebench', \
                 'bench rehearsal', 'bench forecast', and 'bench doctor' unconditionally \
                 apply the clap default {CLAP_DEFAULT_STEP_LIMIT} when --step-limit is not \
                 explicitly passed; your config-file value is silently ignored."
            ),
        });
    }

    hazards
}

/// Navigate a dot-path in a JSON value.
fn get_nested<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

/// Apply the redactor to string values; leave other types unchanged.
fn redact_value(value: Value, redactor: &Redactor) -> Value {
    match value {
        Value::String(s) => Value::String(redactor.redact_text(&s, "config_resolve").text),
        other => other,
    }
}

// ── Text formatter ────────────────────────────────────────────────────────────

/// Render a [`ConfigResolveReport`] as human-readable text (the `--format text` output).
#[must_use]
pub fn format_text(report: &ConfigResolveReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(out, "=== agent config resolve (no model call made) ===");
    let _ = writeln!(out);

    let _ = writeln!(out, "{:<42} {:<32} Layer", "Field", "Value");
    let _ = writeln!(out, "{}", "-".repeat(82));

    for field in &report.fields {
        let value_str = json_value_display(&field.value);
        let layer_str = match field.layer {
            ProvenanceLayer::Default => "default",
            ProvenanceLayer::File => "file",
            ProvenanceLayer::Env => "env",
            ProvenanceLayer::Flag => "flag",
        };
        let _ = writeln!(out, "{:<42} {:<32} {}", field.key, value_str, layer_str);
    }

    let _ = writeln!(out);
    if report.hazards.is_empty() {
        let _ = writeln!(out, "--- Hazards: NONE ---");
    } else {
        let _ = writeln!(out, "--- Hazards ---");
        for hazard in &report.hazards {
            let _ = writeln!(out, "[CLAP_DEFAULT_OVERRIDE] {}", hazard.field);
            let _ = writeln!(out, "  {}", hazard.message);
            let _ = writeln!(
                out,
                "  Affected commands: {}",
                hazard.commands_affected.join(", ")
            );
        }
    }

    out
}

fn json_value_display(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::fs;

    use super::*;

    fn no_args() -> ConfigResolveArgs {
        ConfigResolveArgs {
            config: None,
            model_flag: None,
            step_limit_flag: None,
            observation_max_bytes_flag: None,
            observation_head_ratio_flag: None,
            per_task_budget_usd_flag: None,
            hide_budget_from_agent_flag: false,
            env_flag: None,
            workdir_flag: None,
            detect_stagnation_flag: None,
            stagnation_repeat_threshold_flag: None,
            stagnation_window_flag: None,
        }
    }

    fn find_field<'a>(report: &'a ConfigResolveReport, key: &str) -> &'a ResolvedField {
        report
            .fields
            .iter()
            .find(|f| f.key == key)
            .unwrap_or_else(|| panic!("field '{key}' not found in report"))
    }

    // ── RED-phase tests ───────────────────────────────────────────────────────

    #[test]
    fn schema_version_is_1() {
        let report = run_config_resolve(&no_args()).unwrap();
        assert_eq!(report.schema_version, 1);
    }

    #[test]
    fn all_defaults_no_config_file_shows_default_layer() {
        let report = run_config_resolve(&no_args()).unwrap();
        let model_field = find_field(&report, "model.name");
        assert_eq!(model_field.layer, ProvenanceLayer::Default);
        assert_eq!(model_field.value.as_str().unwrap(), "claude-opus-4-7");
    }

    #[test]
    fn step_limit_defaults_to_50_with_default_layer() {
        let report = run_config_resolve(&no_args()).unwrap();
        let f = find_field(&report, "agent.step_limit");
        assert_eq!(f.layer, ProvenanceLayer::Default);
        assert_eq!(f.value.as_u64().unwrap(), 50);
    }

    #[test]
    fn file_layer_when_config_sets_model_name() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "model.name");
        assert_eq!(f.layer, ProvenanceLayer::File);
        assert_eq!(f.value.as_str().unwrap(), "claude-sonnet-4-6");
    }

    #[test]
    fn file_layer_leaves_unchanged_fields_as_default() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        // step_limit was not changed by the file
        let f = find_field(&report, "agent.step_limit");
        assert_eq!(f.layer, ProvenanceLayer::Default);
        assert_eq!(f.value.as_u64().unwrap(), 50);
    }

    #[test]
    fn flag_layer_when_model_flag_passed() {
        let args = ConfigResolveArgs {
            model_flag: Some("claude-haiku-4-5-20251001".into()),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "model.name");
        assert_eq!(f.layer, ProvenanceLayer::Flag);
        assert_eq!(f.value.as_str().unwrap(), "claude-haiku-4-5-20251001");
    }

    #[test]
    fn flag_wins_over_file_for_model_name() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            model_flag: Some("claude-haiku-4-5-20251001".into()),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "model.name");
        assert_eq!(f.layer, ProvenanceLayer::Flag);
        assert_eq!(f.value.as_str().unwrap(), "claude-haiku-4-5-20251001");
    }

    #[test]
    fn step_limit_flag_sets_flag_layer() {
        let args = ConfigResolveArgs {
            step_limit_flag: Some(30),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "agent.step_limit");
        assert_eq!(f.layer, ProvenanceLayer::Flag);
        assert_eq!(f.value.as_u64().unwrap(), 30);
    }

    // ── Hazard detection tests ────────────────────────────────────────────────

    #[test]
    fn no_hazards_with_no_config_file() {
        let report = run_config_resolve(&no_args()).unwrap();
        assert!(!report.has_hazards);
        assert!(report.hazards.is_empty());
    }

    #[test]
    fn detects_model_clap_default_hazard_when_no_flag() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            model_flag: None,
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        assert!(report.has_hazards);
        let model_hazards: Vec<_> = report
            .hazards
            .iter()
            .filter(|h| h.field == "model.name")
            .collect();
        assert_eq!(model_hazards.len(), 1);
        assert!(
            model_hazards[0]
                .commands_affected
                .contains(&"mini".to_string())
        );
        assert!(
            model_hazards[0]
                .commands_affected
                .contains(&"bench swebench".to_string())
        );
    }

    #[test]
    fn hazard_shows_correct_file_value_and_clap_default() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let h = report
            .hazards
            .iter()
            .find(|h| h.field == "model.name")
            .unwrap();
        assert_eq!(h.file_value.as_str().unwrap(), "claude-sonnet-4-6");
        assert_eq!(h.clap_default_value.as_str().unwrap(), "claude-opus-4-7");
    }

    #[test]
    fn no_model_hazard_when_flag_explicitly_passed() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            model_flag: Some("claude-sonnet-4-6".into()),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        assert!(!report.hazards.iter().any(|h| h.field == "model.name"));
    }

    #[test]
    fn no_model_hazard_when_config_matches_clap_default() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        // Setting the same value as the clap default — no hazard expected
        fs::write(&config_path, "[model]\nname = \"claude-opus-4-7\"\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            model_flag: None,
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        assert!(!report.hazards.iter().any(|h| h.field == "model.name"));
    }

    #[test]
    fn detects_step_limit_clap_default_hazard() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[agent]\nstep_limit = 100\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            step_limit_flag: None,
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let step_hazards: Vec<_> = report
            .hazards
            .iter()
            .filter(|h| h.field == "agent.step_limit")
            .collect();
        assert_eq!(step_hazards.len(), 1);
        assert!(
            step_hazards[0]
                .commands_affected
                .contains(&"bench swebench".to_string())
        );
    }

    #[test]
    fn no_step_limit_hazard_when_flag_passed() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[agent]\nstep_limit = 100\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            step_limit_flag: Some(100),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        assert!(!report.hazards.iter().any(|h| h.field == "agent.step_limit"));
    }

    #[test]
    fn no_step_limit_hazard_when_config_matches_default() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[agent]\nstep_limit = 50\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        assert!(!report.hazards.iter().any(|h| h.field == "agent.step_limit"));
    }

    // ── Output format tests ───────────────────────────────────────────────────

    #[test]
    fn format_text_contains_model_name_field() {
        let report = run_config_resolve(&no_args()).unwrap();
        let text = format_text(&report);
        assert!(text.contains("model.name"));
        assert!(text.contains("claude-opus-4-7"));
    }

    #[test]
    fn format_text_shows_none_when_no_hazards() {
        let report = run_config_resolve(&no_args()).unwrap();
        let text = format_text(&report);
        assert!(text.contains("NONE"));
    }

    #[test]
    fn format_text_shows_hazard_when_detected() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let text = format_text(&report);
        assert!(text.contains("CLAP_DEFAULT_OVERRIDE"));
        assert!(text.contains("model.name"));
    }

    #[test]
    fn json_serialization_is_deterministic() {
        let report = run_config_resolve(&no_args()).unwrap();
        let val = serde_json::to_value(&report).unwrap();
        let json1 = serde_json::to_string(&val).unwrap();
        let json2 = serde_json::to_string(&val).unwrap();
        assert_eq!(json1, json2);
    }

    #[test]
    fn all_scalar_fields_present_in_report() {
        let report = run_config_resolve(&no_args()).unwrap();
        let keys: Vec<&str> = report.fields.iter().map(|f| f.key.as_str()).collect();
        assert!(keys.contains(&"model.name"));
        assert!(keys.contains(&"agent.step_limit"));
        assert!(keys.contains(&"environment.kind"));
        assert!(keys.contains(&"environment.workdir"));
    }

    #[test]
    fn observation_max_bytes_flag_sets_flag_layer() {
        let args = ConfigResolveArgs {
            observation_max_bytes_flag: Some(8192),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "agent.observation_max_bytes");
        assert_eq!(f.layer, ProvenanceLayer::Flag);
        assert_eq!(f.value.as_u64().unwrap(), 8192);
    }

    #[test]
    fn flag_value_string_is_redacted_when_it_matches_secret_literal() {
        // Configure a secret literal in the redaction section, then pass it
        // as --model. The flag value must be masked in the output.
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            "[redaction]\nsecret_literals = [\"my-secret-model-name\"]\n",
        )
        .unwrap();

        let args = ConfigResolveArgs {
            config: Some(config_path),
            model_flag: Some("my-secret-model-name".into()),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "model.name");
        assert_eq!(f.layer, ProvenanceLayer::Flag);
        // The raw secret must not appear verbatim in the resolved output.
        assert!(
            !f.value
                .as_str()
                .unwrap_or("")
                .contains("my-secret-model-name")
        );
    }

    #[test]
    fn serde_default_fields_show_typed_values_not_null() {
        // Fields absent from default.toml but with serde defaults must resolve
        // to their actual runtime values, not null.
        let report = run_config_resolve(&no_args()).unwrap();
        let detect = find_field(&report, "agent.detect_stagnation");
        assert_eq!(detect.layer, ProvenanceLayer::Default);
        assert_eq!(detect.value, Value::Bool(true));

        let threshold = find_field(&report, "agent.stagnation_repeat_threshold");
        assert_eq!(threshold.layer, ProvenanceLayer::Default);
        assert_eq!(threshold.value.as_u64().unwrap(), 4);

        let window = find_field(&report, "agent.stagnation_window");
        assert_eq!(window.layer, ProvenanceLayer::Default);
        assert_eq!(window.value.as_u64().unwrap(), 8);
    }

    #[test]
    fn stability_best_of_suite_listed_in_model_hazard() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[model]\nname = \"claude-sonnet-4-6\"\n").unwrap();
        let args = ConfigResolveArgs {
            config: Some(config_path),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let h = report
            .hazards
            .iter()
            .find(|h| h.field == "model.name")
            .unwrap();
        assert!(h.commands_affected.contains(&"agent stability".to_string()));
        assert!(h.commands_affected.contains(&"agent best-of".to_string()));
        assert!(h.commands_affected.contains(&"agent suite".to_string()));
    }

    #[test]
    fn hazard_model_file_value_is_redacted() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            "[model]\nname = \"secret-model\"\n[redaction]\nsecret_literals = [\"secret-model\"]\n",
        )
        .unwrap();
        let args = ConfigResolveArgs {
            config: Some(config_path),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let h = report
            .hazards
            .iter()
            .find(|h| h.field == "model.name")
            .unwrap();
        assert!(!h.file_value.as_str().unwrap_or("").contains("secret-model"));
        assert!(!h.message.contains("secret-model"));
    }

    #[test]
    fn hide_budget_flag_sets_flag_layer() {
        let args = ConfigResolveArgs {
            hide_budget_from_agent_flag: true,
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "agent.hide_budget_from_agent");
        assert_eq!(f.layer, ProvenanceLayer::Flag);
        assert_eq!(f.value, Value::Bool(true));
    }

    #[test]
    fn env_flag_sets_flag_layer_for_environment_kind() {
        let args = ConfigResolveArgs {
            env_flag: Some("docker".to_string()),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "environment.kind");
        assert_eq!(f.layer, ProvenanceLayer::Flag);
        assert_eq!(f.value.as_str().unwrap(), "docker");
    }

    #[test]
    fn workdir_flag_sets_flag_layer_for_environment_workdir() {
        let args = ConfigResolveArgs {
            workdir_flag: Some(PathBuf::from("/tmp/myrepo")),
            ..no_args()
        };
        let report = run_config_resolve(&args).unwrap();
        let f = find_field(&report, "environment.workdir");
        assert_eq!(f.layer, ProvenanceLayer::Flag);
        assert_eq!(f.value.as_str().unwrap(), "/tmp/myrepo");
    }

    #[test]
    fn stagnation_window_less_than_threshold_is_error() {
        let args = ConfigResolveArgs {
            stagnation_repeat_threshold_flag: Some(5),
            stagnation_window_flag: Some(3),
            ..no_args()
        };
        let err = run_config_resolve(&args).unwrap_err();
        assert!(err.to_string().contains("stagnation-window"));
    }

    #[test]
    fn stagnation_zero_threshold_is_error() {
        let args = ConfigResolveArgs {
            stagnation_repeat_threshold_flag: Some(0),
            ..no_args()
        };
        let err = run_config_resolve(&args).unwrap_err();
        assert!(err.to_string().contains("stagnation-repeat-threshold"));
    }

    #[test]
    fn stagnation_zero_window_is_error() {
        let args = ConfigResolveArgs {
            stagnation_window_flag: Some(0),
            ..no_args()
        };
        let err = run_config_resolve(&args).unwrap_err();
        assert!(err.to_string().contains("stagnation-window"));
    }

    #[test]
    fn stagnation_disabled_skips_bounds_check() {
        // With detect_stagnation=false, window < threshold is allowed.
        let args = ConfigResolveArgs {
            detect_stagnation_flag: Some(false),
            stagnation_repeat_threshold_flag: Some(5),
            stagnation_window_flag: Some(1),
            ..no_args()
        };
        assert!(run_config_resolve(&args).is_ok());
    }
}
