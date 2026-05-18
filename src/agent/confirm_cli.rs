//! Stderr single-line confirmation prompt for `mini --interactive`.
//!
//! Prints proposed command, step, cost, and cache marker to stderr, then
//! reads one keystroke from the TTY (`y` approve, `n` reject, `a`/Esc/Ctrl-C
//! abort). When stdin is a TTY we enter raw mode for single-keystroke
//! input. When stdin is not a TTY the constructor returns `None`; the
//! agent runner translates that into the documented
//! "interactive mode requires a TTY; pass --yolo for unattended runs"
//! error.

use std::io::{IsTerminal as _, Write as _};

use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::confirm::{ConfirmCallback, ConfirmContext, ConfirmDecision};

pub struct StderrCliConfirmer;

impl StderrCliConfirmer {
    /// Build a stderr confirmer iff stdin is a TTY. `None` otherwise.
    #[must_use]
    pub fn new_if_tty() -> Option<Self> {
        std::io::stdin().is_terminal().then_some(Self)
    }

    /// Forced constructor for callers that have already gated on a TTY
    /// elsewhere (or tests that drive a fake stdin).
    #[must_use]
    pub fn forced() -> Self {
        Self
    }
}

#[async_trait]
impl ConfirmCallback for StderrCliConfirmer {
    async fn confirm(&self, ctx: &ConfirmContext) -> ConfirmDecision {
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || prompt_blocking(&ctx))
            .await
            .unwrap_or(ConfirmDecision::Abort)
    }
}

fn prompt_blocking(ctx: &ConfirmContext) -> ConfirmDecision {
    let mut err = std::io::stderr();
    let banner = format!(
        "\n[interactive] step {}/{}  cost ${:.4}  {}\n\
         [interactive] tool: {}\n\
         [interactive] command:\n{}\n\
         [interactive] (y)approve / (n)reject / (a)abort? ",
        ctx.step,
        ctx.step_limit,
        ctx.cost_usd,
        ctx.cache_marker,
        ctx.tool_name,
        indent_command(&ctx.command),
    );
    let _ = err.write_all(banner.as_bytes());
    let _ = err.flush();

    match read_single_keystroke() {
        Some(decision) => {
            let _ = err.write_all(b"\n");
            decision
        }
        None => read_line_buffered(),
    }
}

fn indent_command(cmd: &str) -> String {
    cmd.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_single_keystroke() -> Option<ConfirmDecision> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    if crossterm::terminal::enable_raw_mode().is_err() {
        return None;
    }
    let decision = loop {
        match crossterm::event::read() {
            Ok(Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                ..
            })) => {
                if modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(code, KeyCode::Char('c' | 'C'))
                {
                    break ConfirmDecision::Abort;
                }
                match code {
                    KeyCode::Char('y' | 'Y') => break ConfirmDecision::Approve,
                    KeyCode::Char('n' | 'N') => break ConfirmDecision::Reject,
                    KeyCode::Char('a' | 'A') | KeyCode::Esc => break ConfirmDecision::Abort,
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(_) => break ConfirmDecision::Abort,
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    Some(decision)
}

fn read_line_buffered() -> ConfirmDecision {
    let mut buf = String::new();
    let Ok(_) = std::io::stdin().read_line(&mut buf) else {
        return ConfirmDecision::Abort;
    };
    parse_line_decision(&buf)
}

/// Map a line of operator input into a `ConfirmDecision`. EOF and empty
/// input map to `Abort` so a closed/piped stdin never silently approves.
pub fn parse_line_decision(input: &str) -> ConfirmDecision {
    let trimmed = input.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "y" | "yes" => ConfirmDecision::Approve,
        "n" | "no" => ConfirmDecision::Reject,
        _ => ConfirmDecision::Abort,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_command_indents_every_line() {
        assert_eq!(indent_command("a\nb\nc"), "    a\n    b\n    c");
    }

    #[test]
    fn parse_line_decision_matches_documented_keys() {
        assert_eq!(parse_line_decision("y\n"), ConfirmDecision::Approve);
        assert_eq!(parse_line_decision(" Y "), ConfirmDecision::Approve);
        assert_eq!(parse_line_decision("yes"), ConfirmDecision::Approve);
        assert_eq!(parse_line_decision("n"), ConfirmDecision::Reject);
        assert_eq!(parse_line_decision("NO"), ConfirmDecision::Reject);
        assert_eq!(parse_line_decision("a"), ConfirmDecision::Abort);
        assert_eq!(parse_line_decision("abort"), ConfirmDecision::Abort);
        assert_eq!(parse_line_decision(""), ConfirmDecision::Abort);
        assert_eq!(parse_line_decision("z"), ConfirmDecision::Abort);
    }

    #[test]
    fn forced_constructor_builds_in_non_tty() {
        let _c = StderrCliConfirmer::forced();
    }
}
