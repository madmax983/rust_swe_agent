//! Issue #312 — `--interactive`, `--yolo`, `--ui` flag parsing on `mini`.

#![allow(clippy::unwrap_used)]

use clap::Parser;
use maxwells_daemon::cli::{Cli, Command, args::UiKind};

fn parse(argv: &[&str]) -> Cli {
    Cli::try_parse_from(argv).unwrap()
}

#[test]
fn interactive_flag_defaults_false() {
    let cli = parse(&["max", "mini", "--task", "t"]);
    let Command::Mini(m) = cli.command else {
        panic!("expected mini");
    };
    let m = *m;
    assert!(!m.interactive);
    assert!(!m.yolo);
    assert!(matches!(m.ui, UiKind::Stderr));
}

#[test]
fn interactive_flag_parses() {
    let cli = parse(&["max", "mini", "--task", "t", "--interactive"]);
    let Command::Mini(m) = cli.command else {
        panic!("expected mini");
    };
    let m = *m;
    assert!(m.interactive);
}

#[test]
fn yolo_flag_parses() {
    let cli = parse(&["max", "mini", "--task", "t", "--yolo"]);
    let Command::Mini(m) = cli.command else {
        panic!("expected mini");
    };
    let m = *m;
    assert!(m.yolo);
}

#[test]
fn interactive_and_yolo_can_both_be_set() {
    let cli = parse(&["max", "mini", "--task", "t", "--interactive", "--yolo"]);
    let Command::Mini(m) = cli.command else {
        panic!("expected mini");
    };
    let m = *m;
    assert!(m.interactive);
    assert!(m.yolo);
}

#[test]
fn ui_ratatui_parses() {
    let cli = parse(&[
        "max",
        "mini",
        "--task",
        "t",
        "--interactive",
        "--ui",
        "ratatui",
    ]);
    let Command::Mini(m) = cli.command else {
        panic!("expected mini");
    };
    let m = *m;
    assert!(matches!(m.ui, UiKind::Ratatui));
}

#[test]
fn ui_stderr_parses() {
    let cli = parse(&[
        "max",
        "mini",
        "--task",
        "t",
        "--interactive",
        "--ui",
        "stderr",
    ]);
    let Command::Mini(m) = cli.command else {
        panic!("expected mini");
    };
    let m = *m;
    assert!(matches!(m.ui, UiKind::Stderr));
}

#[test]
fn unknown_ui_value_is_rejected() {
    let res = Cli::try_parse_from([
        "max",
        "mini",
        "--task",
        "t",
        "--interactive",
        "--ui",
        "bogus",
    ]);
    assert!(res.is_err());
}

#[test]
fn interactive_and_render_only_are_mutually_exclusive() {
    let res = Cli::try_parse_from([
        "max",
        "mini",
        "--task",
        "t",
        "--interactive",
        "--render-only",
    ]);
    assert!(res.is_err());
}
