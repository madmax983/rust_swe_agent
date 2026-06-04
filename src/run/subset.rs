//! Core logic for `bench subset` — materialise a sampled dataset slice as a
//! pinned JSONL artifact plus a self-describing sidecar manifest.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::run::swebench::{FilterSpec, SweBenchInstance};

/// Schema-version tag written into every `.manifest.json` sidecar.
pub const MANIFEST_SCHEMA_VERSION: &str = "subset-manifest-v1";

/// Arguments passed to [`run_subset`].
pub struct SubsetArgs<'a> {
    /// The already-resolved, already-filtered instances to materialise.
    pub instances: Vec<SweBenchInstance>,
    /// SHA-256 hex digest of the *source* dataset bytes (pre-filter).
    pub source_sha256: String,
    /// Named alias (`full`, `lite`, `verified`) if `--dataset` was used.
    pub alias: Option<String>,
    /// Split selector (`train`, `test`, `dev`) if `--dataset` was used.
    pub split: Option<String>,
    /// Filter parameters that produced `instances`.
    pub filter_spec: FilterSpec,
    /// Destination JSONL path.  The sidecar manifest is written next to it.
    pub output: &'a Path,
}

/// Sidecar manifest emitted alongside the JSONL slice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubsetManifest {
    pub schema_version: String,
    pub source_dataset_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
    pub selection: FilterSpec,
    pub instance_count: usize,
    pub resolved_instance_ids: Vec<String>,
    /// Per-repo instance counts; present only when `--stratify-by` was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_stratum_counts: Option<BTreeMap<String, usize>>,
}

/// Write the resolved instances to `args.output` as JSONL and emit the
/// sidecar manifest.  Returns the in-memory manifest.
pub fn run_subset(args: SubsetArgs<'_>) -> Result<SubsetManifest, Error> {
    let per_stratum_counts = if args.filter_spec.stratify_by.is_some() {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for inst in &args.instances {
            let key = inst.repo.clone().unwrap_or_else(|| "<unknown>".to_owned());
            *counts.entry(key).or_default() += 1;
        }
        Some(counts)
    } else {
        None
    };

    let resolved_ids: Vec<String> = args
        .instances
        .iter()
        .map(|i| i.instance_id.clone())
        .collect();

    let mut jsonl = String::new();
    for inst in &args.instances {
        let line = serde_json::to_string(inst)?;
        jsonl.push_str(&line);
        jsonl.push('\n');
    }

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(args.output, &jsonl)?;

    let manifest = SubsetManifest {
        schema_version: MANIFEST_SCHEMA_VERSION.to_owned(),
        source_dataset_sha256: args.source_sha256,
        alias: args.alias,
        split: args.split,
        selection: args.filter_spec,
        instance_count: args.instances.len(),
        resolved_instance_ids: resolved_ids,
        per_stratum_counts,
    };

    let manifest_path = manifest_path_for(args.output);
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(&manifest_path, manifest_json)?;

    Ok(manifest)
}

/// Derive the sidecar manifest path from the JSONL output path.
///
/// `out/slice.jsonl` → `out/slice.manifest.json`
/// `slice`           → `slice.manifest.json`
pub fn manifest_path_for(output: &Path) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.manifest.json"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::run::swebench::{StratifyBy, StratifyMode};

    fn make_instance(id: &str, repo: &str) -> SweBenchInstance {
        SweBenchInstance {
            instance_id: id.to_owned(),
            repo: Some(repo.to_owned()),
            base_commit: None,
            problem_statement: Some(format!("Fix {id}")),
            image: None,
            other: serde_json::Map::new(),
        }
    }

    // ── RED: these tests fail until run_subset is fully implemented ──────────

    #[test]
    fn writes_jsonl_and_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("slice.jsonl");

        let instances = vec![
            make_instance("repo__1", "owner/repo"),
            make_instance("repo__2", "owner/repo"),
        ];
        let filter_spec = FilterSpec {
            original_count: 10,
            selected_count: 2,
            instance_ids: None,
            limit: None,
            sample: Some(2),
            seed: Some(42),
            stratify_by: None,
            stratify_mode: None,
        };

        let manifest = run_subset(SubsetArgs {
            instances,
            source_sha256: "abcdef1234".to_owned(),
            alias: None,
            split: None,
            filter_spec,
            output: &output,
        })
        .unwrap();

        // JSONL file exists with two lines
        assert!(output.exists());
        let content = std::fs::read_to_string(&output).unwrap();
        assert_eq!(content.lines().count(), 2);

        // Sidecar manifest exists
        assert!(manifest_path_for(&output).exists());

        // Manifest fields
        assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.instance_count, 2);
        assert_eq!(manifest.resolved_instance_ids, vec!["repo__1", "repo__2"]);
        assert!(manifest.per_stratum_counts.is_none());
        assert_eq!(manifest.source_dataset_sha256, "abcdef1234");
    }

    #[test]
    fn jsonl_is_byte_identical_on_rerun() {
        let temp = tempfile::tempdir().unwrap();
        let output1 = temp.path().join("slice1.jsonl");
        let output2 = temp.path().join("slice2.jsonl");

        let instances = vec![
            make_instance("repo__1", "owner/repo"),
            make_instance("repo__2", "owner/repo"),
        ];
        let filter_spec = FilterSpec {
            original_count: 5,
            selected_count: 2,
            instance_ids: None,
            limit: None,
            sample: Some(2),
            seed: Some(99),
            stratify_by: None,
            stratify_mode: None,
        };

        for output in [&output1, &output2] {
            run_subset(SubsetArgs {
                instances: instances.clone(),
                source_sha256: "sha256abc".to_owned(),
                alias: None,
                split: None,
                filter_spec: filter_spec.clone(),
                output,
            })
            .unwrap();
        }

        assert_eq!(
            std::fs::read(&output1).unwrap(),
            std::fs::read(&output2).unwrap(),
            "JSONL must be byte-identical across reruns"
        );
    }

    #[test]
    fn per_stratum_counts_present_when_stratify_by_set() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("stratified.jsonl");

        let instances = vec![
            make_instance("repoA__1", "owner/repoA"),
            make_instance("repoA__2", "owner/repoA"),
            make_instance("repoB__1", "owner/repoB"),
        ];
        let filter_spec = FilterSpec {
            original_count: 10,
            selected_count: 3,
            instance_ids: None,
            limit: None,
            sample: Some(3),
            seed: Some(7),
            stratify_by: Some(StratifyBy::Repo),
            stratify_mode: Some(StratifyMode::Proportional),
        };

        let manifest = run_subset(SubsetArgs {
            instances,
            source_sha256: "hash".to_owned(),
            alias: Some("verified".to_owned()),
            split: Some("test".to_owned()),
            filter_spec,
            output: &output,
        })
        .unwrap();

        let counts = manifest.per_stratum_counts.unwrap();
        assert_eq!(*counts.get("owner/repoA").unwrap(), 2);
        assert_eq!(*counts.get("owner/repoB").unwrap(), 1);
        assert_eq!(manifest.alias, Some("verified".to_owned()));
        assert_eq!(manifest.split, Some("test".to_owned()));
    }

    #[test]
    fn jsonl_roundtrips_through_load_dataset() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("roundtrip.jsonl");

        let instances = vec![
            make_instance("repo__1", "owner/repo"),
            make_instance("repo__2", "owner/repo"),
        ];
        let filter_spec = FilterSpec {
            original_count: 2,
            selected_count: 2,
            instance_ids: None,
            limit: None,
            sample: None,
            seed: None,
            stratify_by: None,
            stratify_mode: None,
        };

        run_subset(SubsetArgs {
            instances,
            source_sha256: "hash".to_owned(),
            alias: None,
            split: None,
            filter_spec,
            output: &output,
        })
        .unwrap();

        let bytes = std::fs::read(&output).unwrap();
        let loaded = crate::run::swebench::load_dataset_from_bytes_pub(&bytes).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].instance_id, "repo__1");
        assert_eq!(loaded[1].instance_id, "repo__2");
    }

    #[test]
    fn manifest_path_for_derives_correctly() {
        assert_eq!(
            manifest_path_for(Path::new("out/slice.jsonl")),
            PathBuf::from("out/slice.manifest.json")
        );
        assert_eq!(
            manifest_path_for(Path::new("slice")),
            PathBuf::from("slice.manifest.json")
        );
        assert_eq!(
            manifest_path_for(Path::new("a/b/c.jsonl")),
            PathBuf::from("a/b/c.manifest.json")
        );
    }
}
