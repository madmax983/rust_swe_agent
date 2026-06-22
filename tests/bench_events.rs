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

use maxwells_daemon::config::RedactionCfg;
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
        // Full-fidelity by default for direct API callers/tests; the CLI sets
        // this per output format.
        include_payloads: true,
        // Default policy (enabled, no configured literals) reproduces the prior
        // default/env-only redaction; individual tests override as needed.
        redaction: RedactionCfg::default(),
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
    // Summary mode must not retain raw event payloads (memory-bounded scan).
    assert!(
        report.events.is_empty(),
        "summary mode should aggregate counts without retaining raw events"
    );
    assert_eq!(report.summary.total, 8);
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

#[test]
fn unit_sorts_by_instant_not_lexical_string() {
    // Two events for one instance whose lexical `ts` order is the REVERSE of
    // their chronological order: "…00.500Z" < "…00Z" as strings ('.' < 'Z'),
    // but 00.5s is later than 00s. The parsed-instant sort must order them right.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("e.jsonl");
    std::fs::write(
        &path,
        "{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00.500Z\",\"event_type\":\"bash_result\",\"instance_id\":\"x\"}\n\
         {\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"x\"}\n",
    )
    .unwrap();

    let report = events_run(&base_args(path)).unwrap();
    assert_eq!(report.events.len(), 2);
    assert_eq!(
        report.events[0].event_type, "run_started",
        "the 00Z event is earlier and must sort first"
    );
    assert_eq!(report.events[1].event_type, "bash_result");
}

#[test]
fn unit_redacts_secret_shaped_instance_id_in_display_and_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("e.jsonl");
    // A fake GitHub token shape the default redactor masks, used AS the instance id.
    let fake_token = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    std::fs::write(
        &path,
        format!(
            "{{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"{fake_token}\"}}\n"
        ),
    )
    .unwrap();

    let mut args = base_args(path);
    args.summary = true;
    let report = events_run(&args).unwrap();
    for key in report.summary.by_instance.keys() {
        assert!(
            !key.contains(fake_token),
            "per-instance summary key must not leak the raw secret-shaped id: {key}"
        );
    }
}

#[test]
fn unit_applies_configured_literal_to_instance_id() {
    // A plain word the *default* rules never touch but a configured
    // `secret_literals` does. The writer injects `instance_id` after the runtime
    // RedactingSink, so the on-disk id is raw and only the query-time redactor can
    // mask it — proving `bench events` must honor the run's configured policy.
    let secret = "sw33tcustomliteral";
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("e.jsonl");
    std::fs::write(
        &path,
        format!(
            "{{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"{secret}\"}}\n"
        ),
    )
    .unwrap();

    // Default policy: nothing matches, so the id passes through verbatim.
    let mut args = base_args(path);
    args.summary = true;
    let report = events_run(&args).unwrap();
    assert!(
        report.summary.by_instance.contains_key(secret),
        "default rules should leave a non-secret-shaped id unchanged"
    );

    // Configured literal: the id must be masked in the per-instance summary.
    args.redaction = RedactionCfg {
        enabled: true,
        secret_literals: vec![secret.to_owned()],
        ..RedactionCfg::default()
    };
    let report = events_run(&args).unwrap();
    for key in report.summary.by_instance.keys() {
        assert!(
            !key.contains(secret),
            "configured literal must not survive in the per-instance summary: {key}"
        );
    }
}

#[test]
fn unit_discovers_sibling_sweep_event_log() {
    // Documented sweep pattern: `--output runs/sweep --event-log runs/sweep.events.jsonl`,
    // i.e. the log is a sibling of the sweep dir. Passing the dir must still find it.
    let tmp = tempfile::tempdir().unwrap();
    let sweep = tmp.path().join("sweep");
    std::fs::create_dir(&sweep).unwrap();
    std::fs::write(
        tmp.path().join("sweep.events.jsonl"),
        "{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"x\"}\n",
    )
    .unwrap();

    let report = events_run(&base_args(sweep)).unwrap();
    assert_eq!(
        report.events.len(),
        1,
        "the sibling sweep.events.jsonl must be discovered"
    );
    assert_eq!(report.events[0].instance_id, "x");
}

#[test]
fn unit_table_mode_does_not_retain_payloads() {
    let mut args = base_args(fixture_sweep());
    args.include_payloads = false; // table/summary CLI path
    let report = events_run(&args).unwrap();
    assert!(!report.events.is_empty());
    // Typed columns are kept (table renders these); raw payload is dropped.
    for ev in &report.events {
        assert!(!ev.event_type.is_empty());
        assert!(
            ev.raw.is_null(),
            "table-mode rows must not retain the payload"
        );
    }
}

#[test]
fn unit_json_mode_retains_payloads() {
    let mut args = base_args(fixture_sweep());
    args.include_payloads = true; // json/jsonl CLI path
    let report = events_run(&args).unwrap();
    assert!(
        report.events.iter().any(|e| !e.raw.is_null()),
        "json/jsonl mode must retain the raw payload"
    );
}

#[test]
fn unit_reported_path_is_redacted() {
    // A secret-shaped segment in the path must not leak into the shareable report.
    let fake_token = "ghp_0123456789ABCDEF0123456789ABCDEF0123";
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(format!("run-{fake_token}"));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(
        dir.join("e.jsonl"),
        "{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"x\"}\n",
    )
    .unwrap();

    let report = events_run(&base_args(dir)).unwrap();
    assert!(
        !report.path.contains(fake_token),
        "reported query path must be redacted, got: {}",
        report.path
    );
}

#[cfg(unix)]
#[test]
fn unit_skips_symlinked_jsonl_during_discovery() {
    // A symlinked *.jsonl entry inside the dir must be skipped (is_file() is false
    // for symlinks), so discovery can't follow links outside the artifact tree.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sweep");
    std::fs::create_dir(&dir).unwrap();
    let real = tmp.path().join("outside.jsonl");
    std::fs::write(
        &real,
        "{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"leak\"}\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&real, dir.join("link.jsonl")).unwrap();

    let report = events_run(&base_args(dir)).unwrap();
    assert!(
        report.events.is_empty(),
        "symlinked jsonl must not be followed during discovery"
    );
}

#[test]
fn unit_non_string_schema_is_skipped() {
    // A present-but-non-string `schema` (e.g. `{}`) means the line is not one of
    // our events; it must be skipped, not waved through because `as_str()` is None.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("e.jsonl");
    std::fs::write(
        &path,
        "{\"schema\":{},\"event_type\":\"run_started\",\"instance_id\":\"x\",\"ts\":\"2026-01-01T00:00:00Z\"}\n\
         {\"schema\":\"event-log-v1\",\"event_type\":\"run_ended\",\"instance_id\":\"x\",\"ts\":\"2026-01-01T00:00:01Z\"}\n",
    )
    .unwrap();

    let report = events_run(&base_args(path)).unwrap();
    assert_eq!(
        report.events.len(),
        1,
        "only the valid event-log-v1 line should be kept"
    );
    assert_eq!(report.events[0].event_type, "run_ended");
    assert!(
        report.lines_skipped >= 1,
        "the non-string-schema line must be counted as skipped"
    );
}

#[test]
fn unit_invalid_utf8_line_is_skipped_not_fatal() {
    // A non-UTF-8 byte line must not abort the query over an otherwise valid log.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("e.jsonl");
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(
        b"{\"schema\":\"event-log-v1\",\"event_type\":\"run_started\",\"instance_id\":\"x\",\"ts\":\"2026-01-01T00:00:00Z\"}\n",
    );
    bytes.extend_from_slice(&[0xff, 0xfe, 0x00, b'\n']); // invalid UTF-8 line
    bytes.extend_from_slice(
        b"{\"schema\":\"event-log-v1\",\"event_type\":\"run_ended\",\"instance_id\":\"x\",\"ts\":\"2026-01-01T00:00:01Z\"}\n",
    );
    std::fs::write(&path, bytes).unwrap();

    let report = events_run(&base_args(path)).unwrap();
    assert_eq!(
        report.events.len(),
        2,
        "both valid events survive an interleaved non-UTF-8 line"
    );
    assert!(
        report.lines_skipped >= 1,
        "the invalid-UTF-8 line must be counted as skipped"
    );
}

#[test]
fn unit_discovers_sibling_with_trailing_separator() {
    // A dir passed with a trailing separator (shell tab-completion) must still
    // resolve its sibling `{dir}.events.jsonl`, not `{dir}/.events.jsonl`.
    let tmp = tempfile::tempdir().unwrap();
    let sweep = tmp.path().join("sweep");
    std::fs::create_dir(&sweep).unwrap();
    std::fs::write(
        tmp.path().join("sweep.events.jsonl"),
        "{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"x\"}\n",
    )
    .unwrap();

    // Append a separator so the path ends in `/`.
    let with_sep = PathBuf::from(format!("{}/", sweep.display()));
    let report = events_run(&base_args(with_sep)).unwrap();
    assert_eq!(
        report.events.len(),
        1,
        "sibling must be found even with a trailing separator"
    );
}

#[cfg(unix)]
#[test]
fn unit_skips_symlinked_sibling_event_log() {
    // A symlinked `{dir}.events.jsonl` sibling must not be followed (it could
    // escape the artifact tree or point at a blocking FIFO).
    let tmp = tempfile::tempdir().unwrap();
    let sweep = tmp.path().join("sweep");
    std::fs::create_dir(&sweep).unwrap();
    let real = tmp.path().join("outside.jsonl");
    std::fs::write(
        &real,
        "{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"leak\"}\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&real, tmp.path().join("sweep.events.jsonl")).unwrap();

    let report = events_run(&base_args(sweep)).unwrap();
    assert!(
        report.events.is_empty(),
        "symlinked sibling event log must not be followed"
    );
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

/// Write a sweep dir whose `manifest.json` records `resolved` (a `[redaction]`
/// TOML block) and an event log with one event for `instance_id`. Returns the dir.
fn write_sweep(tmp: &Path, resolved: &str, instance_id: &str) -> PathBuf {
    let dir = tmp.join("sweep");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::json!({ "config": { "resolved": resolved } }).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("run.events.jsonl"),
        format!(
            "{{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"{instance_id}\"}}\n"
        ),
    )
    .unwrap();
    dir
}

#[test]
fn cli_sweep_redacted_literal_needs_config() {
    // Production sweeps redact secret_literals in their manifest (build_manifest,
    // src/run/swebench.rs), storing `[REDACTED:…]`. `merge_recorded_sweep_redaction`
    // skips those, so auto-recovery CANNOT mask a literal-shaped id — `--config` is
    // the reliable lever. This characterizes that limitation honestly.
    let secret = "sw33tcustomliteral";
    let tmp = tempfile::tempdir().unwrap();
    let dir = write_sweep(
        tmp.path(),
        "[redaction]\nenabled = true\nsecret_literals = [\"[REDACTED:configured_literal]\"]\n",
        secret,
    );

    // Without --config the redacted manifest yields nothing recoverable: the raw id
    // is still rendered (json so the per-instance map is always serialized).
    let bare = run_cli(&[dir.to_str().unwrap(), "--summary", "--format", "json"]);
    assert!(
        String::from_utf8(bare.stdout).unwrap().contains(secret),
        "a redacted-in-manifest literal cannot be auto-recovered; id remains until --config"
    );

    // --config supplying the real literal masks it.
    let cfg = tmp.path().join("cfg.toml");
    std::fs::write(
        &cfg,
        format!("[redaction]\nsecret_literals = [\"{secret}\"]\n"),
    )
    .unwrap();
    let out = run_cli(&[
        dir.to_str().unwrap(),
        "--summary",
        "--format",
        "json",
        "--config",
        cfg.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        !String::from_utf8(out.stdout).unwrap().contains(secret),
        "--config must mask the literal-shaped instance id"
    );
}

#[test]
fn cli_auto_recovers_sweep_custom_pattern() {
    // Unlike literals, a `custom_patterns` regex *source* is stored plaintext in the
    // manifest (it does not match its own regex), so it IS auto-recovered and masks
    // a matching instance id with no `--config`.
    let tmp = tempfile::tempdir().unwrap();
    let dir = write_sweep(
        tmp.path(),
        "[redaction]\nenabled = true\ncustom_patterns = [\"inst-[0-9]+\"]\n",
        "inst-12345",
    );

    let out = run_cli(&[dir.to_str().unwrap(), "--summary", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("\"total\": 1") || stdout.contains("\"total\":1"),
        "the recorded event should still be counted: {stdout}"
    );
    assert!(
        !stdout.contains("inst-12345"),
        "the sweep's recorded custom_pattern must mask the matching id: {stdout}"
    );
}

#[test]
fn cli_config_flag_masks_instance_id_for_bare_log() {
    // A bare event-log file outside any sweep dir: no manifest to recover from, so
    // `--config` is the operator's lever to apply the run's redaction policy.
    let secret = "sw33tcustomliteral";
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("e.jsonl");
    std::fs::write(
        &log,
        format!(
            "{{\"schema\":\"event-log-v1\",\"ts\":\"2026-01-01T00:00:00Z\",\"event_type\":\"run_started\",\"instance_id\":\"{secret}\"}}\n"
        ),
    )
    .unwrap();
    let cfg = tmp.path().join("cfg.toml");
    std::fs::write(
        &cfg,
        format!("[redaction]\nsecret_literals = [\"{secret}\"]\n"),
    )
    .unwrap();

    // Without --config (and no manifest), the id passes through verbatim. Use
    // json so the per-instance map is always serialized (a single-instance
    // summary table omits the per-instance breakdown).
    let bare = run_cli(&[log.to_str().unwrap(), "--summary", "--format", "json"]);
    assert!(
        String::from_utf8(bare.stdout).unwrap().contains(secret),
        "control: default policy leaves the id unchanged"
    );

    // With --config, the configured literal masks it.
    let out = run_cli(&[
        log.to_str().unwrap(),
        "--summary",
        "--format",
        "json",
        "--config",
        cfg.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains(secret),
        "--config literal must mask the matching instance id: {stdout}"
    );
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
