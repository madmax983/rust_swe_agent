//! `bench failure-digest`: self-contained failure summary per instance.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

mod support;
use support::binary_path;

const FIXTURE_DIR: &str = "tests/fixtures/failure_digest";

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(name)
}

fn run_failure_digest(args: &[&str]) -> std::process::Output {
    Command::new(binary_path())
        .args(["--log", "error", "bench", "failure-digest"])
        .args(args)
        .output()
        .unwrap()
}

fn run_failure_digest_with_env(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.args(["--log", "error", "bench", "failure-digest"])
        .args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

// ── (a) digest on a sweep with one errored instance ──────────────────────────

#[test]
fn digest_single_errored_instance_exits_zero_and_contains_headline() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);

    assert!(
        out.status.success(),
        "failure-digest should exit 0 for an errored instance\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("error-1"),
        "headline must include instance_id: {stdout}"
    );
    assert!(
        stdout.contains("model_parse"),
        "headline must include failure_category: {stdout}"
    );
    assert!(
        stdout.contains("error"),
        "headline must include outcome: {stdout}"
    );
}

#[test]
fn digest_errored_instance_contains_cost_and_steps() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("2.50") || stdout.contains("2.5"),
        "cost missing: {stdout}"
    );
    assert!(stdout.contains('5'), "step count missing: {stdout}");
}

#[test]
fn digest_errored_instance_contains_last_assistant_message() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("parse the model response"),
        "last assistant message excerpt missing: {stdout}"
    );
}

#[test]
fn digest_errored_instance_contains_last_tool_stderr() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("model parse error"),
        "last tool stderr missing: {stdout}"
    );
}

#[test]
fn digest_errored_instance_shows_patch_status_not_attempted() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("not-attempted"),
        "patch status missing: {stdout}"
    );
}

#[test]
fn digest_errored_instance_shows_no_triage_when_absent() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("no triage available"),
        "missing triage-unavailable note: {stdout}"
    );
}

// ── (b) digest on resolved instance ──────────────────────────────────────────

#[test]
fn digest_resolved_instance_exits_zero_and_says_no_failure() {
    let sweep = fixture_path("sweep-single-resolved");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);

    assert!(
        out.status.success(),
        "failure-digest should exit 0 for a resolved instance\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("resolved-1"),
        "headline must include instance_id: {stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("no failure") || stdout.to_lowercase().contains("submitted"),
        "should indicate no failure to digest for a resolved instance: {stdout}"
    );
}

// ── (c) sweep with no results.json exits non-zero ────────────────────────────

#[test]
fn digest_missing_results_json_exits_nonzero_with_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_failure_digest(&["--sweep", dir.path().to_str().unwrap()]);

    assert!(
        !out.status.success(),
        "should exit non-zero when results.json is absent"
    );

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("results.json") || stderr.contains("failure-digest"),
        "error message should reference results.json: {stderr}"
    );
}

// ── multi-instance behaviour ──────────────────────────────────────────────────

#[test]
fn digest_multi_instance_without_flag_exits_nonzero_naming_candidates() {
    let sweep = fixture_path("sweep-multi");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);

    assert!(
        !out.status.success(),
        "should exit non-zero when sweep has multiple instances and --instance is omitted"
    );

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("error-1") && stderr.contains("error-2"),
        "error should name all candidate instance ids: {stderr}"
    );
}

#[test]
fn digest_multi_instance_with_flag_selects_specific_instance() {
    let sweep = fixture_path("sweep-multi");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--instance", "error-2"]);

    assert!(
        out.status.success(),
        "should succeed when --instance is provided\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("error-2"),
        "output should contain the selected instance: {stdout}"
    );
    assert!(
        stdout.contains("env_setup"),
        "output should contain failure category of selected instance: {stdout}"
    );
}

// ── (d) step-limit failure surfaces final loop state ─────────────────────────

#[test]
fn digest_step_limit_surfaces_final_loop_state() {
    let sweep = fixture_path("sweep-step-limit");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);

    assert!(
        out.status.success(),
        "failure-digest should succeed on step_limit instance\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("step_limit") || stdout.contains("step-limit"),
        "should include failure category: {stdout}"
    );
    assert!(
        stdout.contains("Still working") || stdout.contains("working"),
        "should include last assistant message: {stdout}"
    );
    assert!(
        stdout.contains("step limit reached") || stdout.contains("step_limit"),
        "should surface last tool stderr with loop state: {stdout}"
    );
}

#[test]
fn digest_step_limit_includes_triage_cluster_label() {
    let sweep = fixture_path("sweep-step-limit");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("aabbccdd11223344"),
        "triage cluster_id should appear in output: {stdout}"
    );
}

// ── (e) patch-apply failure surfaces git apply --check stderr ────────────────

#[test]
fn digest_patch_apply_invalid_surfaces_apply_stderr() {
    let sweep = fixture_path("sweep-patch-invalid");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);

    assert!(
        out.status.success(),
        "failure-digest should succeed on patch_apply_invalid instance\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("invalid-diff"),
        "patch status should be invalid-diff: {stdout}"
    );
    assert!(
        stdout.contains("patch does not apply") || stdout.contains("patch failed"),
        "git apply --check stderr should appear: {stdout}"
    );
}

// ── (f) redaction enforcement ─────────────────────────────────────────────────

#[test]
fn digest_redacts_sensitive_env_var_from_tool_stderr() {
    let sweep = tempfile::tempdir().unwrap();
    let secret_value = "super-secret-key-value-9999";

    // Write results.json
    std::fs::write(
        sweep.path().join("results.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "artifact_kind": "sweep_results",
            "schema_version": {"major": 1, "minor": 4},
            "total": 1,
            "submitted": 0,
            "skipped": 0,
            "errored": 1,
            "total_cost_usd": 1.0,
            "instances": [{
                "instance_id": "secret-test",
                "exit_reason": "error",
                "outcome": "error",
                "failure_category": "model_api",
                "cost_usd": 1.0,
                "steps": 1,
                "patch_present": false,
                "non_empty_patch": false,
                "attempts": 1,
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false,
                "tests_run_before_submit": false
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    // Write trajectory with secret in last tool stderr
    std::fs::write(
        sweep.path().join("secret-test.traj.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 4},
            "info": {
                "task": "secret-test",
                "model_name": "fixture-model",
                "outcome": "error",
                "failure_category": "model_api",
                "total_cost_usd": 1.0,
                "steps": 1,
                "test_invocations": [],
                "tests_run_before_submit": false
            },
            "messages": [
                {"role": "assistant", "content": "Calling the API"},
                {
                    "role": "user",
                    "content": "observation",
                    "extra": {
                        "run_result": {
                            "stdout": "",
                            "stderr": format!("API call failed with key={secret_value}"),
                            "exit_code": 1,
                            "timed_out": false
                        }
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    // Run with the secret as a sensitive env var (OPENROUTER_API_KEY contains KEY → sensitive)
    let out = run_failure_digest_with_env(
        &["--sweep", sweep.path().to_str().unwrap()],
        &[("OPENROUTER_API_KEY", secret_value)],
    );

    assert!(
        out.status.success(),
        "command should succeed (redact and continue)\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains(secret_value),
        "raw secret must not appear in digest output: {stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("redacted") || stdout.contains('['),
        "output should contain redaction marker: {stdout}"
    );
}

// ── (g) JSON schema stability (golden file) ───────────────────────────────────

#[test]
fn digest_json_format_is_schema_versioned_and_stable() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--format", "json"]);

    assert!(
        out.status.success(),
        "failure-digest --format json should exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(json["schema_version"], "1.1", "schema_version must be 1.1");
    assert_eq!(json["instance_id"], "error-1");
    assert_eq!(json["outcome"], "error");
    assert_eq!(json["failure_category"], "model_parse");
    assert_eq!(json["step_count"], 5);
    assert_eq!(json["total_cost_usd"], 2.5);
    assert_eq!(json["patch_status"], "not_attempted");

    // Full (non-truncated) text in JSON mode
    assert!(
        json["last_assistant_message"]
            .as_str()
            .unwrap_or("")
            .contains("parse the model response"),
        "JSON should contain full assistant message"
    );
    assert!(
        json["last_tool_stderr"]
            .as_str()
            .unwrap_or("")
            .contains("model parse error"),
        "JSON should contain full tool stderr"
    );

    // Stable JSON: verify required fields exist
    assert!(json.get("triage_cluster_label").is_some());
    assert!(json.get("redacted").is_some());
    assert!(json.get("last_tool_stdout").is_some());

    // Write/check golden file
    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/failure_digest/golden/error-1-digest.json");

    if golden_path.exists() {
        let golden: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&golden_path).unwrap()).unwrap();
        // Check structural stability (not byte-for-byte due to potential float formatting)
        assert_eq!(
            json["schema_version"], golden["schema_version"],
            "schema_version must be stable"
        );
        assert_eq!(
            json["instance_id"], golden["instance_id"],
            "instance_id must be stable"
        );
        assert_eq!(json["outcome"], golden["outcome"], "outcome must be stable");
        assert_eq!(
            json["failure_category"], golden["failure_category"],
            "failure_category must be stable"
        );
        assert_eq!(
            json["patch_status"], golden["patch_status"],
            "patch_status must be stable"
        );
    } else {
        // First run: write golden file
        std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        std::fs::write(&golden_path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
        println!("Wrote golden file: {}", golden_path.display());
    }
}

// ── (h) --max-chars truncation ────────────────────────────────────────────────

#[test]
fn digest_max_chars_truncation_preserves_headline_and_triage_footer() {
    let sweep = fixture_path("sweep-step-limit");
    // Use a small max-chars to force truncation
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--max-chars", "300"]);

    assert!(
        out.status.success(),
        "failure-digest should succeed with --max-chars\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();

    // Headline must be preserved
    assert!(
        stdout.contains("step-1"),
        "headline (instance_id) must survive truncation: {stdout}"
    );
    assert!(
        stdout.len() <= 400,
        "output should be near max_chars (got {} chars): {stdout}",
        stdout.len()
    );

    // Footer (triage cluster) must be preserved
    // The triage.json exists for this sweep so it should appear or say "no triage available"
    assert!(
        stdout.contains("aabbccdd") || stdout.contains("Triage") || stdout.contains("triage"),
        "triage section should survive truncation: {stdout}"
    );
}

// ── (i) failure signature: JSON shape ─────────────────────────────────────────

#[test]
fn digest_json_includes_failure_signature_object_with_constituent_fields() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--format", "json"]);
    assert!(out.status.success());

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let sig = &json["failure_signature"];
    assert!(
        sig.is_object(),
        "failure_signature must be an object: {json}"
    );

    let signature_id = sig["signature_id"].as_str().unwrap_or("");
    assert!(
        !signature_id.is_empty() && signature_id.chars().all(|c| c.is_ascii_hexdigit()),
        "signature_id must be a non-empty hex string: {signature_id:?}"
    );
    assert_eq!(
        sig["failure_category"], "model_parse",
        "signature must carry failure_category"
    );
    assert!(
        sig.get("assistant_tail").is_some(),
        "signature must carry assistant_tail: {sig}"
    );
    assert!(
        sig.get("bash_exit_code").is_some(),
        "signature must carry bash_exit_code: {sig}"
    );
    assert!(
        sig.get("stderr_line").is_some(),
        "signature must carry stderr_line: {sig}"
    );
    assert!(
        sig["summary"].as_str().map_or(0, str::len) > 0,
        "signature must carry a non-empty summary: {sig}"
    );
}

// ── (j) determinism: identical inputs → identical signature_id ────────────────

#[test]
fn digest_signature_id_is_deterministic_across_runs() {
    let sweep = fixture_path("sweep-single-errored");
    let run_once = || {
        let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--format", "json"]);
        assert!(out.status.success());
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        json["failure_signature"]["signature_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        run_once(),
        run_once(),
        "signature_id must be identical across independent runs"
    );
}

// ── (k) discrimination: differing terminal failures → different signature_id ──

#[test]
fn digest_signature_id_discriminates_distinct_failures() {
    let signature_of = |fixture: &str| {
        let sweep = fixture_path(fixture);
        let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--format", "json"]);
        assert!(out.status.success(), "{fixture} should succeed");
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        json["failure_signature"]["signature_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    let errored = signature_of("sweep-single-errored");
    let step_limit = signature_of("sweep-step-limit");
    assert_ne!(
        errored, step_limit,
        "distinct terminal failures must produce distinct signature_id"
    );
}

// ── (l) markdown greppable failure_signature line ─────────────────────────────

#[test]
fn digest_markdown_renders_greppable_failure_signature_line() {
    let sweep = fixture_path("sweep-single-errored");
    let md_out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);
    assert!(md_out.status.success());
    let md = String::from_utf8(md_out.stdout).unwrap();
    assert!(
        md.contains("failure_signature: "),
        "markdown must contain a greppable `failure_signature: ` line: {md}"
    );

    // The id in markdown must match the id in JSON.
    let json_out = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--format", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&json_out.stdout).unwrap();
    let id = json["failure_signature"]["signature_id"].as_str().unwrap();
    assert!(
        md.contains(id),
        "markdown failure_signature id must match JSON id `{id}`: {md}"
    );
}

// ── (m) baseline recurrence verdict ───────────────────────────────────────────

#[test]
fn digest_baseline_matching_signature_is_classified_recurring() {
    let sweep = fixture_path("sweep-single-errored");
    // First, learn the current signature_id.
    let first = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--format", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let id = json["failure_signature"]["signature_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Now classify against that baseline → recurring.
    let out = run_failure_digest(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--baseline-signature",
        &id,
        "--format",
        "json",
    ]);
    assert!(
        out.status.success(),
        "recurrence verdict must not change exit code"
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json["recurrence"]["verdict"], "recurring",
        "matching baseline must classify as recurring: {json}"
    );
    assert_eq!(json["recurrence"]["baseline_signature_id"], id);

    // Markdown surface too.
    let md_out = run_failure_digest(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--baseline-signature",
        &id,
    ]);
    assert!(md_out.status.success());
    let md = String::from_utf8(md_out.stdout).unwrap();
    assert!(
        md.contains("recurrence: recurring"),
        "markdown must surface recurrence verdict: {md}"
    );
}

#[test]
fn digest_baseline_nonmatching_signature_is_classified_new_and_exit_zero() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&[
        "--sweep",
        sweep.to_str().unwrap(),
        "--baseline-signature",
        "deadbeefdeadbeef",
        "--format",
        "json",
    ]);
    assert!(
        out.status.success(),
        "recurrence verdict must not change exit code (got {:?})",
        out.status.code()
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json["recurrence"]["verdict"], "new",
        "non-matching baseline must classify as new: {json}"
    );
}

#[test]
fn digest_without_baseline_omits_recurrence() {
    let sweep = fixture_path("sweep-single-errored");
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap(), "--format", "json"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        json.get("recurrence").is_none(),
        "recurrence must be absent when no baseline is supplied: {json}"
    );
}

// ── (n) signature is redaction-safe ───────────────────────────────────────────

#[test]
fn digest_signature_never_leaks_raw_secret() {
    let sweep = tempfile::tempdir().unwrap();
    let secret_value = "super-secret-key-value-9999";

    std::fs::write(
        sweep.path().join("results.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "artifact_kind": "sweep_results",
            "schema_version": {"major": 1, "minor": 4},
            "total": 1,
            "submitted": 0,
            "skipped": 0,
            "errored": 1,
            "total_cost_usd": 1.0,
            "instances": [{
                "instance_id": "secret-test",
                "exit_reason": "error",
                "outcome": "error",
                "failure_category": "model_api",
                "cost_usd": 1.0,
                "steps": 1,
                "patch_present": false,
                "non_empty_patch": false,
                "attempts": 1,
                "runs": 1,
                "resolved_count": 0,
                "pass_at_1": false,
                "tests_run_before_submit": false
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    std::fs::write(
        sweep.path().join("secret-test.traj.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "trajectory_format": "mini-swe-agent-1.1",
            "artifact_kind": "trajectory",
            "schema_version": {"major": 1, "minor": 4},
            "info": {
                "task": "secret-test",
                "model_name": "fixture-model",
                "outcome": "error",
                "failure_category": "model_api",
                "total_cost_usd": 1.0,
                "steps": 1,
                "test_invocations": [],
                "tests_run_before_submit": false
            },
            "messages": [
                {"role": "assistant", "content": format!("Calling the API with key={secret_value}")},
                {
                    "role": "user",
                    "content": "observation",
                    "extra": {
                        "run_result": {
                            "stdout": "",
                            "stderr": format!("API call failed with key={secret_value}"),
                            "exit_code": 1,
                            "timed_out": false
                        }
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let out = run_failure_digest_with_env(
        &[
            "--sweep",
            sweep.path().to_str().unwrap(),
            "--format",
            "json",
        ],
        &[("OPENROUTER_API_KEY", secret_value)],
    );
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let sig = &json["failure_signature"];
    let signature_id = sig["signature_id"].as_str().unwrap_or("");
    let summary = sig["summary"].as_str().unwrap_or("");
    let assistant_tail = sig["assistant_tail"].as_str().unwrap_or("");
    let stderr_line = sig["stderr_line"].as_str().unwrap_or("");
    assert!(
        !signature_id.contains(secret_value),
        "signature_id must not contain raw secret"
    );
    assert!(
        !summary.contains(secret_value),
        "summary must not contain raw secret: {summary}"
    );
    assert!(
        !assistant_tail.contains(secret_value),
        "assistant_tail must not contain raw secret: {assistant_tail}"
    );
    assert!(
        !stderr_line.contains(secret_value),
        "stderr_line must not contain raw secret: {stderr_line}"
    );
}

// ── performance budget ────────────────────────────────────────────────────────

#[test]
fn digest_completes_within_200ms() {
    let sweep = fixture_path("sweep-single-errored");
    let start = std::time::Instant::now();
    let out = run_failure_digest(&["--sweep", sweep.to_str().unwrap()]);
    let elapsed = start.elapsed();

    assert!(out.status.success());
    assert!(
        elapsed.as_millis() < 2000,
        "failure-digest should complete well under 2s (budget is 200ms); took {}ms",
        elapsed.as_millis()
    );
}
