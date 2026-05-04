//! Export utilities for transforming trajectories into human-readable formats.
//!
//! While the primary `.traj.json` format is optimized for machine replay and metric
//! extraction, it can be dense for humans. This module provides exporters (like
//! [`MarkdownExporter`]) that render the back-and-forth conversation into a clean
//! narrative document, complete with headers and code blocks.
//!
//! You can extend this module with new formats by implementing the [`TrajectoryExporter`] trait.

use super::Trajectory;

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
/// use rust_swe_agent::trajectory::Trajectory;
/// use rust_swe_agent::model::Message;
/// use rust_swe_agent::trajectory::export::{TrajectoryExporter, MarkdownExporter};
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

use std::fmt::Write;

#[cfg(feature = "html-export")]
impl TrajectoryExporter for HtmlExporter {
    fn export(trajectory: &Trajectory) -> String {
        let mut html = String::new();

        html.push_str("<!DOCTYPE html>\n");
        html.push_str("<html>\n");
        html.push_str("<head>\n");
        html.push_str("<meta charset=\"utf-8\">\n");
        html.push_str("<title>Trajectory Export</title>\n");
        html.push_str("<style>\n");
        html.push_str("body { font-family: sans-serif; padding: 20px; }\n");
        html.push_str(".message { margin-bottom: 20px; padding: 10px; border-radius: 5px; }\n");
        html.push_str(".system { background-color: #f0f0f0; border: 1px solid #ccc; }\n");
        html.push_str(".user { background-color: #e6f2ff; border: 1px solid #b3d9ff; }\n");
        html.push_str(".assistant { background-color: #e6ffe6; border: 1px solid #b3ffb3; }\n");
        html.push_str(".tool { background-color: #fff2e6; border: 1px solid #ffcc99; }\n");
        html.push_str("pre { white-space: pre-wrap; }\n");
        html.push_str("</style>\n");
        html.push_str("</head>\n");
        html.push_str("<body>\n");
        html.push_str("<h1>Trajectory Export</h1>\n");

        if let Some(task) = &trajectory.info.task {
            let safe_task = task
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let _ = writeln!(html, "<div><strong>Task:</strong> {safe_task}</div>");
        }

        if let Some(outcome) = &trajectory.info.outcome {
            let safe_outcome = outcome
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let _ = writeln!(html, "<div><strong>Outcome:</strong> {safe_outcome}</div>");
        }

        html.push_str("<hr>\n");

        for msg in &trajectory.messages {
            let role_class = msg.role.as_str();
            let role_title = match role_class {
                "system" => "System",
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                other => other,
            };

            let safe_content = msg
                .content
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\n', "<br>");

            let _ = writeln!(html, "<div class=\"message {role_class}\">");
            let _ = writeln!(html, "  <strong>{role_title}</strong>");
            let _ = writeln!(html, "  <p>{safe_content}</p>");
            html.push_str("</div>\n");
        }

        html.push_str("</body>\n");
        html.push_str("</html>\n");

        html
    }
}

#[cfg(feature = "csv-export")]
impl TrajectoryExporter for CsvExporter {
    fn export(trajectory: &Trajectory) -> String {
        let mut csv = String::new();
        csv.push_str("role,content\n");

        for msg in &trajectory.messages {
            let role = msg.role.as_str();

            let escaped_content = if msg.content.contains('"')
                || msg.content.contains(',')
                || msg.content.contains('\n')
                || msg.content.contains('\r')
            {
                format!("\"{}\"", msg.content.replace('"', "\"\""))
            } else {
                msg.content.clone()
            };

            let _ = writeln!(csv, "{role},{escaped_content}");
        }

        csv
    }
}

impl TrajectoryExporter for MarkdownExporter {
    fn export(trajectory: &Trajectory) -> String {
        let mut md = String::new();

        md.push_str("# Trajectory Export\n\n");

        if let Some(task) = &trajectory.info.task {
            let _ = write!(md, "**Task:** {task}\n\n");
        }

        if let Some(outcome) = &trajectory.info.outcome {
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

            let _ = write!(md, "### {role_title}\n\n{}\n\n", msg.content);
        }

        md
    }
}

#[cfg(feature = "mermaid-export")]
impl TrajectoryExporter for MermaidExporter {
    fn export(trajectory: &Trajectory) -> String {
        let mut mermaid = String::new();
        mermaid.push_str("sequenceDiagram\n");

        if let Some(task) = &trajectory.info.task {
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

            let safe_content = msg
                .content
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
            let _ = writeln!(mermaid, "\n    Note over S,T: Outcome: {outcome}");
        }

        mermaid
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

    #[cfg(feature = "html-export")]
    #[test]
    fn test_html_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());
        t.info.outcome = Some(outcome::SUBMITTED.to_string());

        t.record_message(&Message::system("System prompt; echo 1 >&2"));
        t.record_message(&Message::user("Hello agent\nMulti-line"));
        t.record_message(&Message::assistant("Hello \"user\""));

        let html = HtmlExporter::export(&t);

        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html>"));
        assert!(html.contains("<head>"));
        assert!(html.contains("<title>Trajectory Export</title>"));
        assert!(html.contains("<body>"));
        assert!(html.contains("<h1>Trajectory Export</h1>"));
        assert!(html.contains("<strong>Task:</strong> Add a feature"));
        assert!(html.contains("<strong>Outcome:</strong> submitted"));

        assert!(html.contains("<div class=\"message system\">"));
        assert!(
            html.contains("System prompt#59; echo 1 &gt#59;&amp#59;2")
                || html.contains("System prompt; echo 1 &gt;&amp;2")
        ); // Depending on escape method
        assert!(html.contains("<div class=\"message user\">"));
        assert!(html.contains("Hello agent<br>Multi-line"));
        assert!(html.contains("<div class=\"message assistant\">"));
        assert!(html.contains("Hello &quot;user&quot;") || html.contains("Hello \"user\"")); // Depending on escape method
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
}
