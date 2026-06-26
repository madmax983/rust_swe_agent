use super::args::CatalogCmd;
use crate::error::Error;
use comfy_table::{Cell, Color, Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};
use serde::Serialize;

pub const STAGES: &[&str] = &["preflight", "run", "inspect", "analyze", "publish"];

#[derive(Clone, Serialize)]
pub struct CatalogEntry {
    pub path: &'static str,
    pub summary: &'static str,
    pub cost_tier: &'static str, // "free" or "paid"
    pub stage: &'static str,
}

#[derive(Serialize)]
struct JsonCatalogResponse {
    schema_version: &'static str,
    commands: Vec<CatalogEntry>,
}

#[allow(clippy::too_many_lines)]
pub fn entries() -> &'static [CatalogEntry] {
    &[
        // Top-level subcommands
        CatalogEntry {
            path: "catalog",
            summary: "Display the daemon's subcommand catalog with metadata",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "explain",
            summary: "Explain an exit code, outcome class, or failure category offline",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "cleanup",
            summary: "Reap leftover Maxwell's Daemon containers, including legacy labels",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "hello-world",
            summary: "Smoke-test: scripted model + local env writes a trajectory",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "mini",
            summary: "Run one task end-to-end and write a trajectory",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "replay",
            summary: "Replay an existing trajectory using a deterministic model",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "ui",
            summary: "Serve a read-only local sweep browser",
            cost_tier: "free",
            stage: "publish",
        },
        // Agent subcommands
        CatalogEntry {
            path: "agent annotate",
            summary: "Attach a schema-versioned human verdict and notes to a trajectory sidecar",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "agent best-of",
            summary: "Sample N runs and emit the best patch by --verify oracle",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "agent apply",
            summary: "Apply a captured patch artifact to a working tree",
            cost_tier: "free",
            stage: "run",
        },
        CatalogEntry {
            path: "agent config resolve",
            summary: "Print the fully-resolved effective run configuration annotated with provenance",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "agent env preview",
            summary: "Preview environment configuration for a task without running anything",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "agent doctor",
            summary: "Preflight host readiness before a live run (no model call, $0)",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "agent fs-audit",
            summary: "Audit sweep trajectories for out-of-workdir filesystem access",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "agent injection-audit",
            summary: "Audit sweep trajectories for prompt-injection signals",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "agent artifact-check",
            summary: "Validate artifact files against the Artifact Contract (zero-cost, no model call)",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "agent policy-check",
            summary: "Check a command corpus against the policy config",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "agent profile",
            summary: "Profile a single trajectory file: cost, tokens, stage latency, and action mix",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "agent runs",
            summary: "List and summarize single-task trajectory files in a directory",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "agent redact-audit",
            summary: "Audit a finished sweep tree for secret leaks in stored artifacts",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "agent redact-check",
            summary: "Verify the secret-redaction config against sample input",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "agent skills-preview",
            summary: "Preview which skills will activate for one or more tasks",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "agent stability",
            summary: "Measure single-task run-to-run variance by running N times and reporting pass@k",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "agent suite",
            summary: "Run an operator-defined personal eval task pack",
            cost_tier: "paid",
            stage: "run",
        },
        // Bench subcommands
        CatalogEntry {
            path: "bench annotate add",
            summary: "Attach persistent operator triage tags and notes to a SWE-bench instance",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench annotate list",
            summary: "List persistent operator triage tags and notes",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench annotate rm",
            summary: "Remove persistent operator triage tags and notes from a SWE-bench instance",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench assert",
            summary: "Evaluate operator-declared SLO rules against completed sweep artifacts and gate CI",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench audit",
            summary: "Re-derive and verify sweep aggregates against trajectories",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench behavior",
            summary: "Surface agent action-class mix by outcome bucket",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench bisect",
            summary: "Identify the commit that introduced a resolved-rate regression",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "bench budget-fit",
            summary: "Right-size step, cost, and wallclock caps from completed sweep distributions",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench variance",
            summary: "Classify per-instance flakiness from a rerun sweep (zero cost)",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench bundle",
            summary: "Export or verify a portable, redacted sweep archive",
            cost_tier: "free",
            stage: "publish",
        },
        CatalogEntry {
            path: "bench cache-stats",
            summary: "Surface prompt-cache hit rate, savings, and spend for a completed sweep",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench ledger",
            summary: "Roll up cumulative actual spend across runs/sweeps by model, dataset, and day",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench context-pressure",
            summary: "Report context-window pressure telemetry per sweep",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench calibrate",
            summary: "Compare a forecast artifact against completed sweep results",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench cascade",
            summary: "Cost-optimized model-tier routing per instance",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "bench command-stats",
            summary: "Aggregate shell-command frequency and cost by outcome bucket",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench compare",
            summary: "Diff two completed sweep runs by instance id",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench contamination-check",
            summary: "Score each resolved instance for training-leakage risk using deterministic trajectory signals",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench dataset-stats",
            summary: "Preview SWE-bench dataset composition offline and zero-cost before running a sweep",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench dataset-verify",
            summary: "Verify candidate datasets offline against canonical official releases",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench diff-config",
            summary: "Diff two completed sweep manifests to analyze configuration drift",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench doctor",
            summary: "Validate sweep inputs without launching tasks",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench eval-flake",
            summary: "Quantify evaluator-side verdict noise by replaying the evaluator N times per patch",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench eval-parity",
            summary: "Compare offline and canonical evaluator verdicts to measure parity and gate CI",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench evaluate",
            summary: "Evaluate a completed sweep with an evaluation backend",
            cost_tier: "free",
            stage: "run",
        },
        CatalogEntry {
            path: "bench evaluator-selftest",
            summary: "Verify the evaluator pipeline using gold patches as a zero-cost preflight",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench events",
            summary: "Query the structured per-run event log by type, instance, and time window",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench export-ci",
            summary: "Export a completed sweep as JUnit XML and/or GitHub Actions annotations",
            cost_tier: "free",
            stage: "publish",
        },
        CatalogEntry {
            path: "bench export-otlp",
            summary: "Backfill OTLP traces from a completed sweep to a collector with live-identical IDs",
            cost_tier: "free",
            stage: "publish",
        },
        CatalogEntry {
            path: "bench failure-digest",
            summary: "Emit a self-contained failure summary for one instance in a completed sweep",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench forecast",
            summary: "Forecast sweep cost from a reproducible calibration slice",
            cost_tier: "paid",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench fork",
            summary: "Fork a trajectory at step N and continue with config overrides",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "bench frontier",
            summary: "Pareto frontier across multiple sweep runs: ASCII chart + JSON dataset",
            cost_tier: "free",
            stage: "publish",
        },
        CatalogEntry {
            path: "bench grep",
            summary: "Search every trajectory in a sweep for a regex pattern",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench import",
            summary: "Ingest an external predictions file and materialise it as a normalised sweep directory",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench inspect",
            summary: "Inspect a single trajectory or list filtered instance summaries",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench instance-history",
            summary: "Join historical sweeps on instance_id and report resolution history",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench ladder",
            summary: "Resolved-rate and cost trend across sweeps in a root directory",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench matrix",
            summary: "Run multiple sweep arms against the same instance set",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "bench merge",
            summary: "Combine sharded sweep result directories into one aggregate",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench shard",
            summary: "Deterministically partition a dataset into N disjoint balanced shards",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench near-miss",
            summary: "Rank unresolved sweep instances by gold-patch proximity",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench policy-impact",
            summary: "Measure sweep policy impact on outcomes",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench power",
            summary: "Calculate statistical power or required sample size",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench rehearsal",
            summary: "Walk the full sweep pipeline end-to-end at zero cost using gold-patch shadow submissions",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench report",
            summary: "Produce a shareable markdown or HTML sweep summary",
            cost_tier: "free",
            stage: "publish",
        },
        CatalogEntry {
            path: "bench reproduce",
            summary: "Replay a saved sweep from its manifest and report reproducibility",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "bench retry",
            summary: "Re-run only the failed instances of a completed sweep and merge results",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "bench scriptability-check",
            summary: "Probe every configured MCP server and dry-run every configured hook before any model call",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench self-check",
            summary: "Score agent self-verdict against the evaluator",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench stagnation-report",
            summary: "Surface looped-action signatures across a sweep",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench subset",
            summary: "Export a sampled dataset slice as a pinned JSONL artifact and provenance manifest",
            cost_tier: "free",
            stage: "preflight",
        },
        CatalogEntry {
            path: "bench swebench",
            summary: "Run a SWE-bench sweep over a local JSONL dataset",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "bench tail",
            summary: "Tail live aggregate progress for a running sweep directory",
            cost_tier: "free",
            stage: "inspect",
        },
        CatalogEntry {
            path: "bench test-progress",
            summary: "Compute per-test partial-credit scores across a completed sweep",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench tool-ablation",
            summary: "Systematic per-tool removal ablation: baseline plus one arm per removed tool",
            cost_tier: "paid",
            stage: "run",
        },
        CatalogEntry {
            path: "bench skill-coverage",
            summary: "Measure agent skill activation and correlate with outcome across a sweep",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench tool-coverage",
            summary: "Measure MCP tool usage and correlate with outcome across a sweep",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench triage",
            summary: "Cluster unresolved sweep failures into ranked actionable groups",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench triage-diff",
            summary: "Diff failure-cluster composition between two sweeps",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench utilization",
            summary: "Report sweep concurrency efficiency: effective parallelism, utilization, idle waste",
            cost_tier: "free",
            stage: "analyze",
        },
        CatalogEntry {
            path: "bench watch",
            summary: "Attach to a single in-flight instance and stream its turns live",
            cost_tier: "free",
            stage: "inspect",
        },
    ]
}

#[allow(clippy::needless_pass_by_value)]
pub fn run_catalog(cmd: CatalogCmd) -> Result<(), Error> {
    // 1. Filter entries
    let mut filtered: Vec<CatalogEntry> = entries()
        .iter()
        .filter(|e| {
            if cmd.free_only && e.cost_tier != "free" {
                return false;
            }
            if let Some(ref s) = cmd.stage {
                if e.stage != s {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();

    // Sort by path for stable output
    filtered.sort_by_key(|e| e.path);

    if cmd.format == "json" {
        let resp = JsonCatalogResponse {
            schema_version: "1.0",
            commands: filtered,
        };
        let json = serde_json::to_string_pretty(&resp)?;
        println!("{json}");
        return Ok(());
    }

    // Text table output
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header([
            Cell::new("Command").fg(Color::Cyan),
            Cell::new("Summary").fg(Color::White),
            Cell::new("Cost").fg(Color::Yellow),
            Cell::new("Stage").fg(Color::Green),
        ]);

    for entry in &filtered {
        table.add_row([entry.path, entry.summary, entry.cost_tier, entry.stage]);
    }

    println!("{table}");
    Ok(())
}
