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
            "body { font-family: system-ui, -apple-system, sans-serif; max-width: 900px; margin: 0 auto; padding: 20px; background-color: #f9fafb; color: #1f2937; line-height: 1.6; }\n",
        );
        html.push_str(
            "h1 { color: #111827; border-bottom: 2px solid #e5e7eb; padding-bottom: 10px; }\n",
        );
        html.push_str("p { margin-bottom: 1.5em; }\n");
        html.push_str("strong { color: #374151; }\n");
        html.push_str(".message { margin-bottom: 24px; padding: 18px 24px; border-radius: 12px; box-shadow: 0 1px 3px rgba(0,0,0,0.1); }\n");
        html.push_str(".message::before { display: block; font-weight: 600; margin-bottom: 8px; font-size: 0.85em; text-transform: uppercase; letter-spacing: 0.05em; }\n");
        html.push_str(".system { background-color: #fef2f2; color: #991b1b; border-left: 4px solid #ef4444; }\n");
        html.push_str(".system::before { content: 'System'; color: #b91c1c; }\n");
        html.push_str(".user { background-color: #eff6ff; color: #1e3a8a; border-left: 4px solid #3b82f6; }\n");
        html.push_str(".user::before { content: 'User'; color: #2563eb; }\n");
        html.push_str(".assistant { background-color: #f0fdf4; color: #166534; border-left: 4px solid #22c55e; }\n");
        html.push_str(".assistant::before { content: 'Assistant'; color: #15803d; }\n");
        html.push_str(".tool { background-color: #f3f4f6; color: #374151; border-left: 4px solid #6b7280; font-family: ui-monospace, monospace; white-space: pre-wrap; font-size: 0.9em; }\n");
        html.push_str(".tool::before { content: 'Tool'; color: #4b5563; font-family: system-ui, -apple-system, sans-serif; }\n");
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
}
