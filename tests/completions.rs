//! Integration tests for `max completions` (issue #541).
//!
//! Spawns the `max` binary and asserts that `max completions <shell>` emits
//! non-empty output for supported shells, and fails with exit code 2 (usage error)
//! on invalid shells or missing shell argument.

#![allow(clippy::unwrap_used)]

use std::process::Command;

mod support;
use support::binary_path;

fn completions() -> Command {
    let mut cmd = Command::new(binary_path());
    cmd.arg("completions");
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("OPENAI_API_KEY");
    cmd
}

#[test]
fn completions_bash_emits_non_empty_output() {
    let out = completions().arg("bash").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "bash completion script should not be empty"
    );
}

#[test]
fn completions_zsh_emits_non_empty_output() {
    let out = completions().arg("zsh").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "zsh completion script should not be empty"
    );
}

#[test]
fn completions_fish_emits_non_empty_output() {
    let out = completions().arg("fish").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "fish completion script should not be empty"
    );
}

#[test]
fn completions_powershell_emits_non_empty_output() {
    let out = completions().arg("powershell").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "powershell completion script should not be empty"
    );
}

#[test]
fn completions_elvish_emits_non_empty_output() {
    let out = completions().arg("elvish").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "elvish completion script should not be empty"
    );
}

#[test]
fn completions_no_shell_is_usage_error() {
    let out = completions().output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "running completions without a shell must exit with exit code 2 (usage error)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage_error") || stderr.contains("Usage:"),
        "expected usage error output, got:\n{stderr}"
    );
}

#[test]
fn completions_invalid_shell_is_usage_error() {
    let out = completions().arg("invalid-shell-name").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "running completions with an invalid shell must exit with exit code 2 (usage error)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage_error") || stderr.contains("invalid value"),
        "expected usage error output, got:\n{stderr}"
    );
}
