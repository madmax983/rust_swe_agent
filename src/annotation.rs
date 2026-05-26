//! Persistent operator annotation store for SWE-bench instance triage notes.
//!
//! Schema: `annotations-1.0` — a JSON file keyed by `instance_id`, then by
//! `tag`, storing a note and RFC 3339 timestamps.  Writes are atomic
//! (write-to-temp-then-rename) so two concurrent `annotate add` calls both
//! land; last-writer-wins on the same `(instance_id, tag)` pair, with
//! `updated_at` reflecting the winner.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error};

/// Regex constraining valid tag values: kebab-case, 1–32 chars.
pub const TAG_REGEX: &str = r"^[a-z0-9][a-z0-9-]{0,31}$";

/// Maximum note length in Unicode chars.
pub const NOTE_MAX_CHARS: usize = 1024;

/// `schema_version` written to the JSON store.
pub const SCHEMA_VERSION: &str = "annotations-1.0";

/// Environment variable that overrides the default store path.
pub const BENCH_ANNOTATIONS_PATH_ENV: &str = "BENCH_ANNOTATIONS_PATH";

/// Default store filename, resolved relative to the current working directory.
pub const DEFAULT_STORE_FILENAME: &str = "annotations.json";

/// Determine the annotation store path: `--store` flag > env var > default.
#[must_use]
pub fn resolve_store_path(flag: Option<&Path>) -> PathBuf {
    if let Some(p) = flag {
        return p.to_path_buf();
    }
    if let Ok(env) = std::env::var(BENCH_ANNOTATIONS_PATH_ENV) {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    PathBuf::from(DEFAULT_STORE_FILENAME)
}

/// A single annotation record for one `(instance_id, tag)` pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Annotation {
    pub instance_id: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// On-disk JSON representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreData {
    schema_version: String,
    /// `instance_id` → (`tag` → entry).
    instances: BTreeMap<String, BTreeMap<String, StoreEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    created_at: String,
    updated_at: String,
}

/// In-memory annotation store.  Load with [`AnnotationStore::load_or_default`],
/// mutate with [`add`][`AnnotationStore::add`] / [`remove`][`AnnotationStore::remove`],
/// persist with [`save`][`AnnotationStore::save`].
#[derive(Debug, Clone)]
pub struct AnnotationStore {
    data: StoreData,
}

impl AnnotationStore {
    /// Load an existing store from `path`, or return an empty store if the
    /// file does not exist.  Returns an error on parse failures.
    pub fn load_or_default(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Ok(Self {
                data: StoreData {
                    schema_version: SCHEMA_VERSION.to_owned(),
                    instances: BTreeMap::new(),
                },
            });
        }
        let text = std::fs::read_to_string(path)?;
        let data: StoreData = serde_json::from_str(&text).map_err(|e| {
            Error::Config(ConfigError::Invalid(format!(
                "annotations: failed to parse {}: {e}",
                path.display()
            )))
        })?;
        Ok(Self { data })
    }

    /// Add (or update) an annotation record for `(instance_id, tag)`.
    ///
    /// Validates the tag format and note length before mutating.
    pub fn add(
        &mut self,
        instance_id: &str,
        tag: &str,
        note: Option<&str>,
    ) -> Result<(), Error> {
        validate_tag(tag)?;
        if let Some(n) = note {
            validate_note(n)?;
        }

        let now = utc_now_rfc3339();
        let instance_map = self.data.instances.entry(instance_id.to_owned()).or_default();
        let entry = instance_map.entry(tag.to_owned()).or_insert_with(|| StoreEntry {
            note: None,
            created_at: now.clone(),
            updated_at: now.clone(),
        });
        entry.note = note.map(str::to_owned);
        entry.updated_at = now;
        Ok(())
    }

    /// Remove annotation(s) for `instance_id`.  If `tag` is `Some`, remove
    /// only that tag; if `None`, remove all tags for the instance.
    pub fn remove(&mut self, instance_id: &str, tag: Option<&str>) {
        match tag {
            Some(t) => {
                if let Some(instance_map) = self.data.instances.get_mut(instance_id) {
                    instance_map.remove(t);
                    if instance_map.is_empty() {
                        self.data.instances.remove(instance_id);
                    }
                }
            }
            None => {
                self.data.instances.remove(instance_id);
            }
        }
    }

    /// List annotations, optionally filtered by `instance_id` and/or `tag`.
    #[must_use]
    pub fn list(
        &self,
        instance_id: Option<&str>,
        tag_filter: Option<&str>,
    ) -> Vec<Annotation> {
        let mut results = Vec::new();
        for (iid, tags) in &self.data.instances {
            if let Some(filter) = instance_id {
                if iid != filter {
                    continue;
                }
            }
            for (t, entry) in tags {
                if let Some(tf) = tag_filter {
                    if t != tf {
                        continue;
                    }
                }
                results.push(Annotation {
                    instance_id: iid.clone(),
                    tag: t.clone(),
                    note: entry.note.clone(),
                    created_at: entry.created_at.clone(),
                    updated_at: entry.updated_at.clone(),
                });
            }
        }
        results
    }

    /// Return compact tag list for one instance (used by triage display).
    #[must_use]
    pub fn tags_for(&self, instance_id: &str) -> Vec<String> {
        self.data
            .instances
            .get(instance_id)
            .map(|tags| tags.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Atomically write the store to `path` (write-temp-then-rename).
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string_pretty(&self.data)?;
        let bytes = json.into_bytes();
        atomic_write(path, &bytes)?;
        Ok(())
    }
}

// ── validation helpers ───────────────────────────────────────────────────────

fn validate_tag(tag: &str) -> Result<(), Error> {
    let re = tag_regex();
    if !re.is_match(tag) {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "annotations: invalid tag format `{tag}`; must match {TAG_REGEX}"
        ))));
    }
    Ok(())
}

fn validate_note(note: &str) -> Result<(), Error> {
    let len = note.chars().count();
    if len > NOTE_MAX_CHARS {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "annotations: note is {len} chars; maximum is {NOTE_MAX_CHARS}; \
             truncate or shorten the note before adding"
        ))));
    }
    Ok(())
}

fn tag_regex() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(TAG_REGEX).expect("TAG_REGEX is valid"))
}

// ── atomic write ─────────────────────────────────────────────────────────────

fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write as _;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(dest).map_err(|e| Error::Io(e.error))?;
    Ok(())
}

// ── time helper ──────────────────────────────────────────────────────────────

fn utc_now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_tags_pass() {
        let re = regex::Regex::new(TAG_REGEX).unwrap();
        assert!(re.is_match("a"));
        assert!(re.is_match("ignore"));
        assert!(re.is_match("evaluator-flake"));
        assert!(re.is_match("real-regression"));
        assert!(re.is_match("0xdeadbeef"));
    }

    #[test]
    fn invalid_tags_fail() {
        let re = regex::Regex::new(TAG_REGEX).unwrap();
        assert!(!re.is_match("-bad"));
        assert!(!re.is_match("BadTag"));
        assert!(!re.is_match("bad_tag"));
        assert!(!re.is_match("bad.tag"));
        assert!(!re.is_match(""));
        assert!(!re.is_match(&"a".repeat(33)));
    }

    #[test]
    fn add_and_list_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        let mut store = AnnotationStore::load_or_default(&path).unwrap();
        store.add("id1", "tag1", Some("note1")).unwrap();
        store.save(&path).unwrap();

        let loaded = AnnotationStore::load_or_default(&path).unwrap();
        let anns = loaded.list(Some("id1"), None);
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].note.as_deref(), Some("note1"));
    }

    #[test]
    fn note_too_long_rejected() {
        let mut store = AnnotationStore::load_or_default(Path::new("/nonexistent")).unwrap();
        assert!(store.add("id1", "tag1", Some(&"x".repeat(1025))).is_err());
    }
}
