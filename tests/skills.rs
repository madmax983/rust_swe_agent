#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::sync::Arc;

use rust_swe_agent::agent::default::DefaultAgentBuilder;
use rust_swe_agent::skills::{
    ActiveSkill, ActiveSkillSet, SkillActivationReason, SkillRegistry, SkillResolveRequest,
    resolve_for_task,
};
use rust_swe_agent::{
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

    rust_swe_agent::run::mini::run(rust_swe_agent::run::mini::MiniArgs {
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

fn write_skill(root: &Path, dirname: &str, content: &str) {
    let dir = root.join(dirname);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), content).unwrap();
}

fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}
