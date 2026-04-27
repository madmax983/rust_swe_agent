//! End-to-end behavior of `bench swebench` dataset subsetting flags
//! (`--instance-ids` / `--limit` / `--sample` / `--seed`).
//!
//! Coverage matrix:
//!   * id filter selects only matching ids and no others
//!   * id filter referencing a missing id exits non-zero before any task launches
//!   * `--sample` is reproducible under a fixed seed (across two full runs)
//!   * `--limit` interacts with `--sample` and `--instance-ids`
//!   * an empty composed subset exits non-zero and writes no `results.json`
//!   * `results.json` records the resolved `filter_spec`

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use rust_swe_agent::Config;
use rust_swe_agent::run::filter::FilterArgs;
use rust_swe_agent::run::swebench::{SwebenchArgs, SweepResults, run};

fn write_dataset(path: &Path, instance_ids: &[&str]) {
    let mut s = String::new();
    for id in instance_ids {
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\"}}"
        );
    }
    std::fs::write(path, s).unwrap();
}

fn submit_only_responses() -> Vec<String> {
    vec!["COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nfresh-run\n```".into()]
}

fn init_repo(dir: &Path) {
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@test"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
}

fn config_with_workdir(dir: &Path) -> Config {
    let yaml = format!("environment:\n  workdir: {}\n", dir.display());
    Config::from_yaml_str(&yaml).unwrap()
}

#[tokio::test]
async fn instance_ids_flag_subsets_to_matching_ids() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["alpha", "beta", "gamma", "delta"]);

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 2,
        config: cfg,
        resume: false,
        cost_limit_usd: None,
        filter: FilterArgs {
            instance_ids: Some(vec!["beta".into(), "delta".into()]),
            ..Default::default()
        },
        deterministic_responses: Some(submit_only_responses()),
        deterministic_usage_per_call: None,
    })
    .await
    .unwrap();

    // Post-filter total reflects only the requested ids.
    assert_eq!(results.total, 2, "post-filter total must equal selected ids");
    let dispatched: Vec<&str> = results.instances.iter().map(|r| r.instance_id.as_str()).collect();
    assert!(dispatched.contains(&"beta"));
    assert!(dispatched.contains(&"delta"));
    assert!(!dispatched.contains(&"alpha"));
    assert!(!dispatched.contains(&"gamma"));

    // Trajectories were only written for the filtered-in ids.
    assert!(output.join("beta.traj.json").exists());
    assert!(output.join("delta.traj.json").exists());
    assert!(!output.join("alpha.traj.json").exists());
    assert!(!output.join("gamma.traj.json").exists());

    // results.json records the filter_spec.
    let raw = std::fs::read_to_string(output.join("results.json")).unwrap();
    let sweep: SweepResults = serde_json::from_str(&raw).unwrap();
    let spec = sweep.filter_spec.unwrap();
    assert_eq!(spec.original_count, 4);
    assert_eq!(spec.selected_count, 2);
    assert_eq!(
        spec.instance_ids.as_deref().map(<[String]>::len),
        Some(2),
        "filter_spec.instance_ids should record what was requested"
    );
}

#[tokio::test]
async fn instance_ids_flag_with_unknown_id_exits_before_launch() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["only-real-id"]);

    let cfg = config_with_workdir(&repo);
    let err = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        config: cfg,
        resume: false,
        cost_limit_usd: None,
        filter: FilterArgs {
            instance_ids: Some(vec!["only-real-id".into(), "phantom-id".into()]),
            ..Default::default()
        },
        deterministic_responses: Some(submit_only_responses()),
        deterministic_usage_per_call: None,
    })
    .await
    .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("phantom-id"), "missing unknown id in error: {msg}");
    assert!(msg.contains("not present"), "should mention 'not present': {msg}");

    // No task should have run; no per-instance trajectory exists.
    assert!(!output.join("only-real-id.traj.json").exists());
    // results.json must not be written when the runner refuses to launch.
    assert!(!output.join("results.json").exists());
}

async fn run_sample_sweep(
    dataset: &Path,
    repo: &Path,
    out_root: &Path,
    tag: &str,
) -> Vec<String> {
    let output = out_root.join(tag);
    std::fs::create_dir_all(&output).unwrap();
    let cfg = config_with_workdir(repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset.to_owned(),
        output_dir: output,
        parallel: 1,
        config: cfg,
        resume: false,
        cost_limit_usd: None,
        filter: FilterArgs {
            sample: Some(4),
            seed: Some(99),
            ..Default::default()
        },
        deterministic_responses: Some(submit_only_responses()),
        deterministic_usage_per_call: None,
    })
    .await
    .unwrap();

    let mut got: Vec<String> = results
        .instances
        .iter()
        .map(|r| r.instance_id.clone())
        .collect();
    got.sort();
    got
}

#[tokio::test]
async fn sample_with_fixed_seed_is_reproducible_across_runs() {
    // Build a 12-instance dataset so a sample of 4 is small enough that
    // a different seed (or non-determinism) would obviously diverge.
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    std::fs::create_dir_all(work.path()).unwrap();

    let ids: Vec<String> = (0..12).map(|i| format!("inst-{i:02}")).collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    write_dataset(&dataset, &id_refs);

    let out_root = work.path().join("out");
    let a = run_sample_sweep(&dataset, &repo, &out_root, "run_a").await;
    let b = run_sample_sweep(&dataset, &repo, &out_root, "run_b").await;

    assert_eq!(a.len(), 4);
    assert_eq!(a, b, "same dataset + same --sample/--seed must select identical ids");
}

#[tokio::test]
async fn limit_truncates_after_other_filters() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    let ids: Vec<String> = (0..10).map(|i| format!("i{i:02}")).collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    write_dataset(&dataset, &id_refs);

    let cfg = config_with_workdir(&repo);
    let results = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 2,
        config: cfg,
        resume: false,
        cost_limit_usd: None,
        filter: FilterArgs {
            // Restrict to 6 ids, sample 5 of them, then take the first 2.
            instance_ids: Some(
                (0..6).map(|i| format!("i{i:02}")).collect(),
            ),
            sample: Some(5),
            seed: Some(1),
            limit: Some(2),
        },
        deterministic_responses: Some(submit_only_responses()),
        deterministic_usage_per_call: None,
    })
    .await
    .unwrap();

    assert_eq!(results.total, 2);
    for r in &results.instances {
        // Whatever the sample picked, all dispatched ids must come from
        // the i00..i05 set established by `--instance-ids`.
        let n: u32 = r
            .instance_id
            .trim_start_matches('i')
            .parse()
            .unwrap();
        assert!(n < 6, "unexpected id leaked past id-filter: {}", r.instance_id);
    }

    let raw = std::fs::read_to_string(output.join("results.json")).unwrap();
    let sweep: SweepResults = serde_json::from_str(&raw).unwrap();
    let spec = sweep.filter_spec.unwrap();
    assert_eq!(spec.original_count, 10);
    assert_eq!(spec.selected_count, 2);
    assert_eq!(spec.limit, Some(2));
    assert_eq!(spec.sample, Some(5));
    assert_eq!(spec.seed, Some(1));
}

#[tokio::test]
async fn empty_subset_errors_and_writes_no_results_json() {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let dataset = work.path().join("dataset.jsonl");
    let output = work.path().join("runs");
    std::fs::create_dir_all(&output).unwrap();

    write_dataset(&dataset, &["a", "b", "c"]);

    let cfg = config_with_workdir(&repo);
    let err = run(SwebenchArgs {
        dataset_path: dataset,
        output_dir: output.clone(),
        parallel: 1,
        config: cfg,
        resume: false,
        cost_limit_usd: None,
        filter: FilterArgs {
            limit: Some(0),
            ..Default::default()
        },
        deterministic_responses: Some(submit_only_responses()),
        deterministic_usage_per_call: None,
    })
    .await
    .unwrap_err();

    assert!(
        err.to_string().contains("zero instances"),
        "expected empty-set error, got: {err}"
    );
    assert!(!output.join("results.json").exists(), "results.json must not be written for an empty subset");
    assert!(!output.join("a.traj.json").exists());
}
