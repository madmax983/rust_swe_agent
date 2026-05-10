//! Integration tests for named SWE-bench dataset aliases and local cache (issue #97).
//!
//! RED-phase: these tests verify behaviour that the implementation must satisfy.
//! They are written before the implementation is complete; some will fail until
//! the GREEN phase wires them up.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rust_swe_agent::Config;
use rust_swe_agent::run::dataset::{
    CacheStatus, DatasetSource, DatasetSourceKind, SwebenchAlias, SwebenchSplit, cache_path_for,
    check_cache, resolve_dataset, write_cache,
};
use rust_swe_agent::run::swebench::{DatasetManifest, SwebenchArgs, run};

// ── helpers ────────────────────────────────────────────────────────────────

fn write_jsonl(path: &Path, ids: &[&str]) {
    let mut s = String::new();
    for id in ids {
        let _ = writeln!(
            s,
            "{{\"instance_id\":\"{id}\",\"problem_statement\":\"noop\"}}"
        );
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, s).unwrap();
}

fn base_args(dataset_source: DatasetSource, output_dir: PathBuf) -> SwebenchArgs {
    SwebenchArgs {
        dataset_source,
        dataset_cache_dir: PathBuf::from("/nonexistent-cache"),
        output_dir,
        parallel: 1,
        config: Config::defaults().unwrap(),
        reruns: 1,
        resume: false,
        cost_limit_usd: None,
        task_timeout_secs: None,
        instance_ids: None,
        limit: None,
        sample: None,
        seed: None,
        stratify_by: None,
        stratify_mode: rust_swe_agent::run::swebench::StratifyMode::Proportional,
        max_retries: 0,
        retry_on: None,
        retry_backoff_base_ms: 0,
        retry_backoff_cap_s: 0,
        retry_on_resume: false,
        deterministic_responses: Some(vec![
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\nok\n```".into(),
        ]),
        deterministic_usage_per_call: None,
        config_overlay_paths: vec![],
        dry_run: true,
        skip_preflight: true,
        preflight_format: "text".into(),
        skip_model_probe: true,
        preflight_check_timeout_s: 10,
        preflight_total_timeout_s: 60,
        preflight_mode: "test".into(),
        skip_patch_validation: true,
        max_rpm: None,
        max_input_tpm: None,
        cancel_deadline_secs: 5,
        install_os_signal_handlers: false,
        cancellation_signals: None,
        github_pr: None,
    }
}

// ── DatasetSource::LocalPath continues to work ─────────────────────────────

#[tokio::test]
async fn local_path_source_runs_sweep_dry_run() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("data.jsonl");
    write_jsonl(&dataset, &["local-1", "local-2"]);

    let output = work.path().join("out");
    let args = base_args(DatasetSource::LocalPath(dataset), output);
    let results = run(args).await.unwrap();
    assert_eq!(results.total, 0, "dry_run should not launch tasks");
}

// ── named alias cache-hit path ─────────────────────────────────────────────

#[tokio::test]
async fn named_alias_cache_hit_resolves_without_error() {
    let cache_dir = tempfile::tempdir().unwrap();
    let content = b"{\"instance_id\":\"v-1\",\"problem_statement\":\"p\"}\n\
                   {\"instance_id\":\"v-2\",\"problem_statement\":\"q\"}\n";
    write_cache(
        cache_dir.path(),
        &SwebenchAlias::Verified,
        &SwebenchSplit::Test,
        content,
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut args = base_args(
        DatasetSource::Named {
            alias: SwebenchAlias::Verified,
            split: SwebenchSplit::Test,
        },
        work.path().join("out"),
    );
    args.dataset_cache_dir = cache_dir.path().to_path_buf();

    let results = run(args).await.unwrap();
    // dry_run=true so total is 0, but no error means the cache was found
    assert_eq!(results.total, 0);
}

// ── named alias cache-miss produces actionable error ───────────────────────

#[tokio::test]
async fn named_alias_cache_miss_errors_before_any_task_launches() {
    let cache_dir = tempfile::tempdir().unwrap(); // empty cache
    let work = tempfile::tempdir().unwrap();
    let mut args = base_args(
        DatasetSource::Named {
            alias: SwebenchAlias::Lite,
            split: SwebenchSplit::Test,
        },
        work.path().join("out"),
    );
    args.dataset_cache_dir = cache_dir.path().to_path_buf();
    args.dry_run = false;
    args.skip_preflight = false; // let preflight run so it catches the miss early

    let err = run(args).await.unwrap_err();
    let msg = err.to_string();
    // must name the alias and the expected path in the error
    assert!(msg.contains("lite"), "error must name alias: {msg}");
    assert!(msg.contains("test"), "error must name split: {msg}");
    assert!(
        msg.contains("lite/test.jsonl"),
        "error must name cache path: {msg}"
    );
}

// ── corrupt cache entry produces distinct error ────────────────────────────

#[tokio::test]
async fn named_alias_corrupt_cache_produces_distinct_error() {
    let cache_dir = tempfile::tempdir().unwrap();
    // write a corrupt (non-JSON) file
    let path = cache_path_for(
        cache_dir.path(),
        &SwebenchAlias::Verified,
        &SwebenchSplit::Dev,
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"THIS IS NOT JSON AT ALL\n").unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut args = base_args(
        DatasetSource::Named {
            alias: SwebenchAlias::Verified,
            split: SwebenchSplit::Dev,
        },
        work.path().join("out"),
    );
    args.dataset_cache_dir = cache_dir.path().to_path_buf();
    args.dry_run = false;
    args.skip_preflight = false;

    let err = run(args).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("corrupt"), "must say corrupt: {msg}");
}

// ── invalid alias via resolve_dataset ─────────────────────────────────────

#[test]
fn invalid_alias_string_rejected_with_list_of_valid_aliases() {
    let err = "bogus_dataset".parse::<SwebenchAlias>().unwrap_err();
    assert!(err.contains("bogus_dataset"), "{err}");
    assert!(err.contains("full"), "{err}");
    assert!(err.contains("lite"), "{err}");
    assert!(err.contains("verified"), "{err}");
}

#[test]
fn invalid_split_string_rejected_with_list_of_valid_splits() {
    let err = "validation".parse::<SwebenchSplit>().unwrap_err();
    assert!(err.contains("validation"), "{err}");
    assert!(err.contains("train"), "{err}");
    assert!(err.contains("test"), "{err}");
    assert!(err.contains("dev"), "{err}");
}

// ── provenance recording ───────────────────────────────────────────────────

#[test]
fn dataset_manifest_source_kind_local_for_local_path() {
    let manifest = DatasetManifest {
        path: "/data/foo.jsonl".into(),
        sha256: "abc".into(),
        instance_count: 5,
        filter_spec: None,
        source_kind: "local".into(),
        alias: None,
        split: None,
        source_revision: None,
        cache_path: None,
        selected_row_count: 5,
        post_filter_row_count: 5,
    };
    assert_eq!(manifest.source_kind, "local");
    assert!(manifest.alias.is_none());
    assert!(manifest.split.is_none());
}

#[test]
fn dataset_manifest_source_kind_named_for_alias() {
    let manifest = DatasetManifest {
        path: "/cache/verified/test.jsonl".into(),
        sha256: "def".into(),
        instance_count: 10,
        filter_spec: None,
        source_kind: "named".into(),
        alias: Some("verified".into()),
        split: Some("test".into()),
        source_revision: Some("sha256:def".into()),
        cache_path: Some("/cache/verified/test.jsonl".into()),
        selected_row_count: 10,
        post_filter_row_count: 3,
    };
    assert_eq!(manifest.source_kind, "named");
    assert_eq!(manifest.alias.as_deref(), Some("verified"));
    assert_eq!(manifest.split.as_deref(), Some("test"));
    assert_eq!(manifest.selected_row_count, 10);
    assert_eq!(manifest.post_filter_row_count, 3);
    assert!(manifest.source_revision.is_some());
    assert!(manifest.cache_path.is_some());
}

// ── provenance in saved artifact ──────────────────────────────────────────

#[tokio::test]
async fn sweep_artifact_records_provenance_for_named_dataset() {
    let cache_dir = tempfile::tempdir().unwrap();
    let content = b"{\"instance_id\":\"prov-1\",\"problem_statement\":\"p\"}\n";
    write_cache(
        cache_dir.path(),
        &SwebenchAlias::Verified,
        &SwebenchSplit::Test,
        content,
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut args = base_args(
        DatasetSource::Named {
            alias: SwebenchAlias::Verified,
            split: SwebenchSplit::Test,
        },
        work.path().join("out"),
    );
    args.dataset_cache_dir = cache_dir.path().to_path_buf();
    // Run for real (not dry-run) but with deterministic model so no API calls
    args.dry_run = false;

    let results = run(args).await.unwrap();
    let manifest = results.manifest.expect("sweep must record a manifest");
    assert_eq!(manifest.dataset.source_kind, "named");
    assert_eq!(manifest.dataset.alias.as_deref(), Some("verified"));
    assert_eq!(manifest.dataset.split.as_deref(), Some("test"));
    assert!(
        manifest.dataset.cache_path.is_some(),
        "cache path must be recorded"
    );
    assert!(
        manifest.dataset.source_revision.is_some(),
        "source revision (content hash) must be recorded"
    );
}

#[tokio::test]
async fn sweep_artifact_records_provenance_for_local_dataset() {
    let work = tempfile::tempdir().unwrap();
    let dataset = work.path().join("data.jsonl");
    write_jsonl(&dataset, &["loc-1"]);

    let mut args = base_args(DatasetSource::LocalPath(dataset), work.path().join("out"));
    args.dry_run = false;

    let results = run(args).await.unwrap();
    let manifest = results.manifest.expect("sweep must record a manifest");
    assert_eq!(manifest.dataset.source_kind, "local");
    assert!(manifest.dataset.alias.is_none());
    assert!(manifest.dataset.split.is_none());
    assert!(manifest.dataset.cache_path.is_none());
}

// ── sampling parity: named vs local ──────────────────────────────────────

#[tokio::test]
async fn named_and_local_datasets_produce_identical_sampling_for_same_seed() {
    let cache_dir = tempfile::tempdir().unwrap();
    // build a 10-instance dataset
    let mut content = String::new();
    for i in 0..10 {
        let _ = writeln!(
            content,
            "{{\"instance_id\":\"samp-{i:02}\",\"problem_statement\":\"p{i}\"}}"
        );
    }
    let content_bytes = content.as_bytes();
    write_cache(
        cache_dir.path(),
        &SwebenchAlias::Lite,
        &SwebenchSplit::Test,
        content_bytes,
    )
    .unwrap();

    let local_file = cache_dir.path().join("local.jsonl");
    std::fs::write(&local_file, content_bytes).unwrap();

    async fn dry_run_with_sample(
        source: DatasetSource,
        cache_dir: PathBuf,
        out: PathBuf,
        sample: usize,
        seed: u64,
    ) -> rust_swe_agent::run::swebench::SweepResults {
        let mut args = base_args(source, out);
        args.dataset_cache_dir = cache_dir;
        args.sample = Some(sample);
        args.seed = Some(seed);
        args.dry_run = false;
        args.skip_preflight = true;
        run(args).await.unwrap()
    }

    let work = tempfile::tempdir().unwrap();
    let named_results = dry_run_with_sample(
        DatasetSource::Named {
            alias: SwebenchAlias::Lite,
            split: SwebenchSplit::Test,
        },
        cache_dir.path().to_path_buf(),
        work.path().join("named"),
        3,
        42,
    )
    .await;

    let local_results = dry_run_with_sample(
        DatasetSource::LocalPath(local_file),
        cache_dir.path().to_path_buf(),
        work.path().join("local"),
        3,
        42,
    )
    .await;

    // Both should select the same 3 instances
    let named_ids: Vec<_> = named_results
        .instances
        .iter()
        .map(|i| &i.instance_id)
        .collect();
    let local_ids: Vec<_> = local_results
        .instances
        .iter()
        .map(|i| &i.instance_id)
        .collect();
    assert_eq!(named_ids, local_ids, "sampling must be identical");
}

// ── bench doctor cache-status reporting ───────────────────────────────────

#[tokio::test]
async fn doctor_reports_cache_hit_without_launching_tasks() {
    let cache_dir = tempfile::tempdir().unwrap();
    write_cache(
        cache_dir.path(),
        &SwebenchAlias::Lite,
        &SwebenchSplit::Test,
        b"{\"instance_id\":\"dr-1\",\"problem_statement\":\"p\"}\n",
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut args = base_args(
        DatasetSource::Named {
            alias: SwebenchAlias::Lite,
            split: SwebenchSplit::Test,
        },
        work.path().join("out"),
    );
    args.dataset_cache_dir = cache_dir.path().to_path_buf();
    args.dry_run = true;
    args.skip_preflight = false;
    args.preflight_mode = "doctor".into();

    // doctor mode (dry_run=true) must complete without error
    let results = run(args).await.unwrap();
    assert_eq!(results.total, 0, "doctor must not launch any tasks");
}

#[tokio::test]
async fn doctor_reports_cache_miss_without_launching_tasks() {
    let cache_dir = tempfile::tempdir().unwrap(); // empty cache
    let work = tempfile::tempdir().unwrap();
    let mut args = base_args(
        DatasetSource::Named {
            alias: SwebenchAlias::Lite,
            split: SwebenchSplit::Test,
        },
        work.path().join("out"),
    );
    args.dataset_cache_dir = cache_dir.path().to_path_buf();
    args.dry_run = true;
    args.skip_preflight = false;
    args.preflight_mode = "doctor".into();

    // In doctor mode a cache miss must not return Err — it reports the miss as
    // a [WARN] check and exits cleanly so the operator can see the full report.
    let results = run(args)
        .await
        .expect("doctor cache-miss must return Ok, not Err");
    assert_eq!(
        results.total, 0,
        "doctor must not launch tasks on cache miss"
    );
}

// ── CLI alias / split arg parsing ─────────────────────────────────────────

#[test]
fn dataset_source_from_alias_and_split_strings() {
    let alias: SwebenchAlias = "verified".parse().unwrap();
    let split: SwebenchSplit = "test".parse().unwrap();
    let src = DatasetSource::Named {
        alias: alias.clone(),
        split: split.clone(),
    };
    assert_eq!(src.kind(), DatasetSourceKind::Named);
    if let DatasetSource::Named { alias: a, split: s } = src {
        assert_eq!(a, SwebenchAlias::Verified);
        assert_eq!(s, SwebenchSplit::Test);
    }
}

#[test]
fn dataset_source_local_path_kind_is_local() {
    let src = DatasetSource::LocalPath(PathBuf::from("foo.jsonl"));
    assert_eq!(src.kind(), DatasetSourceKind::Local);
}

// ── AC8: unsupported local file format ────────────────────────────────────

#[tokio::test]
async fn local_path_non_jsonl_format_produces_distinct_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let bad_file = dir.path().join("instances.csv");
    std::fs::write(
        &bad_file,
        "instance_id,problem_statement\ntask-1,fix the bug\n",
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut args = base_args(DatasetSource::LocalPath(bad_file), work.path().join("out"));
    args.skip_preflight = false; // must run preflight so the parse error surfaces

    let err = run(args).await.unwrap_err();
    let msg = err.to_string();
    // Must mention a line-level parse problem — distinct from cache-miss or I/O errors.
    assert!(
        msg.contains("line") || msg.contains("parse") || msg.contains("json"),
        "error must describe a parse/format problem, got: {msg}"
    );
    // Must NOT contain cache-miss language.
    assert!(
        !msg.contains("cache miss") && !msg.contains("not in cache"),
        "error must not be mistaken for a cache-miss: {msg}"
    );
}
