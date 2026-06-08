The `run` function in `src/run/audit.rs` is over 600 lines long and has a cognitive complexity of 103, triggering Clippy warnings `clippy::too_many_lines` and `clippy::cognitive_complexity`. To resolve this, I will refactor the function according to the Forge persona directives. I'll extract logic into smaller private helper functions with clear names, reducing nesting and improving readability.

Here is the plan:
1.  **Refactor `run` in `src/run/audit.rs`**
    - Extract `parse_trajectory_paths` to handle discovering trajectories.
    - Extract `parse_results_instances` to parse and build the maps from `results.json`.
    - Extract `parse_evaluation_instances` to parse `evaluation.json`.
    - Extract `check_bijective_trajectory_results` and `check_bijective_trajectory_evaluation` to perform bijective checks.
    - Extract `validate_rerun_completeness`.
    - Extract `recompute_and_validate_aggregates`.
    - Extract `validate_outcomes` and `validate_contradictions`.
    - The main `run` function will become a sequence of these helper calls, drastically reducing its length and cognitive complexity.
2.  **Verify compilation and tests**
    - Ensure `cargo clippy --all-targets --all-features -- -D warnings` runs cleanly.
    - Ensure `cargo test` passes.
3.  **Complete pre-commit steps**
    - Complete pre-commit steps.
4.  **Submit PR**
    - Create PR with title "⚒️ Forge: [Refactor `run` in `src/run/audit.rs`]"
