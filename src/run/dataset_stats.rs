//! Analysis logic for previewing SWE-bench dataset statistics offline (`bench dataset-stats`).

use comfy_table::Table;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::run::swebench::{SweBenchInstance, SweepResults};
use litellm_rs::utils::ai::counter::token_counter::TokenCounter;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsDistribution {
    pub min: usize,
    pub p50: usize,
    pub p90: usize,
    pub max: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalStatsDistribution {
    pub min: f64,
    pub p50: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoStats {
    pub repo: String,
    pub count: usize,
    pub percent_share: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubsetSelectorStats {
    pub limit: Option<usize>,
    pub sample: Option<usize>,
    pub seed: Option<u64>,
    pub instance_ids: Option<String>,
    pub stratify_by: Option<String>,
    pub stratify_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetStatsReport {
    pub schema_version: u32,
    pub dataset_hash: String,
    pub dataset_path: String,
    pub subset_selector: SubsetSelectorStats,
    pub total_instances: usize,
    pub repos: Vec<RepoStats>,
    pub problem_statement_tokens: StatsDistribution,
    pub expected_tests: StatsDistribution,
    pub languages: Vec<String>,
    pub historical_resolved_rate: Option<HistoricalStatsDistribution>,
    pub slice_skew: bool,
}

impl DatasetStatsReport {
    pub const SCHEMA_VERSION: u32 = 1;
}

/// Core function to compute SWE-bench dataset statistics offline.
#[allow(clippy::too_many_lines)]
pub fn compute_stats(
    slice_instances: &[SweBenchInstance],
    full_instances: &[SweBenchInstance],
    model: &str,
    runs_dir: &Path,
    dataset_hash: &Option<String>,
) -> Result<DatasetStatsReport, Error> {
    let total_instances = slice_instances.len();

    // 1. Calculate per-repo instance counts and percent share
    let mut repo_counts = BTreeMap::new();
    for inst in slice_instances {
        let repo = inst.repo.clone().unwrap_or_else(|| "unknown".to_owned());
        *repo_counts.entry(repo).or_insert(0) += 1;
    }
    let mut repos = Vec::new();
    for (repo, count) in repo_counts {
        #[allow(clippy::cast_precision_loss)]
        let percent_share = if total_instances > 0 {
            (count as f64 / total_instances as f64) * 100.0
        } else {
            0.0
        };
        repos.push(RepoStats {
            repo,
            count,
            percent_share,
        });
    }
    // Sort repos by count descending, then alphabetically by repo name
    repos.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.repo.cmp(&b.repo)));

    // 2. Count tokens in problem statement using TokenCounter
    let counter = TokenCounter::new();
    let mut slice_tokens = Vec::new();
    for inst in slice_instances {
        let text = inst.problem_statement.as_deref().unwrap_or("");
        let count = match counter.count_completion_tokens(model, text) {
            Ok(est) => est.total_tokens as usize,
            Err(_) => text.split_whitespace().count(), // Fallback
        };
        slice_tokens.push(count);
    }
    slice_tokens.sort_unstable();

    let problem_statement_tokens = StatsDistribution {
        min: slice_tokens.first().copied().unwrap_or(0),
        p50: median(&slice_tokens),
        p90: percentile_90(&slice_tokens),
        max: slice_tokens.last().copied().unwrap_or(0),
    };

    // Calculate full dataset median for token length skew checking
    let mut full_tokens = Vec::new();
    for inst in full_instances {
        let text = inst.problem_statement.as_deref().unwrap_or("");
        let count = match counter.count_completion_tokens(model, text) {
            Ok(est) => est.total_tokens as usize,
            Err(_) => text.split_whitespace().count(),
        };
        full_tokens.push(count);
    }
    full_tokens.sort_unstable();
    let full_median = median(&full_tokens);

    // 3. Count expected tests from FAIL_TO_PASS and PASS_TO_PASS
    let mut expected_tests_list = Vec::new();
    for inst in slice_instances {
        expected_tests_list.push(get_expected_tests_count(inst));
    }
    expected_tests_list.sort_unstable();

    let expected_tests = StatsDistribution {
        min: expected_tests_list.first().copied().unwrap_or(0),
        p50: median(&expected_tests_list),
        p90: percentile_90(&expected_tests_list),
        max: expected_tests_list.last().copied().unwrap_or(0),
    };

    // 4. Multi-tiered language detection
    let languages = detect_languages(slice_instances);

    // 5. Historical resolved-rate
    let historical_rates =
        analyze_historical_resolved_rates(slice_instances, runs_dir, dataset_hash.as_ref());
    let historical_resolved_rate = if historical_rates.is_empty() {
        None
    } else {
        let min = historical_rates.first().copied().unwrap_or(0.0);
        let max = historical_rates.last().copied().unwrap_or(0.0);
        let p50 = median_f64(&historical_rates);
        Some(HistoricalStatsDistribution { min, p50, max })
    };

    // 6. Skew and representativeness analysis
    let slice_repos: BTreeSet<_> = slice_instances
        .iter()
        .filter_map(|i| i.repo.as_ref())
        .collect();
    let full_repos: BTreeSet<_> = full_instances
        .iter()
        .filter_map(|i| i.repo.as_ref())
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let repo_skew = if full_repos.is_empty() {
        false
    } else {
        (slice_repos.len() as f64) < 0.50 * (full_repos.len() as f64)
    };

    #[allow(clippy::cast_precision_loss)]
    let token_skew = if full_median == 0 {
        false
    } else {
        let diff = (problem_statement_tokens.p50 as f64 - full_median as f64).abs();
        diff > 0.25 * (full_median as f64)
    };

    let slice_skew = repo_skew || token_skew;

    Ok(DatasetStatsReport {
        schema_version: DatasetStatsReport::SCHEMA_VERSION,
        dataset_hash: dataset_hash.clone().unwrap_or_default(),
        dataset_path: String::new(), // Populated by CLI driver
        subset_selector: SubsetSelectorStats {
            limit: None,
            sample: None,
            seed: None,
            instance_ids: None,
            stratify_by: None,
            stratify_mode: None,
        },
        total_instances,
        repos,
        problem_statement_tokens,
        expected_tests,
        languages,
        historical_resolved_rate,
        slice_skew,
    })
}

fn median(values: &[usize]) -> usize {
    if values.is_empty() {
        return 0;
    }
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        values[mid - 1].midpoint(values[mid])
    } else {
        values[mid]
    }
}

fn percentile_90(values: &[usize]) -> usize {
    if values.is_empty() {
        return 0;
    }
    let idx = (values.len() - 1) * 9 / 10;
    values[idx]
}

fn median_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        values[mid - 1].midpoint(values[mid])
    } else {
        values[mid]
    }
}

fn get_expected_tests_count(inst: &SweBenchInstance) -> usize {
    let mut total = 0;
    for key in &["FAIL_TO_PASS", "PASS_TO_PASS"] {
        if let Some(val) = inst.other.get(*key) {
            let items: Vec<serde_json::Value> = if let Some(arr) = val.as_array() {
                arr.clone()
            } else if let Some(s) = val.as_str() {
                serde_json::from_str(s).unwrap_or_default()
            } else {
                vec![]
            };
            total += items.iter().filter_map(|v| v.as_str()).count();
        }
    }
    total
}

fn detect_languages(instances: &[SweBenchInstance]) -> Vec<String> {
    let mut counts = BTreeMap::new();
    for inst in instances {
        let mut detected = false;
        // 1. Scan diff strings (patch and test_patch)
        for key in &["patch", "test_patch"] {
            if let Some(val) = inst.other.get(*key).and_then(|v| v.as_str()) {
                for line in val.lines() {
                    if line.starts_with("--- a/") || line.starts_with("+++ b/") {
                        if let Some(ext) = line.split('.').next_back() {
                            let ext_clean: String = ext
                                .chars()
                                .filter(|c| c.is_alphanumeric())
                                .flat_map(char::to_lowercase)
                                .collect();
                            let lang = match ext_clean.as_str() {
                                "py" => Some("Python"),
                                "rs" => Some("Rust"),
                                "go" => Some("Go"),
                                "java" => Some("Java"),
                                "js" | "jsx" => Some("JavaScript"),
                                "ts" | "tsx" => Some("TypeScript"),
                                "c" | "h" => Some("C"),
                                "cpp" | "hpp" | "cc" | "cxx" => Some("C++"),
                                _ => None,
                            };
                            if let Some(l) = lang {
                                *counts.entry(l.to_string()).or_insert(0) += 1;
                                detected = true;
                            }
                        }
                    }
                }
            }
        }
        // 2. Fall back to repository name mapping
        if !detected {
            if let Some(repo) = &inst.repo {
                let repo_lower = repo.to_lowercase();
                let lang = if repo_lower.contains("django")
                    || repo_lower.contains("pytest")
                    || repo_lower.contains("pandas")
                    || repo_lower.contains("sympy")
                    || repo_lower.contains("sphinx")
                    || repo_lower.contains("flask")
                    || repo_lower.contains("requests")
                    || repo_lower.contains("pylint")
                    || repo_lower.contains("matplotlib")
                    || repo_lower.contains("scikit-learn")
                    || repo_lower.contains("astropy")
                {
                    Some("Python")
                } else {
                    None
                };
                if let Some(l) = lang {
                    *counts.entry(l.to_string()).or_insert(0) += 1;
                    detected = true;
                }
            }
        }
        // 3. Absolute fallback
        if !detected {
            *counts.entry("Python".to_string()).or_insert(0) += 1;
        }
    }

    let mut list: Vec<(String, usize)> = counts.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    list.into_iter().map(|(l, _)| l).collect()
}

fn find_results_jsons(dir: &Path, paths: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                find_results_jsons(&path, paths);
            } else if path.file_name() == Some(OsStr::new("results.json")) {
                paths.push(path);
            }
        }
    }
}

fn analyze_historical_resolved_rates(
    instances: &[SweBenchInstance],
    runs_dir: &Path,
    dataset_hash: Option<&String>,
) -> Vec<f64> {
    if !runs_dir.is_dir() {
        return Vec::new();
    }
    let mut results_paths = Vec::new();
    find_results_jsons(runs_dir, &mut results_paths);

    // Map instance_id -> (sum_resolved, sum_runs)
    let mut hist_map = HashMap::new();

    for path in results_paths {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(results) = serde_json::from_str::<SweepResults>(&text) {
                let matches = match (dataset_hash, &results.manifest) {
                    (Some(curr_hash), Some(manifest)) => manifest.dataset.sha256 == **curr_hash,
                    (Some(_), None) => false,
                    _ => true,
                };
                if matches {
                    for inst_res in &results.instances {
                        let entry = hist_map
                            .entry(inst_res.instance_id.clone())
                            .or_insert((0u32, 0u32));
                        let runs = if inst_res.runs == 0 { 1 } else { inst_res.runs };
                        let resolved = if inst_res.runs == 0 {
                            u32::from(
                                inst_res.pass_at_1
                                    || inst_res.outcome.as_deref() == Some("resolved")
                                    || (inst_res.outcome.as_deref() == Some("submitted")
                                        && inst_res.failure_category.is_none()),
                            )
                        } else {
                            inst_res.resolved_count
                        };
                        entry.0 += resolved;
                        entry.1 += runs;
                    }
                }
            }
        }
    }

    let mut rates = Vec::new();
    for inst in instances {
        if let Some(&(resolved, runs)) = hist_map.get(&inst.instance_id) {
            if runs > 0 {
                rates.push(f64::from(resolved) / f64::from(runs));
            }
        }
    }
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    rates
}

/// Render statistics Cozy Table format.
pub fn render_text(report: &DatasetStatsReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\n=== SWE-bench Dataset Statistics Preview ===");
    let _ = writeln!(out, "Total Instances: {}", report.total_instances);
    let _ = writeln!(out, "Languages Present: {}", report.languages.join(", "));
    let _ = writeln!(out, "\n--- Repository Share ---");

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["Repository", "Instance Count", "Percent Share"]);

    for repo in &report.repos {
        table.add_row(vec![
            repo.repo.clone(),
            repo.count.to_string(),
            format!("{:.2}%", repo.percent_share),
        ]);
    }
    let _ = writeln!(out, "{table}");

    let _ = writeln!(out, "\n--- Problem-Statement Tokens Distribution ---");
    let _ = writeln!(
        out,
        "  Min: {}  |  P50: {}  |  P90: {}  |  Max: {}",
        report.problem_statement_tokens.min,
        report.problem_statement_tokens.p50,
        report.problem_statement_tokens.p90,
        report.problem_statement_tokens.max,
    );

    let _ = writeln!(out, "\n--- Expected Tests Distribution ---");
    let _ = writeln!(
        out,
        "  Min: {}  |  P50: {}  |  P90: {}  |  Max: {}",
        report.expected_tests.min,
        report.expected_tests.p50,
        report.expected_tests.p90,
        report.expected_tests.max,
    );

    let _ = writeln!(out, "\n--- Historical Sweep Resolved Rates ---");
    if let Some(hist) = &report.historical_resolved_rate {
        let _ = writeln!(
            out,
            "  Min: {:.4}  |  P50: {:.4}  |  Max: {:.4}",
            hist.min, hist.p50, hist.max,
        );
    } else {
        let _ = writeln!(
            out,
            "  No prior completed sweeps found matching current dataset hash."
        );
    }

    if report.slice_skew {
        let _ = writeln!(
            out,
            "\n[WARNING] Slice Skew detected! The selected subset may be unrepresentative of the full dataset."
        );
    }

    out
}
