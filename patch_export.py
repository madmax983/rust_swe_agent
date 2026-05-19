import sys

with open('src/trajectory/export.rs', 'r') as f:
    content = f.read()

# Add JupyterExporter struct
struct_def = """#[cfg(feature = "jupyter-export")]
pub struct JupyterExporter;
"""
if struct_def not in content:
    content = content.replace('pub struct HtmlExporter;', 'pub struct HtmlExporter;\n\n' + struct_def)

# Add JupyterExporter impl
impl_def = """
#[cfg(feature = "jupyter-export")]
impl TrajectoryExporter for JupyterExporter {
    fn export(trajectory: &Trajectory) -> String {
        let redactor = Redactor::default_enabled();

        let mut cells = Vec::new();

        // Title cell
        let mut title_source = vec!["# Trajectory Export\\n\\n".to_string()];

        if let Some(task) = &trajectory.info.task {
            let task = redactor.redact_text(task, surface::EXPORT).text;
            title_source.push(format!("**Task:** {}\\n\\n", task));
        }

        if let Some(outcome) = &trajectory.info.outcome {
            let outcome_redacted = redactor.redact_text(outcome, surface::EXPORT).text;
            title_source.push(format!("**Outcome:** {}\\n", outcome_redacted));
        }

        cells.push(serde_json::json!({
            "cell_type": "markdown",
            "metadata": {},
            "source": title_source
        }));

        for msg in &trajectory.messages {
            let role_title = match msg.role.as_str() {
                "system" => "System",
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                other => other,
            };

            let content = redactor.redact_text(&msg.content, surface::EXPORT).text;

            // Format as a single markdown string
            let mut cell_source = Vec::new();
            cell_source.push(format!("### {}\\n\\n", role_title));

            // Basic formatting - could be enhanced to detect code blocks
            // and create actual code cells
            for line in content.split('\\n') {
                cell_source.push(format!("{}\\n", line));
            }

            cells.push(serde_json::json!({
                "cell_type": "markdown",
                "metadata": {},
                "source": cell_source
            }));
        }

        let notebook = serde_json::json!({
            "cells": cells,
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5
        });

        serde_json::to_string_pretty(&notebook).unwrap_or_default()
    }
}
"""
if "impl TrajectoryExporter for JupyterExporter" not in content:
    content = content.replace('#[cfg(test)]\nmod tests {', impl_def + '\n#[cfg(test)]\nmod tests {')

# Add test
test_def = """
    #[cfg(feature = "jupyter-export")]
    #[test]
    fn test_jupyter_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());
        t.info.outcome = Some("submitted".to_string());

        t.record_message(&Message::user("Hello agent"));

        let jupyter = JupyterExporter::export(&t);

        assert!(jupyter.contains("\\"nbformat\\": 4"));
        assert!(jupyter.contains("Add a feature"));
        assert!(jupyter.contains("submitted"));
        assert!(jupyter.contains("Hello agent"));

        // Ensure it parses as valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&jupyter).unwrap();
        assert!(parsed.get("cells").is_some());
    }
"""
if "test_jupyter_export_format" not in content:
    content = content.replace('    }\n}', '    }\n' + test_def + '}')

with open('src/trajectory/export.rs', 'w') as f:
    f.write(content)
