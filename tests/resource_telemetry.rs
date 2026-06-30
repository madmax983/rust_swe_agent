//! Tests for per-run peak memory and CPU usage telemetry (issue #546).
//!
//! These tests cover:
//! - TrajectoryInfo fields peak_memory_bytes and cpu_seconds
//! - InstanceResult propagation of resource fields
//! - SweepResults sweep-level rollup (max/median peak_memory_bytes, total cpu_seconds)
//! - resource::measure() not panicking on any platform
//! - Schema version bump to 1.13

#![allow(clippy::expect_used, clippy::unwrap_used)]

use maxwells_daemon::artifact::ArtifactSchemaVersion;
use maxwells_daemon::run::swebench::{InstanceResult, SweepResults};
use maxwells_daemon::trajectory::TrajectoryInfo;

// ---- TrajectoryInfo field tests ----

#[test]
fn trajectory_info_deserializes_peak_memory_bytes() {
    let json = serde_json::json!({ "peak_memory_bytes": 524_288_000_u64 });
    let info: TrajectoryInfo = serde_json::from_value(json).unwrap();
    assert_eq!(info.peak_memory_bytes, Some(524_288_000));
}

#[test]
fn trajectory_info_deserializes_cpu_seconds() {
    let json = serde_json::json!({ "cpu_seconds": 3.75_f64 });
    let info: TrajectoryInfo = serde_json::from_value(json).unwrap();
    assert_eq!(info.cpu_seconds, Some(3.75));
}

#[test]
fn trajectory_info_defaults_resource_fields_to_none() {
    let info: TrajectoryInfo = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(info.peak_memory_bytes, None);
    assert_eq!(info.cpu_seconds, None);
}

#[test]
fn trajectory_info_omits_null_resource_fields_from_json() {
    let info = TrajectoryInfo::default();
    assert!(info.peak_memory_bytes.is_none());
    assert!(info.cpu_seconds.is_none());

    let json = serde_json::to_string(&info).unwrap();
    assert!(
        !json.contains("peak_memory_bytes"),
        "null peak_memory_bytes must be omitted from JSON: {json}"
    );
    assert!(
        !json.contains("cpu_seconds"),
        "null cpu_seconds must be omitted from JSON: {json}"
    );
}

#[test]
fn trajectory_info_serializes_resource_fields_when_present() {
    let mut info = TrajectoryInfo::default();
    info.peak_memory_bytes = Some(268_435_456);
    info.cpu_seconds = Some(2.5);

    let json = serde_json::to_string(&info).unwrap();
    assert!(
        json.contains("peak_memory_bytes"),
        "peak_memory_bytes must appear when Some: {json}"
    );
    assert!(
        json.contains("cpu_seconds"),
        "cpu_seconds must appear when Some: {json}"
    );

    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["peak_memory_bytes"], 268_435_456_u64);
    assert!((value["cpu_seconds"].as_f64().unwrap() - 2.5).abs() < f64::EPSILON);
}

// ---- InstanceResult field tests ----

#[test]
fn instance_result_deserializes_resource_fields() {
    let json = serde_json::json!({
        "instance_id": "task-a",
        "exit_reason": "submitted",
        "peak_memory_bytes": 104_857_600_u64,
        "cpu_seconds": 1.2_f64,
    });
    let ir: InstanceResult = serde_json::from_value(json).unwrap();
    assert_eq!(ir.peak_memory_bytes, Some(104_857_600));
    assert_eq!(ir.cpu_seconds, Some(1.2));
}

#[test]
fn instance_result_defaults_resource_fields_to_none() {
    let json = serde_json::json!({
        "instance_id": "task-a",
        "exit_reason": "submitted",
    });
    let ir: InstanceResult = serde_json::from_value(json).unwrap();
    assert_eq!(ir.peak_memory_bytes, None);
    assert_eq!(ir.cpu_seconds, None);
}

// ---- SweepResults rollup field tests ----

#[test]
fn sweep_results_deserializes_resource_rollup_fields() {
    let json = serde_json::json!({
        "total": 2,
        "submitted": 2,
        "skipped": 0,
        "errored": 0,
        "total_prompt_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "instances": [],
        "max_peak_memory_bytes": 536_870_912_u64,
        "median_peak_memory_bytes": 268_435_456_u64,
        "total_cpu_seconds": 10.5_f64,
    });
    let results: SweepResults = serde_json::from_value(json).unwrap();
    assert_eq!(results.max_peak_memory_bytes, Some(536_870_912));
    assert_eq!(results.median_peak_memory_bytes, Some(268_435_456));
    assert_eq!(results.total_cpu_seconds, Some(10.5));
}

#[test]
fn sweep_results_defaults_resource_rollup_to_none() {
    let json = serde_json::json!({
        "total": 0,
        "submitted": 0,
        "skipped": 0,
        "errored": 0,
        "total_prompt_tokens": 0,
        "total_completion_tokens": 0,
        "estimated_cost_usd": 0.0,
        "instances": [],
    });
    let results: SweepResults = serde_json::from_value(json).unwrap();
    assert_eq!(results.max_peak_memory_bytes, None);
    assert_eq!(results.median_peak_memory_bytes, None);
    assert_eq!(results.total_cpu_seconds, None);
}

// ---- resource::measure() smoke test ----

#[test]
fn resource_measure_does_not_panic() {
    let usage = maxwells_daemon::resource::measure();
    // On Linux the values may be Some or None depending on the environment.
    // On non-Linux they are always None.
    // The critical invariant: this must never panic.
    let _ = usage.peak_memory_bytes;
    let _ = usage.cpu_seconds;
}

#[test]
fn resource_measure_never_returns_fabricated_zero_for_memory() {
    // peak_memory_bytes=0 would mean a process with zero RSS, which is
    // physically impossible. Any successful measurement must be > 0.
    let usage = maxwells_daemon::resource::measure();
    if let Some(bytes) = usage.peak_memory_bytes {
        assert!(
            bytes > 0,
            "peak_memory_bytes must not be a fabricated 0; got {bytes}"
        );
    }
    // If None, that's correct graceful degradation — no assertion needed.
}

// ---- Schema version test ----

#[test]
fn schema_version_is_1_13_for_resource_telemetry() {
    // Schema 1.13 adds peak_memory_bytes and cpu_seconds to TrajectoryInfo
    // and the corresponding rollup fields to SweepResults (issue #546).
    assert_eq!(
        ArtifactSchemaVersion::CURRENT,
        ArtifactSchemaVersion::new(1, 13),
        "Schema version must be 1.13 after adding resource telemetry fields"
    );
}
