//! Export utilities for transforming trajectories into human-readable formats.
//!
//! While the primary `.traj.json` format is optimized for machine replay and metric
//! extraction, it can be dense for humans. This module provides exporters (like
//! [`crate::trajectory::export::MarkdownExporter`]) that render the back-and-forth conversation into a clean
//! narrative document, complete with headers and code blocks.
//!
//! You can extend this module with new formats by implementing the [`crate::trajectory::export::TrajectoryExporter`] trait.

use super::Trajectory;
use crate::redaction::{Redactor, surface};

/// A contract for types that can convert a [`Trajectory`] into a specialized string format.
///
/// Implement this trait to provide a new serialization layout (e.g., Markdown, CSV).
pub trait TrajectoryExporter {
    /// Transforms the provided [`Trajectory`] into a formatted `String`.
    fn export(trajectory: &Trajectory) -> String;
}

/// Transforms a [`Trajectory`] into a structured Markdown document.
///
/// It renders the task, outcome, and all messages sequentially under appropriate headers.
///
/// ## Examples
///
/// ```rust
/// use maxwells_daemon::trajectory::Trajectory;
/// use maxwells_daemon::model::Message;
/// use maxwells_daemon::trajectory::export::{TrajectoryExporter, MarkdownExporter};
///
/// let mut traj = Trajectory::new();
/// traj.info.task = Some("Fix tests".to_string());
/// traj.record_message(&Message::user("Hello agent"));
///
/// let md = MarkdownExporter::export(&traj);
/// assert!(md.contains("# Trajectory Export"));
/// assert!(md.contains("**Task:** Fix tests"));
/// assert!(md.contains("### User"));
/// assert!(md.contains("Hello agent"));
/// ```
pub struct MarkdownExporter;

/// Transforms a [`Trajectory`] into a flat CSV file, with `role` and `content` columns.
///
/// Note: This exporter properly handles and escapes embedded quotes and newlines in message content.
#[cfg(feature = "csv-export")]
pub struct CsvExporter;

#[cfg(feature = "mermaid-export")]
pub struct MermaidExporter;

#[cfg(feature = "html-export")]
pub struct HtmlExporter;

#[cfg(feature = "bash-export")]
pub struct BashScriptExporter;

use std::fmt::Write;

#[cfg(feature = "csv-export")]
impl TrajectoryExporter for CsvExporter {
    fn export(trajectory: &Trajectory) -> String {
        let redactor = Redactor::default_enabled();
        let mut csv = String::new();
        csv.push_str("role,content\n");

        for msg in &trajectory.messages {
            let role = msg.role.as_str();
            let content = redactor.redact_text(&msg.content, surface::EXPORT).text;

            let escaped_content = if content.contains('"')
                || content.contains(',')
                || content.contains('\n')
                || content.contains('\r')
            {
                format!("\"{}\"", content.replace('"', "\"\""))
            } else {
                content
            };

            let _ = writeln!(csv, "{role},{escaped_content}");
        }

        csv
    }
}

impl TrajectoryExporter for MarkdownExporter {
    fn export(trajectory: &Trajectory) -> String {
        let redactor = Redactor::default_enabled();
        let mut md = String::new();

        md.push_str("# Trajectory Export\n\n");

        if let Some(task) = &trajectory.info.task {
            let task = redactor.redact_text(task, surface::EXPORT).text;
            let _ = write!(md, "**Task:** {task}\n\n");
        }

        if let Some(outcome) = &trajectory.info.outcome {
            let outcome = redactor.redact_text(outcome, surface::EXPORT).text;
            let _ = write!(md, "**Outcome:** {outcome}\n\n");
        }

        md.push_str("## Messages\n\n");

        for msg in &trajectory.messages {
            let role_title = match msg.role.as_str() {
                "system" => "System",
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                other => other,
            };

            let content = redactor.redact_text(&msg.content, surface::EXPORT).text;
            let _ = write!(md, "### {role_title}\n\n{content}\n\n");
        }

        md
    }
}

#[cfg(feature = "html-export")]
impl TrajectoryExporter for HtmlExporter {
    fn export(trajectory: &Trajectory) -> String {
        let redactor = Redactor::default_enabled();
        let mut html = String::new();

        html.push_str("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"UTF-8\">\n");
        html.push_str("<title>Trajectory Export</title>\n");
        html.push_str("<style>\n");
        html.push_str(
            "body { font-family: sans-serif; max-width: 800px; margin: 0 auto; padding: 20px; }\n",
        );
        html.push_str(".message { margin-bottom: 20px; padding: 15px; border-radius: 8px; }\n");
        html.push_str(".system { background-color: #f8d7da; color: #721c24; }\n");
        html.push_str(".user { background-color: #d1ecf1; color: #0c5460; }\n");
        html.push_str(".assistant { background-color: #d4edda; color: #155724; }\n");
        html.push_str(".tool { background-color: #e2e3e5; color: #383d41; font-family: monospace; white-space: pre-wrap; }\n");
        html.push_str("</style>\n</head>\n<body>\n");

        html.push_str("<h1>Trajectory Export</h1>\n");

        if let Some(task) = &trajectory.info.task {
            let task = redactor.redact_text(task, surface::EXPORT).text;
            let safe_task = task
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let _ = writeln!(html, "<p><strong>Task:</strong> {safe_task}</p>");
        }

        if let Some(outcome) = &trajectory.info.outcome {
            let outcome_redacted = redactor.redact_text(outcome, surface::EXPORT).text;
            let safe_outcome = outcome_redacted
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace(';', "&#59;");
            let _ = writeln!(html, "<p><strong>Outcome:</strong> {safe_outcome}</p>");
        }

        for msg in &trajectory.messages {
            let role_class = msg.role.as_str();
            let role_title = match role_class {
                "system" => "System",
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                other => other,
            };

            let content = redactor.redact_text(&msg.content, surface::EXPORT).text;
            let safe_content = content
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('\n', "<br>");

            let safe_role_title = role_title
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace(';', "&#59;");
            let safe_role_class = role_class.replace('"', "&quot;");

            let _ = writeln!(
                html,
                "<div class=\"message {safe_role_class}\">\n<h2>{safe_role_title}</h2>\n<p>{safe_content}</p>\n</div>"
            );
        }

        html.push_str("</body>\n</html>");
        html
    }
}

#[cfg(feature = "mermaid-export")]
impl TrajectoryExporter for MermaidExporter {
    fn export(trajectory: &Trajectory) -> String {
        let redactor = Redactor::default_enabled();
        let mut mermaid = String::new();
        mermaid.push_str("sequenceDiagram\n");

        if let Some(task) = &trajectory.info.task {
            let task = redactor.redact_text(task, surface::EXPORT).text;
            let _ = writeln!(mermaid, "    title {task}");
        }

        mermaid.push_str("    participant S as System\n");
        mermaid.push_str("    participant U as User\n");
        mermaid.push_str("    participant A as Assistant\n");
        mermaid.push_str("    participant T as Tool\n\n");

        let mut prev_role = "U";
        for msg in &trajectory.messages {
            let role_abbr = match msg.role.as_str() {
                "system" => "S",
                "assistant" => "A",
                "tool" => "T",
                _ => "U",
            };

            let content = redactor.redact_text(&msg.content, surface::EXPORT).text;
            let safe_content = content
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace(';', "#59;")
                .replace('\n', "<br>");

            if role_abbr == "S" {
                let _ = writeln!(mermaid, "    S->>S: {safe_content}");
            } else {
                let _ = writeln!(mermaid, "    {prev_role}->>{role_abbr}: {safe_content}");
                prev_role = role_abbr;
            }
        }

        if let Some(outcome) = &trajectory.info.outcome {
            let outcome = redactor.redact_text(outcome, surface::EXPORT).text;
            let _ = writeln!(mermaid, "\n    Note over S,T: Outcome: {outcome}");
        }

        mermaid
    }
}

#[cfg(feature = "bash-export")]
impl TrajectoryExporter for BashScriptExporter {
    fn export(trajectory: &Trajectory) -> String {
        let mut script = String::new();
        script.push_str(
            "#!/usr/bin/env bash
",
        );
        script.push_str(
            "set -euo pipefail

",
        );

        if let Some(task) = &trajectory.info.task {
            // We do not redact the task for the bash script as it is just a comment,
            // but we can still use the redactor for consistency.
            let redactor = Redactor::default_enabled();
            let task = redactor.redact_text(task, surface::EXPORT).text;
            let _ = writeln!(script, "# Task: {}", task.replace('\n', " "));
        }
        script.push('\n');

        // Note: tool calls are stored in the assistant's `actions` extra metadata,
        // OR as tool messages. We need to extract the bash tool calls.
        // Wait, looking at `MessageRecord`, if the role is "assistant", `extra.actions` contains
        // tool calls. Wait, `actions` might be JSON encoded. Wait, `ToolCall::bash` input is the script.
        // Actually, if we look at `ToolCall::action_label`, it just returns `input`.
        // Let's see how actions are recorded. In `MessageExtra`, `actions` is `Option<Vec<String>>`.
        // If it's a tool call, `actions` has `ToolCall` representations.

        // Let's write a simple implementation that extracts bash blocks from assistant's `content` or `actions`.
        // Or better yet, we can look for `actions` that contain bash scripts or just look at `role == "assistant"`
        // and parse for `ToolCall` or `bash`.
        // A safer way: tool executions are recorded in `role == "tool"`. But `MessageRecord` doesn't have the `name` of the tool.
        // Wait, if we use regex or simply extract anything within ```bash ... ``` in assistant messages.
        // Yes, the agent generates bash inside ```bash ... ```. This is the simplest and most robust way.
        let mut step_count = 1;
        for msg in &trajectory.messages {
            if msg.role == "assistant" {
                let content = &msg.content;
                let mut in_bash_block = false;
                let mut current_block = String::new();

                for line in content.lines() {
                    if line.trim().starts_with("```bash") {
                        in_bash_block = true;
                        continue;
                    }
                    if line.trim() == "```" && in_bash_block {
                        in_bash_block = false;
                        let _ = writeln!(script, "# Step {step_count}");
                        script.push_str(&current_block);
                        script.push('\n');
                        current_block.clear();
                        step_count += 1;
                        continue;
                    }
                    if in_bash_block {
                        current_block.push_str(line);
                        current_block.push('\n');
                    }
                }
            }
        }

        script
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Message;
    use crate::trajectory::outcome;

    #[test]
    fn test_markdown_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());
        t.info.outcome = Some(outcome::SUBMITTED.to_string());

        t.record_message(&Message::system("System prompt"));
        t.record_message(&Message::user("Hello agent"));
        t.record_message(&Message::assistant("Hello user"));

        let md = MarkdownExporter::export(&t);

        assert!(md.contains("# Trajectory Export"));
        assert!(md.contains("**Task:** Add a feature"));
        assert!(md.contains("**Outcome:** submitted"));
        assert!(md.contains("## Messages"));
        assert!(md.contains("### System"));
        assert!(md.contains("System prompt"));
        assert!(md.contains("### User"));
        assert!(md.contains("Hello agent"));
        assert!(md.contains("### Assistant"));
        assert!(md.contains("Hello user"));
    }

    #[cfg(feature = "csv-export")]
    #[test]
    fn test_csv_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());
        t.info.outcome = Some(outcome::SUBMITTED.to_string());

        t.record_message(&Message::system("System prompt"));
        t.record_message(&Message::user("Hello agent\nMulti-line"));
        t.record_message(&Message::assistant("Hello \"user\""));

        let csv = CsvExporter::export(&t);

        assert!(csv.starts_with("role,content"));
        assert!(csv.contains("system,System prompt"));
        assert!(csv.contains("user,\"Hello agent\nMulti-line\""));
        assert!(csv.contains("assistant,\"Hello \"\"user\"\"\""));
    }

    #[cfg(feature = "mermaid-export")]
    #[test]
    fn test_mermaid_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());
        t.info.outcome = Some(outcome::SUBMITTED.to_string());

        t.record_message(&Message::system("System prompt; echo 1 >&2"));
        t.record_message(&Message::user("Hello agent\nMulti-line"));
        t.record_message(&Message::assistant("Hello \"user\""));

        let mermaid = MermaidExporter::export(&t);

        assert!(mermaid.starts_with("sequenceDiagram"));
        assert!(mermaid.contains("title Add a feature"));
        assert!(mermaid.contains("participant S as System"));
        assert!(mermaid.contains("participant U as User"));
        assert!(mermaid.contains("participant A as Assistant"));
        assert!(mermaid.contains("participant T as Tool"));

        assert!(mermaid.contains("S->>S: System prompt#59; echo 1 &gt#59;&amp#59;2"));
        assert!(mermaid.contains("U->>U: Hello agent<br>Multi-line"));
        assert!(mermaid.contains("U->>A: Hello \"user\""));

        assert!(mermaid.contains("Note over S,T: Outcome: submitted"));
    }

    #[cfg(feature = "html-export")]
    #[test]
    fn test_html_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());
        t.info.outcome = Some("submitted".to_string());

        t.record_message(&Message::user("Hello agent"));
        t.record_message(&Message::user("Hello user"));

        let html = HtmlExporter::export(&t);

        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>Trajectory Export</title>"));
        assert!(html.contains("Add a feature"));
        assert!(html.contains("submitted"));
        assert!(html.contains("Hello agent"));
        assert!(html.contains("Hello user"));
    }

    #[cfg(feature = "bash-export")]
    #[test]
    fn test_bash_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Debug script".to_string());

        t.record_message(&Message::assistant(
            "I will run this:
```bash
echo 'hello'
ls -la
```",
        ));

        let bash = BashScriptExporter::export(&t);

        assert!(bash.starts_with("#!/usr/bin/env bash"));
        assert!(bash.contains("set -euo pipefail"));
        assert!(bash.contains("# Task: Debug script"));
        assert!(bash.contains("# Step 1"));
        assert!(bash.contains("echo 'hello'"));
        assert!(bash.contains("ls -la"));
    }
}
