import sys

with open('src/run/report.rs', 'r') as f:
    content = f.read()

# Add Jupyter export rendering
jupyter_match = """    match args.format {
        ReportFormat::Markdown => Ok(md),
        ReportFormat::Html => Ok(md_to_html(&md)),
        ReportFormat::Jupyter => Ok(md_to_jupyter(&md)),
    }"""

content = content.replace("""    match args.format {
        ReportFormat::Markdown => Ok(md),
        ReportFormat::Html => Ok(md_to_html(&md)),
    }""", jupyter_match)

jupyter_fn = """

fn md_to_jupyter(md: &str) -> String {
    let mut cells = Vec::new();

    // Simple naive splitting by ## headers for demonstration
    // Could be much more sophisticated
    let blocks = md.split("\\n## ");

    for (i, block) in blocks.enumerate() {
        let mut cell_source = Vec::new();
        let content = if i == 0 {
            block.to_string()
        } else {
            format!("## {}\\n", block)
        };

        let lines: Vec<&str> = content.split('\\n').collect();
        for (j, line) in lines.iter().enumerate() {
            if j < lines.len() - 1 {
                cell_source.push(format!("{}\\n", line));
            } else if !line.is_empty() {
                cell_source.push(line.to_string());
            }
        }

        if !cell_source.is_empty() {
            cells.push(serde_json::json!({
                "cell_type": "markdown",
                "metadata": {},
                "source": cell_source
            }));
        }
    }

    let notebook = serde_json::json!({
        "cells": cells,
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    });

    serde_json::to_string_pretty(&notebook).unwrap_or_default()
}
"""
content += jupyter_fn

with open('src/run/report.rs', 'w') as f:
    f.write(content)
