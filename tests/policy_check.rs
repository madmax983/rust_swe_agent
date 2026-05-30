//! Tests for `agent policy-check` (issue #335).
//! RED phase: written before implementation — will fail to compile until
//! `src/run/policy_check.rs` is created.
#![allow(clippy::unwrap_used)]

use maxwells_daemon::config::Config;
use maxwells_daemon::run::policy_check::{
    ExpectAssertion, PolicyCheckOpts, PolicyCheckSource, VerdictKind, format_json, format_text,
    run_policy_check,
};

fn check_one(cfg: &Config, cmd: &str) -> maxwells_daemon::run::policy_check::CommandVerdict {
    let output = run_policy_check(
        cfg,
        &PolicyCheckOpts {
            source: PolicyCheckSource::Commands(vec![cmd.to_owned()]),
            expect: vec![],
        },
    )
    .unwrap();
    assert_eq!(output.verdicts.len(), 1, "expected exactly one verdict");
    output.verdicts.into_iter().next().unwrap()
}

// ── AC: deny-corpus matches deny verdict ──────────────────────────────────────

#[test]
fn builtin_deny_corpus_returns_deny() {
    let cfg = Config::defaults().unwrap();
    let v = check_one(&cfg, "rm -rf /");
    assert_eq!(v.verdict, VerdictKind::Deny, "rm -rf / must be denied by built-in corpus");
    assert!(!v.matching_rule.is_empty(), "matching_rule must be non-empty for a deny");
}

// ── AC: operator allow-rule overrides built-in deny ───────────────────────────

#[test]
fn allow_rule_overrides_builtin_deny() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.policy.extra_allow_patterns = vec![r"rm\s.*/$".to_owned()];
    let v = check_one(&cfg, "rm -rf /");
    assert_eq!(
        v.verdict,
        VerdictKind::Allow,
        "extra_allow_patterns must override built-in deny"
    );
    assert!(
        v.matching_rule.starts_with("cfg-allow-"),
        "allow override must report a cfg-allow-N label, got: {}",
        v.matching_rule
    );
}

// ── AC: operator deny-rule layered on `safe` ──────────────────────────────────

#[test]
fn extra_deny_rule_denies_normally_allowed_command() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.policy.extra_deny_patterns = vec![r"curl\b".to_owned()];
    let v = check_one(&cfg, "curl https://example.com");
    assert_eq!(
        v.verdict,
        VerdictKind::Deny,
        "extra_deny_patterns must deny otherwise-allowed command"
    );
    assert!(
        v.matching_rule.starts_with("cfg-deny-"),
        "extra deny must report a cfg-deny-N label, got: {}",
        v.matching_rule
    );
}

// ── AC: `ask` profile resolves to `deny` in non-interactive ──────────────────

#[test]
fn ask_profile_resolves_to_deny_non_interactive() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.policy.profile = "ask".to_owned();
    let v = check_one(&cfg, "ls");
    assert_eq!(
        v.verdict,
        VerdictKind::Deny,
        "ask profile must resolve to deny in non-interactive context"
    );
    assert_eq!(
        v.matching_rule, "ask-non-interactive",
        "ask resolved to deny must carry 'ask-non-interactive' label"
    );
    assert_eq!(v.profile, "ask", "profile must be reported as 'ask'");
}

// ── AC: `yolo` allows everything with a `yolo_bypass` marker ─────────────────

#[test]
fn yolo_profile_allows_everything() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.policy.profile = "yolo".to_owned();
    let v = check_one(&cfg, "rm -rf /");
    assert_eq!(v.verdict, VerdictKind::Allow, "yolo profile must allow all commands");
    assert_eq!(v.matching_rule, "yolo-bypass", "yolo must carry 'yolo-bypass' label");
}

// ── AC: `--expect` regression assertion fails closed ─────────────────────────

#[test]
fn expect_mismatch_is_reported() {
    let cfg = Config::defaults().unwrap();
    let output = run_policy_check(
        &cfg,
        &PolicyCheckOpts {
            source: PolicyCheckSource::Commands(vec!["ls".to_owned()]),
            expect: vec![ExpectAssertion {
                command: "ls".to_owned(),
                expected: VerdictKind::Deny, // wrong: ls → allow under safe
            }],
        },
    )
    .unwrap();
    assert!(
        !output.mismatches.is_empty(),
        "--expect mismatch for 'ls' must be recorded"
    );
    assert_eq!(output.mismatches[0].command, "ls");
    assert_eq!(output.mismatches[0].expected, VerdictKind::Deny);
    assert_eq!(output.mismatches[0].actual, VerdictKind::Allow);
}

#[test]
fn expect_correct_verdict_has_no_mismatch() {
    let cfg = Config::defaults().unwrap();
    let output = run_policy_check(
        &cfg,
        &PolicyCheckOpts {
            source: PolicyCheckSource::Commands(vec!["ls".to_owned()]),
            expect: vec![ExpectAssertion {
                command: "ls".to_owned(),
                expected: VerdictKind::Allow,
            }],
        },
    )
    .unwrap();
    assert!(output.mismatches.is_empty(), "correct --expect must not produce mismatches");
}

#[test]
fn expect_deny_on_actually_denied_command() {
    let cfg = Config::defaults().unwrap();
    let output = run_policy_check(
        &cfg,
        &PolicyCheckOpts {
            source: PolicyCheckSource::Commands(vec!["rm -rf /".to_owned()]),
            expect: vec![ExpectAssertion {
                command: "rm -rf /".to_owned(),
                expected: VerdictKind::Deny,
            }],
        },
    )
    .unwrap();
    assert!(output.mismatches.is_empty(), "expect deny on a denied command must not mismatch");
}

// ── AC: blank lines and # comments in corpus file are ignored ─────────────────

#[test]
fn corpus_file_ignores_blank_and_comments() {
    use std::io::Write as _;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, "# this is a comment").unwrap();
    writeln!(tmp, "").unwrap();
    writeln!(tmp, "  ").unwrap(); // whitespace-only
    writeln!(tmp, "ls").unwrap();
    writeln!(tmp, "# another comment").unwrap();
    let path = tmp.path().to_owned();
    let cfg = Config::defaults().unwrap();
    let output = run_policy_check(
        &cfg,
        &PolicyCheckOpts {
            source: PolicyCheckSource::CommandsFile(path),
            expect: vec![],
        },
    )
    .unwrap();
    assert_eq!(output.verdicts.len(), 1, "only 'ls' should survive filtering");
    assert_eq!(output.verdicts[0].command, "ls");
}

// ── AC: JSON format produces schema-versioned output ─────────────────────────

#[test]
fn json_format_includes_schema_version_and_artifact_kind() {
    let cfg = Config::defaults().unwrap();
    let output = run_policy_check(
        &cfg,
        &PolicyCheckOpts {
            source: PolicyCheckSource::Commands(vec!["ls".to_owned()]),
            expect: vec![],
        },
    )
    .unwrap();
    let json_val = format_json(&output).unwrap();
    assert_eq!(
        json_val.get("artifact_kind").and_then(|v| v.as_str()),
        Some("policy_check"),
        "JSON must have artifact_kind = 'policy_check'"
    );
    assert!(json_val.get("schema_version").is_some(), "JSON must have schema_version");
    assert!(json_val.get("verdicts").is_some(), "JSON must have verdicts array");
    assert!(json_val.get("profile").is_some(), "JSON must have profile");
}

// ── AC: text format includes one row per command ──────────────────────────────

#[test]
fn text_format_includes_all_commands_and_verdicts() {
    let cfg = Config::defaults().unwrap();
    let output = run_policy_check(
        &cfg,
        &PolicyCheckOpts {
            source: PolicyCheckSource::Commands(vec![
                "ls".to_owned(),
                "rm -rf /".to_owned(),
            ]),
            expect: vec![],
        },
    )
    .unwrap();
    assert_eq!(output.verdicts.len(), 2);
    let text = format_text(&output);
    assert!(text.contains("ls"), "text output must include 'ls'");
    assert!(text.contains("allow"), "text output must include 'allow' verdict");
    assert!(text.contains("deny"), "text output must include 'deny' verdict");
}

// ── AC: effective profile is reported on each verdict ────────────────────────

#[test]
fn verdict_reports_effective_profile() {
    let mut cfg = Config::defaults().unwrap();
    cfg.root.policy.profile = "ask".to_owned();
    let v = check_one(&cfg, "ls");
    assert_eq!(v.profile, "ask", "each verdict must carry the effective profile");
}

// ── AC: Zero network/model calls — no API key needed ─────────────────────────

#[test]
fn no_api_key_required() {
    let cfg = Config::defaults().unwrap();
    let result = run_policy_check(
        &cfg,
        &PolicyCheckOpts {
            source: PolicyCheckSource::Commands(vec!["echo hello".to_owned()]),
            expect: vec![],
        },
    );
    assert!(result.is_ok(), "policy-check must succeed without any API key");
}

// ── AC: safe profile allows ordinary commands by default ─────────────────────

#[test]
fn safe_profile_allows_ordinary_commands() {
    let cfg = Config::defaults().unwrap();
    let v = check_one(&cfg, "ls -la");
    assert_eq!(v.verdict, VerdictKind::Allow, "ls -la must be allowed under safe profile");
    assert_eq!(v.matching_rule, "default-allow");
    assert_eq!(v.profile, "safe");
}
