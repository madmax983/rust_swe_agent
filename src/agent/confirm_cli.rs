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
    let _ = err.write_all(render_banner(ctx).as_bytes());
    let _ = err.flush();

    let mut decision = match read_single_keystroke() {
        Some(decision) => {
            let _ = err.write_all(b"\n");
            decision
        }
        None => read_line_buffered(),
    };

    if let ConfirmDecision::Reject(_) = decision {
        if std::io::stdin().is_terminal() {
            let _ = err.write_all(b"Provide corrective feedback (optional): ");
            let _ = err.flush();
            let mut feedback = String::new();
            if std::io::stdin().read_line(&mut feedback).is_ok() {
                let trimmed = feedback.trim();
                if !trimmed.is_empty() {
                    decision = ConfirmDecision::Reject(Some(trimmed.to_owned()));
                }
            }
        }
    }
    decision
}

/// Pure renderer for the stderr prompt banner, factored out so tests can
/// assert on the bytes without going through stderr.
fn render_banner(ctx: &ConfirmContext) -> String {
    format!(
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
    )
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
            Ok(Event::Key(key)) => {
                if let Some(d) = key_event_to_decision(key) {
                    break d;
                }
            }
            Ok(_) => {}
            Err(_) => break ConfirmDecision::Abort,
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    Some(decision)
}

/// Map a crossterm key event to a `ConfirmDecision`. Returns `None` for
/// non-press events or keys outside the documented y/n/a set so the
/// caller can keep polling. Pulled out so tests can exercise the
/// keystroke mapping without raw mode.
fn key_event_to_decision(key: KeyEvent) -> Option<ConfirmDecision> {
    if !matches!(key.kind, KeyEventKind::Press) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        return Some(ConfirmDecision::Abort);
    }
    match key.code {
        KeyCode::Char('y' | 'Y') => Some(ConfirmDecision::Approve),
        KeyCode::Char('n' | 'N') => Some(ConfirmDecision::Reject(None)),
        KeyCode::Char('a' | 'A') | KeyCode::Esc => Some(ConfirmDecision::Abort),
        _ => None,
    }
}

#[cfg(test)]
pub static MOCK_STDIN_EOF: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn read_line_buffered() -> ConfirmDecision {
    #[cfg(test)]
    {
        if MOCK_STDIN_EOF.load(std::sync::atomic::Ordering::Relaxed) {
            return ConfirmDecision::Abort;
        }
    }
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
        "n" | "no" => ConfirmDecision::Reject(None),
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
        assert_eq!(parse_line_decision("n"), ConfirmDecision::Reject(None));
        assert_eq!(parse_line_decision("NO"), ConfirmDecision::Reject(None));
        assert_eq!(parse_line_decision("a"), ConfirmDecision::Abort);
        assert_eq!(parse_line_decision("abort"), ConfirmDecision::Abort);
        assert_eq!(parse_line_decision(""), ConfirmDecision::Abort);
        assert_eq!(parse_line_decision("z"), ConfirmDecision::Abort);
    }

    #[test]
    fn forced_constructor_builds_in_non_tty() {
        let _c = StderrCliConfirmer::forced();
    }

    fn ctx_for(cmd: &str) -> ConfirmContext {
        ConfirmContext {
            tool_name: "bash".into(),
            command: cmd.into(),
            step: 2,
            step_limit: 7,
            cost_usd: 0.0123,
            cache_marker: "cache:explicit",
        }
    }

    #[test]
    fn render_banner_includes_step_cost_command_and_keys() {
        let banner = render_banner(&ctx_for("echo hi"));
        assert!(banner.contains("step 2/7"));
        assert!(banner.contains("cost $0.0123"));
        assert!(banner.contains("cache:explicit"));
        assert!(banner.contains("tool: bash"));
        assert!(banner.contains("    echo hi"));
        assert!(banner.contains("(y)approve / (n)reject / (a)abort?"));
    }

    #[test]
    fn render_banner_indents_multiline_commands() {
        let banner = render_banner(&ctx_for("ls -la\necho done"));
        assert!(banner.contains("    ls -la\n    echo done"));
    }

    #[test]
    fn key_event_to_decision_press_keys_map_correctly() {
        for (code, modifiers, expected) in [
            (
                KeyCode::Char('y'),
                KeyModifiers::NONE,
                Some(ConfirmDecision::Approve),
            ),
            (
                KeyCode::Char('Y'),
                KeyModifiers::NONE,
                Some(ConfirmDecision::Approve),
            ),
            (
                KeyCode::Char('n'),
                KeyModifiers::NONE,
                Some(ConfirmDecision::Reject(None)),
            ),
            (
                KeyCode::Char('N'),
                KeyModifiers::NONE,
                Some(ConfirmDecision::Reject(None)),
            ),
            (
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                Some(ConfirmDecision::Abort),
            ),
            (
                KeyCode::Esc,
                KeyModifiers::NONE,
                Some(ConfirmDecision::Abort),
            ),
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                Some(ConfirmDecision::Abort),
            ),
            (
                KeyCode::Char('C'),
                KeyModifiers::CONTROL,
                Some(ConfirmDecision::Abort),
            ),
            // Junk keys keep the prompt alive.
            (KeyCode::Char('z'), KeyModifiers::NONE, None),
            (KeyCode::Enter, KeyModifiers::NONE, None),
            // Plain 'c' (no Ctrl) is junk too — only Ctrl-C is abort.
            (KeyCode::Char('c'), KeyModifiers::NONE, None),
        ] {
            let key = KeyEvent::new(code, modifiers);
            assert_eq!(key_event_to_decision(key), expected, "code={code:?}");
        }
    }

    #[test]
    fn key_event_to_decision_ignores_release_events() {
        let mut key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        key.kind = KeyEventKind::Release;
        assert_eq!(key_event_to_decision(key), None);
    }

    #[test]
    fn read_single_keystroke_returns_none_when_stdin_is_not_a_tty() {
        // `cargo test` redirects stdin away from the TTY, so this is the
        // production non-interactive path and should bail out cleanly
        // rather than blocking on raw-mode reads.
        assert!(read_single_keystroke().is_none());
    }

    #[test]
    fn new_if_tty_returns_none_under_cargo_test() {
        // Under `cargo test` stdin is not a TTY; `new_if_tty` must refuse
        // to construct the confirmer.
        assert!(StderrCliConfirmer::new_if_tty().is_none());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn confirm_under_non_tty_stdin_aborts_on_eof() {
        // `prompt_blocking` writes the banner to stderr, finds no TTY,
        // falls through to `read_line_buffered`. With cargo-test's
        // closed/redirected stdin, the read returns EOF, which
        // `parse_line_decision` maps to Abort. End-to-end: confirm()
        // returns Abort without hanging.
        MOCK_STDIN_EOF.store(true, std::sync::atomic::Ordering::Relaxed);
        let c = StderrCliConfirmer::forced();
        let ctx = ConfirmContext {
            tool_name: "bash".into(),
            command: "echo x".into(),
            step: 0,
            step_limit: 1,
            cost_usd: 0.0,
            cache_marker: "cache:auto-or-none",
        };
        let d = c.confirm(&ctx).await;
        MOCK_STDIN_EOF.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(d, ConfirmDecision::Abort);
    }
}
