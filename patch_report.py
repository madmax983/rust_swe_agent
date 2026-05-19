import sys

with open('src/run/report.rs', 'r') as f:
    content = f.read()

# Update ReportFormat enum
report_format_replacement = """pub enum ReportFormat {
    Markdown,
    Html,
    Jupyter,
}"""

content = content.replace("""pub enum ReportFormat {
    Markdown,
    Html,
}""", report_format_replacement)

# Update format parsing in cli/mod.rs instead of report.rs ? Wait, parsing is in cli/mod.rs
with open('src/run/report.rs', 'w') as f:
    f.write(content)

with open('src/cli/mod.rs', 'r') as f:
    cli_content = f.read()

cli_format_replacement = """        "markdown" | "md" => crate::run::report::ReportFormat::Markdown,
        "html" => crate::run::report::ReportFormat::Html,
        "jupyter" | "ipynb" => crate::run::report::ReportFormat::Jupyter,
"""

cli_content = cli_content.replace("""        "markdown" | "md" => crate::run::report::ReportFormat::Markdown,
        "html" => crate::run::report::ReportFormat::Html,
""", cli_format_replacement)

with open('src/cli/mod.rs', 'w') as f:
    f.write(cli_content)
