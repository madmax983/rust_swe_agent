use super::Trajectory;

pub trait TrajectoryExporter {
    fn export(trajectory: &Trajectory) -> String;
}

pub struct MarkdownExporter;

#[cfg(feature = "csv-export")]
pub struct CsvExporter;

use std::fmt::Write;

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
}

#[cfg(feature = "html-export")]
pub struct HtmlExporter;

#[cfg(feature = "html-export")]
impl TrajectoryExporter for HtmlExporter {
    fn export(trajectory: &Trajectory) -> String {
        let mut html = String::new();

        html.push_str("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>Trajectory Export</title>\n");
        html.push_str("<style>\n");
        html.push_str("body { font-family: sans-serif; line-height: 1.6; max-width: 800px; margin: 0 auto; padding: 20px; }\n");
        html.push_str(".message { margin-bottom: 20px; padding: 15px; border-radius: 5px; }\n");
        html.push_str(".system { background-color: #f0f0f0; border-left: 5px solid #ccc; }\n");
        html.push_str(".user { background-color: #e6f3ff; border-left: 5px solid #0066cc; }\n");
        html.push_str(".assistant { background-color: #e6ffe6; border-left: 5px solid #00cc00; }\n");
        html.push_str(".tool { background-color: #fff0e6; border-left: 5px solid #ff6600; }\n");
        html.push_str("pre { white-space: pre-wrap; word-wrap: break-word; }\n");
        html.push_str("</style>\n</head>\n<body>\n");

        html.push_str("<h1>Trajectory Export</h1>\n");

        if let Some(task) = &trajectory.info.task {
            let _ = writeln!(
                html,
                "<p><strong>Task:</strong> {}</p>",
                html_escape(task)
            );
        }

        if let Some(outcome) = &trajectory.info.outcome {
            let _ = writeln!(
                html,
                "<p><strong>Outcome:</strong> {}</p>",
                html_escape(outcome)
            );
        }

        html.push_str("<h2>Messages</h2>\n");

        for msg in &trajectory.messages {
            let role_class = msg.role.as_str();
            let role_title = match msg.role.as_str() {
                "system" => "System",
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                other => other,
            };

            let _ = write!(
                html,
                "<div class=\"message {}\">\n<h3>{}</h3>\n<pre>{}</pre>\n</div>\n",
                role_class,
                role_title,
                html_escape(&msg.content)
            );
        }

        html.push_str("</body>\n</html>");

        html
    }
}

#[cfg(feature = "html-export")]
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
     .replace('\'', "&#39;")
}

#[cfg(test)]
#[cfg(feature = "html-export")]
pub mod html_tests {
    use super::*;
    use crate::model::Message;
    use crate::trajectory::outcome;

    #[test]
    fn test_html_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature <tag>".to_string());
        t.info.outcome = Some(outcome::SUBMITTED.to_string());

        t.record_message(&Message::system("System prompt & test"));
        t.record_message(&Message::user("Hello agent"));
        t.record_message(&Message::assistant("Hello user"));

        let html = HtmlExporter::export(&t);

        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("<h1>Trajectory Export</h1>"));
        assert!(html.contains("<strong>Task:</strong> Add a feature &lt;tag&gt;"));
        assert!(html.contains("<strong>Outcome:</strong> submitted"));
        assert!(html.contains("<h2>Messages</h2>"));
        assert!(html.contains("<div class=\"message system\">"));
        assert!(html.contains("<h3>System</h3>"));
        assert!(html.contains("System prompt &amp; test"));
        assert!(html.contains("<div class=\"message user\">"));
        assert!(html.contains("<h3>User</h3>"));
        assert!(html.contains("Hello agent"));
        assert!(html.contains("<div class=\"message assistant\">"));
        assert!(html.contains("<h3>Assistant</h3>"));
        assert!(html.contains("Hello user"));
    }
}
