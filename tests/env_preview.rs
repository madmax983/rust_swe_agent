//! Tests for `agent env preview` — issue #313.
//!
//! RED phase: these tests should FAIL until the implementation exists.
//! GREEN phase: implement src/run/env_preview.rs and wire CLI.
//! REFACTOR phase: clean up.

#![allow(clippy::unwrap_used)]

use maxwells_daemon::config::Config;
use maxwells_daemon::exit_code::ExitCode;
use maxwells_daemon::run::env_preview::{
    EnvPreview, EnvPreviewOpts, EnvVarPreview, HookEntry, HooksPreview, McpServerPreview,
    PolicyPreview, PreviewFinding, format_preview_text, is_risky, run_env_preview,
};

// ── Exit code ─────────────────────────────────────────────────────────────────

#[test]
fn env_preview_warning_exit_code_is_13() {
    assert_eq!(ExitCode::EnvPreviewWarning.as_i32(), 13);
}

#[test]
fn env_preview_warning_outcome_class_string() {
    assert_eq!(
        ExitCode::EnvPreviewWarning.outcome_class(),
        "env_preview_warning"
    );
}

// ── Struct fields exist ───────────────────────────────────────────────────────

#[test]
fn env_preview_struct_has_expected_fields() {
    let preview = EnvPreview {
        schema_version: 1,
        env_type: "local".into(),
        host_paths: vec!["/workspace".into()],
        network_egress: "unrestricted".into(),
        hooks: HooksPreview {
            pre_tool_use: vec![],
            post_tool_use: vec![],
        },
        mcp_servers: vec![],
        env_vars: vec![],
        policy: PolicyPreview {
            profile: "safe".into(),
            extra_deny: vec![],
            extra_allow: vec![],
        },
        findings: vec![],
    };
    assert_eq!(preview.schema_version, 1);
    assert_eq!(preview.env_type, "local");
}

#[test]
fn hook_entry_has_name_and_command() {
    let hook = HookEntry {
        name: "my-hook".into(),
        command: "echo hello".into(),
    };
    assert_eq!(hook.name, "my-hook");
    assert_eq!(hook.command, "echo hello");
}

#[test]
fn mcp_server_preview_has_outside_workdir_flag() {
    let mcp = McpServerPreview {
        name: "my-mcp".into(),
        command: "/usr/local/bin/mcp-server".into(),
        outside_workdir: true,
    };
    assert!(mcp.outside_workdir);
}

#[test]
fn env_var_preview_has_sensitive_flag() {
    let ev = EnvVarPreview {
        name: "FAKE_API_KEY".into(),
        value_or_redacted: "[REDACTED]".into(),
        sensitive: true,
    };
    assert!(ev.sensitive);
}

#[test]
fn preview_finding_has_severity_and_message() {
    let f = PreviewFinding {
        severity: "warning".into(),
        message: "Local env with wide path grants full host access".into(),
    };
    assert_eq!(f.severity, "warning");
}

// ── run_env_preview ───────────────────────────────────────────────────────────

#[test]
fn run_env_preview_returns_preview_with_correct_env_type() {
    let cfg = Config::defaults().unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "fix the bug".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert_eq!(preview.env_type, "local");
    assert_eq!(preview.schema_version, 1);
}

#[test]
fn run_env_preview_schema_version_is_one() {
    let cfg = Config::defaults().unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert_eq!(preview.schema_version, 1);
}

// ── Risky detection ───────────────────────────────────────────────────────────

#[test]
fn is_risky_returns_false_for_clean_preview() {
    let preview = EnvPreview {
        schema_version: 1,
        env_type: "local".into(),
        host_paths: vec!["/workspace".into()],
        network_egress: "unrestricted".into(),
        hooks: HooksPreview {
            pre_tool_use: vec![],
            post_tool_use: vec![],
        },
        mcp_servers: vec![],
        env_vars: vec![],
        policy: PolicyPreview {
            profile: "safe".into(),
            extra_deny: vec![],
            extra_allow: vec![],
        },
        findings: vec![],
    };
    assert!(!is_risky(&preview));
}

#[test]
fn is_risky_returns_true_when_findings_present() {
    let preview = EnvPreview {
        schema_version: 1,
        env_type: "local".into(),
        host_paths: vec!["/".into()],
        network_egress: "unrestricted".into(),
        hooks: HooksPreview {
            pre_tool_use: vec![],
            post_tool_use: vec![],
        },
        mcp_servers: vec![],
        env_vars: vec![],
        policy: PolicyPreview {
            profile: "safe".into(),
            extra_deny: vec![],
            extra_allow: vec![],
        },
        findings: vec![PreviewFinding {
            severity: "warning".into(),
            message: "Local env with wide host path: /".into(),
        }],
    };
    assert!(is_risky(&preview));
}

// ── Local env + wide path triggers warning ────────────────────────────────────

#[test]
fn local_env_with_root_workdir_triggers_warning() {
    let cfg = Config::from_toml_str(
        r#"
[environment]
kind = "local"
workdir = "/"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        is_risky(&preview),
        "local env with root workdir should trigger a risky finding"
    );
    assert!(
        preview.findings.iter().any(|f| f.severity == "warning"),
        "expected at least one warning finding"
    );
}

// ── Sensitive env var detection ───────────────────────────────────────────────

#[test]
fn sensitive_env_var_name_is_flagged() {
    let cfg = Config::defaults().unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    for ev in &preview.env_vars {
        if ev.sensitive {
            assert!(
                !ev.value_or_redacted.contains("sk-deadbeef"),
                "sensitive var value leaked verbatim: {}",
                ev.value_or_redacted
            );
        }
    }
}

// ── Redaction: sensitive values always masked when show_values=false ──────────

#[test]
fn redaction_sk_deadbeef_does_not_appear_in_text_output() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["sk-deadbeef"]
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task referencing sk-deadbeef".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    let text_output = format!("{preview:?}");
    for ev in &preview.env_vars {
        assert!(
            !ev.value_or_redacted.contains("sk-deadbeef"),
            "sk-deadbeef leaked in env var value: {}",
            ev.value_or_redacted
        );
    }
    assert!(
        !text_output.contains("sk-deadbeef")
            || preview
                .findings
                .iter()
                .all(|f| !f.message.contains("sk-deadbeef")),
        "sk-deadbeef should not appear verbatim in findings"
    );
}

#[test]
fn redaction_disabled_config_still_masks_env_vars_without_show_values() {
    // P1 fix: redaction disabled in config must NOT leak sensitive env var values
    // when --show-values was not passed.
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = false
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    for ev in &preview.env_vars {
        assert_eq!(
            ev.value_or_redacted, "[REDACTED:env_preview]",
            "sensitive var '{}' should be masked even when redaction config is disabled",
            ev.name
        );
    }
}

// ── JSON serialization ────────────────────────────────────────────────────────

#[test]
fn env_preview_serializes_to_json_with_schema_version() {
    let preview = EnvPreview {
        schema_version: 1,
        env_type: "local".into(),
        host_paths: vec!["/workspace".into()],
        network_egress: "unrestricted".into(),
        hooks: HooksPreview {
            pre_tool_use: vec![],
            post_tool_use: vec![],
        },
        mcp_servers: vec![],
        env_vars: vec![],
        policy: PolicyPreview {
            profile: "safe".into(),
            extra_deny: vec![],
            extra_allow: vec![],
        },
        findings: vec![],
    };
    let json = serde_json::to_string(&preview).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["env_type"], "local");
}

#[test]
fn env_preview_json_has_env_preview_wrapper_key() {
    let preview = EnvPreview {
        schema_version: 1,
        env_type: "docker".into(),
        host_paths: vec![],
        network_egress: "unrestricted".into(),
        hooks: HooksPreview {
            pre_tool_use: vec![],
            post_tool_use: vec![],
        },
        mcp_servers: vec![],
        env_vars: vec![],
        policy: PolicyPreview {
            profile: "safe".into(),
            extra_deny: vec![],
            extra_allow: vec![],
        },
        findings: vec![],
    };
    let wrapped = serde_json::json!({ "env_preview": &preview });
    let json_str = serde_json::to_string(&wrapped).unwrap();
    assert!(json_str.contains("\"env_preview\""));
    assert!(json_str.contains("\"schema_version\":1"));
}

// ── MCP server outside workdir ────────────────────────────────────────────────

#[test]
fn mcp_server_outside_workdir_triggers_warning() {
    let cfg = Config::from_toml_str(
        r#"
[environment]
kind = "local"
workdir = "/workspace"

[[agent.mcp_servers]]
command = "/usr/local/bin/external-mcp"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "expected MCP server outside workdir to be detected"
    );
}

#[test]
fn mcp_server_path_boundary_not_confused_by_prefix_match() {
    // P2 fix: /workspace-tools should NOT count as inside /workspace.
    let cfg = Config::from_toml_str(
        r#"
[environment]
kind = "local"
workdir = "/workspace"

[[agent.mcp_servers]]
command = "/workspace-tools/mcp-server"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "/workspace-tools should be detected as outside /workspace"
    );
}

#[test]
fn mcp_server_inside_workdir_not_flagged() {
    let cfg = Config::from_toml_str(
        r#"
[environment]
kind = "local"
workdir = "/workspace"

[[agent.mcp_servers]]
command = "/workspace/bin/mcp-server"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        !preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "/workspace/bin/mcp-server should NOT be flagged as outside /workspace"
    );
}

// ── Policy preview ────────────────────────────────────────────────────────────

#[test]
fn policy_preview_reflects_config_profile() {
    let cfg = Config::from_toml_str(
        r#"
[policy]
profile = "yolo"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert_eq!(preview.policy.profile, "yolo");
}

// ── Hooks preview ─────────────────────────────────────────────────────────────

#[test]
fn hooks_preview_reflects_config_hooks() {
    let cfg = Config::from_toml_str(
        r#"
[[agent.hooks.pre_tool_use]]
name = "pre-check"
command = "echo pre"

[[agent.hooks.post_tool_use]]
name = "post-log"
command = "echo post"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert_eq!(preview.hooks.pre_tool_use.len(), 1);
    assert_eq!(preview.hooks.post_tool_use.len(), 1);
    assert_eq!(preview.hooks.pre_tool_use[0].name, "pre-check");
    assert_eq!(preview.hooks.post_tool_use[0].name, "post-log");
}

// ── Broad sensitive-name detection ────────────────────────────────────────────

#[test]
fn sensitive_var_detection_covers_password_and_credential() {
    // P2 fix: PASSWORD and CREDENTIAL names must be detected, not just TOKEN/SECRET/API_KEY.
    // We can't easily inject env vars in unit tests, but we can test is_sensitive_var_name
    // indirectly by verifying the preview produced with a config that has a secret literal
    // matching those patterns doesn't fail to flag vars (if they are set).
    // Direct unit test via the public `run_env_preview` path is done by checking
    // that env vars with broader names are caught when present in the process env.
    // Since we can't guarantee those are set in CI, we test the logic explicitly here.
    use maxwells_daemon::run::env_preview::is_sensitive_var_name_test_helper;
    assert!(is_sensitive_var_name_test_helper("DB_PASSWORD"));
    assert!(is_sensitive_var_name_test_helper("AWS_SECRET_ACCESS_KEY"));
    assert!(is_sensitive_var_name_test_helper("GOOGLE_CREDENTIALS"));
    assert!(is_sensitive_var_name_test_helper("USER_CREDENTIAL"));
    assert!(is_sensitive_var_name_test_helper("ANTHROPIC_API_KEY"));
    assert!(is_sensitive_var_name_test_helper("GITHUB_TOKEN"));
    assert!(!is_sensitive_var_name_test_helper("HOME"));
    assert!(!is_sensitive_var_name_test_helper("PATH"));
    assert!(!is_sensitive_var_name_test_helper("USER"));
}

// ── Network egress always shows unrestricted ──────────────────────────────────

#[test]
fn preview_network_egress_is_unrestricted() {
    let cfg = Config::defaults().unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert_eq!(preview.network_egress, "unrestricted");
}

// ── format_preview_text covers all 7 sections ────────────────────────────────

#[test]
fn format_preview_text_contains_all_sections() {
    let preview = EnvPreview {
        schema_version: 1,
        env_type: "local".into(),
        host_paths: vec!["/workspace".into()],
        network_egress: "unrestricted".into(),
        hooks: HooksPreview {
            pre_tool_use: vec![HookEntry {
                name: "pre".into(),
                command: "echo pre".into(),
            }],
            post_tool_use: vec![HookEntry {
                name: "post".into(),
                command: "echo post".into(),
            }],
        },
        mcp_servers: vec![McpServerPreview {
            name: "mcp-0".into(),
            command: "/usr/bin/mcp".into(),
            outside_workdir: true,
        }],
        env_vars: vec![EnvVarPreview {
            name: "MY_TOKEN".into(),
            value_or_redacted: "[REDACTED:env_preview]".into(),
            sensitive: true,
        }],
        policy: PolicyPreview {
            profile: "safe".into(),
            extra_deny: vec!["rm -rf".into()],
            extra_allow: vec![],
        },
        findings: vec![PreviewFinding {
            severity: "warning".into(),
            message: "MCP outside workdir".into(),
        }],
    };
    let text = format_preview_text(&preview);
    assert!(text.contains("env_type:       local"));
    assert!(text.contains("host_paths:     /workspace"));
    assert!(text.contains("network_egress: unrestricted"));
    assert!(text.contains("--- Hooks ---"));
    assert!(text.contains("PreToolUse  [pre]: echo pre"));
    assert!(text.contains("PostToolUse [post]: echo post"));
    assert!(text.contains("--- MCP Servers ---"));
    assert!(text.contains("[OUTSIDE WORKDIR]"));
    assert!(text.contains("--- Env Vars (sensitive) ---"));
    assert!(text.contains("MY_TOKEN: [REDACTED:env_preview]"));
    assert!(text.contains("--- Policy ---"));
    assert!(text.contains("profile:     safe"));
    assert!(text.contains("extra_deny:  rm -rf"));
    assert!(text.contains("extra_allow: (none)"));
    assert!(text.contains("--- Findings ---"));
    assert!(text.contains("[WARNING] MCP outside workdir"));
}

#[test]
fn format_preview_text_clean_findings_label() {
    let preview = EnvPreview {
        schema_version: 1,
        env_type: "docker".into(),
        host_paths: vec![],
        network_egress: "unrestricted".into(),
        hooks: HooksPreview {
            pre_tool_use: vec![],
            post_tool_use: vec![],
        },
        mcp_servers: vec![],
        env_vars: vec![],
        policy: PolicyPreview {
            profile: "safe".into(),
            extra_deny: vec![],
            extra_allow: vec!["safe-cmd".into()],
        },
        findings: vec![],
    };
    let text = format_preview_text(&preview);
    assert!(text.contains("--- Findings: CLEAN ---"));
    assert!(text.contains("(none)"), "empty hooks/mcp/vars show (none)");
    assert!(text.contains("extra_allow: safe-cmd"));
}

// ── Docker env skips host-process env scan ────────────────────────────────────

#[test]
fn docker_env_type_produces_no_env_var_findings() {
    let cfg = Config::defaults().unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview.env_vars.is_empty(),
        "docker preview should not scan the host process environment"
    );
}

// ── Relative MCP command is flagged ──────────────────────────────────────────

#[test]
fn relative_mcp_command_flagged_as_outside_workdir() {
    let cfg = Config::from_toml_str(
        r#"
[environment]
workdir = "/workspace"

[[agent.mcp_servers]]
command = "diagnostic-mcp --port 9000"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "relative MCP command 'diagnostic-mcp' should be flagged as outside workdir"
    );
}

#[test]
fn shell_prefixed_mcp_command_detected_correctly() {
    // FOO=bar /usr/local/bin/mcp — exe is /usr/local/bin/mcp, which is outside /workspace.
    let cfg = Config::from_toml_str(
        r#"
[environment]
workdir = "/workspace"

[[agent.mcp_servers]]
command = "FOO=bar /usr/local/bin/external-mcp"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "shell-env-prefixed MCP with external binary should be flagged"
    );
}

// ── Policy strings are redacted ───────────────────────────────────────────────

#[test]
fn policy_extra_deny_is_redacted() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["sk-deadbeef"]

[policy]
extra_deny_patterns = ["sk-deadbeef-pattern"]
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    for pattern in &preview.policy.extra_deny {
        assert!(
            !pattern.contains("sk-deadbeef"),
            "secret literal leaked in extra_deny_patterns: {pattern}"
        );
    }
}

// ── Policy profile is redacted ────────────────────────────────────────────────

#[test]
fn policy_profile_is_redacted_when_it_contains_a_secret_literal() {
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["sk-secret"]

[policy]
profile = "custom-sk-secret-profile"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        !preview.policy.profile.contains("sk-secret"),
        "secret literal leaked verbatim in policy.profile: {}",
        preview.policy.profile
    );
}

// ── Docker without docker_image triggers warning ──────────────────────────────

#[test]
fn docker_env_without_docker_image_triggers_warning() {
    // Config::defaults() sets docker_image = None, so docker preview should warn.
    let cfg = Config::defaults().unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        is_risky(&preview),
        "docker preview with no docker_image should be risky"
    );
    assert!(
        preview
            .findings
            .iter()
            .any(|f| f.message.contains("docker_image")),
        "expected a finding mentioning docker_image; got: {:?}",
        preview.findings
    );
}

// ── Local env always warns about full host filesystem access ──────────────────

#[test]
fn local_env_has_full_host_access_warning() {
    let cfg = Config::from_toml_str(
        r#"
[environment]
kind = "local"
workdir = "/workspace"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "local".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        is_risky(&preview),
        "local env should always have at least one risky finding"
    );
    assert!(
        preview
            .findings
            .iter()
            .any(|f| f.message.contains("not confined to workdir")),
        "expected a finding about unconfined workdir access; got: {:?}",
        preview.findings
    );
}

// ── Quoted shell assignment in MCP command not flagged as outside workdir ─────

#[test]
fn quoted_shell_assignment_in_mcp_command_not_flagged_as_outside() {
    // FOO='bar baz' /workspace/bin/mcp — the quoted value has a space; the exe
    // is /workspace/bin/mcp which is INSIDE the workdir.
    let cfg = Config::from_toml_str(
        r#"
[environment]
workdir = "/workspace"

[[agent.mcp_servers]]
command = "FOO='bar baz' /workspace/bin/mcp"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        !preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "quoted shell assignment should not confuse the exe-path parser; mcp_servers: {:?}",
        preview.mcp_servers
    );
}

// ── Policy validation error is redacted ──────────────────────────────────────

#[test]
fn policy_validation_error_does_not_leak_secret_literal() {
    // extra_deny_patterns contains both a secret literal and an invalid regex
    // character, so PolicyEngine::from_cfg returns an error whose display text
    // includes the raw pattern. The finding must not expose the literal.
    let cfg = Config::from_toml_str(
        r#"
[redaction]
enabled = true
secret_literals = ["sk-secret"]

[policy]
extra_deny_patterns = ["sk-secret("]
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    for finding in &preview.findings {
        assert!(
            !finding.message.contains("sk-secret"),
            "secret literal leaked in policy validation finding: {}",
            finding.message
        );
    }
}

// ── Preview env-type mismatches config.environment.kind ──────────────────────

#[test]
fn preview_env_mismatch_with_config_kind_triggers_warning() {
    // Config says kind=local, but --env docker is passed → runs use local.
    let cfg = Config::from_toml_str(
        r#"
[environment]
kind = "local"
workdir = "/workspace"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview
            .findings
            .iter()
            .any(|f| f.message.contains("does not match")),
        "expected a mismatch finding; got: {:?}",
        preview.findings
    );
}

// ── Docker host_paths is empty ────────────────────────────────────────────────

#[test]
fn docker_env_host_paths_is_empty() {
    let cfg = Config::from_toml_str(
        r#"
[environment]
kind = "docker"
workdir = "/workspace"
docker_image = "ubuntu:22.04"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview.host_paths.is_empty(),
        "docker preview must not report container cwd as a host path; got: {:?}",
        preview.host_paths
    );
}

// ── Compound MCP command is flagged as outside workdir ────────────────────────

#[test]
fn compound_mcp_command_flagged_as_outside_workdir() {
    // /workspace/bin/setup && /usr/local/bin/mcp — the second binary is external;
    // since we can't safely analyse compound commands, the whole thing is flagged.
    let cfg = Config::from_toml_str(
        r#"
[environment]
workdir = "/workspace"

[[agent.mcp_servers]]
command = "/workspace/bin/setup && /usr/local/bin/mcp"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "compound shell command should be flagged as outside workdir"
    );
}

// ── Docker without the docker feature warns ───────────────────────────────────

#[cfg(not(feature = "docker"))]
#[test]
fn docker_without_docker_feature_triggers_warning() {
    let cfg = Config::from_toml_str(
        r#"
[environment]
kind = "docker"
workdir = "/workspace"
docker_image = "ubuntu:22.04"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview
            .findings
            .iter()
            .any(|f| f.message.contains("docker")),
        "non-docker build should warn about the missing docker feature; got: {:?}",
        preview.findings
    );
}

// ── Quoted shell operator in assignment value not flagged ─────────────────────

#[test]
fn quoted_shell_operator_in_assignment_value_not_flagged() {
    // FOO='a;b' /workspace/bin/mcp — the ';' is inside quotes; the exe is
    // /workspace/bin/mcp which is inside the workdir. Should NOT be flagged.
    let cfg = Config::from_toml_str(
        r#"
[environment]
workdir = "/workspace"

[[agent.mcp_servers]]
command = "FOO='a;b' /workspace/bin/mcp"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        !preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "quoted ';' in assignment must not trigger the compound-command check; \
         mcp_servers: {:?}",
        preview.mcp_servers
    );
}

// ── Background-operator MCP command flagged as outside workdir ────────────────

#[test]
fn background_operator_mcp_command_flagged_as_outside_workdir() {
    // /workspace/bin/setup & /usr/local/bin/mcp — single & backgrounds the
    // first command; the shell then starts the external MCP process.
    let cfg = Config::from_toml_str(
        r#"
[environment]
workdir = "/workspace"

[[agent.mcp_servers]]
command = "/workspace/bin/setup & /usr/local/bin/mcp"
"#,
    )
    .unwrap();
    let opts = EnvPreviewOpts {
        env_type: "docker".into(),
        task: "task".into(),
        config_path: None,
        show_values: false,
    };
    let preview = run_env_preview(&cfg, &opts);
    assert!(
        preview.mcp_servers.iter().any(|m| m.outside_workdir),
        "background-operator '&' should flag the command as a compound command"
    );
}

// ── is_sensitive_var_name_test_helper covers all branch values ────────────────
// (already tested above in sensitive_var_detection_covers_password_and_credential)
