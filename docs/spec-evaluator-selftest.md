# spec: bench evaluator-selftest

## Purpose

`bench evaluator-selftest` is a **zero-cost preflight** that verifies the
evaluator pipeline is wired up correctly before you spend money on inference.

It takes each instance's gold `patch` field from the dataset as the "agent
output" and asserts that the evaluator marks it resolved. If gold does not
resolve gold, the harness is broken — no model run will rescue that.

This is the cheapest possible smoke for the most expensive failure: paying for
a sweep whose verdict was never wired up correctly.

---

## When to run it

Run `bench evaluator-selftest` whenever any of the following changes:

| Event | Why |
|---|---|
| Switching to a new dataset version | The gold patches change; the evaluator may not agree with the new dataset. |
| Updating the evaluator image / container | Container image rot silently breaks grading. |
| Moving to a new machine or CI environment | Missing toolchain, path, or network access. |
| Updating the harness binary | Any of the evaluator glue code could have drifted. |

Running it before every paid sweep is low cost and catches silent failures.

---

## Usage

```
bench evaluator-selftest \
  --dataset-path /data/swe-bench_lite/test.jsonl \
  --output ./selftest-out
```

### Slicing flags

The command supports the same dataset slicing flags as `bench swebench`:

| Flag | Description |
|---|---|
| `--instance-ids id1,id2` | Comma-separated instance ids or `@file.txt` |
| `--limit N` | Keep at most N instances |
| `--sample N --seed S` | Reproducibly random-sample N instances |

### Output formats

| Flag | Output |
|---|---|
| `--format text` (default) | One-line headline + ranked table of non-resolved instances |
| `--format json` | JSON artifact emitted to stdout |

---

## Outputs

### Stdout (text mode)

```
evaluator self-test: 50/50 resolved on swe-bench_lite_test.jsonl

```

If any instances do not resolve:

```
evaluator self-test: 48/50 resolved on swe-bench_lite_test.jsonl

Non-resolved instances:
instance_id                               exit_reason
----------------------------------------------------------------------
astropy__astropy-12345                    gold_patch_missing
django__django-99999                      evaluator_failed
```

### `evaluator_selftest.json`

Written to `{--output}/evaluator_selftest.json`.

```json
{
  "schema_version": 1,
  "dataset_path": "/data/swe-bench_lite/test.jsonl",
  "dataset_sha256": "abcdef...",
  "harness_git_sha": "a1b2c3d...",
  "timestamp_utc": "2026-05-13T10:00:00Z",
  "evaluator_backend": "none",
  "instances": [
    {
      "instance_id": "astropy__astropy-12345",
      "resolved": true,
      "evaluator_exit_reason": "resolved",
      "evaluator_duration_ms": 12
    }
  ],
  "totals": {
    "instances_total": 1,
    "instances_resolved": 1,
    "instances_unresolved": 0,
    "instances_errored": 0
  }
}
```

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Every selected instance resolved — safe to launch a sweep. |
| `3` | At least one instance errored (e.g. `gold_patch_missing`, evaluator crash). |
| `4` | At least one instance was unresolved (evaluator ran but said "no"). |

---

## Per-instance `evaluator_exit_reason` values

| Value | Meaning |
|---|---|
| `resolved` | Gold patch resolved. |
| `gold_patch_missing` | The dataset row has no non-empty `patch` field. The instance is counted as `errored`. |
| `evaluator_failed` | The evaluator was invoked but could not grade the patch (reserved for forced-fail test fixtures). |

---

## Expected runtime

| Backend | Typical per-instance time |
|---|---|
| `none` (default) | < 1 ms — no subprocess, just field inspection |
| `sb-cli` (planned) | Same as a real `bench evaluate` run — depends on the evaluator and network |

---

## Worked example with a fixture instance

Given a dataset file `fixture.jsonl`:

```json
{"instance_id": "example__repo-001", "patch": "diff --git a/fix.py b/fix.py\n--- a/fix.py\n+++ b/fix.py\n@@ -1 +1 @@\n-broken\n+fixed\n"}
{"instance_id": "example__repo-002"}
```

Running:

```
bench evaluator-selftest \
  --dataset-path fixture.jsonl \
  --output ./selftest-out
```

Produces stdout:

```
evaluator self-test: 1/2 resolved on fixture.jsonl

Non-resolved instances:
instance_id                               exit_reason
----------------------------------------------------------------------
example__repo-002                         gold_patch_missing
```

Exits with code `3` because one instance errored (`gold_patch_missing`).

---

## Gate sweeps on a passing self-test

In CI, run this before `bench swebench`:

```bash
bench evaluator-selftest \
  --dataset-path "$DATASET" \
  --output ./selftest-out \
  --limit 5
# Non-zero exit aborts the pipeline; zero means all 5 gold patches resolved.
bench swebench --dataset-path "$DATASET" --output ./sweep-out ...
```

---

## Related commands

- [`bench doctor`](../docs/spec-checkpointing.md) — validates dataset, env, model credential
- [`bench evaluate`](../docs/spec-evaluation.md) — scores a completed sweep with the evaluator
