use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::cli::args::BisectCmd;
use crate::error::Error;
use crate::run::swebench::{ProvenanceManifest, SweepResults};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitResult {
    pub commit_sha: String,
    pub resolved: usize,
    pub errored: usize,
    pub cost_usd: f64,
    pub wallclock_secs: f64,
    pub smoke_artifact_path: String,
    pub cache_reuse_count: usize,
    pub status: String, // "good" | "bad" | "schema_break" | "errored"
    pub systemic_halt_category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BisectState {
    pub schema_version: String,
    pub good_sha: String,
    pub bad_sha: String,
    pub commits_visited: Vec<String>,
    pub per_commit: BTreeMap<String, CommitResult>,
    pub suspect_commit: Option<String>,
    pub total_cost: f64,
    pub total_wallclock: f64,
    pub cache_reuse_count: usize,
    pub schema_breaks: Vec<String>,
    pub outcome: Option<String>,
}

impl Default for BisectState {
    fn default() -> Self {
        Self {
            schema_version: "1.0.0".to_owned(),
            good_sha: String::new(),
            bad_sha: String::new(),
            commits_visited: Vec::new(),
            per_commit: BTreeMap::new(),
            suspect_commit: None,
            total_cost: 0.0,
            total_wallclock: 0.0,
            cache_reuse_count: 0,
            schema_breaks: Vec::new(),
            outcome: None,
        }
    }
}

/// A Drop guard to restore the original branch/HEAD commit when exiting
struct GitRestoreGuard {
    original_head: String,
}

impl Drop for GitRestoreGuard {
    fn drop(&mut self) {
        if !self.original_head.is_empty() {
            println!(
                "[bisect] Restoring original Git HEAD: {}",
                self.original_head
            );
            let _ = Command::new("git")
                .args(["checkout", "-f", &self.original_head])
                .output();
        }
    }
}

fn get_current_git_head() -> Result<String, Error> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(Error::Io)?;
    if !out.status.success() {
        return Err(Error::Preflight(
            "failed to get current git HEAD".to_owned(),
        ));
    }
    let ref_name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if ref_name == "HEAD" {
        // Detached HEAD, get the exact SHA
        let sha_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(Error::Io)?;
        Ok(String::from_utf8_lossy(&sha_out.stdout).trim().to_owned())
    } else {
        Ok(ref_name)
    }
}

fn is_git_working_tree_clean() -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.is_empty())
}

fn load_sweep_results(dir: &Path) -> Result<SweepResults, Error> {
    let path = dir.join("results.json");
    if !path.exists() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "Sweep results file not found at {}",
            path.display()
        ))));
    }
    let file = std::fs::File::open(&path).map_err(Error::Io)?;
    serde_json::from_reader(std::io::BufReader::new(file)).map_err(Error::Json)
}

fn hash_manifest(manifest: &ProvenanceManifest) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_string(manifest).unwrap_or_default();
    let hash = Sha256::digest(json.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in hash {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    format!("sha256:{hex}")
}

fn parse_hex_seed(hex: &str) -> u64 {
    let hex_trimmed = hex.strip_prefix("sha256:").unwrap_or(hex);
    let slice = if hex_trimmed.len() >= 16 {
        &hex_trimmed[..16]
    } else {
        hex_trimmed
    };
    u64::from_str_radix(slice, 16).unwrap_or(42)
}

#[allow(dead_code)]
fn simple_hash(s: &str) -> u64 {
    let mut x = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        x ^= u64::from(b);
        x = x.wrapping_mul(0x1000_0000_01b3);
    }
    x
}

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

/// Retrieve the CURRENT ArtifactSchemaVersion from the text of src/artifact.rs
fn read_current_schema_version_from_file(artifact_file_path: &Path) -> Option<(u16, u16)> {
    if !artifact_file_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(artifact_file_path).ok()?;
    // Search for major: <num> and minor: <num> in the definition of CURRENT
    let re =
        regex::Regex::new(r"CURRENT:\s*Self\s*=\s*Self\s*\{\s*major:\s*(\d+),\s*minor:\s*(\d+)")
            .ok()?;
    if let Some(caps) = re.captures(&content) {
        let major = caps.get(1)?.as_str().parse::<u16>().ok()?;
        let minor = caps.get(2)?.as_str().parse::<u16>().ok()?;
        return Some((major, minor));
    }
    // Try simpler search if formatting differs
    let major_re = regex::Regex::new(r"major:\s*(\d+)").ok()?;
    let minor_re = regex::Regex::new(r"minor:\s*(\d+)").ok()?;
    let major = major_re
        .captures(&content)?
        .get(1)?
        .as_str()
        .parse::<u16>()
        .ok()?;
    let minor = minor_re
        .captures(&content)?
        .get(1)?
        .as_str()
        .parse::<u16>()
        .ok()?;
    Some((major, minor))
}

#[allow(
    clippy::unused_async,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub async fn run(args: &BisectCmd) -> Result<(), Error> {
    // 1. If not clean, refuse to run (to prevent data loss on checkout)
    let is_test = std::env::var("MAX_BISECT_TEST_ENV").is_ok();
    if !is_test && !is_git_working_tree_clean() {
        return Err(Error::Preflight(
            "Git working tree is dirty; commit or stash changes before running bench bisect"
                .to_owned(),
        ));
    }

    // 2. Load original Git HEAD
    let original_head = if is_test {
        String::new()
    } else {
        get_current_git_head()?
    };
    let _restore_guard = GitRestoreGuard { original_head };

    // 3. Load good/bad or resume state
    let mut state = BisectState::default();
    if let Some(ref resume_path) = args.resume {
        if resume_path.exists() {
            println!(
                "[bisect] Resuming from bisect state file: {}",
                resume_path.display()
            );
            let content = std::fs::read_to_string(resume_path).map_err(Error::Io)?;
            state = serde_json::from_str(&content).map_err(Error::Json)?;
        } else {
            println!(
                "[bisect] Starting new bisect run, output will be written to: {}",
                resume_path.display()
            );
            let good_sweep = load_sweep_results(&args.good)?;
            let bad_sweep = load_sweep_results(&args.bad)?;

            let good_manifest = good_sweep.manifest.ok_or_else(|| {
                Error::Config(crate::error::ConfigError::Invalid(
                    "Good sweep directory results.json is missing 'manifest'".to_owned(),
                ))
            })?;
            let bad_manifest = bad_sweep.manifest.ok_or_else(|| {
                Error::Config(crate::error::ConfigError::Invalid(
                    "Bad sweep directory results.json is missing 'manifest'".to_owned(),
                ))
            })?;

            let good_sha = good_manifest.harness.git_sha.ok_or_else(|| {
                Error::Config(crate::error::ConfigError::Invalid(
                    "Good sweep manifest is missing harness git_sha".to_owned(),
                ))
            })?;
            let bad_sha = bad_manifest.harness.git_sha.ok_or_else(|| {
                Error::Config(crate::error::ConfigError::Invalid(
                    "Bad sweep manifest is missing harness git_sha".to_owned(),
                ))
            })?;

            state.good_sha = good_sha;
            state.bad_sha = bad_sha;
        }
    } else {
        let good_sweep = load_sweep_results(&args.good)?;
        let bad_sweep = load_sweep_results(&args.bad)?;

        let good_manifest = good_sweep.manifest.ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(
                "Good sweep directory results.json is missing 'manifest'".to_owned(),
            ))
        })?;
        let bad_manifest = bad_sweep.manifest.ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(
                "Bad sweep directory results.json is missing 'manifest'".to_owned(),
            ))
        })?;

        let good_sha = good_manifest.harness.git_sha.ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(
                "Good sweep manifest is missing harness git_sha".to_owned(),
            ))
        })?;
        let bad_sha = bad_manifest.harness.git_sha.ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Invalid(
                "Bad sweep manifest is missing harness git_sha".to_owned(),
            ))
        })?;

        state.good_sha = good_sha;
        state.bad_sha = bad_sha;
    }

    println!("[bisect] Good commit: {}", state.good_sha);
    println!("[bisect] Bad commit: {}", state.bad_sha);

    // 4. Retrieve commit list to walk in O(log N)
    // Runs git rev-list --topo-order --reverse good_sha..bad_sha
    let commit_list = if is_test {
        // In test environment, the commit list can be passed via env or mocked
        if let Ok(commits_env) = std::env::var("MAX_BISECT_MOCK_COMMITS") {
            commits_env.split(',').map(str::to_owned).collect()
        } else {
            vec![state.good_sha.clone(), state.bad_sha.clone()]
        }
    } else {
        let out = Command::new("git")
            .args([
                "rev-list",
                "--topo-order",
                "--reverse",
                &format!("{}..{}", state.good_sha, state.bad_sha),
            ])
            .output()
            .map_err(Error::Io)?;
        if !out.status.success() {
            return Err(Error::Preflight(format!(
                "Failed to get commit range: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let list_str = String::from_utf8_lossy(&out.stdout);
        let list: Vec<String> = list_str
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        list
    };

    if commit_list.is_empty() {
        println!(
            "[bisect] No commits in the range. Good and Bad commits might be identical or Good is not an ancestor of Bad."
        );
        return Ok(());
    }

    println!(
        "[bisect] Range has {} candidate commits to walk.",
        commit_list.len()
    );

    // Load good sweep dataset properties for reproducible subset selection
    let good_sweep = load_sweep_results(&args.good)?;
    let manifest = good_sweep.manifest.as_ref().ok_or_else(|| {
        Error::Config(crate::error::ConfigError::Invalid(
            "Good sweep is missing manifest".to_owned(),
        ))
    })?;

    // Derive stable seed if not provided
    let smoke_seed = args.smoke_seed.unwrap_or_else(|| {
        let hash = hash_manifest(manifest);
        parse_hex_seed(&hash)
    });

    // Select N smoke instances reproducibly
    let mut all_instances: Vec<String> = good_sweep
        .instances
        .iter()
        .map(|inst| inst.instance_id.clone())
        .collect();
    all_instances.sort();

    let mut rng = XorShift64::new(smoke_seed);
    let mut shuffled = all_instances.clone();
    for i in (1..shuffled.len()).rev() {
        let j = (rng.next_u64() as usize) % (i + 1);
        shuffled.swap(i, j);
    }
    let smoke_instances_subset: Vec<String> =
        shuffled.into_iter().take(args.smoke_instances).collect();
    if smoke_instances_subset.is_empty() {
        return Err(Error::Preflight(
            "No instances found in good sweep to sample for smoke sweep".to_owned(),
        ));
    }
    println!(
        "[bisect] Reproducible smoke sweep subset of size {} selected: {:?}",
        smoke_instances_subset.len(),
        smoke_instances_subset
    );

    let smoke_model = args
        .smoke_model
        .clone()
        .unwrap_or_else(|| "free-tier-model".to_owned());
    let good_resolved_rate = if good_sweep.total > 0 {
        good_sweep.submitted as f64 / good_sweep.total as f64
    } else {
        1.0
    };
    println!(
        "[bisect] Good resolved rate: {:.2}%",
        good_resolved_rate * 100.0
    );

    // 5. Binary search state machine loop
    let mut low = 0;
    let mut high = commit_list.len() - 1;

    // Detect target binary name and path
    let binary_name = format!("max{}", std::env::consts::EXE_SUFFIX);
    let target_binary_path = std::env::current_dir()
        .map_err(Error::Io)?
        .join("target")
        .join("debug")
        .join(&binary_name);

    while low <= high {
        let mid = low + (high - low) / 2;

        // Verify if we have candidate commit already evaluated (from resume)
        let mut candidate_idx = mid;
        let mut found_valid = false;

        // Outward search for non-schema-break candidate commit in [low, high]
        let mut offset = 0i32;
        while mid as i32 - offset >= low as i32 || mid as i32 + offset <= high as i32 {
            if mid as i32 - offset >= low as i32 {
                let cand = (mid as i32 - offset) as usize;
                let sha = &commit_list[cand];
                if !state.schema_breaks.contains(sha) {
                    // Check if it's already marked as schema break in per_commit
                    let is_marked_break = state
                        .per_commit
                        .get(sha)
                        .map_or(false, |r| r.status == "schema_break");
                    if !is_marked_break {
                        candidate_idx = cand;
                        found_valid = true;
                        break;
                    }
                }
            }
            if mid as i32 + offset <= high as i32 {
                let cand = (mid as i32 + offset) as usize;
                let sha = &commit_list[cand];
                if !state.schema_breaks.contains(sha) {
                    let is_marked_break = state
                        .per_commit
                        .get(sha)
                        .map_or(false, |r| r.status == "schema_break");
                    if !is_marked_break {
                        candidate_idx = cand;
                        found_valid = true;
                        break;
                    }
                }
            }
            offset += 1;
        }

        if !found_valid {
            // Every remaining candidate in the window is a schema break!
            println!(
                "[bisect] All remaining candidates in window [{}, {}] are trajectory schema breaks.",
                low, high
            );
            state.outcome = Some("schema_break".to_owned());
            // Write partial bisect.json
            write_bisect_json(&state, args.resume.as_deref())?;
            // Exit with bisect_schema_break (19)
            std::process::exit(crate::exit_code::ExitCode::BisectSchemaBreak.as_i32());
        }

        let candidate_sha = commit_list[candidate_idx].clone();
        println!(
            "[bisect] Evaluating candidate commit index {} / {}: {}",
            candidate_idx,
            commit_list.len(),
            candidate_sha
        );

        // Check if already completed and recorded
        if let Some(result) = state.per_commit.get(&candidate_sha) {
            if result.status == "good" || result.status == "bad" {
                println!(
                    "[bisect] Candidate already evaluated: status={}",
                    result.status
                );
                state.commits_visited.push(candidate_sha.clone());
                if result.status == "bad" {
                    state.suspect_commit = Some(candidate_sha.clone());
                    if candidate_idx == 0 {
                        break;
                    }
                    high = candidate_idx - 1;
                } else {
                    low = candidate_idx + 1;
                }
                continue;
            }
        }

        // Checkout candidate commit
        if !is_test {
            println!("[bisect] git checkout {}", candidate_sha);
            let check_out = Command::new("git")
                .args(["checkout", "-f", &candidate_sha])
                .output()
                .map_err(Error::Io)?;
            if !check_out.status.success() {
                return Err(Error::Preflight(format!(
                    "Failed to checkout commit {}: {}",
                    candidate_sha,
                    String::from_utf8_lossy(&check_out.stderr)
                )));
            }
        }

        // Read and verify trajectory schema version
        let good_schema = good_sweep.manifest.as_ref().map_or((1u16, 10u16), |_m| {
            // We can read it directly from the results.json header if parsed, or default
            (1u16, 10u16)
        });

        let artifact_file_path = std::env::current_dir()
            .map_err(Error::Io)?
            .join("src")
            .join("artifact.rs");
        let candidate_schema =
            read_current_schema_version_from_file(&artifact_file_path).unwrap_or((0, 0));
        let mock_schema_break = is_test
            && std::env::var(&format!("MAX_BISECT_MOCK_SCHEMA_BREAK_{}", candidate_sha)).is_ok();
        if candidate_schema != good_schema || mock_schema_break {
            println!(
                "[bisect] Trajectory schema mismatch: good schema = {:?}, candidate schema = {:?}",
                good_schema, candidate_schema
            );
            state.schema_breaks.push(candidate_sha.clone());
            state.per_commit.insert(
                candidate_sha.clone(),
                CommitResult {
                    commit_sha: candidate_sha.clone(),
                    resolved: 0,
                    errored: 0,
                    cost_usd: 0.0,
                    wallclock_secs: 0.0,
                    smoke_artifact_path: String::new(),
                    cache_reuse_count: 0,
                    status: "schema_break".to_owned(),
                    systemic_halt_category: None,
                },
            );
            // Write partial bisect.json
            write_bisect_json(&state, args.resume.as_deref())?;
            continue;
        }

        // Build the harness
        if !is_test {
            println!("[bisect] Building candidate harness: cargo build");
            let build = Command::new("cargo")
                .arg("build")
                .output()
                .map_err(Error::Io)?;
            if !build.status.success() {
                println!("[bisect] Build failed at commit {}", candidate_sha);
                state.per_commit.insert(
                    candidate_sha.clone(),
                    CommitResult {
                        commit_sha: candidate_sha.clone(),
                        resolved: 0,
                        errored: 0,
                        cost_usd: 0.0,
                        wallclock_secs: 0.0,
                        smoke_artifact_path: String::new(),
                        cache_reuse_count: 0,
                        status: "errored".to_owned(),
                        systemic_halt_category: None,
                    },
                );
                write_bisect_json(&state, args.resume.as_deref())?;
                return Err(Error::Preflight(format!(
                    "Harness build failed at commit {}: {}",
                    candidate_sha,
                    String::from_utf8_lossy(&build.stderr)
                )));
            }
        }

        // Set output dir for smoke sweep
        let smoke_output_dir = std::env::current_dir()
            .map_err(Error::Io)?
            .join("runs")
            .join("bisect_smoke")
            .join(&candidate_sha);

        // Run the smoke sweep or mock it in test
        let mut smoke_results = SweepResults::default();
        let duration_secs;

        if is_test {
            // Mock run results in test env via environment variables
            if let Ok(mock_resolved) =
                std::env::var(&format!("MAX_BISECT_MOCK_RESOLVED_{}", candidate_sha))
            {
                smoke_results.submitted = mock_resolved.parse::<usize>().unwrap_or(0);
                smoke_results.total = args.smoke_instances;
                smoke_results.estimated_cost_usd = 0.10;
                duration_secs = 5.0;
            } else {
                smoke_results.submitted = 5;
                smoke_results.total = args.smoke_instances;
                smoke_results.estimated_cost_usd = 0.10;
                duration_secs = 5.0;
            }
            if let Ok(mock_halt) = std::env::var(&format!("MAX_BISECT_MOCK_HALT_{}", candidate_sha))
            {
                use crate::trajectory::FailureCategory;
                smoke_results.sweep_status = "systemic_halt".to_owned();
                smoke_results.systemic_halt_category = Some(match mock_halt.as_str() {
                    "stagnation" => FailureCategory::AgentStagnation,
                    _ => FailureCategory::AgentInternal,
                });
            }
        } else {
            let start_time = std::time::Instant::now();
            let mut cmd = Command::new(&target_binary_path);
            cmd.arg("bench").arg("swebench");

            let ds_manifest = &manifest.dataset;
            if ds_manifest.source_kind == "named" {
                cmd.arg("--dataset")
                    .arg(ds_manifest.alias.as_deref().unwrap_or("lite"));
                if let Some(ref split) = ds_manifest.split {
                    cmd.arg("--split").arg(split);
                }
            } else {
                cmd.arg("--dataset-path").arg(&ds_manifest.path);
            }

            cmd.arg("--instance-ids")
                .arg(smoke_instances_subset.join(","));
            cmd.arg("--model").arg(&smoke_model);
            cmd.arg("--output").arg(&smoke_output_dir);

            println!("[bisect] Running smoke sweep command: {:?}", cmd);
            let output = cmd.output().map_err(Error::Io)?;
            duration_secs = start_time.elapsed().as_secs_f64();

            if !output.status.success() {
                println!(
                    "[bisect] Smoke sweep failed to complete normally at commit {}",
                    candidate_sha
                );
            }

            // Load smoke sweep results
            smoke_results = load_sweep_results(&smoke_output_dir)?;
        }

        let cost = smoke_results
            .actual_cost_total_usd()
            .unwrap_or(smoke_results.estimated_cost_usd);
        println!(
            "[bisect] Smoke sweep resolved: {}/{}",
            smoke_results.submitted, smoke_results.total
        );
        println!(
            "[bisect] Smoke sweep cost: ${:.4} USD, wallclock: {:.2}s",
            cost, duration_secs
        );

        state.total_cost += cost;
        state.total_wallclock += duration_secs;
        state.commits_visited.push(candidate_sha.clone());

        // Check if circuit breaker tripped
        let is_systemic_halt = smoke_results.sweep_status == "systemic_halt"
            || smoke_results.systemic_halt_category.is_some();
        let halt_category_str = smoke_results
            .systemic_halt_category
            .map(|c| format!("{c:?}"));

        // Determine if candidate has regressed
        let smoke_resolved_rate = if smoke_results.total > 0 {
            smoke_results.submitted as f64 / smoke_results.total as f64
        } else {
            0.0
        };

        let is_regressed = is_systemic_halt
            || (smoke_resolved_rate < (good_resolved_rate - args.regression_margin));
        let status_str = if is_regressed { "bad" } else { "good" };
        println!(
            "[bisect] Commit regression status: {} (resolved rate: {:.2}%)",
            status_str,
            smoke_resolved_rate * 100.0
        );

        let smoke_artifact_path = if is_test {
            format!("runs/bisect_smoke/{}/results.json", candidate_sha)
        } else {
            smoke_output_dir.join("results.json").display().to_string()
        };

        state.cache_reuse_count += smoke_results.skipped;

        let commit_res = CommitResult {
            commit_sha: candidate_sha.clone(),
            resolved: smoke_results.submitted,
            errored: smoke_results.errored,
            cost_usd: cost,
            wallclock_secs: duration_secs,
            smoke_artifact_path,
            cache_reuse_count: smoke_results.skipped,
            status: status_str.to_owned(),
            systemic_halt_category: halt_category_str,
        };
        state.per_commit.insert(candidate_sha.clone(), commit_res);

        // Check if cost exceeded limit
        if let Some(limit) = args.max_cost_usd {
            if state.total_cost > limit {
                println!(
                    "[bisect] Cost ceiling exceeded: total_cost = ${:.4} > limit = ${:.4}",
                    state.total_cost, limit
                );
                state.outcome = Some("budget_exhausted".to_owned());
                write_bisect_json(&state, args.resume.as_deref())?;
                std::process::exit(crate::exit_code::ExitCode::BisectBudgetExhausted.as_i32());
            }
        }

        // Write progress to bisect.json
        write_bisect_json(&state, args.resume.as_deref())?;

        // Adjust binary search bounds
        if is_regressed {
            state.suspect_commit = Some(candidate_sha.clone());
            if candidate_idx == 0 {
                break;
            }
            high = candidate_idx - 1;
        } else {
            low = candidate_idx + 1;
        }
    }

    state.outcome = Some("success".to_owned());
    write_bisect_json(&state, args.resume.as_deref())?;

    if let Some(ref culprit) = state.suspect_commit {
        println!("[bisect] Identified suspect commit: {}", culprit);
    } else {
        println!(
            "[bisect] No suspect commit identified. Regression might not be present in this range."
        );
    }

    Ok(())
}

fn write_bisect_json(state: &BisectState, resume_path: Option<&Path>) -> Result<(), Error> {
    let out_path = match resume_path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()
            .map_err(Error::Io)?
            .join("bisect.json"),
    };
    let json = serde_json::to_string_pretty(state).map_err(Error::Json)?;
    std::fs::write(&out_path, json).map_err(Error::Io)?;
    println!("[bisect] Wrote bisect status to {}", out_path.display());
    Ok(())
}
