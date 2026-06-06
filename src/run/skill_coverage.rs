//! `bench skill-coverage`: per-sweep agent skill activation summary by outcome.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactKind, classify_json_value};
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::run::compare::{load_evaluation_results_checked, load_sweep};
use crate::run::swebench::InstanceResult;
use crate::trajectory::{FailureCategory, Trajectory};

const VALID_BUCKETS: &[&str] = &["resolved", "unresolved", "errored", "all"];

// ── public argument struct ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SkillCoverageArgs {
    pub sweep_dir: PathBuf,
    pub bucket: Option<String>,
    pub filter: Option<String>,
    pub per_instance: bool,
}

// ── report types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillUniverseEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillOutcomeMetrics {
    pub instances_activated: usize,
    pub instances_total: usize,
    pub usage_rate: f64,
    pub resolved_rate_when_active: f64,
    pub resolved_rate_when_not_active: f64,
    pub resolved_rate_delta: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetrics {
    pub total_activations: usize,
    pub instances_activated: usize,
    pub activation_rate: f64,
    pub share_of_all_activations: f64,
    pub reasons: BTreeMap<String, usize>,
    pub by_outcome: BTreeMap<String, SkillOutcomeMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSetDriftGroup {
    pub fingerprint: String,
    pub instance_count: usize,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSetDrift {
    pub groups: Vec<SkillSetDriftGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceSkillRecord {
    pub instance_id: String,
    pub active_skills: BTreeMap<String, String>, // name -> reason
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCoverageReport {
    pub sweep: String,
    pub generated_at: String,
    pub skill_universe: Vec<SkillUniverseEntry>,
    pub by_skill: BTreeMap<String, SkillMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_set_drift: Option<SkillSetDrift>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_instance: Option<Vec<InstanceSkillRecord>>,
}

// ── internal struct for instance data aggregation ────────────────────────────

#[derive(Debug)]
struct InstanceSkillData {
    id: String,
    bucket: OutcomeBucket,
    active_skills_unique: Vec<crate::skills::ActiveSkillManifest>,
    active_skills_all: Vec<crate::skills::ActiveSkillManifest>,
    eligible_skills: HashSet<String>,
    eligible_paths: Vec<String>,
    skills_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OutcomeBucket {
    Resolved,
    Unresolved,
    Errored,
}

impl OutcomeBucket {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Unresolved => "unresolved",
            Self::Errored => "errored",
        }
    }
}

// ── run entrypoint ────────────────────────────────────────────────────────────

pub fn run(args: &SkillCoverageArgs) -> Result<SkillCoverageReport, Error> {
    let mut report = build_report(args)?;
    report.generated_at = utc_now_iso8601();

    let output_path = args.sweep_dir.join("skill-coverage.json");
    let file = std::fs::File::create(output_path)?;
    crate::artifact::to_writer_pretty(file, ArtifactKind::SkillCoverage, &report)?;

    Ok(report)
}

// ── text rendering ────────────────────────────────────────────────────────────

pub fn render_text(report: &SkillCoverageReport, bucket_filter: Option<&str>) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    let display_bucket = bucket_filter.unwrap_or("all");

    let mut out = String::new();
    out.push_str("\n=== bench skill-coverage ===\n");
    let _ = writeln!(out, "Sweep: {}", report.sweep);
    let _ = writeln!(
        out,
        "Skill universe: {} skills",
        report.skill_universe.len()
    );
    if bucket_filter.is_some() {
        let _ = writeln!(out, "Bucket filter: {display_bucket}");
    }
    out.push('\n');

    if let Some(drift) = &report.skill_set_drift {
        let _ = writeln!(
            out,
            "Skill-set Drift detected: {} distinct skill configurations observed across the sweep.",
            drift.groups.len()
        );
        for entry in &drift.groups {
            let _ = writeln!(
                out,
                "  [{} instance(s)]: {}",
                entry.instance_count,
                entry.paths.join(", ")
            );
        }
        out.push('\n');
    }

    let mut rows: Vec<(&str, &SkillMetrics)> = report
        .by_skill
        .iter()
        .map(|(name, m)| (name.as_str(), m))
        .collect();

    // Sort by total activations desc, then by name asc
    rows.sort_by(|a, b| {
        b.1.total_activations
            .cmp(&a.1.total_activations)
            .then_with(|| a.0.cmp(b.0))
    });

    if !rows.is_empty() {
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "skill",
                "total_activations",
                "instances_activated",
                "activation_rate",
                "share",
                "reasons (explicit/auto)",
                "resolved_rate_when_active",
                "resolved_rate_when_not_active",
                "delta",
            ]);

        for (name, m) in &rows {
            let bucket_metrics = m.by_outcome.get(display_bucket);

            let (activated, _total, usage, rr_active, rr_not_active, delta) = match bucket_metrics {
                Some(o) => (
                    o.instances_activated,
                    o.instances_total,
                    o.usage_rate,
                    o.resolved_rate_when_active,
                    o.resolved_rate_when_not_active,
                    o.resolved_rate_delta,
                ),
                None => (0, 0, 0.0, 0.0, 0.0, 0.0),
            };

            let explicit_count = m.reasons.get("explicit_mention").copied().unwrap_or(0);
            let auto_count = m.reasons.get("auto_match").copied().unwrap_or(0);
            let reasons_str = format!("{explicit_count}/{auto_count}");

            table.add_row(vec![
                (*name).to_owned(),
                m.total_activations.to_string(),
                activated.to_string(),
                format!("{usage:.4}"),
                format!("{:.4}", m.share_of_all_activations),
                reasons_str,
                format!("{rr_active:.3}"),
                format!("{rr_not_active:.3}"),
                format!("{delta:+.3}"),
            ]);
        }

        out.push_str(&table.to_string());
        out.push('\n');
    }

    out
}

// ── helper logic ──────────────────────────────────────────────────────────────

fn classify_outcome(
    instance_id: &str,
    instance: &InstanceResult,
    resolved_set: &HashSet<String>,
) -> OutcomeBucket {
    if resolved_set.contains(instance_id) {
        return OutcomeBucket::Resolved;
    }
    if instance.outcome.as_deref() == Some(crate::trajectory::outcome::ERROR) {
        return OutcomeBucket::Errored;
    }
    OutcomeBucket::Unresolved
}

fn matches_filter(
    instance: &InstanceResult,
    is_resolved: bool,
    filter: &str,
) -> Result<bool, Error> {
    let Some((key, value)) = filter.split_once('=') else {
        return Err(Error::Config(crate::error::ConfigError::Invalid(
            "skill-coverage: --filter expects key=value (e.g. failure_category=model_parse)".into(),
        )));
    };
    let (key, value) = (key.trim(), value.trim());
    match key {
        "resolved" => {
            if value != "true" && value != "false" {
                return Err(Error::Config(crate::error::ConfigError::Invalid(
                    "skill-coverage: resolved filter must be `true` or `false`".into(),
                )));
            }
            Ok(is_resolved == (value == "true"))
        }
        "failure_category" => Ok(instance
            .failure_category
            .is_some_and(|fc| failure_category_label(fc) == value)),
        other => Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "skill-coverage: unsupported filter key `{other}`; supported: `resolved`, `failure_category`"
        )))),
    }
}

fn failure_category_label(c: FailureCategory) -> &'static str {
    match c {
        FailureCategory::EnvSetup => "env_setup",
        FailureCategory::ModelApi => "model_api",
        FailureCategory::ModelParse => "model_parse",
        FailureCategory::StepLimit => "step_limit",
        FailureCategory::CostLimit => "cost_limit",
        FailureCategory::BudgetExhausted => "budget_exhausted",
        FailureCategory::WallclockTimeout => "wallclock_timeout",
        FailureCategory::AgentInternal => "agent_internal",
        FailureCategory::AgentStagnation => "agent_stagnation",
        FailureCategory::PatchApplyInvalid => "patch_apply_invalid",
        FailureCategory::PatchEmpty => "patch_empty",
        FailureCategory::SecretLeakDetected => "secret_leak_detected",
        FailureCategory::HistoryCompactionFailed => "history_compaction_failed",
        FailureCategory::Unknown => "unknown",
        FailureCategory::ReadOnlyViolation => "read_only_violation",
    }
}

fn resolve_trajectory_paths(sweep: &Path, instance_id: &str) -> Vec<PathBuf> {
    let nested = sweep.join(instance_id).join("trajectory.json");
    if nested.exists() {
        return vec![nested];
    }

    let instance_dir = sweep.join(instance_id);
    if instance_dir.is_dir() {
        let run_paths: Vec<PathBuf> = std::fs::read_dir(&instance_dir)
            .map(|entries| {
                let mut paths: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("run-") && n.ends_with(".traj.json"))
                    })
                    .collect();
                paths.sort();
                paths
            })
            .unwrap_or_default();
        if !run_paths.is_empty() {
            return run_paths;
        }
    }

    let flat = sweep.join(format!("{instance_id}.traj.json"));
    if flat.exists() {
        return vec![flat];
    }

    let bundled = sweep
        .join("trajectories")
        .join(format!("{instance_id}.traj.json"));
    if bundled.exists() {
        vec![bundled]
    } else {
        vec![]
    }
}

fn load_trajectory(path: &Path) -> Result<Trajectory, Error> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    classify_json_value(&value, ArtifactKind::Trajectory, path.display().to_string())
        .map_err(|err| Error::Trajectory(err.to_string()))?;
    serde_json::from_value(value).map_err(Into::into)
}

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[allow(clippy::too_many_lines)]
fn build_report(args: &SkillCoverageArgs) -> Result<SkillCoverageReport, Error> {
    if let Some(b) = &args.bucket {
        if !VALID_BUCKETS.contains(&b.as_str()) {
            return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
                "skill-coverage: unknown --bucket `{b}`; valid values: resolved, unresolved, errored, all"
            ))));
        }
    }

    let redactor = Redactor::default_enabled();
    let sweep = load_sweep(&args.sweep_dir)?;
    let evaluation = load_evaluation_results_checked(&args.sweep_dir)?;

    let mut resolved_set: HashSet<String> = evaluation
        .as_ref()
        .map(|ev| {
            ev.results
                .instances
                .iter()
                .filter(|i| i.resolved)
                .map(|i| i.instance_id.clone())
                .collect()
        })
        .unwrap_or_default();
    if evaluation.is_none() {
        for (id, inst) in &sweep.instances {
            if inst.resolved_count > 0 {
                resolved_set.insert(id.clone());
            }
        }
    }

    // Resolve global skill configuration
    let mut enabled = false;
    let mut global_skill_paths = Vec::new();
    if let Some(manifest) = &sweep.manifest {
        if let Ok(mut root_cfg) =
            toml::from_str::<crate::config::schema::RootCfg>(&manifest.config.resolved)
        {
            enabled = root_cfg.skills.enabled;
            global_skill_paths = std::mem::take(&mut root_cfg.skills.paths);
        }
    }

    let mut sorted_ids: Vec<String> = sweep.instances.keys().cloned().collect();
    sorted_ids.sort();

    let mut instance_data: Vec<InstanceSkillData> = Vec::new();
    let mut skill_descriptions = HashMap::new();

    for id in &sorted_ids {
        let instance = &sweep.instances[id];
        let is_resolved = resolved_set.contains(id);

        if let Some(filter) = &args.filter {
            if !matches_filter(instance, is_resolved, filter)? {
                continue;
            }
        }

        let bucket = classify_outcome(id, instance, &resolved_set);
        let mut active_skills_unique: Vec<crate::skills::ActiveSkillManifest> = Vec::new();
        let mut active_skills_all: Vec<crate::skills::ActiveSkillManifest> = Vec::new();
        let mut instance_skill_paths = global_skill_paths.clone();
        let mut instance_enabled = enabled;

        let traj_paths = resolve_trajectory_paths(&args.sweep_dir, id);
        for traj_path in traj_paths {
            if let Ok(traj) = load_trajectory(&traj_path) {
                // Parse instance config override
                if let Some(manifest) = &traj.info.manifest {
                    if let Ok(root_cfg) = serde_json::from_value::<crate::config::schema::RootCfg>(
                        manifest.config_redacted.clone(),
                    ) {
                        instance_enabled = root_cfg.skills.enabled;
                        instance_skill_paths.clone_from(&root_cfg.skills.paths);
                    }
                }

                // Parse active skills from trajectory other metadata
                if let Some(raw_skills) = traj.info.other.get("active_skills") {
                    if let Ok(skills) = serde_json::from_value::<
                        Vec<crate::skills::ActiveSkillManifest>,
                    >(raw_skills.clone())
                    {
                        for skill in skills {
                            active_skills_all.push(skill.clone());
                            if !active_skills_unique.iter().any(|s| s.name == skill.name) {
                                active_skills_unique.push(skill);
                            }
                        }
                    }
                }
            }
        }

        // Collect eligible skills for this instance if skills subsystem is enabled
        let mut eligible_skills = HashSet::new();
        if instance_enabled {
            let expanded_paths = instance_skill_paths
                .iter()
                .map(crate::skills::expand_skill_path_pub)
                .collect::<Vec<_>>();
            if let Ok(registry) = crate::skills::SkillRegistry::scan_paths(expanded_paths) {
                for manifest in registry.manifests() {
                    eligible_skills.insert(manifest.name.clone());
                    skill_descriptions.insert(manifest.name.clone(), manifest.description.clone());
                }
            }
        }

        instance_data.push(InstanceSkillData {
            id: id.clone(),
            bucket,
            active_skills_unique,
            active_skills_all,
            eligible_skills,
            eligible_paths: instance_skill_paths,
            skills_enabled: instance_enabled,
        });
    }

    // If skills subsystem is disabled or empty across all scanned instances, return empty report
    let any_enabled = instance_data.iter().any(|d| d.skills_enabled);
    if !any_enabled
        && instance_data
            .iter()
            .all(|d| d.active_skills_unique.is_empty())
    {
        return Ok(SkillCoverageReport {
            sweep: args.sweep_dir.display().to_string(),
            generated_at: String::new(),
            skill_universe: Vec::new(),
            by_skill: BTreeMap::new(),
            skill_set_drift: None,
            per_instance: None,
        });
    }

    // Gather raw skill names and descriptions
    let mut all_raw_skills: BTreeMap<String, SkillUniverseEntry> = BTreeMap::new();
    for d in &instance_data {
        for active in &d.active_skills_unique {
            all_raw_skills.insert(
                active.name.clone(),
                SkillUniverseEntry {
                    name: active.name.clone(),
                    description: active.description.clone(),
                },
            );
        }
        for name in &d.eligible_skills {
            all_raw_skills.entry(name.clone()).or_insert_with(|| {
                let description = skill_descriptions
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "Configured skill".to_owned());
                SkillUniverseEntry {
                    name: name.clone(),
                    description,
                }
            });
        }
    }

    // Build raw -> redacted map for skill names
    let raw_to_redacted: HashMap<String, String> = all_raw_skills
        .keys()
        .map(|raw| {
            let redacted = redactor.redact_text(raw, surface::TRAJECTORY).text;
            (raw.clone(), redacted)
        })
        .collect();

    // Map the skill universe using redacted names
    let mut skill_universe_map: BTreeMap<String, SkillUniverseEntry> = BTreeMap::new();
    for (raw_name, entry) in &all_raw_skills {
        let redacted = raw_to_redacted
            .get(raw_name)
            .cloned()
            .unwrap_or_else(|| raw_name.clone());
        let redacted_description = redactor
            .redact_text(&entry.description, surface::TRAJECTORY)
            .text;
        skill_universe_map
            .entry(redacted.clone())
            .or_insert_with(|| SkillUniverseEntry {
                name: redacted,
                description: redacted_description,
            });
    }

    let skill_universe: Vec<SkillUniverseEntry> = skill_universe_map.values().cloned().collect();

    // Group configurations to detect drift
    let mut config_groups: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new(); // fingerprint -> (instance_ids, paths)
    for d in &instance_data {
        let mut sorted_paths = d.eligible_paths.clone();
        sorted_paths.sort();
        let paths_str = sorted_paths.join(",");
        let fingerprint = format!("enabled={}:{}", d.skills_enabled, paths_str);
        let entry = config_groups
            .entry(fingerprint)
            .or_insert_with(|| (Vec::new(), d.eligible_paths.clone()));
        entry.0.push(d.id.clone());
    }

    let skill_set_drift = if config_groups.len() > 1 {
        let mut groups: Vec<SkillSetDriftGroup> = config_groups
            .into_iter()
            .map(|(fp, (instances, paths))| SkillSetDriftGroup {
                fingerprint: fp,
                instance_count: instances.len(),
                paths,
            })
            .collect();
        // Sort groups by instance count descending, then fingerprint ascending
        groups.sort_by(|a, b| {
            b.instance_count
                .cmp(&a.instance_count)
                .then_with(|| a.fingerprint.cmp(&b.fingerprint))
        });
        Some(SkillSetDrift { groups })
    } else {
        None
    };

    // Calculate metrics per skill (using redacted names)
    let mut by_skill: BTreeMap<String, SkillMetrics> = BTreeMap::new();
    let bucket_names = ["resolved", "unresolved", "errored", "all"];

    for (redacted_name, raw_name) in raw_to_redacted.iter().map(|(k, v)| (v.clone(), k.clone())) {
        let mut total_activations = 0;
        let mut instances_activated_global = 0;
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();

        // Track counts per bucket
        let mut per_bucket_activated: HashMap<&str, usize> = HashMap::new();
        let mut per_bucket_total: HashMap<&str, usize> = HashMap::new();
        let mut per_bucket_resolved_active: HashMap<&str, usize> = HashMap::new();
        let mut per_bucket_resolved_total: HashMap<&str, usize> = HashMap::new();

        for &bname in &bucket_names {
            per_bucket_activated.insert(bname, 0);
            per_bucket_total.insert(bname, 0);
            per_bucket_resolved_active.insert(bname, 0);
            per_bucket_resolved_total.insert(bname, 0);
        }

        for d in &instance_data {
            let is_active = d.active_skills_unique.iter().any(|s| s.name == raw_name);
            let is_eligible = d.eligible_skills.contains(&raw_name);

            // Skill is in scope if it was active or eligible (configured)
            let in_scope = is_active || is_eligible;
            let is_resolved = d.bucket == OutcomeBucket::Resolved;

            if is_active {
                instances_activated_global += 1;
            }

            // Count repeated activations and activation reasons across retry trajectories
            let mut activation_count_for_instance = 0;
            for entry in d.active_skills_all.iter().filter(|s| s.name == raw_name) {
                activation_count_for_instance += 1;
                let reason_str = match entry.activation_reason {
                    crate::skills::SkillActivationReason::ExplicitMention => "explicit_mention",
                    crate::skills::SkillActivationReason::AutoMatch => "auto_match",
                };
                *reasons.entry(reason_str.to_owned()).or_default() += 1;
            }
            total_activations += activation_count_for_instance;

            for &bname in &bucket_names {
                let in_bucket = bname == "all" || d.bucket.as_str() == bname;
                if !in_bucket {
                    continue;
                }

                if in_scope {
                    *per_bucket_total.entry(bname).or_default() += 1;
                    *per_bucket_resolved_total.entry(bname).or_default() +=
                        usize::from(is_resolved);
                }

                if is_active {
                    *per_bucket_activated.entry(bname).or_default() += 1;
                    *per_bucket_resolved_active.entry(bname).or_default() +=
                        usize::from(is_resolved);
                }
            }
        }

        // Calculate rates
        let total_instances = instance_data.len();
        #[allow(clippy::cast_precision_loss)]
        let activation_rate = if total_instances > 0 {
            instances_activated_global as f64 / total_instances as f64
        } else {
            0.0
        };

        let mut by_outcome: BTreeMap<String, SkillOutcomeMetrics> = BTreeMap::new();
        for &bname in &bucket_names {
            let activated = per_bucket_activated[bname];
            let total = per_bucket_total[bname];
            let res_active = per_bucket_resolved_active[bname];
            let res_total = per_bucket_resolved_total[bname];

            #[allow(clippy::cast_precision_loss)]
            let usage_rate = if total > 0 {
                activated as f64 / total as f64
            } else {
                0.0
            };

            #[allow(clippy::cast_precision_loss)]
            let resolved_rate_when_active = if activated > 0 {
                res_active as f64 / activated as f64
            } else {
                0.0
            };

            let not_active = total.saturating_sub(activated);
            let res_not_active = res_total.saturating_sub(res_active);
            #[allow(clippy::cast_precision_loss)]
            let resolved_rate_when_not_active = if not_active > 0 {
                res_not_active as f64 / not_active as f64
            } else {
                0.0
            };

            let resolved_rate_delta = resolved_rate_when_active - resolved_rate_when_not_active;

            by_outcome.insert(
                bname.to_owned(),
                SkillOutcomeMetrics {
                    instances_activated: activated,
                    instances_total: total,
                    usage_rate,
                    resolved_rate_when_active,
                    resolved_rate_when_not_active,
                    resolved_rate_delta,
                },
            );
        }

        by_skill.insert(
            redacted_name,
            SkillMetrics {
                total_activations,
                instances_activated: instances_activated_global,
                activation_rate,
                share_of_all_activations: 0.0,
                reasons,
                by_outcome,
            },
        );
    }

    // Compute share of all activations
    let grand_total_activations: usize = by_skill.values().map(|m| m.total_activations).sum();
    for metrics in by_skill.values_mut() {
        #[allow(clippy::cast_precision_loss)]
        if grand_total_activations > 0 {
            metrics.share_of_all_activations =
                metrics.total_activations as f64 / grand_total_activations as f64;
        }
    }

    // Build per-instance rows
    let per_instance = if args.per_instance {
        let mut records = Vec::new();
        for d in &instance_data {
            let mut active_skills = BTreeMap::new();
            for skill in &d.active_skills_unique {
                let redacted = redactor.redact_text(&skill.name, surface::TRAJECTORY).text;
                let reason_str = match skill.activation_reason {
                    crate::skills::SkillActivationReason::ExplicitMention => "explicit_mention",
                    crate::skills::SkillActivationReason::AutoMatch => "auto_match",
                };
                active_skills.insert(redacted, reason_str.to_owned());
            }
            records.push(InstanceSkillRecord {
                instance_id: d.id.clone(),
                active_skills,
            });
        }
        records.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Some(records)
    } else {
        None
    };

    Ok(SkillCoverageReport {
        sweep: args.sweep_dir.display().to_string(),
        generated_at: String::new(),
        skill_universe,
        by_skill,
        skill_set_drift,
        per_instance,
    })
}
