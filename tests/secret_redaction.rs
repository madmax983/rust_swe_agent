//! Secret redaction contract tests for trajectories, streams, exports, inspect,
//! and submission artifacts.

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use rust_swe_agent::agent::default::DefaultAgentBuilder;
use rust_swe_agent::env::RunRequest;
use rust_swe_agent::error::EnvError;
use rust_swe_agent::run::inspect::{InspectArgs, InspectOutput};
use rust_swe_agent::run::swebench::{
    SwebenchArgs, patch_path_for_run, run as run_sweep, trajectory_path_for_run,
};
use rust_swe_agent::stream::{BroadcastSink, StreamSink};
use rust_swe_agent::{
    Agent, Config, DeterministicModel, Environment, ExitReason, Model, RunResult,
};

#[derive(Debug, Clone)]
struct StaticEnvironment {
    result: RunResult,
}

#[async_trait]
impl Environment for StaticEnvironment {
    async fn run(&self, _req: RunRequest) -> Result<RunResult, EnvError> {
        Ok(self.result.clone())
    }
}

#[tokio::test]
async fn redacts_hundred_secret_fixture_from_trajectory_and_streams() {
    let secrets = synthetic_secret_corpus();
    assert_eq!(secrets.len(), 100);
    let custom_literal = "literal-secret-fixture-value";
    let custom_pattern_secret = "CUSTOMSECRET-042";

    let stdout = format!(
        "{}\nrepeat={}\ncustom={custom_literal}\npattern={custom_pattern_secret}\n",
        secrets[..50].join("\n"),
        secrets[0]
    );
    let stderr = format!("{}\nrepeat={}\n", secrets[50..99].join("\n"), secrets[0]);
    let assistant_command_secret = &secrets[99];
    let command = format!(
        "curl -H 'Authorization: Bearer {assistant_command_secret}' https://example.invalid"
    );

    let cfg = Config::from_toml_str(&format!(
        r#"
[agent]
step_limit = 5

[redaction]
secret_literals = ["{custom_literal}"]
custom_patterns = ["CUSTOMSECRET-[0-9]{{3}}"]
"#
    ))
    .unwrap();
    let bcast = Arc::new(BroadcastSink::default());
    let mut rx = bcast.subscribe();
    let model = Arc::new(DeterministicModel::new(vec![
        format!("```bash\n{command}\n```"),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let env: Box<dyn Environment> = Box::new(StaticEnvironment {
        result: RunResult {
            stdout,
            stderr,
            exit_code: 0,
            timed_out: false,
        },
    });
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: "redact all the sharp things".into(),
        extra_context: None,
        renderer: None,
        stream: Some(bcast as Arc<dyn StreamSink>),
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let trajectory_json = agent.trajectory.to_json_pretty().unwrap();
    assert_no_raw_values(
        "trajectory",
        &trajectory_json,
        secrets
            .iter()
            .map(String::as_str)
            .chain([custom_literal, custom_pattern_secret]),
    );
    assert!(
        trajectory_json.contains("[REDACTED:"),
        "trajectory should include stable redaction markers:\n{trajectory_json}"
    );
    assert!(
        trajectory_json.contains("\"redaction\""),
        "trajectory info should report redaction counts:\n{trajectory_json}"
    );
    assert!(
        trajectory_json.contains("\"stream\"") && trajectory_json.contains("\"trajectory\""),
        "redaction counts should be grouped by surface:\n{trajectory_json}"
    );

    let mut stream_json = String::new();
    while let Ok(event) = rx.try_recv() {
        writeln!(
            &mut stream_json,
            "{}",
            serde_json::to_string(&event).unwrap()
        )
        .unwrap();
    }
    assert_no_raw_values(
        "stream events",
        &stream_json,
        secrets
            .iter()
            .map(String::as_str)
            .chain([custom_literal, custom_pattern_secret]),
    );
    assert!(
        stream_json.contains("[REDACTED:"),
        "stream events should include redaction markers:\n{stream_json}"
    );

    let markers = markers_for_repeated_secret(&trajectory_json, &stream_json);
    assert!(
        markers.len() == 1,
        "same secret should have one stable marker within the run, got {markers:?}"
    );
}

#[test]
fn trajectory_task_metadata_redacts_configured_literals_on_build() {
    let configured_secret = "task-metadata-secret-value";
    let cfg = Config::from_toml_str(&format!(
        r#"
[redaction]
secret_literals = ["{configured_secret}"]
"#
    ))
    .unwrap();
    let model = Arc::new(DeterministicModel::new(Vec::new()));
    let env: Box<dyn Environment> = Box::new(StaticEnvironment {
        result: RunResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        },
    });

    let agent = DefaultAgentBuilder {
        config: cfg,
        model,
        env,
        task: format!("fix the leak with {configured_secret}"),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();

    let trajectory_json = agent.trajectory.to_json_pretty().unwrap();
    assert_no_raw_values(
        "trajectory task metadata",
        &trajectory_json,
        [configured_secret],
    );
    assert!(
        agent
            .trajectory
            .info
            .task
            .as_deref()
            .is_some_and(|task| task.contains("[REDACTED:configured_literal:")),
        "{trajectory_json}"
    );
}

#[tokio::test]
async fn rendered_observation_template_static_literals_are_redacted_before_history() {
    let configured_secret = "static-observation-template-secret";
    let mut cfg = Config::from_toml_str(&format!(
        r#"
[agent]
step_limit = 5

[redaction]
secret_literals = ["{configured_secret}"]
"#
    ))
    .unwrap();
    cfg.root.agent.observation_template = format!(
        "ci annotation: {configured_secret}\nExit code: {{{{ returncode }}}}\nOutput:\n{{{{ output }}}}"
    );

    let model = Arc::new(DeterministicModel::new(vec![
        "```bash\necho visible-output\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
    ]));
    let model_for_agent: Arc<dyn Model> = model.clone();
    let env: Box<dyn Environment> = Box::new(StaticEnvironment {
        result: RunResult {
            stdout: "visible-output\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        },
    });
    let mut agent = DefaultAgentBuilder {
        config: cfg,
        model: model_for_agent,
        env,
        task: "redact static observation template text".into(),
        extra_context: None,
        renderer: None,
        stream: None,
    }
    .build()
    .unwrap();

    let exit = agent.run().await.unwrap();
    assert!(matches!(exit, ExitReason::Submitted { .. }));

    let history_text = agent
        .history
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!history_text.contains(configured_secret), "{history_text}");
    assert!(
        history_text.contains("[REDACTED:configured_literal:"),
        "{history_text}"
    );

    let recorded_inputs = model.recorded_inputs();
    assert!(
        recorded_inputs.len() >= 2,
        "expected a second model query, got {recorded_inputs:#?}"
    );
    let second_prompt = recorded_inputs[1]
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !second_prompt.contains(configured_secret),
        "{second_prompt}"
    );
    assert!(
        second_prompt.contains("[REDACTED:configured_literal:"),
        "{second_prompt}"
    );
}

#[cfg(feature = "markdown-export")]
#[test]
fn markdown_export_redacts_raw_trajectory_content() {
    use rust_swe_agent::model::Message;
    use rust_swe_agent::trajectory::Trajectory;
    use rust_swe_agent::trajectory::export::{MarkdownExporter, TrajectoryExporter};

    let secret = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    let mut trajectory = Trajectory::new();
    trajectory.info.task = Some(format!("fix with {secret}"));
    trajectory.record_message(&Message::assistant(format!("```bash\necho {secret}\n```")));
    trajectory.record_message(&Message::user(format!("stdout: {secret}")));

    let exported = MarkdownExporter::export(&trajectory);
    assert!(!exported.contains(secret), "{exported}");
    assert!(exported.contains("[REDACTED:"), "{exported}");
}

#[tokio::test]
async fn configured_literal_in_patch_blocks_submission_and_predictions() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let base_commit = init_repo_with_file(&repo, "secret.txt", "before\n");
    let configured_secret = "literal-secret-fixture-value";
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();
    write_dataset(&dataset, "secret-leak", &base_commit);

    let edit_cmd = write_file_command(&repo.join("secret.txt"), configured_secret);
    let cfg = Config::from_toml_str(&format!(
        r#"
[environment]
workdir = "{}"

[model]
name = "scripted-test-model"

[redaction]
secret_literals = ["{configured_secret}"]
"#,
        toml_escape_path(&repo)
    ))
    .unwrap();

    let results = run_sweep(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        reruns: 1,
        config: cfg,
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: rust_swe_agent::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(vec![
            format!("```bash\n{edit_cmd}\n```"),
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```".into(),
        ]),
        deterministic_usage_per_call: None,
        config_overlay_paths: Vec::new(),
        dry_run: false,
        skip_preflight: true,
        preflight_format: "text".into(),
        skip_model_probe: true,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "test".into(),
        skip_patch_validation: true,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: 30,
        install_os_signal_handlers: false,
        cancellation_signals: None,
        github_pr: None,
    })
    .await
    .unwrap();

    assert_eq!(results.submitted, 0, "{results:#?}");
    assert_eq!(results.errored, 1, "{results:#?}");
    let failure = serde_json::to_value(results.instances[0].failure_category).unwrap();
    assert_eq!(failure, serde_json::json!("secret_leak_detected"));

    let trajectory_path = trajectory_path_for_run(&output, "secret-leak", 1);
    let trajectory_json = std::fs::read_to_string(&trajectory_path).unwrap();
    assert!(
        !trajectory_json.contains(configured_secret),
        "{trajectory_json}"
    );
    assert!(
        trajectory_json.contains("secret_leak_detected"),
        "{trajectory_json}"
    );

    let patch_text =
        std::fs::read_to_string(patch_path_for_run(&output, "secret-leak", 1)).unwrap_or_default();
    assert!(!patch_text.contains(configured_secret), "{patch_text}");

    let predictions = std::fs::read_to_string(output.join("all_preds.jsonl")).unwrap();
    assert!(
        !predictions.contains(configured_secret),
        "prediction artifact leaked configured literal:\n{predictions}"
    );
    assert!(
        predictions.trim().is_empty(),
        "blocked submissions should not emit prediction rows:\n{predictions}"
    );
}

#[test]
fn bench_inspect_redacts_legacy_raw_trajectory_and_warns() {
    let work = tempfile::tempdir().unwrap();
    let sweep = work.path().join("runs");
    let instance_dir = sweep.join("legacy-raw");
    std::fs::create_dir_all(&instance_dir).unwrap();
    let secret = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    let trajectory = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.1",
        "info": {
            "task": "legacy",
            "model_name": "model",
            "outcome": "submitted"
        },
        "messages": [
            {"role": "assistant", "content": format!("```bash\necho {secret}\n```"), "extra": {"actions": [format!("echo {secret}")]}},
            {
                "role": "user",
                "content": format!("Exit code: 0\nOutput:\n{secret}"),
                "extra": {
                    "other": {
                        "run_result": {
                            "stdout": secret,
                            "stderr": "",
                            "exit_code": 0,
                            "timed_out": false
                        }
                    }
                }
            }
        ]
    });
    std::fs::write(
        instance_dir.join("trajectory.json"),
        serde_json::to_string_pretty(&trajectory).unwrap(),
    )
    .unwrap();

    let output = rust_swe_agent::run::inspect::run(&InspectArgs {
        sweep,
        instance: Some("legacy-raw".into()),
        filter: None,
        full: true,
    })
    .unwrap();
    let text = rust_swe_agent::run::inspect::render_text(&output);
    assert!(!text.contains(secret), "{text}");
    assert!(text.contains("[REDACTED:"), "{text}");
    assert!(text.to_ascii_lowercase().contains("redacted"), "{text}");
    match output {
        InspectOutput::Instance(report) => {
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|warning| warning.to_ascii_lowercase().contains("redacted")),
                "{:#?}",
                report.warnings
            );
        }
        InspectOutput::Summary(_) => panic!("expected instance report"),
    }
}

fn synthetic_secret_corpus() -> Vec<String> {
    (0..100)
        .map(|i| format!("ghp_{i:036X}"))
        .collect::<Vec<_>>()
}

fn markers_for_repeated_secret(first: &str, second: &str) -> std::collections::BTreeSet<String> {
    let mut markers = std::collections::BTreeSet::new();
    for text in [first, second] {
        for (start, _) in text.match_indices("repeat=") {
            let rest = &text[start + "repeat=".len()..];
            let end = rest
                .find(['\\', '\n', '\r', '"', '\'', ',', '}'])
                .unwrap_or(rest.len());
            let marker = rest[..end].trim();
            if marker.starts_with("[REDACTED:") {
                markers.insert(marker.to_owned());
            }
        }
    }
    markers
}

fn assert_no_raw_values<'a>(label: &str, text: &str, values: impl IntoIterator<Item = &'a str>) {
    for value in values {
        assert!(
            !text.contains(value),
            "{label} leaked raw secret {value:?}:\n{text}"
        );
    }
}

fn init_repo_with_file(dir: &Path, filename: &str, contents: &str) -> String {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@test"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
    std::fs::write(dir.join(filename), contents).unwrap();
    git(dir, &["add", filename]);
    git(dir, &["commit", "-q", "-m", "base"]);
    git_stdout(dir, &["rev-parse", "HEAD"]).trim().to_owned()
}

fn write_dataset(path: &Path, instance_id: &str, base_commit: &str) {
    std::fs::write(
        path,
        format!(
            "{{\"instance_id\":\"{instance_id}\",\"problem_statement\":\"noop\",\"base_commit\":\"{base_commit}\"}}\n"
        ),
    )
    .unwrap();
}

fn write_file_command(path: &Path, contents: &str) -> String {
    if cfg!(windows) {
        format!(
            "powershell -NoProfile -Command \"Set-Content -LiteralPath '{}' -Value '{}'\"",
            path.display(),
            contents
        )
    } else {
        format!("printf '%s\\n' '{}' > '{}'", contents, path.display())
    }
}

fn toml_escape_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
