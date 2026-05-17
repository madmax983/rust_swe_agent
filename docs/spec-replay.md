# `bench replay` — Spec and Drift-Detection Contract

`bench replay` re-runs a previously recorded agent trajectory using a
`DeterministicModel` that emits the original scripted assistant responses in
order. Starting with schema version **1.4**, each assistant message in a
trajectory stores a cryptographic fingerprint of the model-input that produced
it. During replay the harness recomputes the fingerprint before consuming each
scripted response and fails loudly when the inputs have drifted.

---

## Why it matters

`bench replay` is the $0 regression gate for prompt and harness changes. Before
fingerprinting, replay consumed scripted responses in order and reported success
even when the agent was asking the model completely different questions (changed
system prompt, changed observation format, changed tool schema, …). Fingerprint
enforcement turns replay into an honest cassette: it fails the moment the first
input diverges from what was recorded.

---

## Exit-code contract

| Code | `outcome_class`              | When emitted |
|-----:|------------------------------|--------------|
| 0    | `success`                    | Replay completed with no drift. |
| 1    | `internal_error`             | I/O failure, JSON parse error, or other unexpected error. |
| 2    | `usage_error`                | Trajectory has no fingerprints and `--allow-unfingerprinted` was not passed. |
| 9    | `replay_prompt_drift`        | At least one step's input fingerprint did not match the cassette. |
| 10   | `replay_response_exhausted`  | Scripted responses ran out before the agent finished (structural drift). |

---

## Fingerprint algorithm

**What is hashed:** The full input message vector passed to `model.query()` at
each step. Each message contributes only its `role` and `content`; `extra`
fields (cost, timestamp, raw API response) and `cache_hint` are **excluded**
because they carry per-run metadata that legitimately varies.

**Canonical form:** Compact JSON array where each element is
`{"content": "<string>", "role": "<string>"}` — keys in ASCII-alphabetical
order, no indentation, no trailing whitespace.

**Hash:** SHA-256 over the UTF-8 canonical string, truncated to the first 8
bytes, encoded as 16 lowercase hex characters.

**Stored fields** (on each assistant `MessageRecord.extra.model_call`):

```json
"model_call": {
  "input_fingerprint": "a1b2c3d4e5f60718",
  "input_canonical_size": 1234,
  "input_canonical": "[{\"content\":\"…\",\"role\":\"user\"},…]",
  "input_canonical_truncated": false
}
```

`input_canonical` stores the compact canonical JSON string up to 64 KiB per step
(capped with a `[truncated]` marker). It is used by `bench replay` to produce a
human-readable unified diff when a fingerprint mismatch is detected — the diff
shows exactly which messages changed and how.

`input_canonical_truncated` is `true` when the canonical was larger than the 64 KiB
cap; in that case the unified diff in the drift report is best-effort.

---

## Schema version

The `input_fingerprint`, `input_canonical_size`, `input_canonical`, and
`input_canonical_truncated` fields were introduced in trajectory schema version
**1.4**. Reader code is forward-compatible: trajectories without these fields
(schema < 1.4 or pre-versioning) are classified as _legacy_ and handled
according to the `--allow-unfingerprinted` flag.

---

## CLI flags

### `--allow-unfingerprinted`

Opt-in to replaying trajectories that have no stored fingerprints. A warning is
emitted to stderr for every unfingerprinted step. Without this flag, the first
step without a fingerprint causes exit code 2.

### `--report-only`

Run to completion despite any fingerprint drift. All divergent steps are
collected into `{output-dir}/replay-drift.json` and the command exits 0.
Useful for "show me everything that changed" diagnostics after a large
prompt refactor.

### `--drift-cap-bytes <N>` (default: 8192)

Maximum bytes of the unified diff string to include per divergent step in the
drift report. Excess is replaced with `[truncated]`.

---

## Drift report (`replay-drift.json`)

When drift is detected the harness writes a structured JSON report to
`{output-dir}/replay-drift.json` and echoes a summary to stderr.

**Schema:**

```json
{
  "steps": [
    {
      "step_index": 0,
      "recorded_fingerprint": "deadbeef00000000",
      "actual_fingerprint":   "a1b2c3d4e5f60718",
      "unified_diff": "--- recorded\n+++ actual\n-  \"content\": \"Fix the bug\"\n+  \"content\": \"Fix the bug (attempt 2)\"\n",
      "diff_truncated": false
    }
  ]
}
```

| Field | Description |
|-------|-------------|
| `step_index` | 0-based model-query index where drift was detected. |
| `recorded_fingerprint` | Hash stored in the cassette trajectory. |
| `actual_fingerprint` | Hash computed from the actual replay input. |
| `unified_diff` | Line-level unified diff (`-` = recorded, `+` = actual) of the pretty-printed canonical inputs, capped to `drift_cap_bytes` with a `[truncated]` marker if cut. |
| `diff_truncated` | `true` when the diff was capped **or** when the recorded canonical was already truncated in the cassette (best-effort diff). |

---

## Example workflow

### Recording a trajectory (normal `mini` run)

```bash
max mini --task "Fix the off-by-one in sort.py" \
  --output runs/baseline/
# → runs/baseline/fix-the-off-by-one-in-sort-py.traj.json
#   (each assistant message now carries extra.model_call.input_fingerprint)
```

### Replaying as a regression gate

```bash
# After changing a prompt template:
max replay \
  --trajectory-path runs/baseline/fix-the-off-by-one-in-sort-py.traj.json \
  --output runs/replay/
echo "exit: $?"
# → 0   if nothing drifted
# → 9   if any prompt changed (drift report at runs/replay/replay-drift.json)
```

### Diagnosing drift with `--report-only`

```bash
max replay \
  --trajectory-path runs/baseline/fix-the-off-by-one-in-sort-py.traj.json \
  --output runs/replay-diag/ \
  --report-only
# exits 0; runs/replay-diag/replay-drift.json lists every divergent step
```

### Accepting a legacy trajectory

```bash
max replay \
  --trajectory-path runs/old/pre-1.4-trajectory.traj.json \
  --output runs/replay/ \
  --allow-unfingerprinted
# exits 0 with per-step warnings; no fingerprint checking performed
```

---

## Out of scope

- Auto-rerecording or auto-healing cassettes on drift.
- Semantic / fuzzy equivalence ("these two prompts mean the same thing").
- Storing or comparing model *output* (this spec is strictly about *input* drift).
- Cross-model replay (replaying a Claude trajectory against an OpenAI agent run).
