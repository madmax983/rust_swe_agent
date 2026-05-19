import sys

with open('src/cli/args.rs', 'r') as f:
    content = f.read()

report_cmd_replacement = """pub struct ReportCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Output file path for the report.
    #[arg(long)]
    pub output: PathBuf,

    /// Optional baseline sweep directory for delta comparison using `bench compare` machinery.
    #[arg(long)]
    pub baseline: Option<PathBuf>,

    /// Number of top failed instances to include in the report.
    #[arg(long, default_value_t = 10)]
    pub top_failures: usize,

    /// Output format: `markdown` (default), `html` (single-file, inline CSS, no JS), or `jupyter` (Jupyter Notebook).
    #[arg(long, default_value = "markdown")]
    pub format: String,
}"""

content = content.replace("""pub struct ReportCmd {
    /// Completed sweep directory produced by `bench swebench`.
    #[arg(long)]
    pub sweep: PathBuf,

    /// Output file path for the report.
    #[arg(long)]
    pub output: PathBuf,

    /// Optional baseline sweep directory for delta comparison using `bench compare` machinery.
    #[arg(long)]
    pub baseline: Option<PathBuf>,

    /// Number of top failed instances to include in the report.
    #[arg(long, default_value_t = 10)]
    pub top_failures: usize,

    /// Output format: `markdown` (default) or `html` (single-file, inline CSS, no JS).
    #[arg(long, default_value = "markdown")]
    pub format: String,
}""", report_cmd_replacement)

with open('src/cli/args.rs', 'w') as f:
    f.write(content)
