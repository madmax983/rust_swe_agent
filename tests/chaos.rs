//! Integration: `--chaos-fail-every` deterministic fault injection (issue #340).
//!
//! Exercises a full `mini` run wired with `chaos_fail_every = 2` against the
//! scripted deterministic model and asserts:
//!   (a) injected timeouts appear at the expected step indices,
//!   (b) the agent loop terminates cleanly rather than panicking,
//!   (c) running twice produces byte-identical trajectories modulo timestamps.

#![allow(clippy::unwrap_used)]

use maxwells_daemon::Config;
use maxwells_daemon::run::mini::{InteractiveMode, MiniArgs, run as mini_run};

fn chaos_args(cfg: Config, output: std::path::PathBuf, name: &str) -> MiniArgs {
    // Four bash actions then submit. With fail_every = 2 the 2nd and 4th
    // environment invocations are deterministically replaced with timeouts.
    let responses = vec![
        "```bash\necho alpha\n```".into(),
        "```bash\necho bravo\n```".into(),
        "```bash\necho charlie\n```".into(),
        "```bash\necho delta\n```".into(),
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```".into(),
    ];
    MiniArgs {
        driver: maxwells_daemon::run::mini::RunDriver::Builtin,
        driver_append_system_prompt: false,
        driver_isolated: false,
        task: "exercise chaos".into(),
        extra_context: None,
        config: cfg,
        output_dir: output,
        trajectory_name: name.into(),
        deterministic_responses: Some(responses),
        deterministic_usage_per_call: None,
        task_timeout_secs: None,
        cancellation: None,
        stream_addr: None,
        patch_capture: None,
        verification_checks: vec![],
        verification_timeout_secs: 60,
        interactive_mode: InteractiveMode::Off,
        no_bell: false,
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
        issue_provenance: None,
    }
}

/// Collect, in order, the `chaos_injected` flag for every bash observation
/// (user message carrying a `run_result`).
fn injected_flags(traj: &serde_json::Value) -> Vec<bool> {
    traj["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["extra"].get("run_result"))
        .map(|rr| {
            rr.get("chaos_injected")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .collect()
}

/// Recursively null out fields that legitimately vary between two otherwise
/// identical runs (wall-clock timestamps, measured latencies, durations).
fn scrub_volatile(value: &mut serde_json::Value) {
    const VOLATILE: &[&str] = &[
        "timestamp",
        "started_at",
        "ended_at",
        "started_at_utc",
        "ended_at_utc",
        "duration_secs",
        "model_latency_ms",
        "tool_latency_ms",
        "harness_overhead_ms",
        "trace_id",
    ];
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if VOLATILE.contains(&k.as_str()) {
                    *v = serde_json::Value::Null;
                } else {
                    scrub_volatile(v);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                scrub_volatile(item);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn chaos_fail_every_two_injects_at_expected_indices_and_terminates_cleanly() {
    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 10;
    cfg.root.environment.chaos_fail_every = 2;

    mini_run(chaos_args(cfg, output.clone(), "chaos-run"))
        .await
        .unwrap();

    let path = output.join("chaos-run.traj.json");
    assert!(path.exists(), "trajectory should be written");
    let traj: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

    // (a) injected timeouts at the expected indices: 2nd and 4th bash steps.
    let flags = injected_flags(&traj);
    assert_eq!(
        flags,
        vec![false, true, false, true],
        "fail_every=2 should inject at the 2nd and 4th env invocations; got {flags:?}"
    );

    // The injected observations are genuine timeouts.
    let injected: Vec<&serde_json::Value> = traj["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["extra"].get("run_result"))
        .filter(|rr| rr["chaos_injected"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(injected.len(), 2);
    for rr in injected {
        assert_eq!(rr["timed_out"].as_bool(), Some(true));
    }

    // (b) clean termination: the run submitted rather than panicking.
    assert_eq!(traj["info"]["outcome"].as_str(), Some("submitted"));
    assert!(!traj["info"]["partial"].as_bool().unwrap_or(false));

    // The manifest records chaos_fail_every as a first-class field.
    assert_eq!(
        traj["info"]["manifest"]["chaos_fail_every"].as_u64(),
        Some(2)
    );
}

#[tokio::test]
async fn bench_inspect_reports_chaos_counts() {
    use maxwells_daemon::run::inspect::{InspectArgs, InspectOutput, run as inspect_run};

    let work = tempfile::tempdir().unwrap();
    let output = work.path().join("runs");

    let mut cfg = Config::defaults().unwrap();
    cfg.root.agent.step_limit = 10;
    cfg.root.environment.chaos_fail_every = 2;
    mini_run(chaos_args(cfg, output.clone(), "inst-1"))
        .await
        .unwrap();

    // Per-instance: two injected timeouts (2nd and 4th bash), one recovery
    // (the step after the first injection succeeds; the second injection is
    // the last bash command before submit, so no following step to recover).
    let out = inspect_run(&InspectArgs {
        sweep: output.clone(),
        instance: Some("inst-1".into()),
        filter: None,
        full: false,
        show_expected: false,
        flake_report: None,
    })
    .unwrap();
    let InspectOutput::Instance(report) = out else {
        panic!("expected instance report");
    };
    assert_eq!(report.chaos_injected_steps, 2);
    assert_eq!(report.chaos_recoveries, 1);
}

#[tokio::test]
async fn chaos_run_is_reproducible_modulo_timestamps() {
    let work = tempfile::tempdir().unwrap();

    let run_once = |dir: std::path::PathBuf| async move {
        let mut cfg = Config::defaults().unwrap();
        cfg.root.agent.step_limit = 10;
        cfg.root.environment.chaos_fail_every = 2;
        mini_run(chaos_args(cfg, dir.clone(), "repro"))
            .await
            .unwrap();
        let path = dir.join("repro.traj.json");
        let mut traj: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        scrub_volatile(&mut traj);
        traj
    };

    let a = run_once(work.path().join("a")).await;
    let b = run_once(work.path().join("b")).await;
    assert_eq!(
        a, b,
        "two chaos runs must be byte-identical after scrubbing volatile fields"
    );
}
