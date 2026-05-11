//! `bench bundle`: deterministic, redaction-strict sweep archive export.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read as _};
use std::path::{Component, Path, PathBuf};

use flate2::{Compression, GzBuilder, read::GzDecoder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::artifact::{ArtifactKind, ArtifactSchemaVersion, classify_json_value};
use crate::config::RedactionCfg;
use crate::redaction::{Redactor, surface};

pub const BUNDLE_MANIFEST_PATH: &str = "BUNDLE.json";

#[derive(Debug, Clone)]
pub struct BundleCreateArgs {
    pub sweep_dir: PathBuf,
    pub output_path: PathBuf,
    pub instance: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BundleCreateReport {
    pub output_path: PathBuf,
    pub files: Vec<BundleFileEntry>,
}

#[derive(Debug, Clone)]
pub struct BundleVerifyReport {
    pub problems: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("{0}")]
    MissingSource(String),
    #[error("redaction:retrigger:{path}")]
    RedactionRetrigger { path: String },
    #[error("{0}")]
    InvalidArchive(String),
    #[error("{0}")]
    Schema(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleFileEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    pub artifact_kind: ArtifactKind,
    pub schema_version: ArtifactSchemaVersion,
    pub source_sweep_dir: String,
    pub source_manifest_hash: String,
    pub harness_git_sha: Option<String>,
    pub bundle_generated_at: String,
    pub instance_scope: String,
    pub files: Vec<BundleFileEntry>,
}

#[derive(Debug, Clone)]
struct PreparedFile {
    archive_path: String,
    bytes: Vec<u8>,
}

struct PathNormalizer {
    needles: Vec<String>,
    windows_abs: Option<Regex>,
    unix_abs: Option<Regex>,
}

impl PathNormalizer {
    fn new(sweep_dir: &Path) -> Self {
        let mut needles = BTreeSet::new();
        push_path_needles(&mut needles, &sweep_dir.display().to_string());
        if let Ok(canonical) = std::fs::canonicalize(sweep_dir) {
            push_path_needles(&mut needles, &canonical.display().to_string());
        }
        Self {
            needles: needles
                .into_iter()
                .filter(|value| !value.is_empty() && value != ".")
                .collect(),
            windows_abs: Regex::new(r#"[A-Za-z]:(?:\\\\|\\|/)[^"'\r\n\s,}\]]+"#).ok(),
            unix_abs: Regex::new(r#"(^|[\s"'\[({:=,])/(?:[^\s/"'\]\[{}(),]+/)+[^\s/"'\]\[{}(),]+"#)
                .ok(),
        }
    }

    fn normalize_text(&self, input: &str) -> String {
        let normalized = self
            .needles
            .iter()
            .fold(input.to_owned(), |acc, needle| acc.replace(needle, "."));
        let normalized = self.windows_abs.as_ref().map_or_else(
            || normalized.clone(),
            |regex| regex.replace_all(&normalized, ".").into_owned(),
        );
        self.unix_abs.as_ref().map_or_else(
            || normalized.clone(),
            |regex| regex.replace_all(&normalized, "${1}.").into_owned(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct ResolvedConfigForBundle {
    #[serde(default)]
    redaction: Option<RedactionCfg>,
}

pub fn create_bundle(args: &BundleCreateArgs) -> Result<BundleCreateReport, BundleError> {
    if !args.sweep_dir.is_dir() {
        return Err(BundleError::MissingSource(format!(
            "bundle: sweep directory does not exist: {}",
            args.sweep_dir.display()
        )));
    }
    validate_instance_scope(args.instance.as_deref())?;

    let normalizer = PathNormalizer::new(&args.sweep_dir);
    let results_path = args.sweep_dir.join("results.json");
    let results_text = read_required_text(&results_path, "results.json")?;
    let mut results_value: serde_json::Value = serde_json::from_str(&results_text)?;
    classify_json_value(
        &results_value,
        ArtifactKind::SweepResults,
        results_path.display().to_string(),
    )
    .map_err(|err| BundleError::Schema(err.to_string()))?;

    let all_instance_ids = instance_ids_from_results(&results_value)?;
    let included_ids = included_instance_ids(&all_instance_ids, args.instance.as_deref())?;
    let included_set = included_ids.iter().cloned().collect::<BTreeSet<_>>();

    let manifest_bytes = load_manifest_bytes(&args.sweep_dir, &results_value, &normalizer)?;
    let manifest_value: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let redactor = source_redactor(&results_value, &manifest_value);
    let source_manifest_hash = format!("sha256:{}", sha256_hex(&manifest_bytes));
    strict_redaction_check("manifest.json", &manifest_bytes, &redactor)?;

    if args.instance.is_some() {
        filter_results_for_scope(&mut results_value, &included_set);
    }
    let results_bytes = normalized_json_bytes(&results_value, &normalizer)?;
    strict_redaction_check("results.json", &results_bytes, &redactor)?;

    let mut files = vec![
        PreparedFile {
            archive_path: "manifest.json".into(),
            bytes: manifest_bytes,
        },
        PreparedFile {
            archive_path: "results.json".into(),
            bytes: results_bytes,
        },
    ];

    if let Some(evaluation) = load_optional_evaluation(&args.sweep_dir, &included_set, &normalizer)?
    {
        strict_redaction_check("evaluation.json", &evaluation, &redactor)?;
        files.push(PreparedFile {
            archive_path: "evaluation.json".into(),
            bytes: evaluation,
        });
    }

    let mut patch_files = Vec::new();
    for instance_id in &included_ids {
        let trajectory_src =
            find_trajectory_path(&args.sweep_dir, instance_id).ok_or_else(|| {
                BundleError::MissingSource(format!(
                    "bundle: missing trajectory for instance `{instance_id}` in {}",
                    args.sweep_dir.display()
                ))
            })?;
        let trajectory_dest = format!("trajectories/{instance_id}.traj.json");
        let trajectory = normalized_text_file(&trajectory_src, &normalizer)?;
        strict_redaction_check(&trajectory_dest, &trajectory, &redactor)?;
        files.push(PreparedFile {
            archive_path: trajectory_dest,
            bytes: trajectory,
        });

        if let Some(patch_src) = find_patch_path(&args.sweep_dir, instance_id) {
            let patch_dest = format!("patches/{instance_id}.patch");
            let patch = normalized_text_file(&patch_src, &normalizer)?;
            strict_redaction_check(&patch_dest, &patch, &redactor)?;
            patch_files.push(PreparedFile {
                archive_path: patch_dest,
                bytes: patch,
            });
        }
    }
    files.extend(patch_files);

    let file_entries = files
        .iter()
        .map(|file| BundleFileEntry {
            path: file.archive_path.clone(),
            sha256: sha256_hex(&file.bytes),
            bytes: u64::try_from(file.bytes.len()).unwrap_or(u64::MAX),
        })
        .collect::<Vec<_>>();

    let bundle_manifest = BundleManifest {
        artifact_kind: ArtifactKind::BundleManifest,
        schema_version: ArtifactSchemaVersion::CURRENT,
        source_sweep_dir: ".".into(),
        source_manifest_hash,
        harness_git_sha: current_git_sha(),
        bundle_generated_at: bundle_generated_at()?,
        instance_scope: args.instance.clone().unwrap_or_else(|| "full".into()),
        files: file_entries.clone(),
    };
    let bundle_bytes = serde_json::to_vec_pretty(&bundle_manifest)?;

    write_archive_atomically(&args.output_path, &files, &bundle_bytes)?;

    Ok(BundleCreateReport {
        output_path: args.output_path.clone(),
        files: file_entries,
    })
}

pub fn verify_bundle(archive_path: &Path) -> Result<BundleVerifyReport, BundleError> {
    let mut actual = read_archive_files(archive_path)?;
    let mut problems = Vec::new();
    let Some(bundle_bytes) = actual.remove(BUNDLE_MANIFEST_PATH) else {
        return Ok(BundleVerifyReport {
            problems: vec![format!("missing:{BUNDLE_MANIFEST_PATH}")],
        });
    };
    let bundle: BundleManifest = serde_json::from_slice(&bundle_bytes)?;
    if bundle.artifact_kind != ArtifactKind::BundleManifest {
        return Err(BundleError::Schema(format!(
            "bundle: artifact kind mismatch: expected bundle_manifest, found {}",
            bundle.artifact_kind
        )));
    }
    if bundle.schema_version.major > ArtifactSchemaVersion::CURRENT.major {
        return Err(BundleError::Schema(format!(
            "bundle: unsupported future bundle schema {}",
            bundle.schema_version
        )));
    }

    let expected = bundle
        .files
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<BTreeMap<_, _>>();

    for (path, entry) in &expected {
        match actual.get(path) {
            Some(bytes) => {
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != entry.bytes {
                    problems.push(format!("hash_mismatch:{path}"));
                    continue;
                }
                let digest = sha256_hex(bytes);
                if digest != entry.sha256 {
                    problems.push(format!("hash_mismatch:{path}"));
                }
            }
            None => problems.push(format!("missing:{path}")),
        }
    }

    for path in actual.keys() {
        if !expected.contains_key(path) {
            problems.push(format!("extra:{path}"));
        }
    }
    problems.sort();
    Ok(BundleVerifyReport { problems })
}

fn load_manifest_bytes(
    sweep_dir: &Path,
    results_value: &serde_json::Value,
    normalizer: &PathNormalizer,
) -> Result<Vec<u8>, BundleError> {
    let manifest_path = sweep_dir.join("manifest.json");
    if manifest_path.exists() {
        return normalized_text_file(&manifest_path, normalizer);
    }
    let manifest = results_value.get("manifest").ok_or_else(|| {
        BundleError::MissingSource("bundle: results.json has no manifest block".into())
    })?;
    normalized_json_bytes(manifest, normalizer)
}

fn source_redactor(
    results_value: &serde_json::Value,
    manifest_value: &serde_json::Value,
) -> Redactor {
    let mut cfg = RedactionCfg::default();
    for source in [results_value, manifest_value] {
        for pointer in ["/manifest/config/resolved", "/config/resolved"] {
            if let Some(resolved) = source.pointer(pointer).and_then(serde_json::Value::as_str) {
                merge_resolved_redaction(&mut cfg, resolved);
            }
        }
    }
    Redactor::from_config_lossy(&cfg)
}

fn merge_resolved_redaction(cfg: &mut RedactionCfg, resolved: &str) {
    let Ok(parsed) = toml::from_str::<ResolvedConfigForBundle>(resolved) else {
        return;
    };
    let Some(redaction) = parsed.redaction else {
        return;
    };
    cfg.secret_literals.extend(
        redaction
            .secret_literals
            .into_iter()
            .filter(|value| !looks_like_redaction_marker(value)),
    );
    cfg.custom_patterns.extend(
        redaction
            .custom_patterns
            .into_iter()
            .filter(|value| !looks_like_redaction_marker(value))
            .filter(|value| Regex::new(value).is_ok()),
    );
}

fn looks_like_redaction_marker(value: &str) -> bool {
    value.starts_with("[REDACTED:")
}

fn load_optional_evaluation(
    sweep_dir: &Path,
    included: &BTreeSet<String>,
    normalizer: &PathNormalizer,
) -> Result<Option<Vec<u8>>, BundleError> {
    let path = sweep_dir.join("evaluation.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let mut value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(
        &value,
        ArtifactKind::EvaluationResults,
        path.display().to_string(),
    )
    .map_err(|err| BundleError::Schema(err.to_string()))?;
    if let Some(instances) = value
        .get_mut("instances")
        .and_then(serde_json::Value::as_array_mut)
    {
        instances.retain(|row| {
            row.get("instance_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| included.contains(id))
        });
    }
    normalized_json_bytes(&value, normalizer).map(Some)
}

fn filter_results_for_scope(value: &mut serde_json::Value, included: &BTreeSet<String>) {
    let Some((total, submitted, skipped, with_patch, patch_empty, budget_halted, failures)) = value
        .get_mut("instances")
        .and_then(serde_json::Value::as_array_mut)
        .map(|instances| {
            instances.retain(|row| {
                row.get("instance_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| included.contains(id))
            });

            let total = instances.len();
            let submitted = instances
                .iter()
                .filter(|row| string_field(row, "outcome") == Some("submitted"))
                .count();
            let skipped = instances
                .iter()
                .filter(|row| {
                    string_field(row, "outcome") == Some("skipped")
                        || string_field(row, "exit_reason") == Some("skipped")
                })
                .count();
            let with_patch = instances
                .iter()
                .filter(|row| bool_field(row, "patch_present"))
                .count();
            let patch_empty = instances
                .iter()
                .filter(|row| {
                    bool_field(row, "patch_present") && !bool_field(row, "non_empty_patch")
                })
                .count();
            let budget_halted = instances
                .iter()
                .filter(|row| string_field(row, "exit_reason") == Some("budget_halt"))
                .count();
            let mut failures = serde_json::Map::new();
            for category in instances
                .iter()
                .filter_map(|row| string_field(row, "failure_category"))
            {
                let next = failures
                    .get(category)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default()
                    .saturating_add(1);
                failures.insert(category.to_owned(), serde_json::json!(next));
            }
            (
                total,
                submitted,
                skipped,
                with_patch,
                patch_empty,
                budget_halted,
                failures,
            )
        })
    else {
        return;
    };

    if let serde_json::Value::Object(map) = value {
        map.insert("total".into(), serde_json::json!(total));
        map.insert("completed".into(), serde_json::json!(total));
        map.insert("submitted".into(), serde_json::json!(submitted));
        map.insert("skipped".into(), serde_json::json!(skipped));
        map.insert(
            "errored".into(),
            serde_json::json!(total.saturating_sub(submitted + skipped)),
        );
        map.insert("with_patch".into(), serde_json::json!(with_patch));
        map.insert("patch_empty".into(), serde_json::json!(patch_empty));
        map.insert("budget_halted".into(), serde_json::json!(budget_halted));
        map.insert(
            "failures_by_category".into(),
            serde_json::Value::Object(failures),
        );
        if let Some(filter_spec) = map
            .get_mut("filter_spec")
            .and_then(serde_json::Value::as_object_mut)
        {
            filter_spec.insert("selected_count".into(), serde_json::json!(total));
            filter_spec.insert(
                "instance_ids".into(),
                serde_json::Value::Array(
                    included
                        .iter()
                        .map(|id| serde_json::Value::String(id.clone()))
                        .collect(),
                ),
            );
        }
    }
}

fn string_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

fn bool_field(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn instance_ids_from_results(value: &serde_json::Value) -> Result<Vec<String>, BundleError> {
    let instances = value
        .get("instances")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            BundleError::MissingSource("bundle: results.json has no instances".into())
        })?;
    let mut ids = Vec::new();
    for row in instances {
        let id = row
            .get("instance_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                BundleError::MissingSource(
                    "bundle: results.json instance missing instance_id".into(),
                )
            })?;
        validate_instance_scope(Some(id))?;
        ids.push(id.to_owned());
    }
    Ok(ids)
}

fn included_instance_ids(
    all: &[String],
    requested: Option<&str>,
) -> Result<Vec<String>, BundleError> {
    match requested {
        Some(id) => {
            if all.iter().any(|candidate| candidate == id) {
                Ok(vec![id.to_owned()])
            } else {
                Err(BundleError::MissingSource(format!(
                    "bundle: instance `{id}` not found in results.json"
                )))
            }
        }
        None => Ok(all.to_vec()),
    }
}

fn validate_instance_scope(instance: Option<&str>) -> Result<(), BundleError> {
    let Some(instance) = instance else {
        return Ok(());
    };
    let invalid = instance.is_empty()
        || instance.contains('/')
        || instance.contains('\\')
        || instance.contains('\0')
        || instance == "."
        || instance == "..";
    if invalid {
        return Err(BundleError::MissingSource(format!(
            "bundle: invalid instance id for bundle layout: `{instance}`"
        )));
    }
    Ok(())
}

fn find_trajectory_path(sweep_dir: &Path, instance_id: &str) -> Option<PathBuf> {
    [
        sweep_dir.join(instance_id).join("trajectory.json"),
        sweep_dir.join(instance_id).join("run-1.traj.json"),
        sweep_dir.join(format!("{instance_id}.traj.json")),
        sweep_dir
            .join("trajectories")
            .join(format!("{instance_id}.traj.json")),
    ]
    .into_iter()
    .find(|path| path.exists())
}

fn find_patch_path(sweep_dir: &Path, instance_id: &str) -> Option<PathBuf> {
    [
        sweep_dir.join(instance_id).join("run-1.patch"),
        sweep_dir.join(format!("{instance_id}.patch")),
        sweep_dir
            .join("patches")
            .join(format!("{instance_id}.patch")),
    ]
    .into_iter()
    .find(|path| path.exists())
}

fn read_required_text(path: &Path, label: &str) -> Result<String, BundleError> {
    std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            BundleError::MissingSource(format!("bundle: missing {label}: {}", path.display()))
        } else {
            BundleError::Io(err)
        }
    })
}

fn normalized_text_file(path: &Path, normalizer: &PathNormalizer) -> Result<Vec<u8>, BundleError> {
    let text = std::fs::read_to_string(path)?;
    Ok(normalizer.normalize_text(&text).into_bytes())
}

fn normalized_json_bytes(
    value: &serde_json::Value,
    normalizer: &PathNormalizer,
) -> Result<Vec<u8>, BundleError> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(normalizer.normalize_text(&text).into_bytes())
}

fn strict_redaction_check(
    archive_path: &str,
    bytes: &[u8],
    redactor: &Redactor,
) -> Result<(), BundleError> {
    let text = std::str::from_utf8(bytes).map_err(|err| {
        BundleError::InvalidArchive(format!("bundle: {archive_path} is not UTF-8: {err}"))
    })?;
    let outcome = redactor.redact_text(text, surface::EXPORT);
    if outcome.redacted {
        return Err(BundleError::RedactionRetrigger {
            path: archive_path.to_owned(),
        });
    }
    Ok(())
}

fn write_archive_atomically(
    output_path: &Path,
    files: &[PreparedFile],
    bundle_bytes: &[u8],
) -> Result<(), BundleError> {
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = temporary_output_path(output_path);
    let result = write_archive(&tmp, files, bundle_bytes);
    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if output_path.exists() {
        std::fs::remove_file(output_path)?;
    }
    std::fs::rename(&tmp, output_path)?;
    Ok(())
}

fn write_archive(
    output_path: &Path,
    files: &[PreparedFile],
    bundle_bytes: &[u8],
) -> Result<(), BundleError> {
    let file = std::fs::File::create(output_path)?;
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for file in files {
        append_tar_file(&mut builder, &file.archive_path, &file.bytes)?;
    }
    append_tar_file(&mut builder, BUNDLE_MANIFEST_PATH, bundle_bytes)?;
    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(())
}

fn append_tar_file<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    archive_path: &str,
    bytes: &[u8],
) -> Result<(), BundleError> {
    validate_archive_path(archive_path)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder.append_data(&mut header, archive_path, &mut Cursor::new(bytes))?;
    Ok(())
}

fn temporary_output_path(output_path: &Path) -> PathBuf {
    let pid = std::process::id();
    let file_name = output_path
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "bundle.tar.gz".to_owned(), ToOwned::to_owned);
    output_path.with_file_name(format!(".{file_name}.{pid}.tmp"))
}

fn read_archive_files(archive_path: &Path) -> Result<BTreeMap<String, Vec<u8>>, BundleError> {
    let file = std::fs::File::open(archive_path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut out = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let raw_path = entry.path()?;
        let path = normalize_archive_path(&raw_path)?;
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        if out.insert(path.clone(), bytes).is_some() {
            return Err(BundleError::InvalidArchive(format!(
                "bundle: duplicate archive entry {path}"
            )));
        }
    }
    Ok(out)
}

fn validate_archive_path(path: &str) -> Result<(), BundleError> {
    let normalized = normalize_archive_path(Path::new(path))?;
    if normalized != path {
        return Err(BundleError::InvalidArchive(format!(
            "bundle: non-normalized archive path `{path}`"
        )));
    }
    Ok(())
}

fn normalize_archive_path(path: &Path) -> Result<String, BundleError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| {
                    BundleError::InvalidArchive("bundle: archive path is not UTF-8".into())
                })?;
                parts.push(text.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(BundleError::InvalidArchive(format!(
                    "bundle: unsafe archive path `{}`",
                    path.display()
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(BundleError::InvalidArchive(
            "bundle: empty archive path".into(),
        ));
    }
    Ok(parts.join("/"))
}

fn bundle_generated_at() -> Result<String, BundleError> {
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
        let seconds = epoch.parse::<i64>().map_err(|err| {
            BundleError::MissingSource(format!("bundle: invalid SOURCE_DATE_EPOCH: {err}"))
        })?;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0).ok_or_else(|| {
            BundleError::MissingSource("bundle: SOURCE_DATE_EPOCH is out of range".into())
        })?;
        return Ok(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    Ok(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn current_git_sha() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let sha = text.trim();
    (!sha.is_empty()).then(|| sha.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn push_path_needles(out: &mut BTreeSet<String>, raw: &str) {
    if raw.is_empty() {
        return;
    }
    let slash = raw.replace('\\', "/");
    let backslash = raw.replace('/', "\\");
    for value in [raw.to_owned(), slash, backslash] {
        out.insert(value.clone());
        out.insert(value.replace('\\', "\\\\"));
    }
}
