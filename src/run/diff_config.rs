use crate::error::{ConfigError, Error};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DiffConfigArgs {
    pub baseline: PathBuf,
    pub candidate: PathBuf,
    pub format: String,
    pub fail_on_change: bool,
    pub ignore: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiffConfigReport {
    pub schema_version: String,
    pub generated_at: String,
    pub summary: DiffSummary,
    pub compared_fields: Vec<ComparedField>,
    pub changed_fields: Vec<ChangedField>,
    pub unchanged_groups: Vec<String>,
    pub ignored_fields: Vec<IgnoredField>,
}

#[derive(Debug, Serialize)]
pub struct DiffSummary {
    pub identical: bool,
    pub changed_field_count: usize,
    pub ignored_field_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComparedField {
    pub group: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedField {
    pub group: String,
    pub path: String,
    pub baseline: Value,
    pub candidate: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct IgnoredField {
    pub group: String,
    pub path: String,
    pub baseline: Value,
    pub candidate: Value,
}

#[allow(clippy::too_many_lines)]
pub fn run(args: &DiffConfigArgs) -> Result<(), Error> {
    if args.format != "text" && args.format != "json" {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "unsupported output format `{}`; must be `text` or `json`",
            args.format
        ))));
    }

    let baseline_manifest = load_manifest(&args.baseline)?;
    let candidate_manifest = load_manifest(&args.candidate)?;

    let mut ignore_patterns = Vec::new();
    if let Some(ref ignore_str) = args.ignore {
        for s in ignore_str.split(',') {
            let s_trimmed = s.trim();
            if !s_trimmed.is_empty() {
                ignore_patterns.push(s_trimmed.to_string());
            }
        }
    }

    let mut compared_fields = Vec::new();
    let mut changed_fields = Vec::new();
    let mut ignored_fields = Vec::new();

    // 1. harness
    compare_field(
        "harness",
        ".name",
        &get_field_val(&baseline_manifest, &["harness", "name"]),
        &get_field_val(&candidate_manifest, &["harness", "name"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "harness",
        ".version",
        &get_field_val(&baseline_manifest, &["harness", "version"]),
        &get_field_val(&candidate_manifest, &["harness", "version"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "harness",
        ".git_sha",
        &get_field_val(&baseline_manifest, &["harness", "git_sha"]),
        &get_field_val(&candidate_manifest, &["harness", "git_sha"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "harness",
        ".git_dirty",
        &get_field_val(&baseline_manifest, &["harness", "git_dirty"]),
        &get_field_val(&candidate_manifest, &["harness", "git_dirty"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    // 2. dataset
    compare_field(
        "dataset",
        ".path",
        &get_field_val(&baseline_manifest, &["dataset", "path"]),
        &get_field_val(&candidate_manifest, &["dataset", "path"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "dataset",
        ".sha256",
        &get_field_val(&baseline_manifest, &["dataset", "sha256"]),
        &get_field_val(&candidate_manifest, &["dataset", "sha256"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "dataset",
        ".instance_count",
        &get_field_val(&baseline_manifest, &["dataset", "instance_count"]),
        &get_field_val(&candidate_manifest, &["dataset", "instance_count"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    let base_fs = get_field_val(&baseline_manifest, &["dataset", "filter_spec"]);
    let cand_fs = get_field_val(&candidate_manifest, &["dataset", "filter_spec"]);
    let mut base_fs_leaves = BTreeMap::new();
    let mut cand_fs_leaves = BTreeMap::new();
    collect_leaves(".filter_spec", &base_fs, &mut base_fs_leaves);
    collect_leaves(".filter_spec", &cand_fs, &mut cand_fs_leaves);
    let all_fs_keys: BTreeSet<String> = base_fs_leaves
        .keys()
        .cloned()
        .chain(cand_fs_leaves.keys().cloned())
        .collect();
    for key in all_fs_keys {
        let b_val = base_fs_leaves.get(&key).cloned().unwrap_or(Value::Null);
        let c_val = cand_fs_leaves.get(&key).cloned().unwrap_or(Value::Null);
        compare_field(
            "dataset",
            &key,
            &b_val,
            &c_val,
            &mut compared_fields,
            &mut changed_fields,
            &mut ignored_fields,
            &ignore_patterns,
        );
    }

    // 3. prompt_template
    compare_field(
        "prompt_template",
        ".source",
        &get_field_val(&baseline_manifest, &["prompt_template", "source"]),
        &get_field_val(&candidate_manifest, &["prompt_template", "source"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "prompt_template",
        ".path",
        &get_field_val(&baseline_manifest, &["prompt_template", "path"]),
        &get_field_val(&candidate_manifest, &["prompt_template", "path"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "prompt_template",
        ".sha256",
        &get_field_val(&baseline_manifest, &["prompt_template", "sha256"]),
        &get_field_val(&candidate_manifest, &["prompt_template", "sha256"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    // 4. model
    compare_field(
        "model",
        ".name",
        &get_field_val(&baseline_manifest, &["model", "name"]),
        &get_field_val(&candidate_manifest, &["model", "name"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "model",
        ".backend",
        &get_field_val(&baseline_manifest, &["model", "backend"]),
        &get_field_val(&candidate_manifest, &["model", "backend"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "model",
        ".backend_version",
        &get_field_val(&baseline_manifest, &["model", "backend_version"]),
        &get_field_val(&candidate_manifest, &["model", "backend_version"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "model",
        ".base_url",
        &get_field_val(&baseline_manifest, &["model", "base_url"]),
        &get_field_val(&candidate_manifest, &["model", "base_url"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    // 5. sampling
    let base_sampling = get_field_val(&baseline_manifest, &["sampling"]);
    let cand_sampling = get_field_val(&candidate_manifest, &["sampling"]);
    compare_block(
        "sampling",
        &base_sampling,
        &cand_sampling,
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    // 6. tools
    let base_tools = get_field_val(&baseline_manifest, &["tools"]);
    let cand_tools = get_field_val(&candidate_manifest, &["tools"]);
    compare_block(
        "tools",
        &base_tools,
        &cand_tools,
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    // 7. hooks
    let base_hooks = get_field_val(&baseline_manifest, &["hooks"]);
    let cand_hooks = get_field_val(&candidate_manifest, &["hooks"]);
    compare_block(
        "hooks",
        &base_hooks,
        &cand_hooks,
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    // 8. limits
    let base_limits = get_field_val(&baseline_manifest, &["limits"]);
    let cand_limits = get_field_val(&candidate_manifest, &["limits"]);
    compare_field(
        "limits",
        ".step_limit",
        &get_field_val(&base_limits, &["step_limit"]),
        &get_field_val(&cand_limits, &["step_limit"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "limits",
        ".per_task_budget_usd",
        &get_field_val(&base_limits, &["per_task_budget_usd"]),
        &get_field_val(&cand_limits, &["per_task_budget_usd"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "limits",
        ".task_timeout_secs",
        &get_field_val(&base_limits, &["task_timeout_secs"]),
        &get_field_val(&cand_limits, &["task_timeout_secs"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );
    compare_field(
        "limits",
        ".sweep_cost_limit_usd",
        &get_field_val(&base_limits, &["sweep_cost_limit_usd"]),
        &get_field_val(&cand_limits, &["sweep_cost_limit_usd"]),
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    // 9. config.resolved
    let base_resolved_val = get_field_val(&baseline_manifest, &["config", "resolved"]);
    let cand_resolved_val = get_field_val(&candidate_manifest, &["config", "resolved"]);

    let base_resolved: Value = if base_resolved_val.is_null() {
        Value::Null
    } else {
        let s = base_resolved_val.as_str().ok_or_else(|| {
            Error::Config(ConfigError::Invalid(
                "baseline config.resolved must be a string".to_string(),
            ))
        })?;
        toml::from_str(s).map_err(|e| {
            Error::Config(ConfigError::Invalid(format!(
                "failed to parse baseline config.resolved: {e}"
            )))
        })?
    };

    let cand_resolved: Value = if cand_resolved_val.is_null() {
        Value::Null
    } else {
        let s = cand_resolved_val.as_str().ok_or_else(|| {
            Error::Config(ConfigError::Invalid(
                "candidate config.resolved must be a string".to_string(),
            ))
        })?;
        toml::from_str(s).map_err(|e| {
            Error::Config(ConfigError::Invalid(format!(
                "failed to parse candidate config.resolved: {e}"
            )))
        })?
    };
    compare_block(
        "config.resolved",
        &base_resolved,
        &cand_resolved,
        &mut compared_fields,
        &mut changed_fields,
        &mut ignored_fields,
        &ignore_patterns,
    );

    // 10. cli.argv
    let base_argv = get_field_val(&baseline_manifest, &["cli", "argv"]);
    let cand_argv = get_field_val(&candidate_manifest, &["cli", "argv"]);
    if let (Some(b_arr), Some(c_arr)) = (base_argv.as_array(), cand_argv.as_array()) {
        let max_len = std::cmp::max(b_arr.len(), c_arr.len());
        for i in 0..max_len {
            let b_val = b_arr.get(i).cloned().unwrap_or(Value::Null);
            let c_val = c_arr.get(i).cloned().unwrap_or(Value::Null);
            let path = format!("[{i}]");
            compare_field(
                "cli.argv",
                &path,
                &b_val,
                &c_val,
                &mut compared_fields,
                &mut changed_fields,
                &mut ignored_fields,
                &ignore_patterns,
            );
        }
    } else {
        compare_block(
            "cli.argv",
            &base_argv,
            &cand_argv,
            &mut compared_fields,
            &mut changed_fields,
            &mut ignored_fields,
            &ignore_patterns,
        );
    }

    let all_groups = [
        "harness",
        "dataset",
        "prompt_template",
        "model",
        "sampling",
        "tools",
        "hooks",
        "limits",
        "config.resolved",
        "cli.argv",
    ];

    let mut unchanged_groups = Vec::new();
    for group in all_groups {
        if !changed_fields.iter().any(|f| f.group == group) {
            unchanged_groups.push(group.to_string());
        }
    }

    let has_changes = !changed_fields.is_empty();

    if args.format == "json" {
        let report = DiffConfigReport {
            schema_version: "1.0.0".to_string(),
            generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            summary: DiffSummary {
                identical: changed_fields.is_empty(),
                changed_field_count: changed_fields.len(),
                ignored_field_count: ignored_fields.len(),
            },
            compared_fields,
            changed_fields,
            unchanged_groups,
            ignored_fields,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(Error::Json)?
        );
    } else {
        if changed_fields.is_empty() {
            println!(
                "manifests identical ({} fields compared)",
                compared_fields.len()
            );
        } else {
            let mut headline = "other configuration drift detected";
            if changed_fields
                .iter()
                .any(|f| f.group == "prompt_template" && f.path == ".sha256")
            {
                headline = "prompt_template.sha256 changed";
            } else if changed_fields
                .iter()
                .any(|f| f.group == "model" && f.path == ".name")
            {
                headline = "model.name changed";
            } else if changed_fields
                .iter()
                .any(|f| f.group == "harness" && f.path == ".git_sha")
            {
                headline = "harness.git_sha changed";
            } else if changed_fields.iter().any(|f| f.group == "limits") {
                headline = "limits changed";
            } else if changed_fields.iter().any(|f| f.group == "tools") {
                headline = "tools changed";
            } else if changed_fields.iter().any(|f| f.group == "hooks") {
                headline = "hooks changed";
            }

            println!("HEADLINE: {headline}");
            println!();

            for group in all_groups {
                let group_changes: Vec<&ChangedField> =
                    changed_fields.iter().filter(|f| f.group == group).collect();
                if group_changes.is_empty() {
                    println!("[{group}] identical");
                } else {
                    println!("[{group}]");
                    for change in group_changes {
                        let b_str = serde_json::to_string(&change.baseline)
                            .unwrap_or_else(|_| String::new());
                        let c_str = serde_json::to_string(&change.candidate)
                            .unwrap_or_else(|_| String::new());
                        println!("  {}: {} → {}", change.path, b_str, c_str);
                    }
                }
            }
        }

        if !ignored_fields.is_empty() {
            println!();
            println!("ignored:");
            for f in &ignored_fields {
                println!("  - {}{}", f.group, f.path);
            }
        }
    }

    if args.fail_on_change && has_changes {
        return Err(Error::Preflight(
            "config drift detected with --fail-on-change".to_string(),
        ));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compare_field(
    group: &str,
    path: &str,
    b_val: &Value,
    c_val: &Value,
    compared_fields: &mut Vec<ComparedField>,
    changed_fields: &mut Vec<ChangedField>,
    ignored_fields: &mut Vec<IgnoredField>,
    ignore_patterns: &[String],
) {
    compared_fields.push(ComparedField {
        group: group.to_string(),
        path: path.to_string(),
    });
    if b_val != c_val {
        if is_ignored(group, path, ignore_patterns) {
            ignored_fields.push(IgnoredField {
                group: group.to_string(),
                path: path.to_string(),
                baseline: b_val.clone(),
                candidate: c_val.clone(),
            });
        } else {
            changed_fields.push(ChangedField {
                group: group.to_string(),
                path: path.to_string(),
                baseline: b_val.clone(),
                candidate: c_val.clone(),
            });
        }
    }
}

fn compare_block(
    group: &str,
    base_val: &Value,
    cand_val: &Value,
    compared_fields: &mut Vec<ComparedField>,
    changed_fields: &mut Vec<ChangedField>,
    ignored_fields: &mut Vec<IgnoredField>,
    ignore_patterns: &[String],
) {
    let mut base_leaves = BTreeMap::new();
    let mut cand_leaves = BTreeMap::new();
    collect_leaves("", base_val, &mut base_leaves);
    collect_leaves("", cand_val, &mut cand_leaves);

    let all_keys: BTreeSet<String> = base_leaves
        .keys()
        .cloned()
        .chain(cand_leaves.keys().cloned())
        .collect();
    for key in all_keys {
        let b_val = base_leaves.get(&key).cloned().unwrap_or(Value::Null);
        let c_val = cand_leaves.get(&key).cloned().unwrap_or(Value::Null);
        let disp_path = if key.is_empty() {
            String::new()
        } else if key.starts_with('.') || key.starts_with('[') {
            key.clone()
        } else {
            format!(".{key}")
        };
        compare_field(
            group,
            &disp_path,
            &b_val,
            &c_val,
            compared_fields,
            changed_fields,
            ignored_fields,
            ignore_patterns,
        );
    }
}

fn is_ignored(group: &str, path: &str, patterns: &[String]) -> bool {
    let full_path = format!("{group}{path}");
    for pattern in patterns {
        if pattern == group {
            return true;
        }
        if pattern == &full_path {
            return true;
        }
        if full_path.starts_with(pattern) {
            let suffix = &full_path[pattern.len()..];
            if suffix.starts_with('.') || suffix.starts_with('[') {
                return true;
            }
        }
    }
    false
}

fn load_manifest(dir: &Path) -> Result<Value, Error> {
    let results_path = dir.join("results.json");
    if !results_path.exists() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "sweep results file not found at `{}`; does the directory exist?",
            results_path.display()
        ))));
    }

    let file = std::fs::File::open(&results_path).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "failed to open results.json at `{}`: {e}",
            results_path.display()
        )))
    })?;
    let reader = std::io::BufReader::new(file);
    let root: Value = serde_json::from_reader(reader).map_err(|e| {
        Error::Config(ConfigError::Invalid(format!(
            "failed to parse results.json at `{}`: {e}",
            results_path.display()
        )))
    })?;

    let manifest = root.get("manifest").ok_or_else(|| {
        Error::Config(ConfigError::Invalid(format!(
            "results.json at `{}` is missing 'manifest' block; the sweep may pre-date configuration provenance",
            dir.display()
        )))
    })?;

    if manifest.is_null() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "results.json at `{}` is missing 'manifest' block; the sweep may pre-date configuration provenance",
            dir.display()
        ))));
    }

    if !manifest.is_object() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "results.json at `{}` has an invalid 'manifest' type: expected a JSON object, found {}",
            dir.display(),
            manifest_type_name(manifest)
        ))));
    }

    Ok(manifest.clone())
}

fn manifest_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn get_field_val(obj: &Value, keys: &[&str]) -> Value {
    let mut current = obj;
    for &key in keys {
        if let Some(next) = current.get(key) {
            current = next;
        } else {
            return Value::Null;
        }
    }
    current.clone()
}

fn escape_key(k: &str) -> String {
    let mut s = String::with_capacity(k.len());
    for c in k.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '.' => s.push_str("\\."),
            '[' => s.push_str("\\["),
            ']' => s.push_str("\\]"),
            other => s.push(other),
        }
    }
    s
}

fn collect_leaves(prefix: &str, val: &Value, map: &mut BTreeMap<String, Value>) {
    match val {
        Value::Object(obj) => {
            if obj.is_empty() {
                map.insert(prefix.to_string(), val.clone());
            } else {
                for (k, v) in obj {
                    let escaped_k = escape_key(k);
                    let next_prefix = if prefix.is_empty() {
                        format!(".{escaped_k}")
                    } else {
                        format!("{prefix}.{escaped_k}")
                    };
                    collect_leaves(&next_prefix, v, map);
                }
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                map.insert(prefix.to_string(), val.clone());
            } else {
                for (i, v) in arr.iter().enumerate() {
                    let next_prefix = format!("{prefix}[{i}]");
                    collect_leaves(&next_prefix, v, map);
                }
            }
        }
        other => {
            map.insert(prefix.to_string(), other.clone());
        }
    }
}

