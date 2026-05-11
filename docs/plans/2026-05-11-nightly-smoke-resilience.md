# Nightly Smoke Resilience Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make nightly smoke runs recover from provider-native tool calls, warn the model before wallclock cutoff, and discourage dependency-install detours in constrained local SWE-bench runs.

**Architecture:** Keep changes inside the existing model/agent contract. Preserve fenced-block parsing as the primary protocol, but normalize OpenAI-style `tool_calls` from raw model responses into the same `Action` path. Add a model-visible deadline warning before the hard timeout by reusing the existing agent history/trajectory machinery.

**Tech Stack:** Rust 2024, Tokio, serde_json, existing `DefaultAgent`, `mini::run` timeout wrapper, and TOML prompt defaults.

---

### Task 1: Native Tool Calls

**Files:**
- Modify: `src/agent/parse.rs`
- Modify: `src/agent/default.rs`
- Test: `src/agent/parse.rs`
- Test: `src/agent/default.rs`

**Step 1:** Add failing tests showing OpenAI-style raw `tool_calls` with `function.name = "bash"` and JSON `arguments.command` parse into `Action::Bash`.

**Step 2:** Add a failing agent-loop test where a model response has empty content but raw `tool_calls`, and assert the command executes without a format-error turn.

**Step 3:** Implement a helper that extracts one registered tool call from `ModelResponse.raw` and falls back to content parsing.

**Step 4:** Run `cargo test agent::parse agent::default::tests::<new test>`.

### Task 2: Deadline Submit Nudge

**Files:**
- Modify: `src/agent/default.rs`
- Modify: `src/run/mini.rs`
- Test: `src/agent/default.rs`

**Step 1:** Add a failing test that calls a deadline-warning helper and asserts it appends a user message telling the model to submit if it has a useful patch.

**Step 2:** Add a best-effort warning before `run_agent_with_timeout` reaches the hard timeout.

**Step 3:** Preserve existing wallclock timeout behavior when the warning does not lead to submission.

**Step 4:** Run focused agent/mini timeout tests.

### Task 3: Dependency Install Guidance

**Files:**
- Modify: `src/config/defaults/default.toml`
- Test: `tests/config_reference.rs` or `tests/agent_loop.rs`

**Step 1:** Add a failing prompt regression asserting the default system prompt tells the model not to install dependencies unless explicitly needed.

**Step 2:** Update the default system prompt with concise guidance: inspect first, avoid dependency installation tar pits, and submit when a patch is ready.

**Step 3:** Run focused prompt/config tests.

### Final Verification

**Commands:**
- `cargo fmt --all`
- `cargo test agent::parse`
- `cargo test --test agent_loop`
- `cargo test --test config_reference`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `git diff --check`
