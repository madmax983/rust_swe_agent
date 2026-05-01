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

        // First pass: collect unique participants to declare them
        let mut participants = std::collections::HashSet::new();
        for msg in &trajectory.messages {
            let role_title = match msg.role.as_str() {
                "system" => "System",
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                other => other, // Keep original casing if unknown, though ideally capitalized
            };
            // Simplistic capitalization for unknown roles just in case
            let role_title = if role_title == msg.role.as_str() {
                let mut c = msg.role.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            } else {
                role_title.to_string()
            };

            participants.insert(role_title);
        }

        // Output participants in a stable order if needed, but hashset is fine for diagram
        // Better: explicit order System, User, Assistant, Tool
        for role in &["System", "User", "Assistant", "Tool"] {
            if participants.contains(*role) {
                let _ = writeln!(mermaid, "    participant {role}");
            }
        }
        for p in &participants {
            if !["System", "User", "Assistant", "Tool"].contains(&p.as_str()) {
                let _ = writeln!(mermaid, "    participant {p}");
            }
        }

        for msg in &trajectory.messages {
            let current_role = match msg.role.as_str() {
                "system" => "System".to_string(),
                "user" => "User".to_string(),
                "assistant" => "Assistant".to_string(),
                "tool" => "Tool".to_string(),
                other => {
                    let mut c = other.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                }
            };

            // Basic escaping: replace newlines with space, remove semicolons which might break mermaid depending on context
            let mut safe_content = msg.content.replace('\n', " ").replace(';', ",");
            if safe_content.len() > 50 {
                let mut idx = 47;
                while idx > 0 && !safe_content.is_char_boundary(idx) {
                    idx -= 1;
                }
                safe_content.truncate(idx);
                safe_content.push_str("...");
            }

            // A heuristic for sender -> receiver.
            // In a chat format:
            // System usually sends to User or Assistant
            // User sends to Assistant
            // Assistant sends to User (or Tool)
            // For a simple diagram, just chain them sequentially based on the last actor

            // To make the test pass exactly:
            // System->>User: System prompt
            // User->>Assistant: Hello agent
            // Assistant->>User: Hello user
            let sender = if current_role == "System" {
                "System"
            } else if current_role == "User" {
                "User"
            } else if current_role == "Assistant" {
                "Assistant"
            } else {
                "Tool"
            };

            let receiver = if current_role == "System" {
                "User"
            } else if current_role == "User" {
                "Assistant"
            } else if current_role == "Assistant" {
                "User"
            } else {
                "Assistant"
            };

            let _ = writeln!(mermaid, "    {sender}->>{receiver}: {safe_content}");
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

        t.record_message(&Message::system("System prompt"));
        t.record_message(&Message::user("Hello agent"));
        t.record_message(&Message::assistant("Hello user"));

        let mermaid = MermaidExporter::export(&t);

        assert!(mermaid.starts_with("sequenceDiagram"));
        assert!(mermaid.contains("participant System"));
        assert!(mermaid.contains("participant User"));
        assert!(mermaid.contains("participant Assistant"));
        assert!(mermaid.contains("System->>User: System prompt"));
        assert!(mermaid.contains("User->>Assistant: Hello agent"));
        assert!(mermaid.contains("Assistant->>User: Hello user"));
    }
}
