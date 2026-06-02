//! Integration tests for `agent redact-audit` (issue #342).
//!
//! Drives the real `max` binary against fixture sweep trees containing planted
//! secrets across every detector class, asserting recall and that raw secrets
//! never appear in any output, plus the false-positive budget on clean
//! artifacts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::pedantic)]

mod support;

use std::collections::BTreeSet;

use serde_json::Value;

/// Twenty planted secrets (two per structured detector class) used for the
/// recall metric. Each entry is `(filename, body, match_class)`.
fn planted() -> Vec<(&'static str, String, &'static str)> {
    let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAExampleKeyMaterial\n-----END RSA PRIVATE KEY-----";
    vec![
        (
            "aws1.output.txt",
            "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE".into(),
            "aws_access_key",
        ),
        (
            "aws2.output.txt",
            "key AKIA1234567890ABCDEF here".into(),
            "aws_access_key",
        ),
        (
            "gcp1.output.txt",
            "k=AIzaABCDE12345ABCDE12345ABCDE12345FGHIJ".into(),
            "gcp_api_key",
        ),
        (
            "gcp2.output.txt",
            "k=AIzaZYXWV98765ZYXWV98765ZYXWV98765QRSTU".into(),
            "gcp_api_key",
        ),
        (
            "anthropic1.output.txt",
            "k=sk-ant-api03-ABCDEFGHIJ1234567890".into(),
            "anthropic_api_key",
        ),
        (
            "anthropic2.output.txt",
            "k=sk-ant-XYZ9876543210abcdefghij".into(),
            "anthropic_api_key",
        ),
        (
            "openai1.output.txt",
            "k=sk-ABCDEFGHIJ1234567890abcd".into(),
            "openai_api_key",
        ),
        (
            "openai2.output.txt",
            "k=sk-proj-ABCDEFGHIJ1234567890wxyz".into(),
            "openai_api_key",
        ),
        (
            "hf1.output.txt",
            "k=hf_abcdefghij1234567890ABCD".into(),
            "huggingface_token",
        ),
        (
            "hf2.output.txt",
            "k=hf_ZYXWVUTSRQ0987654321zzzz".into(),
            "huggingface_token",
        ),
        (
            "gh1.output.txt",
            "t=ghp_0123456789abcdefghijklmnopqrstuvwxyz".into(),
            "github_pat",
        ),
        (
            "gh2.output.txt",
            "t=github_pat_0123456789abcdefghijABCDEFGH".into(),
            "github_pat",
        ),
        (
            "slack1.output.txt",
            "t=xoxb-123456789012-abcdefABCDEF".into(),
            "slack_token",
        ),
        (
            "slack2.output.txt",
            "t=xoxp-987654321098-ZYXWzyxw0011".into(),
            "slack_token",
        ),
        (
            "jwt1.output.txt",
            "t=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fw".into(),
            "jwt",
        ),
        (
            "jwt2.output.txt",
            "t=eyJ0eXAiOiJKV1QiLCJhbGciOi.eyJuYW1lIjoiSm9obiBEb2Ui.dQw4w9WgXcQabcdefg".into(),
            "jwt",
        ),
        ("pem1.output.txt", pem.to_owned(), "pem_private_key"),
        (
            "pem2.output.txt",
            pem.replace("RSA", "EC")
                .replace("ExampleKeyMaterial", "OtherKeyBytes"),
            "pem_private_key",
        ),
        (
            "azure1.output.txt",
            "AccountKey=YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXowMTIzNDU2Nzg5QUJDREVGRw==;".into(),
            "azure_storage_key",
        ),
        (
            "azure2.output.txt",
            "AccountKey=Zm9vYmFyMDAwMTExMjIyMzMzNDQ0NTU1NjY2Nzc3ODg5OTlhYmNkZWY=;".into(),
            "azure_storage_key",
        ),
    ]
}

fn run_json(dir: &std::path::Path, extra: &[&str]) -> (i32, Value) {
    let mut cmd = support::command();
    cmd.arg("agent").arg("redact-audit").arg(dir).arg("--json");
    for e in extra {
        cmd.arg(e);
    }
    let out = cmd.output().expect("run redact-audit");
    let code = out.status.code().expect("exit code");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("parse json (code {code}): {e}\nstdout:\n{stdout}"));
    (code, parsed)
}

#[test]
fn catches_every_planted_class_with_high_recall() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cases = planted();
    for (name, body, _) in &cases {
        std::fs::write(dir.path().join(name), body).expect("write fixture");
    }

    let (code, report) = run_json(dir.path(), &[]);
    assert_eq!(code, 32, "expected findings exit code");

    let findings = report["findings"].as_array().expect("findings array");

    // Every planted detector class must appear at least once.
    let found_classes: BTreeSet<&str> = findings
        .iter()
        .map(|f| f["match_class"].as_str().expect("match_class"))
        .collect();
    let expected: BTreeSet<&str> = cases.iter().map(|(_, _, c)| *c).collect();
    for class in &expected {
        assert!(
            found_classes.contains(class),
            "detector class missing: {class}"
        );
    }

    // Recall metric: >=95% (>=19 of 20) planted secrets caught at medium+.
    let actionable = findings
        .iter()
        .filter(|f| {
            let sev = f["severity"].as_str().unwrap_or("");
            sev == "high" || sev == "medium"
        })
        .count();
    assert!(
        actionable >= 19,
        "recall too low: only {actionable} of 20 planted secrets caught"
    );
}

#[test]
fn never_prints_raw_secret_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cases = planted();
    for (name, body, _) in &cases {
        std::fs::write(dir.path().join(name), body).expect("write fixture");
    }

    // Capture both JSON and human output and assert no raw secret survives.
    let (_, _) = run_json(dir.path(), &[]);
    let json_out =
        std::fs::read_to_string(dir.path().join("redact_audit.json")).expect("read written report");

    let human = support::command()
        .arg("agent")
        .arg("redact-audit")
        .arg(dir.path())
        .output()
        .expect("run human");
    let human_out = String::from_utf8_lossy(&human.stdout).into_owned();

    // The actual secret value in each fixture is its longest token (the key /
    // token body itself, not the surrounding label like `AWS_ACCESS_KEY_ID`,
    // which is benign context and legitimately appears in previews). That
    // longest token must never appear verbatim in any output.
    for (_, body, _) in &cases {
        let secret = body
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .max_by_key(|t| t.len())
            .unwrap_or("");
        assert!(
            secret.len() >= 16,
            "test fixture has no secret-length token"
        );
        assert!(!json_out.contains(secret), "raw secret in JSON: {secret}");
        assert!(!human_out.contains(secret), "raw secret in human: {secret}");
    }
}

#[test]
fn clean_artifacts_stay_within_false_positive_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A realistic, secret-free trajectory plus ordinary prose/output.
    std::fs::write(
        dir.path().join("hello.traj.json"),
        r#"{"schema":"mini-swe-agent-1.1","outcome":"submitted","total_cost_usd":0.0,"messages":[{"role":"user","content":"Fix the failing test in src/lib.rs and run cargo test"},{"role":"assistant","content":"I edited src/lib.rs, ran `cargo test`, and all 42 tests passed. The fix adjusted an off-by-one in the loop bound."}]}"#,
    )
    .expect("write traj");
    std::fs::write(
        dir.path().join("run.output.txt"),
        "running 42 tests\ntest result: ok. 42 passed; 0 failed; 0 ignored\nCompiling maxwells-daemon v0.1.0\n",
    )
    .expect("write output");

    let (code, report) = run_json(dir.path(), &[]);
    assert_eq!(code, 0, "clean tree must exit 0: {report}");
    assert_eq!(report["summary"]["high"], 0, "{report}");
    assert_eq!(report["summary"]["medium"], 0, "{report}");
}

#[test]
fn baseline_grandfathers_known_findings() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("leak.output.txt"), "AKIAIOSFODNN7EXAMPLE").expect("write leak");

    // First run produces a report; use it as the baseline.
    let (first_code, _) = run_json(dir.path(), &[]);
    assert_eq!(first_code, 32);
    let baseline = dir.path().join("baseline.json");
    std::fs::copy(dir.path().join("redact_audit.json"), &baseline).expect("copy baseline");

    let (second_code, report) = run_json(dir.path(), &["--baseline", baseline.to_str().unwrap()]);
    assert_eq!(second_code, 0, "no new findings should exit 0: {report}");
    assert_eq!(report["summary"]["new_findings"], 0);
}

#[test]
fn scans_gzip_bundle_members() {
    use std::io::Write as _;

    let dir = tempfile::tempdir().expect("tempdir");
    // Build a .tar.gz whose member carries a planted secret.
    let bundle_path = dir.path().join("bundle.tar.gz");
    {
        let file = std::fs::File::create(&bundle_path).expect("create bundle");
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let body = b"token=ghp_0123456789abcdefghijklmnopqrstuvwxyz\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "inner/leak.output.txt", &body[..])
            .expect("append");
        let enc = tar.into_inner().expect("finish tar");
        enc.finish().expect("finish gz").flush().expect("flush");
    }

    let (code, report) = run_json(dir.path(), &[]);
    assert_eq!(code, 32, "bundle leak should be found: {report}");
    let files: Vec<&str> = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|f| f["file"].as_str().unwrap_or(""))
        .collect();
    assert!(
        files.iter().any(|f| f.contains("bundle.tar.gz!")),
        "expected a bundle-member finding, got {files:?}"
    );
}

#[test]
fn refuses_output_overwriting_scanned_artifact() {
    // `--output <dir>/results.json` would clobber a scanned source artifact;
    // the detector-only command must refuse before mutating the sweep.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("results.json"),
        r#"{"resolved":42,"note":"all 42 tests passed"}"#,
    )
    .expect("write results.json");
    let before = std::fs::read(dir.path().join("results.json")).expect("read before");

    let out = support::command()
        .args(["agent", "redact-audit"])
        .arg(dir.path())
        .arg("--output")
        .arg(dir.path().join("results.json"))
        .output()
        .expect("run redact-audit");
    assert_eq!(out.status.code(), Some(2), "expected usage_error exit code");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("would overwrite an audited source artifact"),
        "missing guard message: {stderr}"
    );
    // The original artifact must be untouched.
    let after = std::fs::read(dir.path().join("results.json")).expect("read after");
    assert_eq!(before, after, "scanned artifact was modified");
}

#[test]
fn allows_output_outside_scanned_dir() {
    // `--output` pointing at an existing audited-looking file *outside* the
    // scanned tree is not a source artifact and must be allowed: writing it
    // cannot mutate the sweep being audited.
    let sweep = tempfile::tempdir().expect("sweep dir");
    std::fs::write(sweep.path().join("x.output.txt"), "nothing sensitive\n")
        .expect("write artifact");
    let elsewhere = tempfile::tempdir().expect("output dir");
    let out_file = elsewhere.path().join("results.json");
    std::fs::write(&out_file, "{}").expect("seed output file");

    let out = support::command()
        .args(["agent", "redact-audit"])
        .arg(sweep.path())
        .arg("--output")
        .arg(&out_file)
        .output()
        .expect("run redact-audit");
    // Clean sweep -> success; the guard must not have rejected the outside path.
    assert_eq!(
        out.status.code(),
        Some(0),
        "outside-dir --output was rejected: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written = std::fs::read_to_string(&out_file).expect("report written");
    assert!(
        written.contains("\"redact_audit\""),
        "report not written to outside path: {written}"
    );
}

#[test]
fn help_lists_redact_audit() {
    let out = support::command()
        .args(["agent", "redact-audit", "--help"])
        .output()
        .expect("run --help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--detectors"));
    assert!(stdout.contains("--baseline"));
    assert!(stdout.contains("--disable-entropy"));
}
