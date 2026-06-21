# Event Log (`--event-log`)

`--event-log <PATH>` enables append-only JSONL emission of `StreamEvent`s for `bench mini` and `bench swebench`.

## Line schema (v1)
Each line is one JSON object with:
- `schema`: fixed string `event-log-v1`
- `ts`: RFC3339 timestamp when the line is emitted
- `event_type`: snake_case stream event type (`run_started`, `assistant_message`, `bash_start`, `bash_result`, `observation`, `format_error`, `run_ended`, `auto_approve_rule_created`)
- `instance_id`: per-instance id (`mini` for standalone mini runs unless overridden by caller; SWE-bench uses dataset `instance_id`)
- event-specific fields copied from the event payload

## Redaction
Event-log emission is passed through the same redaction surface used for stream sinks before disk writes.

## Reliability and failure mode
- Open mode: append (`O_APPEND` semantics via `OpenOptions::append(true)`).
- One event per line; flush after each line.
- Open/write/reopen failures do not fail the run; a warning is emitted to stderr.

## Reopen on `SIGHUP`
On Unix, the sink requests file-handle reopen when `SIGHUP` is received, enabling log rotation workflows.

## Versioning policy
- Backward-compatible additions (new optional fields) keep `schema=event-log-v1`.
- Breaking shape changes must increment schema string (`event-log-v2`, etc.) and document migration notes here.

## Examples
```bash
# live tail
bench mini --task "..." --event-log runs/events.jsonl

tail -f runs/events.jsonl | jq -c '.event_type'

# per-instance progress in sweeps
bench swebench --dataset-path data.jsonl --output runs/sweep --event-log runs/sweep.events.jsonl
```

## Post-hoc querying

For filtering and aggregating a completed event log by type, instance, and time
window after a run/sweep finishes, see `bench events` (`docs/spec-events.md`).
`tail`/`watch` are the live counterparts; `bench events` is read-only and offline.
