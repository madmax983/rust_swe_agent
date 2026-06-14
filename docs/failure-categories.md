# Failure Category Reference and Triage Runbook

Every trajectory produced by the harness carries a `failure_category` field in
its `info` block when `outcome` is `error`.  This string drives
[`bench triage`](spec-triage.md), [`bench compare`](spec-evaluation.md), the
[systemic-halt circuit breaker](spec-systemic-halt.md), nightly-smoke auto-issues,
and the failure mix in [`bench tail`](spec-tail.md).

This page is the single canonical reference for every `failure_category` value:
what it means, why you see it, what to do next, and how it integrates with the
commands that consume it.

See also: [`docs/exit-codes.md`](exit-codes.md) for coarse sweep-level outcome
classes, [`docs/artifact-contract.md`](artifact-contract.md) for the full
trajectory schema.

---

## How to Read This Reference

Each entry below covers one operator-visible `failure_category` string and
answers these questions:

| Field | Meaning |
|---|---|
| **JSON string** | The exact value you see in `failure_category` and in `bench inspect` / `bench triage` output |
| **Definition** | What the category means in one paragraph |
| **You will see this when…** | Typical triggering conditions |
| **Recommended action** | What to do next |
| **Systemic-halt actionable** | Whether this category counts toward the [circuit-breaker threshold](spec-systemic-halt.md) |
| **Partial patch preserved** | Whether the harness writes a patch file when this category fires |
| **Typical sweep outcome class** | The most common process exit code from [`docs/exit-codes.md`](exit-codes.md) when this category dominates a sweep |

---

## Category Entries

### `env_setup`

| | |
|---|---|
| **JSON string** | `env_setup` |
| **Systemic-halt actionable** | **Yes** |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `systemic_halt` (11) when dominant; `success` (0) otherwise |

**Definition.** The environment setup phase failed before the agent loop
started. The harness attempted to prepare the task workspace — pulling a Docker
image, cloning the repository, or running environment installation steps — and
the preparation did not complete successfully.  No agent turns were executed.

**You will see this when…**
- The Docker daemon is not running or the socket is not accessible.
- The Docker image tag does not exist or the pull times out.
- The task dataset specifies a repository that cannot be cloned (private, moved, or removed).
- The container fails to start or the environment setup script exits non-zero.
- Network access to the container registry is blocked.

**Recommended action.**
1. Run `bench doctor` to verify Docker availability and connectivity.
2. Check that the container image exists and is pullable: `docker pull <image>`.
3. Review `bench inspect <sweep_dir> --instance <id>` for the specific error message.
4. If `env_setup` is dominant across instances, the circuit breaker may trip with exit 11 (`systemic_halt`).  Halt reports are in `halt-report.json`.
5. Use `bench retry` to rerun only affected instances after fixing the infrastructure issue.

---

### `model_api`

| | |
|---|---|
| **JSON string** | `model_api` |
| **Systemic-halt actionable** | **Yes** |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `systemic_halt` (11) when dominant; `success` (0) otherwise |

**Definition.** The model API returned repeated, non-transient errors during the
agent loop.  The harness retried according to its retry policy and ultimately
gave up because every attempt failed.

**You will see this when…**
- The `ANTHROPIC_API_KEY` (or equivalent provider key) is invalid, expired, or missing.
- The account's token quota or rate limit is exhausted at the project level.
- The configured model name does not exist or has been deprecated.
- The model API endpoint is unreachable (network outage, firewall rule).
- A provider-side outage causes all calls to fail.

**Recommended action.**
1. Verify the API key: `echo $ANTHROPIC_API_KEY | wc -c` and confirm it is set in the environment.
2. Check the provider dashboard for quota or outage notices.
3. Confirm the model name in your config matches a currently-available model: `bench doctor --skip-model-probe`.
4. Run `bench inspect --sweep <sweep_dir> --instance <id>` to see the raw API error text.
5. After fixing the credential issue, use `bench retry` or `bench swebench --resume` to continue.

---

### `model_parse`

| | |
|---|---|
| **JSON string** | `model_parse` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `success` (0) |

**Definition.** The harness repeatedly received a response from the model API
that it could not parse into a valid agent action.  The model produced output but
it was structurally incompatible with the expected tool-call or action format.

**You will see this when…**
- You switch to a model that uses a different response format from the one the harness expects.
- The model produces free-form prose instead of structured tool calls (prompt mismatch).
- The model API returns valid JSON but with an unexpected schema (provider-side format change).
- The model is severely truncating its responses due to a context-length edge case.

**Recommended action.**
1. Confirm you are using a model supported by this harness version.
2. Check [`bench inspect`](spec-inspect.md) on a failing instance to see the raw model output.
3. If you recently changed the model name, verify format compatibility.
4. File a bug report if a previously-working model starts producing this category.

---

### `step_limit`

| | |
|---|---|
| **JSON string** | `step_limit` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `success` (0) |

**Definition.** The agent exhausted its configured maximum step budget
(`agent.step_limit`) without submitting a result.  The agent loop ran to
completion — no infrastructure failure occurred — but the agent did not reach a
terminal submission within the allotted turns.

**You will see this when…**
- Tasks are more complex than the configured step limit allows.
- The step limit is deliberately set low for budget-testing purposes.
- The agent is spending many turns on unproductive exploration.
- You see `step_limit` paired with high `cost_usd` per instance in `bench tail`.

**Recommended action.**
1. Run `bench triage` to identify which instance clusters produce `step_limit` most often.
2. Inspect representative instances with `bench inspect` to understand where the agent stalls.
3. Increase `agent.step_limit` in your config if the tasks genuinely require more turns.
4. Consider prompt improvements to guide the agent to submit earlier.

---

### `cost_limit`

| | |
|---|---|
| **JSON string** | `cost_limit` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `success` (0); `budget_halt` (5) when the sweep-level cap is also hit |

**Definition.** The per-task cost ceiling was reached before the task could
complete.  The harness stopped dispatching new turns and recorded the instance
as failed.

**You will see this when…**
- The `agent.cost_limit_usd` config field is set too low for the model and task complexity.
- You are intentionally running with a tight per-task cap for cost exploration.
- Token usage per turn is unusually high (e.g., long file reads, verbose tool output).

**Recommended action.**
1. Review per-task cost in `bench tail` or `bench inspect --sweep <dir>` to understand typical spend.
2. Increase `agent.cost_limit_usd` in your config file, or switch to a cheaper model.
3. Use `bench forecast` to project total cost before raising limits.

---

### `budget_exhausted`

| | |
|---|---|
| **JSON string** | `budget_exhausted` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | No — patch capture only runs on `Submitted` exit; the in-flight worktree state is not written to disk |
| **Typical sweep outcome class** | `budget_halt` (5) when the sweep-level cost cap terminates the run; `success` (0) for per-task cap |

**Definition.** The per-task USD ceiling (`agent.per_task_budget_usd`) was
reached mid-loop.  The harness terminated the agent before submission.  Patch
capture only runs on a `Submitted` exit, so no `.patch` artifact is written even
if the agent had partial edits in the worktree.

**You will see this when…**
- The per-task budget was consumed partway through an agent turn.
- A single very expensive model call pushed the instance over its ceiling.
- The sweep-level cost cap (`--sweep-cost-limit-usd`) is hit, causing remaining instances to be skipped.

**Recommended action.**
1. Check `bench tail` cost burn per instance to calibrate the ceiling.
2. Increase `agent.per_task_budget_usd` in your config, or use `bench forecast` to size the budget before re-running.
3. Use `bench retry` to attempt only the budget-exhausted instances with a higher ceiling.

---

### `wallclock_timeout`

| | |
|---|---|
| **JSON string** | `wallclock_timeout` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `success` (0) |

**Definition.** The per-task real-time execution limit (`task_timeout_secs`) was
exceeded.  The harness killed the agent turn after the configured wall-clock
duration regardless of progress.

**You will see this when…**
- Tool calls block for an unexpectedly long time (slow network, large file I/O).
- The model API has unusually high latency (provider-side degradation).
- The Docker container starts slowly (image layer hydration on first run).
- `task_timeout_secs` is set too low for the model and task combination.

**Recommended action.**
1. Check provider status for API latency anomalies.
2. Increase `task_timeout_secs` in your config.
3. Use `bench inspect` to see at which step the timeout occurred.
4. If timeouts are rare, they may be noise; if they dominate, investigate infrastructure latency.

---

### `agent_internal`

| | |
|---|---|
| **JSON string** | `agent_internal` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `internal_error` (1) in severe / sweep-halting cases; `success` (0) otherwise |

**Definition.** An internal logic error occurred within the agent harness itself.
This category indicates a bug in the harness code, not in the model, the
environment, or the task.

**You will see this when…**
- A harness code path hits an unexpected state or invariant violation.
- A JSON deserialization error occurs on internal data (not model output).
- A file I/O error prevents trajectory writing mid-run.

**Recommended action.**
1. Use `bench inspect` to retrieve the error message and stack details.
2. Report the issue with the full trajectory and the harness version.
3. Use `bench bundle` to capture the sweep artifacts for a bug report.
4. Consider pinning the harness to a known-good version while the fix is applied.

---

### `patch_apply_invalid`

| | |
|---|---|
| **JSON string** | `patch_apply_invalid` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | **Yes** — the invalid patch file is written for inspection |
| **Typical sweep outcome class** | `success` (0) |

**Definition.** The agent produced a patch that failed `git apply --check`
validation at capture time.  The patch file is written to disk so you can
inspect it, but the harness records the instance as failed because the patch
cannot be applied cleanly.

**You will see this when…**
- The agent generates a diff against a different base commit than the one in the repo.
- The agent produces syntactically malformed unified diff output.
- The patch includes hunks that conflict with files already modified by an earlier step.
- You use `agent apply` on an incompatible tree (use `apply_check_failed`, exit 29, for that scenario).

**Recommended action.**
1. Inspect the invalid patch: `bench inspect <sweep_dir> --instance <id> --show-patch`.
2. Check the base commit the agent was working against.
3. Review the agent's final diff-generation step in the trajectory.
4. If systematic, investigate the model's patch-formatting capability.

---

### `patch_empty`

| | |
|---|---|
| **JSON string** | `patch_empty` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | **Yes** — the empty patch file is written to disk before validation fires |
| **Typical sweep outcome class** | `success` (0) |

**Definition.** The agent submitted but the captured diff was empty (zero bytes
or whitespace only).  The agent called the submission tool but no file changes
were recorded.

**You will see this when…**
- The agent submitted without making any edits to the codebase.
- All edits the agent made were reverted before submission.
- The agent's edits were to a file outside the repository root (not tracked by git).
- A shell command the agent ran cleaned up its own changes.

**Recommended action.**
1. Review the trajectory with `bench inspect` to find where the agent submitted and whether it made edits.
2. Check if the task explicitly requires no code change (rare but possible).
3. If systematic, investigate whether the agent is skipping the edit step.

---

### `secret_leak_detected`

| | |
|---|---|
| **JSON string** | `secret_leak_detected` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | **Yes** — the redacted patch is written to disk before the run is downgraded |
| **Typical sweep outcome class** | `verification_failure` (7) for `mini`; `success` (0) for `bench swebench` |

**Definition.** A configured secret literal was found in a submission artifact
(patch, trajectory, or output file).  The harness blocked the submission and
recorded the instance as failed to prevent credential exposure.

**You will see this when…**
- The agent incorporates an API key or password into generated code or test files.
- A configured `secret_literals` entry matches a substring of the patch.
- The model echoes a secret from its context window into a tool call argument.

**Recommended action.**
1. Run `agent redact-check` to verify your secret literals configuration is correct.
2. Run `agent redact-audit` on the sweep directory to locate any exposed secrets.
3. Review the task and model prompt to understand why the secret appeared in the output.
4. See [`docs/spec-secret-redaction.md`](spec-secret-redaction.md) for the full redaction policy.

---

### `agent_stagnation`

| | |
|---|---|
| **JSON string** | `agent_stagnation` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `agent_stagnation` (12) for `mini`; `success` (0) for `bench swebench` |

**Definition.** The stagnation detector fired: the agent repeated the same
action at least K times within a trailing window of W steps.  The harness halted
the run rather than let the agent loop indefinitely.

When `failure_category` is `agent_stagnation`, `info.other["stagnation"]`
contains a diagnostic object:

```json
{
  "action_hash":  "<32-hex-char SHA-256 prefix of canonical action>",
  "count":        4,
  "window":       8,
  "step_indices": [2, 4, 6, 8]
}
```

**You will see this when…**
- The agent retries the same failing command repeatedly without adjusting its strategy.
- A tool always returns the same error, and the agent does not adapt.
- The model lacks enough context variety to break out of a loop.

**Recommended action.**
1. Use `bench inspect` on the stagnating instance to see which action repeated.
2. Review the `stagnation` diagnostic object to identify the repeated step indices.
3. Use `bench stagnation-report` for cross-sweep aggregation of stagnation patterns.
4. Adjust the prompt or the stagnation window (`agent.stagnation_window`, `agent.stagnation_count`) if needed.
5. See [`docs/spec-stagnation.md`](spec-stagnation.md) for the full detection rule and config reference.

---

### `history_compaction_failed`

| | |
|---|---|
| **JSON string** | `history_compaction_failed` |
| **Systemic-halt actionable** | **Yes** |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `systemic_halt` (11) when dominant; `success` (0) otherwise |

**Definition.** The trajectory history could not be compacted to fit within
`history_max_input_tokens` even after eliding all older observations.  The
harness terminates the run rather than sending an oversized prompt or triggering
a provider context-length error.

**You will see this when…**
- The task requires a very long context (many files, large diffs, long tool output).
- `history_max_input_tokens` is configured too low for the model's context window.
- A single tool call returns an extremely large response that cannot be elided.
- You are using a model with a smaller context window than the task demands.

**Recommended action.**
1. Increase `history_max_input_tokens` in your config to match the model's context window.
2. Check `bench inspect` to identify which observation caused the overflow.
3. Consider switching to a model with a larger context window.
4. Tune tool verbosity to reduce output size per step.
5. If this category dominates a sweep, the circuit breaker may trip (exit 11); see `halt-report.json`.

---

### `read_only_violation`

| | |
|---|---|
| **JSON string** | `read_only_violation` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | No |
| **Typical sweep outcome class** | `success` (0) |

**Definition.** The agent attempted a write operation (file edit, shell command
with side-effects) while running in `--read-only` mode.  The harness blocked the
tool invocation and terminated the run.

**You will see this when…**
- You run a sweep with `--read-only` but the task requires writing files.
- An agent action unexpectedly triggers the read-only policy.
- You are using `agent env preview` and the policy is stricter than expected.

**Recommended action.**
1. Remove `--read-only` if the task requires write access.
2. Use `bench inspect` to identify which tool call triggered the violation.
3. See [`docs/spec-read-only.md`](spec-read-only.md) for the full read-only policy reference.

---

### `unknown`

| | |
|---|---|
| **JSON string** | `unknown` |
| **Systemic-halt actionable** | No |
| **Partial patch preserved** | Unknown |
| **Typical sweep outcome class** | `success` (0) |

**Definition.** An unknown or unclassified failure occurred, or a value was
produced by a newer harness version that this reader does not recognise.
`unknown` is the catch-all and deserialization fallback (see
[Compatibility Policy](#compatibility-policy) below).

**You will see this when…**
- You are running an older harness version reading trajectories from a newer one that
  introduced a category this version does not know about.
- A failure occurred that the harness could not classify into any named category.

**Recommended action.**
1. Upgrade the harness to the latest version to get the full category vocabulary.
2. Use `bench inspect` to read the raw trajectory and look for error details in other fields.

---

## Compatibility Policy

The `failure_category` string is a **stable, versioned contract** governed by
the same rules as `docs/exit-codes.md`:

- **Renaming a published variant is a breaking change.**  Any consumer that
  stores or routes on `failure_category` strings must be updated.  Breaking
  renames require a major version bump.
- **Adding a new variant is a minor change.**  Consumers that do not recognise
  the new string must treat it as `unknown` (forward-compatible parsing).
- **`unknown` is reserved.**  This string will never be assigned to a
  newly-introduced named category.  Readers may always treat `unknown` as the
  catch-all / forward-compat sentinel without ambiguity.
- **Removing a variant is a breaking change** if any tooling routes on it;
  deprecate first, remove later.

A CI test (`tests/failure_category_docs.rs`) enforces that every
`FailureCategory` enum variant has a corresponding entry in this file.  The test
keys off the serde-serialized string, not the Rust identifier, so renaming a
variant without updating this doc causes a compile-time failure.

---

## Worked Triage Example

Given the following `results.json` failure mix from a completed sweep:

```json
{
  "total": 50,
  "submitted": 18,
  "skipped": 0,
  "errored": 32,
  "failures_by_category": {
    "step_limit":    14,
    "model_api":      9,
    "patch_empty":    5,
    "env_setup":      3,
    "agent_internal": 1
  }
}
```

**Step 1 — Identify the dominant category.**
`step_limit` (14) is the most frequent, but `model_api` (9) is operator-actionable
and likely indicates a broken API key or quota issue.  Investigate `model_api`
first because it is in the systemic-halt whitelist and prevents any meaningful work.

**Step 2 — Diagnose the actionable category.**

```bash
bench triage --sweep runs/sweep --bucket model_api
bench inspect --sweep runs/sweep --instance <failing_id>
```

Check the provider dashboard for quota alerts.  If the API key is invalid, fix
it and re-run.  `model_api` failures cost tokens for nothing; they are the
highest-priority fix.

**Step 3 — Address `step_limit` after the API issue is cleared.**

```bash
bench triage --sweep runs/sweep --bucket step_limit --top 10
```

Look for clusters of similar task types.  Increase `agent.step_limit` or
improve prompting for the clustered task types.

**Step 4 — Investigate `patch_empty`.**

```bash
bench inspect --sweep runs/sweep --instance <patch_empty_id> --show-patch
```

Review the trajectory to understand why the agent submitted with no changes.
These may be tasks the model interprets as "already done."

**Step 5 — Bundle and archive.**

```bash
bench bundle runs/sweep --output sweep-archive.tar.gz
```

Use `bench tail` to monitor any re-run in real time:

```bash
bench tail runs/sweep-retry
```

---

## Quick Reference Table

| JSON string | Systemic-halt | Patch preserved | Typical sweep outcome |
|---|---|---|---|
| `env_setup` | **Yes** | No | `systemic_halt` (11) / `success` (0) |
| `model_api` | **Yes** | No | `systemic_halt` (11) / `success` (0) |
| `history_compaction_failed` | **Yes** | No | `systemic_halt` (11) / `success` (0) |
| `model_parse` | No | No | `success` (0) |
| `step_limit` | No | No | `success` (0) |
| `cost_limit` | No | No | `success` (0) / `budget_halt` (5) |
| `budget_exhausted` | No | No | `budget_halt` (5) / `success` (0) |
| `wallclock_timeout` | No | No | `success` (0) |
| `agent_internal` | No | No | `internal_error` (1) / `success` (0) |
| `patch_apply_invalid` | No | **Yes** (invalid patch) | `success` (0) |
| `patch_empty` | No | **Yes** (empty patch file) | `success` (0) |
| `secret_leak_detected` | No | **Yes** (redacted patch) | `verification_failure` (7) / `success` (0) |
| `agent_stagnation` | No | No | `agent_stagnation` (12) / `success` (0) |
| `read_only_violation` | No | No | `success` (0) |
| `unknown` | No | Unknown | `success` (0) |
