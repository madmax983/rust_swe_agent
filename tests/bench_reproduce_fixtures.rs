//! CI fixture tests for `bench reproduce` — prove manifest parsing and drift
//! detection work against checked-in sweep directories, without spending any
//! model credits (`--skip-model-probe` equivalent logic tested here via the
//! public API directly).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rust_swe_agent::run::reproduce::{DriftSeverity, compare_manifests, load_manifest_from_sweep};

const CURRENT_FIXTURE: &str = "tests/fixtures/reproduce/current_sweep";
const LEGACY_FIXTURE: &str = "tests/fixtures/reproduce/legacy_sweep";

#[test]
fn current_fixture_manifest_parses_successfully() {
    let manifest = load_manifest_from_sweep(std::path::Path::new(CURRENT_FIXTURE)).unwrap();
    assert_eq!(manifest.model.name, "deterministic");
    assert_eq!(
        manifest.harness.git_sha.as_deref(),
        Some("fixture-sha-current")
    );
    assert_eq!(manifest.dataset.sha256, "fixture-dataset-hash");
}

#[test]
fn legacy_fixture_manifest_parses_successfully() {
    let manifest = load_manifest_from_sweep(std::path::Path::new(LEGACY_FIXTURE)).unwrap();
    assert_eq!(manifest.model.name, "deterministic");
    assert_eq!(
        manifest.harness.git_sha.as_deref(),
        Some("fixture-sha-legacy")
    );
}

#[test]
fn drift_detection_identifies_sha_difference_between_fixtures() {
    let current = load_manifest_from_sweep(std::path::Path::new(CURRENT_FIXTURE)).unwrap();
    let legacy = load_manifest_from_sweep(std::path::Path::new(LEGACY_FIXTURE)).unwrap();

    let drifts = compare_manifests(&legacy, &current);

    // The two fixtures have different git SHAs → hard drift expected.
    let sha_drift = drifts
        .iter()
        .find(|d| d.field == "harness.git_sha")
        .expect("expected harness.git_sha drift between fixtures");

    assert_eq!(sha_drift.severity, DriftSeverity::Hard);
    assert!(sha_drift.message.contains("fixture-sha-legacy"));
    assert!(sha_drift.message.contains("fixture-sha-current"));
}

#[test]
fn same_fixture_produces_no_drift() {
    let m = load_manifest_from_sweep(std::path::Path::new(CURRENT_FIXTURE)).unwrap();
    let drifts = compare_manifests(&m, &m);
    assert!(
        drifts.is_empty(),
        "identical manifests must produce no drift: {drifts:?}"
    );
}
