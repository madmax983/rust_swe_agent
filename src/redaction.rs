//! Run-scoped secret redaction.
//!
//! This is deliberately not a full DLP engine. It masks configured literals,
//! common structured secret shapes, and current-process environment values
//! with sensitive names before text reaches persisted or shareable surfaces.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::RedactionCfg;
use crate::stream::{StreamEvent, StreamSink};

pub mod surface {
    pub const TRAJECTORY: &str = "trajectory";
    pub const MODEL_OBSERVATION: &str = "model_observation";
    pub const STREAM: &str = "stream";
    pub const INSPECT: &str = "inspect";
    pub const EXPORT: &str = "export";
    pub const PATCH_SUBMISSION: &str = "patch_submission";
    pub const GITHUB_COMMENT: &str = "github_comment";
}

const KIND_CONFIGURED_LITERAL: &str = "configured_literal";
const KIND_CUSTOM_PATTERN: &str = "custom_pattern";
const KIND_PRIVATE_KEY: &str = "private_key";
const KIND_GITHUB_TOKEN: &str = "github_token";
const KIND_BEARER_TOKEN: &str = "bearer_token";
const KIND_API_KEY: &str = "api_key";
const KIND_ENV_ASSIGNMENT: &str = "env_assignment";
const KIND_SECRET_FIELD: &str = "secret_field";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactionCount {
    pub surface: String,
    pub kind: String,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactionSummary {
    pub enabled: bool,
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counts: Vec<RedactionCount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretLeak {
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionOutcome {
    pub text: String,
    pub redacted: bool,
}

#[derive(Clone)]
pub struct Redactor {
    inner: Arc<RedactorInner>,
}

struct RedactorInner {
    enabled: bool,
    unsafe_allow_secret_leaks: bool,
    salt: String,
    rules: Vec<RedactionRule>,
    blocking_literals: Vec<String>,
    markers: Mutex<BTreeMap<String, String>>,
    counts: Mutex<BTreeMap<(String, String), u64>>,
}

#[derive(Clone)]
struct RedactionRule {
    kind: String,
    matcher: RuleMatcher,
}

#[derive(Clone)]
enum RuleMatcher {
    Literal(String),
    Regex {
        regex: Regex,
        capture_group: Option<usize>,
    },
    EnvAssignment {
        regex: Regex,
    },
}

#[derive(Debug)]
struct RedactionMatch {
    start: usize,
    end: usize,
    kind: String,
    raw: String,
}

impl Redactor {
    pub fn from_config(cfg: &RedactionCfg) -> Result<Self, regex::Error> {
        let mut rules = Vec::new();
        let mut blocking_literals = Vec::new();
        let mut seen_literals = BTreeSet::new();

        if cfg.enabled {
            for literal in cfg.secret_literals.iter().filter(|value| !value.is_empty()) {
                push_literal_rule(
                    &mut rules,
                    &mut blocking_literals,
                    &mut seen_literals,
                    KIND_CONFIGURED_LITERAL,
                    literal.clone(),
                );
            }

            for (name, value) in std::env::vars() {
                if let Some(kind) = env_literal_kind(&name, &value) {
                    push_literal_rule(
                        &mut rules,
                        &mut blocking_literals,
                        &mut seen_literals,
                        kind,
                        value,
                    );
                }
            }

            rules.extend(default_rules()?);

            for pattern in &cfg.custom_patterns {
                rules.push(RedactionRule {
                    kind: KIND_CUSTOM_PATTERN.to_owned(),
                    matcher: RuleMatcher::Regex {
                        regex: Regex::new(pattern)?,
                        capture_group: None,
                    },
                });
            }
        }

        Ok(Self {
            inner: Arc::new(RedactorInner {
                enabled: cfg.enabled,
                unsafe_allow_secret_leaks: cfg.unsafe_allow_secret_leaks,
                salt: new_salt(),
                rules,
                blocking_literals,
                markers: Mutex::new(BTreeMap::new()),
                counts: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    pub fn from_config_lossy(cfg: &RedactionCfg) -> Self {
        Self::from_config(cfg).unwrap_or_else(|_| Self::disabled())
    }

    pub fn default_enabled() -> Self {
        Self::from_config_lossy(&RedactionCfg::default())
    }

    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(RedactorInner {
                enabled: false,
                unsafe_allow_secret_leaks: false,
                salt: new_salt(),
                rules: Vec::new(),
                blocking_literals: Vec::new(),
                markers: Mutex::new(BTreeMap::new()),
                counts: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    #[must_use]
    pub fn redact_text(&self, input: &str, surface: &str) -> RedactionOutcome {
        if !self.inner.enabled || input.is_empty() {
            return RedactionOutcome {
                text: input.to_owned(),
                redacted: false,
            };
        }

        let mut matches = self.collect_matches(input);
        if matches.is_empty() {
            return RedactionOutcome {
                text: input.to_owned(),
                redacted: false,
            };
        }

        matches.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| b.end.cmp(&a.end))
                .then_with(|| a.kind.cmp(&b.kind))
        });

        let mut filtered = Vec::new();
        let mut next_available = 0usize;
        for candidate in matches {
            if candidate.start < next_available {
                continue;
            }
            next_available = candidate.end;
            filtered.push(candidate);
        }

        let mut out = String::with_capacity(input.len());
        let mut last = 0usize;
        for matched in filtered {
            out.push_str(&input[last..matched.start]);
            let marker = self.marker_for(&matched.raw, &matched.kind);
            out.push_str(&marker);
            self.increment(surface, &matched.kind);
            last = matched.end;
        }
        out.push_str(&input[last..]);
        RedactionOutcome {
            text: out,
            redacted: true,
        }
    }

    pub fn redact_json_value(&self, value: &mut serde_json::Value, surface: &str) -> bool {
        if !self.inner.enabled {
            return false;
        }
        match value {
            serde_json::Value::Object(map) => {
                let mut redacted = false;
                for (key, child) in map.iter_mut() {
                    if let Some(kind) = sensitive_key_kind(key) {
                        redacted |= self.redact_sensitive_value(child, surface, kind);
                    } else {
                        redacted |= self.redact_json_value(child, surface);
                    }
                }
                redacted
            }
            serde_json::Value::Array(values) => {
                let mut redacted = false;
                for child in values {
                    redacted |= self.redact_json_value(child, surface);
                }
                redacted
            }
            serde_json::Value::String(text) => {
                let outcome = self.redact_text(text, surface);
                if outcome.redacted {
                    *text = outcome.text;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn configured_literal_leak(&self, text: &str) -> Option<SecretLeak> {
        if !self.inner.enabled || self.inner.unsafe_allow_secret_leaks {
            return None;
        }
        self.inner
            .blocking_literals
            .iter()
            .filter(|literal| !literal.is_empty())
            .find(|literal| text.contains(literal.as_str()))
            .map(|_| SecretLeak {
                kind: KIND_CONFIGURED_LITERAL.to_owned(),
            })
    }

    #[must_use]
    pub fn unsafe_allow_secret_leaks(&self) -> bool {
        self.inner.unsafe_allow_secret_leaks
    }

    #[must_use]
    pub fn summary(&self) -> RedactionSummary {
        let mut rows = {
            let counts = self
                .inner
                .counts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            counts
                .iter()
                .map(|((surface, kind), count)| RedactionCount {
                    surface: surface.clone(),
                    kind: kind.clone(),
                    count: *count,
                })
                .collect::<Vec<_>>()
        };
        rows.sort_by(|a, b| a.surface.cmp(&b.surface).then_with(|| a.kind.cmp(&b.kind)));
        RedactionSummary {
            enabled: self.inner.enabled,
            redacted: !rows.is_empty(),
            counts: rows,
        }
    }

    fn collect_matches(&self, input: &str) -> Vec<RedactionMatch> {
        let mut out = Vec::new();
        for rule in &self.inner.rules {
            match &rule.matcher {
                RuleMatcher::Literal(literal) => {
                    collect_literal_matches(input, literal, &rule.kind, &mut out);
                }
                RuleMatcher::Regex {
                    regex,
                    capture_group,
                } => {
                    for captures in regex.captures_iter(input) {
                        let matched = capture_group
                            .and_then(|idx| captures.get(idx))
                            .or_else(|| captures.get(0));
                        if let Some(matched) = matched {
                            if matched.start() < matched.end() {
                                out.push(RedactionMatch {
                                    start: matched.start(),
                                    end: matched.end(),
                                    kind: rule.kind.clone(),
                                    raw: matched.as_str().to_owned(),
                                });
                            }
                        }
                    }
                }
                RuleMatcher::EnvAssignment { regex } => {
                    for captures in regex.captures_iter(input) {
                        let (Some(name), Some(value)) = (captures.get(1), captures.get(2)) else {
                            continue;
                        };
                        if env_name_is_sensitive(name.as_str()) && value.start() < value.end() {
                            out.push(RedactionMatch {
                                start: value.start(),
                                end: value.end(),
                                kind: rule.kind.clone(),
                                raw: value.as_str().to_owned(),
                            });
                        }
                    }
                }
            }
        }
        out
    }

    fn marker_for(&self, raw: &str, kind: &str) -> String {
        let mut markers = self
            .inner
            .markers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(marker) = markers.get(raw) {
            return marker.clone();
        }
        let mut hasher = Sha256::new();
        hasher.update(self.inner.salt.as_bytes());
        hasher.update([0]);
        hasher.update(raw.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let hash = &digest[..12];
        let marker = format!("[REDACTED:{kind}:{}:{hash}]", size_class(raw));
        markers.insert(raw.to_owned(), marker.clone());
        marker
    }

    fn increment(&self, surface: &str, kind: &str) {
        let mut counts = self
            .inner
            .counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (surface.to_owned(), kind.to_owned());
        let value = counts.entry(key).or_insert(0);
        *value = value.saturating_add(1);
        drop(counts);
    }

    fn redact_sensitive_value(
        &self,
        value: &mut serde_json::Value,
        surface: &str,
        kind: &str,
    ) -> bool {
        match value {
            serde_json::Value::String(text) => {
                if text.is_empty() {
                    return false;
                }
                let marker = self.marker_for(text, kind);
                *text = marker;
                self.increment(surface, kind);
                true
            }
            serde_json::Value::Array(values) => {
                let mut redacted = false;
                for child in values {
                    redacted |= self.redact_sensitive_value(child, surface, kind);
                }
                redacted
            }
            serde_json::Value::Object(map) => {
                let mut redacted = false;
                for child in map.values_mut() {
                    redacted |= self.redact_sensitive_value(child, surface, kind);
                }
                redacted
            }
            _ => false,
        }
    }
}

pub struct RedactingSink {
    inner: Arc<dyn StreamSink>,
    redactor: Redactor,
}

impl RedactingSink {
    pub fn new(inner: Arc<dyn StreamSink>, redactor: Redactor) -> Self {
        Self { inner, redactor }
    }
}

impl StreamSink for RedactingSink {
    fn emit(&self, event: StreamEvent) {
        self.inner.emit(redact_stream_event(&event, &self.redactor));
    }
}

pub fn redact_stream_event(event: &StreamEvent, redactor: &Redactor) -> StreamEvent {
    match event {
        StreamEvent::RunStarted {
            task,
            model,
            started_at,
        } => StreamEvent::RunStarted {
            task: redactor.redact_text(task, surface::STREAM).text,
            model: redactor.redact_text(model, surface::STREAM).text,
            started_at: started_at.clone(),
        },
        StreamEvent::AssistantMessage {
            step,
            content,
            cost_usd,
            timestamp,
        } => StreamEvent::AssistantMessage {
            step: *step,
            content: redactor.redact_text(content, surface::STREAM).text,
            cost_usd: *cost_usd,
            timestamp: timestamp.clone(),
        },
        StreamEvent::BashStart {
            step,
            command,
            timestamp,
        } => StreamEvent::BashStart {
            step: *step,
            command: redactor.redact_text(command, surface::STREAM).text,
            timestamp: timestamp.clone(),
        },
        StreamEvent::BashResult {
            step,
            exit_code,
            stdout,
            stderr,
            timed_out,
            timestamp,
        } => StreamEvent::BashResult {
            step: *step,
            exit_code: *exit_code,
            stdout: redactor.redact_text(stdout, surface::STREAM).text,
            stderr: redactor.redact_text(stderr, surface::STREAM).text,
            timed_out: *timed_out,
            timestamp: timestamp.clone(),
        },
        StreamEvent::Observation {
            step,
            content,
            timestamp,
        } => StreamEvent::Observation {
            step: *step,
            content: redactor.redact_text(content, surface::STREAM).text,
            timestamp: timestamp.clone(),
        },
        StreamEvent::FormatError {
            step,
            content,
            timestamp,
        } => StreamEvent::FormatError {
            step: *step,
            content: redactor.redact_text(content, surface::STREAM).text,
            timestamp: timestamp.clone(),
        },
        StreamEvent::RunEnded {
            exit_reason,
            failure_category,
            final_output,
            steps,
            total_cost_usd,
            ended_at,
        } => StreamEvent::RunEnded {
            exit_reason: exit_reason.clone(),
            failure_category: *failure_category,
            final_output: final_output
                .as_ref()
                .map(|value| redactor.redact_text(value, surface::STREAM).text),
            steps: *steps,
            total_cost_usd: *total_cost_usd,
            ended_at: ended_at.clone(),
        },
    }
}

fn default_rules() -> Result<Vec<RedactionRule>, regex::Error> {
    Ok(vec![
        RedactionRule {
            kind: KIND_PRIVATE_KEY.to_owned(),
            matcher: RuleMatcher::Regex {
                regex: Regex::new(
                    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
                )?,
                capture_group: None,
            },
        },
        RedactionRule {
            kind: KIND_BEARER_TOKEN.to_owned(),
            matcher: RuleMatcher::Regex {
                regex: Regex::new(r"(?i)\bBearer\s+([A-Za-z0-9._~+/=-]{16,})")?,
                capture_group: Some(1),
            },
        },
        RedactionRule {
            kind: KIND_GITHUB_TOKEN.to_owned(),
            matcher: RuleMatcher::Regex {
                regex: Regex::new(
                    r"\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,})\b",
                )?,
                capture_group: None,
            },
        },
        RedactionRule {
            kind: KIND_API_KEY.to_owned(),
            matcher: RuleMatcher::Regex {
                regex: Regex::new(
                    r"\b(?:sk-[A-Za-z0-9][A-Za-z0-9_-]{16,}|sk-ant-[A-Za-z0-9_-]{16,}|AKIA[0-9A-Z]{16})\b",
                )?,
                capture_group: None,
            },
        },
        RedactionRule {
            kind: KIND_ENV_ASSIGNMENT.to_owned(),
            matcher: RuleMatcher::EnvAssignment {
                regex: Regex::new(
                    r#"(?m)^[+\- ]?(?:export\s+)?([A-Z][A-Z0-9_-]*)\s*=\s*([^ \t\r\n'";]{4,})[ \t]*(?:#.*)?$"#,
                )?,
            },
        },
        RedactionRule {
            kind: KIND_ENV_ASSIGNMENT.to_owned(),
            matcher: RuleMatcher::EnvAssignment {
                regex: Regex::new(
                    r#"(?m)^[+\- ]?(?:export\s+)?([A-Z][A-Z0-9_-]*)\s*=\s*"([^"\r\n]{4,})"[ \t]*(?:#.*)?$"#,
                )?,
            },
        },
        RedactionRule {
            kind: KIND_ENV_ASSIGNMENT.to_owned(),
            matcher: RuleMatcher::EnvAssignment {
                regex: Regex::new(
                    r#"(?m)^[+\- ]?(?:export\s+)?([A-Z][A-Z0-9_-]*)\s*=\s*'([^'\r\n]{4,})'[ \t]*(?:#.*)?$"#,
                )?,
            },
        },
    ])
}

fn collect_literal_matches(input: &str, literal: &str, kind: &str, out: &mut Vec<RedactionMatch>) {
    if literal.is_empty() {
        return;
    }
    let mut search_start = 0usize;
    while let Some(offset) = input[search_start..].find(literal) {
        let start = search_start + offset;
        let end = start + literal.len();
        out.push(RedactionMatch {
            start,
            end,
            kind: kind.to_owned(),
            raw: literal.to_owned(),
        });
        search_start = end;
    }
}

fn push_literal_rule(
    rules: &mut Vec<RedactionRule>,
    blocking_literals: &mut Vec<String>,
    seen_literals: &mut BTreeSet<String>,
    kind: &str,
    literal: String,
) {
    if literal.is_empty() || !seen_literals.insert(literal.clone()) {
        return;
    }
    rules.push(RedactionRule {
        kind: kind.to_owned(),
        matcher: RuleMatcher::Literal(literal.clone()),
    });
    blocking_literals.push(literal);
}

fn new_salt() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!(
        "{nanos}:{}:{:?}",
        std::process::id(),
        std::thread::current().id()
    )
}

fn size_class(value: &str) -> &'static str {
    match value.len() {
        0..=15 => "short",
        16..=63 => "medium",
        _ => "long",
    }
}

fn env_name_is_sensitive(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if ["TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
    {
        return true;
    }
    upper.split(['_', '-']).any(|segment| segment == "KEY")
}

fn env_literal_kind(name: &str, value: &str) -> Option<&'static str> {
    (value.len() >= 4 && env_name_is_sensitive(name)).then(|| env_secret_kind(name))
}

fn env_secret_kind(name: &str) -> &'static str {
    let upper = name.to_ascii_uppercase();
    if upper.contains("TOKEN") {
        "env_token"
    } else if upper.contains("SECRET") {
        "env_secret"
    } else if upper.contains("PASSWORD") {
        "env_password"
    } else if upper.contains("CREDENTIAL") {
        "env_credential"
    } else {
        "env_key"
    }
}

fn sensitive_key_kind(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    if lower.contains("token") {
        Some("env_token")
    } else if lower.contains("secret") {
        Some("env_secret")
    } else if lower.contains("password") {
        Some("env_password")
    } else if lower.contains("credential") {
        Some("env_credential")
    } else if lower.ends_with("key") || lower.contains("_key") || lower.contains("-key") {
        Some("env_key")
    } else if lower == "custom_patterns" {
        Some(KIND_CUSTOM_PATTERN)
    } else if lower.contains("literal") {
        Some(KIND_SECRET_FIELD)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn same_value_gets_same_marker() {
        let redactor = Redactor::default_enabled();
        let one = redactor.redact_text("ghp_0123456789ABCDEF0123456789ABCDEF0123", "a");
        let two = redactor.redact_text("x ghp_0123456789ABCDEF0123456789ABCDEF0123", "b");
        let marker = one.text;
        assert!(two.text.contains(&marker));
    }

    #[test]
    fn configured_literal_detection_respects_unsafe_bypass() {
        let cfg = RedactionCfg {
            secret_literals: vec!["literal-secret".into()],
            ..RedactionCfg::default()
        };
        let redactor = Redactor::from_config(&cfg).unwrap();
        assert!(
            redactor
                .configured_literal_leak("x literal-secret y")
                .is_some()
        );

        let cfg = RedactionCfg {
            secret_literals: vec!["literal-secret".into()],
            unsafe_allow_secret_leaks: true,
            ..RedactionCfg::default()
        };
        let redactor = Redactor::from_config(&cfg).unwrap();
        assert!(
            redactor
                .configured_literal_leak("x literal-secret y")
                .is_none()
        );
    }

    #[test]
    fn configured_literals_are_deduplicated_before_rules() {
        let cfg = RedactionCfg {
            secret_literals: vec!["repeat-secret".into(), "repeat-secret".into()],
            ..RedactionCfg::default()
        };

        let redactor = Redactor::from_config(&cfg).unwrap();
        let configured_rules = redactor
            .inner
            .rules
            .iter()
            .filter(|rule| rule.kind == KIND_CONFIGURED_LITERAL)
            .count();
        let blocking_literals = redactor
            .inner
            .blocking_literals
            .iter()
            .filter(|literal| literal.as_str() == "repeat-secret")
            .count();

        assert_eq!(configured_rules, 1);
        assert_eq!(blocking_literals, 1);
    }

    #[test]
    fn env_key_matching_ignores_key_substrings() {
        assert!(!env_name_is_sensitive("KEYBOARD_LAYOUT"));
        assert!(!env_name_is_sensitive("GNOME_KEYRING_PID"));
        assert!(!env_name_is_sensitive("MONKEY"));
        assert!(env_name_is_sensitive("API_KEY"));
        assert!(env_name_is_sensitive("API-KEY"));
        assert!(env_name_is_sensitive("GITHUB_TOKEN"));
    }

    #[test]
    fn env_literal_candidates_require_minimum_length() {
        assert!(env_literal_kind("API_TOKEN", "abc").is_none());
        assert_eq!(env_literal_kind("API_TOKEN", "abcd"), Some("env_token"));
        assert!(env_literal_kind("KEYBOARD_LAYOUT", "us").is_none());
    }

    #[test]
    fn env_assignment_redaction_ignores_key_substrings() {
        let redactor = Redactor::default_enabled();

        let outcome = redactor.redact_text(
            "MONKEY=abcd\nKEYBOARD_LAYOUT=uspc\nAPI_KEY=secret-value\nGITHUB_TOKEN=\"quoted-token-value\"\n",
            surface::TRAJECTORY,
        );

        assert!(outcome.redacted, "expected sensitive assignments to redact");
        assert!(
            outcome.text.contains("MONKEY=abcd"),
            "benign KEY substring was redacted: {}",
            outcome.text
        );
        assert!(
            outcome.text.contains("KEYBOARD_LAYOUT=uspc"),
            "ordinary key-containing name was redacted: {}",
            outcome.text
        );
        assert!(
            !outcome.text.contains("secret-value"),
            "API_KEY value leaked: {}",
            outcome.text
        );
        assert!(
            !outcome.text.contains("quoted-token-value"),
            "GITHUB_TOKEN value leaked: {}",
            outcome.text
        );
        assert!(outcome.text.contains("API_KEY=[REDACTED:env_assignment:"));
        assert!(
            outcome
                .text
                .contains("GITHUB_TOKEN=\"[REDACTED:env_assignment:")
        );
    }

    #[test]
    fn env_assignment_redaction_ignores_lowercase_source_assignments() {
        let redactor = Redactor::default_enabled();

        let patch =
            "+let key = \"name\";\n+api_key = \"test\";\n+token = \"none\";\n+API_KEY = test;\n";
        let outcome = redactor.redact_text(patch, surface::PATCH_SUBMISSION);

        assert!(
            !outcome.redacted,
            "ordinary source assignments should not be redacted:\n{}",
            outcome.text
        );
        assert_eq!(outcome.text, patch);
    }

    #[test]
    fn env_assignment_redaction_detects_env_style_patch_lines() {
        let redactor = Redactor::default_enabled();

        let outcome = redactor.redact_text(
            "+API_KEY=secret-value\n+export GITHUB_TOKEN=\"quoted-token-value\"\n",
            surface::PATCH_SUBMISSION,
        );

        assert!(outcome.redacted, "expected env-style assignments to redact");
        assert!(!outcome.text.contains("secret-value"), "{}", outcome.text);
        assert!(
            !outcome.text.contains("quoted-token-value"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn json_redaction_visits_every_sensitive_array_member() {
        let redactor = Redactor::default_enabled();
        let mut value = serde_json::json!({
            "api_keys": [
                "sk-firstsecretvalue000000",
                "sk-secondsecretvalue00000"
            ],
            "nested": [
                "ghp_0123456789ABCDEF0123456789ABCDEF0123",
                "MY_SECRET=\"quoted-secret-value\""
            ]
        });

        assert!(redactor.redact_json_value(&mut value, surface::TRAJECTORY));
        let rendered = value.to_string();
        assert!(!rendered.contains("sk-firstsecretvalue000000"));
        assert!(!rendered.contains("sk-secondsecretvalue00000"));
        assert!(!rendered.contains("ghp_0123456789ABCDEF0123456789ABCDEF0123"));
        assert!(!rendered.contains("quoted-secret-value"));
    }
    #[test]
    fn env_secret_kind_handles_various_formats() {
        let cases = vec![
            ("MY_TOKEN", "env_token"),
            ("github_token", "env_token"),
            ("api_secret", "env_secret"),
            ("SECRET_KEY", "env_secret"),
            ("USER_PASSWORD", "env_password"),
            ("password123", "env_password"),
            ("AWS_CREDENTIAL", "env_credential"),
            ("credential_file", "env_credential"),
            ("API_KEY", "env_key"),
            ("random_key", "env_key"),
            ("something_else", "env_key"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                env_secret_kind(input),
                expected,
                "Failed for input: {input}",
            );
        }
    }

    #[test]
    fn sensitive_key_kind_handles_various_formats() {
        let cases = vec![
            ("auth_token", Some("env_token")),
            ("TOKEN", Some("env_token")),
            ("client_secret", Some("env_secret")),
            ("db_password", Some("env_password")),
            ("aws_credential", Some("env_credential")),
            ("api_key", Some("env_key")),
            ("API-KEY", Some("env_key")),
            ("custom_patterns", Some(KIND_CUSTOM_PATTERN)),
            ("field_literal", Some(KIND_SECRET_FIELD)),
            ("benign_value", None),
            ("keyboard_layout", None),
        ];

        for (input, expected) in cases {
            assert_eq!(
                sensitive_key_kind(input),
                expected,
                "Failed for input: {input}",
            );
        }
    }
}
