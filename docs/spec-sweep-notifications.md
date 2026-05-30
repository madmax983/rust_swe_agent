# Sweep-Level Webhook Notifications

**Status:** Implemented  
**Feature gate:** `webhook` (enabled by default in `Cargo.toml`)

## Overview

`bench swebench` can POST structured JSON events to an HTTP endpoint at
key milestones during a sweep: when the sweep starts, every 25 %
completion milestone, when each instance completes, if the systemic-halt
circuit breaker fires, when cumulative cost crosses 25/50/75/100 % of the
budget cap, and when the sweep ends.

This is separate from (and complementary to) the per-step `--webhook-url`
transport that fires an event for every agent turn.

## CLI Flags

| Flag | Description |
|------|-------------|
| `--notify-webhook <URL>` | HTTP(S) endpoint that receives sweep events (optional). |
| `--notify-webhook-headers <HEADER>` | Additional headers, repeatable. Format: `Name: Value`. Useful for `Authorization: Bearer $TOKEN`. |

```bash
bench swebench \
  --dataset-path swe-bench-lite.jsonl \
  --model claude-opus-4-7 \
  --output runs/my-sweep \
  --notify-webhook https://hooks.example.com/sweep-events \
  --notify-webhook-headers "Authorization: Bearer $TOKEN"
```

## Event Taxonomy

All events are delivered inside a versioned envelope:

```json
{
  "schema_version": { "major": 1, "minor": 0 },
  "sweep_id": "<uuid-v4>",
  "event": { "type": "<event_type>", ...fields },
  "emitted_at": "<RFC 3339>"
}
```

### `sweep_started`

Fired once before the first instance is dispatched.

```json
{ "type": "sweep_started", "total_instances": 300, "model": "claude-opus-4-7" }
```

### `sweep_milestone`

Fired at 25 %, 50 %, 75 % completion. Not fired again if the same threshold
is hit twice (e.g., if completion jumps from 24 % to 51 %).

```json
{ "type": "sweep_milestone", "completed_share": 0.25, "completed": 75, "total": 300 }
```

### `instance_completed`

Fired for every instance, whether it resolved or failed.

```json
{
  "type": "instance_completed",
  "instance_id": "django__django-11099",
  "resolved": true,
  "failure_category": null,
  "cost_usd": 0.0412,
  "duration_secs": 47.3
}
```

`failure_category` is `null` on success or one of the `FailureCategory`
variants (e.g., `"format_error"`, `"timeout"`) on failure.

### `systemic_halt_tripped`

Fired once when the circuit breaker halts the sweep early.

```json
{
  "type": "systemic_halt_tripped",
  "dominant_category": "bad_credentials",
  "share": 1.0
}
```

### `cost_threshold_crossed`

Fired when cumulative cost crosses 25 %, 50 %, 75 %, or 100 % of
`--cost-limit-usd`. Only fires if `--cost-limit-usd` is set.

```json
{
  "type": "cost_threshold_crossed",
  "threshold_share": 0.50,
  "cumulative_cost_usd": 12.50,
  "cost_limit_usd": 25.00
}
```

### `sweep_completed`

Fired once after the last result is written.

```json
{
  "type": "sweep_completed",
  "total_resolved": 87,
  "total_attempted": 300,
  "total_cost_usd": 24.11,
  "wallclock_secs": 3241.7,
  "terminal_reason": "finished",
  "webhook_events_dropped": 0
}
```

`webhook_events_dropped` is the number of events that were silently
dropped because the internal buffer was full (see Delivery Guarantees).

## Delivery Guarantees

- **Best-effort, non-blocking.** The sweep loop never waits for a POST to
  complete. If the endpoint is slow or unreachable, the sweep continues.
- **Bounded buffer.** Events are queued in a 1024-slot channel. If the channel
  is full (endpoint too slow), the event is dropped and counted; the final
  `sweep_completed` event reports the total.
- **5-second timeout** per POST attempt. No retries on failure.
- **Secrets redacted** before every POST. The same `Redactor` that
  scrubs `--webhook-url` events is applied to all sweep notification
  payloads.
- **HTTP errors** do not propagate to the sweep; they are logged at
  `warn` level only.

## Doctor Probe

When `--notify-webhook` is provided, `bench doctor` performs a non-fatal
reachability check before running the sweep. It POSTs a synthetic
`doctor_probe` event and prints:

```
[bench doctor] Webhook reachability: https://... — HTTP 200
```

If the endpoint is unreachable the line says so (e.g., `connection refused`)
but `bench doctor` still exits 0 — the reachability check is advisory.

## Schema Version

The `schema_version` field uses the same `{"major": N, "minor": N}` struct
as the per-step webhook transport. The current version is `{"major":1,"minor":0}`.
Minor version increments are backward-compatible additions; major version
increments are breaking changes.

## Related

- [`docs/spec-streaming.md`](spec-streaming.md) — per-step SSE / webhook / event-log transports
- [`docs/spec-systemic-halt.md`](spec-systemic-halt.md) — circuit-breaker behaviour
- [`docs/spec-secret-redaction.md`](spec-secret-redaction.md) — redaction rules
