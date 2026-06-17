//! Integration tests for `agent fs-audit` — filesystem boundary audit (issue #511).
//!
//! RED PHASE: These tests are written before the implementation and are expected
//! to fail until `src/run/fs_audit.rs` is implemented.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use maxwells_daemon::exit_code::ExitCode;
use maxwells_daemon::run::fs_audit::{
    AccessKind, FsAuditFormat, FsAuditOpts, FsAuditSource, run_fs_audit,
};

fn sweep_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fs_audit")
        .join(name)
}

fn traj_fixture(sweep: &str, name: &str) -> PathBuf {
    sweep_fixture(sweep).join(name)
}

// ── exit code constants ────────────────────────────────────────────────────────

#[test]
fn fs_audit_findings_exit_code_is_45() {
    assert_eq!(ExitCode::FsAuditFindings.as_i32(), 45);
    assert_eq!(
        ExitCode::FsAuditFindings.outcome_class(),
        "fs_audit_findings"
    );
}

#[test]
fn fs_audit_scan_error_exit_code_is_46() {
    assert_eq!(ExitCode::FsAuditScanError.as_i32(), 46);
    assert_eq!(
        ExitCode::FsAuditScanError.outcome_class(),
        "fs_audit_scan_error"
    );
}

// ── clean sweep (zero false positives) ───────────────────────────────────────

#[test]
fn clean_sweep_produces_no_findings() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Sweep(sweep_fixture("clean_sweep")),
        workdir_override: None,
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("clean sweep should scan without error");
    assert_eq!(
        report.total_findings, 0,
        "clean sweep must produce zero findings but got: {:?}",
        report.findings
    );
    assert!(
        report.scan_errors.is_empty(),
        "clean sweep must produce zero scan errors"
    );
    assert_eq!(
        report.exit_code(),
        ExitCode::Success,
        "clean sweep exit code must be 0"
    );
}

#[test]
fn clean_sweep_scans_both_trajectories() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Sweep(sweep_fixture("clean_sweep")),
        workdir_override: None,
        allow: vec![],
        format: FsAuditFormat::Text,
    };
    let report = run_fs_audit(&opts).expect("clean sweep should scan without error");
    assert_eq!(
        report.trajectories_scanned, 2,
        "clean sweep has 2 trajectory files"
    );
}

// ── dirty sweep (100% recall) ────────────────────────────────────────────────

#[test]
fn etc_access_trajectory_is_flagged() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "etc-access.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    assert!(
        report.total_findings >= 1,
        "cat /etc/passwd must produce at least one finding"
    );
    let finding = &report.findings[0];
    assert_eq!(finding.instance_id, "etc-access");
    assert!(
        finding.matched_path.contains("/etc"),
        "finding must reference /etc path, got: {}",
        finding.matched_path
    );
    assert_eq!(finding.access, AccessKind::Read, "cat is a read command");
    assert_eq!(
        report.exit_code(),
        ExitCode::FsAuditFindings,
        "non-zero exit when findings present"
    );
}

#[test]
fn home_access_trajectory_is_flagged() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "home-access.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    assert!(
        report.total_findings >= 1,
        "$HOME/.ssh/id_rsa must produce at least one finding"
    );
    let finding = &report.findings[0];
    assert!(
        finding.matched_path.contains("HOME") || finding.matched_path.contains(".ssh"),
        "finding must reference home path, got: {}",
        finding.matched_path
    );
}

#[test]
fn dotdot_traversal_trajectory_is_flagged() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture(
            "dirty_sweep",
            "traversal-access.traj.json",
        )),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    assert!(
        report.total_findings >= 1,
        "../../etc/shadow must produce at least one finding"
    );
    let finding = &report.findings[0];
    assert!(
        finding.matched_path.contains(".."),
        "finding must reference .. traversal, got: {}",
        finding.matched_path
    );
}

#[test]
fn tmp_write_trajectory_is_flagged() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "tmp-write.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    assert!(
        report.total_findings >= 1,
        "/tmp/leak.txt must produce at least one finding"
    );
    let finding = report
        .findings
        .iter()
        .find(|f| f.matched_path.contains("/tmp"))
        .expect("must have a finding for /tmp path");
    assert!(
        matches!(finding.access, AccessKind::Write | AccessKind::Ambiguous),
        "write to /tmp should be classified as write or ambiguous, got: {:?}",
        finding.access
    );
}

#[test]
fn multi_step_trajectory_flags_both_violations() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "multi-step.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    assert!(
        report.total_findings >= 2,
        "multi-step trajectory has 2 out-of-workdir accesses (/usr and /var), got {} findings",
        report.total_findings
    );
}

#[test]
fn dirty_sweep_flags_all_planted_violations() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Sweep(sweep_fixture("dirty_sweep")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    // dirty_sweep has 5 trajectory files, each with at least 1 violation
    assert!(
        report.total_findings >= 5,
        "dirty sweep must flag all K=5 planted accesses (recall=1.0), got {} findings",
        report.total_findings
    );
    assert_eq!(
        report.exit_code(),
        ExitCode::FsAuditFindings,
        "dirty sweep exit code must be 45"
    );
}

// ── step index reporting ──────────────────────────────────────────────────────

#[test]
fn finding_reports_correct_step_index() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "etc-access.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    // In etc-access.traj.json, the bash action is the first assistant message (step 0)
    let finding = &report.findings[0];
    assert_eq!(finding.step_index, 0, "first assistant message is step 0");
}

#[test]
fn finding_reports_command_head() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "etc-access.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    let finding = &report.findings[0];
    assert_eq!(finding.command_head, "cat", "command head must be 'cat'");
}

// ── workdir resolution ────────────────────────────────────────────────────────

#[test]
fn workdir_resolved_from_trajectory_info() {
    // etc-access.traj.json has local_workdir = "/repo" in info
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "etc-access.traj.json")),
        workdir_override: None, // no override — must come from trajectory
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    assert!(
        report.total_findings >= 1,
        "workdir from trajectory info must be used when no --workdir override"
    );
    assert_eq!(
        report.workdir, "/repo",
        "workdir in report must match trajectory info"
    );
}

#[test]
fn workdir_override_takes_precedence() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "etc-access.traj.json")),
        workdir_override: Some(PathBuf::from("/workspace")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    assert_eq!(
        report.workdir, "/workspace",
        "--workdir override must appear in report"
    );
}

// ── allowlist suppression ────────────────────────────────────────────────────

#[test]
fn allowlist_suppresses_matching_findings() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "etc-access.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        // Allow /etc so that /etc/passwd is suppressed
        allow: vec![PathBuf::from("/etc")],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    assert_eq!(
        report.total_findings, 0,
        "allowlisted /etc must suppress the /etc/passwd finding"
    );
    assert_eq!(
        report.exit_code(),
        ExitCode::Success,
        "allowlisted finding must not trigger non-zero exit"
    );
}

#[test]
fn allowlist_suppresses_only_matching_paths() {
    // Allow /etc but not /var — multi-step has both /usr and /var violations
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "multi-step.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![PathBuf::from("/usr")],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    // /usr is suppressed, /var is not; must still have at least 1 finding
    assert!(
        report.total_findings >= 1,
        "only /usr is allowlisted; /var finding must remain"
    );
}

// ── format output ─────────────────────────────────────────────────────────────

#[test]
fn json_output_has_required_fields() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Sweep(sweep_fixture("clean_sweep")),
        workdir_override: None,
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    let json = maxwells_daemon::run::fs_audit::format_json(&report)
        .expect("JSON serialization must succeed");
    let obj = json.as_object().expect("JSON output must be an object");
    assert!(obj.contains_key("artifact_kind"), "must have artifact_kind");
    assert!(
        obj.contains_key("schema_version"),
        "must have schema_version"
    );
    assert!(obj.contains_key("source"), "must have source");
    assert!(obj.contains_key("workdir"), "must have workdir");
    assert!(
        obj.contains_key("trajectories_scanned"),
        "must have trajectories_scanned"
    );
    assert!(
        obj.contains_key("total_findings"),
        "must have total_findings"
    );
    assert!(obj.contains_key("findings"), "must have findings");
    assert!(obj.contains_key("scan_errors"), "must have scan_errors");
    assert_eq!(
        obj["artifact_kind"].as_str(),
        Some("fs_audit"),
        "artifact_kind must be 'fs_audit'"
    );
}

#[test]
fn json_schema_version_is_1_0() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Sweep(sweep_fixture("clean_sweep")),
        workdir_override: None,
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    let json = maxwells_daemon::run::fs_audit::format_json(&report)
        .expect("JSON serialization must succeed");
    let sv = &json["schema_version"];
    assert_eq!(
        sv["major"].as_i64(),
        Some(1),
        "schema_version.major must be 1"
    );
    assert_eq!(
        sv["minor"].as_i64(),
        Some(0),
        "schema_version.minor must be 0"
    );
}

#[test]
fn json_finding_has_all_required_fields() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "etc-access.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    let json = maxwells_daemon::run::fs_audit::format_json(&report)
        .expect("JSON serialization must succeed");
    let findings = json["findings"].as_array().expect("findings must be array");
    assert!(!findings.is_empty(), "must have at least one finding");
    let f = &findings[0];
    assert!(
        f["instance_id"].is_string(),
        "finding must have instance_id"
    );
    assert!(f["step_index"].is_number(), "finding must have step_index");
    assert!(
        f["command_head"].is_string(),
        "finding must have command_head"
    );
    assert!(
        f["matched_path"].is_string(),
        "finding must have matched_path"
    );
    assert!(f["access"].is_string(), "finding must have access");
}

#[test]
fn text_format_includes_summary_line() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Sweep(sweep_fixture("clean_sweep")),
        workdir_override: None,
        allow: vec![],
        format: FsAuditFormat::Text,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    let text = maxwells_daemon::run::fs_audit::format_text(&report);
    assert!(
        text.contains("fs-audit"),
        "text output must contain 'fs-audit'"
    );
    assert!(
        text.contains("trajectories"),
        "text output must mention trajectories scanned"
    );
}

#[test]
fn text_format_includes_findings_when_present() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(traj_fixture("dirty_sweep", "etc-access.traj.json")),
        workdir_override: Some(PathBuf::from("/repo")),
        allow: vec![],
        format: FsAuditFormat::Text,
    };
    let report = run_fs_audit(&opts).expect("scan should succeed");
    let text = maxwells_daemon::run::fs_audit::format_text(&report);
    assert!(
        text.contains("etc-access") || text.contains("/etc"),
        "text output must reference the finding instance or path"
    );
}

// ── scan error handling ───────────────────────────────────────────────────────

#[test]
fn missing_sweep_dir_is_error() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Sweep(PathBuf::from("/nonexistent/sweep/dir")),
        workdir_override: None,
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let result = run_fs_audit(&opts);
    assert!(result.is_err(), "missing sweep directory must return Err");
}

#[test]
fn missing_trajectory_file_is_error() {
    let opts = FsAuditOpts {
        source: FsAuditSource::Trajectory(PathBuf::from("/nonexistent/traj.json")),
        workdir_override: None,
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    let result = run_fs_audit(&opts);
    assert!(result.is_err(), "missing trajectory file must return Err");
}

#[test]
fn scan_error_exit_code_when_no_findings() {
    // A report with scan errors but no findings returns ScanError exit code
    let report = maxwells_daemon::run::fs_audit::FsAuditReport {
        artifact_kind: "fs_audit".to_owned(),
        schema_version: maxwells_daemon::artifact::ArtifactSchemaVersion::new(1, 0),
        source: "./sweep".to_owned(),
        workdir: "/repo".to_owned(),
        trajectories_scanned: 0,
        total_findings: 0,
        findings: vec![],
        scan_errors: vec!["bad.traj.json: parse error".to_owned()],
    };
    assert_eq!(
        report.exit_code(),
        ExitCode::FsAuditScanError,
        "scan errors with no findings must produce FsAuditScanError"
    );
}

#[test]
fn findings_take_precedence_over_scan_errors() {
    use maxwells_daemon::run::fs_audit::{AccessKind, FsAuditFinding};
    let report = maxwells_daemon::run::fs_audit::FsAuditReport {
        artifact_kind: "fs_audit".to_owned(),
        schema_version: maxwells_daemon::artifact::ArtifactSchemaVersion::new(1, 0),
        source: "./sweep".to_owned(),
        workdir: "/repo".to_owned(),
        trajectories_scanned: 1,
        total_findings: 1,
        findings: vec![FsAuditFinding {
            instance_id: "inst1".to_owned(),
            step_index: 0,
            command_head: "cat".to_owned(),
            matched_path: "/etc/passwd".to_owned(),
            access: AccessKind::Read,
        }],
        scan_errors: vec!["bad.traj.json: parse error".to_owned()],
    };
    assert_eq!(
        report.exit_code(),
        ExitCode::FsAuditFindings,
        "findings take precedence over scan errors (exits 45, not 46)"
    );
}

// ── zero-cost guarantee ────────────────────────────────────────────────────────

#[test]
fn run_fs_audit_is_read_only_no_files_created() {
    let sweep_dir = sweep_fixture("clean_sweep");
    let entries_before: Vec<_> = std::fs::read_dir(&sweep_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    let opts = FsAuditOpts {
        source: FsAuditSource::Sweep(sweep_dir.clone()),
        workdir_override: None,
        allow: vec![],
        format: FsAuditFormat::Json,
    };
    run_fs_audit(&opts).expect("scan should succeed");

    let entries_after: Vec<_> = std::fs::read_dir(&sweep_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    assert_eq!(
        entries_before.len(),
        entries_after.len(),
        "fs-audit must not write any files to the sweep directory"
    );
}
