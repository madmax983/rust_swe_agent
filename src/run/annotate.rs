//! `bench annotate`: add / list / rm persistent operator triage notes.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::annotation::{Annotation, AnnotationStore, resolve_store_path};
use crate::error::Error;
use crate::redaction::{Redactor, surface};

// ── arg structs ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AnnotateAddArgs {
    pub instance_id: String,
    pub tags: Vec<String>,
    pub note: Option<String>,
    pub store: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AnnotateListArgs {
    pub instance: Option<String>,
    pub tag: Option<String>,
    pub store: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AnnotateRmArgs {
    pub instance_id: String,
    pub tag: Option<String>,
    pub store: Option<PathBuf>,
}

// ── output types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotateAddReport {
    pub store_path: PathBuf,
    pub instance_id: String,
    pub tags_added: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotateListReport {
    pub store_path: PathBuf,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotateRmReport {
    pub store_path: PathBuf,
    pub instance_id: String,
    pub tag: Option<String>,
    pub removed_count: usize,
}

// ── run functions ─────────────────────────────────────────────────────────────

pub fn run_add(args: &AnnotateAddArgs) -> Result<AnnotateAddReport, Error> {
    if args.tags.is_empty() {
        return Err(crate::error::Error::Config(
            crate::error::ConfigError::Invalid(
                "annotate add: at least one --tag is required".into(),
            ),
        ));
    }

    let store_path = resolve_store_path(args.store.as_deref());
    let redactor = Redactor::default_enabled();

    let redacted_note = args.note.as_deref().map(|n| {
        redactor.redact_text(n, surface::EXPORT).text
    });

    let mut store = AnnotationStore::load_or_default(&store_path)?;
    for tag in &args.tags {
        store.add(&args.instance_id, tag, redacted_note.as_deref())?;
    }
    store.save(&store_path)?;

    Ok(AnnotateAddReport {
        store_path,
        instance_id: args.instance_id.clone(),
        tags_added: args.tags.clone(),
    })
}

pub fn run_list(args: &AnnotateListArgs) -> Result<AnnotateListReport, Error> {
    let store_path = resolve_store_path(args.store.as_deref());
    let store = AnnotationStore::load_or_default(&store_path)?;
    let annotations = store.list(args.instance.as_deref(), args.tag.as_deref());
    Ok(AnnotateListReport {
        store_path,
        annotations,
    })
}

pub fn run_rm(args: &AnnotateRmArgs) -> Result<AnnotateRmReport, Error> {
    let store_path = resolve_store_path(args.store.as_deref());
    let mut store = AnnotationStore::load_or_default(&store_path)?;

    let before = store.list(Some(&args.instance_id), args.tag.as_deref()).len();
    store.remove(&args.instance_id, args.tag.as_deref());
    let after = store.list(Some(&args.instance_id), args.tag.as_deref()).len();
    let removed_count = before.saturating_sub(after);

    store.save(&store_path)?;

    Ok(AnnotateRmReport {
        store_path,
        instance_id: args.instance_id.clone(),
        tag: args.tag.clone(),
        removed_count,
    })
}

// ── text renderers ────────────────────────────────────────────────────────────

pub fn render_add_text(report: &AnnotateAddReport) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "annotate: added {} tag(s) for `{}` → {}",
        report.tags_added.len(),
        report.instance_id,
        report.store_path.display()
    );
    for tag in &report.tags_added {
        let _ = writeln!(s, "  + {tag}");
    }
    s
}

pub fn render_list_text(report: &AnnotateListReport) -> String {
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;

    if report.annotations.is_empty() {
        return format!(
            "annotate: no annotations found in {}\n",
            report.store_path.display()
        );
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n=== bench annotate list ({}) ===",
        report.store_path.display()
    );

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["instance_id", "tag", "note", "updated_at"]);

    for ann in &report.annotations {
        table.add_row(vec![
            ann.instance_id.clone(),
            ann.tag.clone(),
            ann.note.as_deref().unwrap_or("").to_owned(),
            ann.updated_at.clone(),
        ]);
    }
    out.push_str(&table.to_string());
    out.push('\n');
    out
}

pub fn render_rm_text(report: &AnnotateRmReport) -> String {
    let tag_desc = report
        .tag
        .as_deref()
        .map_or_else(|| "all tags".to_owned(), |t| format!("tag `{t}`"));
    format!(
        "annotate: removed {} annotation(s) ({tag_desc}) for `{}` → {}\n",
        report.removed_count,
        report.instance_id,
        report.store_path.display()
    )
}

/// Load annotations for an instance from the default or given store path.
/// Returns an empty vec on any error (best-effort; never blocks callers).
#[must_use]
pub fn load_annotations_best_effort(instance_id: &str, store_path: Option<&std::path::Path>) -> Vec<Annotation> {
    let path = resolve_store_path(store_path);
    match AnnotationStore::load_or_default(&path) {
        Ok(store) => store.list(Some(instance_id), None),
        Err(_) => Vec::new(),
    }
}

/// Load compact tag list for one instance. Best-effort; returns empty on error.
#[must_use]
pub fn load_tags_best_effort(instance_id: &str, store_path: Option<&std::path::Path>) -> Vec<String> {
    let path = resolve_store_path(store_path);
    match AnnotationStore::load_or_default(&path) {
        Ok(store) => store.tags_for(instance_id),
        Err(_) => Vec::new(),
    }
}

/// Diff two annotation stores; returns `(only_in_left, only_in_right)` as
/// `(instance_id, tag)` pairs.  Used by `bench reproduce` to surface
/// annotation drift.
#[must_use]
pub fn diff_annotation_stores(
    left: &AnnotationStore,
    right: &AnnotationStore,
) -> (
    Vec<(String, String)>,
    Vec<(String, String)>,
) {
    let left_set: std::collections::BTreeSet<(String, String)> = left
        .list(None, None)
        .into_iter()
        .map(|a| (a.instance_id, a.tag))
        .collect();
    let right_set: std::collections::BTreeSet<(String, String)> = right
        .list(None, None)
        .into_iter()
        .map(|a| (a.instance_id, a.tag))
        .collect();

    let only_left = left_set.difference(&right_set).cloned().collect();
    let only_right = right_set.difference(&left_set).cloned().collect();
    (only_left, only_right)
}
