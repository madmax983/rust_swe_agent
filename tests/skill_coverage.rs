//! `bench skill-coverage`: per-sweep agent skill activation by outcome.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

mod support;
use support::binary_path;

// ── test setup helpers ────────────────────────────────────────────────────────

fn write_json_file<T: serde::Serialize>(path: &Path, val: &T) {
    let file = File::create(path).unwrap();
    serde_json::to_writer_pretty(file, val).unwrap();
}

fn create_mock_skill_file(dir: &Path, name: &str, description: &str) -> PathBuf {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    let skill_file = skill_dir.join("SKILL.md");
    let mut file = File::create(&skill_file).unwrap();
    writeln!(
        file,
        "---\nname: {name}\ndescription: {description}\n---\nBody of {name}"
    )
    .unwrap();
    skill_file
}

fn run_skill_coverage(sweep: &Path, extra_args: &[&str]) -> std::process::Output {
    Command::new(binary_path())
        .args(["--log", "error", "bench", "skill-coverage", "--sweep"])
        .arg(sweep)
        .args(extra_args)
        .output()
        .unwrap()
}

// ── test cases ────────────────────────────────────────────────────────────────

#[test]
fn cli_exits_2_on_usage_error() {
    // Missing required --sweep
    let out = Command::new(binary_path())
        .args(["bench", "skill-coverage"])
        .output()
        .unwrap();
    assert_eq!(out.status.code().unwrap(), 2);
}

#[test]
fn cli_exits_0_and_warns_when_skills_disabled() {
    let sweep = tempfile::tempdir().unwrap();

    // Create a results.json with skills disabled
    let toml_config = r#"
[skills]
enabled = false
paths = []
"#;
    let results = serde_json::json!({
        "total": 0,
        "sweep_status": "completed",
        "submitted": 0,
        "skipped": 0,
        "errored": 0,
        "instances": [],
        "filter_spec": {
            "original_count": 0,
            "selected_count": 0
        },
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 0 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": toml_config,
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    let out = run_skill_coverage(sweep.path(), &[]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("empty") || stdout.contains("disabled") || stdout.contains("0 skills"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn cli_calculates_skill_activation_rates_and_deltas() {
    let sweep = tempfile::tempdir().unwrap();
    let skills_dir = sweep.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();

    // Create eligible/configured skills
    create_mock_skill_file(&skills_dir, "skill_a", "Skill A description");
    create_mock_skill_file(&skills_dir, "skill_b", "Skill B description");
    create_mock_skill_file(&skills_dir, "skill_c", "Skill C description");

    let toml_config = format!(
        r#"
[skills]
enabled = true
paths = ["{}"]
"#,
        skills_dir.display().to_string().replace('\\', "/")
    );

    let results = serde_json::json!({
        "total": 3,
        "sweep_status": "completed",
        "submitted": 3,
        "skipped": 0,
        "errored": 0,
        "instances": [
            { "instance_id": "inst_1", "exit_reason": "submitted", "resolved_count": 1 },
            { "instance_id": "inst_2", "exit_reason": "submitted", "resolved_count": 0 },
            { "instance_id": "inst_3", "exit_reason": "submitted", "resolved_count": 0 }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 3 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": toml_config,
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    // Create evaluation.json
    let evaluation = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved": true, "eval_exit_reason": "resolved" },
            { "instance_id": "inst_2", "resolved": false, "eval_exit_reason": "unresolved" },
            { "instance_id": "inst_3", "resolved": false, "eval_exit_reason": "unresolved" }
        ]
    });
    write_json_file(&sweep.path().join("evaluation.json"), &evaluation);

    // Create trajectories
    // inst_1 (resolved): active_skills: skill_a (explicit_mention)
    let traj_1 = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": "task 1",
            "model_name": "claude-3-5",
            "outcome": "submitted",
            "active_skills": [
                {
                    "name": "skill_a",
                    "description": "Skill A description",
                    "path": "/mock/path/a",
                    "sha256": "hash_a",
                    "activation_reason": "explicit_mention"
                }
            ]
        },
        "messages": []
    });
    fs::create_dir_all(sweep.path().join("inst_1")).unwrap();
    write_json_file(
        &sweep.path().join("inst_1").join("trajectory.json"),
        &traj_1,
    );

    // inst_2 (unresolved): active_skills: skill_a (auto_match), skill_b (explicit_mention)
    let traj_2 = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": "task 2",
            "model_name": "claude-3-5",
            "outcome": "submitted",
            "active_skills": [
                {
                    "name": "skill_a",
                    "description": "Skill A description",
                    "path": "/mock/path/a",
                    "sha256": "hash_a",
                    "activation_reason": "auto_match"
                },
                {
                    "name": "skill_b",
                    "description": "Skill B description",
                    "path": "/mock/path/b",
                    "sha256": "hash_b",
                    "activation_reason": "explicit_mention"
                }
            ]
        },
        "messages": []
    });
    fs::create_dir_all(sweep.path().join("inst_2")).unwrap();
    write_json_file(
        &sweep.path().join("inst_2").join("trajectory.json"),
        &traj_2,
    );

    // inst_3 (unresolved): no active skills
    let traj_3 = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": "task 3",
            "model_name": "claude-3-5",
            "outcome": "submitted"
        },
        "messages": []
    });
    fs::create_dir_all(sweep.path().join("inst_3")).unwrap();
    write_json_file(
        &sweep.path().join("inst_3").join("trajectory.json"),
        &traj_3,
    );

    // Run skill-coverage format json
    let out = run_skill_coverage(sweep.path(), &["--format", "json"]);
    assert!(
        out.status.success(),
        "Command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    // Verification of fields
    assert_eq!(report["sweep"], sweep.path().display().to_string());
    assert!(report["generated_at"].is_string());
    assert!(report["skill_universe"].is_array());

    let universe = report["skill_universe"].as_array().unwrap();
    assert_eq!(
        universe.len(),
        3,
        "Universe should contain skill_a, skill_b, and skill_c"
    );

    let by_skill = &report["by_skill"];
    assert!(by_skill["skill_a"].is_object());
    assert!(by_skill["skill_b"].is_object());
    assert!(by_skill["skill_c"].is_object());

    // skill_a metrics
    let a = &by_skill["skill_a"];
    assert_eq!(a["total_activations"].as_u64().unwrap(), 2);
    assert_eq!(a["instances_activated"].as_u64().unwrap(), 2);
    assert_eq!(a["activation_rate"].as_f64().unwrap(), 2.0 / 3.0);
    assert_eq!(a["share_of_all_activations"].as_f64().unwrap(), 2.0 / 3.0);
    assert_eq!(a["reasons"]["explicit_mention"].as_u64().unwrap(), 1);
    assert_eq!(a["reasons"]["auto_match"].as_u64().unwrap(), 1);

    // skill_a outcomes
    let a_outcomes = &a["by_outcome"];
    // all outcome
    let a_all = &a_outcomes["all"];
    assert_eq!(a_all["instances_activated"].as_u64().unwrap(), 2);
    assert_eq!(a_all["instances_total"].as_u64().unwrap(), 3);
    assert_eq!(a_all["usage_rate"].as_f64().unwrap(), 2.0 / 3.0);
    assert_eq!(a_all["resolved_rate_when_active"].as_f64().unwrap(), 0.5); // inst_1 is resolved (active), inst_2 is unresolved (active)
    assert_eq!(
        a_all["resolved_rate_when_not_active"].as_f64().unwrap(),
        0.0
    ); // inst_3 is unresolved (not active)
    assert_eq!(a_all["resolved_rate_delta"].as_f64().unwrap(), 0.5);
    assert_eq!(a_all["total_activations"].as_u64().unwrap(), 2);
    assert_eq!(
        a_all["share_of_all_activations"].as_f64().unwrap(),
        2.0 / 3.0
    );
    assert_eq!(a_all["reasons"]["explicit_mention"].as_u64().unwrap(), 1);
    assert_eq!(a_all["reasons"]["auto_match"].as_u64().unwrap(), 1);

    // skill_c metrics (never activated)
    let c = &by_skill["skill_c"];
    assert_eq!(c["total_activations"].as_u64().unwrap(), 0);
    assert_eq!(c["instances_activated"].as_u64().unwrap(), 0);
    assert_eq!(c["activation_rate"].as_f64().unwrap(), 0.0);
    assert_eq!(c["share_of_all_activations"].as_f64().unwrap(), 0.0);

    // check if skill-coverage.json artifact was written
    assert!(sweep.path().join("skill-coverage.json").exists());

    // Verify bucket filter output for JSON
    let out_unresolved = run_skill_coverage(
        sweep.path(),
        &["--format", "json", "--bucket", "unresolved"],
    );
    assert!(out_unresolved.status.success());
    let report_unresolved: serde_json::Value =
        serde_json::from_slice(&out_unresolved.stdout).unwrap();
    let by_skill_unresolved = &report_unresolved["by_skill"];

    let a_unres = &by_skill_unresolved["skill_a"]["by_outcome"]["unresolved"];
    assert_eq!(a_unres["instances_activated"].as_u64().unwrap(), 1);
    assert_eq!(a_unres["instances_total"].as_u64().unwrap(), 2); // inst_2 and inst_3 are unresolved
    assert_eq!(a_unres["usage_rate"].as_f64().unwrap(), 0.5);
    assert_eq!(a_unres["total_activations"].as_u64().unwrap(), 1);
    assert_eq!(a_unres["share_of_all_activations"].as_f64().unwrap(), 0.5); // skill_a: 1, skill_b: 1. Grand total in unresolved: 2.
    assert_eq!(a_unres["reasons"]["auto_match"].as_u64().unwrap(), 1);
    assert_eq!(
        a_unres["reasons"]
            .get("explicit_mention")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        0
    );

    // Verify bucket filter output for rendered text
    let out_text = run_skill_coverage(sweep.path(), &["--bucket", "unresolved"]);
    assert!(out_text.status.success());
    let text_stdout = String::from_utf8(out_text.stdout).unwrap();
    assert!(text_stdout.contains("Bucket filter: unresolved"));
    assert!(text_stdout.contains("skill_a"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn cli_detects_skill_set_drift() {
    let sweep = tempfile::tempdir().unwrap();

    let toml_config = r#"
[skills]
enabled = true
paths = ["/dummy/path/a"]
"#;

    let results = serde_json::json!({
        "total": 2,
        "sweep_status": "completed",
        "submitted": 2,
        "skipped": 0,
        "errored": 0,
        "instances": [
            { "instance_id": "inst_1", "exit_reason": "submitted" },
            { "instance_id": "inst_2", "exit_reason": "submitted" }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 2 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": toml_config,
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    // Trajectory for inst_1 has no override
    let traj_1 = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": "task 1",
            "outcome": "submitted",
            "manifest": {
                "harness_binary_version": "1.0",
                "started_at_utc": "2026-06-06T00:00:00Z",
                "env_kind": "local",
                "config_sha256": "hash",
                "config_redacted": {
                    "skills": {
                        "enabled": true,
                        "paths": ["/dummy/path/a"]
                    }
                },
                "cli_invocation": [],
                "extra_context_present": false,
                "step_limit": 50,
                "model_name": "claude-3",
                "redaction_policy_id": "id",
                "deterministic_mode": false
            }
        },
        "messages": []
    });
    fs::create_dir_all(sweep.path().join("inst_1")).unwrap();
    write_json_file(
        &sweep.path().join("inst_1").join("trajectory.json"),
        &traj_1,
    );

    // Trajectory for inst_2 has overridden paths
    let traj_2 = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": "task 2",
            "outcome": "submitted",
            "manifest": {
                "harness_binary_version": "1.0",
                "started_at_utc": "2026-06-06T00:00:00Z",
                "env_kind": "local",
                "config_sha256": "hash",
                "config_redacted": {
                    "skills": {
                        "enabled": true,
                        "paths": ["/dummy/path/b"] // Different!
                    }
                },
                "cli_invocation": [],
                "extra_context_present": false,
                "step_limit": 50,
                "model_name": "claude-3",
                "redaction_policy_id": "id",
                "deterministic_mode": false
            }
        },
        "messages": []
    });
    fs::create_dir_all(sweep.path().join("inst_2")).unwrap();
    write_json_file(
        &sweep.path().join("inst_2").join("trajectory.json"),
        &traj_2,
    );

    let out = run_skill_coverage(sweep.path(), &["--format", "json"]);
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert!(report["skill_set_drift"].is_object());
    let drift_groups = report["skill_set_drift"]["groups"].as_array().unwrap();
    assert_eq!(drift_groups.len(), 2);
}

#[test]
fn cli_supports_per_instance_activation_reasons() {
    let sweep = tempfile::tempdir().unwrap();

    let results = serde_json::json!({
        "total": 1,
        "sweep_status": "completed",
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "instances": [
            { "instance_id": "inst_1", "exit_reason": "submitted" }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 1 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": "[skills]\nenabled=true\npaths=[]",
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    let traj = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": "task 1",
            "outcome": "submitted",
            "active_skills": [
                {
                    "name": "skill_a",
                    "description": "desc",
                    "path": "/path/a",
                    "sha256": "hash",
                    "activation_reason": "explicit_mention"
                }
            ]
        },
        "messages": []
    });
    fs::create_dir_all(sweep.path().join("inst_1")).unwrap();
    write_json_file(&sweep.path().join("inst_1").join("trajectory.json"), &traj);

    let out = run_skill_coverage(sweep.path(), &["--format", "json", "--per-instance"]);
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let per_instance = report["per_instance"].as_array().unwrap();
    assert_eq!(per_instance.len(), 1);
    let inst = &per_instance[0];
    assert_eq!(inst["instance_id"], "inst_1");
    assert_eq!(inst["active_skills"]["skill_a"], "explicit_mention");
}

#[test]
fn cli_fails_when_instance_has_no_trajectory() {
    let sweep = tempfile::tempdir().unwrap();

    let results = serde_json::json!({
        "total": 1,
        "sweep_status": "completed",
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "instances": [
            { "instance_id": "inst_1", "exit_reason": "submitted" }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 1 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": "[skills]\nenabled=true\npaths=[]",
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    let out = run_skill_coverage(sweep.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no trajectory found for instance inst_1"));
}

#[test]
#[allow(clippy::similar_names)]
fn cli_fails_when_skill_registry_scan_fails() {
    let sweep = tempfile::tempdir().unwrap();
    let skills_dir = sweep.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();

    let skill_a_dir = skills_dir.join("skill_a");
    fs::create_dir_all(&skill_a_dir).unwrap();
    let skill_b_dir = skills_dir.join("skill_b");
    fs::create_dir_all(&skill_b_dir).unwrap();

    fs::write(
        skill_a_dir.join("SKILL.md"),
        "---\nname: duplicate_skill\ndescription: desc\n---\nbody",
    )
    .unwrap();

    fs::write(
        skill_b_dir.join("SKILL.md"),
        "---\nname: duplicate_skill\ndescription: desc\n---\nbody",
    )
    .unwrap();

    let toml_config = format!(
        r#"
[skills]
enabled = true
paths = ["{}"]
"#,
        skills_dir.display().to_string().replace('\\', "/")
    );

    let results = serde_json::json!({
        "total": 1,
        "sweep_status": "completed",
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "instances": [
            { "instance_id": "inst_1", "exit_reason": "submitted" }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 1 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": toml_config,
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    let traj = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": "task 1",
            "outcome": "submitted"
        },
        "messages": []
    });
    fs::create_dir_all(sweep.path().join("inst_1")).unwrap();
    write_json_file(&sweep.path().join("inst_1").join("trajectory.json"), &traj);

    let out = run_skill_coverage(sweep.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("duplicate skill name"));
}

#[test]
fn cli_fails_on_malformed_active_skills_manifest() {
    let sweep = tempfile::tempdir().unwrap();

    let results = serde_json::json!({
        "total": 1,
        "sweep_status": "completed",
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "instances": [
            { "instance_id": "inst_1", "exit_reason": "submitted" }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 1 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": "[skills]\nenabled=true\npaths=[]",
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    // active_skills contains malformed json (a string instead of list of manifests)
    let traj = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": "task 1",
            "outcome": "submitted",
            "active_skills": "not-a-list"
        },
        "messages": []
    });
    fs::create_dir_all(sweep.path().join("inst_1")).unwrap();
    write_json_file(&sweep.path().join("inst_1").join("trajectory.json"), &traj);

    let out = run_skill_coverage(sweep.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("invalid type"));
}

#[test]
fn cli_attributes_retry_activations_to_correct_outcome_bucket() {
    let sweep = tempfile::tempdir().unwrap();
    let results = mock_sweep_results(false);
    write_json_file(&sweep.path().join("results.json"), &results);

    // Mock sb_cli_reports
    let report_dir = sweep.path().join("sb_cli_reports");
    fs::create_dir_all(&report_dir).unwrap();

    let report_1 = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved": false }
        ]
    });
    write_json_file(&report_dir.join("max__test__run-run-1.json"), &report_1);

    let report_2 = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved": true }
        ]
    });
    write_json_file(&report_dir.join("max__test__run-run-2.json"), &report_2);

    let traj_1 = mock_trajectory("task 1", "skill_a", "/path/a");
    let inst_dir = sweep.path().join("inst_1");
    fs::create_dir_all(&inst_dir).unwrap();
    write_json_file(&inst_dir.join("run-1.traj.json"), &traj_1);

    let traj_2 = mock_trajectory("task 1", "skill_b", "/path/b");
    write_json_file(&inst_dir.join("run-2.traj.json"), &traj_2);

    let out = run_skill_coverage(sweep.path(), &["--format", "json"]);
    assert!(
        out.status.success(),
        "Stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let by_skill = &report["by_skill"];

    let a_resolved = &by_skill["skill_a"]["by_outcome"]["resolved"];
    assert_eq!(a_resolved["instances_activated"].as_u64().unwrap(), 0);
    let a_unresolved = &by_skill["skill_a"]["by_outcome"]["unresolved"];
    assert_eq!(a_unresolved["instances_activated"].as_u64().unwrap(), 1);

    let b_resolved = &by_skill["skill_b"]["by_outcome"]["resolved"];
    assert_eq!(b_resolved["instances_activated"].as_u64().unwrap(), 1);
    let b_unresolved = &by_skill["skill_b"]["by_outcome"]["unresolved"];
    assert_eq!(b_unresolved["instances_activated"].as_u64().unwrap(), 0);
}

#[test]
fn cli_attributes_retry_activations_using_heuristic_fallback() {
    let sweep = tempfile::tempdir().unwrap();
    let results = mock_sweep_results(true);
    write_json_file(&sweep.path().join("results.json"), &results);

    let traj_1 = mock_trajectory("task 1", "skill_a", "/path/a");
    let inst_dir = sweep.path().join("inst_1");
    fs::create_dir_all(&inst_dir).unwrap();
    write_json_file(&inst_dir.join("run-1.traj.json"), &traj_1);

    let traj_2 = mock_trajectory("task 1", "skill_b", "/path/b");
    write_json_file(&inst_dir.join("run-2.traj.json"), &traj_2);

    let out = run_skill_coverage(sweep.path(), &["--format", "json"]);
    assert!(
        out.status.success(),
        "Stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let by_skill = &report["by_skill"];

    let a_resolved = &by_skill["skill_a"]["by_outcome"]["resolved"];
    assert_eq!(a_resolved["instances_activated"].as_u64().unwrap(), 0);
    let a_unresolved = &by_skill["skill_a"]["by_outcome"]["unresolved"];
    assert_eq!(a_unresolved["instances_activated"].as_u64().unwrap(), 1);

    let b_resolved = &by_skill["skill_b"]["by_outcome"]["resolved"];
    assert_eq!(b_resolved["instances_activated"].as_u64().unwrap(), 1);
    let b_unresolved = &by_skill["skill_b"]["by_outcome"]["unresolved"];
    assert_eq!(b_unresolved["instances_activated"].as_u64().unwrap(), 0);
}

fn mock_trajectory(task: &str, skill_name: &str, skill_path: &str) -> serde_json::Value {
    serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "artifact_kind": "trajectory",
        "schema_version": { "major": 1, "minor": 10 },
        "info": {
            "task": task,
            "outcome": "submitted",
            "active_skills": [
                {
                    "name": skill_name,
                    "description": "desc",
                    "path": skill_path,
                    "sha256": "hash",
                    "activation_reason": "explicit_mention"
                }
            ]
        },
        "messages": []
    })
}

fn mock_sweep_results(finished: bool) -> serde_json::Value {
    let mut runtime =
        serde_json::json!({ "started_at_utc": "2026-06-06T00:00:00Z", "host_os": "linux" });
    if finished {
        runtime["finished_at_utc"] = serde_json::Value::String("2026-06-06T00:01:00Z".to_owned());
    }
    serde_json::json!({
        "total": 1,
        "sweep_status": "completed",
        "submitted": 1,
        "skipped": 0,
        "errored": 0,
        "instances": [
            {
                "instance_id": "inst_1",
                "exit_reason": "submitted",
                "resolved_count": 1,
                "runs": 2,
                "pass_at_1": false
            }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 1 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": "[skills]\nenabled=true\npaths=[]",
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": runtime,
            "cli": { "argv": [] }
        }
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn cli_attributes_retry_activations_with_stale_reports_and_duplicate_activations() {
    let sweep = tempfile::tempdir().unwrap();

    // results.json with two instances inst_1 and inst_2
    let results = serde_json::json!({
        "total": 2,
        "sweep_status": "completed",
        "submitted": 2,
        "skipped": 0,
        "errored": 0,
        "instances": [
            {
                "instance_id": "inst_1",
                "exit_reason": "submitted",
                "resolved_count": 1,
                "runs": 2,
                "pass_at_1": false
            },
            {
                "instance_id": "inst_2",
                "exit_reason": "submitted",
                "resolved_count": 1,
                "runs": 2,
                "pass_at_1": false
            }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 2 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": "[skills]\nenabled=true\npaths=[\"/dummy/path/a\"]",
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "finished_at_utc": "2026-06-06T00:01:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    // evaluation.json with provenance
    let evaluation = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved": true, "eval_exit_reason": "resolved" },
            { "instance_id": "inst_2", "resolved": true, "eval_exit_reason": "resolved" }
        ],
        "provenance": {
            "backend": "sb-cli",
            "run_id": "current-run",
            "dataset_subset": "swe-bench",
            "dataset_split": "test"
        }
    });
    write_json_file(&sweep.path().join("evaluation.json"), &evaluation);

    // sb_cli_reports directory
    let report_dir = sweep.path().join("sb_cli_reports");
    fs::create_dir_all(&report_dir).unwrap();

    // current run reports
    // report_1 uses top-level Array format to test array-shaped evaluator reports
    let report_1 = serde_json::json!([
        { "instance_id": "inst_1", "resolved": false },
        { "instance_id": "inst_2", "resolved": false }
    ]);
    write_json_file(
        &report_dir.join("swe-bench__test__current-run-run-1.json"),
        &report_1,
    );

    let report_2 = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved": true },
            { "instance_id": "inst_2", "resolved": true }
        ]
    });
    write_json_file(
        &report_dir.join("swe-bench__test__current-run-run-2.json"),
        &report_2,
    );

    // stale run report (run-2 unresolved) - if this was read, it would overwrite run-2's outcome or corrupt it
    let stale_report = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved": false },
            { "instance_id": "inst_2", "resolved": false }
        ]
    });
    write_json_file(
        &report_dir.join("swe-bench__test__stale-run-run-2.json"),
        &stale_report,
    );

    // trajectories for inst_1 (run-1 and run-2)
    // both have skill_a activated, so it is activated on both runs of the same parent instance!
    let traj_1 = mock_trajectory("task 1", "skill_a", "/path/a");
    let inst_dir_1 = sweep.path().join("inst_1");
    fs::create_dir_all(&inst_dir_1).unwrap();
    write_json_file(&inst_dir_1.join("run-1.traj.json"), &traj_1);
    write_json_file(&inst_dir_1.join("run-2.traj.json"), &traj_1);

    // trajectories for inst_2 (run-1 and run-2)
    // inst_2 has overridden config to trigger drift detection, but same skill_a is active
    let mut traj_2 = mock_trajectory("task 2", "skill_a", "/path/a");
    traj_2["info"]["manifest"] = serde_json::json!({
        "harness_binary_version": "1.0",
        "started_at_utc": "2026-06-06T00:00:00Z",
        "env_kind": "local",
        "config_sha256": "hash",
        "config_redacted": {
            "skills": {
                "enabled": true,
                "paths": ["/dummy/path/b"] // Different configuration overlay!
            }
        },
        "cli_invocation": [],
        "extra_context_present": false,
        "step_limit": 50,
        "model_name": "claude-3",
        "redaction_policy_id": "id",
        "deterministic_mode": false
    });
    let inst_dir_2 = sweep.path().join("inst_2");
    fs::create_dir_all(&inst_dir_2).unwrap();
    write_json_file(&inst_dir_2.join("run-1.traj.json"), &traj_2);
    write_json_file(&inst_dir_2.join("run-2.traj.json"), &traj_2);

    let out = run_skill_coverage(sweep.path(), &["--format", "json"]);
    assert!(
        out.status.success(),
        "Stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let by_skill = &report["by_skill"];

    // skill_a instances_activated should be 2 (one for inst_1, one for inst_2)
    let skill_a_metrics = &by_skill["skill_a"];
    assert_eq!(skill_a_metrics["instances_activated"].as_u64().unwrap(), 2);

    // resolved bucket metrics:
    // inst_1 and inst_2 both have run-2 resolved, so resolved bucket instances_activated should be 2
    let a_resolved = &skill_a_metrics["by_outcome"]["resolved"];
    assert_eq!(a_resolved["instances_activated"].as_u64().unwrap(), 2);
    // and resolved rate when active in resolved bucket should be 1.0 (since they all resolved in this bucket)
    assert_eq!(
        a_resolved["resolved_rate_when_active"].as_f64().unwrap(),
        1.0
    );

    // unresolved bucket metrics:
    // inst_1 and inst_2 both have run-1 unresolved, so unresolved bucket instances_activated should be 2
    let a_unresolved = &skill_a_metrics["by_outcome"]["unresolved"];
    assert_eq!(a_unresolved["instances_activated"].as_u64().unwrap(), 2);
    // but resolved rate when active in unresolved bucket should be 0.0 (since they did NOT resolve in this bucket)
    assert_eq!(
        a_unresolved["resolved_rate_when_active"].as_f64().unwrap(),
        0.0
    );

    // Verify drift groups
    let drift = &report["skill_set_drift"];
    let groups = drift["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    // Each group should have instance_count exactly equal to 1 (distinct parent instances), NOT 2 (runs)
    assert_eq!(groups[0]["instance_count"].as_u64().unwrap(), 1);
    assert_eq!(groups[1]["instance_count"].as_u64().unwrap(), 1);
}

#[test]
fn cli_fails_when_sb_cli_report_fails_to_parse() {
    let sweep = tempfile::tempdir().unwrap();

    // results.json
    let results = mock_sweep_results(true);
    write_json_file(&sweep.path().join("results.json"), &results);

    // evaluation.json with provenance matching our report
    let evaluation = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved": true, "eval_exit_reason": "resolved" }
        ],
        "provenance": {
            "backend": "sb-cli",
            "run_id": "current-run",
            "dataset_subset": "swe-bench",
            "dataset_split": "test"
        }
    });
    write_json_file(&sweep.path().join("evaluation.json"), &evaluation);

    // sb_cli_reports directory
    let report_dir = sweep.path().join("sb_cli_reports");
    fs::create_dir_all(&report_dir).unwrap();

    // current run report is malformed (invalid JSON syntax)
    let malformed_report_path = report_dir.join("swe-bench__test__current-run-run-1.json");
    let mut file = File::create(&malformed_report_path).unwrap();
    writeln!(file, "{{ malformed json").unwrap();

    // trajectory
    let traj_1 = mock_trajectory("task 1", "skill_a", "/path/a");
    let inst_dir = sweep.path().join("inst_1");
    fs::create_dir_all(&inst_dir).unwrap();
    write_json_file(&inst_dir.join("run-1.traj.json"), &traj_1);

    // Running the command should fail with exit code 1 or similar
    let out = run_skill_coverage(sweep.path(), &["--format", "json"]);
    assert!(!out.status.success());
}

#[test]
fn cli_handles_mixed_trajectories_double_counting_and_errored_fallbacks() {
    let sweep = tempfile::tempdir().unwrap();

    // results.json with one legacy inst_1 (outcome errored) and one retry inst_2 (2 runs, pass_at_1=true, resolved_count=1)
    let results = serde_json::json!({
        "total": 2,
        "sweep_status": "completed",
        "submitted": 1,
        "skipped": 0,
        "errored": 1,
        "instances": [
            {
                "instance_id": "inst_1",
                "exit_reason": "errored",
                "outcome": "error",
                "resolved_count": 0,
                "runs": 1,
                "pass_at_1": false
            },
            {
                "instance_id": "inst_2",
                "exit_reason": "submitted",
                "resolved_count": 1,
                "runs": 2,
                "pass_at_1": true
            }
        ],
        "filter_spec": {},
        "manifest": {
            "harness": { "name": "max", "version": "1.0", "git_resolution": "clean" },
            "dataset": { "path": "x", "sha256": "x", "instance_count": 2 },
            "prompt_template": { "source": "x", "sha256": "x" },
            "config": {
                "resolved": "[skills]\nenabled=true\npaths=[]",
                "overlay_paths": []
            },
            "model": { "name": "claude-3-5", "backend": "anthropic" },
            "runtime": { "started_at_utc": "2026-06-06T00:00:00Z", "finished_at_utc": "2026-06-06T00:01:00Z", "host_os": "linux" },
            "cli": { "argv": [] }
        }
    });
    write_json_file(&sweep.path().join("results.json"), &results);

    // inst_1 is legacy/errored, trajectory has no outcome field to verify fallback to results.json row outcome: "error"
    let mut traj_1 = mock_trajectory("task 1", "skill_a", "/path/a");
    // remove outcome
    traj_1["info"].as_object_mut().unwrap().remove("outcome");
    let inst_dir_1 = sweep.path().join("inst_1");
    fs::create_dir_all(&inst_dir_1).unwrap();
    write_json_file(&inst_dir_1.join("trajectory.json"), &traj_1);

    // inst_2 has legacy trajectory.json (representing run 1) and run-2.traj.json (representing run 2)
    // both have skill_a activated. This validates that we resolve both when mixed together in the directory!
    let traj_2_run_1 = mock_trajectory("task 2", "skill_a", "/path/a");
    let traj_2_run_2 = mock_trajectory("task 2", "skill_a", "/path/a");
    let inst_dir_2 = sweep.path().join("inst_2");
    fs::create_dir_all(&inst_dir_2).unwrap();
    write_json_file(&inst_dir_2.join("trajectory.json"), &traj_2_run_1);
    write_json_file(&inst_dir_2.join("run-2.traj.json"), &traj_2_run_2);

    // We do NOT write sb_cli_reports JSON, so skill-coverage falls back to the heuristic
    // For inst_2 (pass_at_1=true, resolved_count=1, runs=2):
    // run 1 should be Resolved (because pass_at_1=true)
    // run 2 should NOT be Resolved (double-counting prevention check!)

    let out = run_skill_coverage(sweep.path(), &["--format", "json"]);
    assert!(
        out.status.success(),
        "Stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let by_skill = &report["by_skill"];
    let skill_a_metrics = &by_skill["skill_a"];

    // inst_1 outcome "error" fallback test
    // skill_a should be activated in errored bucket
    let a_errored = &skill_a_metrics["by_outcome"]["errored"];
    assert_eq!(a_errored["instances_activated"].as_u64().unwrap(), 1);

    // inst_2 double-counting fallback test
    // run-2 should NOT be resolved, so resolved rate when active in unresolved bucket should be 0.0
    let a_unresolved = &skill_a_metrics["by_outcome"]["unresolved"];
    assert_eq!(a_unresolved["instances_activated"].as_u64().unwrap(), 1);
    assert_eq!(
        a_unresolved["resolved_rate_when_active"].as_f64().unwrap(),
        0.0
    );
}

#[test]
fn cli_handles_missing_resolved_in_evaluator_report() {
    let sweep = tempfile::tempdir().unwrap();

    // results.json
    let results = mock_sweep_results(true);
    write_json_file(&sweep.path().join("results.json"), &results);

    // evaluation.json with provenance matching our report
    let evaluation = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved": false, "eval_exit_reason": "unresolved" }
        ],
        "provenance": {
            "backend": "sb-cli",
            "run_id": "current-run",
            "dataset_subset": "swe-bench",
            "dataset_split": "test"
        }
    });
    write_json_file(&sweep.path().join("evaluation.json"), &evaluation);

    // sb_cli_reports directory
    let report_dir = sweep.path().join("sb_cli_reports");
    fs::create_dir_all(&report_dir).unwrap();

    // current run report: row lacks resolved boolean entirely, but has instance_id
    let report = serde_json::json!([
        { "instance_id": "inst_1", "eval_exit_reason": "unresolved" }
    ]);
    write_json_file(
        &report_dir.join("swe-bench__test__current-run-run-1.json"),
        &report,
    );

    // trajectory
    let traj_1 = mock_trajectory("task 1", "skill_a", "/path/a");
    let inst_dir = sweep.path().join("inst_1");
    fs::create_dir_all(&inst_dir).unwrap();
    write_json_file(&inst_dir.join("run-1.traj.json"), &traj_1);

    // Running the command should succeed (missing resolved is treated as false)
    let out = run_skill_coverage(sweep.path(), &["--format", "json"]);
    assert!(
        out.status.success(),
        "Stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let by_skill = &report["by_skill"];
    let skill_a_metrics = &by_skill["skill_a"];

    // unresolved bucket metrics:
    // since resolved was omitted (treated as false), the run-1 outcome bucket is Unresolved.
    let a_unresolved = &skill_a_metrics["by_outcome"]["unresolved"];
    assert_eq!(a_unresolved["instances_activated"].as_u64().unwrap(), 1);
}
