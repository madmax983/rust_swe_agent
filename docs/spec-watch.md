# `bench watch` — Live Single-Instance Trajectory Follower

`bench watch` attaches to a single in-flight sweep instance and streams its turns to stdout as they are written to disk, in the same human-readable format used by `bench inspect`.

## Command

```
bench watch \
  --sweep <DIR> \
  --instance <ID> \
  [--wait-secs N]     # default 30
  [--stall-secs N]    # default 120
  [--full]
  [--max-bytes N]
  [--ndjson]
```

## Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--sweep <DIR>` | required | Sweep output directory produced by `bench swebench`. |
| `--instance <ID>` | required | Instance id to follow. |
| `--wait-secs <N>` | `30` | Seconds to wait for the trajectory file to appear. Set to `0` to fail immediately if not found. |
| `--stall-secs <N>` | `120` | After this many seconds of no new turns, print a stall warning and keep following. |
| `--full` | off | Disable stdout/stderr truncation. |
| `--max-bytes <N>` | `4096` | Override the per-observation truncation threshold in bytes. Ignored when `--full` is set. |
| `--ndjson` | off | Emit one newline-delimited JSON object per turn instead of human-readable text. |

## Exit Codes

| Code | Meaning |
|------|---------|
| `0` | Instance reached a terminal outcome (`submitted`, `errored`, `cancelled`, `timed_out`). |
| `1` | Trajectory file not found after `--wait-secs`. |
| `2` | Sweep directory does not exist or is invalid. |
| `130` | Interrupted by SIGINT (Ctrl-C). |

## Waiting for the file

When `bench watch` is invoked before the worker has started the instance, it waits up to `--wait-secs` for the trajectory file to appear. A progress line is printed to stderr at most every 5 seconds:

```
[watch] waiting for trajectory file for `django-1234`...
```

Set `--wait-secs 0` to fail immediately if the file is not present.

## Stall detection

If no new turns appear for `--stall-secs` seconds after the last observed turn, a warning is printed to stderr:

```
[stalled: no new turns in 120s]
```

`bench watch` continues following the file — it does not exit on a stall, since the worker may recover.

## Redaction

All output passes through the same redactor used by `bench inspect`. Secret-shaped content (GitHub tokens, private keys, bearer tokens, environment API keys) is replaced with deterministic redaction markers before reaching stdout.

## NDJSON mode

With `--ndjson`, each turn is emitted as a single JSON object followed by a newline. The process flushes stdout after every line, making it suitable for piping to `jq` or other stream processors.

### Schema

```json
{
  "schema_version": "watch-1.0",
  "instance_id":    "django-1234",
  "turn_index":     0,
  "role":           "assistant",
  "message":        "I will start by reading the test file.",
  "bash":           null,
  "exit_code":      null,
  "stdout":         null,
  "stderr":         null
}
```

Fields present per `role`:

| `role` | Fields present |
|--------|---------------|
| `assistant` | `message` |
| `bash` | `bash`, `exit_code`, `stdout`, `stderr` |

Optional fields are omitted when `null`.

### Round-trip guarantee

Every NDJSON line is valid JSON and can be parsed by `serde_json`. The `schema_version` field is stamped on every object.

## Operator recipes

### Watch one instance while `bench tail` is running in another terminal

```bash
# Terminal 1: aggregate sweep progress
bench tail --sweep ./runs/my-sweep

# Terminal 2: follow a specific instance
bench watch --sweep ./runs/my-sweep --instance django__django-1234
```

### Pipe NDJSON to `jq` to filter assistant turns

```bash
bench watch --sweep ./runs/my-sweep \
            --instance astropy__astropy-9999 \
            --ndjson \
  | jq 'select(.role == "assistant") | .message'
```

### Fail fast if the file isn't ready yet

```bash
bench watch --sweep ./runs/my-sweep \
            --instance my-instance \
            --wait-secs 0
# exits 1 immediately if trajectory file not found
```

## Relation to other bench commands

| Command | Zoom level | Status |
|---------|-----------|--------|
| `bench tail` | Sweep-wide aggregate | Live |
| `bench watch` | Single instance | Live |
| `bench inspect` | Single instance | Post-mortem (completed trajectory) |
