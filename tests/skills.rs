#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::sync::Arc;

use maxwells_daemon::agent::default::DefaultAgentBuilder;
use maxwells_daemon::skills::{
    ActiveSkill, ActiveSkillSet, SkillActivationReason, SkillRegistry, SkillResolveRequest,
    resolve_for_task,
};
use maxwells_daemon::{
    Agent, Config, DeterministicModel, Environment, ExitReason, LocalEnvironment,
};

#[test]
fn registry_scans_skill_manifests_without_loading_bodies() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        r#"---
name: rust-router
description: Use for Rust questions and cargo work.
---

# Rust Router

RUST_ROUTER_BODY_SHOULD_NOT_BE_IN_MANIFEST
"#,
    );
    write_skill(
        temp.path(),
        "security-review",
        r#"---
name: security-review
description: Use for security review and threat modeling.
version: 1.0.0
---

# Security Review
"#,
    );

    let registry = SkillRegistry::scan_paths([temp.path().to_path_buf()]).unwrap();
    let manifests = registry.manifests();

    assert_eq!(manifests.len(), 2);
    assert_eq!(manifests[0].name, "rust-router");
    assert_eq!(
        manifests[0].description,
        "Use for Rust questions and cargo work."
    );
    assert_eq!(manifests[1].name, "security-review");
    assert!(
        !format!("{manifests:?}").contains("RUST_ROUTER_BODY_SHOULD_NOT_BE_IN_MANIFEST"),
        "manifest registry must not retain unloaded skill bodies"
    );
}

#[test]
fn frontmatter_parser_ignores_trailing_comments_outside_quotes() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "commented-skill",
        r##"---
name: commented-skill # route by this name
description: "Use # inside quoted text" # but not this comment
---

# Commented Skill
"##,
    );

    let registry = SkillRegistry::scan_paths([temp.path().to_path_buf()]).unwrap();
    let manifest = &registry.manifests()[0];

    assert_eq!(manifest.name, "commented-skill");
    assert_eq!(manifest.description, "Use # inside quoted text");
    let active = registry
        .resolve(SkillResolveRequest {
            task: "Use $commented-skill for this task.",
            auto_load: false,
            max_active: 8,
        })
        .unwrap();
    assert_eq!(active.skills[0].name, "commented-skill");
}

#[test]
fn resolver_loads_explicit_skill_without_leaking_inactive_metadata() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        r#"---
name: rust-router
description: Use for Rust questions and cargo work.
---

# Rust Router

RUST_ROUTER_BODY
"#,
    );
    write_skill(
        temp.path(),
        "secret-skill",
        r#"---
name: secret-skill
description: SECRET_METADATA_DO_NOT_LEAK
---

# Secret Skill

SECRET_BODY_DO_NOT_LEAK
"#,
    );

    let registry = SkillRegistry::scan_paths([temp.path().to_path_buf()]).unwrap();
    let active = registry
        .resolve(SkillResolveRequest {
            task: "Use $rust-router to fix this borrow checker issue.",
            auto_load: false,
            max_active: 8,
        })
        .unwrap();

    assert_eq!(active.skills.len(), 1);
    assert_eq!(active.skills[0].name, "rust-router");
    assert_eq!(
        active.skills[0].activation_reason,
        SkillActivationReason::ExplicitMention
    );
    assert_eq!(active.skills[0].sha256.len(), 64);

    let context = active.render_context();
    assert!(context.contains("RUST_ROUTER_BODY"));
    assert!(!context.contains("SECRET_METADATA_DO_NOT_LEAK"));
    assert!(!context.contains("SECRET_BODY_DO_NOT_LEAK"));
}

#[test]
fn auto_resolver_loads_skills_from_task_keywords() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "security-review",
        r#"---
name: security-review
description: Use for security review and threat model work.
---

# Security Review

SECURITY_REVIEW_BODY
"#,
    );

    let registry = SkillRegistry::scan_paths([temp.path().to_path_buf()]).unwrap();
    let active = registry
        .resolve(SkillResolveRequest {
            task: "Please perform a security review of the policy engine.",
            auto_load: true,
            max_active: 8,
        })
        .unwrap();

    assert_eq!(active.skills.len(), 1);
    assert_eq!(active.skills[0].name, "security-review");
    assert_eq!(
        active.skills[0].activation_reason,
        SkillActivationReason::AutoMatch
    );
    assert!(active.render_context().contains("SECURITY_REVIEW_BODY"));
}

#[test]
fn auto_resolver_matches_short_programming_language_tokens() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "go",
        r#"---
name: go
description: Use for Go code.
---

# Go

GO_BODY
"#,
    );

    let registry = SkillRegistry::scan_paths([temp.path().to_path_buf()]).unwrap();
    let active = registry
        .resolve(SkillResolveRequest {
            task: "Fix this Go code.",
            auto_load: true,
            max_active: 8,
        })
        .unwrap();

    assert_eq!(active.skills.len(), 1);
    assert_eq!(active.skills[0].name, "go");
}

#[test]
fn explicit_mentions_require_boundary_after_skill_name() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "go",
        r#"---
name: go
description: Use for Go code.
---

# Go

GO_BODY
"#,
    );

    let registry = SkillRegistry::scan_paths([temp.path().to_path_buf()]).unwrap();

    for task in [
        "Review /goals before planning.",
        "Open /google in the browser.",
        "Audit $governance settings.",
        "Inspect @golang ownership.",
        "Visit https://example.com/google?q=1.",
        "Visit https://example.com/go?q=1.",
        "Email person@go.dev for details.",
    ] {
        let active = registry
            .resolve(SkillResolveRequest {
                task,
                auto_load: false,
                max_active: 8,
            })
            .unwrap();
        assert!(
            active.skills.is_empty(),
            "task `{task}` should not explicitly activate go"
        );
    }

    for task in ["Use /go for this task.", "Use $go.", "Ask @go now."] {
        let active = registry
            .resolve(SkillResolveRequest {
                task,
                auto_load: false,
                max_active: 8,
            })
            .unwrap();
        assert_eq!(active.skills.len(), 1, "task `{task}` should activate go");
        assert_eq!(active.skills[0].name, "go");
    }
}

#[test]
fn active_skill_context_appends_after_operator_extra_context() {
    let active = ActiveSkillSet {
        skills: vec![ActiveSkill {
            name: "rust-router".to_owned(),
            description: "Use for Rust work.".to_owned(),
            path: "rust-router/SKILL.md".into(),
            content: "# Rust Router\n\nRUST_ROUTER_BODY".to_owned(),
            sha256: "0".repeat(64),
            activation_reason: SkillActivationReason::ExplicitMention,
        }],
    };

    let merged = active.merge_extra_context(Some("Operator context".to_owned()));
    let merged = merged.unwrap();

    assert!(merged.contains("Operator context"));
    assert!(merged.contains("Active agent skills"));
    assert!(merged.contains("RUST_ROUTER_BODY"));
}

#[test]
fn active_skill_context_omits_source_path_and_hash() {
    let active = ActiveSkillSet {
        skills: vec![ActiveSkill {
            name: "rust-router".to_owned(),
            description: "Use for Rust work.".to_owned(),
            path: "C:/Users/markm/private/SKILL.md".into(),
            content: "# Rust Router\n\nRUST_ROUTER_BODY".to_owned(),
            sha256: "a".repeat(64),
            activation_reason: SkillActivationReason::ExplicitMention,
        }],
    };

    let context = active.render_context();

    assert!(context.contains("Skill: rust-router"));
    assert!(context.contains("Activation: explicit_mention"));
    assert!(context.contains("RUST_ROUTER_BODY"));
    assert!(!context.contains("C:/Users/markm/private/SKILL.md"));
    assert!(!context.contains(&"a".repeat(64)));
    assert!(!context.contains("SHA-256"));
    assert!(!context.contains("Source:"));
}

#[test]
fn config_parses_skills_section() {
    let cfg = Config::from_toml_str(
        r#"
[skills]
enabled = true
auto_load = false
paths = ["./.agents/skills", "./.codex/skills"]
max_active = 3
"#,
    )
    .unwrap();

    assert!(cfg.root.skills.enabled);
    assert!(!cfg.root.skills.auto_load);
    assert_eq!(
        cfg.root.skills.paths,
        vec!["./.agents/skills", "./.codex/skills"]
    );
    assert_eq!(cfg.root.skills.max_active, 3);
}

#[tokio::test]
async fn selected_skill_context_reaches_agent_prompt_without_inactive_manifest_leak() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        r#"---
name: rust-router
description: Use for Rust questions and cargo work.
---

# Rust Router

RUST_ROUTER_BODY
"#,
    );
    write_skill(
        temp.path(),
        "secret-skill",
        r#"---
name: secret-skill
description: SECRET_METADATA_DO_NOT_LEAK
---

# Secret Skill

SECRET_BODY_DO_NOT_LEAK
"#,
    );

    let skill_path = toml_path(temp.path());
    let cfg = Config::from_toml_str(&format!(
        r#"
[skills]
enabled = true
auto_load = false
paths = ["{skill_path}"]
"#,
    ))
    .unwrap();
    let resolved = resolve_for_task(
        &cfg.root.skills,
        "Use $rust-router to fix this Rust issue.",
        None,
    )
    .unwrap();
    let model = Arc::new(DeterministicModel::new([
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".to_owned(),
    ]));
    let env: Box<dyn Environment> = Box::new(LocalEnvironment::new());
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model: model.clone(),
        env,
        task: "Use $rust-router to fix this Rust issue.".to_owned(),
        extra_context: resolved.merged_extra_context,
        renderer: None,
        stream: None,
        resume_from: None,
        read_only: false,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let recorded = model.recorded_inputs();
    let first_prompt = recorded[0]
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(first_prompt.contains("RUST_ROUTER_BODY"));
    assert!(!first_prompt.contains("SECRET_METADATA_DO_NOT_LEAK"));
    assert!(!first_prompt.contains("SECRET_BODY_DO_NOT_LEAK"));
}

#[tokio::test]
async fn mini_run_records_active_skill_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "security-review",
        r#"---
name: security-review
description: Use for security review and threat model work.
---

# Security Review

SECURITY_REVIEW_BODY
"#,
    );

    let skill_path = toml_path(temp.path());
    let cfg = Config::from_toml_str(&format!(
        r#"
[skills]
enabled = true
auto_load = true
paths = ["{skill_path}"]
"#,
    ))
    .unwrap();

    maxwells_daemon::run::mini::run(maxwells_daemon::run::mini::MiniArgs {
        driver: maxwells_daemon::run::mini::RunDriver::Builtin,
        driver_append_system_prompt: false,
        task: "Please perform a security review.".to_owned(),
        extra_context: None,
        config: cfg,
        output_dir: output.path().to_path_buf(),
        trajectory_name: "skill-run".to_owned(),
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".to_owned(),
        ]),
        deterministic_usage_per_call: None,
        task_timeout_secs: None,
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 60,
        interactive_mode: maxwells_daemon::run::mini::InteractiveMode::Off,
        resume_from: None,
        trace_id: None,
        webhook_url: None,
        webhook_headers: vec![],
        event_log: None,
        event_log_instance_id: None,
        local_workdir: None,
        read_only: false,
        allow_mcp_in_read_only: false,
        rehearsal_gold_patch: None,
        no_step_persist: false,
        parent_sweep_run_id: None,
        continue_from: None,
    })
    .await
    .unwrap();

    let trajectory_path = output.path().join("skill-run.traj.json");
    let trajectory: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(trajectory_path).unwrap()).unwrap();
    let active_skills = trajectory["info"]["active_skills"].as_array().unwrap();
    assert_eq!(active_skills[0]["name"].as_str(), Some("security-review"));
    assert_eq!(
        active_skills[0]["activation_reason"].as_str(),
        Some("auto_match")
    );
    assert_eq!(active_skills[0]["sha256"].as_str().unwrap().len(), 64);

    let first_user_message = trajectory["messages"][1]["content"].as_str().unwrap();
    assert!(first_user_message.contains("SECURITY_REVIEW_BODY"));
}

#[tokio::test]
async fn mini_run_redacts_active_skill_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let secret_root = temp.path().join("TOP_SECRET_ROOT");
    write_skill(
        &secret_root,
        "security-review",
        r#"---
name: security-review
description: Use TOP_SECRET_VALUE for security review work.
---

# Security Review

SECURITY_REVIEW_BODY
"#,
    );

    let skill_path = toml_path(&secret_root);
    let cfg = Config::from_toml_str(&format!(
        r#"
[skills]
enabled = true
auto_load = true
paths = ["{skill_path}"]

[redaction]
secret_literals = ["TOP_SECRET_VALUE", "TOP_SECRET_ROOT"]
"#,
    ))
    .unwrap();

    maxwells_daemon::run::mini::run(maxwells_daemon::run::mini::MiniArgs {
        driver: maxwells_daemon::run::mini::RunDriver::Builtin,
        driver_append_system_prompt: false,
        task: "Please perform a security review.".to_owned(),
        extra_context: None,
        config: cfg,
        output_dir: output.path().to_path_buf(),
        trajectory_name: "skill-redaction-run".to_owned(),
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".to_owned(),
        ]),
        deterministic_usage_per_call: None,
        task_timeout_secs: None,
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 60,
        interactive_mode: maxwells_daemon::run::mini::InteractiveMode::Off,
        resume_from: None,
        trace_id: None,
        webhook_url: None,
        webhook_headers: vec![],
        event_log: None,
        event_log_instance_id: None,
        local_workdir: None,
        read_only: false,
        allow_mcp_in_read_only: false,
        rehearsal_gold_patch: None,
        no_step_persist: false,
        parent_sweep_run_id: None,
        continue_from: None,
    })
    .await
    .unwrap();

    let trajectory_path = output.path().join("skill-redaction-run.traj.json");
    let trajectory_text = fs::read_to_string(trajectory_path).unwrap();
    assert!(!trajectory_text.contains("TOP_SECRET_VALUE"));
    assert!(!trajectory_text.contains("TOP_SECRET_ROOT"));
    assert!(trajectory_text.contains("[REDACTED:configured_literal:"));
}

fn write_skill(root: &Path, dirname: &str, content: &str) {
    let dir = root.join(dirname);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), content).unwrap();
}

fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}
