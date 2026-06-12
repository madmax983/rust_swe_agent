//! Clipboard delivery abstraction for the interactive ratatui dashboard.
//!
//! The default delivery method is the **OSC 52** terminal escape sequence,
//! which works over SSH and inside tmux (`set-clipboard on` in tmux ≥ 3.3).
//! `ClipboardSink` is the swap point for tests and for a future
//! native-clipboard back-end.
//!
//! Issue #651.

use std::io::Write as _;

/// Delivers text to the system clipboard.
///
/// The caller shows a non-fatal notice on `Err`; the run must continue.
pub trait ClipboardSink: Send + Sync {
    fn copy(&self, text: &str) -> std::io::Result<()>;
}

/// Builds the OSC 52 escape sequence for `text`.
///
/// Format: `ESC ] 52 ; c ; <base64(text)> BEL`
pub fn osc52_sequence(text: &str) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07")
}

/// `ClipboardSink` that emits an OSC 52 escape to stdout.
///
/// Works transparently over SSH. For tmux, `set-clipboard on` is required
/// (tmux ≥ 3.3).
pub struct Osc52Clipboard;

impl ClipboardSink for Osc52Clipboard {
    fn copy(&self, text: &str) -> std::io::Result<()> {
        let seq = osc52_sequence(text);
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        out.write_all(seq.as_bytes())?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn osc52_sequence_wraps_base64_payload() {
        // Fails in red phase — osc52_sequence returns ""
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn osc52_sequence_roundtrips_multibyte() {
        use base64::Engine as _;
        let input = "é 🦀 naïve";
        let seq = osc52_sequence(input);
        assert!(
            seq.starts_with("\x1b]52;c;"),
            "expected OSC 52 prefix; got: {seq:?}"
        );
        assert!(
            seq.ends_with('\x07'),
            "expected BEL terminator; got: {seq:?}"
        );
        let payload = seq
            .strip_prefix("\x1b]52;c;")
            .expect("checked above")
            .strip_suffix('\x07')
            .expect("checked above");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("valid base64");
        assert_eq!(decoded, input.as_bytes());
    }
}
