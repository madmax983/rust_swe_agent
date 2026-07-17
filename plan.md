1. **Refactor tests in `src/env/docker.rs` to remove `.unwrap()`**
   - The test functions `build_run_args_include_network_none_when_mode_is_none` and `build_run_args_network_none_positioned_before_image` in `src/env/docker.rs` currently use `.unwrap()` on `Option`, which triggers `clippy::unwrap_used` errors.
   - I will replace these `.unwrap()` calls with `let Some(pos) = ... else { panic!(...) }` or `anyhow::Result<()>` signatures to align with Forge's philosophy of handling Option gracefully even in tests.
   - Wait, Forge's philosophy also mentions returning `anyhow::Result<()>` in tests to avoid `unwrap_used` where appropriate. We can change the test signature to return `anyhow::Result<()>` and use `?`.

   Let's check the exact code for `build_run_args_include_network_none_when_mode_is_none`. We can replace:
   ```rust
   let network_pos = args.iter().position(|a| a == "--network");
   assert!(
       network_pos.is_some(),
       "expected --network flag in args: {args:?}"
   );
   assert_eq!(
       args.get(network_pos.unwrap() + 1).map(String::as_str),
       Some("none")
   );
   ```
   with:
   ```rust
   let Some(network_pos) = args.iter().position(|a| a == "--network") else {
       panic!("expected --network flag in args: {args:?}");
   };
   assert_eq!(
       args.get(network_pos + 1).map(String::as_str),
       Some("none")
   );
   ```
   And for `build_run_args_network_none_positioned_before_image`:
   ```rust
   let Some(network_pos) = args.iter().position(|a| a == "--network") else {
       panic!("expected --network flag in args");
   };
   let Some(image_pos) = args.iter().position(|a| a == "my-image") else {
       panic!("expected image flag in args");
   };
   ```

   This matches the memory guideline: "When refactoring tests to resolve `clippy::unwrap_used` or `clippy::expect_used` warnings on `Result` or `Option` types, change the test signature to return `anyhow::Result<()>` and use the `?` operator. In contexts where `?` cannot be used, use `let Ok(...) = ... else { panic!(...) }` or `let Some(...) = ... else { panic!(...) }` instead."

2. **Final Verification**
   - Run `cargo fmt --all`
   - Run `cargo clippy --all-targets --all-features -- -D warnings`
   - Run `cargo test env::docker --features docker` (if the module tests support running standalone, otherwise just `cargo test`)

3. **Pre Commit**
   - Complete pre-commit instructions.
4. **Submit PR**
   - "⚒️ Forge: Remove unwrap calls in docker env tests"
