# Harness Skills Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add harness-level agent skills so the registry stays private and only selected skill bodies are injected into the agent prompt.

**Architecture:** The harness scans configured skill directories, parses `SKILL.md` frontmatter into a private registry, resolves relevant skills from explicit mentions and task text, loads only selected skill bodies, and passes the active context to `DefaultAgentBuilder`. The agent never receives the complete skill registry by default.

**Tech Stack:** Rust 2024, existing config overlay system, `serde`, `tempfile`, focused unit/integration tests.

---

### Task 1: Skill Registry

**Files:**
- Create: `src/skills.rs`
- Modify: `src/lib.rs`
- Test: `tests/skills.rs`

**Steps:**
1. Write failing tests for scanning `SKILL.md` files and parsing only `name` and `description` frontmatter.
2. Implement a small frontmatter parser without adding dependencies.
3. Add content loading that returns the full skill body for selected skills.

### Task 2: Harness Resolver

**Files:**
- Modify: `src/skills.rs`
- Test: `tests/skills.rs`

**Steps:**
1. Write failing tests for explicit `$skill-name` activation and keyword activation from `name`/`description`.
2. Implement deterministic resolver behavior with deduplication and stable ordering.
3. Record activation reason and SHA-256 provenance.

### Task 3: Config Surface

**Files:**
- Modify: `src/config/schema.rs`
- Modify: `src/config/mod.rs`
- Modify: `src/config/defaults/default.toml`
- Test: `tests/config_reference.rs`

**Steps:**
1. Write failing tests for `[skills] enabled`, `paths`, and `auto_load`.
2. Add `SkillCfg` to root config with safe defaults.
3. Validate configured skill paths and document defaults.

### Task 4: Prompt Injection

**Files:**
- Modify: `src/agent/default.rs`
- Modify: `src/run/mini.rs`
- Modify: `src/run/replay.rs`
- Modify: `src/config/defaults/default.toml`
- Test: `tests/agent_loop.rs` or `tests/skills.rs`

**Steps:**
1. Write failing tests proving inactive skill metadata is absent from initial prompts.
2. Write failing tests proving selected skill body is present in system prompt.
3. Thread `active_skills` from the harness into `DefaultAgentBuilder`.
4. Add active skill provenance to the trajectory manifest.

### Task 5: Verification

**Files:**
- Affected Rust and docs files.

**Steps:**
1. Run `cargo fmt --all`.
2. Run targeted skill tests.
3. Run `cargo test --all-targets --all-features`.
4. Run `cargo clippy --all-targets --all-features -- -D warnings`.
5. Scan affected areas for `TODO`, `FIXME`, and stubs.
