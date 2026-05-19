# issue #336: Add mini `--task-file` to pass multi-line task descriptions without shell escaping

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the `--task-file <path>` option for the `mini` subcommand, allowing operators to supply multi-line task descriptions from a file or stdin (`-`) without shell-escaping issues. Ensure strict mutual exclusion with `--task`, validate empty or unreadable sources, and guarantee recorded/rendered tasks are byte-identical to the source inputs.

**Architecture:**
- Modify `args::MiniCmd` in `src/cli/args.rs` to change `task` to an `Option<String>` and introduce `task_file` as an `Option<String>`.
- Inside `src/cli/mod.rs` (under the `mini_cmd` and `mini_render_only_cmd` dispatch paths), manually validate that exactly one of `task` or `task_file` is provided. If validation fails, return `Error::Config(ConfigError::Invalid(...))`, which will map to `ExitCode::UsageError` (2) and write `outcome_class: usage_error` to stderr.
- Implement loading task content from a local file or stdin (if `--task-file` is `-`), stripping the UTF-8 BOM `\u{FEFF}` if present, but preserving all other bytes (whitespace, newlines, etc.) to guarantee byte-identity.
- Verify through new integration tests in `tests/mini_task_file.rs`.

**Tech Stack:** Rust, Clap (derive), Standard I/O, SHA-256 for integrity verification.

---

## User Review Required

> [!IMPORTANT]
> Change to existing behavior: `args::MiniCmd::task` is changed from `String` to `Option<String>` to accommodate `--task-file`. If neither is supplied, Max will gracefully exit with code `2` rather than letting clap crash with standard validation. This provides more control and stable, machine-readable outcomes.

> [!NOTE]
> UTF-8 Byte Order Mark (BOM) `\u{FEFF}` is stripped if it exists at the very beginning of the task source. This is documented in the specification file to ensure standard text handling on Windows.

## Open Questions

*None currently identified. The issue specifications are highly precise and comprehensive.*

---

## Proposed Changes

### CLI Arguments

#### [MODIFY] [args.rs](file:///c:/Users/markm/rust_swe_agent/src/cli/args.rs)
- Change `pub task: String` to `pub task: Option<String>`.
- Add `pub task_file: Option<String>` with doc comment: `Path to a file containing the task prompt, or '-' to read from stdin.`

### Command Dispatch & Validation

#### [MODIFY] [mod.rs](file:///c:/Users/markm/rust_swe_agent/src/cli/mod.rs)
- Adapt `mini_cmd` and `mini_render_only_cmd` signatures and implementations to extract and validate the task source.
- Return `Error::Config(ConfigError::Invalid(...))` on:
  - Both `--task` and `--task-file` specified
  - Neither specified
  - Task source is empty or whitespace-only
  - File doesn't exist or is unreadable
- Strip BOM `\u{FEFF}` from raw task file/stdin input.

### Integration Tests

#### [NEW] [mini_task_file.rs](file:///c:/Users/markm/rust_swe_agent/tests/mini_task_file.rs)
- Comprehensive test suite checking:
  - Mutual exclusion of flags (both specified, neither specified)
  - Empty `--task`
  - Empty `--task-file` (file & stdin)
  - Nonexistent `--task-file`
  - Successful file reading (verifying byte-identity via SHA-256 in `--render-only --format json` output)
  - Stdin reading (`-`)
  - BOM stripping
  - 50-line markdown success metric

### Documentation

#### [NEW] [spec-mini-task-file.md](file:///c:/Users/markm/rust_swe_agent/docs/spec-mini-task-file.md)
- Complete technical specification covering flag semantics, mutual exclusion, exit codes, encoding, and the stdin sentinel `-`.

#### [MODIFY] [README.md](file:///c:/Users/markm/rust_swe_agent/README.md)
- Add quickstart section for `--task-file` with PowerShell and bash invocation examples.

---

## Technical Execution Plan (TDD Steps)

### Task 1: Initialize Integration Tests (RED Phase)

**Files:**
- Create: `tests/mini_task_file.rs`

**Step 1: Write the failing tests**
Create the skeleton test file `tests/mini_task_file.rs` containing tests for:
- Both `--task` and `--task-file` specified (expects exit code 2 and "both --task and --task-file" error)
- Neither specified (expects exit code 2 and "either --task or --task-file must be provided")
- Empty `--task` (expects exit code 2)
- Nonexistent `--task-file` (expects exit code 2)
- Successful file task loading
- Stdin task loading
- BOM stripping
- 50-line markdown task SHA-256 validation

**Step 2: Run tests to verify they fail**
Run: `cargo test --test mini_task_file`
Expected: Compilation failure or execution failure because `--task-file` parameter is unknown, and `--task` is still a required `String`.

---

### Task 2: Implement CLI Argument Changes (GREEN Phase 1)

**Files:**
- Modify: `src/cli/args.rs`
- Modify: `src/cli/mod.rs` (minimal changes to allow compilation)

**Step 1: Modify `args.rs`**
Update `MiniCmd`:
```rust
    /// The task prompt.
    #[arg(long)]
    pub task: Option<String>,

    /// Path to a file containing the task prompt, or '-' to read from stdin.
    #[arg(long)]
    pub task_file: Option<String>,
```
Update test/mock helper at line 2860 of `mod.rs` if needed to pass `task: None, task_file: None`.

**Step 2: Compile and verify failure changes**
Run: `cargo test --test mini_task_file`
Expected: Still fails or errors, but compiles successfully or fails on validation.

---

### Task 3: Implement Task Loading and Validation (GREEN Phase 2)

**Files:**
- Modify: `src/cli/mod.rs`

**Step 1: Write validation & extraction logic**
Implement task extraction in `mini_cmd` and feed it into `mini_render_only_cmd`.
```rust
    let task = match (&m.task, &m.task_file) {
        (Some(_), Some(_)) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "both --task and --task-file were provided".into(),
            )));
        }
        (None, None) => {
            return Err(Error::Config(crate::error::ConfigError::Invalid(
                "either --task or --task-file must be provided".into(),
            )));
        }
        (Some(t), None) => {
            if t.trim().is_empty() {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "empty --task source".into(),
                )));
            }
            t.clone()
        }
        (None, Some(tf)) => {
            let mut raw_content = if tf == "-" {
                let mut buffer = String::new();
                use std::io::Read;
                std::io::stdin().read_to_string(&mut buffer).map_err(|e| {
                    Error::Config(crate::error::ConfigError::Invalid(format!(
                        "failed to read task from stdin: {e}"
                    )))
                })?;
                buffer
            } else {
                let path = std::path::Path::new(tf);
                if !path.exists() {
                    return Err(Error::Config(crate::error::ConfigError::Invalid(
                        format!("--task-file does not exist: {}", tf),
                    )));
                }
                std::fs::read_to_string(path).map_err(|e| {
                    Error::Config(crate::error::ConfigError::Invalid(format!(
                        "failed to read --task-file `{tf}`: {e}"
                    )))
                })?
            };

            if raw_content.starts_with('\u{FEFF}') {
                raw_content.remove(0);
            }

            if raw_content.trim().is_empty() {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    format!("empty task source from `{tf}`"),
                )));
            }

            raw_content
        }
    };
```
Pass `task` to `mini_render_only_cmd` and replace all other `m.task` with the resolved `task` variable.

**Step 2: Run tests to verify they pass**
Run: `cargo test --test mini_task_file`
Expected: PASS. All 27 existing `render_only` tests must also be run and pass.

---

### Task 4: Refactor & Lint (REFACTOR Phase)

**Files:**
- Modify: `src/cli/mod.rs` (clean up helper methods if needed)
- Run formatters and lints

**Step 1: Clean up code**
Format the code and run clippy.
Run: `cargo fmt` and `cargo clippy --all-targets`

**Step 2: Verify tests remain green**
Run: `cargo test`

---

### Task 5: Documentation & Specification

**Files:**
- Create: `docs/spec-mini-task-file.md`
- Modify: `README.md`

**Step 1: Write spec document**
Document the `--task-file` flag behavior, stdin sentinel `-`, exit codes, BOM stripping, byte-identical formatting, etc.

**Step 2: Update README.md**
Add an example of calling `--task-file` with standard and PowerShell/bash commands.

**Step 3: Run quickstart verification test**
Verify the documentation examples compile/work perfectly.

---

## Verification Plan

### Automated Tests
- Run `cargo test --test mini_task_file` to test our new feature.
- Run `cargo test --test render_only` to ensure no regression in `render_only` paths.
- Run full suite: `cargo test` to ensure all tests pass.

### Manual Verification
- Execute `target/debug/max mini --render-only --task-file - --format json` manually with multi-line input in the terminal and verify output correctness.
