# OTLP Metrics Export

Sweeps can export OpenTelemetry metrics to any OTLP/HTTP collector so progress, cost, and wallclock time can be monitored dynamically (e.g. in Prometheus, Grafana, Datadog).

## Quick start

To enable metrics export, configure an OTLP endpoint:

```bash
bench swebench \
  --dataset dataset.jsonl \
  --output runs/ \
  --otlp-endpoint http://localhost:4318 \
  --otlp-metrics-interval-secs 15
```

Or set the standard env vars instead of the CLI flags:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_METRIC_EXPORT_INTERVAL=15000  # in milliseconds
bench swebench --dataset dataset.jsonl --output runs/
```

## Instruments & Gauges

The following metrics are exported at the configured interval as gauges under the `maxwells-daemon` service name:

| Metric | Type | Description |
|--------|------|-------------|
| `instances_total` | Integer Gauge | Total run slots/instances configured for the sweep. |
| `instances_completed` | Integer Gauge | Number of slots that have finished (includes skipped, resolved, failed, or budget-halted/cancelled). |
| `instances_resolved` | Integer Gauge | Number of slots that completed successfully and resolved the issue. |
| `instances_failed` | Integer Gauge | Number of slots that completed but failed to resolve the issue (includes join errors and budget-halted/cancelled slots). |
| `instances_in_flight` | Integer Gauge | Number of slots currently running in parallel. |
| `cumulative_cost_usd` | Double Gauge | Total API spend in USD since the sweep started. |
| `resolved_rate` | Double Gauge | Fraction of completed slots that were resolved successfully (`instances_resolved / instances_completed`). |
| `error_rate` | Double Gauge | Fraction of completed slots that failed or errored (`instances_failed / instances_completed`). |
| `sweep_wallclock_seconds` | Integer Gauge | Wallclock duration of the sweep in seconds. |

## Resource Attributes

Every metric payload is associated with resource attributes identifying the run:

- `sweep_id`: A stable hash derived from the output directory path and start time.
- `model`: Configured primary LLM model name (e.g. `claude-3-5-sonnet`).
- `dataset`: Resolving name or local path of the dataset (e.g. `verified/test`).
- `service.name`: `maxwells-daemon`
- `service.version`: Package version of the daemon.

## Grafana & Prometheus Query Examples

Here are common PromQL queries for dashboard panels:

### Live Sweep Cost
```promql
cumulative_cost_usd{sweep_id="<sweep_id>"}
```

### Live Success Rate (%)
```promql
resolved_rate{sweep_id="<sweep_id>"} * 100
```

### Concurrency Level
```promql
instances_in_flight{sweep_id="<sweep_id>"}
```

### Progress Completion (%)
```promql
(instances_completed{sweep_id="<sweep_id>"} / instances_total{sweep_id="<sweep_id>"}) * 100
```

## Implementation Details

- **Zero Overhead**: When no OTLP endpoint is configured, metrics export is a pure no-op. No background loop is spawned, and no network sockets/connections are opened.
- **Robustness**: Export failures are logged at `WARN` level and do not interrupt or fail the sweep.
- **Final Flush**: Upon completion, cancellation, or systemic halt, the background exporter is immediately stopped and a final synchronous export of the terminal state is pushed to ensure accurate final values are archived.
