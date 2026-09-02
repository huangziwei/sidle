use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::library::db::{self, BookRow};
use crate::library::source::{self, Source};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Flattened {
    pub sheets: Vec<String>,
    pub generated_classes: usize,
    pub producer: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Candidate {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub series_name: Option<String>,
    pub series_index: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiffInfo {
    pub document: String,
    pub text: String,
    pub property: String,
    pub before: String,
    pub after: String,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreReport {
    pub reference: String,
    pub documents: Vec<String>,
    pub classes: BTreeMap<String, String>,
    pub residual: Vec<String>,
    pub residual_css: String,
    pub diffs: Vec<DiffInfo>,
    pub material: usize,
    pub written: bool,
    pub blocked: Option<String>,
}

pub fn flattened(book: &BookRow) -> Result<Option<Flattened>> {
    let (source, path) = source::of(book)?;
    if source != Source::Epub {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    let found = bokai::formats::epub::flattened_styles(&bytes)
        .map_err(|e| anyhow::anyhow!("inspect the stylesheets: {e}"))?;
    Ok(found.map(|f| Flattened {
        sheets: f.sheets,
        generated_classes: f.generated_classes,
        producer: f.producer,
    }))
}

pub fn candidates(conn: &Connection, book: &BookRow) -> Result<Vec<Candidate>> {
    let mut rows: Vec<(u8, f64, BookRow)> = Vec::new();
    for row in db::list_books(conn)? {
        if row.id == book.id {
            continue;
        }
        let Ok((Source::Epub, _)) = source::of(&row) else {
            continue;
        };
        let same_series = book.series_name.is_some() && row.series_name == book.series_name;
        let same_author = !book.author.is_empty()
            && row.author.eq_ignore_ascii_case(&book.author)
            && row.publisher == book.publisher;
        if !same_series && !same_author {
            continue;
        }
        if flattened(&row).ok().flatten().is_some() {
            continue;
        }
        let distance = match (book.series_index, row.series_index) {
            (Some(a), Some(b)) if same_series => (a - b).abs(),
            _ => f64::MAX,
        };
        rows.push((u8::from(!same_series), distance, row));
    }
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.2.title.cmp(&b.2.title))
    });
    Ok(rows
        .into_iter()
        .map(|(_, _, r)| Candidate {
            id: r.id,
            title: r.title,
            author: r.author,
            series_name: r.series_name,
            series_index: r.series_index,
        })
        .collect())
}

pub fn restore(
    conn: &Connection,
    book: &BookRow,
    reference: &BookRow,
    write: bool,
    force: bool,
    out: Option<&std::path::Path>,
) -> Result<RestoreReport> {
    let (source, path) = source::of(book)?;
    if source != Source::Epub {
        anyhow::bail!("only an EPUB source has stylesheets to restore");
    }
    let (ref_source, ref_path) = source::of(reference)?;
    if ref_source != Source::Epub {
        anyhow::bail!("the reference must be an EPUB source");
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    let ref_bytes = std::fs::read(&ref_path).with_context(|| format!("read {ref_path}"))?;
    let restored = bokai::formats::epub::restore_styles(&bytes, &ref_bytes)
        .map_err(|e| anyhow::anyhow!("restore the stylesheets: {e}"))?;
    if let Some(out) = out {
        std::fs::write(out, &restored.bytes).with_context(|| format!("write {}", out.display()))?;
    }
    let material = restored.material_diffs();
    let blocked = (write && !force && material > 0).then(|| {
        format!(
            "{material} computed-style change(s) would alter the rendering; the reference may follow another template revision. Apply anyway with force."
        )
    });
    let written = write && blocked.is_none();
    if written {
        source::commit(conn, book.id, Source::Epub, &path, &restored.bytes)?;
    }
    Ok(RestoreReport {
        reference: reference.title.clone(),
        documents: restored.documents,
        classes: restored.classes,
        residual: restored.residual,
        residual_css: restored.residual_css,
        diffs: restored
            .diffs
            .into_iter()
            .map(|d| DiffInfo {
                document: d.document,
                text: d.text,
                property: d.property,
                before: d.before,
                after: d.after,
                count: d.count,
            })
            .collect(),
        material,
        written,
        blocked,
    })
}
