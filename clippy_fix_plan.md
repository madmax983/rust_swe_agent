1.  **Refactor `run` in `src/run/audit.rs`**
    - Execute a Python script (`refactor_audit.py`) using `run_in_bash_session` to read `src/run/audit.rs`, parse its logic, and write a refactored version of the file.
    - The refactored version will extract the large blocks in `run` into several private helper functions (e.g. `parse_results_instances`, `parse_evaluation_instances`, `check_bijective_trajectory_results`, `check_bijective_trajectory_evaluation`, `validate_rerun_completeness`, `recompute_aggregates`, `reconcile_outcomes`, `reconcile_sweep_aggregates`, `check_contradictions`, `check_durations`, `check_dataset_hash`), and remove `#[allow(clippy::too_many_lines)]`.
2.  **Verify modifications directly on src/run/audit.rs with git diff**
    - Verify the unstaged edits to `src/run/audit.rs` using `git diff`.
3.  **Verify compilation and tests**
    - Run `cargo clippy --all-targets --all-features -- -D warnings` to verify warnings are resolved.
    - Run `cargo test` to ensure tests still pass.
4.  **Complete pre-commit steps**
    - Complete pre-commit steps to ensure proper testing, verification, review, and reflection are done.
5.  **Submit PR**
    - Create PR with title "⚒️ Forge: Refactor run in src/run/audit.rs"
    - Description with:
      - 🚮 Smell: `run` function in `src/run/audit.rs` is over 600 lines long and has a high cognitive complexity.
      - ✨ Solution: Extracted logic into smaller, named private helper functions to flatten the structure and reduce cognitive load. Removed the `#[allow(clippy::too_many_lines)]` attribute.
      - 🧼 Benefit: Drastically improves readability and maintainability without changing runtime behavior.
      - 🛡️ Verification: `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test` passed. No logic changed.
