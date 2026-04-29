use super::Trajectory;

pub trait TrajectoryExporter {
    fn export(trajectory: &Trajectory) -> String;
}

#[cfg(feature = "markdown-export")]
pub struct MarkdownExporter;

#[cfg(any(feature = "markdown-export", feature = "html-export"))]
use std::fmt::Write;

#[cfg(feature = "html-export")]
pub struct HtmlExporter;

#[cfg(feature = "html-export")]
impl TrajectoryExporter for HtmlExporter {
    fn export(trajectory: &Trajectory) -> String {
        let mut html = String::new();

        html.push_str("<!DOCTYPE html>\n");
        html.push_str("<html>\n<head>\n<title>Trajectory Export</title>\n</head>\n<body>\n");
        html.push_str("<h1>Trajectory Export</h1>\n\n");

        if let Some(task) = &trajectory.info.task {
            let _ = writeln!(html, "<p><strong>Task:</strong> {}</p>", html_escape(task));
        }

        if let Some(outcome) = &trajectory.info.outcome {
            let _ = writeln!(
                html,
                "<p><strong>Outcome:</strong> {}</p>",
                html_escape(outcome)
            );
        }

        html.push_str("<h2>Messages</h2>\n\n");

        for msg in &trajectory.messages {
            let role_title = match msg.role.as_str() {
                "system" => "System",
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                other => other,
            };

            let _ = writeln!(html, "<h3>{role_title}</h3>");
            let _ = writeln!(html, "<pre>{}</pre>", html_escape(&msg.content));
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
        .replace('\'', "&#x27;")
}

#[cfg(feature = "markdown-export")]
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
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::model::Message;
    #[allow(unused_imports)]
    use crate::trajectory::outcome;

    #[cfg(feature = "markdown-export")]
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

    #[cfg(feature = "html-export")]
    #[test]
    fn test_html_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());
        t.info.outcome = Some(outcome::SUBMITTED.to_string());

        t.record_message(&Message::system("System prompt"));
        t.record_message(&Message::user("Hello agent"));
        t.record_message(&Message::assistant("Hello user"));

        let html = HtmlExporter::export(&t);

        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("<html>"));
        assert!(html.contains("<body>"));
        assert!(html.contains("<h1>Trajectory Export</h1>"));
        assert!(html.contains("<strong>Task:</strong> Add a feature"));
        assert!(html.contains("<strong>Outcome:</strong> submitted"));
        assert!(html.contains("<h2>Messages</h2>"));
        assert!(html.contains("<h3>System</h3>"));
        assert!(html.contains("<pre>System prompt</pre>"));
        assert!(html.contains("<h3>User</h3>"));
        assert!(html.contains("<pre>Hello agent</pre>"));
        assert!(html.contains("<h3>Assistant</h3>"));
        assert!(html.contains("<pre>Hello user</pre>"));
    }
}
