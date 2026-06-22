# `bench events` — query the structured per-run event log

`bench events` filters and aggregates the structured, per-run event log that
Maxwell's Daemon writes when `--event-log <PATH>` is passed to `bench mini` /
`bench swebench` (writer: `src/stream/event_log.rs`, line format:
`docs/spec-event-log.md`). It is the **post-hoc, offline** counterpart to the
live `tail` / `watch` views: it reads only completed, on-disk logs, **never calls
a model provider**, and **never mutates run artifacts**.

For free-text search over trajectory *prose* use `bench grep`; this command
filters *structured* events. For live streaming use `bench tail` / `bench watch`.
For remote/OTLP export see `docs/spec-otlp-traces.md`.

## Synopsis

```bash
bench events <PATH> [--type T]... [--instance ID]... \
    [--since RFC3339] [--until RFC3339] [--summary] \
    [--format table|json|jsonl]
```

`<PATH>` may be:
- an event-log `.jsonl` file, or
- a single-run or sweep directory, which is walked **recursively** for regular
  `*.jsonl` files. The sibling log `<dir>.events.jsonl` is also picked up, since
  the documented sweep pattern writes the log next to the output directory
  (`--output runs/sweep --event-log runs/sweep.events.jsonl`) rather than inside
  it. Symlinks, FIFOs, and other non-regular entries are skipped.

Because every event line self-describes its `instance_id`, discovery does not
depend on a file-naming convention: any `.jsonl` file whose lines carry a string
`event_type` (and, when present, an `event-log-v*` `schema`) is treated as an
event log. Lines that are not valid JSON, or that lack an `event_type`, or whose
`schema` is not an event-log schema, are skipped and counted in `lines_skipped`
(mirroring the writer's best-effort, never-fail philosophy).

## Filters

| Flag | Repeatable | Meaning |
|------|:---------:|---------|
| `--type <TYPE>` | yes | Keep only the named event type(s). Multiple values union. |
| `--instance <ID>` | yes | Keep only the named instance id(s). Multiple values union. |
| `--since <RFC3339>` | no | Keep events with `ts >=` this instant (inclusive). |
| `--until <RFC3339>` | no | Keep events with `ts <=` this instant (inclusive). |
| `--summary` | no | Print per-type (and per-instance, over a sweep) counts only. |
| `--format <FMT>` | no | `table` (default), `json`, or `jsonl`. |
| `--config <PATH>` | no | Config whose `[redaction]` rules are applied when rendering (see Redaction). |

Events are emitted sorted by `(ts, instance_id, event_type)` for stable output.

### Redaction

Event content is passed through a redactor (the `inspect` surface) before
printing, so secrets captured in event payloads are masked. Payload fields are
already redacted at write time, but the writer injects the `instance_id`
*after* the runtime redactor (see `src/stream/event_log.rs`), so an event log
records the run's **raw** instance id. To keep a secret-shaped id (a SWE-bench
`instance_id` that matches a configured `secret_literals` / `custom_patterns`)
from leaking into rows and summaries, `bench events` re-applies the run's
redaction policy:

- **`--config <PATH>` is the reliable lever.** A run records its configured
  `secret_literals` *already redacted* — sweeps redact the resolved config in
  `manifest.json` / `results.json` (`build_manifest`), and a standalone
  `bench mini` run redacts them in its trajectory — so a literal value cannot be
  recovered from artifacts. Pass `--config` with the run's config to mask a
  literal-shaped instance id (for both sweeps and mini).
- **Best-effort auto-recovery (no `--config` needed).** When you pass a sweep
  directory, its `{dir}.events.jsonl` sibling, or a parent holding several sweeps,
  each governing `manifest.json` / `results.json` `[redaction]` policy is unioned
  with the defaults, recovering the `enabled` flag and any `custom_patterns`
  (whose regex *source* is stored plaintext). This is a bonus, not a substitute
  for `--config` when the secret is a configured literal.

When `--config` is given, its `enabled` flag is authoritative — auto-recovery
adds rules but never re-enables redaction over an explicit `enabled = false`
(e.g. to inspect raw instance ids). Without `--config`, redaction defaults on.

### Valid `--type` values

All event types emitted by the writer are selectable; an unknown value is a
usage error (exit 2) whose message lists the valid set:

`run_started`, `assistant_message`, `bash_start`, `bash_result`, `observation`,
`format_error`, `run_ended`, `auto_approve_rule_created`.

This set is kept identical to `StreamEvent::event_name` (`src/stream/mod.rs`) and
to the documented types in `docs/spec-event-log.md`.

## Output

### `--format table` (default)

Tab-separated `ts<TAB>instance_id<TAB>event_type`, one row per matched event.
With `--summary` (or when there are no rows), the count view is printed instead.

### `--summary`

```
<N> event(s) across <M> instance(s)
by type:
  <event_type>: <count>
  ...
by instance:            # only when more than one instance is present
  <instance_id>: <total>
    <event_type>: <count>
    ...
```

### `--format json`

A single schema-versioned object (`schema: "events-query-v1"`):

```json
{
  "schema": "events-query-v1",
  "path": "runs/sweep",
  "events": [
    { "instance_id": "instance-a", "ts": "2026-01-01T00:00:03Z",
      "event_type": "format_error", "schema": "event-log-v1", "step": 2, "...": "..." }
  ],
  "summary": {
    "by_type": { "format_error": 1, "run_ended": 2 },
    "by_instance": { "instance-a": { "format_error": 1, "run_ended": 1 } },
    "instances": 2,
    "total": 8
  },
  "files_scanned": 2,
  "lines_skipped": 2
}
```

In `--summary` mode `events` is empty (counts live in `summary`). Each event
object flattens the original event-log line, so the raw `event-log-v1` fields are
preserved losslessly (after redaction).

### `--format jsonl`

One matched event per line (the flattened, redacted event object). Empty in
`--summary` mode. Suitable for piping to `jq`.

## Exit codes

Per `docs/exit-codes.md`:

| Code | Class | When |
|-----:|-------|------|
| 0 | `success` | Query completed (including zero matches). |
| 1 | `internal_error` | The path is missing or an event-log file is unreadable. |
| 2 | `usage_error` | Unknown `--type`, unknown `--format`, or an invalid `--since`/`--until` timestamp. |

The command is strictly read-only and never writes to or modifies the event log
or any other run artifact.

## Examples

```bash
# every tool/format error for one failed instance
bench events runs/sweep --type format_error --instance django__django-12345

# which instances emitted a given event type, as counts
bench events runs/sweep --type bash_result --summary

# events in a time window, machine-readable, piped to jq
bench events runs/sweep --since 2026-01-01T00:00:00Z --until 2026-01-01T01:00:00Z \
    --format jsonl | jq -c '{ts, instance_id, event_type}'
```
