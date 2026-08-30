//! A book's table of contents, judged and repaired from its own source file.

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::library::db::BookRow;
use crate::library::source::{self, Source};

/// What an audit concluded about one book's TOC, flattened to what a report
/// needs. The full evidence is bokai's `TocAudit`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Verdict {
    /// `OK` | `SUSPECT` | `FLATTENED` | `SPARSE` | `MISORDERED`.
    pub verdict: String,
    /// Entries the book's own TOC declares.
    pub entries: usize,
    /// Of those, the ones that are real chapters rather than front matter.
    pub chapters: usize,
    /// Chapter links on the book's in-content Contents page — what a repair
    /// would rebuild the TOC from.
    pub contents_links: usize,
    /// Headings in the content, the other thing a repair can mine.
    pub headings: usize,
}

impl Verdict {
    /// Whether this book's TOC is in good order.
    pub fn is_ok(&self) -> bool {
        self.verdict == "OK"
    }
}

/// Read a book's source and judge its TOC.
pub fn audit(book: &BookRow) -> Result<Verdict> {
    let (source, path) = source::of(book)?;
    if source == Source::Pdf {
        anyhow::bail!("a PDF's outline has nothing inside the book to be judged against");
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    let audit = bokai::validate::source::toc::validate(&bytes)
        .map_err(|e| anyhow::anyhow!("audit the table of contents: {e}"))?;
    Ok(Verdict {
        verdict: audit.verdict.as_str().to_string(),
        entries: audit.nav_count,
        chapters: audit.nav_chapters,
        contents_links: audit.contents_links,
        headings: audit.headings,
    })
}

/// Rebuild a book's TOC from what the book itself declares, and write it back.
pub fn repair(conn: &Connection, book: &BookRow) -> Result<Verdict> {
    let (source, path) = source::of(book)?;
    let bytes = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    let repaired = match source {
        Source::Kfx => bokai::formats::kfx::toc_repair::repair_toc(&bytes)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        Source::Epub => bokai::formats::epub::toc_repair::repair_toc(&bytes)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        Source::Pdf => anyhow::bail!(
            "a PDF's table of contents can't be derived automatically — add the entries by hand"
        ),
    };
    source::commit(conn, book.id, source, &path, &repaired)?;
    audit(book)
}
