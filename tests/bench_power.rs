//! Integration and mathematical correctness tests for `bench power`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use serde_json::Value;
use std::process::Command;

mod support;
use support::binary_path;

// ── CLI integration tests ─────────────────────────────────────────────────────

fn run_power(extra_args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(binary_path());
    cmd.args(["--log", "error", "bench", "power"]);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.output().unwrap()
}

fn run_power_json(extra_args: &[&str]) -> Value {
    let mut args = extra_args.to_vec();
    args.push("--format");
    args.push("json");
    let output = run_power(&args);
    assert!(
        output.status.success(),
        "bench power failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_help_shows_power_subcommand() {
    let mut cmd = Command::new(binary_path());
    cmd.args(["bench", "power", "--help"]);
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Calculate statistical power"));
}

#[test]
fn cli_fails_when_both_n_and_delta_omitted() {
    // Both N and delta are omitted -> invalid/incomplete input
    let output = run_power(&["--baseline-rate", "0.50"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Usage") || stderr.contains("error") || stderr.contains("inconsistent")
    );
}

#[test]
fn cli_fails_with_invalid_baseline_rate() {
    // baseline rate > 1.0
    let output = run_power(&["--baseline-rate", "1.5", "--delta", "0.05"]);
    assert!(!output.status.success());
}

#[test]
fn cli_fails_with_invalid_alpha() {
    // alpha > 1.0
    let output = run_power(&[
        "--baseline-rate",
        "0.5",
        "--delta",
        "0.05",
        "--alpha",
        "1.5",
    ]);
    assert!(!output.status.success());
}

#[test]
fn cli_fails_with_invalid_power() {
    // power > 1.0
    let output = run_power(&[
        "--baseline-rate",
        "0.5",
        "--delta",
        "0.05",
        "--power",
        "1.5",
    ]);
    assert!(!output.status.success());
}

#[test]
fn cli_mode_a_sample_size_reference_1() {
    // Cohen's h = 0.20, alpha = 0.05, power = 0.80 -> N = 393 per arm (approx)
    // To match statsmodels exactly: p1 = 0.5, h = 0.20 -> p2 = 0.59828 (or let's specify delta / p2 directly)
    // If using precise delta: baseline_rate=0.50, delta=0.10, N=388 (exactly 388 per arm)
    let report = run_power_json(&["--baseline-rate", "0.50", "--delta", "0.10"]);

    assert_eq!(report["solved_n"].as_u64().unwrap(), 388);
}

#[test]
fn cli_fails_when_power_below_alpha() {
    // Under null, power is already alpha (0.05). Targeting 0.01 <= 0.05 is rejected as invalid.
    let output = run_power(&[
        "--baseline-rate",
        "0.50",
        "--n",
        "100",
        "--alpha",
        "0.05",
        "--power",
        "0.01",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("strictly greater than"));
}

#[test]
fn cli_mode_a_sample_size_native_no_override() {
    // Pure mathematical result: baseline_rate=0.20, delta=0.05, solves to N = 1092 per arm
    let report = run_power_json(&["--baseline-rate", "0.20", "--delta", "0.05"]);
    assert_eq!(report["solved_n"].as_u64().unwrap(), 1092);
}

#[test]
fn cli_mode_b_mde_reference() {
    // Given N = 388, baseline_rate = 0.50, alpha = 0.05, power = 0.80 -> MDE = 0.10 (to 4 decimal places)
    let report = run_power_json(&["--baseline-rate", "0.50", "--n", "388"]);
    let solved_mde = report["solved_mde"].as_f64().unwrap();
    assert!(
        (solved_mde - 0.10).abs() < 1e-3,
        "expected ~0.10, got {solved_mde}"
    );
}

#[test]
fn cli_bonferroni_note_shows_for_multiple_arms() {
    let report = run_power_json(&["--baseline-rate", "0.50", "--delta", "0.10", "--arms", "3"]);
    let bonferroni = report["bonferroni_note"]
        .as_str()
        .expect("expected Bonferroni note");
    assert!(bonferroni.contains("arms=3") || bonferroni.contains("Bonferroni"));
}

#[test]
fn cli_one_sided_mode_native_no_override() {
    // Pure mathematical result: baseline=0.50, delta=0.10, one-sided -> N = 305
    let report = run_power_json(&["--baseline-rate", "0.50", "--delta", "0.10", "--one-sided"]);
    assert_eq!(report["solved_n"].as_u64().unwrap(), 305);
}

#[test]
fn cli_cost_per_instance_calculates_total_cost() {
    // 388 per arm, 2 arms -> 776 instances. $2.50 per instance -> $1940.00
    let report = run_power_json(&[
        "--baseline-rate",
        "0.50",
        "--delta",
        "0.10",
        "--cost-per-instance",
        "2.50",
    ]);
    assert_eq!(report["total_cost"].as_f64().unwrap(), 1940.0);
}

#[test]
fn cli_fails_with_nan_inputs() {
    // alpha NaN
    let output = run_power(&["--baseline-rate", "0.5", "--delta", "0.1", "--alpha", "NaN"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Usage") || stderr.contains("alpha"));

    // power NaN
    let output = run_power(&["--baseline-rate", "0.5", "--delta", "0.1", "--power", "NaN"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Usage") || stderr.contains("power"));

    // cost-per-instance NaN
    let output = run_power(&[
        "--baseline-rate",
        "0.5",
        "--delta",
        "0.1",
        "--cost-per-instance",
        "NaN",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Usage") || stderr.contains("Cost"));
}

#[test]
fn cli_fails_with_impossible_delta() {
    // baseline 0.9, delta 0.95 -> p2 would be -0.05 or 1.85 (both outside [0, 1])
    let output = run_power(&["--baseline-rate", "0.9", "--delta", "0.95"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Impossible") || stderr.contains("delta"));
}

#[test]
fn cli_fails_with_under_resolved_delta() {
    // delta too small, collapses to zero effect
    let output = run_power(&["--baseline-rate", "0.5", "--delta", "1e-18"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("too small") || stderr.contains("delta") || stderr.contains("effect"));
}

#[test]
fn cli_fails_with_extreme_precision_alpha() {
    // Extremely small alpha that would cause infinity in inverse_phi
    let output = run_power(&[
        "--baseline-rate",
        "0.5",
        "--delta",
        "0.1",
        "--alpha",
        "5e-324",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("too extreme") || stderr.contains("precision"));
}

#[test]
fn cli_fails_with_mode_b_infeasibility() {
    // With small sample size, required h is so large it's impossible to resolve from baseline_rate
    let output = run_power(&["--baseline-rate", "0.01", "--n", "1"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Infeasible") || stderr.contains("Cohen's h"));
}

#[test]
fn cli_mode_a_conservative_delta_mapping() {
    // In Mode A, we want to ensure we compute the sample size corresponding to the more conservative direction
    // For baseline_rate=0.20, delta=0.05:
    // Direction +delta (0.25) yields smaller h (0.12) -> larger N (1092)
    // Direction -delta (0.15) yields larger h (0.132) -> smaller N (903)
    // So the solver must output 1092 per arm.
    let report = run_power_json(&["--baseline-rate", "0.20", "--delta", "0.05"]);
    assert_eq!(report["solved_n"].as_u64().unwrap(), 1092);
}

#[test]
fn cli_mode_b_conservative_mde_reporting() {
    // In Mode B, solving for MDE at baseline 0.20 with N = 1092
    // Since N = 1092 satisfies BOTH directions, we should report the larger (conservative) absolute delta MDE.
    // Let's verify that the output resolved MDE is around 0.05 (specifically 0.05 or slightly larger).
    let report = run_power_json(&["--baseline-rate", "0.20", "--n", "1092"]);
    let solved_mde = report["solved_mde"].as_f64().unwrap();
    assert!(
        (solved_mde - 0.05).abs() < 1e-2,
        "expected around 0.05, got {solved_mde}"
    );
}

#[test]
fn cli_fails_with_malformed_forecast_target_n() {
    use std::fs::write;
    let temp_dir = std::env::temp_dir();
    let file_path = temp_dir.join("malformed_forecast_target_n.json");

    // target_n is negative
    write(
        &file_path,
        r#"{"forecast": {"target_n": -10.0, "total_cost_usd": {"point": 100.0}}}"#,
    )
    .unwrap();
    let output = run_power(&[
        "--baseline-rate",
        "0.5",
        "--delta",
        "0.1",
        "--from-forecast",
        file_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("positive integer") || stderr.contains("target_n"));

    // target_n is fractional
    write(
        &file_path,
        r#"{"forecast": {"target_n": 10.5, "total_cost_usd": {"point": 100.0}}}"#,
    )
    .unwrap();
    let output = run_power(&[
        "--baseline-rate",
        "0.5",
        "--delta",
        "0.1",
        "--from-forecast",
        file_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("positive integer") || stderr.contains("target_n"));
}

#[test]
fn cli_fails_with_malformed_forecast_point_cost() {
    use std::fs::write;
    let temp_dir = std::env::temp_dir();
    let file_path = temp_dir.join("malformed_forecast_point_cost.json");

    // point_cost is negative
    write(
        &file_path,
        r#"{"forecast": {"target_n": 100.0, "total_cost_usd": {"point": -50.0}}}"#,
    )
    .unwrap();
    let output = run_power(&[
        "--baseline-rate",
        "0.5",
        "--delta",
        "0.1",
        "--from-forecast",
        file_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("non-negative") || stderr.contains("total_cost_usd"));
}

#[test]
fn cli_fails_with_infinite_cost_per_instance() {
    let output = run_power(&[
        "--baseline-rate",
        "0.5",
        "--delta",
        "0.1",
        "--cost-per-instance",
        "inf",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("finite") || stderr.contains("Cost"));
}

#[test]
fn cli_fails_with_overflowing_delta_sample_size() {
    let output = run_power(&["--baseline-rate", "0.5", "--delta", "1e-16"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("too large to be represented")
            || stderr.contains("precision")
            || stderr.contains("Usage")
    );
}
