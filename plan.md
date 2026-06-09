1. **Add `jupyter-export` feature to `Cargo.toml`.**
2. **Implement `JupyterExporter` in `src/trajectory/export.rs`.**
   - Implements `TrajectoryExporter`.
   - Generates a valid JSON string representing an `.ipynb` file.
   - Maps `Message` into Jupyter cells.
     - `system` -> Markdown cell
     - `user` -> Markdown cell
     - `assistant` -> Markdown cell (contains reasoning)
     - `tool` (bash) -> Code cell (with the bash command and output!)
3. **Integrate it into `src/cli/mod.rs`.**
   - Add `"jupyter"` to the `--format` list.
   - Dispatch to `JupyterExporter`.
4. **Integrate it into `src/cli/args.rs`.**
   - Update help text for `--format` in `InspectCmd`.
5. **Write tests.**
   - Add a unit test for `JupyterExporter` in `src/trajectory/export.rs` inside the `#[cfg(test)] mod tests` block.
6. **Pre-commit checks.**
   - `cargo fmt --all`
   - `cargo clippy --all-targets --all-features -- -D warnings`
   - `cargo test`
7. **Submit the PR as Nova.**
   - Title: `🌟 Nova: [Jupyter Notebook Exporter]`
   - Description following Nova's template.
