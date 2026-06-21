//! `bench events`: query the structured per-run event log after a run/sweep.
//!
//! Covers the issue #531 acceptance criteria over checked-in event-log fixtures:
//! type/instance/time filters, table/json/jsonl output, `--summary` mode,
//! unknown-`--type` usage errors, exit-code mapping, and read-only behavior.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

mod support;
use support::binary_path;

use maxwells_daemon::run::events::{
    ALL_EVENT_TYPES, EventsArgs, run as events_run, validate_types,
};

// ── helpers ───────────────────────────────────────────────────────────────────

fn fixture_sweep() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/events/sweep")
}

fn base_args(path: PathBuf) -> EventsArgs {
    EventsArgs {
        path,
        types: vec![],
        instances: vec![],
        since: None,
        until: None,
        summary: false,
    }
}

// ── unit tests (library core) ─────────────────────────────────────────────────

#[test]
fn unit_reads_events_from_all_instances_in_a_sweep() {
    let report = events_run(&base_args(fixture_sweep())).unwrap();
    // instance-a has 5 events, instance-b has 3 (+2 skipped lines).
    assert_eq!(report.events.len(), 8, "should read all 8 valid events");
    assert!(report.events.iter().any(|e| e.instance_id == "instance-a"));
    assert!(report.events.iter().any(|e| e.instance_id == "instance-b"));
    assert!(report.files_scanned >= 2, "both fixture files scanned");
}

#[test]
fn unit_malformed_lines_are_skipped_not_fatal() {
    let report = events_run(&base_args(fixture_sweep())).unwrap();
    assert!(
        report.lines_skipped >= 2,
        "the non-json and event_type-less lines must be skipped, got {}",
        report.lines_skipped
    );
}

#[test]
fn unit_events_sorted_by_timestamp() {
    let report = events_run(&base_args(fixture_sweep())).unwrap();
    let ts: Vec<&str> = report.events.iter().map(|e| e.ts.as_str()).collect();
    let mut sorted = ts.clone();
    sorted.sort_unstable();
    assert_eq!(ts, sorted, "events should be ordered by ts");
}

#[test]
fn unit_type_filter_restricts_to_format_error() {
    let mut args = base_args(fixture_sweep());
    args.types = vec!["format_error".into()];
    let report = events_run(&args).unwrap();
    assert!(!report.events.is_empty(), "format_error exists in fixture");
    assert!(
        report.events.iter().all(|e| e.event_type == "format_error"),
        "only format_error events should remain"
    );
    assert!(
        report.events.iter().all(|e| e.instance_id == "instance-a"),
        "only instance-a emitted format_error in the fixture"
    );
}

#[test]
fn unit_type_filter_is_a_union_when_repeated() {
    let mut args = base_args(fixture_sweep());
    args.types = vec!["format_error".into(), "run_ended".into()];
    let report = events_run(&args).unwrap();
    assert!(
        report
            .events
            .iter()
            .all(|e| { e.event_type == "format_error" || e.event_type == "run_ended" })
    );
    assert!(report.events.iter().any(|e| e.event_type == "run_ended"));
    assert!(report.events.iter().any(|e| e.event_type == "format_error"));
}

#[test]
fn unit_instance_filter_restricts_to_one_instance() {
    let mut args = base_args(fixture_sweep());
    args.instances = vec!["instance-b".into()];
    let report = events_run(&args).unwrap();
    assert!(!report.events.is_empty());
    assert!(
        report.events.iter().all(|e| e.instance_id == "instance-b"),
        "only instance-b events should remain"
    );
}

#[test]
fn unit_since_filter_includes_only_later_events() {
    let mut args = base_args(fixture_sweep());
    args.since = Some("2026-01-01T00:30:00Z".into());
    let report = events_run(&args).unwrap();
    assert!(!report.events.is_empty());
    assert!(
        report.events.iter().all(|e| e.instance_id == "instance-b"),
        "instance-a events predate --since and must be excluded"
    );
}

#[test]
fn unit_until_filter_includes_only_earlier_events() {
    let mut args = base_args(fixture_sweep());
    args.until = Some("2026-01-01T00:30:00Z".into());
    let report = events_run(&args).unwrap();
    assert!(!report.events.is_empty());
    assert!(
        report.events.iter().all(|e| e.instance_id == "instance-a"),
        "instance-b events postdate --until and must be excluded"
    );
}

#[test]
fn unit_summary_counts_per_type_and_per_instance() {
    let mut args = base_args(fixture_sweep());
    args.summary = true;
    let report = events_run(&args).unwrap();
    assert_eq!(report.summary.by_type.get("run_ended"), Some(&2));
    assert_eq!(report.summary.by_type.get("format_error"), Some(&1));
    assert_eq!(report.summary.instances, 2);
    // per-instance breakdown present for a multi-instance sweep
    let a = report.summary.by_instance.get("instance-a").unwrap();
    assert_eq!(a.get("format_error"), Some(&1));
    let b = report.summary.by_instance.get("instance-b").unwrap();
    assert!(b.get("format_error").is_none());
}

#[test]
fn unit_unknown_type_is_a_usage_error() {
    assert!(validate_types(&["totally_bogus".into()]).is_err());
}

#[test]
fn unit_all_documented_types_validate() {
    let all: Vec<String> = ALL_EVENT_TYPES.iter().map(|s| (*s).to_owned()).collect();
    validate_types(&all).expect("every catalogued event type must validate");
}

#[test]
fn unit_all_event_types_matches_stream_event_names() {
    // Guard: keep the selectable --type list in sync with what the writer emits.
    let expected = [
        "run_started",
        "assistant_message",
        "bash_start",
        "bash_result",
        "observation",
        "format_error",
        "run_ended",
        "auto_approve_rule_created",
    ];
    let mut got: Vec<&str> = ALL_EVENT_TYPES.to_vec();
    got.sort_unstable();
    let mut want: Vec<&str> = expected.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "ALL_EVENT_TYPES must equal the 8 emitted names");
}

#[test]
fn unit_missing_path_returns_error() {
    let mut args = base_args(PathBuf::from("/tmp/nonexistent-events-dir-xyz-531"));
    args.summary = true;
    assert!(events_run(&args).is_err());
}

#[test]
fn unit_accepts_a_single_event_log_file() {
    let report = events_run(&base_args(fixture_sweep().join("instance-a.events.jsonl"))).unwrap();
    assert_eq!(report.events.len(), 5);
    assert!(report.events.iter().all(|e| e.instance_id == "instance-a"));
}

// ── CLI integration tests ─────────────────────────────────────────────────────

fn run_cli(args: &[&str]) -> std::process::Output {
    let mut full = vec!["--log", "error", "bench", "events"];
    full.extend_from_slice(args);
    Command::new(binary_path()).args(&full).output().unwrap()
}

#[test]
fn cli_table_default_lists_rows_and_exits_0() {
    let sweep = fixture_sweep();
    let out = run_cli(&[sweep.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "happy path exits 0");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("instance-a"));
    assert!(stdout.contains("format_error"));
}

#[test]
fn cli_summary_prints_counts_not_rows() {
    let sweep = fixture_sweep();
    let out = run_cli(&[sweep.to_str().unwrap(), "--summary"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("run_ended"));
    assert!(stdout.contains("format_error"));
}

#[test]
fn cli_json_emits_single_schema_versioned_object() {
    let sweep = fixture_sweep();
    let out = run_cli(&[sweep.to_str().unwrap(), "--format", "json"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["schema"], "events-query-v1");
    assert!(v["events"].is_array());
    assert!(v["summary"].is_object());
}

#[test]
fn cli_jsonl_emits_one_event_per_line() {
    let sweep = fixture_sweep();
    let out = run_cli(&[sweep.to_str().unwrap(), "--format", "jsonl"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let mut count = 0;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let obj: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
        assert!(obj["event_type"].is_string());
        assert!(obj["instance_id"].is_string());
        count += 1;
    }
    assert_eq!(count, 8, "all 8 events emitted, one per line");
}

#[test]
fn cli_type_filter_restricts_output() {
    let sweep = fixture_sweep();
    let out = run_cli(&[
        sweep.to_str().unwrap(),
        "--format",
        "jsonl",
        "--type",
        "format_error",
    ]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(obj["event_type"], "format_error");
    }
}

#[test]
fn cli_unknown_type_exits_2_and_lists_valid_types() {
    let sweep = fixture_sweep();
    let out = run_cli(&[sweep.to_str().unwrap(), "--type", "totally_bogus"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "unknown --type is a usage error (exit 2)"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("format_error") && stderr.contains("run_ended"),
        "error should list valid event types; got: {stderr}"
    );
}

#[test]
fn cli_bad_format_exits_2() {
    let sweep = fixture_sweep();
    let out = run_cli(&[sweep.to_str().unwrap(), "--format", "yaml"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "unknown --format is usage error"
    );
}

#[test]
fn cli_bad_timestamp_exits_2() {
    let sweep = fixture_sweep();
    let out = run_cli(&[sweep.to_str().unwrap(), "--since", "not-a-timestamp"]);
    assert_eq!(out.status.code(), Some(2), "invalid --since is usage error");
}

#[test]
fn cli_missing_path_exits_1() {
    let out = run_cli(&["/tmp/nonexistent-events-dir-xyz-531"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "missing/unreadable artifact exits 1 (internal_error)"
    );
}

#[test]
fn cli_help_mentions_zero_cost_or_read_only() {
    let out = Command::new(binary_path())
        .args(["bench", "events", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap().to_lowercase();
    assert!(
        stdout.contains("zero-cost")
            || stdout.contains("read-only")
            || stdout.contains("never calls a model"),
        "help should advertise the read-only / zero-cost guarantee: {stdout}"
    );
}

#[test]
fn cli_does_not_mutate_the_event_log_dir() {
    // Copy the fixture, snapshot file bytes, run the command, assert unchanged.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sweep");
    std::fs::create_dir_all(&dir).unwrap();
    let mut before = Vec::new();
    for entry in std::fs::read_dir(fixture_sweep()).unwrap() {
        let entry = entry.unwrap();
        let bytes = std::fs::read(entry.path()).unwrap();
        std::fs::write(dir.join(entry.file_name()), &bytes).unwrap();
        before.push((entry.file_name(), bytes));
    }

    let out = run_cli(&[dir.to_str().unwrap(), "--summary"]);
    assert!(out.status.success());

    let mut after_files = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        after_files += 1;
        let bytes = std::fs::read(entry.path()).unwrap();
        let (_, orig) = before
            .iter()
            .find(|(name, _)| name == &entry.file_name())
            .expect("no new files created");
        assert_eq!(&bytes, orig, "file contents must be unchanged");
    }
    assert_eq!(after_files, before.len(), "no files added or removed");
}

// ── performance smoke ─────────────────────────────────────────────────────────

#[test]
fn cli_300_instance_summary_is_fast() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sweep");
    std::fs::create_dir_all(&dir).unwrap();

    // One event-log file per instance, a handful of events each.
    for i in 0..300usize {
        let id = format!("perf-{i:03}");
        let mut buf = String::new();
        for (k, ev) in ["run_started", "bash_result", "format_error", "run_ended"]
            .iter()
            .enumerate()
        {
            let _ = writeln!(
                buf,
                "{{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:{k:02}Z\",\"event_type\":\"{ev}\",\"instance_id\":\"{id}\",\"step\":1}}"
            );
        }
        std::fs::write(dir.join(format!("{id}.events.jsonl")), buf).unwrap();
    }

    let start = std::time::Instant::now();
    let out = run_cli(&[dir.to_str().unwrap(), "--summary"]);
    let elapsed = start.elapsed();
    assert!(out.status.success());
    assert!(
        elapsed.as_secs_f64() < 2.0,
        "summary over 300-instance log must be under 2s, took {:.2}s",
        elapsed.as_secs_f64()
    );
}
