//! `bench events`: query the structured per-run event log after a run/sweep.
//!
//! Reads the append-only JSONL event log(s) written by [`crate::stream::event_log`]
//! (format: `docs/spec-event-log.md`) and filters/aggregates them by event type,
//! instance id, and time window. This is the post-hoc counterpart to the live
//! `tail`/`watch` views: it operates only on completed, on-disk logs, never calls
//! a model provider, and never mutates run artifacts.
//!
//! Each event line self-describes its `instance_id`, so discovery does not depend
//! on a rigid file-naming convention — any `.jsonl` file whose lines carry a
//! string `event_type` (and, when present, an `event-log-v*` `schema`) is treated
//! as an event log. Non-event/garbage lines are skipped, mirroring the writer's
//! best-effort philosophy.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::BufRead as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::config::RedactionCfg;
use crate::error::{ConfigError, Error};
use crate::redaction::{Redactor, surface};

/// Output schema identifier for the machine-readable (`--format json`) report.
pub const QUERY_SCHEMA: &str = "events-query-v1";

/// Every event type that the writer can emit, in the canonical order from
/// `StreamEvent::event_name`. Kept in sync by a guard test in `tests/bench_events.rs`.
pub const ALL_EVENT_TYPES: &[&str] = &[
    "run_started",
    "assistant_message",
    "bash_start",
    "bash_result",
    "observation",
    "format_error",
    "run_ended",
    "auto_approve_rule_created",
];

#[derive(Debug, Clone)]
pub struct EventsArgs {
    /// A single-run directory, a sweep directory, or an event-log `.jsonl` file.
    pub path: PathBuf,
    /// Event types to keep (`--type`, repeatable). Empty = all types.
    pub types: Vec<String>,
    /// Instance ids to keep (`--instance`, repeatable). Empty = all instances.
    pub instances: Vec<String>,
    /// Inclusive RFC3339 lower bound on `ts` (`--since`).
    pub since: Option<String>,
    /// Inclusive RFC3339 upper bound on `ts` (`--until`).
    pub until: Option<String>,
    /// Emit per-type/per-instance counts instead of individual rows.
    pub summary: bool,
    /// Retain each event's full (redacted) raw payload on the returned rows.
    /// Only `--format json`/`jsonl` serialize the payload; `table` prints just
    /// `ts`/`instance_id`/`event_type`, so the CLI sets this `false` there to
    /// avoid allocating large assistant content / bash output for every event.
    pub include_payloads: bool,
    /// Redaction policy applied when rendering/aggregating events. The on-disk
    /// log's payload fields were already redacted at write time, but
    /// event-only fields the writer injects *after* the runtime `RedactingSink`
    /// — notably `instance_id` (see `crate::stream::event_log::EventLogSink`) —
    /// carry the run's raw values, so a configured `secret_literals` /
    /// `custom_patterns` must be applied here to keep a secret-shaped id from
    /// leaking into rows and summaries. The CLI seeds this from `--config` (the
    /// reliable lever — recorded `secret_literals` are stored already-redacted
    /// and so cannot be recovered from artifacts) unioned with the run/sweep's
    /// best-effort recorded policy (recovers `custom_patterns`/`enabled`);
    /// `default()` reproduces the prior default/env-only behavior.
    pub redaction: RedactionCfg,
}

/// One matched event. Serializes as the full original line object (`raw`, after
/// redaction) so machine-readable output is lossless and free of duplicate keys;
/// the typed `instance_id`/`ts`/`event_type` are decoded copies kept only for
/// in-process filtering, sorting, and aggregation. `instance_id`/`raw` hold the
/// redacted view used for display; `parsed_ts` is the decoded instant used for
/// correct chronological ordering regardless of RFC3339 subsecond precision.
#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    #[serde(skip)]
    pub instance_id: String,
    #[serde(skip)]
    pub ts: String,
    #[serde(skip)]
    pub event_type: String,
    #[serde(skip)]
    pub parsed_ts: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub raw: Value,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EventsSummary {
    /// event_type -> count.
    pub by_type: BTreeMap<String, usize>,
    /// instance_id -> (event_type -> count).
    pub by_instance: BTreeMap<String, BTreeMap<String, usize>>,
    /// Number of distinct instances represented in the matched events.
    pub instances: usize,
    /// Total number of matched events.
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventsReport {
    pub schema: String,
    pub path: String,
    /// Matched events (empty in `--summary` mode).
    pub events: Vec<EventRow>,
    pub summary: EventsSummary,
    pub files_scanned: usize,
    pub lines_skipped: usize,
}

/// Validate `--type` values against [`ALL_EVENT_TYPES`].
///
/// Returns a [`ConfigError::Invalid`] (→ usage error, exit 2) naming the unknown
/// value and listing the valid types.
pub fn validate_types(types: &[String]) -> Result<(), Error> {
    for ty in types {
        if !ALL_EVENT_TYPES.contains(&ty.as_str()) {
            return Err(Error::Config(ConfigError::Invalid(format!(
                "events: unknown --type `{ty}`; valid types: {}",
                ALL_EVENT_TYPES.join(", ")
            ))));
        }
    }
    Ok(())
}

/// Query the event log(s) under `args.path`.
#[allow(clippy::too_many_lines)]
pub fn run(args: &EventsArgs) -> Result<EventsReport, Error> {
    validate_types(&args.types)?;
    let since = parse_bound(args.since.as_deref(), "--since")?;
    let until = parse_bound(args.until.as_deref(), "--until")?;

    if !args.path.exists() {
        return Err(Error::Trajectory(format!(
            "events: path does not exist: {}",
            args.path.display()
        )));
    }

    let files = discover_event_files(&args.path)?;

    let type_set: Option<HashSet<&str>> = if args.types.is_empty() {
        None
    } else {
        Some(args.types.iter().map(String::as_str).collect())
    };
    let instance_set: Option<HashSet<&str>> = if args.instances.is_empty() {
        None
    } else {
        Some(args.instances.iter().map(String::as_str).collect())
    };

    let redactor = Redactor::from_config_lossy(&args.redaction);
    let mut events: Vec<EventRow> = Vec::new();
    // In `--summary` mode we never retain raw events (a shared event log can be
    // multi-GB); aggregate counts during the scan instead so memory stays bounded
    // by the distinct (instance, type) cardinality.
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_instance: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut matched: usize = 0;
    let mut files_scanned = 0usize;
    let mut lines_skipped = 0usize;

    for file in &files {
        // Stream line-by-line: a completed sweep can share one `--event-log`,
        // so the file may be very large — never load it whole into memory.
        let mut reader = std::io::BufReader::new(std::fs::File::open(file)?);
        files_scanned += 1;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            // Read one raw line as bytes: a non-UTF-8 byte sequence must not
            // abort the whole query (the writer is best-effort and a shared log
            // can contain arbitrary captured output), so decode leniently and
            // skip undecodable lines, counting them as skipped.
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            let Ok(line) = std::str::from_utf8(&buf) else {
                lines_skipped += 1;
                continue;
            };
            let line = line.trim_end_matches(['\n', '\r']);
            if line.trim().is_empty() {
                continue;
            }
            let Ok(mut value) = serde_json::from_str::<Value>(line) else {
                lines_skipped += 1;
                continue;
            };
            // A line is an event iff it carries a string event_type and, when a
            // schema is present, it is an event-log schema.
            let Some(event_type) = value
                .get("event_type")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
            else {
                lines_skipped += 1;
                continue;
            };
            // When a `schema` key is present it must be an event-log schema
            // *string*; a present-but-non-string schema (e.g. `{}`) — or a
            // string naming a different schema — means the line is not one of
            // our events, so skip it rather than letting `as_str()` returning
            // `None` silently wave it through. Absent `schema` stays permissive.
            if let Some(schema) = value.get("schema") {
                let is_event_log = schema
                    .as_str()
                    .is_some_and(|s| s.starts_with("event-log-v"));
                if !is_event_log {
                    lines_skipped += 1;
                    continue;
                }
            }

            // Raw (un-redacted) id is used only for filtering against the
            // operator-supplied `--instance` values; the displayed/aggregated id
            // is taken from the redacted object below so a secret-shaped instance
            // id can't leak through the table or the per-instance summary.
            let raw_instance_id = value
                .get("instance_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let ts = value
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            // Parse the timestamp once and keep it for both window-filtering and
            // chronological sorting; lexical RFC3339 comparison mis-orders events
            // whose subsecond precision or offset formatting differs.
            let parsed_ts = parse_ts(&ts);

            if let Some(ref keep) = type_set {
                if !keep.contains(event_type.as_str()) {
                    continue;
                }
            }
            if let Some(ref keep) = instance_set {
                if !keep.contains(raw_instance_id.as_str()) {
                    continue;
                }
            }
            if since.is_some() || until.is_some() {
                let Some(parsed) = parsed_ts else {
                    // Cannot place the event in the window — exclude it.
                    continue;
                };
                if let Some(lower) = since {
                    if parsed < lower {
                        continue;
                    }
                }
                if let Some(upper) = until {
                    if parsed > upper {
                        continue;
                    }
                }
            }

            redactor.redact_json_value(&mut value, surface::INSPECT);
            let instance_id = value
                .get("instance_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            matched += 1;
            if args.summary {
                // Count and drop the value — `--summary` only needs the
                // (redacted) instance id and the event type, not the payload.
                *by_type.entry(event_type.clone()).or_insert(0) += 1;
                *by_instance
                    .entry(instance_id)
                    .or_default()
                    .entry(event_type)
                    .or_insert(0) += 1;
            } else {
                // Only json/jsonl serialize the payload; for table output the
                // renderer needs just the typed fields, so drop the raw value to
                // keep memory bounded on large logs.
                let raw = if args.include_payloads {
                    value
                } else {
                    Value::Null
                };
                events.push(EventRow {
                    instance_id,
                    ts,
                    event_type,
                    parsed_ts,
                    raw,
                });
            }
        }
    }

    let summary = if args.summary {
        EventsSummary {
            instances: by_instance.len(),
            total: matched,
            by_type,
            by_instance,
        }
    } else {
        events.sort_by(|a, b| {
            a.parsed_ts
                .cmp(&b.parsed_ts)
                .then_with(|| a.ts.cmp(&b.ts))
                .then_with(|| a.instance_id.cmp(&b.instance_id))
                .then_with(|| a.event_type.cmp(&b.event_type))
        });
        build_summary(&events)
    };

    // Redact the reported path: an operator-chosen event-log path or temp dir
    // name can itself be secret-shaped, and this string is copied into the
    // shareable JSON report alongside the (already redacted) payloads.
    let path = redactor
        .redact_text(&args.path.display().to_string(), surface::INSPECT)
        .text;

    Ok(EventsReport {
        schema: QUERY_SCHEMA.to_owned(),
        path,
        events,
        summary,
        files_scanned,
        lines_skipped,
    })
}

fn build_summary(events: &[EventRow]) -> EventsSummary {
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_instance: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for ev in events {
        *by_type.entry(ev.event_type.clone()).or_insert(0) += 1;
        *by_instance
            .entry(ev.instance_id.clone())
            .or_default()
            .entry(ev.event_type.clone())
            .or_insert(0) += 1;
    }
    EventsSummary {
        instances: by_instance.len(),
        total: events.len(),
        by_type,
        by_instance,
    }
}

/// Discover candidate event-log files under `path`.
///
/// If `path` is a file it is used directly. If it is a directory it is walked
/// recursively for `*.jsonl` files; additionally, the sibling `{dir}.events.jsonl`
/// is included when present, because the documented sweep pattern writes the log
/// *next to* the output dir (`--output runs/sweep --event-log
/// runs/sweep.events.jsonl`) rather than inside it. Results are deduplicated and
/// sorted for deterministic output.
pub(crate) fn discover_event_files(path: &Path) -> Result<Vec<PathBuf>, Error> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut out = Vec::new();
    collect_jsonl(path, &mut out)?;
    // Probe the conventional sibling log `{dir}.events.jsonl`. Build it from the
    // dir's own file name (not the raw path string) so a trailing separator —
    // `runs/sweep/`, as shell tab-completion commonly produces — yields
    // `runs/sweep.events.jsonl` rather than `runs/sweep/.events.jsonl`.
    if let Some(name) = path.file_name() {
        let mut sibling_name = name.to_owned();
        sibling_name.push(".events.jsonl");
        let sibling = path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(sibling_name);
        // `symlink_metadata` does not follow symlinks, so a symlinked sibling
        // (which could escape the artifact tree or point at a blocking FIFO) is
        // not picked up — consistent with the symlink-safe `collect_jsonl` walk.
        if std::fs::symlink_metadata(&sibling).is_ok_and(|m| m.file_type().is_file()) {
            out.push(sibling);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        // Use the directory entry's own file type (does not follow symlinks) so a
        // circular symlink can't drive infinite recursion / stack overflow, and
        // non-regular entries (symlinks, FIFOs, devices) are skipped — a FIFO
        // would block `File::open` and a symlink could escape the artifact tree.
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_jsonl(&p, out)?;
        } else if file_type.is_file()
            && p.extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            out.push(p);
        }
    }
    Ok(())
}

fn parse_bound(raw: Option<&str>, flag: &str) -> Result<Option<DateTime<Utc>>, Error> {
    match raw {
        None => Ok(None),
        Some(s) => parse_ts(s).map(Some).ok_or_else(|| {
            Error::Config(ConfigError::Invalid(format!(
                "events: invalid {flag} timestamp `{s}`; expected RFC3339 (e.g. 2026-01-01T00:00:00Z)"
            )))
        }),
    }
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(std::convert::Into::into)
}

// ── rendering ─────────────────────────────────────────────────────────────────

/// Human-readable table of individual events (default `--format table`).
#[must_use]
pub fn render_table(report: &EventsReport) -> String {
    if report.events.is_empty() {
        // Nothing to list (summary mode or a filter matched nothing) — fall back
        // to the count view so the command always prints something useful.
        return render_summary_table(report);
    }
    let mut out = String::new();
    for ev in &report.events {
        let _ = writeln!(out, "{}\t{}\t{}", ev.ts, ev.instance_id, ev.event_type);
    }
    out
}

/// Per-type (and, for multi-instance logs, per-instance) counts.
#[must_use]
pub fn render_summary_table(report: &EventsReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} event(s) across {} instance(s)",
        report.summary.total, report.summary.instances
    );
    out.push_str("by type:\n");
    if report.summary.by_type.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for (ty, count) in &report.summary.by_type {
            let _ = writeln!(out, "  {ty}: {count}");
        }
    }
    if report.summary.instances > 1 {
        out.push_str("by instance:\n");
        for (instance, types) in &report.summary.by_instance {
            let total: usize = types.values().sum();
            let _ = writeln!(out, "  {instance}: {total}");
            for (ty, count) in types {
                let _ = writeln!(out, "    {ty}: {count}");
            }
        }
    }
    out
}

/// Single schema-versioned JSON object (`--format json`).
pub fn render_json(report: &EventsReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

/// One matched event per line (`--format jsonl`). Empty in `--summary` mode.
pub fn render_jsonl(report: &EventsReport) -> Result<String, serde_json::Error> {
    let mut lines = Vec::with_capacity(report.events.len());
    for ev in &report.events {
        lines.push(serde_json::to_string(ev)?);
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn ev(ts: &str, instance: &str, ty: &str) -> EventRow {
        EventRow {
            instance_id: instance.to_owned(),
            ts: ts.to_owned(),
            event_type: ty.to_owned(),
            parsed_ts: parse_ts(ts),
            raw: serde_json::json!({"event_type": ty, "instance_id": instance, "ts": ts}),
        }
    }

    #[test]
    fn validate_types_accepts_all_known() {
        let all: Vec<String> = ALL_EVENT_TYPES.iter().map(|s| (*s).to_owned()).collect();
        assert!(validate_types(&all).is_ok());
    }

    #[test]
    fn validate_types_rejects_unknown() {
        assert!(validate_types(&["nope".to_owned()]).is_err());
    }

    #[test]
    fn summary_counts_per_type_and_instance() {
        let events = vec![
            ev("t1", "a", "run_started"),
            ev("t2", "a", "run_ended"),
            ev("t3", "b", "run_ended"),
        ];
        let s = build_summary(&events);
        assert_eq!(s.total, 3);
        assert_eq!(s.instances, 2);
        assert_eq!(s.by_type.get("run_ended"), Some(&2));
        assert_eq!(s.by_instance["a"].get("run_started"), Some(&1));
    }

    #[test]
    fn jsonl_round_trips_event_fields() {
        let report = EventsReport {
            schema: QUERY_SCHEMA.to_owned(),
            path: ".".to_owned(),
            events: vec![ev("t1", "a", "bash_result")],
            summary: EventsSummary::default(),
            files_scanned: 1,
            lines_skipped: 0,
        };
        let jsonl = render_jsonl(&report).unwrap();
        let v: Value = serde_json::from_str(jsonl.trim()).unwrap();
        assert_eq!(v["event_type"], "bash_result");
        assert_eq!(v["instance_id"], "a");
    }
}
