//! `bench merge` — combine sharded sweep result directories into one canonical aggregate.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod support;
use support::binary_path;

// ── fixture helpers ───────────────────────────────────────────────────────────

/// A minimal completed sweep fixture for testing.
struct ShardFixture {
    dir: PathBuf,
}

impl ShardFixture {
    fn create(root: &Path, label: &str, instances: &[InstanceSpec]) -> Self {
        let dir = root.join(label);
        fs::create_dir_all(&dir).unwrap();

        let mut inst_results = Vec::new();
        let mut total_cost = 0.0_f64;
        let mut total_input = 0_u64;
        let mut total_completion = 0_u64;
        let mut preds = String::new();

        for spec in instances {
            total_cost += spec.cost;
            total_input += spec.input_tokens;
            total_completion += spec.completion_tokens;

            let submitted = spec.outcome == "submitted";
            let pass_at_1 = submitted;

            if submitted {
                preds.push_str(
                    &serde_json::to_string(&serde_json::json!({
                        "instance_id": spec.id,
                        "model_patch": format!("--- a/{id}\n+++ b/{id}\n", id = spec.id),
                        "model_name_or_path": "deterministic",
                    }))
                    .unwrap(),
                );
                preds.push('\n');
            }

            inst_results.push(serde_json::json!({
                "instance_id": spec.id,
                "exit_reason": spec.outcome,
                "outcome": spec.outcome,
                "cost_usd": spec.cost,
                "total_input_tokens": spec.input_tokens,
                "total_completion_tokens": spec.completion_tokens,
                "duration_secs": 1.0,
                "patch_present": submitted,
                "non_empty_patch": submitted,
                "attempts": 1,
                "runs": 1,
                "resolved_count": u8::from(submitted),
                "pass_at_1": pass_at_1,
                "tests_run_before_submit": false,
                "steps": 2
            }));

            // Budget-halted tasks never ran: no trajectory/patch on disk (mirrors a
            // real budget-capped sweep, where audit counts them from results.json).
            if spec.outcome != "budget_halted" {
                // Write a trajectory file (nested layout: <id>/run-1.traj.json)
                let inst_dir = dir.join(spec.id);
                fs::create_dir_all(&inst_dir).unwrap();
                let traj = serde_json::json!({
                    "trajectory_format": "mini-swe-agent-1.1",
                    "artifact_kind": "trajectory",
                    "schema_version": {"major": 1, "minor": 3},
                    "info": {
                        "task": spec.id,
                        "model_name": "deterministic",
                        "outcome": spec.outcome,
                        "exit_reason": spec.outcome,
                        "total_cost_usd": spec.cost,
                        "token_usage": {
                            "prompt_tokens": spec.input_tokens,
                            "completion_tokens": spec.completion_tokens
                        },
                        "redaction": {"enabled": false, "redacted": false},
                        "steps": 2,
                        "test_invocations": [],
                        "tests_run_before_submit": false
                    },
                    "messages": []
                });
                fs::write(
                    inst_dir.join("run-1.traj.json"),
                    serde_json::to_string_pretty(&traj).unwrap(),
                )
                .unwrap();

                // Write a patch for submitted instances
                if submitted {
                    fs::write(
                        inst_dir.join("run-1.patch"),
                        format!(
                            "--- a/{id}\n+++ b/{id}\n@@ -0,0 +1 @@\n+fix\n",
                            id = spec.id
                        ),
                    )
                    .unwrap();
                }
            }
        }

        // Predictions: aggregate + single-run file (mirrors a real single-run sweep).
        fs::write(dir.join("all_preds.jsonl"), &preds).unwrap();
        fs::write(dir.join("all_preds.run-1.jsonl"), &preds).unwrap();

        let submitted_count = instances
            .iter()
            .filter(|i| i.outcome == "submitted")
            .count();
        let errored_count = instances.iter().filter(|i| i.outcome == "error").count();
        let budget_halted_count = instances
            .iter()
            .filter(|i| i.outcome == "budget_halted")
            .count();
        #[allow(clippy::cast_precision_loss)]
        let pass_at_k = if instances.is_empty() {
            0.0
        } else {
            submitted_count as f64 / instances.len() as f64
        };

        let results = serde_json::json!({
            "artifact_kind": "sweep_results",
            "schema_version": {"major": 1, "minor": 11},
            "total": instances.len(),
            "sweep_status": "completed",
            "completed": instances.len(),
            "in_flight_at_cancel": 0,
            "not_started": 0,
            "submitted": submitted_count,
            "submitted_with_tests": 0,
            "skipped": 0,
            "errored": errored_count,
            "failures_by_category": {},
            "budget_halted": budget_halted_count,
            "with_patch": submitted_count,
            "patch_empty": 0,
            "patch_apply_invalid": 0,
            "github_pr_failures": 0,
            "total_input_tokens": total_input,
            "total_cache_read_tokens": 0,
            "total_cache_creation_tokens": 0,
            "total_completion_tokens": total_completion,
            "total_cost_usd": total_cost,
            "cache_hit_rate": 0.0,
            "retries": 0,
            "retried_instances": 0,
            "pass_at_k": pass_at_k,
            "filter_spec": {
                "original_count": instances.len(),
                "selected_count": instances.len()
            },
            "manifest": fixture_manifest(instances.len()),
            "instances": inst_results
        });

        fs::write(
            dir.join("results.json"),
            serde_json::to_string_pretty(&results).unwrap(),
        )
        .unwrap();

        Self { dir }
    }

    fn create_with_eval(
        root: &Path,
        label: &str,
        instances: &[InstanceSpec],
        eval_instances: &[(&str, bool)],
    ) -> Self {
        let fixture = Self::create(root, label, instances);
        let eval_data = serde_json::json!({
            "artifact_kind": "evaluation_results",
            "schema_version": {"major": 1, "minor": 3},
            "instances": eval_instances.iter().map(|(id, resolved)| serde_json::json!({
                "instance_id": id,
                "resolved": resolved,
                "runs": 1,
                "tests_passed": [],
                "tests_failed": [],
                "eval_exit_reason": if *resolved { "resolved" } else { "unresolved" }
            })).collect::<Vec<_>>()
        });
        fs::write(
            fixture.dir.join("evaluation.json"),
            serde_json::to_string_pretty(&eval_data).unwrap(),
        )
        .unwrap();
        fixture
    }

    /// Like `create_with_eval` but writes a *legacy* sb-cli `evaluation.json`
    /// (`resolved_ids`/`submitted_ids` arrays) rather than the modern format.
    fn create_with_legacy_eval(
        root: &Path,
        label: &str,
        instances: &[InstanceSpec],
        resolved_ids: &[&str],
    ) -> Self {
        let fixture = Self::create(root, label, instances);
        let submitted_ids: Vec<&str> = instances
            .iter()
            .filter(|i| i.outcome == "submitted")
            .map(|i| i.id)
            .collect();
        let eval_data = serde_json::json!({
            "resolved_ids": resolved_ids,
            "submitted_ids": submitted_ids,
        });
        fs::write(
            fixture.dir.join("evaluation.json"),
            serde_json::to_string_pretty(&eval_data).unwrap(),
        )
        .unwrap();
        fixture
    }

    /// A completed pass@k shard: each instance runs `runs` times (run-1..run-N
    /// trajectories). Cost/tokens are zero to keep audit reconciliation trivial;
    /// the point is per-run-slot outcome counting. `instances` is `(id, outcome)`.
    fn create_multi_run(
        root: &Path,
        label: &str,
        instances: &[(&'static str, &'static str)],
        runs: u32,
    ) -> Self {
        let dir = root.join(label);
        fs::create_dir_all(&dir).unwrap();

        let mut inst_results = Vec::new();
        let mut aggregate_preds = String::new();
        let mut per_run_preds: Vec<String> = vec![String::new(); runs as usize];
        let mut slot_submitted = 0usize;
        let mut slot_errored = 0usize;

        for (id, outcome) in instances {
            let submitted = *outcome == "submitted";
            let inst_dir = dir.join(id);
            fs::create_dir_all(&inst_dir).unwrap();
            for k in 1..=runs {
                let traj = serde_json::json!({
                    "trajectory_format": "mini-swe-agent-1.1",
                    "artifact_kind": "trajectory",
                    "schema_version": {"major": 1, "minor": 3},
                    "info": {
                        "task": id,
                        "model_name": "deterministic",
                        "outcome": outcome,
                        "exit_reason": outcome,
                        "total_cost_usd": 0.0,
                        "token_usage": {"prompt_tokens": 0, "completion_tokens": 0},
                        "redaction": {"enabled": false, "redacted": false},
                        "steps": 2,
                        "test_invocations": [],
                        "tests_run_before_submit": false
                    },
                    "messages": []
                });
                fs::write(
                    inst_dir.join(format!("run-{k}.traj.json")),
                    serde_json::to_string_pretty(&traj).unwrap(),
                )
                .unwrap();
                if submitted {
                    fs::write(inst_dir.join(format!("run-{k}.patch")), "patch\n").unwrap();
                    per_run_preds[(k - 1) as usize].push_str(
                        &serde_json::to_string(&serde_json::json!({
                            "instance_id": id,
                            "model_patch": "patch",
                            "model_name_or_path": "deterministic",
                            "run_index": k,
                        }))
                        .unwrap(),
                    );
                    per_run_preds[(k - 1) as usize].push('\n');
                    aggregate_preds.push_str(
                        &serde_json::to_string(&serde_json::json!({
                            "instance_id": format!("{id}::run-{k}"),
                            "original_instance_id": id,
                            "run_index": k,
                            "model_patch": "patch",
                            "model_name_or_path": "deterministic",
                        }))
                        .unwrap(),
                    );
                    aggregate_preds.push('\n');
                    slot_submitted += 1;
                } else if *outcome == "error" {
                    slot_errored += 1;
                }
            }

            inst_results.push(serde_json::json!({
                "instance_id": id,
                "exit_reason": outcome,
                "outcome": outcome,
                "cost_usd": 0.0,
                "total_input_tokens": 0,
                "total_completion_tokens": 0,
                "duration_secs": 1.0,
                "patch_present": submitted,
                "non_empty_patch": submitted,
                "attempts": runs,
                "runs": runs,
                "resolved_count": 0,
                "pass_at_1": false,
                "tests_run_before_submit": false,
                "steps": 2
            }));
        }

        fs::write(dir.join("all_preds.jsonl"), &aggregate_preds).unwrap();
        for k in 1..=runs {
            fs::write(
                dir.join(format!("all_preds.run-{k}.jsonl")),
                &per_run_preds[(k - 1) as usize],
            )
            .unwrap();
        }

        let results = serde_json::json!({
            "artifact_kind": "sweep_results",
            "schema_version": {"major": 1, "minor": 11},
            "total": instances.len(),
            "sweep_status": "completed",
            "completed": instances.len(),
            "in_flight_at_cancel": 0,
            "not_started": 0,
            "submitted": slot_submitted,
            "submitted_with_tests": 0,
            "skipped": 0,
            "errored": slot_errored,
            "failures_by_category": {},
            "budget_halted": 0,
            "with_patch": instances.iter().filter(|(_, o)| *o == "submitted").count(),
            "patch_empty": 0,
            "patch_apply_invalid": 0,
            "github_pr_failures": 0,
            "total_input_tokens": 0,
            "total_cache_read_tokens": 0,
            "total_cache_creation_tokens": 0,
            "total_completion_tokens": 0,
            "total_cost_usd": 0.0,
            "cache_hit_rate": 0.0,
            "retries": 0,
            "retried_instances": 0,
            "pass_at_k": 0.0,
            "filter_spec": {
                "original_count": instances.len(),
                "selected_count": instances.len()
            },
            "manifest": fixture_manifest(instances.len()),
            "instances": inst_results
        });
        fs::write(
            dir.join("results.json"),
            serde_json::to_string_pretty(&results).unwrap(),
        )
        .unwrap();

        Self { dir }
    }
}

/// Shared provenance manifest used by every fixture shard so the cross-shard
/// dataset/model/config compatibility checks pass.
fn fixture_manifest(instance_count: usize) -> serde_json::Value {
    serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "fixture",
            "git_sha": "fixture-sha-abc123",
            "git_dirty": false,
            "git_resolution": "ok"
        },
        "dataset": {
            "path": "dataset.jsonl",
            "sha256": "fixture-dataset-hash-abc",
            "instance_count": instance_count,
            "source_kind": "local",
            "selected_row_count": instance_count,
            "post_filter_row_count": instance_count
        },
        "prompt_template": {
            "source": "inline",
            "sha256": "fixture-template-hash"
        },
        "config": {
            "resolved": "[model]\nname = \"deterministic\"\n",
            "overlay_paths": []
        },
        "model": {
            "name": "deterministic",
            "backend": "deterministic"
        },
        "runtime": {
            "started_at_utc": "2026-05-01T00:00:00Z",
            "finished_at_utc": "2026-05-01T00:00:01Z",
            "host_os": "fixture",
            "resume_mode": false
        },
        "cli": {
            "argv": ["max", "bench", "swebench"]
        }
    })
}

struct InstanceSpec {
    id: &'static str,
    outcome: &'static str,
    cost: f64,
    input_tokens: u64,
    completion_tokens: u64,
}

impl InstanceSpec {
    fn submitted(id: &'static str, cost: f64) -> Self {
        Self {
            id,
            outcome: "submitted",
            cost,
            input_tokens: 100,
            completion_tokens: 20,
        }
    }
    fn errored(id: &'static str) -> Self {
        Self {
            id,
            outcome: "error",
            cost: 0.05,
            input_tokens: 50,
            completion_tokens: 10,
        }
    }
    fn budget_halted(id: &'static str) -> Self {
        Self {
            id,
            outcome: "budget_halted",
            cost: 0.0,
            input_tokens: 0,
            completion_tokens: 0,
        }
    }
}

// ── help / basic invocation ───────────────────────────────────────────────────

#[test]
fn help_lists_merge_subcommand() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("merge"),
        "expected 'merge' in bench --help:\n{stdout}"
    );
}

#[test]
fn merge_help_lists_expected_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "merge", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in ["--shard", "--output", "--on-collision", "--format"] {
        assert!(
            stdout.contains(flag),
            "expected '{flag}' in bench merge --help:\n{stdout}"
        );
    }
}

// ── happy path ────────────────────────────────────────────────────────────────

#[test]
fn two_shard_merge_produces_correct_aggregates() {
    let work = tempfile::tempdir().unwrap();

    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[
            InstanceSpec::submitted("django__django-001", 0.10),
            InstanceSpec::errored("django__django-002"),
        ],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[
            InstanceSpec::submitted("django__django-003", 0.20),
            InstanceSpec::errored("django__django-004"),
        ],
    );

    let output = work.path().join("merged");

    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench merge failed!\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert!(output.join("results.json").exists(), "results.json missing");

    let merged: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("results.json")).unwrap()).unwrap();

    // AC2: arithmetically correct counts
    assert_eq!(merged["total"], 4, "total should be 4");
    assert_eq!(merged["submitted"], 2, "submitted should be 2");
    assert_eq!(merged["errored"], 2, "errored should be 2");
    assert_eq!(merged["sweep_status"], "completed");

    // AC2: total cost = sum of per-shard costs (0.10 + 0.05 + 0.20 + 0.05 = 0.40)
    let cost = merged["total_cost_usd"].as_f64().unwrap();
    assert!(
        (cost - 0.40).abs() < 0.001,
        "expected total_cost_usd ≈ 0.40 got {cost}"
    );

    // AC2: pass_at_k recomputed over union = 2/4 = 0.5
    let pak = merged["pass_at_k"].as_f64().unwrap();
    assert!(
        (pak - 0.5).abs() < 0.001,
        "expected pass_at_k ≈ 0.5 got {pak}"
    );

    // AC2: total tokens correct
    assert_eq!(merged["total_input_tokens"], 100 + 50 + 100 + 50);
}

#[test]
fn two_shard_merge_then_audit_passes() {
    let work = tempfile::tempdir().unwrap();

    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[
            InstanceSpec::submitted("inst-001", 0.10),
            InstanceSpec::errored("inst-002"),
        ],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-003", 0.15)],
    );

    let output = work.path().join("merged");

    // Merge
    let merge_out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    let merge_stdout = String::from_utf8_lossy(&merge_out.stdout);
    let merge_stderr = String::from_utf8_lossy(&merge_out.stderr);
    assert!(
        merge_out.status.success(),
        "bench merge failed!\nstdout:\n{merge_stdout}\nstderr:\n{merge_stderr}"
    );

    // AC4/AC5: bench audit must pass on merged output
    let audit_out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&output)
        .output()
        .unwrap();

    let audit_stdout = String::from_utf8_lossy(&audit_out.stdout);
    let audit_stderr = String::from_utf8_lossy(&audit_out.stderr);
    assert!(
        audit_out.status.success(),
        "bench audit failed on merged sweep!\nstdout:\n{audit_stdout}\nstderr:\n{audit_stderr}"
    );
    assert!(
        audit_stdout.contains("pass") || output.join("audit.json").exists(),
        "audit should produce audit.json:\n{audit_stdout}"
    );

    // Verify audit.json says pass
    if output.join("audit.json").exists() {
        let audit_json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(output.join("audit.json")).unwrap()).unwrap();
        assert_eq!(
            audit_json["overall_pass_fail"], "pass",
            "audit should pass:\n{audit_json}"
        );
    }
}

#[test]
fn merged_dir_works_with_report_and_triage() {
    let work = tempfile::tempdir().unwrap();

    // Create shards with evaluation.json so bench triage can run
    let shard_a = ShardFixture::create_with_eval(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
        &[("inst-001", true)],
    );
    let shard_b = ShardFixture::create_with_eval(
        work.path(),
        "shard_b",
        &[InstanceSpec::errored("inst-002")],
        &[("inst-002", false)],
    );

    let output = work.path().join("merged");
    let report_output = work.path().join("report.md");

    // Merge
    let merge_out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        merge_out.status.success(),
        "merge failed: {:?}",
        String::from_utf8_lossy(&merge_out.stderr)
    );

    // AC5: bench report works on merged dir
    let report_out = Command::new(binary_path())
        .args(["bench", "report", "--sweep"])
        .arg(&output)
        .arg("--output")
        .arg(&report_output)
        .output()
        .unwrap();
    let report_stderr = String::from_utf8_lossy(&report_out.stderr);
    assert!(
        report_out.status.success(),
        "bench report failed on merged sweep!\nstderr:\n{report_stderr}"
    );

    // AC5: bench triage works on merged dir
    let triage_out = Command::new(binary_path())
        .args(["bench", "triage", "--sweep"])
        .arg(&output)
        .output()
        .unwrap();
    let triage_stderr = String::from_utf8_lossy(&triage_out.stderr);
    assert!(
        triage_out.status.success(),
        "bench triage failed on merged sweep!\nstderr:\n{triage_stderr}"
    );
}

// ── AC3: collision detection ──────────────────────────────────────────────────

#[test]
fn collision_default_error_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();

    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[
            InstanceSpec::submitted("shared-inst-001", 0.10),
            InstanceSpec::submitted("unique-a-001", 0.05),
        ],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[
            InstanceSpec::submitted("shared-inst-001", 0.10), // collision!
            InstanceSpec::submitted("unique-b-001", 0.05),
        ],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "bench merge should fail on ID collision (default --on-collision error)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("shared-inst-001"),
        "error output should name the colliding id:\n{combined}"
    );
}

#[test]
fn collision_first_wins_exits_zero_and_reports_duplicate() {
    let work = tempfile::tempdir().unwrap();

    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[
            InstanceSpec::submitted("shared-001", 0.10),
            InstanceSpec::submitted("unique-a", 0.05),
        ],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[
            InstanceSpec::submitted("shared-001", 0.20), // collision
            InstanceSpec::submitted("unique-b", 0.05),
        ],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args([
            "bench",
            "merge",
            "--on-collision",
            "first-wins",
            "--format",
            "json",
        ])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bench merge --on-collision first-wins should exit 0!\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["duplicates"], 1,
        "should report 1 duplicate:\n{report}"
    );
    // first-wins: shared-001 from shard_a (cost 0.10) wins
    // total = 3 distinct instances: shared-001 (0.10), unique-a (0.05), unique-b (0.05)
    assert_eq!(report["total_instances"], 3);
}

// ── AC6: --format json summary ────────────────────────────────────────────────

#[test]
fn json_format_summary_contains_required_keys() {
    let work = tempfile::tempdir().unwrap();

    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b =
        ShardFixture::create(work.path(), "shard_b", &[InstanceSpec::errored("inst-002")]);

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge", "--format", "json"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    // AC6: shards merged, instances per shard, total instances, duplicates, top-line metrics
    assert!(
        report.get("shards").is_some(),
        "missing 'shards' key:\n{report}"
    );
    assert!(
        report.get("total_instances").is_some(),
        "missing 'total_instances':\n{report}"
    );
    assert!(
        report.get("duplicates").is_some(),
        "missing 'duplicates':\n{report}"
    );
    assert!(
        report.get("total_cost_usd").is_some(),
        "missing 'total_cost_usd':\n{report}"
    );
    assert!(
        report.get("submitted").is_some(),
        "missing 'submitted':\n{report}"
    );
    assert!(
        report.get("errored").is_some(),
        "missing 'errored':\n{report}"
    );
    assert!(
        report.get("pass_at_k").is_some(),
        "missing 'pass_at_k':\n{report}"
    );

    // Per-shard counts in the shards array
    let shards = report["shards"].as_array().unwrap();
    assert_eq!(shards.len(), 2, "should have 2 shard entries:\n{report}");
    assert!(
        shards[0].get("instance_count").is_some(),
        "shard missing 'instance_count':\n{report}"
    );
    assert!(
        shards[1].get("instance_count").is_some(),
        "shard missing 'instance_count':\n{report}"
    );

    assert_eq!(report["total_instances"], 2);
    assert_eq!(report["duplicates"], 0);
    assert_eq!(report["submitted"], 1);
    assert_eq!(report["errored"], 1);
}

// ── provenance divergence ─────────────────────────────────────────────────────

#[test]
fn provenance_divergence_dataset_sha_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();

    // Create shard_a with one dataset hash
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    // Mutate shard_b's dataset sha256
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );
    let results_path = shard_b.dir.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&results_path).unwrap()).unwrap();
    results["manifest"]["dataset"]["sha256"] = serde_json::json!("different-hash-xyz");
    fs::write(
        &results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "bench merge should fail when dataset_sha256 differs"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("dataset")
            || combined.contains("sha256")
            || combined.contains("provenance"),
        "error should mention dataset/sha256 divergence:\n{combined}"
    );
}

/// Patch a single dotted-path field inside a shard's results.json manifest.
fn patch_manifest_field(shard_dir: &Path, pointer: &str, value: serde_json::Value) {
    let path = shard_dir.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    *results.pointer_mut(pointer).unwrap() = value;
    fs::write(&path, serde_json::to_string_pretty(&results).unwrap()).unwrap();
}

#[test]
fn provenance_divergence_model_endpoint_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );
    // Same model name + config, but a different backend endpoint.
    patch_manifest_field(
        &shard_b.dir,
        "/manifest/model/backend",
        serde_json::json!("different-backend"),
    );

    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(work.path().join("merged"))
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "merge must reject differing model endpoints"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("endpoint"),
        "error should mention the model endpoint"
    );
}

#[test]
fn provenance_divergence_harness_sha_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );
    // A different harness commit produced shard_b.
    patch_manifest_field(
        &shard_b.dir,
        "/manifest/harness/git_sha",
        serde_json::json!("different-harness-sha-999"),
    );

    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(work.path().join("merged"))
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "merge must reject differing harness revisions"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("harness"),
        "error should mention the harness revision"
    );
}

#[test]
fn legacy_runs_zero_merges_with_modern_single_run() {
    let work = tempfile::tempdir().unwrap();
    // shard_a is a legacy single-run sweep whose rows predate the `runs` field
    // (serde-default 0); shard_b is a modern single-run sweep (runs=1). Both are
    // effectively single-run and must merge without a rerun-count mismatch.
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let path = shard_a.dir.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for inst in results["instances"].as_array_mut().unwrap() {
        inst["runs"] = serde_json::json!(0);
    }
    fs::write(&path, serde_json::to_string_pretty(&results).unwrap()).unwrap();

    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );

    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(work.path().join("merged"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "legacy runs=0 must merge with modern runs=1: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn mixed_rerun_counts_exit_nonzero() {
    let work = tempfile::tempdir().unwrap();
    // shard_a is single-run (runs=1); shard_b is pass@k with runs=2.
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b =
        ShardFixture::create_multi_run(work.path(), "shard_b", &[("inst-002", "submitted")], 2);

    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(work.path().join("merged"))
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "merge must reject shards with different --rerun counts"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("rerun"),
        "error should mention the rerun-count mismatch"
    );
}

// ── evaluation.json handling ──────────────────────────────────────────────────

#[test]
fn eval_in_all_shards_is_merged_and_audits() {
    let work = tempfile::tempdir().unwrap();

    // Both shards carry evaluation.json → the merged sweep is fully evaluated.
    let shard_a = ShardFixture::create_with_eval(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
        &[("inst-001", true)],
    );
    let shard_b = ShardFixture::create_with_eval(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.12)],
        &[("inst-002", false)],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "merge failed: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A merged evaluation.json with an entry for every union instance.
    let eval: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("evaluation.json")).unwrap()).unwrap();
    let eval_instances = eval["instances"].as_array().unwrap();
    assert_eq!(eval_instances.len(), 2, "both shards' eval entries present");

    // The fully-evaluated merged sweep must still audit cleanly.
    let audit_out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        audit_out.status.success(),
        "audit failed on merged evaluated sweep!\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&audit_out.stdout),
        String::from_utf8_lossy(&audit_out.stderr)
    );
}

#[test]
fn eval_in_some_shards_is_omitted_and_audits() {
    let work = tempfile::tempdir().unwrap();

    // Only shard_a is evaluated. A partial modern evaluation.json would make
    // shard_b's trajectories orphans under `bench audit`, so merge must omit it.
    let shard_a = ShardFixture::create_with_eval(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
        &[("inst-001", true)],
    );
    let shard_b =
        ShardFixture::create(work.path(), "shard_b", &[InstanceSpec::errored("inst-002")]);

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "merge failed: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    // evaluation.json is omitted because not every shard was evaluated.
    assert!(
        !output.join("evaluation.json").exists(),
        "merged evaluation.json must be omitted when only some shards were evaluated"
    );

    // The (unevaluated) merged sweep still audits cleanly.
    let audit_out = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        audit_out.status.success(),
        "audit failed on merged unevaluated sweep!\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&audit_out.stdout),
        String::from_utf8_lossy(&audit_out.stderr)
    );
}

// ── AC7: error cases exit non-zero ────────────────────────────────────────────

#[test]
fn single_shard_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let output = work.path().join("merged");

    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    // clap requires at least one --shard (required = true) but we need to error
    // on exactly one shard (merge requires 2+)
    // Note: clap won't prevent 1 shard; our validation must catch it
    assert!(
        !out.status.success(),
        "bench merge with a single shard should fail"
    );
}

#[test]
fn missing_results_json_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_bad = work.path().join("shard_bad");
    fs::create_dir_all(&shard_bad).unwrap();
    // No results.json in shard_bad

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_bad)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "bench merge should fail when shard is missing results.json"
    );
}

#[test]
fn incomplete_sweep_status_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    // Mutate shard_a to have sweep_status = "running"
    let results_path = shard_a.dir.join("results.json");
    let mut results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&results_path).unwrap()).unwrap();
    results["sweep_status"] = serde_json::json!("running");
    fs::write(
        &results_path,
        serde_json::to_string_pretty(&results).unwrap(),
    )
    .unwrap();

    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "bench merge should fail when a shard is not completed"
    );
}

#[test]
fn output_dir_not_empty_without_force_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );

    let output = work.path().join("merged");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("something.txt"), "existing content").unwrap();

    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "bench merge to non-empty output without --force should fail"
    );
}

// ── AC4: provenance in merged manifest ────────────────────────────────────────

#[test]
fn merged_manifest_contains_shard_provenance() {
    let work = tempfile::tempdir().unwrap();

    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "merge failed: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("results.json")).unwrap()).unwrap();

    // AC4: provenance preserved; merged_from records per-shard info
    let manifest = &results["manifest"];
    assert!(
        manifest.get("merged_from").is_some(),
        "merged manifest should contain 'merged_from' field:\n{manifest}"
    );
    let merged_from = manifest["merged_from"].as_array().unwrap();
    assert_eq!(
        merged_from.len(),
        2,
        "should have 2 shard entries in merged_from"
    );

    // Each shard entry has label, dir, instance_count
    for entry in merged_from {
        assert!(
            entry.get("label").is_some(),
            "shard entry missing 'label':\n{entry}"
        );
        assert!(
            entry.get("dir").is_some(),
            "shard entry missing 'dir':\n{entry}"
        );
        assert!(
            entry.get("instance_count").is_some(),
            "shard entry missing 'instance_count':\n{entry}"
        );
    }

    // source should be "merge"
    assert_eq!(
        manifest["source"], "merge",
        "manifest source should be 'merge':\n{manifest}"
    );
}

// ── three-shard merge ─────────────────────────────────────────────────────────

#[test]
fn three_shard_merge_aggregates_correctly() {
    let work = tempfile::tempdir().unwrap();

    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[
            InstanceSpec::submitted("inst-001", 0.10),
            InstanceSpec::errored("inst-002"),
        ],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-003", 0.20)],
    );
    let shard_c = ShardFixture::create(
        work.path(),
        "shard_c",
        &[
            InstanceSpec::errored("inst-004"),
            InstanceSpec::errored("inst-005"),
        ],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge", "--format", "json"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--shard")
        .arg(&shard_c.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "3-shard merge failed!\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["total_instances"], 5);
    assert_eq!(report["shards"].as_array().unwrap().len(), 3);

    let merged: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("results.json")).unwrap()).unwrap();
    assert_eq!(merged["total"], 5);
    assert_eq!(merged["submitted"], 2);
    assert_eq!(merged["errored"], 3);
}

// ── pass@k / predictions / safety ─────────────────────────────────────────────

#[test]
fn pass_at_k_multi_run_merge_audits() {
    let work = tempfile::tempdir().unwrap();

    // Two disjoint pass@k shards, each instance run twice.
    let shard_a = ShardFixture::create_multi_run(
        work.path(),
        "shard_a",
        &[("inst-001", "submitted"), ("inst-002", "error")],
        2,
    );
    let shard_b =
        ShardFixture::create_multi_run(work.path(), "shard_b", &[("inst-003", "submitted")], 2);

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "multi-run merge failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Per-run-slot counts: 2 submitted instances × 2 runs = 4 submitted slots;
    // 1 errored instance × 2 runs = 2 errored slots.
    let merged: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("results.json")).unwrap()).unwrap();
    assert_eq!(merged["submitted"], 4, "per-slot submitted count");
    assert_eq!(merged["errored"], 2, "per-slot errored count");
    // with_patch is per slot too: 2 submitted instances × 2 runs, each with a
    // patch → 4 (a per-task count would report only 2).
    assert_eq!(merged["with_patch"], 4, "per-slot with_patch count");

    // The merged pass@k sweep must reconcile under `bench audit`.
    let audit = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        audit.status.success(),
        "audit failed on merged pass@k sweep!\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );
}

#[test]
fn merge_writes_all_preds_for_evaluate() {
    let work = tempfile::tempdir().unwrap();

    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[
            InstanceSpec::submitted("inst-002", 0.12),
            InstanceSpec::errored("inst-003"),
        ],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(out.status.success(), "merge failed");

    // `bench evaluate` reads <sweep>/all_preds.jsonl; it must exist with one row
    // per submitted instance across both shards (the errored one is excluded).
    let preds = fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    let ids: Vec<String> = preds
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["instance_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(ids.len(), 2, "two submitted instances");
    assert!(ids.contains(&"inst-001".to_string()));
    assert!(ids.contains(&"inst-002".to_string()));
    assert!(!ids.contains(&"inst-003".to_string()), "errored excluded");

    // Metadata is rewritten for the merged aggregate.
    assert!(output.join("all_preds.metadata.json").exists());
}

#[test]
fn output_overlapping_shard_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );

    // --output IS shard_a; with --force this would delete the shard before copy.
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&shard_a.dir)
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "merge must reject an --output that overlaps an input shard"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("overlaps input shard"),
        "stderr should explain the overlap: {stderr}"
    );
    // The shard's results.json must still be intact (not deleted).
    assert!(shard_a.dir.join("results.json").exists());
}

#[test]
fn merged_filter_spec_lists_union_ids() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(out.status.success(), "merge failed");

    let merged: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("results.json")).unwrap()).unwrap();
    let ids = merged["filter_spec"]["instance_ids"].as_array().unwrap();
    let ids: Vec<&str> = ids.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(
        ids,
        vec!["inst-001", "inst-002"],
        "filter_spec must list the merged union, not just shard 0"
    );
}

#[test]
fn budget_halted_rows_preserved_and_audits() {
    let work = tempfile::tempdir().unwrap();

    // shard_a has a budget-halted task with no trajectory on disk.
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[
            InstanceSpec::submitted("inst-001", 0.10),
            InstanceSpec::budget_halted("inst-002"),
        ],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-003", 0.10)],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(out.status.success(), "merge failed");

    // The budget-halted row (no trajectory) must survive the per-slot recount.
    let merged: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("results.json")).unwrap()).unwrap();
    assert_eq!(
        merged["budget_halted"], 1,
        "budget_halted must be preserved"
    );

    let audit = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        audit.status.success(),
        "audit failed on merged budget-capped sweep!\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );
}

#[test]
fn legacy_eval_emits_entry_for_every_owned_instance() {
    let work = tempfile::tempdir().unwrap();

    // Both shards carry a legacy sb-cli evaluation.json. shard_a has an errored
    // instance that is absent from resolved_ids/submitted_ids.
    let shard_a = ShardFixture::create_with_legacy_eval(
        work.path(),
        "shard_a",
        &[
            InstanceSpec::submitted("inst-001", 0.10),
            InstanceSpec::errored("inst-002"),
        ],
        &["inst-001"],
    );
    let shard_b = ShardFixture::create_with_legacy_eval(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-003", 0.10)],
        &[],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(out.status.success(), "merge failed");

    // The merged modern evaluation.json must contain an entry for every owned
    // instance — including the errored one absent from the legacy arrays — or
    // bench audit reports audit:orphan:evaluation.
    let eval: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("evaluation.json")).unwrap()).unwrap();
    let ids: Vec<&str> = eval["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["instance_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"inst-001"));
    assert!(
        ids.contains(&"inst-002"),
        "errored instance must have an entry"
    );
    assert!(ids.contains(&"inst-003"));

    let audit = Command::new(binary_path())
        .args(["bench", "audit", "--sweep"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        audit.status.success(),
        "audit failed on merged legacy-eval sweep!\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );
}

#[test]
fn merged_manifest_keeps_source_dataset_count() {
    let work = tempfile::tempdir().unwrap();
    let shard_a = ShardFixture::create(
        work.path(),
        "shard_a",
        &[InstanceSpec::submitted("inst-001", 0.10)],
    );
    let shard_b = ShardFixture::create(
        work.path(),
        "shard_b",
        &[InstanceSpec::submitted("inst-002", 0.10)],
    );

    let output = work.path().join("merged");
    let out = Command::new(binary_path())
        .args(["bench", "merge"])
        .arg("--shard")
        .arg(&shard_a.dir)
        .arg("--shard")
        .arg(&shard_b.dir)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(out.status.success(), "merge failed");

    // Each shard fixture records dataset.instance_count == 1 (its own selection).
    // The merged manifest must keep that source cardinality, not overwrite it
    // with the union size (2); only the subset counts describe the selection.
    let merged: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("results.json")).unwrap()).unwrap();
    let dataset = &merged["manifest"]["dataset"];
    assert_eq!(
        dataset["instance_count"], 1,
        "source dataset count preserved"
    );
    assert_eq!(
        dataset["selected_row_count"], 2,
        "subset count is the union"
    );
}
