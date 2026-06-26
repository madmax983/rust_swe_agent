//! Integration tests for `bench shard` — deterministic dataset partitioning.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::fs;
use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

// ── fixture helpers ───────────────────────────────────────────────────────────

/// Write a minimal JSONL dataset fixture with `n` instances spread across `repos` repo names.
fn write_dataset(path: &Path, n: usize, repos: &[&str]) {
    let mut jsonl = String::new();
    for i in 0..n {
        let repo = repos[i % repos.len()];
        let id = format!("{repo}__inst-{i:03}", repo = repo.replace('/', "_"));
        jsonl.push_str(
            &serde_json::to_string(&serde_json::json!({
                "instance_id": id,
                "repo": repo,
                "base_commit": "abc",
                "problem_statement": format!("Fix issue {i}"),
            }))
            .unwrap(),
        );
        jsonl.push('\n');
    }
    fs::write(path, jsonl).unwrap();
}

// ── happy path ────────────────────────────────────────────────────────────────

#[test]
fn happy_path_creates_n_shards_and_manifests() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 20, &["owner/repo-a", "owner/repo-b", "owner/repo-c"]);
    let output = work.path().join("shards");

    let out = Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("4")
        .arg("--output")
        .arg(&output)
        .arg("--seed")
        .arg("42")
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench shard failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    for i in 0..4_usize {
        assert!(
            output.join(format!("shard-{i:03}.jsonl")).exists(),
            "shard-{i:03}.jsonl missing"
        );
        assert!(
            output.join(format!("shard-{i:03}.manifest.json")).exists(),
            "shard-{i:03}.manifest.json missing"
        );
    }
}

#[test]
fn union_equals_source_and_pairwise_disjoint() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 20, &["owner/repo-a", "owner/repo-b"]);
    let output = work.path().join("shards");

    let status = Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("4")
        .arg("--output")
        .arg(&output)
        .arg("--seed")
        .arg("7")
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "bench shard should succeed");

    // Collect source IDs
    let source_content = fs::read_to_string(&dataset).unwrap();
    let source_ids: std::collections::BTreeSet<String> = source_content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            v["instance_id"].as_str().unwrap().to_owned()
        })
        .collect();

    // Collect shard IDs, checking disjointness as we go
    let mut union: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for i in 0..4_usize {
        let content = fs::read_to_string(output.join(format!("shard-{i:03}.jsonl"))).unwrap();
        for line in content.lines().filter(|l| !l.is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let id = v["instance_id"].as_str().unwrap().to_owned();
            assert!(
                union.insert(id.clone()),
                "instance '{id}' appears in multiple shards"
            );
        }
    }
    assert_eq!(union, source_ids, "union ≠ source");
}

#[test]
fn deterministic_byte_identical_shards() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 15, &["owner/repo"]);

    let out1 = work.path().join("shards1");
    let out2 = work.path().join("shards2");

    for output in [&out1, &out2] {
        let status = Command::new(binary_path())
            .args(["bench", "shard"])
            .arg("--dataset-path")
            .arg(&dataset)
            .arg("--shards")
            .arg("3")
            .arg("--output")
            .arg(output)
            .arg("--seed")
            .arg("99")
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "bench shard failed on run to {}", output.display());
    }

    for i in 0..3_usize {
        let a = fs::read(out1.join(format!("shard-{i:03}.jsonl"))).unwrap();
        let b = fs::read(out2.join(format!("shard-{i:03}.jsonl"))).unwrap();
        assert_eq!(a, b, "shard-{i:03}.jsonl is not byte-identical across runs");
    }
}

// ── --format json ─────────────────────────────────────────────────────────────

#[test]
fn json_format_contains_required_fields() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 16, &["owner/repo-a", "owner/repo-b"]);
    let output = work.path().join("shards");

    let out = Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("4")
        .arg("--output")
        .arg(&output)
        .arg("--seed")
        .arg("0")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "bench shard --format json failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\n{stdout}"));

    assert!(report.get("shard_count").is_some(), "missing 'shard_count':\n{report}");
    assert!(report.get("total_instances").is_some(), "missing 'total_instances':\n{report}");
    assert!(report.get("per_shard_counts").is_some(), "missing 'per_shard_counts':\n{report}");
    assert!(
        report.get("source_dataset_sha256").is_some(),
        "missing 'source_dataset_sha256':\n{report}"
    );
    assert!(report.get("balance_spread").is_some(), "missing 'balance_spread':\n{report}");

    assert_eq!(report["shard_count"].as_u64().unwrap(), 4);
    assert_eq!(report["total_instances"].as_u64().unwrap(), 16);
    let spread = report["balance_spread"].as_u64().unwrap();
    assert!(spread <= 1, "balance_spread = {spread} (expected ≤ 1)");
    let per_shard: Vec<u64> = report["per_shard_counts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(per_shard.len(), 4);
    assert_eq!(per_shard.iter().sum::<u64>(), 16);
}

// ── error cases ───────────────────────────────────────────────────────────────

#[test]
fn error_shards_greater_than_instance_count() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 5, &["owner/repo"]);
    let output = work.path().join("shards");

    let out = Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("10") // 10 > 5
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "bench shard should exit non-zero when --shards > instance count"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("larger than") || combined.contains("instance count"),
        "error output should mention 'larger than' or 'instance count':\n{combined}"
    );
}

#[test]
fn non_empty_output_without_force_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 8, &["owner/repo"]);
    let output = work.path().join("shards");

    // First run
    let first = Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("2")
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(first.status.success(), "first run should succeed");

    // Second run without --force should fail
    let second = Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("2")
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        !second.status.success(),
        "second run without --force should exit non-zero"
    );

    // Third run with --force should succeed
    let third = Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("2")
        .arg("--output")
        .arg(&output)
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        third.status.success(),
        "run with --force should succeed:\n{}",
        String::from_utf8_lossy(&third.stderr)
    );
}

#[test]
fn balance_by_exits_nonzero_with_clear_message() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 8, &["owner/repo"]);
    let output = work.path().join("shards");

    let out = Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("2")
        .arg("--output")
        .arg(&output)
        .arg("--balance-by")
        .arg("estimated_cost_usd")
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "--balance-by should exit non-zero (not yet implemented)"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("not yet implemented") || combined.contains("reserved"),
        "--balance-by error should mention 'not yet implemented' or 'reserved':\n{combined}"
    );
}

// ── manifest provenance ───────────────────────────────────────────────────────

#[test]
fn all_manifests_carry_same_source_sha256() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 12, &["owner/repo-a", "owner/repo-b"]);
    let output = work.path().join("shards");

    Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("3")
        .arg("--output")
        .arg(&output)
        .arg("--seed")
        .arg("55")
        .status()
        .unwrap();

    let shas: Vec<String> = (0..3_usize)
        .map(|i| {
            let content =
                fs::read_to_string(output.join(format!("shard-{i:03}.manifest.json"))).unwrap();
            let v: serde_json::Value = serde_json::from_str(&content).unwrap();
            v["source_dataset_sha256"].as_str().unwrap().to_owned()
        })
        .collect();

    for sha in &shas[1..] {
        assert_eq!(
            &shas[0],
            sha,
            "source_dataset_sha256 should be identical across all shard manifests"
        );
    }

    // sha256 must be a non-empty hex string
    assert!(
        !shas[0].is_empty(),
        "source_dataset_sha256 should not be empty"
    );
}

#[test]
fn manifest_schema_version_correct() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("dataset.jsonl");
    write_dataset(&dataset, 6, &["owner/repo"]);
    let output = work.path().join("shards");

    Command::new(binary_path())
        .args(["bench", "shard"])
        .arg("--dataset-path")
        .arg(&dataset)
        .arg("--shards")
        .arg("2")
        .arg("--output")
        .arg(&output)
        .status()
        .unwrap();

    for i in 0..2_usize {
        let content =
            fs::read_to_string(output.join(format!("shard-{i:03}.manifest.json"))).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            v["schema_version"].as_str().unwrap(),
            "shard-manifest-v1",
            "wrong schema_version in shard-{i:03}.manifest.json"
        );
    }
}
