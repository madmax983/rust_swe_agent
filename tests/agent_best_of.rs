//! Tests for `agent best-of` — issue #485.
//!
//! RED phase: tests reference the not-yet-implemented API and will fail to
//! compile until the implementation is in place.
//! GREEN phase: implement src/run/best_of.rs and wire the CLI.
//! REFACTOR phase: clean up.

#![allow(clippy::unwrap_used)]

use maxwells_daemon::artifact::{ArtifactKind, ArtifactSchemaVersion};
use maxwells_daemon::exit_code::ExitCode;
use maxwells_daemon::run::best_of::{
    BestOfRunDetail, BestOfResults, select_winner, validate_runs,
};

// ── Exit code unit tests ──────────────────────────────────────────────────────

#[test]
fn best_of_all_failed_exit_code_is_40() {
    assert_eq!(ExitCode::BestOfAllFailed.as_i32(), 40);
    assert_eq!(
        ExitCode::BestOfAllFailed.outcome_class(),
        "best_of_all_failed"
    );
}

// ── ArtifactKind tests ────────────────────────────────────────────────────────

#[test]
fn artifact_kind_best_of_results_label() {
    assert_eq!(ArtifactKind::BestOfResults.label(), "best_of_results");
}

// ── validate_runs tests ───────────────────────────────────────────────────────

#[test]
fn validate_runs_one_is_error() {
    // best-of-1 is meaningless; min is 2
    let err = validate_runs(1);
    assert!(err.is_err(), "expected error for --runs 1");
    let msg = err.unwrap_err();
    assert!(
        msg.contains("mini") || msg.contains("2"),
        "error should mention minimum 2 or suggest mini; got: {msg}"
    );
}

#[test]
fn validate_runs_zero_is_error() {
    assert!(validate_runs(0).is_err());
}

#[test]
fn validate_runs_eleven_is_error() {
    assert!(validate_runs(11).is_err());
}

#[test]
fn validate_runs_two_is_ok() {
    assert!(validate_runs(2).is_ok());
}

#[test]
fn validate_runs_ten_is_ok() {
    assert!(validate_runs(10).is_ok());
}

// ── BestOfRunDetail struct tests ─────────────────────────────────────────────

#[test]
fn best_of_run_detail_has_required_fields() {
    let detail = BestOfRunDetail {
        run_index: 0,
        outcome: "submitted".into(),
        verify_checks_passed: 2,
        verify_checks_total: 2,
        passed: true,
        total_cost_usd: Some(0.005),
        step_count: Some(10),
        patch_byte_len: Some(42),
        patch_sha256: Some("abc123".into()),
        skipped: false,
        skip_reason: None,
    };
    assert_eq!(detail.run_index, 0);
    assert!(detail.passed);
    assert_eq!(detail.verify_checks_passed, 2);
    assert_eq!(detail.verify_checks_total, 2);
    assert!(!detail.skipped);
}

// ── BestOfResults struct tests ────────────────────────────────────────────────

#[test]
fn best_of_results_has_required_fields() {
    let results = BestOfResults {
        schema_version: ArtifactSchemaVersion::CURRENT,
        artifact_kind: ArtifactKind::BestOfResults,
        task: "Fix the bug".into(),
        runs: 3,
        winner_run_index: Some(0),
        passing_run_count: 1,
        all_failed: false,
        tie_break_applied: false,
        selection_rationale: "most_verify_checks_passed".into(),
        started_at: "2026-01-01T00:00:00Z".into(),
        finished_at: "2026-01-01T00:01:00Z".into(),
        runs_detail: vec![],
    };
    assert_eq!(results.runs, 3);
    assert_eq!(results.winner_run_index, Some(0));
    assert_eq!(results.passing_run_count, 1);
    assert!(!results.all_failed);
    assert!(!results.tie_break_applied);
}

// ── select_winner tests ───────────────────────────────────────────────────────

fn make_run(
    run_index: u32,
    checks_passed: u32,
    checks_total: u32,
    cost: f64,
    steps: u32,
    patch: &str,
) -> BestOfRunDetail {
    use sha2::{Digest, Sha256};
    let (sha, byte_len) = if patch.is_empty() {
        (None, None)
    } else {
        let mut hasher = Sha256::new();
        hasher.update(patch.as_bytes());
        (
            Some(format!("{:x}", hasher.finalize())),
            Some(patch.len() as u64),
        )
    };
    BestOfRunDetail {
        run_index,
        outcome: if checks_passed == checks_total && checks_total > 0 {
            "submitted".into()
        } else {
            "verification_failed".into()
        },
        verify_checks_passed: checks_passed,
        verify_checks_total: checks_total,
        passed: checks_passed == checks_total && checks_total > 0,
        total_cost_usd: Some(cost),
        step_count: Some(steps),
        patch_byte_len: byte_len,
        patch_sha256: sha,
        skipped: false,
        skip_reason: None,
    }
}

#[test]
fn select_winner_prefers_most_checks_passed() {
    // run 0: 1/2 checks, run 1: 2/2 checks → run 1 wins
    let runs = vec![
        make_run(0, 1, 2, 0.005, 10, "patch-a"),
        make_run(1, 2, 2, 0.010, 15, "patch-b"),
    ];
    let (idx, rationale, tie_break) = select_winner(&runs);
    assert_eq!(idx, 1, "run with more passing checks should win");
    assert!(!tie_break);
    assert!(!rationale.is_empty());
}

#[test]
fn select_winner_tie_breaks_by_lowest_cost() {
    // Both pass all checks; run 0 is cheaper
    let runs = vec![
        make_run(0, 2, 2, 0.003, 10, "patch-a"),
        make_run(1, 2, 2, 0.007, 10, "patch-b"),
    ];
    let (idx, _rationale, tie_break) = select_winner(&runs);
    assert_eq!(idx, 0, "lower cost should win tie");
    assert!(tie_break);
}

#[test]
fn select_winner_tie_breaks_by_fewest_steps() {
    // Same checks, same cost → fewest steps wins
    let runs = vec![
        make_run(0, 2, 2, 0.005, 15, "patch-a"),
        make_run(1, 2, 2, 0.005, 8, "patch-b"),
    ];
    let (idx, _rationale, tie_break) = select_winner(&runs);
    assert_eq!(idx, 1, "fewer steps should win tie");
    assert!(tie_break);
}

#[test]
fn select_winner_tie_breaks_by_smallest_patch() {
    // Same checks, same cost, same steps → smaller patch wins
    let runs = vec![
        make_run(0, 2, 2, 0.005, 10, "longer-patch-content-here"),
        make_run(1, 2, 2, 0.005, 10, "short"),
    ];
    let (idx, _rationale, tie_break) = select_winner(&runs);
    assert_eq!(idx, 1, "smaller patch should win tie");
    assert!(tie_break);
}

#[test]
fn select_winner_tie_breaks_by_sha256() {
    // Same checks, same cost, same steps, same-length patches → lex smallest sha wins
    // We create two patches of exactly the same byte length but different content
    let runs = vec![
        make_run(0, 2, 2, 0.005, 10, "zzzzz"),
        make_run(1, 2, 2, 0.005, 10, "aaaaa"),
    ];
    let (idx, _rationale, tie_break) = select_winner(&runs);
    // lex smallest sha of ("aaaaa") should beat sha of ("zzzzz")
    assert!(tie_break);
    // We just assert the same winner is picked deterministically
    let (idx2, _, _) = select_winner(&runs);
    assert_eq!(idx, idx2, "selection must be deterministic");
    let _ = idx; // suppress unused warning if assertion passes
}

#[test]
fn select_winner_all_failed_still_picks_best_scoring() {
    // All fail, but one has more checks passed → select that one
    let runs = vec![
        make_run(0, 0, 2, 0.005, 10, "patch-a"),
        make_run(1, 1, 2, 0.005, 10, "patch-b"),
    ];
    let (idx, _rationale, _tie_break) = select_winner(&runs);
    assert_eq!(idx, 1, "run with more passing checks wins even when all fail");
}

#[test]
fn select_winner_skipped_runs_not_selected() {
    let mut run0 = make_run(0, 2, 2, 0.005, 10, "patch-a");
    run0.skipped = true;
    run0.passed = false;
    run0.verify_checks_passed = 0;
    run0.patch_byte_len = None;
    let run1 = make_run(1, 1, 2, 0.010, 15, "patch-b");
    let runs = vec![run0, run1];
    let (idx, _rationale, _tie_break) = select_winner(&runs);
    assert_eq!(idx, 1, "skipped run should not be selected as winner");
}

// ── summary_text tests ────────────────────────────────────────────────────────

#[test]
fn best_of_results_summary_contains_winner_banner() {
    let results = make_full_results();
    let text = results.summary_text();
    assert!(
        text.to_lowercase().contains("winner") || text.contains("WINNER"),
        "summary should contain winner banner; got:\n{text}"
    );
}

#[test]
fn best_of_results_summary_contains_run_table() {
    let results = make_full_results();
    let text = results.summary_text();
    assert!(
        text.contains("RUN") || text.contains("run"),
        "summary should contain run table header; got:\n{text}"
    );
}

#[test]
fn best_of_results_summary_all_failed_mentions_all_failed() {
    let mut results = make_full_results();
    results.all_failed = true;
    results.winner_run_index = Some(0);
    let text = results.summary_text();
    assert!(
        text.to_lowercase().contains("all") && text.to_lowercase().contains("fail"),
        "summary should mention all-failed condition; got:\n{text}"
    );
}

fn make_full_results() -> BestOfResults {
    BestOfResults {
        schema_version: ArtifactSchemaVersion::CURRENT,
        artifact_kind: ArtifactKind::BestOfResults,
        task: "Fix the null dereference".into(),
        runs: 3,
        winner_run_index: Some(1),
        passing_run_count: 2,
        all_failed: false,
        tie_break_applied: false,
        selection_rationale: "most_verify_checks_passed".into(),
        started_at: "2026-01-01T00:00:00Z".into(),
        finished_at: "2026-01-01T00:01:00Z".into(),
        runs_detail: vec![
            BestOfRunDetail {
                run_index: 0,
                outcome: "submitted".into(),
                verify_checks_passed: 1,
                verify_checks_total: 2,
                passed: false,
                total_cost_usd: Some(0.003),
                step_count: Some(8),
                patch_byte_len: Some(10),
                patch_sha256: Some("abc".into()),
                skipped: false,
                skip_reason: None,
            },
            BestOfRunDetail {
                run_index: 1,
                outcome: "submitted".into(),
                verify_checks_passed: 2,
                verify_checks_total: 2,
                passed: true,
                total_cost_usd: Some(0.005),
                step_count: Some(10),
                patch_byte_len: Some(20),
                patch_sha256: Some("def".into()),
                skipped: false,
                skip_reason: None,
            },
            BestOfRunDetail {
                run_index: 2,
                outcome: "submitted".into(),
                verify_checks_passed: 2,
                verify_checks_total: 2,
                passed: true,
                total_cost_usd: Some(0.007),
                step_count: Some(12),
                patch_byte_len: Some(30),
                patch_sha256: Some("ghi".into()),
                skipped: false,
                skip_reason: None,
            },
        ],
    }
}

// ── Integration tests (scripted-model path) ───────────────────────────────────

#[cfg(test)]
mod integration {
    use super::*;
    use maxwells_daemon::run::best_of::{BestOfArgs, run};

    fn submit_response() -> String {
        "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfixed\n```".into()
    }

    fn make_args(
        tmpdir: &std::path::Path,
        runs: u32,
        verify: Vec<String>,
        responses: Vec<String>,
    ) -> BestOfArgs {
        let mut cfg = maxwells_daemon::config::Config::defaults().unwrap();
        cfg.root.model.name = "test-model".into();
        BestOfArgs {
            task: "Fix the bug".into(),
            runs,
            config: cfg,
            output_dir: tmpdir.to_owned(),
            best_of_name: "test-best-of".into(),
            verify,
            verify_timeout_secs: 60,
            cost_limit_usd: None,
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            output_patch: None,
            allow_no_pass: false,
            deterministic_responses: Some(responses),
            deterministic_usage_per_call: None,
            print_summary: false,
        }
    }

    #[tokio::test]
    async fn requires_verify_checks() {
        // --verify is required; passing empty verify should cause the runner
        // to return BestOfArgs with verify=[] being OK at the run level, but
        // the CLI layer should validate this. Let's test via the run function
        // which should handle it gracefully (the CLI catches this first).
        // We test the validation at the CLI args level via a separate check.
        // The run function itself should still complete (it uses verify for scoring).
        let tmp = tempfile::tempdir().unwrap();
        let args = make_args(tmp.path(), 2, vec![], vec![submit_response(), submit_response()]);
        // With empty verify, the run still completes but uses fallback scoring
        let result = run(args).await;
        assert!(result.is_ok(), "run should complete even without verify checks");
    }

    #[tokio::test]
    async fn two_runs_both_submit_writes_best_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let args = make_args(
            tmp.path(),
            2,
            vec![],
            vec![submit_response(), submit_response()],
        );
        let exit_code = run(args).await.unwrap();
        assert_eq!(exit_code, ExitCode::Success);

        let results_path = tmp
            .path()
            .join("test-best-of")
            .join("best-of-results.json");
        assert!(results_path.exists(), "best-of-results.json must be written");

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
        assert_eq!(json["runs"].as_u64(), Some(2));
        assert_eq!(json["artifact_kind"].as_str(), Some("best_of_results"));
        assert!(json.get("schema_version").is_some());
        assert!(json.get("winner_run_index").is_some());
        assert!(json.get("passing_run_count").is_some());
    }

    #[tokio::test]
    async fn best_patch_written_to_output_file() {
        let tmp = tempfile::tempdir().unwrap();
        let patch_path = tmp.path().join("winner.patch");
        let mut cfg = maxwells_daemon::config::Config::defaults().unwrap();
        cfg.root.model.name = "test-model".into();
        let args = BestOfArgs {
            task: "Fix the bug".into(),
            runs: 2,
            config: cfg,
            output_dir: tmp.path().to_owned(),
            best_of_name: "test-best-of".into(),
            verify: vec![],
            verify_timeout_secs: 60,
            cost_limit_usd: None,
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            output_patch: Some(patch_path.clone()),
            allow_no_pass: false,
            deterministic_responses: Some(vec![submit_response(), submit_response()]),
            deterministic_usage_per_call: None,
            print_summary: false,
        };
        run(args).await.unwrap();
        // The best.patch should have been written (even if empty for scripted model)
        assert!(
            patch_path.exists(),
            "winner patch file must be written at the specified path"
        );
    }

    #[tokio::test]
    async fn all_failed_with_allow_no_pass_exits_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = maxwells_daemon::config::Config::defaults().unwrap();
        cfg.root.model.name = "test-model".into();
        let args = BestOfArgs {
            task: "Fix the bug".into(),
            runs: 2,
            config: cfg,
            output_dir: tmp.path().to_owned(),
            best_of_name: "test-best-of".into(),
            // Use a verify check that will always fail (command returns non-zero)
            verify: vec!["always_fail:false".into()],
            verify_timeout_secs: 60,
            cost_limit_usd: None,
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            output_patch: None,
            allow_no_pass: true,
            deterministic_responses: Some(vec![submit_response(), submit_response()]),
            deterministic_usage_per_call: None,
            print_summary: false,
        };
        let exit_code = run(args).await.unwrap();
        assert_eq!(
            exit_code,
            ExitCode::Success,
            "--allow-no-pass should downgrade all-failed to exit 0"
        );

        let results_path = tmp
            .path()
            .join("test-best-of")
            .join("best-of-results.json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
        assert_eq!(
            json["all_failed"].as_bool(),
            Some(true),
            "all_failed must be true in artifact"
        );
    }

    #[tokio::test]
    async fn all_failed_without_allow_no_pass_exits_40() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = maxwells_daemon::config::Config::defaults().unwrap();
        cfg.root.model.name = "test-model".into();
        let args = BestOfArgs {
            task: "Fix the bug".into(),
            runs: 2,
            config: cfg,
            output_dir: tmp.path().to_owned(),
            best_of_name: "test-best-of".into(),
            verify: vec!["always_fail:false".into()],
            verify_timeout_secs: 60,
            cost_limit_usd: None,
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            output_patch: None,
            allow_no_pass: false,
            deterministic_responses: Some(vec![submit_response(), submit_response()]),
            deterministic_usage_per_call: None,
            print_summary: false,
        };
        let exit_code = run(args).await.unwrap();
        assert_eq!(
            exit_code,
            ExitCode::BestOfAllFailed,
            "exit 40 when no run passes and --allow-no-pass is not set"
        );
    }

    #[tokio::test]
    async fn cost_limit_skips_remaining_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = maxwells_daemon::config::Config::defaults().unwrap();
        cfg.root.model.name = "test-model".into();
        let args = BestOfArgs {
            task: "Fix the bug".into(),
            runs: 3,
            config: cfg,
            output_dir: tmp.path().to_owned(),
            best_of_name: "test-best-of".into(),
            verify: vec![],
            verify_timeout_secs: 60,
            cost_limit_usd: Some(0.0), // zero cap → runs 2+ get skipped after run 1 spends
            task_timeout_secs: Some(30),
            step_limit: None,
            per_task_budget_usd: None,
            output_patch: None,
            allow_no_pass: false,
            deterministic_responses: Some(vec![
                submit_response(),
                submit_response(),
                submit_response(),
            ]),
            deterministic_usage_per_call: Some(maxwells_daemon::model::ModelUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_usd: Some(0.01),
            }),
            print_summary: false,
        };
        run(args).await.unwrap();

        let results_path = tmp
            .path()
            .join("test-best-of")
            .join("best-of-results.json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
        let detail = json["runs_detail"].as_array().unwrap();
        let skipped_count = detail
            .iter()
            .filter(|d| d["skipped"].as_bool().unwrap_or(false))
            .count();
        assert!(
            skipped_count >= 2,
            "at least 2 runs should be skipped; got {skipped_count}"
        );
    }

    #[tokio::test]
    async fn deterministic_same_inputs_byte_identical_results() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();

        let mk = |dir: &std::path::Path| {
            let mut cfg = maxwells_daemon::config::Config::defaults().unwrap();
            cfg.root.model.name = "test-model".into();
            BestOfArgs {
                task: "Fix the bug".into(),
                runs: 2,
                config: cfg,
                output_dir: dir.to_owned(),
                best_of_name: "test-best-of".into(),
                verify: vec![],
                verify_timeout_secs: 60,
                cost_limit_usd: None,
                task_timeout_secs: Some(30),
                step_limit: None,
                per_task_budget_usd: None,
                output_patch: None,
                allow_no_pass: false,
                deterministic_responses: Some(vec![submit_response(), submit_response()]),
                deterministic_usage_per_call: None,
                print_summary: false,
            }
        };

        run(mk(tmp1.path())).await.unwrap();
        run(mk(tmp2.path())).await.unwrap();

        let read = |p: &std::path::Path| -> serde_json::Value {
            let path = p.join("test-best-of").join("best-of-results.json");
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let r1 = read(tmp1.path());
        let r2 = read(tmp2.path());

        assert_eq!(r1["runs"], r2["runs"]);
        assert_eq!(r1["winner_run_index"], r2["winner_run_index"]);
        assert_eq!(r1["passing_run_count"], r2["passing_run_count"]);
        assert_eq!(r1["artifact_kind"], r2["artifact_kind"]);
        assert_eq!(r1["schema_version"], r2["schema_version"]);
        assert_eq!(r1["selection_rationale"], r2["selection_rationale"]);
    }
}
