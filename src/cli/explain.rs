//! `max explain` — offline lookup over the exit-code and failure-category
//! contracts (issue #535).
//!
//! Read-only, $0, zero network: every answer comes from the compiled-in
//! [`crate::explain`] registry. Mirrors `rustc --explain E0382`.

use super::args::ExplainCmd;
use crate::error::{ConfigError, Error};
use crate::explain::{self, EXPLAIN_SCHEMA_VERSION, ExplainEntry};
use comfy_table::{Color, Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};

/// Run `max explain`. With a selector, explain that code/class/category; without
/// one, print the full index. An unknown selector returns a usage error (exit 2).
pub fn run_explain(cmd: &ExplainCmd) -> Result<(), Error> {
    let json = cmd.format == "json";
    if let Some(selector) = cmd.selector.as_deref() {
        let entry = explain::resolve(selector).ok_or_else(|| unknown_selector_error(selector))?;
        if json {
            print_entry_json(entry)?;
        } else {
            print_entry_text(entry);
        }
    } else if json {
        print_index_json()?;
    } else {
        print_index_text();
    }
    Ok(())
}

/// Comma-joined family wire names, e.g. `exit_code` or `exit_code, failure_category`.
fn families_label(entry: &ExplainEntry) -> String {
    entry
        .families
        .iter()
        .map(|f| f.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn entry_json(entry: &ExplainEntry) -> serde_json::Value {
    serde_json::json!({
        "code": entry.code,
        "outcome_class": entry.outcome_class,
        "family": entry.families.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
        "meaning": entry.meaning,
        "remediation": entry.remediation,
        "docs_ref": entry.docs_ref,
    })
}

fn print_entry_json(entry: &ExplainEntry) -> Result<(), Error> {
    let mut obj = entry_json(entry);
    if let Some(map) = obj.as_object_mut() {
        map.insert(
            "schema_version".to_string(),
            serde_json::Value::from(EXPLAIN_SCHEMA_VERSION),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&obj).map_err(Error::Json)?
    );
    Ok(())
}

fn print_entry_text(entry: &ExplainEntry) {
    use crossterm::style::Stylize;

    if let Some(code) = entry.code {
        println!("{:<14} {}", "Exit code:".bold(), code.to_string().red());
    }
    println!(
        "{:<14} {}",
        "Outcome class:".bold(),
        entry.outcome_class.yellow()
    );
    println!("{:<14} {}", "Family:".bold(), families_label(entry).cyan());
    println!();
    println!("{}", "Meaning:".bold().underlined());
    println!("  {}", entry.meaning);
    println!();
    println!("{}", "Remediation:".bold().underlined());
    println!("  {}", entry.remediation.green());
    println!();
    println!(
        "{:<14} {}",
        "Docs:".bold(),
        entry.docs_ref.blue().underlined()
    );
}

fn print_index_json() -> Result<(), Error> {
    let entries: Vec<serde_json::Value> = explain::entries().iter().map(entry_json).collect();
    let doc = serde_json::json!({
        "schema_version": EXPLAIN_SCHEMA_VERSION,
        "entries": entries,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&doc).map_err(Error::Json)?
    );
    Ok(())
}

fn print_index_text() {
    use crossterm::style::Stylize;

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header([
            comfy_table::Cell::new("Code").fg(Color::Cyan),
            comfy_table::Cell::new("Outcome Class").fg(Color::Yellow),
            comfy_table::Cell::new("Family").fg(Color::Magenta),
            comfy_table::Cell::new("Meaning").fg(Color::Green),
        ]);
    for entry in explain::entries() {
        let code = entry
            .code
            .map_or_else(|| "-".to_string(), |c| c.to_string());
        table.add_row([
            comfy_table::Cell::new(code).fg(Color::Red),
            comfy_table::Cell::new(entry.outcome_class).fg(Color::Yellow),
            comfy_table::Cell::new(families_label(entry)).fg(Color::Cyan),
            comfy_table::Cell::new(entry.meaning),
        ]);
    }
    println!("{table}");
    println!(
        "\nRun {} to explain one entry by exit code, \
         outcome class, or failure category.",
        "`max explain <selector>`".magenta().bold()
    );
}

/// Usage error (exit 2) for an unknown selector; lists the valid selector
/// families so an operator can correct the invocation.
fn unknown_selector_error(selector: &str) -> Error {
    Error::Config(ConfigError::Invalid(format!(
        "unknown explain selector '{selector}'. Valid selectors are: an exit code \
         integer (e.g. 7), an outcome class name (e.g. verification_failure), or a \
         failure category (e.g. step_limit / StepLimit). Run `max explain` with no \
         argument to list every addressable selector."
    )))
}
