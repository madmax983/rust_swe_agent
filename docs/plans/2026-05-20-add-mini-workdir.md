# issue #341: Add mini `--workdir` to root the agent at a specified directory

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the `--workdir <dir>` option for the `mini` subcommand. The flag forces all local agent commands and `--verify` checks to start in the absolute, canonicalized form of `<dir>`. If `<dir>` does not exist or is not a directory, validate early and exit with code 2. If combined with `--env docker`, raise code 2. The trajectory's `info` block gains `local_workdir: Option<String>`, and the trajectory format version is bumped from `mini-swe-agent-1.2` to `mini-swe-agent-1.3`. Artifact schema version is bumped from `1.9` to `1.10`.

**Architecture:**
- Modify `args::MiniCmd` in `src/cli/args.rs` to add `workdir` as an `pub workdir: Option<PathBuf>`.
- Inside `src/cli/mod.rs` (in `mini_cmd`, `mini_resume_cmd`, and `mini_render_only_cmd`), validate that:
  - If `workdir` is specified, it exists and is a directory. If not, return `Error::Config(ConfigError::Usage(format!("--workdir {} does not exist or is not a directory", workdir_val)))`.
  - If `workdir` is specified with `--env docker`, return `Error::Config(ConfigError::Usage("--workdir cannot be used with docker environment".to_string()))`.
  - Resolve and canonicalize the workdir to an absolute path.
- Add `ConfigError::Usage(String)` to `ConfigError` in `src/error.rs` to allow custom clean error messages that print `error: <msg>` directly.
- Add `local_workdir` to `MiniArgs` struct in `src/run/mini.rs`.
- In `LocalEnvironment`, update standard command runs and preflight check/verification commands to use the configured workdir as their default directory.
- Update `RenderOnlyReport` to print `local_workdir` in text and JSON formats.
- Trajectory schema format version bumped to `mini-swe-agent-1.3`. Record `local_workdir` under `agent.trajectory.info.local_workdir`.
- Update `ArtifactSchemaVersion` from `1.9` to `1.10`.
- Restore the `local_workdir` from the trajectory `info.local_workdir` on resume if `--workdir` is omitted.
- Write new integration tests in `tests/workdir.rs`.

**Tech Stack:** Rust, Clap (derive), Standard I/O, Serde.

---

## User Review Required

> [!IMPORTANT]
> - **Mutual Exclusion with Docker:** `--workdir` is strictly prohibited with `--env docker`. It will exit with exit code `2` and explain why.
> - **Exit Code on Invalid Path:** If `--workdir` points to a nonexistent path or a non-directory, `mini` exits with code `2` prior to any model calls.
> - **Trajectory/Artifact Schema Version Bumps:** Trajectory format is bumped to `mini-swe-agent-1.3`, and `ArtifactSchemaVersion` minor version is bumped to `10` (i.e. `1.10`).

## Open Questions

*None currently identified. The issue specifications are highly precise and comprehensive.*

---

## Proposed Changes

### CLI Arguments & Errors

#### [MODIFY] [args.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/src/cli/args.rs)
- Add `pub workdir: Option<PathBuf>` to `MiniCmd` with Clap attribute `#[arg(long)]`.

#### [MODIFY] [error.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/src/error.rs)
- Add `#[error("{0}")] Usage(String)` variant to `ConfigError` enum.

### Command Dispatch & Path Validation

#### [MODIFY] [mod.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/src/cli/mod.rs)
- Modify `mini_cmd`, `mini_resume_cmd`, and `mini_render_only_cmd` to perform existence, type, and environment validation.
- Standardize canonicalization of the workdir path.
- In `mini_resume_cmd`, retrieve the workdir from the trajectory `info.local_workdir` if no CLI override is specified.

### Local Environment Execution CWD

#### [MODIFY] [local.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/src/env/local.rs)
- Equip `LocalEnvironment` with an optional `workdir: Option<PathBuf>`.
- In `run` and `verify`/preflight execution, default to `workdir` if `req.cwd` is `None`.

### Orchestration & Metadata Recording

#### [MODIFY] [mini.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/src/run/mini.rs)
- Add `pub local_workdir: Option<PathBuf>` to `MiniArgs`.
- Update `MiniArgs` builder to initialize `local_workdir`.
- Store `local_workdir` in `agent.trajectory.info.local_workdir`.

#### [MODIFY] [mod.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/src/trajectory/mod.rs)
- Bump `FORMAT_VERSION` to `mini-swe-agent-1.3`.
- Update `TrajectoryInfo` struct to include `pub local_workdir: Option<PathBuf>`.

#### [MODIFY] [artifact.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/src/artifact.rs)
- Update `ArtifactSchemaVersion::CURRENT` to `{ major: 1, minor: 10 }`.

#### [MODIFY] [render_only.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/src/run/render_only.rs)
- Add `pub local_workdir: Option<PathBuf>` to `RenderOnlyReport`.
- Print `local_workdir` in both text and JSON render outputs.

### Tests & Integration

#### [NEW] [workdir.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/tests/workdir.rs)
- Comprehensive test suite to cover:
  - Error when workdir path does not exist.
  - Error when workdir path exists but is not a directory.
  - Error when workdir is combined with `--env docker`.
  - Happy-path integration test with deterministic model executing `ls` (or printing working directory), asserting the observation has correct outcomes and trajectory has `local_workdir` recorded correctly.
  - Verification run via `--verify` targets workdir.
  - Render-only outputs contain the canonicalized workdir path.

#### [MODIFY] [artifact_schema.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/tests/artifact_schema.rs)
- Update test cases expecting `1, 9` schema version to expect `1, 10`.

#### [MODIFY] [bench_retry.rs](file:///C:/Users/markm/.gemini/antigravity/worktrees/rust_swe_agent/implement-issue-341-tdd/tests/bench_retry.rs)
- Update `schema_version_is_1_9` test to `schema_version_is_1_10`.

---

## Technical Execution Plan (TDD Steps)

### Task 1: Initialize Integration Tests (RED Phase)

**Files:**
- Create: `tests/workdir.rs`

**Step 1: Write the failing tests**
Create tests for:
- Nonexistent workdir errors with exit code 2 and descriptive stderr message.
- Is-file workdir errors with exit code 2 and descriptive stderr message.
- Docker + workdir environment error.
- Render-only prints the canonicalized workdir.

**Step 2: Run tests to verify they fail**
`cargo test --test workdir` should fail to compile because `MiniCmd` doesn't have the `workdir` field.

---

### Task 2: Implement CLI and Error Changes (GREEN Phase 1)

**Files:**
- Modify: `src/cli/args.rs`
- Modify: `src/error.rs`
- Modify: `src/cli/mod.rs` (minimal placeholders to compile)

**Step 1: Update CLI definitions**
- Add `workdir` Option to `MiniCmd`.
- Add `ConfigError::Usage` variant.

**Step 2: Implement basic validations**
- Add validation in `mini_cmd` and `mini_render_only_cmd`.

---

### Task 3: Implement CWD in Local Environment & Metadata Recording (GREEN Phase 2)

**Files:**
- Modify: `src/env/local.rs`
- Modify: `src/run/mini.rs`
- Modify: `src/trajectory/mod.rs`
- Modify: `src/artifact.rs`
- Modify: `src/run/render_only.rs`

**Step 1: Wire CWD to LocalEnvironment**
- Make `LocalEnvironment` run commands under `workdir`.

**Step 2: Metadata and Version bumps**
- Record `local_workdir` in trajectory.
- Bump trajectory to `1.3` and schema version to `1.10`.
- Update `RenderOnlyReport` formatting.

**Step 3: Run integration tests**
`cargo test --test workdir` should pass.

---

### Task 4: Refactor and Verify Gaps (REFACTOR Phase)

**Files:**
- Modify: Existing integration tests in `tests/`
- Modify: `README.md`

**Step 1: Update README**
- Update the quickstart snippet using `--workdir`.

**Step 2: Replace `set_current_dir` in existing integration tests**
- Select at least one existing test that uses `set_current_dir` and rewrite it to use `--workdir`.
- Ensure all tests pass.
