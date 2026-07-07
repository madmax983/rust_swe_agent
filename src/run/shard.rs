//! Core logic for `bench shard` — deterministically partition a dataset JSONL
//! into N disjoint, balanced, provenance-stamped shards.
//!
//! This is the **producer** that feeds `bench merge`'s consumer.  The same
//! source `dataset_sha256` is stamped into every shard's sidecar manifest so a
//! downstream merge or audit can verify provenance.
//!
//! # Known limitation
//! A real per-shard sweep (`bench swebench --dataset-path shard-NNN.jsonl`)
//! records each shard JSONL's distinct content hash.  `bench merge`'s
//! `check_provenance_compatibility` requires identical `dataset.sha256` across
//! shards and will therefore reject sweep results produced from different JSONL
//! files.  Wiring `bench swebench` to stamp the *source* sha instead touches
//! the live agent loop and is out of scope (issue #548).  The round-trip
//! guarantee is therefore verified offline: re-uniting all shard JSONLs
//! reproduces the source instance-ID set exactly (0 dropped, 0 duplicated).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::run::swebench::{StratifyBy, StratifyMode, SweBenchInstance, partition_into_shards};

/// Schema-version tag written into every `shard-NNN.manifest.json` sidecar.
pub const MANIFEST_SCHEMA_VERSION: &str = "shard-manifest-v1";

/// Per-shard sidecar manifest written as `shard-NNN.manifest.json`.
///
/// `source_dataset_sha256` is **identical** across all N shards — it records
/// the content hash of the *original* whole-dataset JSONL, not the shard
/// slice.  This is the provenance anchor a future `bench merge` integration
/// will use to verify that shards came from the same source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardManifest {
    pub schema_version: String,
    /// SHA-256 hex digest of the *source* (whole) dataset bytes; identical
    /// across every shard in the same partition.
    pub source_dataset_sha256: String,
    pub shard_index: usize,
    pub shard_count: usize,
    pub seed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stratify_by: Option<StratifyBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stratify_mode: Option<StratifyMode>,
    pub instance_count: usize,
    pub resolved_instance_ids: Vec<String>,
    /// Per-repo instance counts; present only when `--stratify-by` was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_stratum_counts: Option<BTreeMap<String, usize>>,
    /// Named alias (`full`, `lite`, `verified`) if `--dataset` was used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Split selector (`train`, `test`, `dev`) if `--dataset` was used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
}

/// Summary of a `bench shard` run, emitted via `--format text|json`.
#[derive(Debug, Serialize)]
pub struct ShardReport {
    /// Number of output shards.
    pub shard_count: usize,
    /// Total instances across all shards (== source dataset instance count).
    pub total_instances: usize,
    /// Instance count per shard (indexed `0..shard_count`).
    pub per_shard_counts: Vec<usize>,
    /// `max(per_shard_counts) − min(per_shard_counts)`; guaranteed ≤ 1 on success.
    pub balance_spread: usize,
    /// SHA-256 hex digest of the source dataset (same value as every shard manifest).
    pub source_dataset_sha256: String,
}

impl ShardReport {
    pub fn render_text(&self) {
        use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};

        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec!["Shard", "Instances"]);

        for (i, count) in self.per_shard_counts.iter().enumerate() {
            table.add_row(vec![format!("shard-{i:03}"), count.to_string()]);
        }

        println!("\n=== bench shard ===");
        println!("Shards: {}", self.shard_count);
        println!("Total instances: {}", self.total_instances);
        println!("Balance spread: {}", self.balance_spread);
        println!("Dataset SHA256: {}", self.source_dataset_sha256);
        println!("\n{table}");
    }
}

/// Arguments passed to [`run_shard`].
pub struct ShardArgs<'a> {
    /// All instances from the source dataset (pre-partition).
    pub instances: Vec<SweBenchInstance>,
    /// SHA-256 hex digest of the *source* dataset bytes.
    pub source_sha256: String,
    /// Number of output shards (≥ 1 and ≤ instance count).
    pub n_shards: usize,
    /// Determinism seed.  `0` maps to a non-zero internal state in `XorShift64`.
    pub seed: u64,
    /// Optional stratification key (only `Repo` is currently supported).
    pub stratify_by: Option<StratifyBy>,
    /// Allocation mode when `stratify_by` is set.
    pub stratify_mode: StratifyMode,
    /// Destination directory for shard files and manifests.
    pub output: &'a Path,
    /// Overwrite non-empty output directory.
    pub force: bool,
    /// Named alias (`full`, `lite`, `verified`) if `--dataset` was used.
    pub alias: Option<&'a str>,
    /// Split selector (`train`, `test`, `dev`) if `--dataset` was used.
    pub split: Option<&'a str>,
}

/// Partition the dataset into N disjoint, balanced, provenance-stamped shard files.
///
/// Validates all invariants **before** writing any files.  On success, writes:
/// - `shard-000.jsonl` … `shard-(N-1).jsonl` (one line per instance)
/// - `shard-000.manifest.json` … `shard-(N-1).manifest.json` (sidecar per shard)
///
/// # Errors
/// - `--shards` is 0 or greater than the instance count
/// - Source dataset is empty
/// - Source dataset contains duplicate `instance_id` values
/// - Output directory is non-empty and `--force` was not set
/// - Any I/O failure while writing files
#[allow(clippy::too_many_lines)]
pub fn run_shard(args: ShardArgs<'_>) -> Result<ShardReport, Error> {
    // Validate --shards >= 1
    if args.n_shards == 0 {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "--shards must be at least 1".into(),
        )));
    }

    // Validate non-empty source
    if args.instances.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "source dataset is empty; cannot partition zero instances".into(),
        )));
    }

    // Detect duplicate IDs in source
    {
        let mut seen = std::collections::HashSet::new();
        let mut dupes: Vec<&str> = Vec::new();
        for inst in &args.instances {
            if !seen.insert(inst.instance_id.as_str()) {
                dupes.push(&inst.instance_id);
            }
        }
        if !dupes.is_empty() {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "source dataset contains duplicate instance_id(s): {}",
                dupes.join(", ")
            ))));
        }
    }

    // Validate --shards <= instance count (every shard must get at least one instance)
    if args.n_shards > args.instances.len() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "--shards {} is larger than the instance count ({}); \
             every shard must have at least one instance",
            args.n_shards,
            args.instances.len()
        ))));
    }

    // Prepare output directory
    prepare_output_dir(args.output, args.force)?;

    // Capture source provenance before the instances are moved into the
    // partitioner — this avoids cloning the (potentially very large) instance
    // vector just to retain the source ID set for the invariant check.
    let total_instances = args.instances.len();
    let source_ids: std::collections::BTreeSet<String> = args
        .instances
        .iter()
        .map(|i| i.instance_id.clone())
        .collect();

    // Partition into N groups (consumes the instance vector — no clone).
    let shards = partition_into_shards(
        args.instances,
        args.n_shards,
        args.seed,
        args.stratify_by,
        args.stratify_mode,
    );

    // Compute per-shard counts and balance spread once; reused below for both
    // the invariant assertion and the returned report.
    let per_shard_counts: Vec<usize> = shards.iter().map(Vec::len).collect();
    let min_count = per_shard_counts.iter().copied().min().unwrap_or(0);
    let max_count = per_shard_counts.iter().copied().max().unwrap_or(0);
    let balance_spread = max_count - min_count;

    // Assert coverage and disjointness invariants BEFORE writing any files.
    {
        let mut union_ids: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

        for shard in &shards {
            for inst in shard {
                if !union_ids.insert(inst.instance_id.as_str()) {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "partition invariant violated: instance '{}' appears in multiple shards",
                        inst.instance_id
                    ))));
                }
            }
        }

        let source_ids_ref: std::collections::BTreeSet<&str> =
            source_ids.iter().map(String::as_str).collect();
        if union_ids != source_ids_ref {
            let dropped: Vec<&str> = source_ids_ref.difference(&union_ids).copied().collect();
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "partition invariant violated: {} instance(s) were dropped: {}",
                dropped.len(),
                dropped.join(", ")
            ))));
        }

        if balance_spread > 1 {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "partition invariant violated: balance spread is {balance_spread} (expected ≤ 1)"
            ))));
        }
    }

    // Write shard JSONLs and per-shard sidecar manifests
    for (i, shard) in shards.iter().enumerate() {
        let jsonl_path = args.output.join(format!("shard-{i:03}.jsonl"));
        {
            use std::io::Write as _;
            let file = std::fs::File::create(&jsonl_path)?;
            let mut writer = std::io::BufWriter::new(file);
            for inst in shard {
                serde_json::to_writer(&mut writer, inst)?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
        }

        let per_stratum_counts = if args.stratify_by.is_some() {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for inst in shard {
                let key = inst.repo.as_deref().unwrap_or("<unknown>").to_owned();
                *counts.entry(key).or_default() += 1;
            }
            Some(counts)
        } else {
            None
        };

        let manifest = ShardManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.to_owned(),
            source_dataset_sha256: args.source_sha256.clone(),
            shard_index: i,
            shard_count: args.n_shards,
            seed: args.seed,
            stratify_by: args.stratify_by,
            // Always recorded: the mode affects leftover placement (`start`) in
            // `partition_into_shards` even when `stratify_by` is `None`, so the
            // manifest must capture it for the run to be reproducible from disk.
            stratify_mode: Some(args.stratify_mode),
            instance_count: shard.len(),
            resolved_instance_ids: shard.iter().map(|inst| inst.instance_id.clone()).collect(),
            per_stratum_counts,
            alias: args.alias.map(ToOwned::to_owned),
            split: args.split.map(ToOwned::to_owned),
        };

        let manifest_path = args.output.join(format!("shard-{i:03}.manifest.json"));
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
    }

    Ok(ShardReport {
        shard_count: args.n_shards,
        total_instances,
        per_shard_counts,
        balance_spread,
        source_dataset_sha256: args.source_sha256,
    })
}

fn prepare_output_dir(output: &Path, force: bool) -> Result<(), Error> {
    if output.exists() {
        if output.is_file() {
            if !force {
                return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "shard: output path '{}' already exists as a file; \
                     use --force to overwrite",
                    output.display()
                ))));
            }
            std::fs::remove_file(output).map_err(|e| {
                Error::Config(crate::error::ConfigError::Invalid(format!(
                    "shard: failed to remove existing file '{}': {e}",
                    output.display()
                )))
            })?;
        } else {
            let is_empty = output.read_dir().is_ok_and(|mut d| d.next().is_none());
            if !is_empty {
                if !force {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                        "shard: output directory '{}' already exists and is non-empty; \
                         use --force to overwrite",
                        output.display()
                    ))));
                }
                std::fs::remove_dir_all(output).map_err(|e| {
                    Error::Config(crate::error::ConfigError::Invalid(format!(
                        "shard: failed to clear output directory '{}': {e}",
                        output.display()
                    )))
                })?;
            }
        }
    }
    std::fs::create_dir_all(output).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "shard: failed to create output directory '{}': {e}",
            output.display()
        )))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::run::swebench::StratifyBy;

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

    fn instances_n(n: usize) -> Vec<SweBenchInstance> {
        (0..n)
            .map(|i| make_instance(&format!("repo__inst-{i:03}"), "owner/repo"))
            .collect()
    }

    // ── RED phase: tests that encode the acceptance criteria ──────────────────

    #[test]
    fn writes_n_jsonl_and_n_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let instances = instances_n(10);
        run_shard(ShardArgs {
            instances,
            source_sha256: "abc123".to_owned(),
            n_shards: 3,
            seed: 42,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        for i in 0..3_usize {
            assert!(
                temp.path().join(format!("shard-{i:03}.jsonl")).exists(),
                "shard-{i:03}.jsonl missing"
            );
            assert!(
                temp.path()
                    .join(format!("shard-{i:03}.manifest.json"))
                    .exists(),
                "shard-{i:03}.manifest.json missing"
            );
        }
    }

    #[test]
    fn coverage_union_equals_source_pairwise_disjoint() {
        let temp = tempfile::tempdir().unwrap();
        let instances = instances_n(20);
        let source_ids: std::collections::BTreeSet<String> =
            instances.iter().map(|i| i.instance_id.clone()).collect();

        run_shard(ShardArgs {
            instances,
            source_sha256: "sha".to_owned(),
            n_shards: 4,
            seed: 7,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        let mut all_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for i in 0..4_usize {
            let content =
                std::fs::read_to_string(temp.path().join(format!("shard-{i:03}.jsonl"))).unwrap();
            for line in content.lines() {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                let id = v["instance_id"].as_str().unwrap().to_owned();
                assert!(
                    all_ids.insert(id.clone()),
                    "instance '{id}' appears in multiple shards (pairwise-disjoint violated)"
                );
            }
        }
        assert_eq!(all_ids, source_ids, "union ≠ source (coverage violated)");
    }

    #[test]
    fn deterministic_byte_identical_across_runs() {
        let temp1 = tempfile::tempdir().unwrap();
        let temp2 = tempfile::tempdir().unwrap();

        for temp in [temp1.path(), temp2.path()] {
            run_shard(ShardArgs {
                instances: instances_n(15),
                source_sha256: "determ".to_owned(),
                n_shards: 3,
                seed: 99,
                stratify_by: None,
                stratify_mode: StratifyMode::Balanced,
                output: temp,
                force: false,
                alias: None,
                split: None,
            })
            .unwrap();
        }

        for i in 0..3_usize {
            let a = std::fs::read(temp1.path().join(format!("shard-{i:03}.jsonl"))).unwrap();
            let b = std::fs::read(temp2.path().join(format!("shard-{i:03}.jsonl"))).unwrap();
            assert_eq!(
                a, b,
                "shard-{i:03}.jsonl is not byte-identical across runs (determinism violated)"
            );
        }
    }

    #[test]
    fn balance_spread_at_most_one() {
        let temp = tempfile::tempdir().unwrap();
        // 17 instances into 4 shards → floor=4, ceil=5; spread must be ≤ 1
        let report = run_shard(ShardArgs {
            instances: instances_n(17),
            source_sha256: "s".to_owned(),
            n_shards: 4,
            seed: 5,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        assert!(
            report.balance_spread <= 1,
            "balance_spread = {} (expected ≤ 1)",
            report.balance_spread
        );
    }

    #[test]
    fn manifest_carries_correct_provenance() {
        let temp = tempfile::tempdir().unwrap();

        run_shard(ShardArgs {
            instances: instances_n(6),
            source_sha256: "sha256abc".to_owned(),
            n_shards: 2,
            seed: 13,
            stratify_by: Some(StratifyBy::Repo),
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: Some("lite"),
            split: Some("test"),
        })
        .unwrap();

        for i in 0..2_usize {
            let mpath = temp.path().join(format!("shard-{i:03}.manifest.json"));
            let manifest: ShardManifest =
                serde_json::from_str(&std::fs::read_to_string(&mpath).unwrap()).unwrap();

            assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
            assert_eq!(manifest.source_dataset_sha256, "sha256abc");
            assert_eq!(manifest.shard_index, i);
            assert_eq!(manifest.shard_count, 2);
            assert_eq!(manifest.seed, 13);
            assert_eq!(manifest.stratify_by, Some(StratifyBy::Repo));
            assert_eq!(manifest.alias, Some("lite".to_owned()));
            assert_eq!(manifest.split, Some("test".to_owned()));
            assert!(!manifest.resolved_instance_ids.is_empty());
        }

        // source_dataset_sha256 must be identical across all shards
        let get_sha = |idx: usize| -> String {
            let mpath = temp.path().join(format!("shard-{idx:03}.manifest.json"));
            let m: ShardManifest =
                serde_json::from_str(&std::fs::read_to_string(mpath).unwrap()).unwrap();
            m.source_dataset_sha256
        };
        assert_eq!(
            get_sha(0),
            get_sha(1),
            "source_dataset_sha256 must be identical across shards"
        );
    }

    #[test]
    fn offline_roundtrip_union_equals_source() {
        let temp = tempfile::tempdir().unwrap();
        let instances: Vec<SweBenchInstance> = (0..12)
            .map(|i| {
                make_instance(
                    &format!("repo-{}__inst-{i:03}", i % 3),
                    &format!("owner/repo-{}", i % 3),
                )
            })
            .collect();
        let source_ids: std::collections::BTreeSet<String> =
            instances.iter().map(|i| i.instance_id.clone()).collect();

        run_shard(ShardArgs {
            instances,
            source_sha256: "roundtrip".to_owned(),
            n_shards: 5,
            seed: 11,
            stratify_by: Some(StratifyBy::Repo),
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        // Re-uniting all shard JSONLs reproduces the source instance-ID set exactly.
        let mut union: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for i in 0..5_usize {
            let content =
                std::fs::read_to_string(temp.path().join(format!("shard-{i:03}.jsonl"))).unwrap();
            for line in content.lines().filter(|l| !l.is_empty()) {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                let id = v["instance_id"].as_str().unwrap().to_owned();
                assert!(
                    union.insert(id.clone()),
                    "duplicate id in round-trip union: {id}"
                );
            }
        }
        assert_eq!(union, source_ids, "offline round-trip: union ≠ source");
    }

    #[test]
    fn error_empty_dataset() {
        let temp = tempfile::tempdir().unwrap();
        let result = run_shard(ShardArgs {
            instances: vec![],
            source_sha256: "x".to_owned(),
            n_shards: 3,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        });
        assert!(result.is_err(), "empty dataset should error");
    }

    #[test]
    fn error_shards_exceeds_instance_count() {
        let temp = tempfile::tempdir().unwrap();
        let result = run_shard(ShardArgs {
            instances: instances_n(5),
            source_sha256: "x".to_owned(),
            n_shards: 10,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        });
        assert!(result.is_err(), "--shards > instance count should error");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("larger than"),
            "error message should mention 'larger than': {msg}"
        );
    }

    #[test]
    fn error_duplicate_ids_in_source() {
        let temp = tempfile::tempdir().unwrap();
        let mut instances = instances_n(5);
        instances.push(make_instance("repo__inst-000", "owner/repo")); // duplicate
        let result = run_shard(ShardArgs {
            instances,
            source_sha256: "x".to_owned(),
            n_shards: 2,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        });
        assert!(result.is_err(), "duplicate IDs in source should error");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("duplicate"),
            "error message should mention 'duplicate': {msg}"
        );
    }

    #[test]
    fn force_overwrites_non_empty_dir() {
        let temp = tempfile::tempdir().unwrap();
        let instances = instances_n(6);

        // First run succeeds
        run_shard(ShardArgs {
            instances: instances.clone(),
            source_sha256: "s".to_owned(),
            n_shards: 2,
            seed: 1,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        // Second run without --force fails
        assert!(
            run_shard(ShardArgs {
                instances: instances.clone(),
                source_sha256: "s".to_owned(),
                n_shards: 2,
                seed: 1,
                stratify_by: None,
                stratify_mode: StratifyMode::Balanced,
                output: temp.path(),
                force: false,
                alias: None,
                split: None,
            })
            .is_err(),
            "non-empty output without --force should error"
        );

        // Third run with --force succeeds
        run_shard(ShardArgs {
            instances,
            source_sha256: "s".to_owned(),
            n_shards: 2,
            seed: 1,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: true,
            alias: None,
            split: None,
        })
        .unwrap();
    }

    #[test]
    fn report_fields_correct() {
        let temp = tempfile::tempdir().unwrap();

        let report = run_shard(ShardArgs {
            instances: instances_n(10),
            source_sha256: "report_sha".to_owned(),
            n_shards: 3,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        assert_eq!(report.shard_count, 3);
        assert_eq!(report.total_instances, 10);
        assert_eq!(report.per_shard_counts.len(), 3);
        assert_eq!(report.per_shard_counts.iter().sum::<usize>(), 10);
        assert!(report.balance_spread <= 1);
        assert_eq!(report.source_dataset_sha256, "report_sha");
    }

    #[test]
    fn per_stratum_counts_present_when_stratify_by_set() {
        let temp = tempfile::tempdir().unwrap();
        let instances = vec![
            make_instance("repoA__1", "owner/repoA"),
            make_instance("repoA__2", "owner/repoA"),
            make_instance("repoB__1", "owner/repoB"),
            make_instance("repoB__2", "owner/repoB"),
        ];

        run_shard(ShardArgs {
            instances,
            source_sha256: "s".to_owned(),
            n_shards: 2,
            seed: 5,
            stratify_by: Some(StratifyBy::Repo),
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        for i in 0..2_usize {
            let mpath = temp.path().join(format!("shard-{i:03}.manifest.json"));
            let m: ShardManifest =
                serde_json::from_str(&std::fs::read_to_string(mpath).unwrap()).unwrap();
            assert!(
                m.per_stratum_counts.is_some(),
                "per_stratum_counts should be present when --stratify-by is set"
            );
        }
    }

    #[test]
    fn n_equals_1_puts_all_instances_in_one_shard() {
        let temp = tempfile::tempdir().unwrap();
        let instances = instances_n(8);

        run_shard(ShardArgs {
            instances,
            source_sha256: "s".to_owned(),
            n_shards: 1,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        assert!(temp.path().join("shard-000.jsonl").exists());
        assert!(!temp.path().join("shard-001.jsonl").exists());

        let content = std::fs::read_to_string(temp.path().join("shard-000.jsonl")).unwrap();
        assert_eq!(content.lines().count(), 8);
    }

    #[test]
    fn n_equals_instance_count_one_per_shard() {
        let temp = tempfile::tempdir().unwrap();
        let instances = instances_n(5);

        let report = run_shard(ShardArgs {
            instances,
            source_sha256: "s".to_owned(),
            n_shards: 5,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        assert_eq!(report.balance_spread, 0);
        for &count in &report.per_shard_counts {
            assert_eq!(count, 1, "each shard should have exactly 1 instance");
        }
    }

    #[test]
    fn stratify_mode_recorded_even_without_stratify_by() {
        let temp = tempfile::tempdir().unwrap();
        run_shard(ShardArgs {
            instances: instances_n(6),
            source_sha256: "s".to_owned(),
            n_shards: 2,
            seed: 3,
            stratify_by: None,
            stratify_mode: StratifyMode::Proportional,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        let m: ShardManifest = serde_json::from_str(
            &std::fs::read_to_string(temp.path().join("shard-000.manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(m.stratify_by, None);
        assert_eq!(
            m.stratify_mode,
            Some(StratifyMode::Proportional),
            "stratify_mode must be recorded even when stratify_by is None (it controls leftover placement)"
        );
    }

    #[test]
    fn stratify_by_repo_spreads_a_large_repo_evenly() {
        let temp = tempfile::tempdir().unwrap();
        // repo-a has 6 == 2 * n_shards instances → must land exactly 2 per shard
        // under --stratify-by repo, regardless of seed/start rotation.
        let mut instances = Vec::new();
        for i in 0..6 {
            instances.push(make_instance(&format!("a__{i:03}"), "owner/repo-a"));
        }
        for i in 0..3 {
            instances.push(make_instance(&format!("b__{i:03}"), "owner/repo-b"));
        }
        for i in 0..3 {
            instances.push(make_instance(&format!("c__{i:03}"), "owner/repo-c"));
        }

        run_shard(ShardArgs {
            instances,
            source_sha256: "s".to_owned(),
            n_shards: 3,
            seed: 1,
            stratify_by: Some(StratifyBy::Repo),
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        })
        .unwrap();

        for i in 0..3_usize {
            let m: ShardManifest = serde_json::from_str(
                &std::fs::read_to_string(temp.path().join(format!("shard-{i:03}.manifest.json")))
                    .unwrap(),
            )
            .unwrap();
            let a_count = m
                .per_stratum_counts
                .unwrap()
                .get("owner/repo-a")
                .copied()
                .unwrap_or(0);
            assert_eq!(
                a_count, 2,
                "--stratify-by repo must spread repo-a exactly 2 per shard (got {a_count} in shard-{i:03})"
            );
        }
    }

    #[test]
    fn error_n_shards_zero() {
        let temp = tempfile::tempdir().unwrap();
        let result = run_shard(ShardArgs {
            instances: instances_n(4),
            source_sha256: "x".to_owned(),
            n_shards: 0,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: temp.path(),
            force: false,
            alias: None,
            split: None,
        });
        assert!(result.is_err(), "--shards 0 should error");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("at least 1"),
            "error message should mention 'at least 1': {msg}"
        );
    }

    #[test]
    fn output_path_is_a_file_without_force_errors() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("collides");
        std::fs::write(&file_path, b"not a directory").unwrap();

        let result = run_shard(ShardArgs {
            instances: instances_n(4),
            source_sha256: "x".to_owned(),
            n_shards: 2,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: &file_path,
            force: false,
            alias: None,
            split: None,
        });
        assert!(
            result.is_err(),
            "output path that is an existing file must error without --force"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("as a file"),
            "error message should mention the file collision: {msg}"
        );
        // The pre-existing file must be left untouched on the rejection path.
        assert!(file_path.is_file(), "existing file should not be removed");
    }

    #[test]
    fn output_path_is_a_file_with_force_replaces_it_with_a_dir() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("collides");
        std::fs::write(&file_path, b"not a directory").unwrap();

        run_shard(ShardArgs {
            instances: instances_n(4),
            source_sha256: "x".to_owned(),
            n_shards: 2,
            seed: 0,
            stratify_by: None,
            stratify_mode: StratifyMode::Balanced,
            output: &file_path,
            force: true,
            alias: None,
            split: None,
        })
        .unwrap();

        assert!(
            file_path.is_dir(),
            "with --force the colliding file should be replaced by the output directory"
        );
        assert!(file_path.join("shard-000.jsonl").exists());
        assert!(file_path.join("shard-001.jsonl").exists());
    }
}
