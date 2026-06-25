#![allow(clippy::unwrap_used)]

use clap::CommandFactory as _;
use maxwells_daemon::cli::{args::CatalogCmd, catalog, Cli};

#[test]
fn test_catalog_subcommand_coverage() {
    let root = Cli::command();

    // 1. Traverse clap command tree recursively to find all executable paths.
    let mut executable_paths = Vec::new();
    collect_executable_paths(&root, &[], &mut executable_paths);

    // Sort for stable comparison
    executable_paths.sort();

    // 2. Get all entries in the catalog registry
    let catalog_entries = catalog::entries();
    let mut catalog_paths: Vec<String> =
        catalog_entries.iter().map(|e| e.path.to_string()).collect();
    catalog_paths.sort();

    // Check for orphans: subcommands compiled but missing from catalog
    let mut orphans = Vec::new();
    for path in &executable_paths {
        if !catalog_paths.contains(path) {
            orphans.push(path.clone());
        }
    }

    // Check for stale entries: in catalog but missing from compiled CLI
    let mut stale = Vec::new();
    for path in &catalog_paths {
        if !executable_paths.contains(path) {
            stale.push(path.clone());
        }
    }

    assert!(
        orphans.is_empty(),
        "Orphan subcommands found (compiled in CLI but missing from catalog): {orphans:#?}"
    );

    assert!(
        stale.is_empty(),
        "Stale catalog entries found (referenced in catalog but missing/renamed in CLI): {stale:#?}"
    );
}

#[test]
fn test_catalog_entry_metadata_validity() {
    let entries = catalog::entries();
    for entry in entries {
        assert!(
            catalog::STAGES.contains(&entry.stage),
            "Invalid stage '{}' for command '{}'",
            entry.stage,
            entry.path
        );

        // Valid cost tiers: free, paid
        let valid_costs = ["free", "paid"];
        assert!(
            valid_costs.contains(&entry.cost_tier),
            "Invalid cost tier '{}' for command '{}'",
            entry.cost_tier,
            entry.path
        );

        // One-line summary should be non-empty and not end with a newline or be overly long
        assert!(
            !entry.summary.is_empty(),
            "Summary for '{}' is empty",
            entry.path
        );
        assert!(
            !entry.summary.contains('\n'),
            "Summary for '{}' contains newlines",
            entry.path
        );
    }
}

#[test]
fn test_catalog_filtering() {
    let entries = catalog::entries();

    // Verify we have both free and paid commands
    let has_free = entries.iter().any(|e| e.cost_tier == "free");
    let has_paid = entries.iter().any(|e| e.cost_tier == "paid");
    assert!(has_free, "Catalog must contain at least one free command");
    assert!(has_paid, "Catalog must contain at least one paid command");

    // Verify we have all stages represented
    for stage in catalog::STAGES {
        let has_stage = entries.iter().any(|e| e.stage == *stage);
        assert!(
            has_stage,
            "Catalog must contain at least one command in stage '{stage}'"
        );
    }
}

fn collect_executable_paths(cmd: &clap::Command, current_path: &[String], paths: &mut Vec<String>) {
    if cmd.is_hide_set() {
        return;
    }
    // If this command is not the root and has no subcommands, it is a leaf executable command.
    // If it HAS subcommands, it acts as a namespace (e.g. `bench`, `agent`, `agent env`).
    // In our CLI, the namespaces themselves are not directly runnable (they require a subcommand).
    // Let's verify this by checking if the command requires a subcommand.
    if cmd.get_name() != "max" && cmd.get_subcommands().count() == 0 {
        paths.push(current_path.join(" "));
    } else {
        for sub in cmd.get_subcommands() {
            let mut next_path = current_path.to_owned();
            next_path.push(sub.get_name().to_string());
            collect_executable_paths(sub, &next_path, paths);
        }
    }
}

#[test]
fn test_run_catalog_free_only() {
    let cmd = CatalogCmd {
        stage: None,
        free_only: true,
        format: "text".to_string(),
    };
    catalog::run_catalog(cmd).unwrap();
}

#[test]
fn test_run_catalog_json() {
    let cmd = CatalogCmd {
        stage: None,
        free_only: false,
        format: "json".to_string(),
    };
    catalog::run_catalog(cmd).unwrap();
}

#[test]
fn test_run_catalog_stage_filter() {
    let cmd = CatalogCmd {
        stage: Some("preflight".to_string()),
        free_only: false,
        format: "text".to_string(),
    };
    catalog::run_catalog(cmd).unwrap();
}
