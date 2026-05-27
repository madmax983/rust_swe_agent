//! Extract tool actions or the submit sentinel from assistant-text content.
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

use crate::tool::{BASH_TOOL_NAME, ToolCall};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Bash(String),
    Tool(ToolCall),
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

#[allow(clippy::question_mark)]
fn extract_first_registered_tool_block(content: &str, tool_names: &[String]) -> Option<ToolCall> {
    let mut remaining = content;
    while let Some(start) = remaining.find("```") {
        let after_fence = &remaining[start + 3..];
        let line_end = after_fence.find('\n')?;
        let tag = after_fence[..line_end].trim();
        let body = &after_fence[line_end + 1..];
        if let Some(end) = body.find("```") {
            if tool_names.iter().any(|name| name == tag) {
                let input = body[..end].trim_end_matches('\n').to_owned();
                if !input.trim().is_empty() {
                    return Some(ToolCall {
                        name: tag.to_owned(),
                        input,
                    });
                }
            }
            remaining = &body[end + 3..];
        } else {
            return None;
        }
    }
    None
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
/// use maxwells_daemon::agent::{extract_action, Action};
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
/// use maxwells_daemon::agent::{extract_action, Action};
///
/// let response = "I am done.\nCOMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nBug fixed!\n```";
/// let action = extract_action(response);
///
/// assert_eq!(action, Action::Submit("Bug fixed!".to_string()));
/// ```
pub fn extract_action(content: &str) -> Action {
    extract_action_for_tools(content, &[BASH_TOOL_NAME.to_owned()])
}

/// Extracts an action from model text plus the raw provider response.
///
/// Some OpenAI-compatible providers return native `tool_calls` even when the
/// prompt asks for fenced tool blocks. Submit text still wins, but otherwise
/// structured tool calls are normalized into the same action path as fences.
pub fn extract_action_from_model_response(
    content: &str,
    raw: &Value,
    tool_names: &[String],
) -> Action {
    let text_action = extract_action_for_tools(content, tool_names);
    if !matches!(text_action, Action::None) {
        return text_action;
    }
    if let Some(call) = extract_first_registered_raw_tool_call(raw, tool_names) {
        if call.name == BASH_TOOL_NAME {
            return Action::Bash(call.input);
        }
        return Action::Tool(call);
    }
    text_action
}

/// Extracts the intended action using the supplied runtime tool names.
pub fn extract_action_for_tools(content: &str, tool_names: &[String]) -> Action {
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

    // 2) Otherwise, any registered tool fence is the action.
    if let Some(call) = extract_first_registered_tool_block(content, tool_names) {
        if call.name == BASH_TOOL_NAME {
            return Action::Bash(call.input);
        }
        return Action::Tool(call);
    }

    // 3) Preserve the historical bash parser's lenient prefix handling.
    if let Some(cmd) = extract_first_bash_block(content) {
        if tool_names.iter().any(|name| name == BASH_TOOL_NAME) && !cmd.trim().is_empty() {
            return Action::Bash(cmd);
        }
    }

    Action::None
}

fn extract_first_registered_raw_tool_call(raw: &Value, tool_names: &[String]) -> Option<ToolCall> {
    for calls in raw_tool_call_arrays(raw) {
        for call in calls {
            if let Some(tool_call) = raw_tool_call_to_tool_call(call, tool_names) {
                return Some(tool_call);
            }
        }
    }
    None
}

fn raw_tool_call_arrays(raw: &Value) -> Vec<&[Value]> {
    let mut arrays = Vec::new();
    if let Some(choices) = raw.get("choices").and_then(Value::as_array) {
        if let Some(calls) = choices
            .first()
            .and_then(|choice| choice.pointer("/message/tool_calls"))
            .and_then(Value::as_array)
        {
            arrays.push(calls.as_slice());
        }
        return arrays;
    }
    if let Some(calls) = raw.get("tool_calls").and_then(Value::as_array) {
        arrays.push(calls.as_slice());
    }
    if let Some(calls) = raw.pointer("/message/tool_calls").and_then(Value::as_array) {
        arrays.push(calls.as_slice());
    }
    arrays
}

fn raw_tool_call_to_tool_call(call: &Value, tool_names: &[String]) -> Option<ToolCall> {
    let name = call
        .pointer("/function/name")
        .or_else(|| call.get("name"))
        .and_then(Value::as_str)?;
    if !tool_names.iter().any(|tool_name| tool_name == name) {
        return None;
    }
    let arguments = call
        .pointer("/function/arguments")
        .or_else(|| call.get("arguments"))?;
    let input = raw_tool_arguments_to_input(name, arguments)?;
    if input.trim().is_empty() {
        return None;
    }
    Some(ToolCall {
        name: name.to_owned(),
        input,
    })
}

fn raw_tool_arguments_to_input(tool_name: &str, arguments: &Value) -> Option<String> {
    match arguments {
        Value::String(s) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                raw_tool_arguments_to_input(tool_name, &parsed)
            } else {
                Some(s.clone())
            }
        }
        Value::Object(map) if tool_name == BASH_TOOL_NAME => map
            .get("command")
            .or_else(|| map.get("cmd"))
            .or_else(|| map.get("input"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        Value::Object(map) => map
            .get("input")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| serde_json::to_string(arguments).ok()),
        Value::Null => None,
        _ => serde_json::to_string(arguments).ok(),
    }
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

    #[test]
    fn extracts_registered_runtime_tool() {
        let s = "Need a diagnostic.\n```diagnose\ncheck flaky test\n```";
        assert_eq!(
            extract_action_for_tools(s, &["bash".into(), "diagnose".into()]),
            Action::Tool(ToolCall {
                name: "diagnose".into(),
                input: "check flaky test".into(),
            })
        );
    }

    #[test]
    fn ignores_unregistered_tool_fence() {
        let s = "```diagnose\ncheck flaky test\n```";
        assert_eq!(extract_action_for_tools(s, &["bash".into()]), Action::None);
    }

    #[test]
    fn extracts_openai_style_bash_tool_call_from_raw_response() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Let me inspect the repository.",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"pwd && ls -la\"}"
                        }
                    }]
                }
            }]
        });

        assert_eq!(
            extract_action_from_model_response(
                "Let me inspect the repository.",
                &raw,
                &["bash".into()]
            ),
            Action::Bash("pwd && ls -la".into())
        );
    }

    #[test]
    fn fenced_bash_wins_over_conflicting_raw_tool_call() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"echo raw\"}"
                        }
                    }]
                }
            }]
        });
        let content = "```bash\necho fenced\n```";

        assert_eq!(
            extract_action_from_model_response(content, &raw, &["bash".into()]),
            Action::Bash("echo fenced".into())
        );
    }

    #[test]
    fn raw_bash_tool_call_accepts_generic_input_argument() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {
                            "name": "bash",
                            "arguments": "{\"input\":\"echo from-input\"}"
                        }
                    }]
                }
            }]
        });

        assert_eq!(
            extract_action_from_model_response("Let me run that.", &raw, &["bash".into()]),
            Action::Bash("echo from-input".into())
        );
    }

    #[test]
    fn raw_null_tool_call_arguments_are_ignored() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {
                            "name": "bash",
                            "arguments": null
                        }
                    }]
                }
            }]
        });

        assert_eq!(
            extract_action_from_model_response("Let me run that.", &raw, &["bash".into()]),
            Action::None
        );
    }

    #[test]
    fn raw_tool_calls_from_unselected_choices_are_ignored() {
        let raw = serde_json::json!({
            "choices": [
                {
                    "message": {
                        "content": "No tool call here."
                    }
                },
                {
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "function": {
                                "name": "bash",
                                "arguments": "{\"command\":\"echo unselected\"}"
                            }
                        }]
                    }
                }
            ]
        });

        assert_eq!(
            extract_action_from_model_response("No tool call here.", &raw, &["bash".into()]),
            Action::None
        );
    }

    #[test]
    fn submit_text_wins_over_raw_tool_call() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"echo should-not-run\"}"
                        }
                    }]
                }
            }]
        });
        let content = "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfinal\n```";

        assert_eq!(
            extract_action_from_model_response(content, &raw, &["bash".into()]),
            Action::Submit("final".into())
        );
    }
}
