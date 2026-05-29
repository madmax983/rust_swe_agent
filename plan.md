1. **Target**: `src/run/matrix.rs`
2. **Risk**: `build_summary` and `render_summary_text` have important logic for transforming matrix runs into reports, but currently lack test coverage (0% coverage per `cargo llvm-cov`).
3. **Strategy**: Add a `#[cfg(test)] mod tests` module at the bottom of `src/run/matrix.rs`. Write unit tests for `build_summary` to verify ranking, delta computations, and formatting. Write a unit test for `render_summary_text` to verify table generation logic. We will need to construct dummy `MatrixState` and `ArmStatus` structs to test these functions.
4. **Verification**: `cargo test --lib run::matrix::tests` and `cargo llvm-cov test --manifest-path Cargo.toml --lib run::matrix`
