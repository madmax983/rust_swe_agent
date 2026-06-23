1. **The Spark:** "I noticed we export trajectories to markdown, csv, mermaid, and html, but there isn't a direct JSON export format that applies redaction using the same standard path as other exporters."
2. **The Feature:** "Implemented `JsonExporter` and a `--format traj-json` output option to provide a purely redacted JSON string output. Since `.traj.json` is normally written by the harness, having a standard `traj-json` export command can allow downstream systems to pipe heavily redacted raw trajectories without custom post-processing."
3. **The Potential:** "Could be used for safely transferring trajectory artifacts over un-trusted pipelines or into third-party dashboards via jq."
4. **Risk:** "Low. Isolated in `src/trajectory/export.rs`."

Steps:
1. Update `Cargo.toml` to add the `traj-json-export` feature using `replace_with_git_merge_diff`.
2. Implement `JsonExporter` and register it in `src/trajectory/export.rs` using `replace_with_git_merge_diff`.
3. Add test for `traj-json` export in `src/trajectory/export.rs` using `replace_with_git_merge_diff`.
4. Fix the unrelated clippy issues in `src/env/docker.rs` to allow the build to pass.
5. Verify compilation by running `cargo check --all-targets --all-features`.
6. Run unit tests explicitly for the modified module `cargo test --lib trajectory::export::tests --all-features`.
7. Run the integration test explicitly to verify the redactor `cargo test --test export_redaction_conformance --all-features`.
8. Complete pre commit steps to make sure proper testing, verifications, reviews and reflections are done.
9. Submit PR titled "🌟 Nova: [traj-json export format]" with PR description.
