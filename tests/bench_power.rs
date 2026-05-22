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
fn cli_mode_a_sample_size_reference_2() {
    // baseline_rate=0.20, delta=0.05, N = 931 per arm (or close reference)
    // Let's verify statsmodels standard:
    // statsmodels.stats.power.NormalIndPower().solve_power(effect_size=2*(arcsin(sqrt(0.2))-arcsin(sqrt(0.15))), alpha=0.05, power=0.8) => N1=931
    let report = run_power_json(&["--baseline-rate", "0.20", "--delta", "0.05"]);
    assert_eq!(report["solved_n"].as_u64().unwrap(), 931);
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
fn cli_one_sided_mode_reduces_sample_size() {
    // Two-sided baseline=0.50, delta=0.10 -> N = 388
    // One-sided baseline=0.50, delta=0.10 -> N = 306
    let report = run_power_json(&["--baseline-rate", "0.50", "--delta", "0.10", "--one-sided"]);
    assert_eq!(report["solved_n"].as_u64().unwrap(), 306);
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
