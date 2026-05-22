#![allow(clippy::unwrap_used)]
#![allow(clippy::too_many_lines)]

use clap::CommandFactory as _;
use maxwells_daemon::cli::Cli;

#[test]
fn bench_help_documents_diff_config_subcommand_and_options() {
    let mut root = Cli::command();
    let bench = root.find_subcommand_mut("bench").unwrap();
    let diff_config = bench.find_subcommand_mut("diff-config").unwrap();
    let help = diff_config.render_long_help().to_string();

    assert!(help.contains("--baseline"));
    assert!(help.contains("--candidate"));
    assert!(help.contains("--format"));
    assert!(help.contains("--fail-on-change"));
    assert!(help.contains("--ignore"));
}

mod support;

#[test]
fn test_missing_results_json_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    // candidate lacks results.json, baseline lacks results.json
    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("baseline") || stderr.contains("candidate"));
    assert!(stderr.contains("results.json"));
}

#[test]
fn test_missing_manifest_block_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    // Create a results.json without manifest on baseline
    let payload = serde_json::json!({
        "total": 1,
        "instances": []
    });
    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing 'manifest' block"));
}

#[test]
fn test_identical_manifests_text_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let manifest = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        },
        "dataset": {
            "path": "dataset.jsonl",
            "sha256": "dataset_hash",
            "instance_count": 100,
            "filter_spec": {
                "original_count": 100,
                "selected_count": 10
            }
        },
        "prompt_template": {
            "source": "inline",
            "path": null,
            "sha256": "prompt_hash"
        },
        "model": {
            "name": "claude-3-5-sonnet",
            "backend": "litellm",
            "backend_version": "0.1.0",
            "base_url": "https://api.anthropic.com"
        },
        "sampling": {
            "temperature": 0.5,
            "max_tokens": 1000
        },
        "tools": [
            {
                "name": "bash",
                "version": "1.0"
            }
        ],
        "hooks": {
            "pre_tool_use": [
                {
                    "name": "guard",
                    "command": "test"
                }
            ]
        },
        "limits": {
            "step_limit": 50,
            "per_task_budget_usd": 10.0,
            "task_timeout_secs": 3600,
            "sweep_cost_limit_usd": 100.0
        },
        "config": {
            "resolved": "step_limit = 50\nper_task_budget_usd = 10.0\n"
        },
        "cli": {
            "argv": ["cargo", "run", "--", "--baseline", "dir"]
        }
    });

    let payload = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("manifests identical"));
}

#[test]
fn test_identical_manifests_json_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let manifest = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        },
        "dataset": {
            "path": "dataset.jsonl",
            "sha256": "dataset_hash",
            "instance_count": 100,
            "filter_spec": {
                "original_count": 100,
                "selected_count": 10
            }
        },
        "prompt_template": {
            "source": "inline",
            "path": null,
            "sha256": "prompt_hash"
        },
        "model": {
            "name": "claude-3-5-sonnet",
            "backend": "litellm",
            "backend_version": "0.1.0",
            "base_url": "https://api.anthropic.com"
        },
        "sampling": {
            "temperature": 0.5,
            "max_tokens": 1000
        },
        "tools": [
            {
                "name": "bash",
                "version": "1.0"
            }
        ],
        "hooks": {
            "pre_tool_use": [
                {
                    "name": "guard",
                    "command": "test"
                }
            ]
        },
        "limits": {
            "step_limit": 50,
            "per_task_budget_usd": 10.0,
            "task_timeout_secs": 3600,
            "sweep_cost_limit_usd": 100.0
        },
        "config": {
            "resolved": "step_limit = 50\nper_task_budget_usd = 10.0\n"
        },
        "cli": {
            "argv": ["cargo", "run", "--", "--baseline", "dir"]
        }
    });

    let payload = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["summary"]["changed_field_count"].as_u64(), Some(0));
    assert!(parsed["changed_fields"].as_array().unwrap().is_empty());
}

#[test]
fn test_manifest_differences_and_priority_headlines() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let manifest_base = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        },
        "dataset": {
            "path": "dataset.jsonl",
            "sha256": "dataset_hash",
            "instance_count": 100,
            "filter_spec": {
                "original_count": 100,
                "selected_count": 10
            }
        },
        "prompt_template": {
            "source": "inline",
            "path": null,
            "sha256": "prompt_hash"
        },
        "model": {
            "name": "claude-3-5-sonnet",
            "backend": "litellm",
            "backend_version": "0.1.0",
            "base_url": "https://api.anthropic.com"
        },
        "sampling": {
            "temperature": 0.5,
            "max_tokens": 1000
        },
        "tools": [
            {
                "name": "bash",
                "version": "1.0"
            }
        ],
        "hooks": {
            "pre_tool_use": [
                {
                    "name": "guard",
                    "command": "test"
                }
            ]
        },
        "limits": {
            "step_limit": 50,
            "per_task_budget_usd": 10.0,
            "task_timeout_secs": 3600,
            "sweep_cost_limit_usd": 100.0
        },
        "config": {
            "resolved": "step_limit = 50\nper_task_budget_usd = 10.0\n"
        },
        "cli": {
            "argv": ["cargo", "run", "--", "--baseline", "dir"]
        }
    });

    // Candidate has changed prompt_template.sha256 (highest priority) and model.name
    let mut manifest_cand = manifest_base.clone();
    manifest_cand["prompt_template"]["sha256"] = serde_json::json!("prompt_hash_new");
    manifest_cand["model"]["name"] = serde_json::json!("gpt-4o");

    let payload_base = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_base
    });
    let payload_cand = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_cand
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_cand).unwrap(),
    )
    .unwrap();

    // 1. Text mode check: prompt_template.sha256 priority headline
    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("HEADLINE: prompt_template.sha256 changed"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("[prompt_template]"), "stdout: {stdout}");
    assert!(
        stdout.contains("  .sha256: \"prompt_hash\" → \"prompt_hash_new\""),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("[model]"), "stdout: {stdout}");
    assert!(
        stdout.contains("  .name: \"claude-3-5-sonnet\" → \"gpt-4o\""),
        "stdout: {stdout}"
    );
    // harness is unchanged, should show summary line
    assert!(stdout.contains("[harness] identical"), "stdout: {stdout}");

    // 2. Candidate has ONLY model.name changed
    let mut manifest_cand2 = manifest_base;
    manifest_cand2["model"]["name"] = serde_json::json!("gpt-4o");
    let payload_cand2 = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_cand2
    });
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_cand2).unwrap(),
    )
    .unwrap();

    let out2 = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains("HEADLINE: model.name changed"),
        "stdout2: {stdout2}"
    );
    assert!(
        !stdout2.contains("HEADLINE: prompt_template.sha256 changed"),
        "stdout2: {stdout2}"
    );
}

#[test]
fn test_redaction_argv_safety() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let manifest_base = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        },
        "dataset": {
            "path": "dataset.jsonl",
            "sha256": "dataset_hash",
            "instance_count": 100
        },
        "prompt_template": {
            "source": "inline",
            "path": null,
            "sha256": "prompt_hash"
        },
        "model": {
            "name": "claude-3-5-sonnet",
            "backend": "litellm",
            "backend_version": "0.1.0",
            "base_url": "https://api.anthropic.com"
        },
        "cli": {
            "argv": ["cargo", "run", "--", "--anthropic-api-key", "<redacted>"]
        }
    });

    // 1. Both are redacted -> identical manifests
    let payload_base = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_base
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout.contains("manifests identical"));

    // 2. Only baseline is redacted -> differ
    let mut manifest_cand = manifest_base;
    manifest_cand["cli"]["argv"] =
        serde_json::json!(["cargo", "run", "--", "--anthropic-api-key", "secret123"]);
    let payload_cand = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_cand
    });

    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_cand).unwrap(),
    )
    .unwrap();

    let out2 = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(out2.status.code() == Some(0) || out2.status.code() == Some(3)); // we will implement --fail-on-change later, default is exit 0 for diffs unless --fail-on-change
    assert!(stdout2.contains("[cli.argv]"), "stdout2: {stdout2}");
    assert!(
        stdout2.contains("  [4]: \"<redacted>\" → \"secret123\""),
        "stdout2: {stdout2}"
    );
}

#[test]
fn test_ignore_and_fail_on_change() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let manifest_base = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        },
        "dataset": {
            "path": "dataset.jsonl",
            "sha256": "dataset_hash",
            "instance_count": 100
        },
        "prompt_template": {
            "source": "inline",
            "path": null,
            "sha256": "prompt_hash"
        },
        "model": {
            "name": "claude-3-5-sonnet",
            "backend": "litellm",
            "backend_version": "0.1.0",
            "base_url": "https://api.anthropic.com"
        }
    });

    let mut manifest_cand = manifest_base.clone();
    manifest_cand["harness"]["git_sha"] = serde_json::json!("git_hash_new");
    manifest_cand["model"]["name"] = serde_json::json!("gpt-4o");

    let payload_base = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_base
    });
    let payload_cand = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_cand
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_cand).unwrap(),
    )
    .unwrap();

    // 1. Without ignore, fail-on-change exits 3
    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .arg("--fail-on-change")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("preflight_failure"), "stderr: {stderr}");

    // 2. With ignore, fail-on-change exits 0 (changes ignored)
    let out2 = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .arg("--fail-on-change")
        .arg("--ignore")
        .arg("harness,model.name")
        .output()
        .unwrap();

    assert_eq!(out2.status.code(), Some(0));
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("ignored:"), "stdout2: {stdout2}");
    assert!(stdout2.contains("- harness.git_sha"), "stdout2: {stdout2}");
    assert!(stdout2.contains("- model.name"), "stdout2: {stdout2}");

    // 3. JSON mode with ignore reports correct summary and lists
    let out3 = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .arg("--ignore")
        .arg("harness,model.name")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert_eq!(out3.status.code(), Some(0));
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout3).unwrap();
    assert_eq!(parsed["summary"]["changed_field_count"].as_u64(), Some(0));
    assert_eq!(parsed["summary"]["ignored_field_count"].as_u64(), Some(2));
    assert!(parsed["changed_fields"].as_array().unwrap().is_empty());
    assert_eq!(parsed["ignored_fields"].as_array().unwrap().len(), 2);
}

#[test]
fn test_invalid_format_value_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .arg("--format")
        .arg("jsno")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unsupported output format"));
}

#[test]
fn test_corrupted_json_results_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    std::fs::write(baseline.join("results.json"), "{invalid json").unwrap();
    std::fs::write(candidate.join("results.json"), "{}").unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("failed to parse results.json"));
}

#[test]
fn test_malformed_toml_resolved_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let manifest_base = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        },
        "config": {
            "resolved": "invalid = toml = structure = 123"
        }
    });

    let payload_base = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_base
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("failed to parse baseline config.resolved"));
}

#[test]
fn test_malformed_manifest_type_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let payload_base = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": "not-an-object"
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid 'manifest' type: expected a JSON object"));
}

#[test]
fn test_empty_container_drift_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let manifest_base = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        },
        "sampling": {}
    });

    let manifest_cand = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        },
        "sampling": []
    });

    let payload_base = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_base
    });
    let payload_cand = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_cand
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_cand).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[sampling]"));
    assert!(
        stdout.contains("{}")
            && (stdout.contains("[]") || stdout.contains("->") || stdout.contains("→"))
    );
}

#[test]
fn test_fail_on_change_returns_exit_3() {
    let tmp = tempfile::tempdir().unwrap();
    let baseline = tmp.path().join("baseline");
    let candidate = tmp.path().join("candidate");
    std::fs::create_dir(&baseline).unwrap();
    std::fs::create_dir(&candidate).unwrap();

    let manifest_base = serde_json::json!({
        "harness": {
            "name": "maxwells-daemon",
            "version": "1.0.0",
            "git_sha": "abc1234",
            "git_dirty": false
        }
    });

    let mut manifest_cand = manifest_base.clone();
    manifest_cand["harness"]["name"] = serde_json::json!("different");

    let payload_base = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_base
    });
    let payload_cand = serde_json::json!({
        "total": 1,
        "instances": [],
        "manifest": manifest_cand
    });

    std::fs::write(
        baseline.join("results.json"),
        serde_json::to_string(&payload_base).unwrap(),
    )
    .unwrap();
    std::fs::write(
        candidate.join("results.json"),
        serde_json::to_string(&payload_cand).unwrap(),
    )
    .unwrap();

    let out = std::process::Command::new(support::binary_path())
        .arg("bench")
        .arg("diff-config")
        .arg("--baseline")
        .arg(&baseline)
        .arg("--candidate")
        .arg(&candidate)
        .arg("--fail-on-change")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(3));
}
