1. **Fix `rustfmt` failure**: Run `cargo fmt --all` to format the code properly. The `rustfmt` check failed due to a line length issue in `src/run/rate_limit.rs`: `let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();`.
2. **Fix `clippy` failure**: Add `#![allow(clippy::unwrap_used)]` or `#[allow(clippy::unwrap_used)]` to the test in `src/run/rate_limit.rs`.
3. Submit the changes again using `submit` tool to the `havoc-rate-limit-panic` branch.
