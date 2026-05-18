//! `bench cascade`: integration tests for the cost-optimized model-tier routing command.
//!
//! Tests follow TDD RED→GREEN→REFACTOR per issue #275.
//! Unit-level parsing/validation live in `src/run/cascade.rs`;
//! these tests exercise the CLI binary and the public Rust API.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use maxwells_daemon::model::ModelUsage;
use maxwells_daemon::run::cascade::{
    CascadeArgs, CascadeManifest, TierDef, run as cascade_run, validate_tiers,
};
use maxwells_daemon::run::dataset::DatasetSource;
use maxwells_daemon::run::evaluate::EvaluateBackend;
use maxwells_daemon::run::swebench::StratifyMode;

mod support;
use support::binary_path;

// ── helpers ───────────────────────────────────────────────────────────────────

fn minimal_dataset(dir: &Path, instances: &[&str]) -> std::path::PathBuf {
    let path = dir.join("dataset.jsonl");
    let content = instances.iter().fold(String::new(), |mut acc, id| {
        let _ = writeln!(
            acc,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"fix it\"}}"
        );
        acc
    });
    std::fs::write(&path, content).unwrap();
    path
}

fn write_cascade_toml(dir: &Path, tiers: &[(&str, &str)]) -> std::path::PathBuf {
    let path = dir.join("cascade.toml");
    let content = tiers.iter().fold(String::new(), |mut acc, (name, model)| {
        let _ = write!(
            acc,
            "[[tier]]\nname = \"{name}\"\nmodel = \"{model}\"\nstep_limit = 1\n\n"
        );
        acc
    });
    std::fs::write(&path, &content).unwrap();
    path
}

fn default_cascade_args(
    config_path: std::path::PathBuf,
    dataset_path: std::path::PathBuf,
    output_dir: std::path::PathBuf,
) -> CascadeArgs {
    CascadeArgs {
        config_path,
        dataset_source: DatasetSource::LocalPath(dataset_path),
        dataset_cache_dir: std::path::PathBuf::from("/nonexistent"),
        output_dir,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: StratifyMode::Proportional,
        sweep_cost_limit_usd: None,
        resume: false,
        parallel: 1,
        skip_preflight: true,
        skip_model_probe: true,
        eval_backend: EvaluateBackend::None,
        sb_subset: "swe-bench-m".into(),
        sb_split: "test".into(),
        deterministic_responses: Some(vec!["looking at the problem...".into()]),
        deterministic_usage_per_call: None,
        cancel_deadline_secs: 5,
        install_os_signal_handlers: false,
        // No instances resolved by default (all tiers run).
        mock_eval_resolved_ids: Some(vec![]),
    }
}

// ── CLI smoke tests ───────────────────────────────────────────────────────────

#[test]
fn cli_help_includes_cascade_subcommand() {
    let output = Command::new(binary_path())
        .args(["bench", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("cascade"),
        "bench --help should list the `cascade` subcommand, got:\n{stdout}"
    );
}

#[test]
fn cli_cascade_help_shows_required_flags() {
    let output = Command::new(binary_path())
        .args(["bench", "cascade", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--config"), "expected --config in help:\n{stdout}");
    assert!(stdout.contains("--output"), "expected --output in help:\n{stdout}");
    assert!(
        stdout.contains("--dataset-path"),
        "expected --dataset-path in help:\n{stdout}"
    );
    assert!(
        stdout.contains("--sweep-cost-limit-usd"),
        "expected --sweep-cost-limit-usd in help:\n{stdout}"
    );
    assert!(stdout.contains("--resume"), "expected --resume in help:\n{stdout}");
    assert!(
        stdout.contains("--eval-backend"),
        "expected --eval-backend in help:\n{stdout}"
    );
}

#[test]
fn cli_cascade_requires_config_flag() {
    let output = Command::new(binary_path())
        .args([
            "bench",
            "cascade",
            "--dataset-path",
            "x.jsonl",
            "--output",
            "/tmp/out",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "bench cascade without --config should fail"
    );
}

#[test]
fn cli_cascade_requires_dataset_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let config = write_cascade_toml(tmp.path(), &[("haiku", "claude-haiku-4-5")]);

    let output = Command::new(binary_path())
        .args([
            "bench",
            "cascade",
            "--config",
            config.to_str().unwrap(),
            "--output",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "bench cascade without --dataset-path should fail"
    );
}

// ── Unit: manifest parsing ────────────────────────────────────────────────────

#[test]
fn manifest_parses_minimal_tier_definition() {
    let toml_str = "[[tier]]\nname = \"haiku\"\nmodel = \"claude-haiku-4-5\"\n";
    let manifest: CascadeManifest = toml::from_str(toml_str).unwrap();
    assert_eq!(manifest.tiers.len(), 1);
    assert_eq!(manifest.tiers[0].name, "haiku");
    assert_eq!(manifest.tiers[0].model, "claude-haiku-4-5");
    assert!(manifest.tiers[0].step_limit.is_none());
    assert!(manifest.tiers[0].per_task_budget_usd.is_none());
    assert!(manifest.tiers[0].prompt_file.is_none());
    assert!(manifest.tiers[0].extra_args.is_empty());
}

#[test]
fn manifest_parses_tier_with_all_optional_fields() {
    let toml_str = r#"
[[tier]]
name = "sonnet"
model = "claude-sonnet-4-6"
step_limit = 40
per_task_budget_usd = 1.50
prompt_file = "/path/to/config.toml"
extra_args = ["--skip-patch-validation"]
"#;
    let manifest: CascadeManifest = toml::from_str(toml_str).unwrap();
    let tier = &manifest.tiers[0];
    assert_eq!(tier.name, "sonnet");
    assert_eq!(tier.model, "claude-sonnet-4-6");
    assert_eq!(tier.step_limit, Some(40));
    assert_eq!(tier.per_task_budget_usd, Some(1.50));
    assert_eq!(
        tier.prompt_file.as_deref().unwrap().to_str().unwrap(),
        "/path/to/config.toml"
    );
    assert_eq!(tier.extra_args, vec!["--skip-patch-validation"]);
}

#[test]
fn manifest_parses_multiple_tiers_in_order() {
    let content = [("haiku", "claude-haiku-4-5"), ("sonnet", "claude-sonnet-4-6"), ("opus", "claude-opus-4-7")]
        .iter()
        .fold(String::new(), |mut acc, (n, m)| {
            let _ = write!(acc, "[[tier]]\nname = \"{n}\"\nmodel = \"{m}\"\n\n");
            acc
        });
    let manifest: CascadeManifest = toml::from_str(&content).unwrap();
    assert_eq!(manifest.tiers.len(), 3);
    assert_eq!(manifest.tiers[0].name, "haiku");
    assert_eq!(manifest.tiers[1].name, "sonnet");
    assert_eq!(manifest.tiers[2].name, "opus");
}

// ── Unit: tier validation ─────────────────────────────────────────────────────

#[test]
fn validate_tiers_accepts_unique_names() {
    let tiers = vec![
        TierDef { name: "haiku".into(), model: "m1".into(), ..TierDef::default() },
        TierDef { name: "sonnet".into(), model: "m2".into(), ..TierDef::default() },
    ];
    assert!(validate_tiers(&tiers).is_ok());
}

#[test]
fn validate_tiers_rejects_empty_list() {
    let err = validate_tiers(&[]).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("tier") || msg.contains("empty"), "{err}");
}

#[test]
fn validate_tiers_rejects_empty_name() {
    let tiers = vec![TierDef {
        name: String::new(),
        model: "m1".into(),
        ..TierDef::default()
    }];
    let err = validate_tiers(&tiers).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("empty") || msg.contains("name"), "{err}");
}

#[test]
fn validate_tiers_rejects_name_with_path_separator() {
    for bad_name in ["/evil", "tier/../attack", "tier\\win"] {
        let tiers = vec![TierDef {
            name: bad_name.into(),
            model: "m".into(),
            ..TierDef::default()
        }];
        let err = validate_tiers(&tiers).unwrap_err();
        assert!(
            err.to_string().contains("name"),
            "expected tier name validation error for `{bad_name}`, got: {err}"
        );
    }
}

#[test]
fn validate_tiers_rejects_duplicate_names() {
    let tiers = vec![
        TierDef { name: "same".into(), model: "m1".into(), ..TierDef::default() },
        TierDef { name: "same".into(), model: "m2".into(), ..TierDef::default() },
    ];
    let err = validate_tiers(&tiers).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("same"), "error should name the duplicate: {err}");
    assert!(msg.contains("duplicate"), "error should say duplicate: {err}");
}

// ── Integration: preflight ────────────────────────────────────────────────────

/// AC#10: If no evaluation backend is configured, command fails preflight with
/// a clear actionable error (exit code in the `usage_error` class).
#[tokio::test]
async fn cascade_fails_preflight_without_eval_backend_and_no_mock() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_cascade_toml(tmp.path(), &[("haiku", "deterministic")]);
    let output = tmp.path().join("out");

    let mut args = default_cascade_args(config, dataset, output);
    // No backend AND no mock → should fail preflight.
    args.eval_backend = EvaluateBackend::None;
    args.mock_eval_resolved_ids = None;

    let err = cascade_run(args).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("eval") || msg.contains("backend") || msg.contains("evaluation"),
        "preflight error should mention evaluation backend: {err}"
    );
}

// ── Integration: cascade run ──────────────────────────────────────────────────

/// AC#4, AC#5: Creates per-tier dirs, cascade.json artifact with per-instance records.
#[tokio::test]
async fn cascade_creates_tier_dirs_and_cascade_json() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2"]);
    let config = write_cascade_toml(
        tmp.path(),
        &[("haiku", "deterministic"), ("sonnet", "deterministic")],
    );
    let output = tmp.path().join("out");

    let args = default_cascade_args(config, dataset, output.clone());
    let _summary = cascade_run(args).await.unwrap();

    // Per-tier directories exist with results.json (AC#4).
    assert!(
        output.join("tier-haiku").join("results.json").exists(),
        "tier-haiku/results.json must exist"
    );
    assert!(
        output.join("tier-sonnet").join("results.json").exists(),
        "tier-sonnet/results.json must exist"
    );

    // cascade.json artifact at sweep root (AC#5).
    let cascade_path = output.join("cascade.json");
    assert!(cascade_path.exists(), "cascade.json must exist");
    let cascade: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cascade_path).unwrap()).unwrap();

    // Per-instance records present.
    let instances = cascade["instances"].as_object().unwrap();
    assert!(
        instances.contains_key("inst-1"),
        "cascade.json should record inst-1"
    );
    assert!(
        instances.contains_key("inst-2"),
        "cascade.json should record inst-2"
    );

    // Each instance has required fields (AC#5).
    let inst = &instances["inst-1"];
    assert!(inst["attempts"].is_array(), "attempts must be an array");
    assert!(
        inst["resolving_tier"].is_string() || inst["resolving_tier"].is_null(),
        "resolving_tier must be string or null"
    );
    assert!(
        inst["total_cost_usd"].is_number(),
        "total_cost_usd must be a number"
    );

    // Each attempt record has required fields.
    let attempts = inst["attempts"].as_array().unwrap();
    if !attempts.is_empty() {
        let attempt = &attempts[0];
        assert!(attempt["tier_name"].is_string(), "attempt.tier_name must be string");
        assert!(attempt["model"].is_string(), "attempt.model must be string");
        assert!(attempt["cost_usd"].is_number(), "attempt.cost_usd must be number");
    }
}

/// AC#6: cascade-summary.json (and human-readable table) reports per-tier stats.
#[tokio::test]
async fn cascade_creates_cascade_summary_json() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2"]);
    let config = write_cascade_toml(
        tmp.path(),
        &[("haiku", "deterministic"), ("sonnet", "deterministic")],
    );
    let output = tmp.path().join("out");

    let args = default_cascade_args(config, dataset, output.clone());
    let summary = cascade_run(args).await.unwrap();

    // cascade-summary.json exists (AC#6).
    let summary_path = output.join("cascade-summary.json");
    assert!(summary_path.exists(), "cascade-summary.json must exist");

    let summary_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();

    assert_eq!(
        summary_json["artifact_kind"].as_str().unwrap(),
        "cascade-summary"
    );
    assert!(summary_json["tiers"].is_array(), "tiers must be array");
    assert!(
        summary_json["cascade_resolved"].is_number(),
        "cascade_resolved must be number"
    );
    assert!(
        summary_json["total_cost_usd"].is_number(),
        "total_cost_usd must be number"
    );
    assert!(
        summary_json["savings_vs_top_tier_only_usd"].is_number(),
        "savings_vs_top_tier_only_usd must be number"
    );
    assert!(
        summary_json["cost_per_resolved_cascade_usd"].is_number(),
        "cost_per_resolved_cascade_usd must be number"
    );

    // Per-tier rows have required fields (AC#6).
    let tiers_arr = summary_json["tiers"].as_array().unwrap();
    assert_eq!(tiers_arr.len(), 2, "should have 2 tier rows");
    for tier_row in tiers_arr {
        assert!(tier_row["name"].is_string(), "tier.name must be string");
        assert!(tier_row["model"].is_string(), "tier.model must be string");
        assert!(
            tier_row["instances_attempted"].is_number(),
            "tier.instances_attempted must be number"
        );
        assert!(
            tier_row["resolved"].is_number(),
            "tier.resolved must be number"
        );
        assert!(
            tier_row["resolved_rate"].is_number(),
            "tier.resolved_rate must be number"
        );
        assert!(
            tier_row["mean_cost_per_attempt_usd"].is_number(),
            "tier.mean_cost_per_attempt_usd must be number"
        );
    }

    // Returned summary has same tier count.
    assert_eq!(summary.tiers.len(), 2);
}

/// AC#9: When tier k resolves an instance, tier k+1 MUST NOT run for that instance.
#[tokio::test]
async fn cascade_short_circuits_resolved_instances() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2"]);
    let config = write_cascade_toml(
        tmp.path(),
        &[("haiku", "deterministic"), ("sonnet", "deterministic")],
    );
    let output = tmp.path().join("out");

    let mut args = default_cascade_args(config, dataset, output.clone());
    // Mock: tier 0 ("haiku") resolves inst-1; inst-2 is unresolved.
    args.mock_eval_resolved_ids = Some(vec![
        // tier 0 resolved ids
        HashSet::from(["inst-1".to_string()]),
        // tier 1 resolved ids (inst-2 also unresolved by sonnet)
        HashSet::from([]),
    ]);

    let _summary = cascade_run(args).await.unwrap();

    let cascade_path = output.join("cascade.json");
    let cascade: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cascade_path).unwrap()).unwrap();

    let instances = cascade["instances"].as_object().unwrap();

    // inst-1 was resolved by haiku → resolving_tier == "haiku"
    let inst1 = &instances["inst-1"];
    assert_eq!(
        inst1["resolving_tier"].as_str().unwrap(),
        "haiku",
        "inst-1 should be resolved by haiku tier"
    );
    // inst-1 should have exactly 1 attempt (haiku only, no sonnet) — AC#9.
    let attempts1 = inst1["attempts"].as_array().unwrap();
    assert_eq!(
        attempts1.len(),
        1,
        "inst-1 resolved by haiku, sonnet must NOT have run (AC#9)"
    );
    assert_eq!(attempts1[0]["tier_name"].as_str().unwrap(), "haiku");

    // inst-2 was not resolved → resolving_tier == null, both tiers attempted.
    let inst2 = &instances["inst-2"];
    assert!(
        inst2["resolving_tier"].is_null(),
        "inst-2 was never resolved, resolving_tier should be null"
    );
    let attempts2 = inst2["attempts"].as_array().unwrap();
    assert_eq!(
        attempts2.len(),
        2,
        "inst-2 should have been attempted by both tiers"
    );
    assert_eq!(attempts2[0]["tier_name"].as_str().unwrap(), "haiku");
    assert_eq!(attempts2[1]["tier_name"].as_str().unwrap(), "sonnet");
}

/// AC#7: Sweep-wide --sweep-cost-limit-usd enforced; instances after cap are skipped_budget.
#[tokio::test]
async fn cascade_budget_limit_marks_skipped_budget() {
    let tmp = tempfile::tempdir().unwrap();
    // Use 5 instances so some can be skipped_budget.
    let dataset = minimal_dataset(tmp.path(), &["i1", "i2", "i3", "i4", "i5"]);
    let config = write_cascade_toml(tmp.path(), &[("haiku", "deterministic")]);
    let output = tmp.path().join("out");

    let mut args = default_cascade_args(config, dataset, output.clone());
    // Inject non-zero token usage per call so cost accumulates.
    // claude-sonnet pricing: ~$3/MTok input → 1_000_000 tokens ≈ $3 per call.
    // 1_000_000 sonnet input tokens ≈ $3 → one call will blow the budget.
    args.deterministic_usage_per_call = Some(ModelUsage {
        input_tokens: 1_000_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: Some(3.0),
    });
    // Budget of $0.001 will be exhausted after the first instance runs (~$3 cost).
    let budget = Some(0.001_f64);
    args.sweep_cost_limit_usd = budget;

    // Run should complete (not error).
    let summary = cascade_run(args).await.unwrap();

    let cascade_path = output.join("cascade.json");
    let cascade: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cascade_path).unwrap()).unwrap();

    // Some instances should be skipped_budget.
    let instances = cascade["instances"].as_object().unwrap();
    let skipped_count = instances
        .values()
        .filter(|v| {
            v["attempts"]
                .as_array()
                .is_some_and(|a| {
                    a.iter().any(|att| {
                        att["halted_reason"].as_str() == Some("skipped_budget")
                    })
                })
                || v.get("skipped_reason")
                    .and_then(|s| s.as_str())
                    == Some("skipped_budget")
        })
        .count();
    assert!(
        skipped_count > 0,
        "at least one instance should be skipped_budget with a tiny budget, summary: {:?}",
        summary.total_cost_usd
    );

    // Total cost should be less than running all 5 at $3/each.
    assert!(
        summary.total_cost_usd < 15.01,
        "total cost should be less than running all instances: got {:.4}",
        summary.total_cost_usd
    );
}

/// AC#8: --resume skips already-resolved instances; partially-cascaded resumes on next tier.
#[tokio::test]
async fn cascade_resume_skips_completed_instances() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1", "inst-2"]);
    let config = write_cascade_toml(
        tmp.path(),
        &[("haiku", "deterministic"), ("sonnet", "deterministic")],
    );
    let output = tmp.path().join("out");

    // First run: only haiku tier; inst-1 resolved, inst-2 not.
    {
        let mut args = default_cascade_args(config.clone(), dataset.clone(), output.clone());
        // mock: only haiku resolves inst-1
        args.mock_eval_resolved_ids = Some(vec![
            HashSet::from(["inst-1".to_string()]),
            HashSet::new(),
        ]);
        cascade_run(args).await.unwrap();
    }

    // Tamper: read cascade.json and verify state after first run.
    let cascade_after_first: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output.join("cascade.json")).unwrap(),
    ).unwrap();
    let inst2_attempts_after_first = cascade_after_first["instances"]["inst-2"]["attempts"]
        .as_array()
        .unwrap()
        .len();
    // Both tiers ran for inst-2 on first run.
    assert_eq!(inst2_attempts_after_first, 2, "both tiers should have run for inst-2 on first run");

    // Second run with --resume: should skip inst-1 (resolved) and inst-2 (all tiers exhausted).
    {
        let mut args = default_cascade_args(config, dataset, output.clone());
        args.resume = true;
        args.mock_eval_resolved_ids = Some(vec![
            HashSet::new(), // haiku: nothing resolves on "re-run"
            HashSet::new(),
        ]);
        let summary = cascade_run(args).await.unwrap();

        // Check that cascade.json is still consistent.
        let cascade_after_resume: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(output.join("cascade.json")).unwrap(),
        ).unwrap();
        let inst1_resolving = cascade_after_resume["instances"]["inst-1"]["resolving_tier"]
            .as_str()
            .unwrap_or("");
        assert_eq!(
            inst1_resolving, "haiku",
            "inst-1 resolving_tier should be preserved on resume"
        );

        // inst-2 still not resolved (same or same number of attempts).
        let inst2_attempts_after_resume = cascade_after_resume["instances"]["inst-2"]["attempts"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(
            inst2_attempts_after_resume, 2,
            "inst-2 should not get new attempts on resume (all tiers exhausted)"
        );

        // Summary should not have inflated cost from re-running resolved instances.
        let _ = summary;
    }
}

/// AC#5: resolving_tier is null when no tier resolves an instance.
#[tokio::test]
async fn cascade_records_null_resolving_tier_for_unresolved_instances() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_cascade_toml(tmp.path(), &[("haiku", "deterministic")]);
    let output = tmp.path().join("out");

    let args = default_cascade_args(config, dataset, output.clone());
    // mock_eval_resolved_ids = Some(vec![]) → no resolved instances
    cascade_run(args).await.unwrap();

    let cascade: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("cascade.json")).unwrap())
            .unwrap();

    let inst = &cascade["instances"]["inst-1"];
    assert!(
        inst["resolving_tier"].is_null(),
        "resolving_tier should be null when no tier resolves the instance"
    );
}

/// Artifact kind field is present for both artifacts.
#[tokio::test]
async fn cascade_artifacts_have_correct_kind_field() {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = minimal_dataset(tmp.path(), &["inst-1"]);
    let config = write_cascade_toml(tmp.path(), &[("haiku", "deterministic")]);
    let output = tmp.path().join("out");

    let args = default_cascade_args(config, dataset, output.clone());
    cascade_run(args).await.unwrap();

    let cascade: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("cascade.json")).unwrap())
            .unwrap();
    assert_eq!(cascade["artifact_kind"].as_str().unwrap(), "cascade");

    let summary: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output.join("cascade-summary.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["artifact_kind"].as_str().unwrap(), "cascade-summary");
}
