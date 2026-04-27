# `bench inspect` spec

`bench inspect` is a read-only post-run debugging surface for SWE-bench sweeps.
It renders one trajectory as a human-readable transcript, or lists matching
instances in filter mode.

## CLI

```bash
rust-swe-agent bench inspect --sweep <dir> --instance <instance_id>
rust-swe-agent bench inspect --sweep <dir> --instance <instance_id> --full
rust-swe-agent bench inspect --sweep <dir> --instance <instance_id> --format json
rust-swe-agent bench inspect --sweep <dir> --filter resolved=false
rust-swe-agent bench inspect --sweep <dir> --filter failure_category=step_limit
```

### Inputs

* `--sweep`: output directory from `bench swebench`.
* `--instance`: render one instance transcript.
* `--filter`: summary-table mode (`resolved=true|false` or `failure_category=<snake_case>`).
* `--format`: `text` (default) or `json`.
* `--full`: disable stdout/stderr truncation in transcript mode.

Exactly one of `--instance` or `--filter` is required.

## Transcript behavior

Header fields:

* `instance_id`
* `model`
* `outcome`
* `failure_category`
* `total_cost_usd`
* token totals (`prompt`, `completion`)
* `resolved` when `evaluation.json` exists

Step rendering:

* Assistant messages render as `[step N] assistant` and raw model content.
* Bash observations render as `[step N] bash` with:
  * command (inferred from prior assistant action),
  * exit code,
  * stdout/stderr (possibly truncated).

Truncation defaults:

* Truncate when stdout/stderr exceeds **40 lines** or **2 KiB**.
* Marker: `… [N more lines, full output at trajectory.json#/steps/N]`.
* `--full` disables truncation.

## Worked examples

### Resolved instance (text)

```text
=== bench inspect ===
instance_id:      astropy__astropy-12907
model:            claude-opus-4-7
outcome:          submitted
failure_category: none
total_cost_usd:   0.123456
tokens:           prompt=45123 completion=3987
resolved:         true

[step 2] assistant
```bash
pytest -q
```

[step 3] bash
$ pytest -q
exit_code: 0
stdout:
2 passed in 0.74s
```

### Unresolved instance (text)

```text
=== bench inspect ===
instance_id:      astropy__astropy-12908
model:            claude-opus-4-7
outcome:          error
failure_category: patch_apply_failed
total_cost_usd:   0.098732
tokens:           prompt=38011 completion=3502
resolved:         false

[step 8] assistant
```bash
git apply fix.patch
```

[step 9] bash
$ git apply fix.patch
exit_code: 1
stderr:
error: patch failed: src/core.py:10
… [77 more lines, full output at trajectory.json#/steps/9]
```
