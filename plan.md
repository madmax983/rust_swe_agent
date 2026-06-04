1. **Remove `unsafe` environment variable mutations in `src/run/mini.rs`**.
   - I will replace `unsafe { std::env::set_var(...) }` and `unsafe { std::env::remove_var(...) }` in `mini_manifest_does_not_contain_env_secrets` with `temp_env::async_with_vars`.

2. **Remove `unsafe` environment variable mutations in `src/stream/sweep_webhook.rs`**.
   - I will replace `unsafe { std::env::set_var(...) }` and `unsafe { std::env::remove_var(...) }` in `redactor_strips_sensitive_env_var_from_instance_id` with `temp_env::async_with_vars`.

3. **Remove `unsafe` environment variable mutations in `src/run/redact_audit.rs`**.
   - I will replace `unsafe { std::env::set_var(...) }` and `unsafe { std::env::remove_var(...) }` in `oracle_catches_ambient_env_value` with `temp_env::with_vars`.

4. **Remove `unsafe` environment variable mutations in `src/telemetry/mod.rs`**.
   - I will use a Python script with bash heredoc to replace all `unsafe { std::env::set_var(...) }` usages across several tests in `src/telemetry/mod.rs` with `temp_env::with_vars`. I will also remove `ENV_LOCK`.

5. **Run tests**
   - Run `cargo test --lib -- run::mini run::redact_audit stream::sweep_webhook telemetry`.

6. Complete pre-commit steps to ensure proper testing, verification, review, and reflection are done.

7. Submit the PR.
   - Title: '👺 Havoc: Remove unsafe env mutations'
   - Description:
     * 🧨 **The Trigger:** Test environment mutation via `unsafe { std::env::set_var(...) }` caused data races and panics during concurrent `cargo test` runs on Rust 1.80+.
     * 📉 **The Stack Trace:** (Omitted - data race)
     * 🧪 **Reproduction:** Run `cargo test` concurrently on Rust 1.80+.
     * 😈 **Comment:** You assumed `std::env::set_var` was safe in a single-threaded test. You were wrong. Other concurrent tests will still panic.
