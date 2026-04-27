//! Dataset subsetting for `bench swebench`.
//!
//! Lets operators iterate quickly by running a sweep against a
//! reproducible subset of the dataset rather than the full JSONL.
//! Composition order is fixed and documented:
//!
//!   instance-ids filter → sample → limit
//!
//! `--instance-ids` selects a hand-picked set (comma-list or `@file`).
//! `--sample N` then reproducibly picks N at random using `--seed`.
//! `--limit N` finally truncates to the first N. Sample without seed
//! is rejected up front so two operators with the same flags get the
//! same instance set on the same dataset.
//!
//! The resolved [`FilterSpec`] is persisted in `results.json` so a
//! reviewer can identify exactly which subset was run without
//! re-deriving it from the raw dataset.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error};
use crate::run::swebench::SweBenchInstance;

/// Parsed filter inputs from the CLI. Empty/`None` means "no filtering".
#[derive(Debug, Clone, Default)]
pub struct FilterArgs {
    /// Explicit instance ids to keep, already parsed (comma-list or `@file`).
    pub instance_ids: Option<Vec<String>>,
    /// Truncate the post-sample set to at most this many instances.
    pub limit: Option<usize>,
    /// Random sample size; reproducible under `seed`.
    pub sample: Option<usize>,
    /// RNG seed for `--sample`. Required when `sample` is `Some`.
    pub seed: Option<u64>,
}

impl FilterArgs {
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.instance_ids.is_some() || self.limit.is_some() || self.sample.is_some()
    }
}

/// Resolved filter outcome, persisted in `results.json`. Records both
/// the request (`instance_ids` / `limit` / `sample` / `seed`) and the
/// effect (`original_count` → `selected_count`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilterSpec {
    /// Instances loaded from the JSONL before any filtering.
    pub original_count: usize,
    /// Instances actually dispatched after the full filter pipeline.
    pub selected_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// Parse the raw `--instance-ids` argument.
///
/// Two forms:
/// * comma-separated literal: `"id1,id2,id3"`
/// * `@path/to/file.txt`: one id per line, blanks/whitespace ignored
///
/// Trailing/leading whitespace and empty entries are dropped. Returns
/// `Err` on filesystem errors and on a fully-empty resolved list (so a
/// typo in an `@path` doesn't silently match the entire dataset).
pub fn parse_instance_ids_arg(arg: &str) -> Result<Vec<String>, Error> {
    let arg = arg.trim();
    let raw = if let Some(p) = arg.strip_prefix('@') {
        let p = p.trim();
        std::fs::read_to_string(p).map_err(|e| {
            Error::Config(ConfigError::Invalid(format!(
                "--instance-ids: cannot read `{p}`: {e}"
            )))
        })?
    } else {
        // Normalize commas to newlines so the same trim/dedupe path
        // handles both forms.
        arg.replace(',', "\n")
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut ids: Vec<String> = Vec::new();
    for line in raw.lines() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        if seen.insert(s.to_owned()) {
            ids.push(s.to_owned());
        }
    }
    if ids.is_empty() {
        return Err(Error::Config(ConfigError::Invalid(
            "--instance-ids resolved to an empty id list".into(),
        )));
    }
    Ok(ids)
}

/// Apply the filter pipeline. Order: instance-ids → sample → limit.
///
/// Errors:
/// * `--sample` set without `--seed`
/// * `--instance-ids` references ids absent from the dataset
/// * resolved subset is empty
pub fn apply_filter(
    instances: Vec<SweBenchInstance>,
    args: &FilterArgs,
) -> Result<(Vec<SweBenchInstance>, FilterSpec), Error> {
    let original_count = instances.len();

    if args.sample.is_some() && args.seed.is_none() {
        return Err(Error::Config(ConfigError::Invalid(
            "--sample requires --seed for reproducibility (same seed + same dataset → identical subset)".into(),
        )));
    }

    // 1. Filter to the explicit id list, in dataset order. Preserving
    //    dataset order keeps dispatch deterministic across teammates
    //    even when the id list itself was hand-typed in a different
    //    order.
    let mut filtered: Vec<SweBenchInstance> = if let Some(ids) = &args.instance_ids {
        let requested: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let present: HashSet<&str> = instances
            .iter()
            .map(|i| i.instance_id.as_str())
            .collect();
        let mut unknown: Vec<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|id| !present.contains(id))
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            return Err(Error::Config(ConfigError::Invalid(format!(
                "--instance-ids: {} id(s) not present in dataset: {}",
                unknown.len(),
                unknown.join(", ")
            ))));
        }
        instances
            .into_iter()
            .filter(|i| requested.contains(i.instance_id.as_str()))
            .collect()
    } else {
        instances
    };

    // 2. Reproducibly random-sample. When the requested size meets or
    //    exceeds the available count, sampling is a no-op — we record
    //    the request in `FilterSpec.sample` regardless, so a reviewer
    //    can see what was asked for.
    if let Some(n) = args.sample {
        if n < filtered.len() {
            // Unwrap is safe: we validated `seed.is_some()` above.
            let seed = args.seed.unwrap_or(0);
            filtered = sample_in_order(filtered, n, seed);
        }
    }

    // 3. Truncate to first N. Applied last so the limit has a stable
    //    meaning regardless of whether sampling ran.
    if let Some(n) = args.limit {
        if filtered.len() > n {
            filtered.truncate(n);
        }
    }

    if filtered.is_empty() {
        return Err(Error::Config(ConfigError::Invalid(
            "filter resolved to zero instances; nothing to run".into(),
        )));
    }

    let spec = FilterSpec {
        original_count,
        selected_count: filtered.len(),
        instance_ids: args.instance_ids.clone(),
        limit: args.limit,
        sample: args.sample,
        seed: args.seed,
    };

    Ok((filtered, spec))
}

/// Pick `n` distinct entries from `instances` using `seed`, preserving
/// the original dataset order in the output. Order preservation makes
/// dispatch order independent of the RNG's internal index permutation.
fn sample_in_order(instances: Vec<SweBenchInstance>, n: usize, seed: u64) -> Vec<SweBenchInstance> {
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let len = instances.len();
    let mut idxs: Vec<usize> = rand::seq::index::sample(&mut rng, len, n).into_vec();
    idxs.sort_unstable();
    let mut take = idxs.into_iter().peekable();
    let mut out = Vec::with_capacity(n);
    for (i, inst) in instances.into_iter().enumerate() {
        match take.peek() {
            Some(&t) if t == i => {
                out.push(inst);
                take.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn inst(id: &str) -> SweBenchInstance {
        SweBenchInstance {
            instance_id: id.into(),
            repo: None,
            base_commit: None,
            problem_statement: None,
            image: None,
            other: serde_json::Map::new(),
        }
    }

    fn ids(v: &[SweBenchInstance]) -> Vec<String> {
        v.iter().map(|i| i.instance_id.clone()).collect()
    }

    #[test]
    fn no_filter_is_identity() {
        let xs = vec![inst("a"), inst("b"), inst("c")];
        let (got, spec) = apply_filter(xs, &FilterArgs::default()).unwrap();
        assert_eq!(ids(&got), vec!["a", "b", "c"]);
        assert_eq!(spec.original_count, 3);
        assert_eq!(spec.selected_count, 3);
        assert_eq!(spec.instance_ids, None);
        assert_eq!(spec.limit, None);
        assert_eq!(spec.sample, None);
        assert_eq!(spec.seed, None);
    }

    #[test]
    fn instance_ids_filter_keeps_only_matches_in_dataset_order() {
        let xs = vec![inst("a"), inst("b"), inst("c"), inst("d")];
        let args = FilterArgs {
            instance_ids: Some(vec!["c".into(), "a".into()]),
            ..Default::default()
        };
        let (got, spec) = apply_filter(xs, &args).unwrap();
        // Output preserves dataset order, not request order.
        assert_eq!(ids(&got), vec!["a", "c"]);
        assert_eq!(spec.selected_count, 2);
        assert_eq!(spec.original_count, 4);
    }

    #[test]
    fn instance_ids_filter_rejects_unknown_ids() {
        let xs = vec![inst("a"), inst("b")];
        let args = FilterArgs {
            instance_ids: Some(vec!["a".into(), "ghost".into(), "spook".into()]),
            ..Default::default()
        };
        let err = apply_filter(xs, &args).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghost"), "missing ghost in: {msg}");
        assert!(msg.contains("spook"), "missing spook in: {msg}");
        assert!(msg.contains("2 id(s) not present"), "bad msg: {msg}");
    }

    #[test]
    fn sample_requires_seed() {
        let xs = vec![inst("a"), inst("b")];
        let args = FilterArgs {
            sample: Some(1),
            seed: None,
            ..Default::default()
        };
        let err = apply_filter(xs, &args).unwrap_err();
        assert!(
            err.to_string().contains("--sample requires --seed"),
            "got: {err}"
        );
    }

    #[test]
    fn sample_is_reproducible_under_fixed_seed() {
        let xs: Vec<_> = (0..50).map(|i| inst(&format!("inst-{i:02}"))).collect();
        let args = FilterArgs {
            sample: Some(5),
            seed: Some(42),
            ..Default::default()
        };
        let (a, _) = apply_filter(xs.clone(), &args).unwrap();
        let (b, _) = apply_filter(xs.clone(), &args).unwrap();
        assert_eq!(ids(&a), ids(&b), "same seed + same dataset must match");
        assert_eq!(a.len(), 5);

        // A different seed should (very likely) pick a different subset.
        let other = FilterArgs {
            sample: Some(5),
            seed: Some(43),
            ..Default::default()
        };
        let (c, _) = apply_filter(xs, &other).unwrap();
        assert_ne!(
            ids(&a),
            ids(&c),
            "different seed should pick a different subset (50C5 collision is astronomically unlikely)"
        );
    }

    #[test]
    fn sample_larger_than_dataset_is_no_op() {
        let xs = vec![inst("a"), inst("b"), inst("c")];
        let args = FilterArgs {
            sample: Some(100),
            seed: Some(0),
            ..Default::default()
        };
        let (got, spec) = apply_filter(xs, &args).unwrap();
        assert_eq!(ids(&got), vec!["a", "b", "c"]);
        // Spec records the *request*, not the outcome.
        assert_eq!(spec.sample, Some(100));
        assert_eq!(spec.selected_count, 3);
    }

    #[test]
    fn limit_truncates_after_sample() {
        let xs: Vec<_> = (0..20).map(|i| inst(&format!("i{i:02}"))).collect();
        let args = FilterArgs {
            sample: Some(8),
            seed: Some(7),
            limit: Some(3),
            ..Default::default()
        };
        let (got, spec) = apply_filter(xs, &args).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(spec.selected_count, 3);
        assert_eq!(spec.sample, Some(8));
        assert_eq!(spec.limit, Some(3));
    }

    #[test]
    fn limit_alone_keeps_first_n_in_order() {
        let xs = vec![inst("a"), inst("b"), inst("c"), inst("d")];
        let args = FilterArgs {
            limit: Some(2),
            ..Default::default()
        };
        let (got, _) = apply_filter(xs, &args).unwrap();
        assert_eq!(ids(&got), vec!["a", "b"]);
    }

    #[test]
    fn instance_ids_then_sample_then_limit_compose() {
        let xs: Vec<_> = (0..20).map(|i| inst(&format!("i{i:02}"))).collect();
        let args = FilterArgs {
            instance_ids: Some(
                (0..10).map(|i| format!("i{i:02}")).collect(),
            ),
            sample: Some(6),
            seed: Some(123),
            limit: Some(3),
        };
        let (got, spec) = apply_filter(xs, &args).unwrap();
        assert_eq!(got.len(), 3);
        for inst in &got {
            // All must be from the first ten ids (instance-ids filter).
            assert!(inst.instance_id.starts_with('i'));
            let n: u32 = inst.instance_id.trim_start_matches('i').parse().unwrap();
            assert!(n < 10, "limit/sample leaked an out-of-filter id: {}", inst.instance_id);
        }
        assert_eq!(spec.original_count, 20);
        assert_eq!(spec.selected_count, 3);
    }

    #[test]
    fn empty_set_after_filter_errors() {
        let xs = vec![inst("a"), inst("b")];
        let args = FilterArgs {
            limit: Some(0),
            ..Default::default()
        };
        let err = apply_filter(xs, &args).unwrap_err();
        assert!(
            err.to_string().contains("zero instances"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_instance_ids_comma_list() {
        let got = parse_instance_ids_arg("a, b ,c").unwrap();
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_instance_ids_dedupes() {
        let got = parse_instance_ids_arg("a,b,a,c,b").unwrap();
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_instance_ids_at_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ids.txt");
        std::fs::write(&p, "alpha\n  beta  \n\n\ngamma\n").unwrap();
        let arg = format!("@{}", p.display());
        let got = parse_instance_ids_arg(&arg).unwrap();
        assert_eq!(got, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn parse_instance_ids_empty_errors() {
        let err = parse_instance_ids_arg(" ,, ,").unwrap_err();
        assert!(err.to_string().contains("empty id list"), "got: {err}");
    }

    #[test]
    fn parse_instance_ids_missing_file_errors() {
        let err = parse_instance_ids_arg("@/no/such/path/ids.txt").unwrap_err();
        assert!(err.to_string().contains("cannot read"), "got: {err}");
    }
}
