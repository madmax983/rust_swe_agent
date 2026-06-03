//! Analysis logic for verifying SWE-bench dataset authenticity offline (`bench dataset-verify`).

use crate::error::Error;
use crate::run::swebench::SweBenchInstance;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetVerifyReport {
    pub schema_version: u32,
    pub verdict: String,
    pub missing: Vec<String>,
    pub extra: Vec<String>,
    pub mutated: Vec<String>,
}

impl DatasetVerifyReport {
    pub const SCHEMA_VERSION: u32 = 1;
}

pub fn normalize_json_list(val: &serde_json::Value) -> Vec<String> {
    if val.is_null() {
        return vec![];
    }
    if let Some(arr) = val.as_array() {
        arr.iter()
            .filter(|v| !v.is_null())
            .map(|v| {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.to_string()
                }
            })
            .collect()
    } else if let Some(s) = val.as_str() {
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(s) {
            arr.iter()
                .filter(|v| !v.is_null())
                .map(|v| {
                    if let Some(inner_s) = v.as_str() {
                        inner_s.to_string()
                    } else {
                        v.to_string()
                    }
                })
                .collect()
        } else {
            vec![s.to_string()]
        }
    } else {
        vec![val.to_string()]
    }
}

pub fn compute_instance_hash(inst: &SweBenchInstance) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();

    let problem_statement = inst.problem_statement.as_deref().unwrap_or("");
    hasher.update(b"problem_statement:");
    hasher.update(problem_statement.as_bytes());
    hasher.update(b"\n");

    let get_other_str = |key: &str| -> String {
        inst.other
            .get(key)
            .and_then(|v| {
                if v.is_null() {
                    None
                } else if let Some(s) = v.as_str() {
                    Some(s.to_string())
                } else {
                    Some(v.to_string())
                }
            })
            .unwrap_or_default()
    };

    let patch = get_other_str("patch");
    hasher.update(b"patch:");
    hasher.update(patch.as_bytes());
    hasher.update(b"\n");

    let test_patch = get_other_str("test_patch");
    hasher.update(b"test_patch:");
    hasher.update(test_patch.as_bytes());
    hasher.update(b"\n");

    let fail_to_pass = inst
        .other
        .get("FAIL_TO_PASS")
        .map(normalize_json_list)
        .unwrap_or_default();
    hasher.update(b"FAIL_TO_PASS:");
    for item in &fail_to_pass {
        hasher.update(item.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\n");

    let pass_to_pass = inst
        .other
        .get("PASS_TO_PASS")
        .map(normalize_json_list)
        .unwrap_or_default();
    hasher.update(b"PASS_TO_PASS:");
    for item in &pass_to_pass {
        hasher.update(item.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\n");

    let env_commit = get_other_str("environment_setup_commit");
    hasher.update(b"environment_setup_commit:");
    hasher.update(env_commit.as_bytes());
    hasher.update(b"\n");

    format!("{:x}", hasher.finalize())
}

pub fn verify_dataset(
    candidate_instances: &[SweBenchInstance],
    reference_instances: &[SweBenchInstance],
) -> Result<DatasetVerifyReport, Error> {
    let mut cand_hashes = HashMap::new();
    for inst in candidate_instances {
        let hash = compute_instance_hash(inst);
        cand_hashes.insert(inst.instance_id.clone(), hash);
    }

    let mut ref_hashes = HashMap::new();
    for inst in reference_instances {
        let hash = compute_instance_hash(inst);
        ref_hashes.insert(inst.instance_id.clone(), hash);
    }

    let mut missing = Vec::new();
    let mut mutated = Vec::new();
    for (id, ref_hash) in &ref_hashes {
        match cand_hashes.get(id) {
            None => {
                missing.push(id.clone());
            }
            Some(cand_hash) => {
                if cand_hash != ref_hash {
                    mutated.push(id.clone());
                }
            }
        }
    }

    let mut extra = Vec::new();
    for id in cand_hashes.keys() {
        if !ref_hashes.contains_key(id) {
            extra.push(id.clone());
        }
    }

    // Sort to ensure deterministic output
    missing.sort();
    extra.sort();
    mutated.sort();

    let verdict = if missing.is_empty() && extra.is_empty() && mutated.is_empty() {
        "clean".to_string()
    } else {
        "mismatch".to_string()
    };

    Ok(DatasetVerifyReport {
        schema_version: DatasetVerifyReport::SCHEMA_VERSION,
        verdict,
        missing,
        extra,
        mutated,
    })
}

pub fn render_text(report: &DatasetVerifyReport) -> String {
    let mut out = String::new();
    if !report.missing.is_empty() {
        let _ = writeln!(out, "Missing instances ({}):", report.missing.len());
        let limit = 10;
        for id in report.missing.iter().take(limit) {
            let _ = writeln!(out, "  - {id}");
        }
        if report.missing.len() > limit {
            let _ = writeln!(out, "  ... and {} more", report.missing.len() - limit);
        }
    }
    if !report.extra.is_empty() {
        let _ = writeln!(out, "Extra instances ({}):", report.extra.len());
        let limit = 10;
        for id in report.extra.iter().take(limit) {
            let _ = writeln!(out, "  - {id}");
        }
        if report.extra.len() > limit {
            let _ = writeln!(out, "  ... and {} more", report.extra.len() - limit);
        }
    }
    if !report.mutated.is_empty() {
        let _ = writeln!(out, "Mutated instances ({}):", report.mutated.len());
        let limit = 10;
        for id in report.mutated.iter().take(limit) {
            let _ = writeln!(out, "  - {id}");
        }
        if report.mutated.len() > limit {
            let _ = writeln!(out, "  ... and {} more", report.mutated.len() - limit);
        }
    }
    let _ = write!(out, "{}", report.verdict);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_verify_clean_success() {
        let inst = SweBenchInstance {
            instance_id: "test-1".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix it".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };

        let candidate = vec![inst.clone()];
        let reference = vec![inst];

        let result = verify_dataset(&candidate, &reference).unwrap();
        assert_eq!(result.verdict, "clean");
        assert!(result.missing.is_empty());
        assert!(result.extra.is_empty());
        assert!(result.mutated.is_empty());
    }

    #[test]
    fn test_verify_mismatch_missing() {
        let inst = SweBenchInstance {
            instance_id: "test-1".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix it".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };

        let candidate: Vec<SweBenchInstance> = vec![];
        let reference = vec![inst];

        let result = verify_dataset(&candidate, &reference).unwrap();
        assert_eq!(result.verdict, "mismatch");
        assert_eq!(result.missing, vec!["test-1".to_string()]);
        assert!(result.extra.is_empty());
        assert!(result.mutated.is_empty());
    }

    #[test]
    fn test_verify_mismatch_extra() {
        let inst = SweBenchInstance {
            instance_id: "test-1".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix it".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };

        let candidate = vec![inst];
        let reference: Vec<SweBenchInstance> = vec![];

        let result = verify_dataset(&candidate, &reference).unwrap();
        assert_eq!(result.verdict, "mismatch");
        assert!(result.missing.is_empty());
        assert_eq!(result.extra, vec!["test-1".to_string()]);
        assert!(result.mutated.is_empty());
    }

    #[test]
    fn test_verify_mismatch_mutated() {
        let inst_ref = SweBenchInstance {
            instance_id: "test-1".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix it".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };

        let mut inst_cand = inst_ref.clone();
        inst_cand.problem_statement = Some("fix it now".to_string());

        let candidate = vec![inst_cand];
        let reference = vec![inst_ref];

        let result = verify_dataset(&candidate, &reference).unwrap();
        assert_eq!(result.verdict, "mismatch");
        assert!(result.missing.is_empty());
        assert!(result.extra.is_empty());
        assert_eq!(result.mutated, vec!["test-1".to_string()]);
    }

    #[test]
    fn test_verify_hash_variance_fields() {
        let mut inst_ref = SweBenchInstance {
            instance_id: "test-1".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix it".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };
        inst_ref.other.insert(
            "patch".to_string(),
            serde_json::Value::String("diff a/file.py".to_string()),
        );
        inst_ref.other.insert(
            "test_patch".to_string(),
            serde_json::Value::String("diff b/test.py".to_string()),
        );
        inst_ref.other.insert(
            "FAIL_TO_PASS".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("test_a".to_string())]),
        );
        inst_ref.other.insert(
            "PASS_TO_PASS".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("test_b".to_string())]),
        );
        inst_ref.other.insert(
            "environment_setup_commit".to_string(),
            serde_json::Value::String("abcdef".to_string()),
        );

        let fields_to_mutate = vec![
            "problem_statement",
            "patch",
            "test_patch",
            "FAIL_TO_PASS",
            "PASS_TO_PASS",
            "environment_setup_commit",
        ];

        for field in fields_to_mutate {
            let mut inst_cand = inst_ref.clone();
            if field == "problem_statement" {
                inst_cand.problem_statement = Some("fix it modified".to_string());
            } else if field == "FAIL_TO_PASS" || field == "PASS_TO_PASS" {
                inst_cand.other.insert(
                    field.to_string(),
                    serde_json::Value::Array(vec![serde_json::Value::String(
                        "test_modified".to_string(),
                    )]),
                );
            } else {
                inst_cand.other.insert(
                    field.to_string(),
                    serde_json::Value::String("modified".to_string()),
                );
            }

            let candidate = vec![inst_cand];
            let reference = vec![inst_ref.clone()];

            let result = verify_dataset(&candidate, &reference).unwrap();
            assert_eq!(
                result.verdict, "mismatch",
                "Failed to detect mutation on field: {field}"
            );
            assert_eq!(
                result.mutated,
                vec!["test-1".to_string()],
                "Failed to detect mutation on field: {field}"
            );
        }
    }

    #[test]
    fn test_verify_determinism_and_sorting() {
        let inst1 = SweBenchInstance {
            instance_id: "test-b".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix b".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };

        let inst2 = SweBenchInstance {
            instance_id: "test-a".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix a".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };

        let candidate: Vec<SweBenchInstance> = vec![];
        let reference = vec![inst1, inst2];

        let result = verify_dataset(&candidate, &reference).unwrap();
        assert_eq!(result.verdict, "mismatch");
        assert_eq!(
            result.missing,
            vec!["test-a".to_string(), "test-b".to_string()]
        );
    }

    #[test]
    fn test_verify_null_vs_absent_equivalence() {
        let inst_absent = SweBenchInstance {
            instance_id: "test-1".to_string(),
            repo: Some("repo".to_string()),
            base_commit: None,
            problem_statement: Some("fix it".to_string()),
            image: None,
            other: serde_json::Map::new(),
        };

        // Case 1: patch, test_patch, and environment_setup_commit are omitted vs explicitly null
        let mut inst_nulls = inst_absent.clone();
        inst_nulls
            .other
            .insert("patch".to_string(), serde_json::Value::Null);
        inst_nulls
            .other
            .insert("test_patch".to_string(), serde_json::Value::Null);
        inst_nulls.other.insert(
            "environment_setup_commit".to_string(),
            serde_json::Value::Null,
        );

        // Hash of inst_absent should be identical to inst_nulls
        assert_eq!(
            compute_instance_hash(&inst_absent),
            compute_instance_hash(&inst_nulls)
        );

        // Case 2: FAIL_TO_PASS and PASS_TO_PASS are omitted vs explicitly null vs array containing nulls
        let mut inst_lists_null = inst_absent.clone();
        inst_lists_null
            .other
            .insert("FAIL_TO_PASS".to_string(), serde_json::Value::Null);
        inst_lists_null
            .other
            .insert("PASS_TO_PASS".to_string(), serde_json::Value::Null);

        assert_eq!(
            compute_instance_hash(&inst_absent),
            compute_instance_hash(&inst_lists_null)
        );

        let mut inst_lists_with_nulls = inst_absent.clone();
        inst_lists_with_nulls.other.insert(
            "FAIL_TO_PASS".to_string(),
            serde_json::Value::Array(vec![
                serde_json::Value::String("test_a".to_string()),
                serde_json::Value::Null,
            ]),
        );
        inst_lists_with_nulls.other.insert(
            "PASS_TO_PASS".to_string(),
            serde_json::Value::Array(vec![
                serde_json::Value::Null,
                serde_json::Value::String("test_b".to_string()),
            ]),
        );

        let mut inst_lists_clean = inst_absent;
        inst_lists_clean.other.insert(
            "FAIL_TO_PASS".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("test_a".to_string())]),
        );
        inst_lists_clean.other.insert(
            "PASS_TO_PASS".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("test_b".to_string())]),
        );

        assert_eq!(
            compute_instance_hash(&inst_lists_clean),
            compute_instance_hash(&inst_lists_with_nulls)
        );
    }
}
