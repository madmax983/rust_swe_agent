//! Prompt-injection defense: XML-envelope wrapping for untrusted content.
//!
//! Implements the "instruction hierarchy" / "XML isolation" pattern from
//! tldrsec/prompt-injection-defenses.  Every piece of untrusted content
//! (task text, tool output, repo files, hook output) is enclosed in a
//! clearly labelled XML tag so the model can distinguish operator-level
//! instructions from untrusted data.
//!
//! The envelopes do not prevent a capable model from reasoning about
//! malicious content inside them, but they provide a clear structural
//! signal that the content is data, not instructions, and they make
//! prompt-injection attempts visible in trajectories.

/// Category of untrusted content fed into the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrustedKind {
    /// The task description supplied by the caller (SWE-bench problem
    /// statement, user request, etc.).
    TaskText,
    /// Supplementary context supplied alongside the task (e.g. repo
    /// metadata, extra instructions).
    ExtraContext,
    /// Stdout/stderr returned by a bash or MCP tool invocation.
    ToolOutput,
    /// Output produced by a pre/post-tool hook.
    HookOutput,
    /// Content read from repository files (README, source code, etc.).
    RepoContent,
}

/// Wraps untrusted content in XML-style envelopes to separate data from
/// operator instructions in the prompt.
pub struct PromptGuard;

impl PromptGuard {
    /// Wrap `content` with the opening and closing XML tags for `kind`.
    ///
    /// The result has the form:
    /// ```text
    /// <untrusted_task_text>
    /// {content}
    /// </untrusted_task_text>
    /// ```
    pub fn wrap(kind: UntrustedKind, content: &str) -> String {
        let tag = Self::tag(kind);
        format!("<{tag}>\n{content}\n</{tag}>")
    }

    /// Return the XML tag name for `kind` (without angle brackets).
    pub fn tag(kind: UntrustedKind) -> &'static str {
        match kind {
            UntrustedKind::TaskText => "untrusted_task_text",
            UntrustedKind::ExtraContext => "untrusted_extra_context",
            UntrustedKind::ToolOutput => "untrusted_tool_output",
            UntrustedKind::HookOutput => "untrusted_hook_output",
            UntrustedKind::RepoContent => "untrusted_repo_content",
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn wrap_roundtrips_content() {
        let content = "hello world";
        let wrapped = PromptGuard::wrap(UntrustedKind::TaskText, content);
        assert!(wrapped.contains(content));
    }

    #[test]
    fn wrap_adds_opening_and_closing_tags() {
        let wrapped = PromptGuard::wrap(UntrustedKind::ToolOutput, "output");
        assert!(wrapped.starts_with("<untrusted_tool_output>"));
        assert!(wrapped.ends_with("</untrusted_tool_output>"));
    }

    #[test]
    fn all_kinds_produce_distinct_tags() {
        let kinds = [
            UntrustedKind::TaskText,
            UntrustedKind::ExtraContext,
            UntrustedKind::ToolOutput,
            UntrustedKind::HookOutput,
            UntrustedKind::RepoContent,
        ];
        let tags: Vec<_> = kinds.iter().map(|k| PromptGuard::tag(*k)).collect();
        let unique: std::collections::BTreeSet<_> = tags.iter().collect();
        assert_eq!(
            unique.len(),
            kinds.len(),
            "all kinds must have distinct tags"
        );
    }
}
