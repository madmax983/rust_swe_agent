//! Export utilities for transforming trajectories into human-readable formats.
//!
//! While the primary `.traj.json` format is optimized for machine replay and metric
//! extraction, it can be dense for humans. This module provides exporters (like
//! [`crate::trajectory::export::MarkdownExporter`]) that render the back-and-forth conversation into a clean
//! narrative document, complete with headers and code blocks.
//!
//! You can extend this module with new formats by implementing the [`crate::trajectory::export::TrajectoryExporter`] trait.
//! Every new exporter MUST register in [`registry`] and MUST apply redaction via
//! [`crate::redaction::Redactor::default_enabled`] on [`crate::redaction::surface::EXPORT`] before emitting any output.
//! See `docs/spec-export.md` for the full governing contract.

use super::Trajectory;
use crate::redaction::{Redactor, surface};

/// Stability tier for a trajectory export format.
///
/// `stable` formats guarantee that schema/layout changes require a documented version bump.
/// `experimental` formats may change without notice.
/// See `docs/spec-export.md` for the full tier definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StabilityTier {
    Stable,
    Experimental,
}

impl StabilityTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Experimental => "experimental",
        }
    }
}

/// Descriptor for a registered trajectory export format.
///
/// The `render` function pointer calls the exporter implementation directly, which ensures
/// the registry stays in sync with the trait impls without duplicating redaction logic.
pub struct ExportFormat {
    /// CLI `--format` value (e.g. `"markdown"`, `"csv"`).
    pub name: &'static str,
    /// Stability guarantee for this format's output layout.
    pub tier: StabilityTier,
    /// Intended downstream consumer (for documentation and `--list-formats` output).
    pub consumer: &'static str,
    /// Render function. MUST apply redaction via `Redactor::default_enabled()` on `surface::EXPORT`.
    pub render: fn(&Trajectory) -> String,
}

/// Returns every trajectory export format compiled into this build.
///
/// This is the single source of truth for the redaction conformance test, `--list-formats`,
/// and CLI dispatch. Feature-gated formats (csv, html, mermaid) appear only when the
/// corresponding Cargo feature is enabled.
///
/// When adding a new exporter:
/// 1. Implement `TrajectoryExporter` with mandatory `Redactor::default_enabled()` redaction.
/// 2. Add an `ExportFormat` entry here (feature-gated if behind a Cargo feature).
/// 3. Document the format and its tier in `docs/spec-export.md`.
/// 4. The shared conformance test in `tests/export_redaction_conformance.rs` will cover it automatically.
pub fn registry() -> Vec<ExportFormat> {
    // `mut` is only used when at least one feature-gated format is compiled in.
    // Under `--no-default-features` none of the `push` calls exist, so the binding
    // would otherwise trip `-D warnings`/`unused_mut`.
    #[allow(unused_mut)]
    let mut formats = vec![ExportFormat {
        name: "markdown",
        tier: StabilityTier::Stable,
        consumer: "docs, PR review, human readers",
        render: MarkdownExporter::export,
    }];

    #[cfg(feature = "csv-export")]
    formats.push(ExportFormat {
        name: "csv",
        tier: StabilityTier::Stable,
        consumer: "spreadsheets, jq pipelines, tabular tools",
        render: CsvExporter::export,
    });

    #[cfg(feature = "html-export")]
    formats.push(ExportFormat {
        name: "html",
        tier: StabilityTier::Stable,
        consumer: "self-contained browser view, shared notebooks",
        render: HtmlExporter::export,
    });

    #[cfg(feature = "mermaid-export")]
    formats.push(ExportFormat {
        name: "mermaid",
        tier: StabilityTier::Experimental,
        consumer: "Mermaid sequence-diagram renderers (GitLab, GitHub markdown, mermaid.live)",
        render: MermaidExporter::export,
    });

    #[cfg(feature = "asciicast-export")]
    formats.push(ExportFormat {
        name: "asciicast",
        tier: StabilityTier::Experimental,
        consumer: "Asciinema player (terminal session replay)",
        render: AsciicastExporter::export,
    });

    formats
}

/// Export format names that exist in the codebase but are gated behind a Cargo feature,
/// paired with the feature that enables each.
///
/// Used to emit a helpful "feature not compiled in" error when an operator requests a
/// format whose feature is disabled. Compiled-in formats (with metadata and a render fn)
/// live in [`registry`]; a format gated *out* of this build is absent from `registry()`
/// but present here so the CLI can still route it and explain how to enable it.
pub const FEATURE_GATED_FORMATS: &[(&str, &str)] = &[
    ("csv", "csv-export"),
    ("html", "html-export"),
    ("mermaid", "mermaid-export"),
    ("asciicast", "asciicast-export"),
];

/// Returns `true` if `name` is a trajectory export format known to this codebase,
/// whether or not its Cargo feature is compiled into the current build.
///
/// CLI routing uses this so a request for a gated-out format still reaches the export
/// dispatch path (and gets a helpful "rebuild with --features" error) instead of falling
/// through to a generic "unknown format" message. This keeps [`registry`] the single
/// source of truth for *compiled* formats while still recognizing the full catalog.
pub fn is_export_format(name: &str) -> bool {
    registry().iter().any(|f| f.name == name)
        || FEATURE_GATED_FORMATS.iter().any(|(n, _)| *n == name)
}

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

#[cfg(feature = "asciicast-export")]
pub struct AsciicastExporter;

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

#[cfg(feature = "asciicast-export")]
impl TrajectoryExporter for AsciicastExporter {
    fn export(trajectory: &Trajectory) -> String {
        let redactor = Redactor::default_enabled();
        let mut cast = String::new();

        // Asciicast v2 header
        let _ = writeln!(
            cast,
            r#"{{"version": 2, "width": 100, "height": 40, "timestamp": 0, "env": {{"TERM": "xterm-256color"}}}}"#
        );

        let mut current_time = 0.0;

        for msg in &trajectory.messages {
            let role = msg.role.as_str();
            let content = redactor.redact_text(&msg.content, surface::EXPORT).text;

            let formatted = match role {
                "system" => format!("\x1b[1;35m[System]\x1b[0m {content}\r\n"),
                "user" => format!("\x1b[1;34m[User]\x1b[0m {content}\r\n"),
                "assistant" => format!("\x1b[1;32m[Assistant]\x1b[0m {content}\r\n"),
                "tool" => format!("\x1b[1;36m[Tool]\x1b[0m {content}\r\n"),
                _ => format!("[{role}] {content}\r\n"),
            };

            let escaped_text = match serde_json::to_string(&formatted) {
                Ok(s) => s,
                Err(_) => format!("\"{}\"", formatted.replace('"', "\\\"")),
            };

            current_time += 1.0;
            let _ = writeln!(cast, "[{current_time:.3}, \"o\", {escaped_text}]");
        }

        cast
    }
}

#[cfg(all(test, feature = "asciicast-export"))]
mod asciicast_tests {
    use super::*;
    use crate::model::Message;

    #[test]
    fn test_asciicast_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());

        t.record_message(&Message::user("Hello agent"));

        let cast = AsciicastExporter::export(&t);

        assert!(cast.starts_with(r#"{"version": 2"#));
        assert!(cast.contains(r#"[User]"#));
        assert!(cast.contains("Hello agent"));
    }
}
