# Spec: `mini --continue` — Follow-up Instruction to a Completed Run

## Problem

`mini --resume` explicitly refuses terminal trajectories (`resume_already_terminal`,
exit 15); it only recovers crashed/interrupted runs. The only path forward after
a "close-but-wrong" submission was to re-run the whole task from scratch, re-paying
for every prefix token and discarding the model's accumulated context.

`mini --continue` solves this: given a terminal trajectory and a follow-up
instruction, it appends the instruction as a new user turn and runs the agent
starting from that point — reusing the full parent message history via provider
cache semantics.

## Surface

```
max mini --continue PATH --task "FOLLOW-UP INSTRUCTION" [OPTIONS]
```

| Flag | Required? | Description |
|------|-----------|-------------|
| `--continue PATH` | required | Path to terminal `.traj.json` file |
| `--task TEXT` | required | Follow-up instruction (appended as new user turn) |
| `--task-file PATH` | alternative to `--task` | Read follow-up from file or stdin |
| `--continue-allow-step-bump` | optional | Allow raising `--step-limit`, `--task-timeout-secs`, `--per-task-budget-usd` |

### Mutual Exclusions

| Flag pair | Outcome |
|-----------|---------|
| `--continue` + `--resume` | exit 2 (`usage_error`) — clap conflict |
| `--continue` + `--render-only` | exit 2 (`usage_error`) — clap conflict |
| `--continue` + `--interactive` | exit 2 (`usage_error`) — clap conflict |
| `--continue` + `--trajectory-name` | exit 2 (`usage_error`) — clap conflict |
| `--continue` without `--task`/`--task-file` | exit 2 (`usage_error`) |

## Preconditions

A trajectory is valid for `--continue` if and only if it is **terminal**:
- `partial: false`, OR
- `outcome` is present (any value), OR
- `exit_reason` is present (any value)

A trajectory is invalid for `--continue` (exit 27, `continue_non_terminal`) if:
- `partial: true` AND `outcome` is absent AND `exit_reason` is absent

Additionally, the trajectory must contain `task` and `model_name` fields
(exit 16, `resume_manifest_missing` if missing).

These are the **inverse** of `--resume` preconditions.

## Budget and Config Inheritance

| Setting | Behavior |
|---------|----------|
| Model name | Inherited from parent `info.model_name` |
| `step_limit` | Inherited from parent config; may be raised with `--continue-allow-step-bump` |
| `per_task_budget_usd` | Inherited from parent config; may be raised with `--continue-allow-step-bump` |
| `task_timeout_secs` | Inherited from parent config; may be raised with `--continue-allow-step-bump` |
| Step counter | Starts fresh at 0 for the continuation |
| Cost counter | Starts fresh at $0 for the continuation |

## Lineage Schema

The child trajectory records its origin in `info.parent_trajectory`:

```json
{
  "parent_path": "/abs/path/to/parent.traj.json",
  "parent_trajectory_id": "parent",
  "parent_outcome": "submitted",
  "parent_steps": 7
}
```

| Field | Type | Description |
|-------|------|-------------|
| `parent_path` | string | Canonicalized path of the parent trajectory file |
| `parent_trajectory_id` | string | File stem of parent (without `.traj.json`) |
| `parent_outcome` | string? | Terminal outcome of parent (`submitted`, `error`, …) |
| `parent_steps` | integer? | Steps completed in the parent run |

## Output File Naming

The child trajectory is written to the **same directory** as the parent, with
the name: `{parent_stem}-continue-{follow_up_slug}.traj.json`

Where `follow_up_slug` is a max-64-char alphanumeric slug derived from the
follow-up instruction.

Example:
```
runs/task.traj.json          ← parent (terminal, never mutated)
runs/task-continue-also-fix-edge-case.traj.json   ← child
```

Chaining works naturally: continue the child to produce a grandchild, etc.

## Message History

The child trajectory's message list contains:
1. All parent messages (system + task + all agent turns)
2. The new follow-up user message
3. New assistant responses from the continuation

The model is queried **only** for the new turn(s). The parent prefix is sent
to the provider but covered by provider cache semantics, so the marginal token
cost is only the new turns.

## Working Tree Behavior

**Local environment**: The agent runs in the same working directory as the
original run, so the parent's changes are already applied. The follow-up
instruction operates on the patched state.

**Docker environment**: Each continuation starts a fresh container. Operators
must ensure the working directory has the parent's changes applied (e.g., by
mounting a volume that already has the patch applied, or by running
`git apply < parent.patch` in the container setup).

## Exit Codes

| Code | Class | Condition |
|------|-------|-----------|
| 0 | `success` | Continuation completed successfully |
| 2 | `usage_error` | `--continue` without `--task`, or with conflicting flags |
| 15 | `resume_already_terminal` | *(never emitted by `--continue`)* |
| 16 | `resume_manifest_missing` | Parent trajectory missing `task`/`model_name` |
| 27 | `continue_non_terminal` | Parent trajectory is non-terminal (use `--resume` instead) |

See `docs/exit-codes.md` for the full contract.

## Deterministic Test

The following test (in `src/run/mini.rs`) proves the 2-turn continuation
records the parent's N steps plus the new turn and a valid `parent_trajectory`
link at $0 using the scripted-response (deterministic) model backend:

```
cargo test mini_continue_2turn_deterministic_hello_world
```

The test asserts:
1. Parent reaches `submitted` outcome
2. Child reaches `submitted` outcome  
3. Child has a valid `parent_trajectory` lineage link
4. Child messages include all parent messages plus new ones
5. Child has no `resume_history` entries (lineage is via `parent_trajectory`)
6. The model was not re-queried for the parent prefix (one scripted response
   is sufficient — exhaustion would signal re-querying)

## What Is Out of Scope

- Full interactive multi-turn REPL (one follow-up per invocation; chain by
  continuing the continuation)
- Branching/counterfactual exploration (`bench fork` handles that)
- Sweep-level (multi-instance) continuation
- Automatic "keep going until resolved" self-refinement (operator supplies each
  instruction)
