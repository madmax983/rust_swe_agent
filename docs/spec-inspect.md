# `bench inspect` spec

`bench inspect` is a read-only post-run debugging surface for SWE-bench sweeps.
It renders one trajectory as a human-readable transcript, or lists matching
instances in filter mode.

## CLI

```bash
rust-swe-agent bench inspect --sweep <dir> --instance <instance_id>
rust-swe-agent bench inspect --sweep <dir> --instance <instance_id> --full
rust-swe-agent bench inspect --sweep <dir> --instance <instance_id> --format json
rust-swe-agent bench inspect --sweep <dir> --instance <instance_id> --show-expected
rust-swe-agent bench inspect --sweep <dir> --filter resolved=false
rust-swe-agent bench inspect --sweep <dir> --filter failure_category=step_limit
rust-swe-agent bench inspect --diff <baseline.traj.json> <candidate.traj.json>
rust-swe-agent bench inspect --diff <baseline.traj.json> <candidate.traj.json> --format json
rust-swe-agent bench inspect --diff <baseline.traj.json> <candidate.traj.json> --format unified
rust-swe-agent bench inspect --diff <baseline.traj.json> <candidate.traj.json> --show-noise
rust-swe-agent bench compare --baseline <dir> --candidate <dir> --inspect-diff <instance_id>
rust-swe-agent bench compare --baseline <dir> --candidate <dir> --emit-diff-script <out.sh>
```

### Inputs

* `--sweep`: output directory from `bench swebench`.
* `--instance`: render one instance transcript.
* `--filter`: summary-table mode (`resolved=true|false` or `failure_category=<snake_case>`).
* `--format`: `text` (default) or `json`.
* `--full`: disable stdout/stderr truncation in transcript mode.
* `--show-expected`: also print `PASS_TO_PASS` / `FAIL_TO_PASS` expected-test groupings read
  from `<sweep>/dataset.jsonl`. Silently skipped when the file is absent.
* `--diff`: compare two trajectory JSON files for the same `instance_id`.
* `--show-noise`: in diff mode, include whitespace-only and timestamp-only differences.
* `--inspect-diff`: compare sugar that resolves `<instance_id>.traj.json` or `<instance_id>/run-1.traj.json` inside both sweep directories.
* `--emit-diff-script`: writes one `bench inspect --diff` invocation per regressed instance.

Exactly one of `--instance` or `--filter` is required.
Diff mode is separate and cannot be combined with `--sweep`, `--instance`, or
`--filter`.

## Transcript behavior

Header fields:

* `instance_id`
* `model`
* `outcome`
* `failure_category`
* `total_cost_usd`
* `actual_cost_usd` and `actual_cost_source` when available
* `baseline_cost_usd` and `baseline_cost_model` when available
* token totals (`prompt`, `completion`)
* `resolved` when `evaluation.json` exists
* `Failing tests` section when the instance is unresolved (see below)
* `PASS_TO_PASS` / `FAIL_TO_PASS` sections when `--show-expected` is set

### Failing tests section

When the instance is **unresolved** and `evaluation.json` is present, a
`Failing tests` section follows the `resolved` line:

* When the evaluator reported individual test names:
  ```
  Failing tests (N):
    tests/test_widgets.py::test_widget_render
    tests/test_widgets.py::test_widget_init
  ```
* When test names are unavailable (evaluator error, timeout, build failure, etc.):
  ```
  Failing tests: <eval_error>
  ```
  The reason is taken from the `eval_exit_reason` field in `evaluation.json`,
  or falls back to `failure_category` from the trajectory.

When the instance is **resolved**, no `Failing tests` section is printed
(silent on the happy path).

### `--show-expected` sections

When `--show-expected` is set and `<sweep>/dataset.jsonl` contains a record for
the instance, two additional sections are printed:

```
PASS_TO_PASS (N):
  tests/test_core.py::test_existing
FAIL_TO_PASS (M):
  tests/test_core.py::test_targeted
```

The lists are read directly from the `PASS_TO_PASS` and `FAIL_TO_PASS` arrays
in the SWE-bench instance record. If `dataset.jsonl` is absent or has no
matching record, both sections are silently omitted.

### `failing_tests` JSON shape

`--format json` includes a `failing_tests` field on the instance record when
the instance is unresolved:

```json
{
  "failing_tests": {
    "tests": ["tests/test_widgets.py::test_widget_render"],
    "source": "evaluator",
    "reason": ""
  }
}
```

* `source`: `"evaluator"` when names come from evaluator output; `"unavailable"` otherwise.
* `reason`: human-readable explanation when `source == "unavailable"`; empty string when `source == "evaluator"`.
* `tests`: array of test names in the order reported by the evaluator; empty when `source == "unavailable"`.

The `failing_tests` field is absent for resolved instances.

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

## Diff behavior

`bench inspect --diff` is read-only. Both trajectories must resolve to the
same `instance_id`. The ID is read from `info.instance_id` when present and
otherwise derived from the trajectory filename (`<id>.traj.json`) or nested
directory (`<id>/trajectory.json` or `<id>/run-<k>.traj.json`). A mismatch
exits non-zero with a clear error.

The diff aligns by an explicit `(role, logical_step_index)` key, not by raw
message position. A normal action turn groups the pending `system`/`user`
prompt, assistant message, and following tool result into one `assistant`
step. Step indices are role-local: orphan `prompt`/`tool` records are retained
under their own roles, but they do not consume assistant turn indices, so later
assistant turns still align. Matching steps are collapsed as:

```text
[step 0 - identical] role=assistant
```

Divergent steps expand field-by-field in side-by-side columns. When terminal
color is forced with `CLICOLOR_FORCE=1` and `NO_COLOR` is unset, changed field
labels are highlighted with ANSI color. The compared fields are:

* `prompt.content`
* `assistant.content`
* `bash.command`
* `tool.exit_code`
* `tool.stdout`
* `tool.stderr`

Whitespace-only and timestamp-only differences are suppressed by default.
`--show-noise` re-enables them. Text, JSON, and unified formats all use the
same filtered field-diff semantics.

The text header summarizes `instance_id`, both paths, baseline/candidate
failure categories, attempts, cost, token counts, total steps, and the first
divergent step with its role. `--format json` emits:

```json
{
  "instance_id": "<id>",
  "header": {
    "baseline_failure_category": "none",
    "candidate_failure_category": "step_limit",
    "first_divergent_step_index": 3,
    "first_divergent_step_role": "assistant"
  },
  "steps": [
    {
      "index": 3,
      "role": "assistant",
      "status": "diverge",
      "baseline": {},
      "candidate": {},
      "diff_fields": ["assistant.content", "tool.stdout"]
    }
  ]
}
```

`--format unified` emits a line-oriented unified diff over canonical step text
for pager workflows, including classic `---`/`+++` headers, `@@ -a,b +c,d @@`
hunk ranges, unchanged context lines, and `-`/`+` changed lines.

`bench compare --inspect-diff <instance_id>` resolves the two trajectory files
inside the baseline and candidate sweep directories and renders the same diff.
`bench compare --emit-diff-script <out.sh>` writes a shell script containing
one diff command for each regressed instance; it does not run the script.

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
