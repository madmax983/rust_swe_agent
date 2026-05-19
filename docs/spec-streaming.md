# 🔭 Vantage: Spec for Streaming Trajectories

## 👤 User Story
"As a Developer running the agent locally, I want to stream agent steps and outputs in real-time, so that I can monitor progress without waiting for the full trajectory file to be written at the end of the run."

## ✅ Acceptance Criteria
- Must implement real-time streaming of trajectory steps (using a mechanism like SSE or WebSockets).
- Must emit step outcomes (e.g. commands run, exit codes, bash output) as they occur.
- Must handle network disconnects gracefully without crashing the underlying agent process.

## 🚫 Out of Scope
- Full UI web application (Phase 2).
- History playback of old runs over streaming endpoints (Phase 2).

---

## Transports

Maxwell's Daemon supports two complementary transports for per-step event streaming.
They are independently composable: either, both, or neither can be enabled on the same run.

### SSE (Server-Sent Events) — `--stream <host:port>`

The agent binds an HTTP server and pushes events over persistent SSE connections.

- **Direction**: clients dial **in** to the agent.
- **Use case**: interactive local runs where an operator opens a browser or `curl`.
- **Frame format**: standard SSE `event:` / `data:` pairs; `data:` is a JSON object with a `type` discriminator.
- **Disconnect safety**: a dropped client connection silently closes the per-connection task; the agent loop continues unaffected.

### Webhook (HTTP POST) — `--webhook-url <url>`

The agent POSTs each event to your HTTP listener as it occurs.

- **Direction**: the agent calls **out** to your server.
- **Use case**: headless CI runners, Docker containers, remote schedulers that cannot expose an inbound port.
- **Requires**: Cargo feature `webhook` (default-enabled since issue #324).

#### Webhook Envelope

Every POST body is a JSON object with this stable envelope:

```json
{
  "schema_version": { "major": 1, "minor": 0 },
  "run_id":         "<trajectory-name>",
  "event":          { "type": "bash_start", "step": 1, "command": "…", "timestamp": "…" },
  "emitted_at":     "2026-05-19T12:00:00Z"
}
```

For the final `RunEnded` event, an additional envelope-level field is appended:

```json
{
  …,
  "webhook_events_dropped": 0
}
```

`webhook_events_dropped` counts events silently lost due to buffer-full or HTTP failure.
If the count is ≥ 1 the binary prints a single `warn` line to stderr at run end.

#### Delivery Posture

| Property | Value |
|---|---|
| Buffer | Bounded MPSC (1 024 events by default) |
| Full-buffer behaviour | Drop-on-full, non-blocking; counted in `webhook_events_dropped` |
| HTTP timeout | 5 seconds per request |
| Retries | None (best-effort) |
| HTTP failures | Logged at `warn`; counted in `webhook_events_dropped`; run continues |
| Agent blocking | **Never** — delivery failures are fully absorbed by the sink |

#### Custom Headers — `--webhook-header "Name: Value"`

Repeatable. Injects arbitrary HTTP headers on every POST.
Header bytes are **not** echoed in logs and **not** passed through the secret redactor
(operator-supplied credentials are not observed content).

```bash
max mini --task "…" \
  --webhook-url "$SLACK_INCOMING_HOOK" \
  --webhook-header "Authorization: Bearer $TOKEN"
```

#### Secret Redaction

The redactor is applied to every string field of every event payload **before** POST,
identical to the trajectory write path (`stream` surface).
Configured `secret_literals`, `custom_patterns`, and structured-secret rules all apply.

#### Composing SSE + Webhook

```bash
max mini --task "…" \
  --stream 127.0.0.1:7878 \
  --webhook-url http://my-listener/events
```

Both transports observe the same `StreamEvent` sequence; the envelope wrapping differs.

#### Feature Flag

The `webhook` feature is **default-enabled** (`Cargo.toml: default = ["webhook"]`).
If you build with `--no-default-features` and pass `--webhook-url`, the binary prints
an actionable error and exits.
