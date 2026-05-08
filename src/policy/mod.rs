//! Pre-execution command policy engine (issue #90).
//!
//! Three profiles:
//! - `safe`  — default unattended: blocks a built-in dangerous-command corpus.
//! - `ask`   — human approval for every non-allowlisted command; in
//!   non-interactive contexts, Ask decisions fail closed (Deny).
//! - `yolo`  — explicit opt-out; preserves current unrestricted behaviour.
//!
//! Threat model: this is a pre-execution guardrail and audit layer.  It is
//! NOT a replacement for Docker/sandboxing, secret redaction (#86), or
//! repository permissions.  Pattern matching operates on the raw command
//! string; a sufficiently determined adversary can obfuscate.

use regex::Regex;
use serde::{Deserialize, Serialize};

// ── Public config type (lives next to RedactionCfg in schema.rs) ─────────────

/// Operator-level policy configuration.  Mirrors the shape of
/// [`crate::config::schema::RedactionCfg`] so it sits naturally in `[policy]`
/// inside a run config TOML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyCfg {
    /// Named profile: `"safe"` (default), `"ask"`, or `"yolo"`.
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Extra regex patterns that should always be denied, appended after the
    /// built-in corpus.
    #[serde(default)]
    pub extra_deny_patterns: Vec<String>,
    /// Regex patterns that are always allowed even when the built-in corpus
    /// would deny them (escape hatch for known-safe commands).
    #[serde(default)]
    pub extra_allow_patterns: Vec<String>,
}

fn default_profile() -> String {
    "safe".into()
}

impl Default for PolicyCfg {
    fn default() -> Self {
        Self {
            profile: default_profile(),
            extra_deny_patterns: Vec::new(),
            extra_allow_patterns: Vec::new(),
        }
    }
}

// ── Policy profiles ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyProfile {
    /// Block the built-in dangerous-command corpus; allow everything else.
    Safe,
    /// All commands require human approval unless they match an explicit
    /// allow-rule.  Interpretation depends on the runtime:
    /// - non-interactive runners (`DefaultAgent`, sweeps, CI) MUST resolve
    ///   `Ask` to `Deny` via [`PolicyEngine::check_command_non_interactive`].
    ///   Per the spec, ask "fails closed in non-interactive contexts" — so in
    ///   `DefaultAgent` this profile behaves as a stricter `Safe`.
    /// - interactive runners (e.g. a future `InteractiveAgent` integration)
    ///   should call [`PolicyEngine::check_command`] and present the operator
    ///   with an approval prompt for `Ask` decisions.
    Ask,
    /// No restrictions; preserves unrestricted behaviour.  Every command is
    /// allowed but counts as a yolo-bypass in telemetry.
    Yolo,
}

impl PolicyProfile {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "safe" => Some(Self::Safe),
            "ask" => Some(Self::Ask),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Ask => "ask",
            Self::Yolo => "yolo",
        }
    }
}

// ── Policy config errors ─────────────────────────────────────────────────────

/// Error raised when an operator-supplied policy config cannot be parsed
/// into a [`PolicyEngine`].
#[derive(Debug)]
pub enum PolicyConfigError {
    /// `[policy] profile = "..."` was not one of `safe`, `ask`, `yolo`.
    UnknownProfile(String),
    /// A regex in `extra_allow_patterns` or `extra_deny_patterns` failed
    /// to compile.
    InvalidRegex { label: String, source: regex::Error },
}

impl std::fmt::Display for PolicyConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownProfile(p) => write!(
                f,
                "unknown policy profile {p:?} (expected one of: safe, ask, yolo)"
            ),
            Self::InvalidRegex { label, source } => {
                write!(f, "invalid regex in policy rule {label:?}: {source}")
            }
        }
    }
}

impl std::error::Error for PolicyConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnknownProfile(_) => None,
            Self::InvalidRegex { source, .. } => Some(source),
        }
    }
}

// ── Policy decision ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Execute the command.
    Allow,
    /// Pause for human approval.  In non-interactive contexts this becomes
    /// `Deny` via [`PolicyEngine::check_command_non_interactive`].
    Ask,
    /// Block the command.  `label` is a short human-readable rule identifier
    /// persisted in the trajectory.
    Deny { label: String },
}

// ── Policy rules ─────────────────────────────────────────────────────────────

/// A single pattern-based rule.
#[derive(Debug, Clone)]
pub struct PolicyRule {
    pub label: String,
    pub pattern: Regex,
    pub decision: RuleDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleDecision {
    Allow,
    Ask,
    Deny,
}

impl PolicyRule {
    /// Compile a deny rule from a regex pattern string.
    ///
    /// # Panics
    /// Panics if `pattern` is not a valid regex — only use for static built-in
    /// patterns known to compile.
    #[allow(clippy::expect_used)]
    fn deny_static(label: &str, pattern: &str) -> Self {
        Self {
            label: label.to_owned(),
            pattern: Regex::new(pattern).expect("built-in policy regex must compile"),
            decision: RuleDecision::Deny,
        }
    }

    /// Public constructor for a deny rule from a user-supplied pattern.
    ///
    /// # Errors
    /// Returns a [`regex::Error`] if `pattern` does not compile.
    pub fn deny(label: impl Into<String>, pattern: impl AsRef<str>) -> Result<Self, regex::Error> {
        Ok(Self {
            label: label.into(),
            pattern: Regex::new(pattern.as_ref())?,
            decision: RuleDecision::Deny,
        })
    }

    /// Public constructor for an allow rule from a user-supplied pattern.
    ///
    /// # Errors
    /// Returns a [`regex::Error`] if `pattern` does not compile.
    pub fn allow(label: impl Into<String>, pattern: impl AsRef<str>) -> Result<Self, regex::Error> {
        Ok(Self {
            label: label.into(),
            pattern: Regex::new(pattern.as_ref())?,
            decision: RuleDecision::Allow,
        })
    }

    fn matches(&self, command: &str) -> bool {
        self.pattern.is_match(command)
    }
}

// ── Built-in dangerous-command corpus ─────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn builtin_deny_rules() -> Vec<PolicyRule> {
    vec![
        // --- Catastrophic deletes ---
        // Any `rm` (regardless of flag form) targeting bare `/` or `/*` or
        // `~` is treated as catastrophic.  Earlier versions tried to enforce
        // a specific `-rf` shape, which `rm -fr /`, `rm -Rf /`, `rm -r -f /`,
        // and `rm -rf -- /` all bypass.  Matching by target instead avoids
        // the flag-permutation rabbit hole.  False positives like `rm /` (no
        // `-r`, harmless) are acceptable safety conservatism.
        // Match `rm` targeting the (normalized) root: `/`, `//`, `///`, `/.`,
        // `/./`, `///./`, etc.  Linux resolves all of these to `/`.
        PolicyRule::deny_static(
            "catastrophic-delete-root",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?rm\b[^|;\n]*\s+['"]?/+(?:\.+/*)*['"]?(?:$|[\s;&|)`'"])"#,
        ),
        PolicyRule::deny_static(
            "catastrophic-delete-root-glob",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?rm\b[^|;\n]*\s+['"]?/+(?:\.+/+)*\*"#,
        ),
        PolicyRule::deny_static(
            "catastrophic-delete-no-preserve-root",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?rm\b[^|;\n]*--no-preserve-root\b"#,
        ),
        // Note: quoted `~` does NOT undergo tilde expansion in bash, so
        // `rm -rf '~'` removes a file literally named `~`, not the home dir.
        // Only the unquoted form is catastrophic.
        // Note: bash does NOT perform tilde expansion or `$HOME` expansion
        // inside SINGLE quotes, so `rm -rf '~'` and `rm -rf '$HOME'` are
        // literal and harmless.  Double quotes DO expand `$HOME` (but not
        // `~`).  We therefore match: bare `~`, `~/`, `~/*`; bare `$HOME` /
        // `${HOME}`; and `"$HOME"` / `"${HOME}"` — but NOT `'$HOME'`.
        PolicyRule::deny_static(
            "catastrophic-delete-home",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?rm\b[^|;\n]*\s+(?:~[a-zA-Z0-9_-]*(?:/\*?)?|"?\$\{?HOME\}?(?:/\*?)?"?)(?:$|[\s;&|)`'"])"#,
        ),
        // Match `/etc`, `/etc/`, and any path under a system dir that contains
        // a glob `*` (e.g. `/etc/*`, `/etc/*.conf`, `/etc/passwd*`,
        // `/var/log/*`).  Specific subpaths without a glob (e.g.
        // `/home/user/project/target`) are NOT blocked.
        PolicyRule::deny_static(
            "catastrophic-delete-system-dir",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?rm\b[^|;\n]*\s+['"]?/+(?:\.+/+)*(?:etc|var|usr|home|root|boot|lib|bin|sbin)(?:\*[^\s;&|)`'"]*|/(?:[^\s;&|)`'"]*\*[^\s;&|)`'"]*)?)?['"]?(?:$|[\s;&|)`'"])"#,
        ),
        // `find` actions (-delete / -exec rm) targeting bare `/` or any
        // protected system directory.  Optional surrounding quotes are
        // accepted on the path argument.
        PolicyRule::deny_static(
            "find-delete-all",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*find\s+['"]?/(?:etc|var|usr|home|root|boot|lib|bin|sbin)?(?:/[^\s'"|;]*)?['"]?\s+[^|;\n]*-delete\b"#,
        ),
        PolicyRule::deny_static(
            "find-exec-rm-all",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*find\s+['"]?/(?:etc|var|usr|home|root|boot|lib|bin|sbin)?(?:/[^\s'"|;]*)?['"]?\s+[^|;\n]*-exec\s+rm\b"#,
        ),
        // `find . -exec rm -rf /etc \;` runs `rm` against the protected
        // path even though find's own search root is benign.  Block when
        // the rm target inside -exec is itself a protected path or a
        // sensitive system file.
        PolicyRule::deny_static(
            "find-exec-rm-protected-target",
            r#"find\b[^|;\n]*-exec\s+(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-\S+)*\s+)*(?:\S*/)?rm\b[^|;\n]*\s+['"]?(?:/(?:etc|var|usr|home|root|boot|lib|bin|sbin)(?:/[^\s'"|;]*)?|/etc/(?:passwd|shadow|gshadow|sudoers|group|hosts|fstab|resolv\.conf)|/boot/grub/grub\.cfg|/boot/grub2/grub\.cfg|~|\$HOME|\$\{HOME\})['"]?(?:\s|\\;|;|$)"#,
        ),
        // `cd` to root or a protected system dir followed by `rm` makes
        // any relative target (e.g. `etc`) point at the protected dir.
        // Conservative: any `cd /protected ... ; rm` or `cd / ... && rm`.
        PolicyRule::deny_static(
            "cd-to-protected-then-rm",
            r#"(?:^|\n\s*|;\s*|&&\s*|\|\|\s*)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cd|pushd)\s+['"]?/(?:etc|var|usr|home|root|boot|lib|bin|sbin)?(?:/[^|;\n\s'"]*)?['"]?\s*(?:;|&&|\n)\s*(?:[^;\n]*\s)?(?:sudo\s+(?:-\S+\s+)*)?rm\b"#,
        ),
        // --- xargs feeding rm with recursive/force flags ---
        // `printf '/etc\n' | xargs rm -rf` runs `rm -rf` on whatever stdin
        // delivers, so the dangerous target may not be visible in the
        // command text.  Block any `xargs ... rm ... (-rf|-fr|--recursive
        // |--force)` form regardless of upstream arguments.
        PolicyRule::deny_static(
            "xargs-rm-recursive-force",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*xargs\b[^|;\n]*\brm\b[^|;\n]*(?:-[a-zA-Z]*[rRfF][a-zA-Z]*|--(?:recursive|force))"#,
        ),
        // --- Sensitive system files (deletes / overwrites) ---
        // Specific high-impact files that the broader system-dir rule
        // intentionally exempts (it only blocks `/etc`, `/etc/`, `/etc/*`,
        // not `/etc/passwd`).  Listed individually so common subdirs under
        // /home, /var, etc. stay allowed.
        PolicyRule::deny_static(
            "delete-sensitive-system-file",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?rm\b[^|;\n]*\s+['"]?(?:/etc/(?:passwd|shadow|gshadow|sudoers|group|hosts|fstab|resolv\.conf)|/boot/grub/grub\.cfg|/boot/grub2/grub\.cfg)['"]?(?:$|[\s;&|)`'"])"#,
        ),
        // --- Overwriting sensitive system files ---
        // `cp`, `mv`, and `install` can clobber the same critical files
        // covered by the delete rule above.  Match when the destination
        // path is one of those files.
        PolicyRule::deny_static(
            "overwrite-sensitive-system-file",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cp|mv|install)\b[^|;\n]*\s+['"]?(?:/etc/(?:passwd|shadow|gshadow|sudoers|group|hosts|fstab|resolv\.conf)|/boot/grub/grub\.cfg|/boot/grub2/grub\.cfg)['"]?(?:$|[\s;&|)`'"])"#,
        ),
        // `>`/`>>` redirect to a sensitive system file.  The chars between
        // the boundary and the `>` must consume any complete quoted strings
        // verbatim — otherwise an inner `>` inside `'...'` (e.g. a `printf
        // 'echo x > /etc/passwd' > docs.md` doc-writing workflow) would
        // falsely match.  After consuming a full quoted segment we resume
        // scanning, so `cat 'file' > /etc/passwd` is still blocked.
        PolicyRule::deny_static(
            "redirect-to-sensitive-file",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[^|;\n'"]|'[^']*'|"[^"$`]*")*?>>?\s*['"]?(?:/etc/(?:passwd|shadow|gshadow|sudoers|group|hosts|fstab|resolv\.conf)|/boot/grub/grub\.cfg|/boot/grub2/grub\.cfg)['"]?"#,
        ),
        // `tee` to a sensitive system file (with or without sudo / -a).
        PolicyRule::deny_static(
            "tee-to-sensitive-file",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?tee\b[^|;\n]*\s+['"]?(?:/etc/(?:passwd|shadow|gshadow|sudoers|group|hosts|fstab|resolv\.conf)|/boot/grub/grub\.cfg|/boot/grub2/grub\.cfg)['"]?(?:$|[\s;&|)`'"])"#,
        ),
        // `truncate -s SIZE FILE` shrinks (or extends) files; dropping the
        // size of a sensitive system file deletes its contents.
        PolicyRule::deny_static(
            "truncate-sensitive-file",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*(?:\S*/)?truncate\b[^|;\n]*\s+['"]?(?:/etc/(?:passwd|shadow|gshadow|sudoers|group|hosts|fstab|resolv\.conf)|/boot/grub/grub\.cfg|/boot/grub2/grub\.cfg)['"]?(?:$|[\s;&|)`'"])"#,
        ),
        // In-place editors (`sed -i`, `perl -i`, `gawk -i inplace`) mutate
        // the file directly.  When the target is a sensitive system file
        // this corrupts /etc/passwd etc. just like cp/mv/redirect/tee/dd.
        PolicyRule::deny_static(
            "inplace-edit-sensitive-file",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*(?:\S*/)?(?:sed|perl|gawk|awk|ruby)\b[^|;\n]*(?:-i\b|--in-place\b)[^|;\n]*\s+['"]?(?:/etc/(?:passwd|shadow|gshadow|sudoers|group|hosts|fstab|resolv\.conf)|/boot/grub/grub\.cfg|/boot/grub2/grub\.cfg)['"]?(?:$|[\s;&|)`'"])"#,
        ),
        // --- Redirection-to-block-device (`>`/`>>`/`tee`) ---
        // Bash opens the device for writing when stdout/`tee` targets a
        // raw block device, bypassing the dd/mkfs/etc. tool list.
        PolicyRule::deny_static(
            "redirect-to-block-device",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)[^|;\n]*?>>?\s*['"]?/dev/(?:sd[a-z]|hd[a-z]|nvme\d|xvd[a-z]|vd[a-z]|disk[\d/]|mapper/|dm-|md\d|loop\d|ram\d)"#,
        ),
        PolicyRule::deny_static(
            "tee-to-block-device",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?tee\b[^|;\n]*\s+['"]?/dev/(?:sd[a-z]|hd[a-z]|nvme\d|xvd[a-z]|vd[a-z]|disk[\d/]|mapper/|dm-|md\d|loop\d|ram\d)"#,
        ),
        // --- Privilege escalation ---
        // `-\S+` matches any short or long option (e.g. `-n`, `-nE`,
        // `--non-interactive`, `--preserve-env`).  Repeated to allow
        // multiple option groups separated by spaces.
        PolicyRule::deny_static(
            "sudo-shell-spawn",
            r"sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*(?:\S*/)?(?:su|bash|sh|zsh|fish|dash)\b",
        ),
        PolicyRule::deny_static(
            "sudo-interactive-root",
            r"sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*-[a-zA-Z]*i[a-zA-Z]*\b",
        ),
        // `-s` (short) and `--shell` (long) both spawn the user's shell.
        PolicyRule::deny_static(
            "sudo-spawn-shell-s",
            r"sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*(?:-s|--shell)\b",
        ),
        PolicyRule::deny_static(
            "sudo-passwd-change",
            r"sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*passwd\b",
        ),
        PolicyRule::deny_static(
            "sudo-visudo",
            r"sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*visudo\b",
        ),
        PolicyRule::deny_static(
            "sudo-run-as-user-shell",
            r"sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*-u\s+\S+\s+(?:-\S+\s+)*(?:\S*/)?(?:bash|sh|zsh|fish|dash|su)\b",
        ),
        PolicyRule::deny_static(
            "su-root",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+)*su(?:\s+-)?(?:\s+root)?(?:$|[\s;&|)`'"])"#,
        ),
        PolicyRule::deny_static(
            "chmod-sensitive-files",
            r"(?:sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*)?chmod\s+[^|;\n]*(?:/etc/(?:shadow|passwd|sudoers)|/etc\b)",
        ),
        // `-R` (short) and `--recursive` (long) are equivalent for chmod /
        // chown.  Accept both, plus other intermixed flag groups.
        PolicyRule::deny_static(
            "chmod-777-system",
            r"(?:sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*)?chmod\s+(?:-\S+\s+)*(?:-R|--recursive)\s+(?:-\S+\s+)*777\s+/",
        ),
        PolicyRule::deny_static(
            "chown-system-root",
            r#"(?:sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*)?chown\s+(?:-\S+\s+)*(?:-R|--recursive)\s+(?:-\S+\s+)*\S+\s+/(?:$|[\s;&|)`'"])"#,
        ),
        PolicyRule::deny_static(
            "chown-system-dirs",
            r"(?:sudo\s+(?:-[uUgGDhprtT]\s+\S+\s+|--(?:user|group|chdir|host|prompt|role|type)\s+\S+\s+|-\S+\s+)*)?chown\s+(?:-\S+\s+)*(?:-R|--recursive)\s+(?:-\S+\s+)*\S+\s+/(?:etc|var|usr|bin|sbin|lib|boot|home|root)\b",
        ),
        // --- Raw disk / device writes ---
        // Match dd writes to real block devices (sd*, hd*, nvme*, xvd*, vd*, disk*)
        PolicyRule::deny_static(
            "dd-device-write",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?dd\b[^|;\n]*of=['"]?/dev/(?:sd[a-z]|hd[a-z]|nvme\d|xvd[a-z]|vd[a-z]|disk[\d/]|mapper/|dm-|md\d|loop\d|ram\d)"#,
        ),
        // dd writes to sensitive system files.  `dd of=/etc/passwd ...`
        // bypasses the cp/mv/install/redirect/tee rules because dd has its
        // own `of=FILE` syntax.
        PolicyRule::deny_static(
            "dd-write-sensitive-file",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*(?:\S*/)?dd\b[^|;\n]*of=['"]?(?:/etc/(?:passwd|shadow|gshadow|sudoers|group|hosts|fstab|resolv\.conf)|/boot/grub/grub\.cfg|/boot/grub2/grub\.cfg)['"]?"#,
        ),
        PolicyRule::deny_static(
            "mkfs-on-device",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*mkfs(?:\.[a-z0-9]+)?\s+[^|;\n]*['"]?/dev/[a-zA-Z]"#,
        ),
        PolicyRule::deny_static(
            "shred-device",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:\S*/)?shred\b[^|;\n]*['"]?/dev/[a-zA-Z]"#,
        ),
        PolicyRule::deny_static(
            "badblocks-write",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*badblocks\s+-[a-zA-Z]*w[a-zA-Z]*\s"#,
        ),
        PolicyRule::deny_static(
            "hdparm-erase",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*hdparm\s+--security-erase\b"#,
        ),
        PolicyRule::deny_static(
            "fdisk-device",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*fdisk\s+['"]?/dev/[a-zA-Z]"#,
        ),
        PolicyRule::deny_static(
            "parted-device",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*parted\s+['"]?/dev/[a-zA-Z]"#,
        ),
        // --- Credential file reads ---
        // Anchored at command position with the standard prefix so `printf
        // 'cat /etc/passwd' > docs.md` (a quoted string, not an actual
        // read) does not falsely match.  The reader command (cat, head,
        // etc.) must follow a command boundary or a normal prefix
        // (assignments, sudo, wrappers).
        PolicyRule::deny_static(
            "read-ssh-private-key",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more|strings|xxd|hexdump)[\s<][^|;\n]*~?/?\.?ssh/id_(?:rsa|ecdsa|ed25519|dsa)\b"#,
        ),
        PolicyRule::deny_static(
            "read-aws-credentials",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more)[\s<][^|;\n]*~?/?\.aws/(?:credentials|config)\b"#,
        ),
        PolicyRule::deny_static(
            "read-shadow",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more|strings)[\s<][^|;\n]*/etc/(?:shadow|gshadow)\b"#,
        ),
        PolicyRule::deny_static(
            "read-etc-passwd",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more|strings)[\s<][^|;\n]*/etc/passwd\b"#,
        ),
        PolicyRule::deny_static(
            "read-proc-keys",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more)[\s<][^|;\n]*/proc/(?:keys|key-users|kmsg|version)\b"#,
        ),
        PolicyRule::deny_static(
            "read-netrc",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more)[\s<][^|;\n]*~?/?\.netrc\b"#,
        ),
        PolicyRule::deny_static(
            "read-git-credentials",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more)[\s<][^|;\n]*~?/?\.git-credentials\b"#,
        ),
        PolicyRule::deny_static(
            "read-pgpass",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more)[\s<][^|;\n]*~?/?\.pgpass\b"#,
        ),
        PolicyRule::deny_static(
            "read-gcloud-credentials",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*(?:cat|head|tail|less|more)[\s<][^|;\n]*(?:application_default_credentials\.json|gcloud/credentials)"#,
        ),
        PolicyRule::deny_static(
            "find-read-private-keys",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+|(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+['"]?|eval\s+(?:-\S+\s+)*['"]?)*find\s+[^|;\n]*-name\s+[^|;\n]*-exec\s+cat\b"#,
        ),
        // --- Shell-script-from-network ---
        // Anchored at command position so quoted documentation/fixture
        // strings like `printf 'curl ... | bash' > docs.md` don't falsely
        // match.
        PolicyRule::deny_static(
            "script-from-network-pipe-shell",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*(?:curl|wget|fetch)\b[^|;\n]*(?:\|[^|;\n]*)*?\|\s*(?:sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+)*(?:\S*/)?(?:bash|sh|zsh|ksh|dash|fish)\b"#,
        ),
        PolicyRule::deny_static(
            "script-from-network-pipe-python",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*(?:curl|wget|fetch)\b[^|;\n]*(?:\|[^|;\n]*)*?\|\s*(?:sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+)*(?:\S*/)?python[23]?\b"#,
        ),
        PolicyRule::deny_static(
            "script-from-network-pipe-interpreter",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*(?:curl|wget|fetch)\b[^|;\n]*(?:\|[^|;\n]*)*?\|\s*(?:sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+)*(?:\S*/)?(?:perl|ruby|node|php)\b"#,
        ),
        PolicyRule::deny_static(
            "bash-process-substitution-network",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*(?:\S*/)?(?:bash|sh|zsh|ksh|dash|fish)\s+<\s*\(\s*(?:curl|wget|fetch)\b"#,
        ),
        // `bash -c "$(curl ...)"` and ``bash -c "`curl ...`"`` execute the
        // downloaded content as a shell script just like the pipe and
        // process-substitution forms.  Common installer pattern.
        PolicyRule::deny_static(
            "shell-c-from-network-substitution",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*(?:\S*/)?(?:bash|sh|zsh|ksh|dash|fish)\s+(?:-\S+\s+)*-\S*c\S*\s+["']?(?:\$\(|`)\s*(?:[A-Za-z_]\w*=\S*\s+|(?:sudo|env)\s+(?:-\S+\s+)*)*(?:curl|wget|fetch)\b"#,
        ),
        // `eval "$(curl ...)"` and `eval \`curl ...\`` likewise execute
        // the downloaded content.
        PolicyRule::deny_static(
            "eval-from-network-substitution",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*eval\s+(?:-\S+\s+)*["']?(?:\$\(|`)\s*(?:[A-Za-z_]\w*=\S*\s+|(?:sudo|env)\s+(?:-\S+\s+)*)*(?:curl|wget|fetch)\b"#,
        ),
        PolicyRule::deny_static(
            "python-exec-from-network",
            r#"(?:^|\n\s*|\|\s*|;\s*|&&\s*|&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*|\{\s*|\)\s*|[\s;]then\s+|[\s;]do\s+|[\s;]else\s+)(?:[A-Za-z_]\w*=\S*\s+|!\s+|sudo(?:\s+-[uUgGDhprtT]\s+\S+|\s+--(?:user|group|chdir|host|prompt|role|type)\s+\S+|\s+-\S+)*\s+|env(?:\s+\S+)*?\s+-S\s+['"]?|env(?:\s+\S+)*?\s+--split-string(?:\s+|=)['"]?|env(?:\s+-[uCS]\s+\S+|\s+--(?:unset|chdir|split-string|block-signal|default-signal|ignore-signal)\s+\S+|\s+-\S+)*\s+|(?:command|time|exec|nohup|nice|builtin)(?:\s+-\S+)*\s+)*python[23]?\s+-c\s+.+(?:urllib|requests).+exec\b"#,
        ),
        // --- Outbound exfiltration ---
        PolicyRule::deny_static(
            "exfil-curl-post-sensitive",
            r"curl\b[^|;\n]*-[a-zA-Z]*[Xd][a-zA-Z]*\s+[^|;\n]*--data(?:-binary)?\s+@(?:/etc/|~?/?\.(?:ssh|aws|netrc|pgpass|git-credentials))",
        ),
        PolicyRule::deny_static(
            "exfil-curl-data-sensitive-file",
            r"curl\b[^|;\n]*(?:--data|--data-binary|-d)\s+@(?:/etc/|~?/?\.(?:ssh|aws|netrc|pgpass))",
        ),
        PolicyRule::deny_static(
            "exfil-wget-post-sensitive",
            r"wget\b[^|;\n]*--post-file=(?:/etc/|~?/?\.(?:ssh|aws|netrc))",
        ),
        PolicyRule::deny_static(
            "exfil-netcat-sensitive",
            r"nc\s+[^|;\n]+\s+\d+\s+<\s+(?:/etc/(?:passwd|shadow)|~?/?\.(?:ssh|aws))",
        ),
        PolicyRule::deny_static(
            "exfil-pipe-to-netcat",
            r"(?:cat|tar)\s+[^|;\n]*(?:/etc/|~?/?\.(?:ssh|aws))[^|;\n]*\|\s*(?:nc|netcat)\b",
        ),
        PolicyRule::deny_static(
            "exfil-tar-pipe-curl",
            r"tar\s+[^|;\n]*~?/?\.(?:ssh|aws)\s*\|\s*curl\b",
        ),
        // --- Fork bombs ---
        PolicyRule::deny_static("fork-bomb-colon", r":\s*\(\s*\)\s*\{"),
        PolicyRule::deny_static("fork-bomb-named", r"\w+\s*\(\s*\)\s*\{[^}]*\|\s*\w+\s*&"),
        // --- Reverse shells ---
        PolicyRule::deny_static(
            "reverse-shell-tcp-redirect",
            r"(?:ba)?sh\s+-i\s+>&\s*/dev/tcp/",
        ),
        PolicyRule::deny_static(
            "reverse-shell-netcat-exec",
            r"nc\s+[^|;\n]*-e\s+/bin/(?:ba)?sh\b",
        ),
        PolicyRule::deny_static(
            "reverse-shell-socat",
            r#"socat\s+[^|;\n]*exec:['"]?(?:ba)?sh\b"#,
        ),
        PolicyRule::deny_static(
            "reverse-shell-mkfifo",
            r"mkfifo\s+[^|;\n]+/[a-zA-Z]+\s*;.*\|\s*/bin/(?:ba)?sh",
        ),
        PolicyRule::deny_static(
            "reverse-shell-python-socket",
            r"python[23]?\s+-c\s+.+\bsocket\b.+\bconnect\b",
        ),
    ]
}

// ── Heredoc body stripping ───────────────────────────────────────────────────

/// Extract a write-target file from the heredoc-introducing line, if any.
///
/// Recognizes both shell redirection (`>`/`>>`) and `tee [-a] FILE`
/// pipelines.  The redirect can appear before OR after the `<<` operator
/// (`cat > FILE <<'EOF'` and `cat <<'EOF' > FILE` are both supported).
/// Surrounding quotes are trimmed.  Returns `None` when the line writes
/// to no file.
fn extract_redirect_target(intro_line: &str) -> Option<String> {
    // `tee FILE` is checked FIRST: when the line has both
    // `| tee /tmp/x` and `> /dev/null`, the heredoc body goes to
    // `/tmp/x` (tee duplicates stdin to FILE) — picking the redirect
    // would route us to /dev/null and miss the real script file.
    let Ok(tee_re) = Regex::new(r#"\btee\b(?:\s+-\S+)*\s+['"]?([^\s'"<>|;&]+)['"]?"#) else {
        return None;
    };
    if let Some(target) = tee_re
        .captures_iter(intro_line)
        .last()
        .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
    {
        return Some(target);
    }
    // Otherwise look for a `>`/`>>` stdout redirect.  Skip stderr
    // redirects like `2>` and `2>>` (preceded by a digit).  The
    // operator can legitimately appear before OR after the `<<`
    // operator (`cat > FILE <<'EOF'` and `cat <<'EOF' > FILE` are both
    // supported); we scan the whole intro line.
    let Ok(redirect_re) = Regex::new(r#"(?:^|[^0-9>])>>?\s*['"]?([^\s'"<>|;&]+)['"]?"#) else {
        return None;
    };
    redirect_re
        .captures_iter(intro_line)
        .last()
        .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
}

/// Does `region` invoke a known interpreter on the given file path, OR
/// directly execute the file via bare-path invocation?
///
/// Matches:
/// - `bash /tmp/x`, `sh ./script`, `/bin/bash file`, `source FILE`, `. FILE`
/// - bare execution at command position: `; /tmp/x`, `&& ./x`, `\n/tmp/x`
///   (covers the common `chmod +x FILE; FILE` pattern)
///
/// Path-variant handling: a relative target like `script.sh` is
/// commonly executed as `./script.sh` (and vice versa), so both forms
/// are checked.  Absolute paths are matched verbatim.
///
/// Note: `chmod +x FILE` alone is intentionally NOT a trigger — making
/// a file executable is data, not execution.  The actual unsafe case
/// `chmod +x FILE; FILE` is caught by the bare-path check below.  This
/// keeps benign `cat > fixture.sh <<'EOF' ... EOF; chmod +x fixture.sh`
/// workflows allowed.
fn interpreter_invokes_file(region: &str, file: &str) -> bool {
    // Generate the path forms to check.  An absolute path stands alone;
    // a bare basename and `./basename` are interchangeable in shell.
    let candidates: Vec<String> = if file.starts_with('/') {
        vec![file.to_owned()]
    } else if let Some(stripped) = file.strip_prefix("./") {
        vec![file.to_owned(), stripped.to_owned()]
    } else {
        vec![file.to_owned(), format!("./{file}")]
    };

    for candidate in candidates {
        let escaped = regex::escape(&candidate);

        // Direct interpreter invocation: bash FILE, sh FILE, source FILE, . FILE
        let interp_pat = format!(
            r"(?:^|[\s/;&|`(])(?:bash|sh|zsh|ksh|dash|fish|python[23]?|perl|ruby|node|php|tclsh|source|\.)\s+(?:-\S+\s+)*{escaped}\b"
        );
        if Regex::new(&interp_pat).is_ok_and(|re| re.is_match(region)) {
            return true;
        }

        // Bare path execution at command position: `; /tmp/x`, `&& ./x`,
        // `\n/tmp/x`.  The path appears as the first token of a new command.
        let bare_pat =
            format!(r#"(?:^|\n\s*|;\s*|&&\s*|\|\|\s*|\|\s*)['"]?{escaped}(?:\s|$|[;&|)`'"])"#);
        if Regex::new(&bare_pat).is_ok_and(|re| re.is_match(region)) {
            return true;
        }
    }
    false
}

/// Remove the bodies of NON-EXECUTABLE here-documents from a command.
///
/// Bash heredocs come in two important flavours that affect whether the
/// body is data or code:
///
/// 1. **Quoted delimiter** (`<<'EOF'` / `<<"EOF"`): bash does NOT perform
///    parameter or command substitution in the body.  The body is literal.
/// 2. **Unquoted delimiter** (`<<EOF`): bash DOES perform `$VAR` and
///    `$(cmd)` expansion in the body — so `cat <<EOF\n$(rm -rf /)\nEOF`
///    actually executes `rm -rf /` during heredoc processing.  But a body
///    that contains no `$` or `` ` `` is still pure data even with an
///    unquoted delimiter.
///
/// And independently:
///
/// 3. **Consumer** matters: `cat <<'EOF'\nrm -rf /\nEOF` writes `rm -rf /`
///    as data.  But `bash <<'EOF'\nrm -rf /\nEOF` runs the body AS A
///    SCRIPT — the heredoc IS the input to the shell.  Pipes on the same
///    line (`cat <<'EOF' | bash\n...`) likewise route the body to a shell.
///
/// We strip the body when ALL of:
/// - the consumer line mentions no shell/script interpreter, AND
/// - either the delimiter is quoted, OR the body contains no `$` or `` ` ``
///   (no expansion sites where bash could execute substitution), AND
/// - if the heredoc redirects to a file, no later command in the same
///   request invokes an interpreter on that file
///   (`cat > /tmp/x <<'EOF'\n...\nEOF\nbash /tmp/x` retains the body).
///
/// Anything else is left intact so the deny corpus can scan it.  Fail-safe:
/// when we cannot find a closing delimiter we also leave the body in place.
/// Unwrap `env -S 'payload'` / `env --split-string 'payload'` so the
/// payload becomes part of the command stream visible to deny rules.
///
/// Bash documents `env -S, --split-string=S` as splitting the string
/// into arguments and executing them.  Treating the payload as opaque
/// data lets dangerous commands like `env -S 'rm -rf /etc'` slip past
/// rules that only see `env` as a wrapper.
///
/// Both single- and double-quoted forms are unwrapped.  Other surrounding
/// shell structure (sudo, `&&`, `;`, etc.) is preserved.
/// Collapse `..` parent-directory components in absolute paths, so a
/// command like `rm -rf /tmp/../etc` is normalized to `rm -rf /etc`
/// before the deny corpus scans it.
///
/// Iterates until no more `/x/..` patterns remain.  Also collapses
/// leading `/..` (since `/..` resolves to `/` in unix).
///
/// Limitation: only handles absolute-path components in plain command
/// text.  Doesn't follow symlinks, doesn't resolve `..` past variable
/// expansions, and doesn't apply inside shell strings (which heredoc
/// stripping and env-S unwrapping already handle).
/// Expand simple bash brace expansions in absolute paths so the existing
/// catastrophic-delete patterns can fire on each branch.
///
/// Bash expands `rm -rf /{etc,home}` to `rm -rf /etc /home` before
/// invoking rm, but the policy gate sees the literal source.  Without
/// expansion, the system-dir rule's protected-name list doesn't see
/// any of the branches and the destructive command slips through.
///
/// Limitation: only `/{...}` (rooted at `/`) is expanded.  Nested
/// braces and sequence expansions like `{1..5}` are not handled.
fn expand_simple_brace_expansions(command: &str) -> String {
    let Ok(re) = Regex::new(r"/\{([^{}]+)\}") else {
        return command.to_owned();
    };
    re.replace_all(command, |caps: &regex::Captures| {
        let Some(inner) = caps.get(1) else {
            return String::new();
        };
        inner
            .as_str()
            .split(',')
            .map(|p| format!("/{}", p.trim()))
            .collect::<Vec<_>>()
            .join(" ")
    })
    .into_owned()
}

fn collapse_parent_components(command: &str) -> String {
    // Capture the trailing terminator so we can put it back: a `..` at
    // path position is followed by `/`, whitespace, or a separator.
    let Ok(re) = Regex::new(r#"/(?:[^/\s'"]+/)?\.\.([/\s;&|)])"#) else {
        return command.to_owned();
    };
    let mut current = command.to_owned();
    loop {
        let next = re.replace_all(&current, "$1").into_owned();
        if next == current {
            break;
        }
        current = next;
    }
    current
}

fn unwrap_env_split_string_payloads(command: &str) -> String {
    // Unwrap env -S/-c/--split-string by re-injecting `env <preserved_flags>
    // <payload>`.  This lets the existing env-wrapper alternations consume
    // any flags (including leading-dash payloads like `env -S '-u PATH cmd'`
    // where the payload itself starts with a flag), while also exposing
    // the embedded command to the deny corpus.  Capture group 1 captures
    // any flags between `env` and `-S`; group 2 is the payload contents.
    let patterns = [
        r#"env((?:\s+\S+)*?)\s+-S\s+'([^']*)'"#,
        r#"env((?:\s+\S+)*?)\s+-S\s+"([^"$`]*)""#,
        r#"env((?:\s+\S+)*?)\s+--split-string(?:\s+|=)'([^']*)'"#,
        r#"env((?:\s+\S+)*?)\s+--split-string(?:\s+|=)"([^"$`]*)""#,
    ];
    let mut out = command.to_owned();
    for pat in patterns {
        let Ok(re) = Regex::new(pat) else { continue };
        out = re.replace_all(&out, "env$1 $2").into_owned();
    }
    out
}

fn strip_heredoc_bodies(command: &str) -> String {
    // Match all three delimiter styles: single-quoted, double-quoted, and
    // bare.  The regex crate has no backreferences so we list each style
    // as its own alternative and read whichever capture group fired.
    let Ok(heredoc_start) = Regex::new(
        r#"<<-?\s*(?:'([A-Za-z_][A-Za-z0-9_]*)'|"([A-Za-z_][A-Za-z0-9_]*)"|([A-Za-z_][A-Za-z0-9_]*))"#,
    ) else {
        return command.to_owned();
    };
    // Interpreter detection: when one of these is the consumer, the body
    // becomes its script and must remain visible to the deny corpus.
    // Anchored to start-of-line, whitespace, or `/` so `cat > test.sh`
    // doesn't false-match the trailing `sh`.
    let Ok(interpreter_re) = Regex::new(
        r"(?:^|[\s/])(?:bash|sh|zsh|ksh|dash|fish|python[23]?|perl|ruby|node|php|tclsh|awk|sed|expect)\b",
    ) else {
        return command.to_owned();
    };

    let mut out = String::with_capacity(command.len());
    let mut cursor = 0usize;

    while let Some(m) = heredoc_start.find_at(command, cursor) {
        // Find the bounds of the heredoc-introducing line.
        let line_start = command[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        let line_end_after_op = command[m.end()..]
            .find('\n')
            .map_or(command.len(), |i| m.end() + i + 1);
        // Scan the WHOLE introducing line (not just the prefix before `<<`)
        // so a pipe to an interpreter — `cat <<'EOF' | bash` — keeps the
        // body visible to the deny corpus.
        let whole_intro_line = &command[line_start..line_end_after_op];
        if interpreter_re.is_match(whole_intro_line) {
            out.push_str(&command[cursor..m.end()]);
            cursor = m.end();
            continue;
        }

        // Extract the delimiter word and remember whether it was quoted.
        let Some(caps) = heredoc_start.captures(&command[m.start()..m.end()]) else {
            out.push_str(&command[cursor..m.end()]);
            cursor = m.end();
            continue;
        };
        let (delim_word, is_quoted) = if let Some(d) = caps.get(1) {
            (d.as_str(), true)
        } else if let Some(d) = caps.get(2) {
            (d.as_str(), true)
        } else if let Some(d) = caps.get(3) {
            (d.as_str(), false)
        } else {
            out.push_str(&command[cursor..m.end()]);
            cursor = m.end();
            continue;
        };

        // Locate the closing-delimiter line and the body in between.
        let body_start = line_end_after_op;
        let mut search_pos = body_start;
        let mut close_start: Option<usize> = None;
        let mut close_end_with_nl: usize = body_start;
        while search_pos <= command.len() {
            let line_end = command[search_pos..]
                .find('\n')
                .map_or(command.len(), |i| search_pos + i);
            if command[search_pos..line_end].trim() == delim_word {
                close_start = Some(search_pos);
                close_end_with_nl = if line_end < command.len() {
                    line_end + 1
                } else {
                    line_end
                };
                break;
            }
            if line_end >= command.len() {
                break;
            }
            search_pos = line_end + 1;
        }

        let Some(close_line_start) = close_start else {
            // No closing delimiter — leave everything intact (fail-safe).
            out.push_str(&command[cursor..]);
            return out;
        };

        // For unquoted heredocs, only strip when the body has no expansion
        // markers (`$` or `` ` ``).  Otherwise bash could perform command
        // substitution at heredoc time.
        let body = &command[body_start..close_line_start];
        let body_has_expansion = body.contains('$') || body.contains('`');

        // If the heredoc redirects to a file (`cat > /tmp/x <<'EOF'`) and a
        // later command invokes an interpreter on that file, the body IS
        // executed.  Retain it so the deny corpus can scan it.
        let redirect_target = extract_redirect_target(whole_intro_line);
        let later_executes = redirect_target
            .as_deref()
            .is_some_and(|target| interpreter_invokes_file(&command[close_end_with_nl..], target));

        let safe_to_strip = (is_quoted || !body_has_expansion) && !later_executes;

        if safe_to_strip {
            // Append intro line, blank line in place of body, closing-delim
            // line preserved so boundary regex sees the surrounding shape.
            out.push_str(&command[cursor..line_end_after_op]);
            out.push('\n');
            out.push_str(&command[close_line_start..close_end_with_nl]);
        } else {
            // Pass through verbatim so the body is visible to deny rules.
            out.push_str(&command[cursor..close_end_with_nl]);
        }
        cursor = close_end_with_nl;
    }

    out.push_str(&command[cursor..]);
    out
}

// ── PolicyEngine ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct PolicyEngine {
    profile: PolicyProfile,
    /// Rules are evaluated in order; first match wins.
    /// Allow rules placed before deny rules act as explicit escape hatches.
    rules: Vec<PolicyRule>,
}

impl PolicyEngine {
    /// Create an engine for a named profile using only the built-in corpus.
    pub fn new(profile: PolicyProfile) -> Self {
        match profile {
            PolicyProfile::Yolo => Self {
                profile: PolicyProfile::Yolo,
                rules: Vec::new(),
            },
            p => {
                let rules = builtin_deny_rules();
                Self { profile: p, rules }
            }
        }
    }

    /// The active policy profile.
    pub fn profile(&self) -> &PolicyProfile {
        &self.profile
    }

    /// Create an engine with operator-supplied extra rules.
    ///
    /// `extra_allow` rules are prepended (checked first, act as overrides).
    /// `extra_deny` rules are appended after the built-in corpus.
    pub fn with_extra_rules(
        profile: PolicyProfile,
        extra_allow: Vec<PolicyRule>,
        extra_deny: Vec<PolicyRule>,
    ) -> Self {
        match profile {
            PolicyProfile::Yolo => Self {
                profile: PolicyProfile::Yolo,
                rules: Vec::new(),
            },
            p => {
                let mut rules: Vec<PolicyRule> = extra_allow;
                rules.extend(builtin_deny_rules());
                rules.extend(extra_deny);
                Self { profile: p, rules }
            }
        }
    }

    /// Build an engine from an operator config.
    ///
    /// # Errors
    /// Returns [`PolicyConfigError::UnknownProfile`] if `cfg.profile` is not
    /// `"safe"`, `"ask"`, or `"yolo"` (case-insensitive).  Silently falling
    /// back to `Safe` on a typo would downgrade an operator who selected
    /// `ask` for fail-closed behaviour, so it is treated as a hard error.
    /// Returns [`PolicyConfigError::InvalidRegex`] if any extra
    /// allow/deny pattern fails to compile.
    pub fn from_cfg(cfg: &PolicyCfg) -> Result<Self, PolicyConfigError> {
        let profile = PolicyProfile::from_str(&cfg.profile)
            .ok_or_else(|| PolicyConfigError::UnknownProfile(cfg.profile.clone()))?;
        let extra_allow = cfg
            .extra_allow_patterns
            .iter()
            .enumerate()
            .map(|(i, pat)| {
                let label = format!("cfg-allow-{i}");
                Regex::new(pat)
                    .map(|r| PolicyRule {
                        label: label.clone(),
                        pattern: r,
                        decision: RuleDecision::Allow,
                    })
                    .map_err(|source| PolicyConfigError::InvalidRegex { label, source })
            })
            .collect::<Result<Vec<_>, PolicyConfigError>>()?;
        let extra_deny = cfg
            .extra_deny_patterns
            .iter()
            .enumerate()
            .map(|(i, pat)| {
                let label = format!("cfg-deny-{i}");
                Regex::new(pat)
                    .map(|r| PolicyRule {
                        label: label.clone(),
                        pattern: r,
                        decision: RuleDecision::Deny,
                    })
                    .map_err(|source| PolicyConfigError::InvalidRegex { label, source })
            })
            .collect::<Result<Vec<_>, PolicyConfigError>>()?;
        Ok(Self::with_extra_rules(profile, extra_allow, extra_deny))
    }

    /// Evaluate `command` against the policy.
    ///
    /// Rules are tested in order; the first matching rule wins.  If no rule
    /// matches, the default for the profile applies:
    /// - `safe`  → Allow
    /// - `ask`   → Ask
    /// - `yolo`  → Allow (counted as yolo-bypass by caller)
    pub fn check_command(&self, command: &str) -> PolicyDecision {
        if self.profile == PolicyProfile::Yolo {
            return PolicyDecision::Allow;
        }

        // Heredoc bodies are data fed to a tool (e.g. `cat`), not commands
        // to execute.  Strip them before applying boundary-based rules so a
        // model writing a fixture or test file that contains `rm -rf /` text
        // is not incorrectly blocked.
        // Bash removes `\<newline>` line continuations before parsing, so
        // `dd if=/dev/zero \\\n of=/dev/sda` is one logical command.  Do
        // the same here before pattern matching, otherwise the deny
        // patterns' `[^|;\n]*` segment stops at the embedded newline and
        // the second half is missed.
        let normalized = command.replace("\\\n", "");
        let normalized = strip_heredoc_bodies(&normalized);
        let normalized = unwrap_env_split_string_payloads(&normalized);
        let normalized = collapse_parent_components(&normalized);
        let normalized = expand_simple_brace_expansions(&normalized);

        for rule in &self.rules {
            if rule.matches(&normalized) {
                return match rule.decision {
                    RuleDecision::Allow => PolicyDecision::Allow,
                    RuleDecision::Ask => PolicyDecision::Ask,
                    RuleDecision::Deny => PolicyDecision::Deny {
                        label: rule.label.clone(),
                    },
                };
            }
        }

        match self.profile {
            PolicyProfile::Ask => PolicyDecision::Ask,
            PolicyProfile::Safe | PolicyProfile::Yolo => PolicyDecision::Allow,
        }
    }

    /// Like [`check_command`] but resolves `Ask` to `Deny` for non-interactive
    /// contexts (CI, unattended sweeps).  Guarantees no process is launched.
    pub fn check_command_non_interactive(&self, command: &str) -> PolicyDecision {
        match self.check_command(command) {
            PolicyDecision::Ask => PolicyDecision::Deny {
                label: "ask-non-interactive".into(),
            },
            other => other,
        }
    }

    /// Human-readable representation of the effective policy (profile + rule
    /// count summary).
    pub fn effective_policy_human(&self) -> String {
        let deny_count = self
            .rules
            .iter()
            .filter(|r| r.decision == RuleDecision::Deny)
            .count();
        let allow_count = self
            .rules
            .iter()
            .filter(|r| r.decision == RuleDecision::Allow)
            .count();
        format!(
            "Policy profile: {}\n\
             Rules: {} deny, {} allow\n\
             Default (no-match): {}",
            self.profile.as_str(),
            deny_count,
            allow_count,
            match self.profile {
                PolicyProfile::Safe => "allow",
                PolicyProfile::Ask => "ask (fails closed in non-interactive mode)",
                PolicyProfile::Yolo => "allow (yolo: unrestricted)",
            }
        )
    }

    /// Machine-readable JSON representation for pre-run audit logs.
    pub fn effective_policy_machine(&self) -> serde_json::Value {
        let rules: Vec<serde_json::Value> = self
            .rules
            .iter()
            .map(|r| {
                serde_json::json!({
                    "label": r.label,
                    "pattern": r.pattern.as_str(),
                    "decision": match r.decision {
                        RuleDecision::Allow => "allow",
                        RuleDecision::Ask => "ask",
                        RuleDecision::Deny => "deny",
                    },
                })
            })
            .collect();
        serde_json::json!({
            "profile": self.profile.as_str(),
            "rules": rules,
        })
    }
}

// ── PolicyCounts ──────────────────────────────────────────────────────────────

/// Accumulated telemetry for a single run.  Persisted in `TrajectoryInfo` and
/// surfaced in `bench tail` / sweep summary output.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyCounts {
    pub allowed: u64,
    pub asked: u64,
    pub blocked: u64,
    pub yolo_bypassed: u64,
}

impl PolicyCounts {
    pub fn record(&mut self, decision: &PolicyDecision) {
        match decision {
            PolicyDecision::Allow => self.allowed += 1,
            PolicyDecision::Ask => self.asked += 1,
            PolicyDecision::Deny { .. } => self.blocked += 1,
        }
    }

    pub fn record_yolo_bypass(&mut self) {
        self.yolo_bypassed += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.allowed == 0 && self.asked == 0 && self.blocked == 0 && self.yolo_bypassed == 0
    }
}
