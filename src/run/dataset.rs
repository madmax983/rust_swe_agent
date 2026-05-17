//! Named SWE-bench dataset aliases and local cache.
//!
//! Operators can launch sweeps either via a local JSONL path (`--dataset-path`)
//! or a named alias (`--dataset verified --split test`).  Named aliases are
//! resolved against a local on-disk cache; if the cache is warm the run starts
//! without any network access.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Recognised SWE-bench dataset variant names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwebenchAlias {
    Full,
    Lite,
    Verified,
}

impl SwebenchAlias {
    /// Canonical string name used in cache file paths and provenance records.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Lite => "lite",
            Self::Verified => "verified",
        }
    }
}

impl FromStr for SwebenchAlias {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" | "swe-bench" | "swe_bench" => Ok(Self::Full),
            "lite" | "swe-bench_lite" | "swe_bench_lite" => Ok(Self::Lite),
            "verified" | "swe-bench_verified" | "swe_bench_verified" => Ok(Self::Verified),
            other => Err(format!(
                "unknown dataset alias `{other}`; expected one of: full, lite, verified"
            )),
        }
    }
}

impl std::fmt::Display for SwebenchAlias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Dataset split selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwebenchSplit {
    Train,
    Test,
    Dev,
}

impl SwebenchSplit {
    /// Canonical string name used in cache file paths and provenance records.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Train => "train",
            Self::Test => "test",
            Self::Dev => "dev",
        }
    }
}

impl FromStr for SwebenchSplit {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "train" => Ok(Self::Train),
            "test" => Ok(Self::Test),
            "dev" => Ok(Self::Dev),
            other => Err(format!(
                "unknown split `{other}`; expected one of: train, test, dev"
            )),
        }
    }
}

impl std::fmt::Display for SwebenchSplit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Discriminant stored in provenance manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetSourceKind {
    Local,
    Named,
}

impl DatasetSourceKind {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Named => "named",
        }
    }
}

/// How the dataset was supplied — either a direct local JSONL path or a named
/// alias that is resolved against the on-disk cache.
#[derive(Debug, Clone)]
pub enum DatasetSource {
    LocalPath(PathBuf),
    Named {
        alias: SwebenchAlias,
        split: SwebenchSplit,
    },
}

impl DatasetSource {
    #[must_use]
    pub fn kind(&self) -> DatasetSourceKind {
        match self {
            Self::LocalPath(_) => DatasetSourceKind::Local,
            Self::Named { .. } => DatasetSourceKind::Named,
        }
    }

    /// Human-readable path or alias description for log messages.
    #[must_use]
    pub fn display_path(&self) -> String {
        match self {
            Self::LocalPath(p) => p.display().to_string(),
            Self::Named { alias, split } => format!("{alias}/{split}"),
        }
    }
}

/// Result of inspecting the on-disk cache for a named dataset.
#[derive(Debug, Clone)]
pub enum CacheStatus {
    /// Cache file exists and parsed as valid JSONL.
    Hit {
        path: PathBuf,
        sha256: String,
        instance_count: usize,
    },
    /// Cache file does not exist.
    Miss { expected_path: PathBuf },
    /// Cache file exists but failed to parse as JSONL.
    Corrupt { path: PathBuf, reason: String },
}

impl CacheStatus {
    /// Short label used in preflight check messages.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Hit { .. } => "hit",
            Self::Miss { .. } => "miss",
            Self::Corrupt { .. } => "corrupt",
        }
    }
}

/// Return the default dataset cache directory: `~/.cache/max/datasets`.
/// Falls back to `<cwd>/.dataset-cache` when the home directory cannot be determined.
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    home_dir().map_or_else(
        || PathBuf::from(".dataset-cache"),
        |h| h.join(".cache").join("max").join("datasets"),
    )
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Return the cache file path for the given alias and split under `cache_dir`.
#[must_use]
pub fn cache_path_for(cache_dir: &Path, alias: &SwebenchAlias, split: &SwebenchSplit) -> PathBuf {
    cache_dir
        .join(alias.as_str())
        .join(format!("{}.jsonl", split.as_str()))
}

/// Inspect the cache for the given alias/split without modifying anything.
pub fn check_cache(cache_dir: &Path, alias: &SwebenchAlias, split: &SwebenchSplit) -> CacheStatus {
    let path = cache_path_for(cache_dir, alias, split);
    if !path.exists() {
        return CacheStatus::Miss {
            expected_path: path,
        };
    }
    match std::fs::read(&path) {
        Err(e) => CacheStatus::Corrupt {
            path,
            reason: e.to_string(),
        },
        Ok(bytes) => match validate_jsonl_bytes(&bytes) {
            Ok(instance_count) => CacheStatus::Hit {
                sha256: sha256_hex(&bytes),
                path,
                instance_count,
            },
            Err(reason) => CacheStatus::Corrupt { path, reason },
        },
    }
}

/// Write `content` to the cache location for the given alias and split.
/// Creates intermediate directories as needed.
/// Returns the path written.
pub fn write_cache(
    cache_dir: &Path,
    alias: &SwebenchAlias,
    split: &SwebenchSplit,
    content: &[u8],
) -> Result<PathBuf, Error> {
    let path = cache_path_for(cache_dir, alias, split);
    let parent = path.parent().ok_or_else(|| {
        Error::Trajectory(format!(
            "cache path `{}` has no parent directory",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Metadata about a resolved dataset, used to populate provenance manifests.
#[derive(Debug)]
pub struct ResolvedDatasetMeta {
    /// Actual on-disk path from which bytes were read.
    pub path: PathBuf,
    pub sha256: String,
    pub instance_count: usize,
    /// Only set for `DatasetSource::Named`.
    pub alias: Option<SwebenchAlias>,
    /// Only set for `DatasetSource::Named`.
    pub split: Option<SwebenchSplit>,
    /// Cache path for named datasets; `None` for local paths.
    pub cache_path: Option<PathBuf>,
}

/// Resolve a `DatasetSource` to raw JSONL bytes and provenance metadata.
///
/// For `LocalPath` this reads the file directly.
/// For `Named` this checks the cache: on a hit it returns the cached bytes; on a
/// miss or corrupt it returns an actionable `Error`.
pub fn resolve_dataset(
    source: &DatasetSource,
    cache_dir: &Path,
) -> Result<(Vec<u8>, ResolvedDatasetMeta), Error> {
    match source {
        DatasetSource::LocalPath(path) => {
            let bytes = std::fs::read(path)?;
            let instance_count = count_jsonl_lines(&bytes).unwrap_or(0);
            let meta = ResolvedDatasetMeta {
                path: path.clone(),
                sha256: sha256_hex(&bytes),
                instance_count,
                alias: None,
                split: None,
                cache_path: None,
            };
            Ok((bytes, meta))
        }
        DatasetSource::Named { alias, split } => match check_cache(cache_dir, alias, split) {
            CacheStatus::Hit {
                path,
                sha256,
                instance_count,
            } => {
                let bytes = std::fs::read(&path)?;
                let meta = ResolvedDatasetMeta {
                    path: path.clone(),
                    sha256,
                    instance_count,
                    alias: Some(alias.clone()),
                    split: Some(split.clone()),
                    cache_path: Some(path),
                };
                Ok((bytes, meta))
            }
            CacheStatus::Miss { expected_path } => {
                Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "dataset alias `{alias}` split `{split}` not in cache: \
                        expected file at `{path}`\n\
                        \n\
                        To populate the cache, download the SWE-bench JSONL for the \
                        `{alias}` dataset (`{split}` split) and place it at:\n\
                        \n  {path}\n\
                        \n\
                        See: https://www.swebench.com/SWE-bench/guides/datasets/ for \
                        dataset download instructions.",
                    path = expected_path.display()
                ))))
            }
            CacheStatus::Corrupt { path, reason } => {
                Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                    "cached dataset `{alias}` split `{split}` at `{p}` is corrupt: {reason}\n\
                        \n\
                        Delete the file and re-populate the cache:\n\
                        \n  {p}",
                    p = path.display()
                ))))
            }
        },
    }
}

/// Count non-empty lines in a JSONL byte slice (does not validate JSON).
fn count_jsonl_lines(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    Some(text.lines().filter(|l| !l.trim().is_empty()).count())
}

/// Parse and count valid JSONL instances; returns an error string on the first
/// malformed line.
fn validate_jsonl_bytes(bytes: &[u8]) -> Result<usize, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("UTF-8 decode error: {e}"))?;
    let mut count = 0usize;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Use IgnoredAny to avoid allocating an in-memory DOM since we only care about syntax validity.
        let v = serde_json::from_str::<serde::de::IgnoredAny>(line);
        if let Err(e) = v {
            return Err(format!("line {}: {e}", i + 1));
        }
        count += 1;
    }
    Ok(count)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::str::FromStr;

    // ── SwebenchAlias parsing ──────────────────────────────────────────────

    #[test]
    fn alias_parses_canonical_forms() {
        assert_eq!(
            SwebenchAlias::from_str("full").unwrap(),
            SwebenchAlias::Full
        );
        assert_eq!(
            SwebenchAlias::from_str("lite").unwrap(),
            SwebenchAlias::Lite
        );
        assert_eq!(
            SwebenchAlias::from_str("verified").unwrap(),
            SwebenchAlias::Verified
        );
    }

    #[test]
    fn alias_parses_known_alternate_forms() {
        assert_eq!(
            SwebenchAlias::from_str("swe-bench").unwrap(),
            SwebenchAlias::Full
        );
        assert_eq!(
            SwebenchAlias::from_str("swe-bench_lite").unwrap(),
            SwebenchAlias::Lite
        );
        assert_eq!(
            SwebenchAlias::from_str("swe-bench_verified").unwrap(),
            SwebenchAlias::Verified
        );
    }

    #[test]
    fn alias_is_case_insensitive() {
        assert_eq!(
            SwebenchAlias::from_str("FULL").unwrap(),
            SwebenchAlias::Full
        );
        assert_eq!(
            SwebenchAlias::from_str("Lite").unwrap(),
            SwebenchAlias::Lite
        );
        assert_eq!(
            SwebenchAlias::from_str("VERIFIED").unwrap(),
            SwebenchAlias::Verified
        );
    }

    #[test]
    fn alias_rejects_unknown_values() {
        let err = SwebenchAlias::from_str("bogus").unwrap_err();
        assert!(err.contains("bogus"), "{err}");
        assert!(err.contains("full"), "{err}");
    }

    #[test]
    fn alias_display_matches_as_str() {
        for alias in [
            SwebenchAlias::Full,
            SwebenchAlias::Lite,
            SwebenchAlias::Verified,
        ] {
            assert_eq!(alias.to_string(), alias.as_str());
        }
    }

    // ── SwebenchSplit parsing ──────────────────────────────────────────────

    #[test]
    fn split_parses_canonical_forms() {
        assert_eq!(
            SwebenchSplit::from_str("train").unwrap(),
            SwebenchSplit::Train
        );
        assert_eq!(
            SwebenchSplit::from_str("test").unwrap(),
            SwebenchSplit::Test
        );
        assert_eq!(SwebenchSplit::from_str("dev").unwrap(), SwebenchSplit::Dev);
    }

    #[test]
    fn split_is_case_insensitive() {
        assert_eq!(
            SwebenchSplit::from_str("TEST").unwrap(),
            SwebenchSplit::Test
        );
        assert_eq!(SwebenchSplit::from_str("Dev").unwrap(), SwebenchSplit::Dev);
    }

    #[test]
    fn split_rejects_unknown_values() {
        let err = SwebenchSplit::from_str("validation").unwrap_err();
        assert!(err.contains("validation"), "{err}");
        assert!(err.contains("train"), "{err}");
    }

    #[test]
    fn split_display_matches_as_str() {
        for split in [
            SwebenchSplit::Train,
            SwebenchSplit::Test,
            SwebenchSplit::Dev,
        ] {
            assert_eq!(split.to_string(), split.as_str());
        }
    }

    // ── DatasetSourceKind ──────────────────────────────────────────────────

    #[test]
    fn source_kind_from_local_path() {
        let src = DatasetSource::LocalPath(PathBuf::from("file.jsonl"));
        assert_eq!(src.kind(), DatasetSourceKind::Local);
    }

    #[test]
    fn source_kind_from_named() {
        let src = DatasetSource::Named {
            alias: SwebenchAlias::Verified,
            split: SwebenchSplit::Test,
        };
        assert_eq!(src.kind(), DatasetSourceKind::Named);
    }

    // ── cache_path_for ─────────────────────────────────────────────────────

    #[test]
    fn cache_path_for_uses_alias_and_split() {
        let dir = PathBuf::from("/cache");
        let p = cache_path_for(&dir, &SwebenchAlias::Verified, &SwebenchSplit::Test);
        assert_eq!(p, PathBuf::from("/cache/verified/test.jsonl"));

        let p2 = cache_path_for(&dir, &SwebenchAlias::Lite, &SwebenchSplit::Dev);
        assert_eq!(p2, PathBuf::from("/cache/lite/dev.jsonl"));
    }

    // ── check_cache ────────────────────────────────────────────────────────

    #[test]
    fn check_cache_miss_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let status = check_cache(dir.path(), &SwebenchAlias::Verified, &SwebenchSplit::Test);
        let CacheStatus::Miss { expected_path } = status else {
            panic!("expected Miss, got {}", status.label());
        };
        assert_eq!(
            expected_path,
            cache_path_for(dir.path(), &SwebenchAlias::Verified, &SwebenchSplit::Test)
        );
    }

    #[test]
    fn check_cache_hit_when_valid_jsonl_present() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"{\"instance_id\":\"a\",\"problem_statement\":\"fix it\"}\n";
        write_cache(
            dir.path(),
            &SwebenchAlias::Lite,
            &SwebenchSplit::Test,
            content,
        )
        .unwrap();

        let status = check_cache(dir.path(), &SwebenchAlias::Lite, &SwebenchSplit::Test);
        let CacheStatus::Hit {
            instance_count,
            path,
            sha256,
        } = status
        else {
            panic!("expected Hit, got {}", status.label());
        };
        assert_eq!(instance_count, 1);
        assert_eq!(
            path,
            cache_path_for(dir.path(), &SwebenchAlias::Lite, &SwebenchSplit::Test)
        );
        assert!(!sha256.is_empty());
    }

    #[test]
    fn check_cache_corrupt_when_invalid_jsonl_present() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"this is not json\n";
        let path = cache_path_for(dir.path(), &SwebenchAlias::Full, &SwebenchSplit::Dev);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();

        let status = check_cache(dir.path(), &SwebenchAlias::Full, &SwebenchSplit::Dev);
        let CacheStatus::Corrupt { reason, .. } = status else {
            panic!("expected Corrupt, got {}", status.label());
        };
        assert!(!reason.is_empty(), "reason should not be empty");
    }

    // ── write_cache ────────────────────────────────────────────────────────

    #[test]
    fn write_cache_creates_directories_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"{}";
        let written_path = write_cache(
            dir.path(),
            &SwebenchAlias::Verified,
            &SwebenchSplit::Dev,
            content,
        )
        .unwrap();
        assert!(written_path.exists());
        assert_eq!(std::fs::read(&written_path).unwrap(), content);
    }

    // ── resolve_dataset ────────────────────────────────────────────────────

    #[test]
    fn resolve_dataset_local_path_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.jsonl");
        let content = b"{\"instance_id\":\"test-1\",\"problem_statement\":\"p\"}\n";
        std::fs::write(&file, content).unwrap();

        let source = DatasetSource::LocalPath(file.clone());
        let (bytes, meta) = resolve_dataset(&source, dir.path()).unwrap();
        assert_eq!(bytes, content);
        assert_eq!(meta.path, file);
        assert_eq!(meta.instance_count, 1);
        assert!(meta.alias.is_none());
        assert!(meta.split.is_none());
        assert!(meta.cache_path.is_none());
    }

    #[test]
    fn resolve_dataset_named_cache_hit_returns_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"{\"instance_id\":\"verified-1\",\"problem_statement\":\"p\"}\n\
                       {\"instance_id\":\"verified-2\",\"problem_statement\":\"q\"}\n";
        write_cache(
            dir.path(),
            &SwebenchAlias::Verified,
            &SwebenchSplit::Test,
            content,
        )
        .unwrap();

        let source = DatasetSource::Named {
            alias: SwebenchAlias::Verified,
            split: SwebenchSplit::Test,
        };
        let (bytes, meta) = resolve_dataset(&source, dir.path()).unwrap();
        assert_eq!(bytes, content);
        assert_eq!(meta.instance_count, 2);
        assert_eq!(meta.alias, Some(SwebenchAlias::Verified));
        assert_eq!(meta.split, Some(SwebenchSplit::Test));
        assert!(meta.cache_path.is_some());
    }

    #[test]
    fn resolve_dataset_named_cache_miss_returns_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let source = DatasetSource::Named {
            alias: SwebenchAlias::Verified,
            split: SwebenchSplit::Test,
        };
        let err = resolve_dataset(&source, dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("verified"), "{msg}");
        assert!(msg.contains("test"), "{msg}");
        // error message must name the expected cache path
        assert!(
            msg.replace('\\', "/").contains("verified/test.jsonl"),
            "{msg}"
        );
    }

    #[test]
    fn resolve_dataset_named_cache_corrupt_returns_distinct_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path_for(dir.path(), &SwebenchAlias::Lite, &SwebenchSplit::Dev);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"NOT VALID JSON\n").unwrap();

        let source = DatasetSource::Named {
            alias: SwebenchAlias::Lite,
            split: SwebenchSplit::Dev,
        };
        let err = resolve_dataset(&source, dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("corrupt"), "{msg}");
    }
}
