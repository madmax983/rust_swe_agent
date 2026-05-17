# spec-behavior: `bench behavior`

## Overview

`bench behavior` is a read-only post-sweep diagnostic that classifies every
assistant turn into a semantic action class (read, write, test, build, search,
nav, git, other, noop) and surfaces per-class counts, shares, and cost
attribution broken down by outcome bucket. It writes `behavior.json` beside the
sweep artifacts and prints a ranked text table.

No agent loop, no model call, no sweep re-run.

---

## Action Taxonomy

| Class   | Command heads                                                                                   |
|---------|-------------------------------------------------------------------------------------------------|
| `test`  | `pytest`, `cargo test`, `npm test`, `yarn test`, `go test`, `gradle test`, `mvn test`, `tox`, `nose`, `jest`, `mocha`, `vitest`, `phpunit`, `rspec` |
| `write` | `sed`, `awk`, `tee`, `patch`, `dd`; `echo` when the full action string contains `>` (output redirect) |
| `build` | `cargo build`, `cargo check`, `cargo clippy`, `cargo fmt`, `make`, `ninja`, `cmake`, `npm build`, `yarn build`, `go build`, `gradle build`, `mvn package`, `tsc` |
| `search`| `grep`, `rg`, `find`, `fd`, `ack`, `ag`, `locate`                                             |
| `read`  | `cat`, `head`, `tail`, `less`, `more`, `bat`, `file`, `wc`, `od`, `xxd`, `column`, `ls`       |
| `nav`   | `cd`, `pwd`, `which`, `whereis`, `pushd`, `popd`, `dirs`                                       |
| `git`   | any command whose head is `git`                                                                  |
| `noop`  | assistant turn with no parseable bash action (reasoning-only, no actions, all `__SUBMIT__`)    |
| `other` | command heads not in any class above                                                            |

`taxonomy_version` is bumped whenever the mapping table changes so historical
`behavior.json` artifacts remain interpretable.

### Head extraction rules

The same prefix-stripping rules used by `bench command-stats` apply here:

- Leading `sudo` is stripped.
- Leading `time` is stripped.
- Leading `env VAR=val …` is stripped.
- Leading bare `VAR=val` environment assignments are stripped.

For dispatch commands (`cargo`, `npm`, `yarn`, `go`, `gradle`, `mvn`), the
first argument (subcommand) determines the class.

### Per-turn classification

Each assistant turn is classified into exactly one **primary action class**:

1. All bash actions in the turn are split on pipeline operators (`|`, not `||`).
2. Each pipeline segment is classified individually.
3. The turn's class is the **highest-priority class** found across all segments.

Priority order (highest → lowest):

```
test > write > build > search > read > nav > git > other > noop
```

A turn with no actions, or whose only action is `__SUBMIT__`, is classified as
`noop`.

---

## `behavior.json` Schema

```json
{
  "sweep": "<path>",
  "generated_at": "<ISO-8601>",
  "taxonomy_version": 1,
  "totals": {
    "<class>": {
      "turn_count": <integer>,
      "share": <0.0–1.0>
    }
  },
  "by_outcome": {
    "resolved|unresolved|errored|all": {
      "<class>": {
        "turn_count": <integer>,
        "share": <0.0–1.0>,
        "mean_turns_per_instance": <float>,
        "attributed_cost_usd": <float>
      }
    }
  },
  "comparisons": {
    "resolved_vs_unresolved": [
      {
        "action_class": "<class>",
        "resolved_share": <float>,
        "unresolved_share": <float>,
        "share_delta": <float>   // resolved_share − unresolved_share, sorted descending
      }
    ]
  },
  "per_instance": [             // present only when --per-instance is set
    {
      "instance_id": "<id>",
      "class_counts": { "<class>": <integer> }
    }
  ],
  "unclassified_heads": {
    "<head>": <invocation_count>
  }
}
```

- `totals` covers the whole sweep (all outcome buckets combined).
- `by_outcome.all` is the union of all four buckets.
- `comparisons.resolved_vs_unresolved[*].share_delta` is `resolved_share −
  unresolved_share`; positive = class is more prevalent in resolved instances.
- `unclassified_heads` lists command heads that fell into `other`, so operators
  can extend the taxonomy.

---

## CLI Reference

```
bench behavior --sweep <DIR> [OPTIONS]
```

| Flag             | Description                                                                           | Default |
|------------------|---------------------------------------------------------------------------------------|---------|
| `--sweep <DIR>`  | Completed sweep directory (must contain `results.json`).                              | —       |
| `--format`       | `text` (ranked table) or `json` (full artifact to stdout).                            | `text`  |
| `--bucket`       | Restrict text output to one bucket: `resolved`, `unresolved`, `errored`, `all`.       | all     |
| `--min-share`    | Hide classes whose `all`-bucket share is below this threshold (0.0–1.0) in text mode. | `0.0`   |
| `--filter`       | Instance filter in `key=value` form (same syntax as `bench inspect --filter`).        | —       |
| `--per-instance` | Include per-instance class counts in the JSON output and written artifact.            | off     |

### Exit codes

Follows the project's stable contract (#98):

- `0` — success regardless of outcome mix.
- Non-zero — missing/unreadable sweep artifacts or invalid flags.

---

## `bench compare` integration

When both sweep directories contain a `behavior.json`, `bench compare --format
text` appends a one-paragraph **Action-Shape Diff** section that reports the
per-class share deltas (candidate − baseline) with a headline naming the largest
shift. The section is omitted silently when either artifact is absent.

---

## Worked example

Sweep A (edit-heavy prompt):

```
--- Outcome: resolved ---
action_class   turn_count   share    mean_turns/instance   attributed_cost_usd
write          9            0.6000   3.00                  0.090000
test           3            0.2000   1.00                  0.030000
read           3            0.2000   1.00                  0.010000
```

Sweep B (read-heavy prompt):

```
--- Outcome: unresolved ---
action_class   turn_count   share    mean_turns/instance   attributed_cost_usd
read           15           0.8333   5.00                  0.050000
search         3            0.1667   1.00                  0.010000
```

`bench compare` action-shape diff:

```
--- Action-Shape Diff ---
Agent shifted to more write-heavy: +60pp write, +20pp test, -63pp read
```

---

## Non-goals

- MCP tool-call classification (bash only; MCP tool calls are a separate channel).
- Time- or token-weighted shares beyond `attributed_cost_usd`.
- Significance testing on action-class deltas (see #176).
- Per-step sequence patterns or n-gram action transition matrices.
- Adaptive or learned taxonomies.
- Real-time action-shape during a running sweep (post-hoc only).
