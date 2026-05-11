//! `bench bundle`: portable, redacted, verifiable sweep archives.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod support;
use support::binary_path;

const FIXTURE_SWEEP: &str = "tests/fixtures/bundle/sweep";
const FIXED_EPOCH: &str = "1778371200";

#[test]
fn help_lists_bundle_subcommand_and_flags() {
    let out = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("bundle"), "stdout:\n{stdout}");

    let out = Command::new(binary_path())
        .args(["bench", "bundle", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in ["--sweep", "--output", "--instance", "--verify"] {
        assert!(stdout.contains(flag), "missing {flag} in:\n{stdout}");
    }
}

#[test]
fn bundle_create_verify_full_sweep_round_trips_with_fixed_layout() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    inject_absolute_source_path(&sweep);
    let archive = work.path().join("full.tar.gz");

    let out = bundle_create(&sweep, &archive, None);
    assert_success(&out);

    let entries = tar_list(&archive);
    assert_eq!(entries.last().map(String::as_str), Some("BUNDLE.json"));
    assert_eq!(
        entries,
        vec![
            "manifest.json",
            "results.json",
            "evaluation.json",
            "trajectories/alpha.traj.json",
            "trajectories/beta.traj.json",
            "patches/alpha.patch",
            "BUNDLE.json",
        ]
    );
    assert!(
        !entries.iter().any(|path| path.contains("ignored")),
        "unexpected ignored file in archive: {entries:?}"
    );

    let out = bundle_verify(&archive);
    assert_success(&out);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("bundle:ok"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let extracted = work.path().join("extracted-full");
    extract_tar(&archive, &extracted);
    let bundle: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(extracted.join("BUNDLE.json")).unwrap()).unwrap();
    assert_eq!(bundle["artifact_kind"], "bundle_manifest");
    assert_eq!(bundle["instance_scope"], "full");
    assert_eq!(bundle["source_sweep_dir"], ".");
    assert!(
        bundle["source_manifest_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    let files = bundle["files"].as_array().unwrap();
    assert_eq!(files.len(), 6);
    assert!(
        files
            .iter()
            .all(|file| file["path"].as_str() != Some("BUNDLE.json"))
    );

    assert_extracted_files_do_not_contain(&extracted, &sweep.display().to_string());
    assert_extracted_files_do_not_contain(&extracted, &json_escaped_path(&sweep));

    let out = Command::new(binary_path())
        .args(["bench", "inspect", "--sweep"])
        .arg(&extracted)
        .args(["--instance", "alpha"])
        .output()
        .unwrap();
    assert_success(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("instance_id:      alpha"), "{stdout}");
}

#[test]
fn bundle_redaction_uses_source_manifest_custom_patterns() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    inject_custom_redaction_pattern(&sweep);
    let archive = work.path().join("custom-pattern.tar.gz");
    fs::write(
        sweep.join("alpha.traj.json"),
        "post-sweep injected secret: LEGALHOLD-1234\n",
    )
    .unwrap();

    let out = bundle_create(&sweep, &archive, None);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("redaction:retrigger:trajectories/alpha.traj.json")
            || stderr.contains("redaction:retrigger:trajectories/alpha.traj.json"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!archive.exists(), "bundle command must fail closed");
}

#[test]
fn bundle_redaction_uses_standalone_manifest_custom_patterns() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    inject_standalone_manifest_redaction_pattern(&sweep);
    let archive = work.path().join("standalone-pattern.tar.gz");
    fs::write(
        sweep.join("alpha.traj.json"),
        "post-sweep injected secret: LEGALHOLD-1234\n",
    )
    .unwrap();

    let out = bundle_create(&sweep, &archive, None);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("redaction:retrigger:trajectories/alpha.traj.json")
            || stderr.contains("redaction:retrigger:trajectories/alpha.traj.json"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!archive.exists(), "bundle command must fail closed");
}

#[test]
fn bundle_normalizes_unrelated_absolute_paths_inside_artifacts() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let other_abs = if cfg!(windows) {
        r"C:\Users\markm\elsewhere\secret.txt"
    } else {
        "/var/tmp/elsewhere/secret.txt"
    };
    inject_text_into_json_string(
        &sweep.join("alpha.traj.json"),
        "/messages/0/content",
        &format!("looked at {other_abs}"),
    );
    let archive = work.path().join("paths.tar.gz");

    assert_success(&bundle_create(&sweep, &archive, None));

    let extracted = work.path().join("extracted-paths");
    extract_tar(&archive, &extracted);
    assert_extracted_files_do_not_contain(&extracted, other_abs);
    assert_extracted_files_do_not_contain(&extracted, &other_abs.replace('\\', "\\\\"));
}

#[test]
fn extracted_bundle_runs_bench_triage_without_layout_rewrite() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("triage.tar.gz");
    assert_success(&bundle_create(&sweep, &archive, None));
    let extracted = work.path().join("extracted-triage");
    extract_tar(&archive, &extracted);

    let out = Command::new(binary_path())
        .args(["bench", "triage", "--sweep"])
        .arg(&extracted)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_success(&out);
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        report["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cluster| cluster["instance_ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == "beta")),
        "{report:#}"
    );
}

#[test]
fn extracted_bundle_runs_bench_reproduce_manifest_smoke_without_layout_rewrite() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("reproduce.tar.gz");
    assert_success(&bundle_create(&sweep, &archive, None));
    let extracted = work.path().join("extracted-reproduce");
    extract_tar(&archive, &extracted);
    let output = work.path().join("replay");

    let out = Command::new(binary_path())
        .args(["bench", "reproduce", "--from"])
        .arg(&extracted)
        .arg("--output")
        .arg(&output)
        .args([
            "--allow-drift",
            "harness.git_sha",
            "--limit",
            "0",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();
    assert_success(&out);
    assert!(output.join("reproducibility.json").exists());
}

#[test]
fn extracted_bundle_reproduce_with_positive_limit_requires_real_local_dataset() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("reproduce-positive.tar.gz");
    assert_success(&bundle_create(&sweep, &archive, None));
    let extracted = work.path().join("extracted-reproduce-positive");
    extract_tar(&archive, &extracted);
    let output = work.path().join("replay-positive");

    let out = Command::new(binary_path())
        .args(["bench", "reproduce", "--from"])
        .arg(&extracted)
        .arg("--output")
        .arg(&output)
        .args([
            "--allow-drift",
            "harness.git_sha",
            "--limit",
            "1",
            "--skip-model-probe",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bundle reproduce requires the original local dataset"),
        "stderr:\n{stderr}\nstdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !output.join("bundle-reproduce.instances.jsonl").exists(),
        "positive replay must not synthesize runnable SWE-bench rows"
    );
}

#[test]
fn bundle_instance_scope_includes_only_requested_instance_artifacts() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("alpha.tar.gz");

    let out = bundle_create(&sweep, &archive, Some("alpha"));
    assert_success(&out);

    let entries = tar_list(&archive);
    assert!(entries.contains(&"trajectories/alpha.traj.json".to_owned()));
    assert!(entries.contains(&"patches/alpha.patch".to_owned()));
    assert!(
        !entries.iter().any(|path| path.contains("beta")),
        "{entries:?}"
    );

    let extracted = work.path().join("extracted-alpha");
    extract_tar(&archive, &extracted);
    let bundle: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(extracted.join("BUNDLE.json")).unwrap()).unwrap();
    assert_eq!(bundle["instance_scope"], "alpha");

    let results: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(extracted.join("results.json")).unwrap()).unwrap();
    assert_eq!(results["total"], 1);
    assert_eq!(results["completed"], 1);
    assert_eq!(results["submitted"], 1);
    assert_eq!(results["errored"], 0);
    assert_eq!(results["total_input_tokens"], 10);
    assert_eq!(results["total_completion_tokens"], 2);
    assert_eq!(results["pass_at_k"], 1.0);
    assert_eq!(results["instances"].as_array().unwrap().len(), 1);
    assert_eq!(results["instances"][0]["instance_id"], "alpha");

    let evaluation: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(extracted.join("evaluation.json")).unwrap())
            .unwrap();
    assert_eq!(evaluation["instances"].as_array().unwrap().len(), 1);
    assert_eq!(evaluation["instances"][0]["instance_id"], "alpha");
}

#[test]
fn bundle_create_is_identical_modulo_timestamp_without_fixed_epoch() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let first = work.path().join("first-live.tar.gz");
    let second = work.path().join("second-live.tar.gz");

    assert_success(&bundle_create_live_timestamp(&sweep, &first, None));
    std::thread::sleep(Duration::from_secs(1));
    assert_success(&bundle_create_live_timestamp(&sweep, &second, None));

    let first_entries = canonical_archive_entries_modulo_timestamp(&first);
    let second_entries = canonical_archive_entries_modulo_timestamp(&second);
    assert_eq!(first_entries, second_entries);
}

#[test]
fn bundle_create_is_byte_identical_with_fixed_timestamp() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let first = work.path().join("first.tar.gz");
    let second = work.path().join("second.tar.gz");

    assert_success(&bundle_create(&sweep, &first, None));
    assert_success(&bundle_create(&sweep, &second, None));

    let first_bytes = fs::read(&first).unwrap();
    let second_bytes = fs::read(&second).unwrap();
    assert_eq!(first_bytes, second_bytes);
}

#[test]
fn bundle_size_stays_within_ratio_of_explicit_file_tar() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("ratio.tar.gz");
    assert_success(&bundle_create(&sweep, &archive, None));
    let extracted = work.path().join("extracted-ratio");
    extract_tar(&archive, &extracted);

    let baseline_tar = work.path().join("explicit.tar");
    pack_explicit_files_uncompressed(&extracted, &baseline_tar);
    let bundle_size = fs::metadata(&archive).unwrap().len();
    let baseline_size = fs::metadata(&baseline_tar).unwrap().len();
    assert!(
        bundle_size.saturating_mul(10) <= baseline_size.saturating_mul(12),
        "bundle {bundle_size} should be <= 1.2x baseline tar {baseline_size}"
    );
}

#[test]
fn bundle_redaction_retrigger_fails_closed_and_writes_nothing() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("leaky.tar.gz");
    fs::write(
        sweep.join("alpha.traj.json"),
        "ghp_0123456789ABCDEF0123456789ABCDEF0123\n",
    )
    .unwrap();

    let out = bundle_create(&sweep, &archive, None);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("redaction:retrigger") || stderr.contains("redaction:retrigger"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!archive.exists(), "bundle command must fail closed");
}

#[test]
fn extracted_bundle_contains_no_known_secret_shapes() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("clean-secrets.tar.gz");
    assert_success(&bundle_create(&sweep, &archive, None));
    let extracted = work.path().join("extracted-secrets");
    extract_tar(&archive, &extracted);

    assert_no_known_secret_shapes(&extracted);
}

#[test]
fn bundle_verify_reports_duplicate_manifest_paths() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("clean.tar.gz");
    assert_success(&bundle_create(&sweep, &archive, None));

    let duplicate_dir = work.path().join("duplicate-manifest-path");
    extract_tar(&archive, &duplicate_dir);
    let bundle_path = duplicate_dir.join("BUNDLE.json");
    let mut bundle: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    let first_file = bundle["files"].as_array().unwrap()[0].clone();
    bundle["files"].as_array_mut().unwrap().push(first_file);
    fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();
    let duplicate_archive = work.path().join("duplicate-manifest-path.tar.gz");
    pack_dir(&duplicate_dir, &duplicate_archive);

    let out = bundle_verify(&duplicate_archive);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("duplicate:manifest.json"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn bundle_verify_reports_extra_missing_and_hash_mismatch() {
    let work = tempfile::tempdir().unwrap();
    let sweep = copy_fixture_sweep(work.path());
    let archive = work.path().join("clean.tar.gz");
    assert_success(&bundle_create(&sweep, &archive, None));

    let extra_dir = work.path().join("extra");
    extract_tar(&archive, &extra_dir);
    fs::write(extra_dir.join("extra.txt"), "not in BUNDLE\n").unwrap();
    let extra_archive = work.path().join("extra.tar.gz");
    pack_dir(&extra_dir, &extra_archive);
    let out = bundle_verify(&extra_archive);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("extra:extra.txt"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let missing_dir = work.path().join("missing");
    extract_tar(&archive, &missing_dir);
    fs::remove_file(missing_dir.join("patches/alpha.patch")).unwrap();
    let missing_archive = work.path().join("missing.tar.gz");
    pack_dir(&missing_dir, &missing_archive);
    let out = bundle_verify(&missing_archive);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("missing:patches/alpha.patch"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let tampered_dir = work.path().join("tampered");
    extract_tar(&archive, &tampered_dir);
    fs::write(
        tampered_dir.join("trajectories/alpha.traj.json"),
        "{\"tampered\":true}\n",
    )
    .unwrap();
    let tampered_archive = work.path().join("tampered.tar.gz");
    pack_dir(&tampered_dir, &tampered_archive);
    let out = bundle_verify(&tampered_archive);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hash_mismatch:trajectories/alpha.traj.json"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let symlink_dir = work.path().join("symlink");
    extract_tar(&archive, &symlink_dir);
    let symlink_archive = work.path().join("symlink.tar.gz");
    pack_dir_with_extra_symlink(&symlink_dir, &symlink_archive);
    let out = bundle_verify(&symlink_archive);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("extra:links/outside"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn copy_fixture_sweep(root: &Path) -> PathBuf {
    let dst = root.join("sweep");
    copy_dir(Path::new(FIXTURE_SWEEP), &dst);
    dst
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn inject_absolute_source_path(sweep: &Path) {
    let path = sweep.join("results.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value["manifest"]["dataset"]["path"] =
        serde_json::json!(sweep.join("dataset.jsonl").display().to_string());
    value["manifest"]["config"]["overlay_paths"] =
        serde_json::json!([sweep.join("config.toml").display().to_string()]);
    fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

fn inject_custom_redaction_pattern(sweep: &Path) {
    for path in [sweep.join("results.json"), sweep.join("manifest.json")] {
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        if let Some(slot) = value.pointer_mut("/manifest/config/resolved") {
            *slot = serde_json::json!(
                "[model]\nname = \"deterministic\"\n\n[redaction]\ncustom_patterns = [\"LEGALHOLD-[0-9]{4}\"]\n"
            );
        } else if let Some(slot) = value.pointer_mut("/config/resolved") {
            *slot = serde_json::json!(
                "[model]\nname = \"deterministic\"\n\n[redaction]\ncustom_patterns = [\"LEGALHOLD-[0-9]{4}\"]\n"
            );
        }
        fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }
}

fn inject_standalone_manifest_redaction_pattern(sweep: &Path) {
    let path = sweep.join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    *value.pointer_mut("/config/resolved").unwrap() = serde_json::json!(
        "[model]\nname = \"deterministic\"\n\n[redaction]\ncustom_patterns = [\"LEGALHOLD-[0-9]{4}\"]\n"
    );
    fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

fn inject_text_into_json_string(path: &Path, pointer: &str, text: &str) {
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    *value.pointer_mut(pointer).unwrap() = serde_json::json!(text);
    fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

fn json_escaped_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn bundle_create(sweep: &Path, archive: &Path, instance: Option<&str>) -> std::process::Output {
    bundle_create_with_epoch(sweep, archive, instance, Some(FIXED_EPOCH))
}

fn bundle_create_live_timestamp(
    sweep: &Path,
    archive: &Path,
    instance: Option<&str>,
) -> std::process::Output {
    bundle_create_with_epoch(sweep, archive, instance, None)
}

fn bundle_create_with_epoch(
    sweep: &Path,
    archive: &Path,
    instance: Option<&str>,
    epoch: Option<&str>,
) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.args(["bench", "bundle", "--sweep"])
        .arg(sweep)
        .arg("--output")
        .arg(archive);
    if let Some(epoch) = epoch {
        cmd.env("SOURCE_DATE_EPOCH", epoch);
    } else {
        cmd.env_remove("SOURCE_DATE_EPOCH");
    }
    if let Some(instance) = instance {
        cmd.arg("--instance").arg(instance);
    }
    cmd.output().unwrap()
}

fn bundle_verify(archive: &Path) -> std::process::Output {
    Command::new(binary_path())
        .args(["bench", "bundle", "--verify"])
        .arg(archive)
        .output()
        .unwrap()
}

fn tar_list(archive: &Path) -> Vec<String> {
    let out = Command::new("tar")
        .arg("-tzf")
        .arg(archive)
        .output()
        .unwrap();
    assert_success(&out);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(normalize_tar_path)
        .filter(|path| !path.is_empty())
        .collect()
}

fn extract_tar(archive: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dst)
        .output()
        .unwrap();
    assert_success(&out);
}

fn pack_dir(src: &Path, archive: &Path) {
    let mut files = relative_files(src);
    files.sort();
    let mut cmd = Command::new("tar");
    cmd.arg("-czf").arg(archive).arg("-C").arg(src);
    for file in files {
        cmd.arg(file);
    }
    let out = cmd.output().unwrap();
    assert_success(&out);
}

fn pack_explicit_files_uncompressed(src: &Path, archive: &Path) {
    let bundle: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(src.join("BUNDLE.json")).unwrap()).unwrap();
    let mut files: Vec<String> = bundle["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_owned())
        .collect();
    files.sort();
    let mut cmd = Command::new("tar");
    cmd.arg("-cf").arg(archive).arg("-C").arg(src);
    for file in files {
        cmd.arg(file);
    }
    let out = cmd.output().unwrap();
    assert_success(&out);
}

fn pack_dir_with_extra_symlink(src: &Path, archive: &Path) {
    let file = fs::File::create(archive).unwrap();
    let encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut files = relative_files(src);
    files.sort();
    for path in files {
        builder
            .append_path_with_name(src.join(&path), path)
            .unwrap();
    }
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_link(&mut header, "links/outside", "../outside")
        .unwrap();
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap();
}

fn canonical_archive_entries_modulo_timestamp(archive: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = archive_entries(archive);
    for (path, bytes) in &mut entries {
        if path == "BUNDLE.json" {
            let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            value["bundle_generated_at"] = serde_json::json!("1970-01-01T00:00:00Z");
            *bytes = serde_json::to_vec_pretty(&value).unwrap();
        }
    }
    entries
}

fn archive_entries(archive: &Path) -> Vec<(String, Vec<u8>)> {
    let file = fs::File::open(archive).unwrap();
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut out = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = normalize_tar_path(&entry.path().unwrap().to_string_lossy());
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        out.push((path, bytes));
    }
    out
}

fn relative_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_relative_files(root, root, &mut out);
    out
}

fn collect_relative_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            collect_relative_files(root, &path, out);
        } else {
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

fn assert_no_known_secret_shapes(root: &Path) {
    let patterns = [
        regex::Regex::new(r"gh[pousr]_[A-Za-z0-9_]{20,}").unwrap(),
        regex::Regex::new(r"github_pat_[A-Za-z0-9_]{20,}").unwrap(),
        regex::Regex::new(r"sk-[A-Za-z0-9][A-Za-z0-9_-]{16,}").unwrap(),
        regex::Regex::new(r"sk-ant-[A-Za-z0-9_-]{16,}").unwrap(),
        regex::Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
        regex::Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{16,}").unwrap(),
    ];
    for file in relative_files(root) {
        let text = fs::read_to_string(root.join(&file)).unwrap();
        for pattern in &patterns {
            assert!(
                !pattern.is_match(&text),
                "{file} matched known-secret regex {}:\n{text}",
                pattern.as_str()
            );
        }
    }
}

fn assert_extracted_files_do_not_contain(root: &Path, needle: &str) {
    if needle.is_empty() {
        return;
    }
    for file in relative_files(root) {
        let text = fs::read_to_string(root.join(&file)).unwrap();
        assert!(
            !text.contains(needle),
            "{file} contains {needle:?}:\n{text}"
        );
    }
}

fn normalize_tar_path(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned()
}

fn assert_success(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
