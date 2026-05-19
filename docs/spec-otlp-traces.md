# OTLP Trace Export

Sweeps can export OpenTelemetry traces to any OTLP/HTTP collector so runs appear
in your existing observability stack (Jaeger, Tempo, Honeycomb, etc.).

## Quick start

```bash
bench swebench \
  --dataset dataset.jsonl \
  --output runs/ \
  --otlp-endpoint http://localhost:4318
```

Or set the standard env var instead of the CLI flag:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
bench swebench --dataset dataset.jsonl --output runs/
```

## Docker Compose example

```yaml
services:
  otel-collector:
    image: otel/opentelemetry-collector-contrib:latest
    ports:
      - "4318:4318"   # OTLP/HTTP
    volumes:
      - ./otel-config.yaml:/etc/otelcol-contrib/config.yaml

  jaeger:
    image: jaegertracing/all-in-one:latest
    ports:
      - "16686:16686"  # Jaeger UI
```

`otel-config.yaml`:
```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318

exporters:
  jaeger:
    endpoint: jaeger:14250
    tls:
      insecure: true

service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [jaeger]
```

Then open `http://localhost:16686` to browse sweep traces.

## Span structure

Each sweep produces two span types:

| Span | Attributes |
|------|-----------|
| `sweep` | `sweep.id`, `sweep.output_dir`, `sweep.instance_count`, timing |
| `instance` (child) | `gen_ai.system`, `gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `instance.id`, `instance.exit_reason`, `instance.resolved` |

`trace_id` is a deterministic 32-hex-char string derived from
`SHA-256(instance_id + sweep_id)` so rerunning the same instance in the same
sweep produces the same trace ID.

## Resilience

Export failures are non-fatal. The sweep completes normally; failed exports
are counted in `results.json` under `span_export_dropped` and logged at WARN.

## Inspecting trace IDs

`bench inspect` shows the `trace_id` for each instance:

```
$ bench inspect --sweep runs/ --instance repo__owner__1
instance_id:      repo__owner__1
exit_reason:      submitted
trace_id:         a1b2c3d4e5f60708a1b2c3d4e5f60708
...
```
