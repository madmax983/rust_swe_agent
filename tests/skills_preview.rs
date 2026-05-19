//! Integration tests for `agent skills-preview` (issue #337).
//!
//! Red phase: these tests FAIL until the feature is implemented.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

fn binary() -> std::path::PathBuf {
    support::binary_path()
}

fn write_skill(root: &Path, dirname: &str, content: &str) {
    let dir = root.join(dirname);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), content).unwrap();
}

fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn write_skill_config(temp: &Path, skill_root: &Path) -> std::path::PathBuf {
    let config_path = temp.join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[skills]\nenabled = true\nauto_load = true\npaths = [\"{}\"]\n",
            toml_path(skill_root)
        ),
    )
    .unwrap();
    config_path
}

// ── AC1: exits 0 without invoking any model ───────────────────────────────────

#[test]
fn agent_skills_preview_exits_zero_with_valid_task() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix a Rust borrow checker issue",
            "--config",
            &config_path.display().to_string(),
        ])
        .output()
        .expect("failed to spawn binary");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "expected exit 0; code={}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code().unwrap_or(-1)
    );
}

#[test]
fn agent_skills_preview_zero_cost_with_invalid_api_key() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .env("ANTHROPIC_API_KEY", "sk-ant-intentionally-invalid-key-test")
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix a Rust borrow checker issue",
            "--config",
            &config_path.display().to_string(),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "must exit 0 even with invalid API key (no model calls made)\nstderr:\n{stderr}"
    );
}

// ── AC2: --task-file reads one task per line, ignores # prefixed lines ────────

#[test]
fn agent_skills_preview_task_file_reads_tasks() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let task_file = temp.path().join("tasks.txt");
    fs::write(
        &task_file,
        "# comment line ignored\nfix a Rust borrow checker issue\nreview Rust code\n",
    )
    .unwrap();

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task-file",
            &task_file.display().to_string(),
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Two tasks from the file should produce two task hashes in output.
    // May exit 14 (warning) if auto_match fires; that's fine for this test.
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--format json output must be valid JSON; got {stdout}\nerr: {e}\nstderr: {stderr}"));
    let tasks = json["tasks"].as_array().expect("tasks array must exist");
    assert_eq!(
        tasks.len(),
        2,
        "two non-comment tasks should yield two task entries; got {}",
        tasks.len()
    );
}

// ── AC3: human-readable preview fields ───────────────────────────────────────

#[test]
fn agent_skills_preview_text_output_shows_required_fields() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "security-review",
        "---\nname: security-review\ndescription: Use for security review.\nversion: 2.0.0\n---\n\n# Security\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "Use $security-review for this task",
            "--config",
            &config_path.display().to_string(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr:\n{stderr}");

    // (1) task_hash present (12-char hex prefix)
    assert!(
        stdout.contains("task_hash") || stdout.contains("hash"),
        "output must show task hash\nstdout:\n{stdout}"
    );
    // (2) skill name present
    assert!(
        stdout.contains("security-review"),
        "output must show activated skill name\nstdout:\n{stdout}"
    );
    // (3) total_bytes_injected
    assert!(
        stdout.contains("total_bytes") || stdout.contains("bytes"),
        "output must show total bytes injected\nstdout:\n{stdout}"
    );
    // (4) max_active_cap_hit
    assert!(
        stdout.contains("cap_hit") || stdout.contains("cap"),
        "output must show cap hit status\nstdout:\n{stdout}"
    );
}

// ── AC4: skills disabled / no paths configured ───────────────────────────────

#[test]
fn agent_skills_preview_exits_zero_skills_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("disabled.toml");
    fs::write(&config_path, "[skills]\nenabled = false\n").unwrap();

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "any task",
            "--config",
            &config_path.display().to_string(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "must exit 0 when skills disabled\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("disabled") || stdout.contains("skills disabled"),
        "output must indicate skills are disabled\nstdout:\n{stdout}"
    );
}

#[test]
fn agent_skills_preview_exits_zero_no_paths() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("nopaths.toml");
    fs::write(&config_path, "[skills]\nenabled = true\npaths = []\n").unwrap();

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "any task",
            "--config",
            &config_path.display().to_string(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "must exit 0 when no paths configured\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("no skill paths") || stdout.contains("paths"),
        "output must indicate no paths configured\nstdout:\n{stdout}"
    );
}

// ── AC5: --format json emits schema-versioned object ─────────────────────────

#[test]
fn agent_skills_preview_json_is_valid() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix a Rust borrow checker issue",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("must produce valid JSON; got: {stdout}"));
    assert!(v.is_object(), "JSON must be an object");
}

#[test]
fn agent_skills_preview_json_has_artifact_kind() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust code",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        v["artifact_kind"].as_str(),
        Some("skills_preview"),
        "artifact_kind must be 'skills_preview'; got: {}",
        v["artifact_kind"]
    );
}

#[test]
fn agent_skills_preview_json_has_schema_version() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust code",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        v["schema_version"].is_object() || v["schema_version"].is_string(),
        "JSON must have schema_version; got: {v}"
    );
}

#[test]
fn agent_skills_preview_json_has_tasks_and_summary() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust code",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        v["tasks"].is_array(),
        "JSON must have 'tasks' array; got: {v}"
    );
    assert!(
        v["summary"].is_object(),
        "JSON must have 'summary' object; got: {v}"
    );
}

#[test]
fn agent_skills_preview_json_task_entry_has_required_fields() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust borrow checker",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let tasks = v["tasks"].as_array().unwrap();
    assert!(!tasks.is_empty(), "must have at least one task entry");
    let task = &tasks[0];
    for field in [
        "task_hash",
        "active_skills",
        "total_bytes_injected",
        "max_active_cap_hit",
        "dropped_count",
        "merged_extra_context_bytes",
    ] {
        assert!(
            task.get(field).is_some(),
            "task entry missing required field '{field}'; got: {task}"
        );
    }
}

#[test]
fn agent_skills_preview_json_skill_entry_has_required_fields() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "Use $rust-router to fix this",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let tasks = v["tasks"].as_array().unwrap();
    let task = &tasks[0];
    let skills = task["active_skills"].as_array().unwrap();
    assert!(!skills.is_empty(), "rust-router should activate for this task");
    let skill = &skills[0];
    for field in ["name", "reason", "sha256_prefix", "bytes", "path"] {
        assert!(
            skill.get(field).is_some(),
            "skill entry missing required field '{field}'; got: {skill}"
        );
    }
}

#[test]
fn agent_skills_preview_json_summary_has_required_fields() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust code",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let summary = &v["summary"];
    for field in [
        "task_count",
        "unique_skills_activated",
        "p50_bytes_per_task",
        "p95_bytes_per_task",
        "tasks_hitting_max_active",
    ] {
        assert!(
            summary.get(field).is_some(),
            "summary missing required field '{field}'; got: {summary}"
        );
    }
}

#[test]
fn agent_skills_preview_task_hash_is_12_char_hex() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust borrow checker",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let task_hash = v["tasks"][0]["task_hash"].as_str().unwrap();
    assert_eq!(
        task_hash.len(),
        12,
        "task_hash must be 12 chars; got '{task_hash}'"
    );
    assert!(
        task_hash.chars().all(|c| c.is_ascii_hexdigit()),
        "task_hash must be hex; got '{task_hash}'"
    );
}

#[test]
fn agent_skills_preview_skill_sha256_prefix_is_12_char_hex() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "Use $rust-router to fix this borrow checker",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let skill = &v["tasks"][0]["active_skills"][0];
    let sha256_prefix = skill["sha256_prefix"].as_str().unwrap();
    assert_eq!(
        sha256_prefix.len(),
        12,
        "sha256_prefix must be 12 chars; got '{sha256_prefix}'"
    );
    assert!(
        sha256_prefix.chars().all(|c| c.is_ascii_hexdigit()),
        "sha256_prefix must be hex; got '{sha256_prefix}'"
    );
}

// ── AC5: explicit vs auto activation reason ───────────────────────────────────

#[test]
fn agent_skills_preview_explicit_activation_reason() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "Use $rust-router to fix this",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let skill = &v["tasks"][0]["active_skills"][0];
    assert_eq!(
        skill["reason"].as_str(),
        Some("explicit_mention"),
        "explicit $skill-name should give reason=explicit_mention; got: {skill}"
    );
}

#[test]
fn agent_skills_preview_auto_activation_reason() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "security-review",
        "---\nname: security-review\ndescription: Use for security review work.\nversion: 1.0.0\n---\n\n# Security\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "please perform a security review of this code",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let skill = &v["tasks"][0]["active_skills"][0];
    assert_eq!(
        skill["reason"].as_str(),
        Some("auto_match"),
        "auto-matched skill should give reason=auto_match; got: {skill}"
    );
}

// ── AC6: exit code 2 on bad/missing flags ─────────────────────────────────────

#[test]
fn agent_skills_preview_exit_2_on_missing_task() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("cfg.toml");
    fs::write(&config_path, "[skills]\nenabled = false\n").unwrap();

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--config",
            &config_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "must exit non-zero when no --task or --task-file given"
    );
    let exit_code = out.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code, 2,
        "must exit 2 (usage_error) when no --task or --task-file given; got {exit_code}"
    );
}

// ── AC6: skills_preview_warning exit code (14) ───────────────────────────────

#[test]
fn agent_skills_preview_exit_13_when_max_active_cap_hit() {
    let temp = tempfile::tempdir().unwrap();
    // Create 3 skills that all auto-match, but set max_active = 1
    for name in ["alpha-rust", "beta-rust", "gamma-rust"] {
        write_skill(
            temp.path(),
            name,
            &format!(
                "---\nname: {name}\ndescription: Use for Rust code work.\nversion: 1.0.0\n---\n\n# {name}\n"
            ),
        );
    }
    let config_path = temp.path().join("cap.toml");
    fs::write(
        &config_path,
        format!(
            "[skills]\nenabled = true\nauto_load = true\nmax_active = 1\npaths = [\"{}\"]\n",
            toml_path(temp.path())
        ),
    )
    .unwrap();

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust code in this project",
            "--config",
            &config_path.display().to_string(),
        ])
        .output()
        .unwrap();
    let exit_code = out.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code, 14,
        "must exit 14 (skills_preview_warning) when max_active cap is hit; got {exit_code}"
    );
}

#[test]
fn agent_skills_preview_exit_13_when_manifest_missing_version() {
    let temp = tempfile::tempdir().unwrap();
    // Description has enough tokens to auto-match the task below (score >= 2)
    write_skill(
        temp.path(),
        "no-version",
        "---\nname: no-version\ndescription: Use for Rust borrow checker debugging.\n---\n\n# No Version\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust borrow checker issue here",
            "--config",
            &config_path.display().to_string(),
        ])
        .output()
        .unwrap();
    let exit_code = out.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code, 14,
        "must exit 14 (skills_preview_warning) when an activated manifest has no version field; got {exit_code}"
    );
}

// ── AC7: output flows through redactor ───────────────────────────────────────

#[test]
fn agent_skills_preview_redacts_fake_api_key_in_task() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = temp.path().join("redact.toml");
    let skill_path = toml_path(temp.path());
    // Use a synthetic secret literal that should be redacted
    fs::write(
        &config_path,
        format!(
            "[skills]\nenabled = true\nauto_load = true\npaths = [\"{skill_path}\"]\n\
             [redaction]\nsecret_literals = [\"sk-deadbeef\"]\n"
        ),
    )
    .unwrap();

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust code FAKE_API_KEY=sk-deadbeef here",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The preview must not leak the secret literal verbatim
    assert!(
        !stdout.contains("sk-deadbeef"),
        "stdout must not contain the secret literal verbatim\nstdout:\n{stdout}"
    );
    assert!(
        !stderr.contains("sk-deadbeef"),
        "stderr must not contain the secret literal verbatim\nstderr:\n{stderr}"
    );
}

// ── AC8: bench doctor integration ────────────────────────────────────────────

#[test]
fn bench_doctor_includes_skills_preview_section_when_skills_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let output_dir = temp.path().join("out");
    fs::create_dir_all(&output_dir).unwrap();
    let dataset = temp.path().join("data.jsonl");
    fs::write(
        &dataset,
        "{\"instance_id\":\"test-1\",\"problem_statement\":\"fix the bug\"}\n",
    )
    .unwrap();

    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = temp.path().join("doc.toml");
    fs::write(
        &config_path,
        format!(
            "[skills]\nenabled = true\nauto_load = true\npaths = [\"{}\"]\n",
            toml_path(temp.path())
        ),
    )
    .unwrap();

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "bench",
            "doctor",
            "--dataset-path",
            &dataset.display().to_string(),
            "--output",
            &output_dir.display().to_string(),
            "--config",
            &config_path.display().to_string(),
            "--model",
            "claude-opus-4-7",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // bench doctor may exit non-zero if some checks fail, but the skills section
    // should still be printed
    assert!(
        stdout.contains("skills") || stdout.contains("skill"),
        "bench doctor stdout must include skills-preview section when skills enabled\nstdout:\n{stdout}"
    );
}

// ── AC9: spec doc exists ──────────────────────────────────────────────────────

#[test]
fn spec_skills_preview_doc_exists() {
    assert!(
        std::path::Path::new("docs/spec-skills-preview.md").exists(),
        "docs/spec-skills-preview.md must exist"
    );
}

#[test]
fn spec_skills_preview_doc_has_json_schema_example_and_activation_types() {
    let doc = std::fs::read_to_string("docs/spec-skills-preview.md")
        .expect("docs/spec-skills-preview.md must be readable");
    assert!(
        doc.contains("ExplicitMention") || doc.contains("explicit_mention"),
        "spec doc must document ExplicitMention activation"
    );
    assert!(
        doc.contains("AutoMatch") || doc.contains("auto_match"),
        "spec doc must document AutoMatch activation"
    );
    assert!(
        doc.contains("skills_preview"),
        "spec doc must document skills_preview artifact kind"
    );
}

#[test]
fn readme_links_to_spec_skills_preview() {
    let readme = std::fs::read_to_string("README.md").expect("README.md must be readable");
    assert!(
        readme.contains("spec-skills-preview"),
        "README.md must link to spec-skills-preview.md in Advanced Specs"
    );
}

// ── AC10: exit-codes.md documents new exit code ───────────────────────────────

#[test]
fn exit_codes_doc_has_skills_preview_warning() {
    let doc = std::fs::read_to_string("docs/exit-codes.md")
        .expect("docs/exit-codes.md must be readable");
    assert!(
        doc.contains("skills_preview_warning"),
        "docs/exit-codes.md must document the skills_preview_warning exit code"
    );
    assert!(
        doc.contains("14"),
        "docs/exit-codes.md must document code 14 (skills_preview_warning)"
    );
}

// ── Backwards-compatibility unit test for JSON schema ─────────────────────────

#[test]
fn agent_skills_preview_json_additive_only_schema_compat() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(
        temp.path(),
        "rust-router",
        "---\nname: rust-router\ndescription: Use for Rust work.\nversion: 1.0.0\n---\n\n# Rust\n",
    );
    let config_path = write_skill_config(temp.path(), temp.path());

    let out = Command::new(binary())
        .args([
            "--log",
            "error",
            "agent",
            "skills-preview",
            "--task",
            "fix Rust borrow checker",
            "--config",
            &config_path.display().to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    // All originally-specified fields must always be present (additive-only contract)
    let top_level_required = ["artifact_kind", "schema_version", "tasks", "summary"];
    for field in top_level_required {
        assert!(
            v.get(field).is_some(),
            "required top-level field '{field}' missing in schema; got: {v}"
        );
    }
    let task_required = [
        "task_hash",
        "active_skills",
        "total_bytes_injected",
        "max_active_cap_hit",
        "dropped_count",
        "merged_extra_context_bytes",
    ];
    for field in task_required {
        assert!(
            v["tasks"][0].get(field).is_some(),
            "required task field '{field}' missing; got: {}",
            v["tasks"][0]
        );
    }
    let summary_required = [
        "task_count",
        "unique_skills_activated",
        "p50_bytes_per_task",
        "p95_bytes_per_task",
        "tasks_hitting_max_active",
    ];
    for field in summary_required {
        assert!(
            v["summary"].get(field).is_some(),
            "required summary field '{field}' missing; got: {}",
            v["summary"]
        );
    }
}
