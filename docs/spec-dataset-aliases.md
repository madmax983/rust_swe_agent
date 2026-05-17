# Dataset Aliases and Cache

## Overview

`bench swebench`, `bench doctor`, and `bench forecast` accept datasets in two
forms:

1. **Local JSONL path** (existing behaviour, no change): `--dataset-path path/to/data.jsonl`
2. **Named alias + split** (new): `--dataset verified --split test`

Named aliases are resolved against a local on-disk cache.  The cache is warm
when the matching file already exists; otherwise the command fails with an
actionable message that tells the operator exactly which file to place and
where.

Supported aliases: `full`, `lite`, `verified`
Supported splits: `train`, `test` (default), `dev`

---

## Quick-start examples

### 1. Local fixture (no key, no network)

Use `--dataset-path` to point at any local JSONL file.  Good for CI fixtures
and offline development.

```bash
bench swebench \
  --dataset-path tests/fixtures/my_instances.jsonl \
  --output runs/local-smoke \
  --model claude-opus-4-7 \
  --step-limit 10 \
  --sample 3 --seed 42
```

### 2. Cached `verified` calibration sample

Populate the cache once, then every subsequent run is fully offline.

**Step 1 – populate the cache** (run once per machine):

Download the SWE-bench Verified test split JSONL from
<https://www.swebench.com/SWE-bench/guides/datasets/> and place it at:

```
~/.cache/max/datasets/verified/test.jsonl
```

**Step 2 – launch a calibration sweep** (no network access required):

```bash
bench swebench \
  --dataset verified --split test \
  --output runs/verified-calibration \
  --model claude-opus-4-7 \
  --sample 10 --seed 42
```

### 3. Offline cache-hit run

Once the cache is warm, sweeps run completely offline:

```bash
bench swebench \
  --dataset lite --split test \
  --output runs/lite-sweep \
  --model claude-opus-4-7
```

The run will fail immediately with a clear message if the cache file is missing
or corrupt — no tasks will be launched.

---

## Cache directory

The default cache root is `~/.cache/max/datasets/`.  Override it
with `--dataset-cache-dir /custom/path`.

Layout:

```
~/.cache/max/datasets/
  verified/
    test.jsonl      # SWE-bench Verified test split
    dev.jsonl
  lite/
    test.jsonl      # SWE-bench Lite test split
  full/
    test.jsonl      # SWE-bench full test split
```

---

## Checking cache status without launching tasks

`bench doctor` validates the dataset selector and reports the cache status
without starting any task:

```bash
bench doctor \
  --dataset verified --split test \
  --output /tmp/unused
```

Output example (cache hit):

```
[OK]  dataset.cache   cache hit: ~/.cache/max/datasets/verified/test.jsonl (500 instances)
[OK]  dataset.parse   valid jsonl, selected 500 instances
```

Output example (cache miss):

```
[WARN] dataset.cache   cache miss: expected file at ~/.cache/max/datasets/verified/test.jsonl
```

---

## Provenance in sweep artifacts

Every `results.json` records the dataset provenance under `manifest.dataset`:

```json
{
  "manifest": {
    "dataset": {
      "path": "/home/user/.cache/max/datasets/verified/test.jsonl",
      "sha256": "abc123...",
      "instance_count": 500,
      "source_kind": "named",
      "alias": "verified",
      "split": "test",
      "source_revision": "sha256:abc123...",
      "cache_path": "/home/user/.cache/max/datasets/verified/test.jsonl",
      "selected_row_count": 500,
      "post_filter_row_count": 10
    }
  }
}
```

For local-path datasets, `source_kind` is `"local"` and `alias`, `split`,
`cache_path` are absent.

---

## Error reference

| Scenario | Exit code | Message contains |
|---|---|---|
| Missing `--dataset-path` and `--dataset` | 1 | `one of --dataset-path or --dataset is required` |
| Both `--dataset-path` and `--dataset` | 1 | `mutually exclusive` |
| Invalid alias string | 1 | `unknown dataset alias` + list of valid aliases |
| Invalid split string | 1 | `unknown split` + list of valid splits |
| Cache miss | 1 | alias, split, expected cache path, download instructions |
| Corrupt cache entry | 1 | `corrupt`, file path, parse error |
| Local file unreadable | 1 | OS I/O error |
