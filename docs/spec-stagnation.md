# In-Loop Agent Stagnation Detection

## Overview

The harness monitors each agent run for _stagnation_: the condition where the
agent keeps issuing the same bash action repeatedly without making progress.
When stagnation is detected the run is halted immediately rather than burning
budget up to the step limit.

## Rule

A run is considered stagnant when:

> Within the trailing **W** steps, the same canonical bash action has appeared
> at least **K** times.

Default values: **K = 4**, **W = 8**.

## Canonical action fingerprinting

Before comparing actions, each raw bash string is canonicalized:

1. Trim leading and trailing whitespace.
2. Collapse runs of internal whitespace to a single space.
3. Strip a trailing `;`, then trim trailing whitespace again.

The canonical string is then hashed with SHA-256 (first 16 bytes, encoded as
32 lower-case hex characters).  The hash is used for O(1) comparison inside
the sliding window; the canonical text is stored for diagnostics.

Example:

| Raw input          | Canonical form | Hash (first 8 chars) |
|--------------------|----------------|----------------------|
| `  ls  ;`          | `ls`           | `a37c5b26…`          |
| `ls`               | `ls`           | `a37c5b26…`          |
| `ls   -la`         | `ls -la`       | (different)          |

## Detection algorithm

A ring-buffer of the last W `(step_index, action_hash)` pairs is maintained.
After every bash tool use (excluding tool-blocked calls) the new entry is
appended and oldest entries are evicted as needed.  The count of occurrences
of the current action hash within the buffer is then compared against K.  If
the count is ≥ K the detector trips.

## Outcome

When the detector trips:

- `info.exit_reason` = `"agent_stagnation"`
- `info.failure_category` = `"agent_stagnation"`
- `info.other["stagnation"]` — JSON object with diagnostic fields:

```json
{
  "action_hash":   "a37c5b26…",
  "count":         4,
  "window":        8,
  "step_indices":  [2, 4, 6, 8]
}
```

- Process exits with code **12** (`ExitCode::AgentStagnation`).

## Configuration

| Config field                         | CLI flag                        | Default | Description                                        |
|--------------------------------------|---------------------------------|---------|----------------------------------------------------|
| `agent.detect_stagnation`            | `--detect-stagnation`           | `true`  | Enable / disable the detector entirely.            |
| `agent.stagnation_repeat_threshold`  | `--stagnation-repeat-threshold` | `4`     | K — repetition count that trips the detector.      |
| `agent.stagnation_window`            | `--stagnation-window`           | `8`     | W — sliding window width in steps.                 |

The detector requires W ≥ K; the agent builder rejects invalid combinations
at startup.

## Replay mode

Stagnation detection is **disabled** automatically in replay mode (`bench
replay`).  Replays are scripted and deterministic; the agent should run to the
same terminal condition recorded in the original trajectory without being
interrupted by the detector.

## Exit code contract

| Code | `outcome_class`    | Condition                        |
|------|--------------------|----------------------------------|
| 12   | `agent_stagnation` | Stagnation detector tripped.     |

See `docs/exit-codes.md` for the full exit-code table.

## Trajectory format version

This feature was introduced with trajectory format **mini-swe-agent-1.2**.
Trajectories produced before 1.2 will not contain `info.other["stagnation"]`
and their `failure_category` will not be `"agent_stagnation"`.

## Worked example

Step limit = 10, K = 4, W = 8.  The agent issues `ls` at every step:

| Step | Action | Window contents (hashes) | `ls` count | Trip? |
|------|--------|--------------------------|------------|-------|
| 0    | `ls`   | [ls]                     | 1          | No    |
| 1    | `ls`   | [ls, ls]                 | 2          | No    |
| 2    | `ls`   | [ls, ls, ls]             | 3          | No    |
| 3    | `ls`   | [ls, ls, ls, ls]         | 4          | **Yes** — halted |

The run is terminated at step 3 (4th occurrence), well before the step limit.
