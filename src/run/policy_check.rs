//! `agent policy-check` — zero-cost preflight for the command policy config.
//!
//! Issue #335. Feeds a corpus of bash commands through the resolved policy
//! config and reports the per-command verdict (allow / ask / deny) plus the
//! matching rule label. No model call is made and no environment is launched.

use std::io::Read as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactSchemaVersion;
use crate::config::Config;
use crate::error::{ConfigError, Error};
use crate::policy::{PolicyEngine, PolicyEvaluation};

/// Schema version for the `policy-check` JSON artifact.
const POLICY_CHECK_SCHEMA_VERSION: ArtifactSchemaVersion = ArtifactSchemaVersion::new(1, 0);

// ── Input source ──────────────────────────────────────────────────────────────

/// Where the command corpus comes from.
pub enum PolicyCheckSource {
    /// Read one command per line from a file (blank lines and `#` comments ignored).
    CommandsFile(PathBuf),
    /// Read from stdin (no `--commands-file` or `--command` flag given).
    Stdin,
    /// Ad-hoc commands passed directly via `--command` (repeatable).
    Commands(Vec<String>),
}

// ── Verdict kind ──────────────────────────────────────────────────────────────

/// The three possible outcomes from non-interactive policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictKind {
    Allow,
    Ask,
    Deny,
}

impl VerdictKind {
    /// Lowercase string representation for display and JSON.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    fn from_evaluation(eval: &PolicyEvaluation) -> Self {
        use crate::policy::PolicyDecision;
        match &eval.decision {
            PolicyDecision::Allow => Self::Allow,
            PolicyDecision::Ask => Self::Ask,
            PolicyDecision::Deny { .. } => Self::Deny,
        }
    }

    /// Parse from a lowercase string (as produced by `as_str`).
    ///
    /// # Errors
    /// Returns `None` if the string is not one of `allow`, `ask`, `deny`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

// ── Per-command verdict ───────────────────────────────────────────────────────

/// The evaluation result for a single command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandVerdict {
    /// The original command string.
    pub command: String,
    /// The non-interactive verdict.
    pub verdict: VerdictKind,
    /// Label of the rule that produced this verdict (or a sentinel; see
    /// [`crate::policy::PolicyEvaluation`] docs for the full list).
    pub matching_rule: String,
    /// The effective profile (`"safe"`, `"ask"`, or `"yolo"`).
    pub profile: String,
}

// ── Expect assertion / mismatch ───────────────────────────────────────────────

/// A CI regression assertion: the operator expects `command` to receive
/// `expected` verdict.
pub struct ExpectAssertion {
    pub command: String,
    pub expected: VerdictKind,
}

/// A recorded mismatch between an expected and actual verdict.
#[derive(Debug, Clone)]
pub struct ExpectMismatch {
    pub command: String,
    pub expected: VerdictKind,
    /// The actual verdict, or `None` when the command was not found in the corpus.
    pub actual: Option<VerdictKind>,
}

// ── Options ───────────────────────────────────────────────────────────────────

/// Options passed to [`run_policy_check`].
pub struct PolicyCheckOpts {
    /// Where to read commands from.
    pub source: PolicyCheckSource,
    /// Optional `--expect` assertions for CI regression testing.
    pub expect: Vec<ExpectAssertion>,
}

// ── Output ────────────────────────────────────────────────────────────────────

/// The output of a `policy-check` run.
pub struct PolicyCheckOutput {
    /// Per-command verdicts (one per non-blank, non-comment input line).
    pub verdicts: Vec<CommandVerdict>,
    /// Assertions that did not match; empty when all `--expect` clauses passed.
    pub mismatches: Vec<ExpectMismatch>,
    /// The effective profile string (from `cfg.root.policy.profile`).
    pub profile: String,
}

impl PolicyCheckOutput {
    /// Returns true when at least one `--expect` assertion failed.
    #[must_use]
    pub fn has_mismatches(&self) -> bool {
        !self.mismatches.is_empty()
    }
}

// ── Core function ─────────────────────────────────────────────────────────────

/// Run the policy config against the command corpus described by `opts`.
///
/// Resolves the [`PolicyEngine`] from `cfg.root.policy` (same precedence as a
/// real agent run). No model call, no environment, no side effects.
pub fn run_policy_check(cfg: &Config, opts: &PolicyCheckOpts) -> Result<PolicyCheckOutput, Error> {
    let engine = PolicyEngine::from_cfg(&cfg.root.policy)
        .map_err(|e| Error::Config(ConfigError::Invalid(format!("invalid policy config: {e}"))))?;

    let profile = engine.profile().as_str().to_owned();
    let commands = read_source(&opts.source)?;

    let verdicts: Vec<CommandVerdict> = commands
        .iter()
        .map(|cmd| {
            let eval = engine.evaluate_non_interactive(cmd);
            let verdict = VerdictKind::from_evaluation(&eval);
            CommandVerdict {
                command: cmd.clone(),
                verdict,
                matching_rule: eval.matching_rule,
                profile: profile.clone(),
            }
        })
        .collect();

    // Evaluate `--expect` assertions.
    let mismatches: Vec<ExpectMismatch> = opts
        .expect
        .iter()
        .filter_map(|assertion| {
            let actual = verdicts
                .iter()
                .find(|v| v.command == assertion.command)
                .map(|v| v.verdict.clone());
            match actual {
                Some(ref act) if act == &assertion.expected => None,
                Some(act) => Some(ExpectMismatch {
                    command: assertion.command.clone(),
                    expected: assertion.expected.clone(),
                    actual: Some(act),
                }),
                None => {
                    // Command in `--expect` was not found in the corpus.
                    Some(ExpectMismatch {
                        command: assertion.command.clone(),
                        expected: assertion.expected.clone(),
                        actual: None,
                    })
                }
            }
        })
        .collect();

    Ok(PolicyCheckOutput {
        verdicts,
        mismatches,
        profile,
    })
}

fn read_source(source: &PolicyCheckSource) -> Result<Vec<String>, Error> {
    match source {
        PolicyCheckSource::Commands(cmds) => Ok(cmds.clone()),
        PolicyCheckSource::CommandsFile(path) => {
            let raw = std::fs::read_to_string(path).map_err(|e| {
                Error::Config(ConfigError::Usage(format!(
                    "cannot read commands file '{}': {e}",
                    path.display()
                )))
            })?;
            Ok(parse_corpus(&raw))
        }
        PolicyCheckSource::Stdin => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(Error::Io)?;
            Ok(parse_corpus(&buf))
        }
    }
}

/// Parse a multi-line corpus string: blank lines and `#`-prefixed lines are
/// ignored; each remaining line is a command.
fn parse_corpus(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

// ── Formatters ────────────────────────────────────────────────────────────────

/// Format the policy-check output as a human-readable table (one row per command).
#[must_use]
pub fn format_text(output: &PolicyCheckOutput) -> String {
    use std::fmt::Write as _;

    if output.verdicts.is_empty() {
        let mut out = "policy-check: no commands to evaluate\n".to_owned();
        if !output.mismatches.is_empty() {
            out.push('\n');
            out.push_str("EXPECT FAILURES:\n");
            for m in &output.mismatches {
                let actual_str = m.actual.as_ref().map_or("not_found", |v| v.as_str());
                let _ = writeln!(
                    out,
                    "  command='{}': expected={} actual={}",
                    m.command,
                    m.expected.as_str(),
                    actual_str,
                );
            }
        }
        return out;
    }

    let cmd_width = output
        .verdicts
        .iter()
        .map(|v| v.command.len())
        .max()
        .unwrap_or(7)
        .max(7); // min width: "command"
    let rule_width = output
        .verdicts
        .iter()
        .map(|v| v.matching_rule.len())
        .max()
        .unwrap_or(12)
        .max(12); // min width: "matching_rule"

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<width$}  {:<8}  {:<rule_w$}  profile",
        "command",
        "verdict",
        "matching_rule",
        width = cmd_width,
        rule_w = rule_width,
    );
    let _ = writeln!(
        out,
        "{:-<width$}  {:-<8}  {:-<rule_w$}  -------",
        "",
        "",
        "",
        width = cmd_width,
        rule_w = rule_width,
    );

    for v in &output.verdicts {
        let _ = writeln!(
            out,
            "{:<width$}  {:<8}  {:<rule_w$}  {}",
            v.command,
            v.verdict.as_str(),
            v.matching_rule,
            v.profile,
            width = cmd_width,
            rule_w = rule_width,
        );
    }

    if !output.mismatches.is_empty() {
        out.push('\n');
        out.push_str("EXPECT FAILURES:\n");
        for m in &output.mismatches {
            let actual_str = m.actual.as_ref().map_or("not_found", |v| v.as_str());
            let _ = writeln!(
                out,
                "  command='{}': expected={} actual={}",
                m.command,
                m.expected.as_str(),
                actual_str,
            );
        }
    }

    out
}

/// Format the policy-check output as a schema-versioned JSON value.
///
/// # Errors
/// Returns a [`serde_json::Error`] if serialisation fails (should never happen
/// for well-formed inputs).
pub fn format_json(output: &PolicyCheckOutput) -> Result<serde_json::Value, serde_json::Error> {
    let verdicts: Vec<serde_json::Value> = output
        .verdicts
        .iter()
        .map(|v| {
            serde_json::json!({
                "command": v.command,
                "verdict": v.verdict.as_str(),
                "matching_rule": v.matching_rule,
                "profile": v.profile,
            })
        })
        .collect();

    let mismatches: Vec<serde_json::Value> = output
        .mismatches
        .iter()
        .map(|m| {
            serde_json::json!({
                "command": m.command,
                "expected": m.expected.as_str(),
                "actual": m.actual.as_ref().map_or("not_found", |v| v.as_str()),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "artifact_kind": "policy_check",
        "schema_version": POLICY_CHECK_SCHEMA_VERSION.to_string(),
        "profile": output.profile,
        "verdicts": verdicts,
        "mismatches": mismatches,
    }))
}
