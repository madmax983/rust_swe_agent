//! `bench utilization`: report sweep concurrency efficiency from on-disk artifacts.
//!
//! Red/green/refactor TDD coverage for issue #504. These tests exercise the
//! library entry point (`compute` / `render_text`) and the CLI surface
//! (`max bench utilization`), including the exit-code gate.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::unwrap_used
)]

use std::path::Path;
use std::process::Command;

use maxwells_daemon::artifact::ArtifactKind;
use maxwells_daemon::run::swebench::{
    CliManifest, ConfigManifest, DatasetManifest, FilterSpec, HarnessManifest, InstanceResult,
    ModelManifest, PromptTemplateManifest, ProvenanceManifest, RuntimeManifest,
    SWEEP_STATUS_COMPLETED, SweepResults,
};
use maxwells_daemon::run::utilization::{UtilizationArgs, compute, render_text};
use maxwells_daemon::trajectory::outcome;

mod support;
use support::binary_path;

// ── helpers ────────────────────────────────────────────────────────────────

fn instance(id: &str, duration_secs: Option<f64>, attempts: u32, runs: u32) -> InstanceResult {
    InstanceResult {
        instance_id: id.into(),
        exit_reason: "submitted".into(),
        outcome: Some(outcome::SUBMITTED.into()),
        failure_category: None,
        steps: Some(5),
        cost_usd: Some(0.0),
        prompt_tokens: Some(0),
        cache_read_tokens: Some(0),
        cache_creation_tokens: Some(0),
        completion_tokens: Some(0),
        duration_secs,
        error: None,
        github_pr_error: None,
        patch_present: true,
        non_empty_patch: true,
        attempts,
        retry_reasons: Vec::new(),
        runs,
        resolved_count: 1,
        pass_at_1: true,
        tests_run_before_submit: false,
        last_tests_passed: None,
        fallback_count: None,
        final_model: None,
        retry_id: None,
        previous_failure_category: None,
        trace_id: None,
    }
}

#[derive(Clone, Copy)]
enum ParallelStyle {
    /// `--parallel N` (two argv tokens).
    SpaceSeparated,
    /// `--parallel=N` (single argv token).
    Equals,
    /// `-p N` (short flag).
    ShortDash,
}

struct SweepSpec<'a> {
    durations: &'a [Option<f64>],
    parallel_argv: Option<usize>,
    parallel_style: ParallelStyle,
    started: Option<&'a str>,
    finished: Option<&'a str>,
    attempts: u32,
    runs: u32,
    sweep_status: &'a str,
    resume: bool,
}

impl Default for SweepSpec<'_> {
    fn default() -> Self {
        Self {
            durations: &[],
            parallel_argv: Some(8),
            parallel_style: ParallelStyle::SpaceSeparated,
            started: Some("2026-05-01T00:00:00Z"),
            finished: Some("2026-05-01T00:10:00Z"),
            attempts: 1,
            runs: 1,
            sweep_status: SWEEP_STATUS_COMPLETED,
            resume: false,
        }
    }
}

fn write_sweep(dir: &Path, spec: &SweepSpec) {
    let instances: Vec<InstanceResult> = spec
        .durations
        .iter()
        .enumerate()
        .map(|(idx, d)| instance(&format!("inst-{idx}"), *d, spec.attempts, spec.runs))
        .collect();

    let mut argv = vec!["max".to_owned(), "bench".to_owned(), "swebench".to_owned()];
    if let Some(p) = spec.parallel_argv {
        match spec.parallel_style {
            ParallelStyle::SpaceSeparated => {
                argv.push("--parallel".to_owned());
                argv.push(p.to_string());
            }
            ParallelStyle::Equals => argv.push(format!("--parallel={p}")),
            ParallelStyle::ShortDash => {
                argv.push("-p".to_owned());
                argv.push(p.to_string());
            }
        }
    }
    if spec.resume {
        argv.push("--resume".to_owned());
    }

    let manifest = ProvenanceManifest {
        purpose: None,
        harness: HarnessManifest {
            name: "maxwells-daemon".into(),
            version: "test".into(),
            git_sha: None,
            git_dirty: None,
            git_resolution: "test".into(),
        },
        dataset: DatasetManifest {
            path: "dataset.jsonl".into(),
            sha256: "deadbeef".into(),
            instance_count: instances.len(),
            filter_spec: None,
            source_kind: "local".into(),
            source_revision: Some("sha256:deadbeef".into()),
            selected_row_count: instances.len(),
            post_filter_row_count: instances.len(),
            ..DatasetManifest::default()
        },
        prompt_template: PromptTemplateManifest {
            source: "inline".into(),
            path: None,
            sha256: "prompt".into(),
        },
        config: ConfigManifest {
            resolved: "test".into(),
            overlay_paths: Vec::new(),
        },
        model: ModelManifest {
            name: "claude-opus-4-7".into(),
            backend: "deterministic".into(),
            backend_version: None,
            base_url: None,
        },
        runtime: RuntimeManifest {
            started_at_utc: spec.started.unwrap_or("").to_owned(),
            finished_at_utc: spec.finished.map(std::borrow::ToOwned::to_owned),
            host_os: "test".into(),
            resume_mode: spec.resume,
            rust_version: None,
        },
        cli: CliManifest { argv },
        chaos_fail_every: 0,
        circuit_breaker: None,
        source: None,
        import_predictions_path: None,
        import_predictions_sha256: None,
        reproduced_from: None,
    };

    let results = SweepResults {
        total: instances.len(),
        sweep_status: spec.sweep_status.into(),
        submitted: instances.len(),
        filter_spec: FilterSpec {
            original_count: instances.len(),
            selected_count: instances.len(),
            ..FilterSpec::default()
        },
        manifest: Some(manifest),
        instances,
        ..SweepResults::default()
    };

    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("results.json"),
        maxwells_daemon::artifact::to_string_pretty(ArtifactKind::SweepResults, &results).unwrap(),
    )
    .unwrap();
}

// ── library-level tests ──────────────────────────────────────────────────────

#[test]
fn computes_effective_parallelism_and_utilization() {
    // 8 instances × 300s each = 2400s of work; wallclock = 600s; 8 workers.
    // effective parallelism = 2400 / 600 = 4.0; utilization = 4 / 8 = 50%.
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            ..SweepSpec::default()
        },
    );

    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap();

    assert_eq!(report.configured_workers, 8);
    assert_eq!(report.total_instances, 8);
    assert_eq!(report.sum_instance_duration_secs, 2400.0);
    assert_eq!(report.wallclock_secs, 600.0);
    assert!((report.effective_parallelism - 4.0).abs() < 1e-9);
    assert!((report.utilization_pct - 50.0).abs() < 1e-9);
}

#[test]
fn surfaces_idle_waste() {
    // 2400s work over 8 workers → theoretical min = 300s. Observed = 600s.
    // idle waste = 300s = 50% of wallclock.
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            ..SweepSpec::default()
        },
    );

    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap();

    assert!((report.theoretical_min_wallclock_secs - 300.0).abs() < 1e-9);
    assert!((report.idle_waste_secs - 300.0).abs() < 1e-9);
    assert!((report.idle_waste_pct - 50.0).abs() < 1e-9);
}

#[test]
fn full_utilization_has_zero_idle_waste() {
    // 8 instances × 600s = 4800s over 8 workers, wallclock 600s → perfectly packed.
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(600.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            ..SweepSpec::default()
        },
    );

    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap();

    assert!((report.utilization_pct - 100.0).abs() < 1e-9);
    assert!(report.idle_waste_secs.abs() < 1e-9);
    assert!(report.idle_waste_pct.abs() < 1e-9);
}

#[test]
fn missing_finished_timestamp_is_an_error() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 4];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            finished: None,
            ..SweepSpec::default()
        },
    );

    let err = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("finished_at_utc") || msg.to_lowercase().contains("timestamp"),
        "expected timestamp error, got: {msg}"
    );
}

#[test]
fn worker_count_defaults_when_parallel_absent_from_argv() {
    // No --parallel in argv → falls back to the harness default (4).
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 4];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: None,
            ..SweepSpec::default()
        },
    );

    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap();

    assert_eq!(
        report.configured_workers,
        maxwells_daemon::run::swebench::DEFAULT_PARALLEL
    );
}

#[test]
fn retry_merged_sweep_is_flagged() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 4];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            attempts: 2,
            ..SweepSpec::default()
        },
    );

    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap();

    assert!(report.retry_merged, "multi-attempt sweep should be flagged");
}

#[test]
fn min_utilization_gate_records_pass_and_fail() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            ..SweepSpec::default()
        },
    );

    // utilization is 50%. Floor of 40% passes; floor of 60% fails.
    let pass = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: Some(40.0),
    })
    .unwrap();
    assert_eq!(pass.min_utilization_met, Some(true));

    let fail = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: Some(60.0),
    })
    .unwrap();
    assert_eq!(fail.min_utilization_met, Some(false));
}

#[test]
fn text_render_mentions_key_metrics() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            ..SweepSpec::default()
        },
    );
    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap();
    let text = render_text(&report);
    assert!(text.contains("effective parallelism") || text.contains("Effective parallelism"));
    assert!(text.contains("utilization") || text.contains("Utilization"));
    assert!(text.contains("idle") || text.contains("Idle"));
}

// ── CLI-level tests ──────────────────────────────────────────────────────────

#[test]
fn cli_json_output_has_stable_schema() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            ..SweepSpec::default()
        },
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "utilization",
            "--sweep",
            work.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for key in [
        "schema_version",
        "sweep",
        "configured_workers",
        "total_instances",
        "sum_instance_duration_secs",
        "wallclock_secs",
        "effective_parallelism",
        "utilization_pct",
        "theoretical_min_wallclock_secs",
        "idle_waste_secs",
        "idle_waste_pct",
        "retry_merged",
    ] {
        assert!(value.get(key).is_some(), "missing key: {key}");
    }
}

#[test]
fn cli_min_utilization_gate_exits_nonzero_on_failure() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            ..SweepSpec::default()
        },
    );

    // 50% utilization; floor 60% must fail with a non-zero exit.
    let out = Command::new(binary_path())
        .args([
            "bench",
            "utilization",
            "--sweep",
            work.path().to_str().unwrap(),
            "--min-utilization",
            "60",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected non-zero exit for failed gate"
    );

    // A passing floor exits zero.
    let ok = Command::new(binary_path())
        .args([
            "bench",
            "utilization",
            "--sweep",
            work.path().to_str().unwrap(),
            "--min-utilization",
            "40",
        ])
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
}

#[test]
fn cli_missing_timestamps_exits_nonzero() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 4];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            finished: None,
            ..SweepSpec::default()
        },
    );

    let out = Command::new(binary_path())
        .args([
            "bench",
            "utilization",
            "--sweep",
            work.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn non_positive_wallclock_is_an_error() {
    // finished == started → zero-length wallclock; must error rather than
    // divide by zero and emit misleading infinities.
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 4];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            started: Some("2026-05-01T00:00:00Z"),
            finished: Some("2026-05-01T00:00:00Z"),
            ..SweepSpec::default()
        },
    );

    let err = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap_err();
    assert!(err.to_string().contains("non-positive"), "got: {err}");
}

#[test]
fn all_missing_durations_is_an_error() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![None; 4];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            ..SweepSpec::default()
        },
    );

    let err = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap_err();
    assert!(err.to_string().contains("duration_secs"), "got: {err}");
}

#[test]
fn partial_missing_durations_are_counted_not_fatal() {
    // 2 of 4 instances carry a duration; the other two are excluded from the
    // sum and surfaced in instances_missing_duration.
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0), None, Some(300.0), None];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            ..SweepSpec::default()
        },
    );

    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap();

    assert_eq!(report.total_instances, 4);
    assert_eq!(report.instances_with_duration, 2);
    assert_eq!(report.instances_missing_duration, 2);
    assert_eq!(report.sum_instance_duration_secs, 600.0);
}

#[test]
fn parallel_count_parsed_from_equals_and_short_flag_styles() {
    for style in [ParallelStyle::Equals, ParallelStyle::ShortDash] {
        let work = tempfile::tempdir().unwrap();
        let durations = vec![Some(300.0); 4];
        write_sweep(
            work.path(),
            &SweepSpec {
                durations: &durations,
                parallel_argv: Some(6),
                parallel_style: style,
                ..SweepSpec::default()
            },
        );

        let report = compute(&UtilizationArgs {
            sweep_dir: work.path().to_path_buf(),
            min_utilization: None,
        })
        .unwrap();

        assert_eq!(report.configured_workers, 6);
        assert_eq!(
            report.configured_workers_source,
            "manifest.cli.argv[--parallel]"
        );
    }
}

#[test]
fn non_completed_sweep_is_rejected() {
    // A cancelled sweep writes finished_at_utc but only a partial instance set.
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 4];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            sweep_status: "cancelled",
            ..SweepSpec::default()
        },
    );

    let err = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap_err();
    assert!(err.to_string().contains("not 'completed'"), "got: {err}");
}

#[test]
fn resume_sweep_is_rejected() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 4];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            resume: true,
            ..SweepSpec::default()
        },
    );

    let err = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap_err();
    assert!(err.to_string().contains("--resume"), "got: {err}");
}

#[test]
fn retry_merged_sweep_blocks_the_gate_but_not_the_report() {
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            attempts: 2,
            ..SweepSpec::default()
        },
    );

    // Without a gate, the (flagged) report is still produced.
    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: None,
    })
    .unwrap();
    assert!(report.retry_merged);

    // With a gate, the command refuses to emit a verdict on terminal-only durations.
    let err = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: Some(40.0),
    })
    .unwrap_err();
    assert!(err.to_string().contains("within-run retries"), "got: {err}");
}

#[test]
fn rerun_only_sweep_is_measurable_and_gateable() {
    // A clean --rerun/--samples sweep: aggregate_run_results sums duration_secs
    // across run slots and sums attempts (1 per slot → attempts == runs). It is
    // NOT a within-run retry, so it must stay measurable and gateable.
    let work = tempfile::tempdir().unwrap();
    let durations = vec![Some(300.0); 8];
    write_sweep(
        work.path(),
        &SweepSpec {
            durations: &durations,
            parallel_argv: Some(8),
            attempts: 2,
            runs: 2,
            ..SweepSpec::default()
        },
    );

    let report = compute(&UtilizationArgs {
        sweep_dir: work.path().to_path_buf(),
        min_utilization: Some(40.0),
    })
    .unwrap();
    assert!(
        !report.retry_merged,
        "plain reruns (attempts == runs) must not be flagged as retry-merged"
    );
    assert_eq!(report.min_utilization_met, Some(true));
}
