1. **Modify Cargo.toml**: Add `jsonl-export` feature flag using `replace_with_git_merge_diff`.
   ```
   <<<<<<< SEARCH
   csv-export = []
   mermaid-export = []
   # Enables the `max ui` local sweep browser. Requires `html-export` for rendering.
   ui-server = ["html-export"]
   =======
   csv-export = []
   jsonl-export = []
   mermaid-export = []
   # Enables the `max ui` local sweep browser. Requires `html-export` for rendering.
   ui-server = ["html-export"]
   >>>>>>> REPLACE
   ```
2. **Modify docs/spec-inspect.md**: Update documentation to mention `jsonl` export format using `replace_with_git_merge_diff`.
   ```
   <<<<<<< SEARCH
     | `csv`      | Flat CSV with `role` and `content` columns             | `csv-export`           |
     | `mermaid`  | Mermaid `sequenceDiagram` of the conversation          | `mermaid-export`       |
     | `unified`  | Unified diff (diff mode only)                          | —                      |

     When the binary is built without the required feature, `--format <name>` exits
   =======
     | `csv`      | Flat CSV with `role` and `content` columns             | `csv-export`           |
     | `jsonl`    | JSON Lines formatted exported dataset                  | `jsonl-export`         |
     | `mermaid`  | Mermaid `sequenceDiagram` of the conversation          | `mermaid-export`       |
     | `unified`  | Unified diff (diff mode only)                          | —                      |

     When the binary is built without the required feature, `--format <name>` exits
   >>>>>>> REPLACE
   ```
   and
   ```
   <<<<<<< SEARCH
   * `--output <PATH>`: write output to a file instead of stdout. Supported with
     `markdown`, `html`, `csv`, and `mermaid` formats in instance mode. Stdout is
     empty when `--output` is set.
   =======
   * `--output <PATH>`: write output to a file instead of stdout. Supported with
     `markdown`, `html`, `csv`, `jsonl`, and `mermaid` formats in instance mode. Stdout is
     empty when `--output` is set.
   >>>>>>> REPLACE
   ```
   and
   ```
   <<<<<<< SEARCH
   Export formats (`markdown`, `html`, `csv`, `mermaid`) require `--instance` and
   `--sweep`; they cannot be combined with `--filter`.
   =======
   Export formats (`markdown`, `html`, `csv`, `jsonl`, `mermaid`) require `--instance` and
   `--sweep`; they cannot be combined with `--filter`.
   >>>>>>> REPLACE
   ```
3. **Modify src/trajectory/export.rs**: Add `JsonlExporter` struct, `TrajectoryExporter` implementation and tests using `replace_with_git_merge_diff`.
   ```
   <<<<<<< SEARCH
   #[cfg(feature = "csv-export")]
   pub struct CsvExporter;

   #[cfg(feature = "mermaid-export")]
   =======
   #[cfg(feature = "csv-export")]
   pub struct CsvExporter;

   #[cfg(feature = "jsonl-export")]
   pub struct JsonlExporter;

   #[cfg(feature = "mermaid-export")]
   >>>>>>> REPLACE
   ```
   and
   ```
   <<<<<<< SEARCH
   impl TrajectoryExporter for MarkdownExporter {
   =======
   #[cfg(feature = "jsonl-export")]
   impl TrajectoryExporter for JsonlExporter {
       fn export(trajectory: &Trajectory) -> String {
           let redactor = Redactor::default_enabled();
           let mut jsonl = String::new();

           for msg in &trajectory.messages {
               let role = msg.role.as_str();
               let content = redactor.redact_text(&msg.content, surface::EXPORT).text;

               // Use a custom inline representation of the record
               let record = serde_json::json!({
                   "role": role,
                   "content": content
               });

               let mut line = serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_string());
               line.push('\n');
               jsonl.push_str(&line);
           }

           jsonl
       }
   }

   impl TrajectoryExporter for MarkdownExporter {
   >>>>>>> REPLACE
   ```
   and
   ```
   <<<<<<< SEARCH
       #[cfg(feature = "mermaid-export")]
       #[test]
       fn test_mermaid_export_format() {
   =======
       #[cfg(feature = "jsonl-export")]
       #[test]
       fn test_jsonl_export_format() {
           let mut t = Trajectory::new();
           t.info.task = Some("Add a feature".to_string());
           t.info.outcome = Some(outcome::SUBMITTED.to_string());

           t.record_message(&Message::system("System prompt"));
           t.record_message(&Message::user("Hello agent\nMulti-line"));

           let jsonl = JsonlExporter::export(&t);

           assert!(jsonl.contains(r#"{"content":"System prompt","role":"system"}"#) || jsonl.contains(r#"{"role":"system","content":"System prompt"}"#));
           assert!(jsonl.contains(r#"{"content":"Hello agent\nMulti-line","role":"user"}"#) || jsonl.contains(r#"{"role":"user","content":"Hello agent\nMulti-line"}"#));
           assert_eq!(jsonl.lines().count(), 2);
       }

       #[cfg(feature = "mermaid-export")]
       #[test]
       fn test_mermaid_export_format() {
   >>>>>>> REPLACE
   ```
4. **Modify src/cli/args.rs**: Update `--format` and `--output` documentation using `replace_with_git_merge_diff`.
   ```
   <<<<<<< SEARCH
       /// Output format: `text` (default), `json`, or `unified` in diff mode.
       /// In instance mode, also accepts `markdown`, `html`, `csv`, and `mermaid`
       /// (each maps to the corresponding trajectory exporter; feature-gated
       /// formats require the matching Cargo feature at build time).
       #[arg(long, default_value = "text")]
       pub format: String,

       /// Write output to a file instead of stdout. Supported with `markdown`,
       /// `html`, `csv`, and `mermaid` formats in instance mode.
       #[arg(long, value_name = "PATH")]
       pub output: Option<PathBuf>,
   =======
       /// Output format: `text` (default), `json`, or `unified` in diff mode.
       /// In instance mode, also accepts `markdown`, `html`, `csv`, `jsonl`, and `mermaid`
       /// (each maps to the corresponding trajectory exporter; feature-gated
       /// formats require the matching Cargo feature at build time).
       #[arg(long, default_value = "text")]
       pub format: String,

       /// Write output to a file instead of stdout. Supported with `markdown`,
       /// `html`, `csv`, `jsonl`, and `mermaid` formats in instance mode.
       #[arg(long, value_name = "PATH")]
       pub output: Option<PathBuf>,
   >>>>>>> REPLACE
   ```
5. **Modify src/cli/mod.rs**: Wire up `JsonlExporter` using `replace_with_git_merge_diff`.
   ```
   <<<<<<< SEARCH
       if matches!(i.format.as_str(), "markdown" | "html" | "csv" | "mermaid") {
           return bench_inspect_export(i);
       }

       if i.output.is_some() {
           return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
               "inspect: --output is only supported with export formats (markdown/html/csv/mermaid), not `{}`",
               i.format
           ))));
       }
   =======
       if matches!(i.format.as_str(), "markdown" | "html" | "csv" | "jsonl" | "mermaid") {
           return bench_inspect_export(i);
       }

       if i.output.is_some() {
           return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
               "inspect: --output is only supported with export formats (markdown/html/csv/jsonl/mermaid), not `{}`",
               i.format
           ))));
       }
   >>>>>>> REPLACE
   ```
   and
   ```
   <<<<<<< SEARCH
       if !matches!(i.format.as_str(), "markdown" | "html" | "csv" | "mermaid") {
           return Err(Error::Config(crate::error::ConfigError::Invalid(
               "inspect: --output is only supported with export formats (markdown/html/csv/mermaid)".into(),
           )));
       }
   =======
       if !matches!(i.format.as_str(), "markdown" | "html" | "csv" | "jsonl" | "mermaid") {
           return Err(Error::Config(crate::error::ConfigError::Invalid(
               "inspect: --output is only supported with export formats (markdown/html/csv/jsonl/mermaid)".into(),
           )));
       }
   >>>>>>> REPLACE
   ```
   and
   ```
   <<<<<<< SEARCH
           "html" => inspect_export_html(&traj)?,
           "csv" => inspect_export_csv(&traj)?,
           "mermaid" => inspect_export_mermaid(&traj)?,
           _ => unreachable!("dispatch guarded by caller"),
       };
   =======
           "html" => inspect_export_html(&traj)?,
           "csv" => inspect_export_csv(&traj)?,
           "jsonl" => inspect_export_jsonl(&traj)?,
           "mermaid" => inspect_export_mermaid(&traj)?,
           _ => unreachable!("dispatch guarded by caller"),
       };
   >>>>>>> REPLACE
   ```
   and
   ```
   <<<<<<< SEARCH
   #[cfg(not(feature = "csv-export"))]
   fn inspect_export_csv(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
       Err(Error::Config(crate::error::ConfigError::Invalid(
           "format_unavailable: --format csv requires the `csv-export` Cargo feature; \
            rebuild with `--features csv-export`"
               .into(),
       )))
   }

   #[cfg(feature = "mermaid-export")]
   =======
   #[cfg(not(feature = "csv-export"))]
   fn inspect_export_csv(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
       Err(Error::Config(crate::error::ConfigError::Invalid(
           "format_unavailable: --format csv requires the `csv-export` Cargo feature; \
            rebuild with `--features csv-export`"
               .into(),
       )))
   }

   #[cfg(feature = "jsonl-export")]
   fn inspect_export_jsonl(traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
       use crate::trajectory::export::{JsonlExporter, TrajectoryExporter};
       Ok(JsonlExporter::export(traj))
   }

   #[cfg(not(feature = "jsonl-export"))]
   fn inspect_export_jsonl(_traj: &crate::trajectory::Trajectory) -> Result<String, Error> {
       Err(Error::Config(crate::error::ConfigError::Invalid(
           "format_unavailable: --format jsonl requires the `jsonl-export` Cargo feature; \
            rebuild with `--features jsonl-export`"
               .into(),
       )))
   }

   #[cfg(feature = "mermaid-export")]
   >>>>>>> REPLACE
   ```
6. **Verify Source Modifications**: Verify the file edits with `run_in_bash_session` using `git diff`.
7. **Verify Completeness**: Test modified modules sequentially by running `run_in_bash_session` using `cargo test --lib trajectory && cargo test --lib cli`. Ensure `cargo check` passes.
8. **Complete pre-commit steps**: Complete pre-commit steps to ensure proper testing, verification, review, and reflection are done.
9. **Submit**: Create PR using `submit_pr`.
   - Title: `🌟 Nova: JSONL Trajectory Exporter`
   - Description:
     💡 **The Spark:** "We can export trajectories to human readable formats like markdown, HTML, and Mermaid, but we can't easily export them back to JSONL for fine-tuning or feeding to other tools. If we have a JSONL exporter, it becomes trivial to extract the trajectory messages and feed them directly into other tools, bypassing the large complex `.traj.json` object."
     🚀 **The Feature:** "Implemented `JsonlExporter` trait in `src/trajectory/export.rs` and added `jsonl-export` Cargo feature flag. It's wired into the `max bench inspect` CLI command with `--format jsonl`."
     🔮 **The Potential:** "Could be used for data extraction and exporting training datasets to feed into LLMs or testing systems directly from completed agent sweeps."
     ⚠️ **Risk:** "Low. Isolated in `src/trajectory/export.rs` behind a feature flag and doesn't affect core logic."
