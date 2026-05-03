//! Extract bash actions or the submit sentinel from assistant-text content.
//!
//! The grammar (matching mini-swe-agent's Python):
//!
//! * Exactly one ``` ```bash … ``` `` fenced code block — runnable action.
//! * The sentinel `COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT` on its own line,
//!   typically followed by another fenced block whose contents are the
//!   final output to submit. The fence language tag is ignored for the
//!   final block (`text`, nothing, etc. all accepted).
//! * Anything else: `Action::None`, so the agent loop emits a
//!   format_error_template message.

pub const SUBMIT_SENTINEL: &str = "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Bash(String),
    Submit(String),
    None,
}

/// Extract the first bash code-fence content.
fn extract_first_bash_block(s: &str) -> Option<String> {
    let mut remaining = s;
    while let Some(start) = remaining.find("```bash") {
        let after_tag = &remaining[start + "```bash".len()..];
        // Skip to end of line (consume the `\n` or ` ` + `\n`).
        let body_start = after_tag.find('\n').map_or(0, |i| i + 1);
        let body = &after_tag[body_start..];
        if let Some(end) = body.find("```") {
            return Some(body[..end].trim_end_matches('\n').to_owned());
        }
        remaining = after_tag;
    }
    None
}

/// Extract the first fenced code block (any or no language tag). Returns
/// the inner body with trailing newline stripped.
fn extract_first_any_block(s: &str) -> Option<String> {
    let start = s.find("```")?;
    let after_fence = &s[start + 3..];
    let body_start = after_fence.find('\n').map_or(0, |i| i + 1);
    let body = &after_fence[body_start..];
    let end = body.find("```")?;
    Some(body[..end].trim_end_matches('\n').to_owned())
}

/// Extracts the intended `Action` (bash command, submit, or none) from a model's response string.
///
/// This function acts as the critical bridge between the LLM's unstructured text and the agent's
/// structured environment. It scans the provided `content` for either a bash code block or the
/// explicit submission sentinel.
///
/// If a response contains both a bash command and the submit sentinel (on its own line),
/// the submit sentinel always takes precedence.
///
/// ## Examples
///
/// Extracting a basic shell command:
///
/// ```
/// use rust_swe_agent::agent::{extract_action, Action};
///
/// let response = "I will check the directory contents:\n```bash\nls -la\n```";
/// let action = extract_action(response);
///
/// assert_eq!(action, Action::Bash("ls -la".to_string()));
/// ```
///
/// Handling a task submission:
///
/// ```
/// use rust_swe_agent::agent::{extract_action, Action};
///
/// let response = "I am done.\nCOMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nBug fixed!\n```";
/// let action = extract_action(response);
///
/// assert_eq!(action, Action::Submit("Bug fixed!".to_string()));
/// ```
pub fn extract_action(content: &str) -> Action {
    // 1) Submit wins if the sentinel appears on its own line.
    let sentinel_on_own_line = content.lines().any(|l| l.trim() == SUBMIT_SENTINEL);

    if sentinel_on_own_line {
        // After the sentinel line, look for a fenced block.
        let idx = content
            .find(SUBMIT_SENTINEL)
            .map_or(content.len(), |i| i + SUBMIT_SENTINEL.len());
        let tail = &content[idx..];
        let final_output = extract_first_any_block(tail).unwrap_or_default();
        return Action::Submit(final_output);
    }

    // 2) Otherwise, a bash block is the action.
    if let Some(cmd) = extract_first_bash_block(content) {
        if !cmd.trim().is_empty() {
            return Action::Bash(cmd);
        }
    }

    Action::None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn extracts_simple_bash() {
        let s = "Here's the command:\n```bash\necho hi\n```\nDone.";
        assert_eq!(extract_action(s), Action::Bash("echo hi".into()));
    }

    #[test]
    fn extracts_multiline_bash() {
        let s = "```bash\nls -la\ncat /tmp/x\n```";
        assert_eq!(extract_action(s), Action::Bash("ls -la\ncat /tmp/x".into()));
    }

    #[test]
    fn submit_sentinel_matches_final_output() {
        let s = "I'm done.\nCOMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal answer\n```";
        assert_eq!(extract_action(s), Action::Submit("final answer".into()));
    }

    #[test]
    fn submit_sentinel_without_block_submits_empty() {
        let s = "done\nCOMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n";
        assert_eq!(extract_action(s), Action::Submit(String::new()));
    }

    #[test]
    fn submit_wins_over_bash() {
        let s =
            "```bash\necho still here\n```\nCOMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```";
        assert_eq!(extract_action(s), Action::Submit("ok".into()));
    }

    #[test]
    fn no_fenced_block_is_none() {
        let s = "I think we should do stuff.";
        assert_eq!(extract_action(s), Action::None);
    }

    #[test]
    fn empty_bash_block_is_none() {
        let s = "```bash\n\n```";
        assert_eq!(extract_action(s), Action::None);
    }

    #[test]
    fn sentinel_in_prose_does_not_trigger() {
        // Sentinel not on its own line — should not match.
        let s =
            "The sentinel is COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT in prose.\n```bash\necho x\n```";
        assert_eq!(extract_action(s), Action::Bash("echo x".into()));
    }
}
