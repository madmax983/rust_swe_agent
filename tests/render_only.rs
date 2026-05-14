//! Integration tests for `--render-only` zero-cost prompt preview (issue #172).
//!
//! Red phase: these tests FAIL until the feature is implemented.

#![allow(clippy::unwrap_used)]

mod support;

use std::process::Command;

fn binary() -> std::path::PathBuf {
    support::binary_path()
}

/// Run `mini --render-only` with the default config and return (status, stdout, stderr).
fn run_mini_render_only(extra_args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let out = Command::new(binary())
        .args(["--log", "error", "mini", "--render-only", "--task", "hello world", "--model", "claude-opus-4-7"])
        .args(extra_args)
        .output()
        .expect("failed to spawn binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status, stdout, stderr)
}

// ── Acceptance Criteria 1: mini --render-only exits 0 with no network calls ─

#[test]
fn mini_render_only_exits_zero() {
    let (status, _stdout, stderr) = run_mini_render_only(&[]);
    assert!(
        status.success(),
        "expected exit 0 from --render-only; exit={}\nstderr:\n{stderr}",
        status.code().unwrap_or(-1)
    );
}

#[test]
fn mini_render_only_text_output_contains_required_sections() {
    let (status, stdout, stderr) = run_mini_render_only(&[]);
    assert!(status.success(), "stderr:\n{stderr}");
    assert!(
        stdout.contains("system_message") || stdout.contains("System message"),
        "output must contain system message section;\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("user_message") || stdout.contains("User message") || stdout.contains("Instance prompt"),
        "output must contain user message section;\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("tool") || stdout.contains("Tool"),
        "output must contain tools section;\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("token") || stdout.contains("Token"),
        "output must contain token count;\nstdout:\n{stdout}"
    );
}

#[test]
fn mini_render_only_does_not_write_trajectory() {
    let temp = tempfile::tempdir().unwrap();
    let out = Command::new(binary())
        .args([
            "--log", "error",
            "mini", "--render-only",
            "--task", "render only task",
            "--model", "claude-opus-4-7",
            "--output", &temp.path().display().to_string(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    let traj_files: Vec<_> = std::fs::read_dir(temp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "json")
        })
        .collect();
    assert!(
        traj_files.is_empty(),
        "--render-only must not write trajectory files; found: {:?}",
        traj_files.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

// ── Acceptance Criteria 3: --format json emits schema-versioned object ──────

#[test]
fn mini_render_only_json_format_is_valid_json() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(
        status.success(),
        "exit={}\nstderr:\n{stderr}",
        status.code().unwrap_or(-1)
    );
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("--format json output must be valid JSON");
    assert!(
        v.is_object(),
        "JSON output must be an object; got: {v:?}"
    );
}

#[test]
fn mini_render_only_json_has_schema_version() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        v["schema_version"].is_object() || v["schema_version"].is_string(),
        "JSON must have schema_version field; got: {v}"
    );
}

#[test]
fn mini_render_only_json_has_artifact_kind_render_only() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let kind = v["artifact_kind"].as_str().unwrap_or("");
    assert_eq!(kind, "render_only", "artifact_kind must be 'render_only'; got '{kind}'");
}

#[test]
fn mini_render_only_json_has_all_required_fields() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let required = [
        "artifact_kind",
        "schema_version",
        "model",
        "system_message",
        "user_message",
        "tools",
        "hooks",
        "initial_prompt_tokens",
        "context_window_tokens",
        "context_window_pct",
        "upper_bound_cost",
    ];
    for field in required {
        assert!(
            v.get(field).is_some(),
            "JSON output missing required field '{field}'; got: {v}"
        );
    }
}

#[test]
fn mini_render_only_json_tools_list_includes_bash() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let tools = v["tools"].as_array().expect("tools must be an array");
    assert!(
        tools.iter().any(|t| t["name"].as_str() == Some("bash")),
        "tools must include the built-in 'bash' tool; got: {tools:?}"
    );
}

#[test]
fn mini_render_only_json_initial_prompt_tokens_positive() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let tokens = v["initial_prompt_tokens"].as_u64().expect("initial_prompt_tokens must be u64");
    assert!(tokens > 0, "initial_prompt_tokens must be > 0; got {tokens}");
}

#[test]
fn mini_render_only_json_context_window_pct_between_0_and_100() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let pct = v["context_window_pct"].as_f64().expect("context_window_pct must be f64");
    assert!(
        (0.0..=100.0).contains(&pct),
        "context_window_pct must be in [0, 100]; got {pct}"
    );
}

#[test]
fn mini_render_only_json_upper_bound_cost_has_usd_and_caveat() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let cost = &v["upper_bound_cost"];
    assert!(
        cost["usd"].as_f64().is_some(),
        "upper_bound_cost.usd must be f64; got: {cost}"
    );
    assert!(
        cost["caveat"].as_str().is_some(),
        "upper_bound_cost.caveat must be a string; got: {cost}"
    );
}

// ── Acceptance Criteria 4: exit codes on failure ─────────────────────────────

#[test]
fn mini_render_only_broken_template_exits_nonzero() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("broken.toml");
    // Write a config with a broken Jinja template (unclosed block)
    std::fs::write(
        &config_path,
        "[prompts]\nsystem = \"Hello {{ unclosed_variable\"\n",
    )
    .unwrap();
    let out = Command::new(binary())
        .args([
            "--log", "error",
            "mini", "--render-only",
            "--task", "test",
            "--model", "claude-opus-4-7",
            "--config", &config_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "broken template must cause non-zero exit; got 0\nstdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ── Acceptance Criteria 7: mutual exclusivity ─────────────────────────────────

#[test]
fn mini_render_only_conflicts_with_per_task_budget_usd() {
    let out = Command::new(binary())
        .args([
            "--log", "error",
            "mini", "--render-only",
            "--task", "hello",
            "--model", "claude-opus-4-7",
            "--per-task-budget-usd", "1.0",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--render-only combined with --per-task-budget-usd must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("render-only") || stderr.contains("per-task-budget"),
        "error message must reference the conflicting flags;\nstderr:\n{stderr}"
    );
}

#[test]
fn mini_render_only_conflicts_with_task_timeout_secs() {
    let out = Command::new(binary())
        .args([
            "--log", "error",
            "mini", "--render-only",
            "--task", "hello",
            "--model", "claude-opus-4-7",
            "--task-timeout-secs", "60",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--render-only combined with --task-timeout-secs must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("render-only") || stderr.contains("task-timeout"),
        "error message must reference the conflicting flags;\nstderr:\n{stderr}"
    );
}

// ── Acceptance Criteria 5: deterministic JSON snapshot ───────────────────────

#[test]
fn mini_render_only_json_is_deterministic_across_invocations() {
    let (status1, out1, err1) = run_mini_render_only(&["--format", "json"]);
    let (status2, out2, err2) = run_mini_render_only(&["--format", "json"]);
    assert!(status1.success(), "first run failed:\n{err1}");
    assert!(status2.success(), "second run failed:\n{err2}");
    let v1: serde_json::Value = serde_json::from_str(&out1).unwrap();
    let v2: serde_json::Value = serde_json::from_str(&out2).unwrap();
    // Key structural fields must be identical across invocations.
    assert_eq!(
        v1["artifact_kind"], v2["artifact_kind"],
        "artifact_kind differs between runs"
    );
    assert_eq!(
        v1["schema_version"], v2["schema_version"],
        "schema_version differs between runs"
    );
    assert_eq!(
        v1["system_message"], v2["system_message"],
        "system_message differs between runs"
    );
    assert_eq!(
        v1["user_message"], v2["user_message"],
        "user_message differs between runs"
    );
    assert_eq!(v1["tools"], v2["tools"], "tools differ between runs");
    assert_eq!(
        v1["initial_prompt_tokens"], v2["initial_prompt_tokens"],
        "initial_prompt_tokens differs between runs"
    );
}

// ── README quickstart: render-only is documented ──────────────────────────────

#[test]
fn readme_documents_render_only_quickstart_example() {
    let readme = std::fs::read_to_string("README.md").unwrap();
    assert!(
        readme.contains("--render-only"),
        "README must document --render-only as a quickstart step"
    );
}

// ── hooks field in JSON output ────────────────────────────────────────────────

#[test]
fn mini_render_only_json_hooks_has_pre_and_post_fields() {
    let (status, stdout, stderr) = run_mini_render_only(&["--format", "json"]);
    assert!(status.success(), "stderr:\n{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let hooks = &v["hooks"];
    assert!(
        hooks.get("pre_tool_use").is_some(),
        "hooks must have pre_tool_use field; got: {hooks}"
    );
    assert!(
        hooks.get("post_tool_use").is_some(),
        "hooks must have post_tool_use field; got: {hooks}"
    );
}

// ── Extra context is rendered into user message ───────────────────────────────

#[test]
fn mini_render_only_json_includes_extra_context_in_user_message() {
    let out = Command::new(binary())
        .args([
            "--log", "error",
            "mini", "--render-only",
            "--task", "do something",
            "--model", "claude-opus-4-7",
            "--extra-context", "EXTRA_MARKER_XYZ123",
            "--format", "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "stderr:\n{stderr}"
    );
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let user_msg = v["user_message"].as_str().unwrap_or("");
    assert!(
        user_msg.contains("EXTRA_MARKER_XYZ123"),
        "user_message must contain extra context;\nuser_message:\n{user_msg}"
    );
}
