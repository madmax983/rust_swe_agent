use super::Trajectory;

pub trait TrajectoryExporter {
    fn export(trajectory: &Trajectory) -> String;
}

pub struct MarkdownExporter;

#[cfg(feature = "csv-export")]
pub struct CsvExporter;

#[cfg(feature = "mermaid-export")]
pub struct MermaidExporter;

use std::fmt::Write;

#[cfg(feature = "mermaid-export")]
impl TrajectoryExporter for MermaidExporter {
    fn export(trajectory: &Trajectory) -> String {
        let mut mermaid = String::new();
        mermaid.push_str("sequenceDiagram\n");
        mermaid.push_str("    actor User\n");
        mermaid.push_str("    participant Agent\n");
        mermaid.push_str("    participant Environment\n\n");

        for msg in &trajectory.messages {
            match msg.role.as_str() {
                "system" => {
                    let content = msg.content.clone().replace('\n', " ");
                    let mut truncated = content.clone();
                    if truncated.len() > 50 {
                        let mut end = 47;
                        while !truncated.is_char_boundary(end) {
                            end -= 1;
                        }
                        truncated = format!("{}...", &truncated[..end]);
                    }
                    let _ = writeln!(mermaid, "    Note over Agent: System: {truncated}");
                }
                "user" => {
                    let content = msg.content.clone().replace('\n', " ");
                    let mut truncated = content.clone();
                    if truncated.len() > 50 {
                        let mut end = 47;
                        while !truncated.is_char_boundary(end) {
                            end -= 1;
                        }
                        truncated = format!("{}...", &truncated[..end]);
                    }
                    if content.contains("Observation") || content.contains("Exit code") {
                        let _ = writeln!(mermaid, "    Environment->>Agent: {truncated}");
                    } else {
                        let _ = writeln!(mermaid, "    User->>Agent: {truncated}");
                    }
                }
                "assistant" => {
                    let content = msg.content.clone();
                    let first_line = content.lines().next().unwrap_or("").to_string();
                    let mut preview = first_line.clone();
                    if preview.len() > 50 {
                        let mut end = 47;
                        while !preview.is_char_boundary(end) {
                            end -= 1;
                        }
                        preview = format!("{}...", &preview[..end]);
                    }
                    if content.contains("```bash") {
                        let _ = writeln!(mermaid, "    Agent->>Environment: Bash action");
                    } else if content.contains("COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT") {
                        let _ = writeln!(mermaid, "    Agent->>User: Task Complete");
                    } else {
                        let _ = writeln!(mermaid, "    Agent->>Agent: {preview}");
                    }
                }
                _ => {}
            }
        }

        mermaid
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

        t.record_message(&Message::system("System prompt with a very long line that should definitely be truncated because it exceeds the fifty character limit"));
        t.record_message(&Message::user("Observation: test passed"));
        t.record_message(&Message::user("Hello agent"));
        t.record_message(&Message::assistant("```bash\necho hi\n```"));
        t.record_message(&Message::assistant("Thinking about this problem..."));
        t.record_message(&Message::assistant(
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nDone\n```",
        ));

        let mermaid = MermaidExporter::export(&t);

        assert!(mermaid.starts_with("sequenceDiagram"));
        assert!(mermaid.contains("actor User"));
        assert!(mermaid.contains("participant Agent"));
        assert!(mermaid.contains("participant Environment"));

        assert!(mermaid.contains(
            "Note over Agent: System: System prompt with a very long line that should..."
        ));
        assert!(mermaid.contains("Environment->>Agent: Observation: test passed"));
        assert!(mermaid.contains("User->>Agent: Hello agent"));
        assert!(mermaid.contains("Agent->>Environment: Bash action"));
        assert!(mermaid.contains("Agent->>Agent: Thinking about this problem..."));
        assert!(mermaid.contains("Agent->>User: Task Complete"));
    }
}
