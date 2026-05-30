# Spec: `max ui` — Local Sweep Browser

## Overview

`max ui` starts a read-only, single-binary HTTP server that lists every
trajectory in a finished sweep and renders each one as HTML.  It is the
fastest way to triage a completed sweep: one command opens a browser with
every instance's outcome, cost, and step count.  Clicking an instance
renders the full trajectory through the same `HtmlExporter` pipeline that
`bench inspect --format html` uses.

**This command requires the `ui-server` Cargo feature** (off by default).
See [Feature Gate](#feature-gate).

---

## Quickstart

```sh
# Build with the feature enabled:
cargo build --features ui-server

# Serve a sweep and open the browser immediately:
./target/debug/max ui --sweep runs/quickstart --port 0 --open
# ui ready at http://127.0.0.1:XXXXX
```

---

## CLI Reference

```
max ui --sweep <DIR> [--port <PORT>] [--bind <ADDR>] [--open]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--sweep <DIR>` | *(required)* | Path to a completed sweep directory containing `*.traj.json` files. |
| `--port <PORT>` | `0` | TCP port to listen on. `0` means the OS assigns an available port. |
| `--bind <ADDR>` | `127.0.0.1` | IP address to bind to. Default is loopback-only for safety. |
| `--open` | off | Best-effort: launch the system browser to the index URL after binding. Launch failures are logged as warnings and are not fatal. |

---

## Routes

| Path | Status | Content-Type | Description |
|------|--------|--------------|-------------|
| `GET /` | 200 | `text/html; charset=UTF-8` | Index page listing all discovered instances. |
| `GET /instance/<id>` | 200 | `text/html; charset=UTF-8` | Full trajectory rendered by `HtmlExporter`. `<id>` must match a discovered `instance_id` exactly (case-sensitive). |
| `GET /healthz` | 200 | `application/json` | `{"status":"ok"}`. Useful for process supervisors and smoke tests. |
| Anything else | 404 | `text/plain` | All other paths return 404. |

### Index page columns

Instances are listed in an HTML table, **sorted by `instance_id` ascending**.
Columns:

| Column | Source field | Format |
|--------|-------------|--------|
| `instance_id` | Filename stem of `*.traj.json` | Clickable link to `/instance/<id>` |
| `outcome` | `trajectory.info.outcome` | Raw string, e.g. `submitted`, `error` |
| `steps` | `trajectory.info.steps` | Integer |
| `total_cost_usd` | `trajectory.info.total_cost_usd` | 4 decimal places |
| `duration_seconds` | `trajectory.info.duration_secs` | 1 decimal place |

Missing values are displayed as `-`.

### Query parameters

None defined in this version.

### Error model

| Condition | HTTP status |
|-----------|-------------|
| Sweep directory missing or not a directory | Process exits before binding (exit 2) |
| Instance trajectory unreadable at serve time | 500 Internal Server Error |
| `instance_id` not in the discovered set | 404 Not Found |
| All other unknown paths | 404 Not Found |

---

## Security

### No directory traversal

`instance_id` values in `/instance/<id>` are **validated against the set of
instances discovered at startup**.  They are never used as filesystem paths.
A request for `/instance/../../etc/passwd` returns 404; the server never
opens any file path derived from the URL.

### No network egress

The server makes **zero outbound connections**.  It binds only to `--bind`
(default `127.0.0.1`) and never connects to any external host.  This is a
local-dev tool.  For remote access, SSH tunnel.

---

## Redaction

Every byte rendered to the browser passes through the same pipeline as
`bench inspect --format html` (see `docs/spec-secret-redaction.md`):

```
Redactor::default_enabled() + surface::EXPORT
```

The `HtmlExporter` applies this pipeline internally on every message field
before writing HTML.  The index page contains only structured metadata
(`instance_id`, `outcome`, `steps`, `cost`, `duration`), which are not
arbitrary agent-generated text.

---

## Sweep Discovery

At startup the server scans `--sweep` for all files matching `*.traj.json`.
Files that cannot be parsed as valid trajectory JSON are skipped with a
`WARN`-level log message.  The resulting list is sorted by `instance_id`
ascending.  The scan is performed once at startup; the server does not watch
for new trajectories while running.

---

## Feature Gate

`max ui` is compiled only when the `ui-server` Cargo feature is enabled:

```toml
# Cargo.toml
ui-server = ["html-export"]
```

When built **without** `ui-server`, any invocation of `max ui` exits
immediately with:

```
outcome_class: feature_unavailable
error: the `ui` command requires the `ui-server` Cargo feature, ...
```

Exit code: **24** (`feature_unavailable`).

---

## Exit Codes

| Code | Label | Trigger |
|------|-------|---------|
| 0 | `success` | Server stopped cleanly after SIGINT / Ctrl-C. |
| 2 | `usage_error` | Invalid `--bind` address, or `--sweep` directory not found. |
| 24 | `feature_unavailable` | Binary built without `ui-server` feature. |

---

## Lifecycle

1. Parse and validate CLI flags.
2. Scan `--sweep` directory, build the instance list, sort by `instance_id`.
3. Bind TCP listener on `--bind:--port` (port 0 = OS picks).
4. Print `ui ready at http://<bind>:<port>` to **stdout** on a single line.
5. If `--open`, attempt to launch the system browser (non-fatal on failure).
6. Serve requests until SIGINT / Ctrl-C.
7. Signal the accept loop to stop, drain in-flight connections, exit 0.

---

## Gap Analysis

| Harness | Browsable UI? |
|---------|---------------|
| mini-swe-agent | Python/Flask viewer with collapsible panels |
| SWE-agent (Princeton) | Interactive web UI used in published papers |
| OpenHands | HTML session logs written by default, linked in CI |
| Maxwell's Daemon (before this issue) | Terminal-only `bench inspect`; HTML exporter merged but not served |
| **Maxwell's Daemon (this issue)** | **`max ui` — static HTML sweep browser** |

---

## Out of Scope (first slice)

- Real-time streaming / live-tail of an in-flight sweep.
- Multi-sweep dashboards or cross-sweep comparison views.
- Editing, re-running, or annotating trajectories from the UI.
- Remote / hosted deployment, auth, RBAC, or TLS.
- Custom themes, syntax highlighting beyond what `HtmlExporter` emits.
- JavaScript frameworks; inline HTML + CSS only.
- New exporter formats (this wires the existing `HtmlExporter` only).
- Pagination or server-side filtering of the index.
