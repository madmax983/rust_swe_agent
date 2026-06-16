1. **Add Display and FromStr to Role:** In `src/model/mod.rs`, implement `std::fmt::Display` and `std::str::FromStr` for `Role` so it can be parsed to and serialized from a string naturally (`system`, `user`, `assistant`, `tool`).
2. **Refactor trajectory parsing:** In `src/trajectory/mod.rs`, replace the manual `match rec.role.as_str() { ... }` parsing with `rec.role.parse().unwrap_or(Role::User)`.
3. **Refactor trajectory serialization:** In `src/trajectory/mod.rs`, remove `role_to_string` and replace `role_to_string(m.role)` with `m.role.to_string()`.
4. **Refactor fingerprint JSON:** In `src/fingerprint.rs`, replace the manual match inside `role_to_json_str` with `serde_json::Value::String(role.to_string())`.
5. **Run tests & clippy:** Ensure `cargo fmt`, `cargo clippy`, and `cargo test` pass.
