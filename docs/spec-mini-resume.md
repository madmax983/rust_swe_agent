# `mini --resume` — Interrupted Run Recovery

`mini --resume <path>` continues an interrupted `mini` run from its last
persisted checkpoint.  The operator points the flag at the `.traj.json` file
produced by a crashed or cancelled run, and the harness reloads the message
history, budget counters, and configuration from that file before querying the
model for the next turn only.

---

## Why it exists

A `mini` run that crashes on turn 14 of 15 after burning several dollars of
model spend leaves a parseable partial trajectory on disk (the per-step WAL
written by the checkpointing layer).  Without `--resume`, the operator must
re-run from scratch and pay for the same prefix tokens again.  With `--resume`,
the harness trusts the on-disk prefix and asks the model only for the *new*
turns.

---

## CLI Surface

```
max mini --resume <path>          # continue from a partial trajectory
max mini --resume <path> --resume-allow-step-bump --step-limit 80
```

### `--resume <path>`

Path to a `.traj.json` file with `info.partial: true`.

- Mutually exclusive with `--task`, `--render-only`, and `--trajectory-name`;
  supplying any of those alongside `--resume` exits `2` (`usage_error`) with a
  diagnostic naming the conflicting flag.
- The `--task` flag is **not** accepted on resume; the task comes from
  `trajectory.info.task`.

### `--resume-allow-step-bump`

Permits overriding `--step-limit`, `--task-timeout-secs`, or
`--per-task-budget-usd` on the resume invocation.  Useful when the original
run hit one of those caps and you want to give the agent more headroom.
Without this flag, attempting to set any of those flags on resume exits `2`.

---

## Prefix-Trust Rules

The on-disk trajectory is the **single source of truth** for:

| Field                   | Source on resume                              |
|-------------------------|-----------------------------------------------|
| `task`                  | `trajectory.info.task`                        |
| `model_name`            | `trajectory.info.model_name`                  |
| message history         | `trajectory.messages` (verbatim)              |
| `steps` counter         | `trajectory.info.steps`                       |
| `actual_cost_usd`       | `trajectory.info.actual_cost_usd`             |
| token counters          | `trajectory.info.token_usage`                 |

Operator CLI flags that would override any of the above are silently
superseded by the trajectory's recorded values, with the exception of the
three cap flags when `--resume-allow-step-bump` is set.

---

## Manifest Extension

On resume, the trajectory's `info.resume_history` array is extended with a
`ResumeRecord`:

```json
{
  "original_started_at": "2026-01-15T10:00:00Z",
  "resumed_at": "2026-01-15T10:14:00Z",
  "prior_steps": 14,
  "prior_cost_usd": 2.87
}
```

This provides a full audit trail for trajectories that were resumed one or
more times.

---

## On-Disk Trajectory Update

The original trajectory path is updated **in place** using an atomic
temp-file + rename.  No `.resumed.traj.json` sibling is created.  The new
turns extend `messages`; the `info.resume_history` is augmented as described
above; and the final `info.partial` is cleared to `false` on the last write.

---

## Failure Modes and Exit Codes

| Condition                         | Exit code | `outcome_class`              |
|-----------------------------------|----------:|------------------------------|
| Normal completion                 | 0         | `success`                    |
| `--task` / `--render-only` / `--trajectory-name` used with `--resume` | 2 | `usage_error` |
| Cap-raise flag used without `--resume-allow-step-bump` | 2 | `usage_error` |
| Trajectory has a terminal outcome | 15        | `resume_already_terminal`    |
| Trajectory missing `task` or `model_name` | 16  | `resume_manifest_missing`    |
| Trajectory messages empty or < 2  | 17        | `resume_invalid_prefix`      |

See `docs/exit-codes.md` for the full exit-code contract.

---

## What is NOT Supported

- Resuming `bench swebench` sweep instances (see issue #269).
- Cross-host resume (original `working_dir` must still exist).
- Cross-`env_kind` resume (local → docker, etc.).
- Cross-model-family resume (e.g. original used `claude-opus-4-7`, resume
  tries a different model family).
- Auto-resume on next launch; explicit `--resume` only.
- Resuming a trajectory that pre-dates the per-step WAL (no `partial: true`
  field recorded).
