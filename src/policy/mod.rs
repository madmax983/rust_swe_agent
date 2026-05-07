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
        PolicyRule::deny_static(
            "catastrophic-delete-root",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)(?:sudo\s+)?rm\b[^|;\n]*\s+/(?:$|[\s;&|)`])",
        ),
        PolicyRule::deny_static(
            "catastrophic-delete-root-glob",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)(?:sudo\s+)?rm\b[^|;\n]*\s+/\*",
        ),
        PolicyRule::deny_static(
            "catastrophic-delete-no-preserve-root",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)(?:sudo\s+)?rm\b[^|;\n]*--no-preserve-root\b",
        ),
        PolicyRule::deny_static(
            "catastrophic-delete-home",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)(?:sudo\s+)?rm\b[^|;\n]*\s+~/?(?:$|[\s;&|)`])",
        ),
        PolicyRule::deny_static(
            "catastrophic-delete-system-dir",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)(?:sudo\s+)?rm\b[^|;\n]*\s+/(?:etc|var|usr|home|root|boot|lib|bin|sbin)/?(?:$|[\s;&|)`])",
        ),
        PolicyRule::deny_static("find-delete-all", r"find\s+/\s+[^|;\n]*-delete\b"),
        PolicyRule::deny_static("find-exec-rm-all", r"find\s+/\s+[^|;\n]*-exec\s+rm\b"),
        // --- Privilege escalation ---
        PolicyRule::deny_static(
            "sudo-shell-spawn",
            r"sudo\s+(?:-[a-zA-Z]+\s+)*(?:su|bash|sh|zsh|fish|dash)\b",
        ),
        PolicyRule::deny_static("sudo-interactive-root", r"sudo\s+-[a-zA-Z]*i[a-zA-Z]*\b"),
        PolicyRule::deny_static("sudo-spawn-shell-s", r"sudo\s+-s\b"),
        PolicyRule::deny_static("sudo-passwd-change", r"sudo\s+passwd\b"),
        PolicyRule::deny_static("sudo-visudo", r"sudo\s+visudo\b"),
        PolicyRule::deny_static(
            "sudo-run-as-user-shell",
            r"sudo\s+-u\s+\S+\s+(?:bash|sh|zsh|fish|dash|su)\b",
        ),
        PolicyRule::deny_static(
            "su-root",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)su(?:\s+-)?(?:\s+root)?(?:$|[\s;&|)`])",
        ),
        PolicyRule::deny_static(
            "chmod-sensitive-files",
            r"(?:sudo\s+)?chmod\s+[^|;\n]*(?:/etc/(?:shadow|passwd|sudoers)|/etc\b)",
        ),
        PolicyRule::deny_static("chmod-777-system", r"(?:sudo\s+)?chmod\s+-R\s+777\s+/"),
        PolicyRule::deny_static(
            "chown-system-root",
            r"(?:sudo\s+)?chown\s+-R\s+\S+\s+/(?:$|[\s;&|)`])",
        ),
        PolicyRule::deny_static(
            "chown-system-dirs",
            r"(?:sudo\s+)?chown\s+-R\s+\S+\s+/(?:etc|var|usr|bin|sbin|lib|boot|home|root)\b",
        ),
        // --- Raw disk / device writes ---
        // Match dd writes to real block devices (sd*, hd*, nvme*, xvd*, vd*, disk*)
        PolicyRule::deny_static(
            "dd-device-write",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)dd\b[^|;\n]*of=/dev/(?:sd|hd|nvme|xvd|vd|disk)[a-zA-Z0-9]",
        ),
        PolicyRule::deny_static(
            "mkfs-on-device",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)mkfs(?:\.[a-z0-9]+)?\s+[^|;\n]*/dev/[a-zA-Z]",
        ),
        PolicyRule::deny_static(
            "shred-device",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)shred\b[^|;\n]*/dev/[a-zA-Z]",
        ),
        PolicyRule::deny_static(
            "badblocks-write",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)badblocks\s+-[a-zA-Z]*w[a-zA-Z]*\s",
        ),
        PolicyRule::deny_static(
            "hdparm-erase",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)hdparm\s+--security-erase\b",
        ),
        PolicyRule::deny_static(
            "fdisk-device",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)fdisk\s+/dev/[a-zA-Z]",
        ),
        PolicyRule::deny_static(
            "parted-device",
            r"(?:^|\n\s*|\|\s*|;\s*|&&\s*|\|\|\s*|\$\(\s*|`\s*|\(\s*)parted\s+/dev/[a-zA-Z]",
        ),
        // --- Credential file reads ---
        PolicyRule::deny_static(
            "read-ssh-private-key",
            r"(?:cat|head|tail|less|more|strings|xxd|hexdump)\s+[^|;\n]*~?/?\.?ssh/id_(?:rsa|ecdsa|ed25519|dsa)\b",
        ),
        PolicyRule::deny_static(
            "read-aws-credentials",
            r"(?:cat|head|tail|less|more)\s+[^|;\n]*~?/?\.aws/(?:credentials|config)\b",
        ),
        PolicyRule::deny_static(
            "read-shadow",
            r"(?:cat|head|tail|less|more|strings)\s+[^|;\n]*/etc/(?:shadow|gshadow)\b",
        ),
        PolicyRule::deny_static(
            "read-etc-passwd",
            r"(?:cat|head|tail|less|more|strings)\s+[^|;\n]*/etc/passwd\b",
        ),
        PolicyRule::deny_static(
            "read-proc-keys",
            r"(?:cat|head|tail|less|more)\s+[^|;\n]*/proc/(?:keys|key-users|kmsg|version)\b",
        ),
        PolicyRule::deny_static(
            "read-netrc",
            r"(?:cat|head|tail|less|more)\s+[^|;\n]*~?/?\.netrc\b",
        ),
        PolicyRule::deny_static(
            "read-git-credentials",
            r"(?:cat|head|tail|less|more)\s+[^|;\n]*~?/?\.git-credentials\b",
        ),
        PolicyRule::deny_static(
            "read-pgpass",
            r"(?:cat|head|tail|less|more)\s+[^|;\n]*~?/?\.pgpass\b",
        ),
        PolicyRule::deny_static(
            "read-gcloud-credentials",
            r"(?:cat|head|tail|less|more)\s+[^|;\n]*(?:application_default_credentials\.json|gcloud/credentials)",
        ),
        PolicyRule::deny_static(
            "find-read-private-keys",
            r"find\s+[^|;\n]*-name\s+[^|;\n]*-exec\s+cat\b",
        ),
        // --- Shell-script-from-network ---
        PolicyRule::deny_static(
            "script-from-network-pipe-shell",
            r"(?:curl|wget|fetch)\b[^|;\n]*\|\s*(?:ba)?sh\b",
        ),
        PolicyRule::deny_static(
            "script-from-network-pipe-python",
            r"(?:curl|wget|fetch)\b[^|;\n]*\|\s*python[23]?\b",
        ),
        PolicyRule::deny_static(
            "script-from-network-pipe-interpreter",
            r"(?:curl|wget|fetch)\b[^|;\n]*\|\s*(?:perl|ruby|node|php)\b",
        ),
        PolicyRule::deny_static(
            "bash-process-substitution-network",
            r"(?:ba)?sh\s+<\s*\(\s*(?:curl|wget|fetch)\b",
        ),
        PolicyRule::deny_static(
            "python-exec-from-network",
            r"python[23]?\s+-c\s+.+(?:urllib|requests).+exec\b",
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

// ── PolicyEngine ──────────────────────────────────────────────────────────────

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
    pub fn from_cfg(cfg: &PolicyCfg) -> Result<Self, regex::Error> {
        let profile = PolicyProfile::from_str(&cfg.profile).unwrap_or(PolicyProfile::Safe);
        let extra_allow = cfg
            .extra_allow_patterns
            .iter()
            .enumerate()
            .map(|(i, pat)| {
                Ok(PolicyRule {
                    label: format!("cfg-allow-{i}"),
                    pattern: Regex::new(pat)?,
                    decision: RuleDecision::Allow,
                })
            })
            .collect::<Result<Vec<_>, regex::Error>>()?;
        let extra_deny = cfg
            .extra_deny_patterns
            .iter()
            .enumerate()
            .map(|(i, pat)| {
                Ok(PolicyRule {
                    label: format!("cfg-deny-{i}"),
                    pattern: Regex::new(pat)?,
                    decision: RuleDecision::Deny,
                })
            })
            .collect::<Result<Vec<_>, regex::Error>>()?;
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

        for rule in &self.rules {
            if rule.matches(command) {
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
