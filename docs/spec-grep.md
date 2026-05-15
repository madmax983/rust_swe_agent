# `bench grep` — Trajectory Search

## Overview

`bench grep` scans every trajectory in a sweep for a regex pattern and prints
structured hits. It reads only on-disk artifacts and **never calls a model
provider** (zero-cost guarantee).

## Usage

```
bench grep --sweep <DIR> [OPTIONS] <PATTERN>
```

### Required

| Argument | Description |
|---|---|
| `--sweep <DIR>` | Completed sweep directory produced by `bench swebench` |
| `<PATTERN>` | Regex pattern to search across trajectory messages |

### Optional

| Flag | Default | Description |
|---|---|---|
| `--role <ROLE>` | all roles | Restrict search to specific message roles (repeatable). E.g. `--role assistant --role user` |
| `--field <FIELD>` | `content` | Restrict to a specific message field: `content` (message text) or `actions` (bash commands) |
| `--instance-ids <IDS>` | all | Comma-separated instance IDs to include |
| `--exclude-instance-ids <IDS>` | none | Comma-separated instance IDs to exclude |
| `--outcome <OUTCOME>` | all | Filter to instances with one of the specified outcomes (repeatable). E.g. `--outcome error` |
| `--context <N>` | `80` | Characters of context before and after each match in the snippet |
| `--max-matches-per-instance <K>` | unbounded | Cap per-instance match count to prevent flooding stdout |
| `--format <FORMAT>` | `text` | Output format: `text` or `json` |

## Output Format

### Text (default)

One tab-separated line per match:

```
instance_id\tturn_index\trole\tsnippet
```

Example:

```
instance-a	1	user	FAILED tests/test_foo.py::test_bar - ImportError: cannot import name 'foo'
instance-a	2	assistant	I see the ImportError. Let me fix the import.
instance-b	1	user	ImportError: No module named 'bar'
```

### JSON (`--format json`)

One JSON object per match, newline-delimited (JSONL):

```json
{"instance_id":"instance-a","turn_index":1,"role":"user","snippet":"...ImportError..."}
{"instance_id":"instance-a","turn_index":2,"role":"assistant","snippet":"...ImportError..."}
```

Each object has the fields:

| Field | Type | Description |
|---|---|---|
| `instance_id` | string | Instance identifier |
| `turn_index` | integer | Zero-based index of the message turn in the trajectory |
| `role` | string | Message role: `assistant` or `user` |
| `snippet` | string | Matched text with surrounding context (controlled by `--context`) |

## Exit Codes

| Code | Meaning |
|---|---|
| 0 | At least one match was found |
| 1 | No matches found (mirrors the `grep` convention) |
| 2 | Usage or configuration error (e.g., invalid regex, bad `--field`) |

This exit-code contract makes `bench grep` composable in shell pipelines and CI
gates. For example:

```bash
bench grep --sweep ./my-sweep "ImportError" && echo "found import errors"
```

## Redaction

Matches honor the existing redaction layer. Any content that would be masked by
`bench inspect` (e.g., GitHub tokens, Bearer tokens, configured secret literals)
is also masked in `bench grep` output. The search runs **on already-redacted
text**, so secrets are never matched against and never appear in snippets.

## Examples

```bash
# Which instances hit ImportError?
bench grep --sweep ./sweep "ImportError"

# Which turns ran `pytest -x`?
bench grep --sweep ./sweep --field actions "pytest -x"

# Show only errored instances that have the apology pattern
bench grep --sweep ./sweep --outcome error "I apologize"

# Restrict to specific instances
bench grep --sweep ./sweep --instance-ids "django__django-1234,django__django-5678" "TypeError"

# Cap output to avoid flooding stdout on a broad pattern
bench grep --sweep ./sweep --max-matches-per-instance 3 "."

# JSON output for downstream processing
bench grep --sweep ./sweep --format json "ImportError" | jq '.instance_id' | sort | uniq -c
```

## Performance

The command scans trajectories on demand without a persistent index. For a
300-trajectory sweep with a non-pathological regex, the scan completes in well
under 5 seconds on a developer laptop.

## Scope

- Operates on a single sweep directory at a time.
- Regex-only (no semantic/embedding search).
- Read-only; does not modify any artifacts.
