//! Writing books out of the library, as files someone else can read.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::library::db::{self, BookRow};
use crate::library::paths::dedup_path;

/// The file a caller wants written out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Epub,
    Pdf,
    Kfx,
    /// Plain text, produced by converting the book's content to Markdown.
    Txt,
}

impl Format {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "epub" => Format::Epub,
            "pdf" => Format::Pdf,
            "kfx" => Format::Kfx,
            "txt" => Format::Txt,
            other => anyhow::bail!("unknown export format: {other}"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Epub => "epub",
            Format::Pdf => "pdf",
            Format::Kfx => "kfx",
            Format::Txt => "txt",
        }
    }
}

/// Summary of a multi-book export.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    /// Files actually written.
    pub exported: usize,
    /// Books with no file of the requested format on disk (or a copy error).
    pub skipped: usize,
    /// The destination folder.
    pub dest: String,
    /// First few human-readable skip reasons (capped).
    pub errors: Vec<String>,
}

/// Write `format` for every book in `book_ids` into `dest_dir`, as a flat folder
/// of files (no per-author subfolders).
pub fn export_books(
    conn: &Connection,
    book_ids: &[i64],
    format: Format,
    dest_dir: &Path,
) -> anyhow::Result<Summary> {
    if !dest_dir.is_dir() {
        anyhow::bail!("{} is not a folder", dest_dir.display());
    }
    let mut summary = Summary {
        dest: dest_dir.to_string_lossy().to_string(),
        ..Default::default()
    };
    for &id in book_ids {
        let Some(book) = db::get_book(conn, id)? else {
            summary.skipped += 1;
            continue;
        };
        match export_one(&book, format, dest_dir) {
            Ok(_) => summary.exported += 1,
            Err(e) => {
                summary.skipped += 1;
                if summary.errors.len() < 8 {
                    summary.errors.push(format!("{}: {e:#}", book.title));
                }
            }
        }
    }
    Ok(summary)
}

/// Write one book's `format` into `dest_dir`, returning the file written.
pub fn export_one(book: &BookRow, format: Format, dest_dir: &Path) -> anyhow::Result<PathBuf> {
    // The source file to read. The copy formats name that exact file; `txt` is
    // generated from the best available content source — the EPUB if it's on
    // disk, else the universal KFX side.
    let src: Option<&Path> = match format {
        Format::Kfx => existing(book.kfx_path.as_deref()),
        Format::Epub => existing(book.epub_path.as_deref()),
        Format::Pdf => existing(book.pdf_path.as_deref()),
        Format::Txt => [book.epub_path.as_deref(), book.kfx_path.as_deref()]
            .into_iter()
            .flatten()
            .map(Path::new)
            .find(|p| p.exists()),
    };
    let Some(src) = src else {
        anyhow::bail!(
            "{}",
            match format {
                Format::Txt => "no EPUB or KFX source on disk".to_string(),
                other => format!("no {} file on disk", other.as_str().to_uppercase()),
            }
        );
    };

    // Target filename. Copy formats keep the source's name verbatim; `txt` swaps
    // the source's extension for `.txt` (both companion sides share the same
    // `[Author] Title (Year)` stem, so the source choice doesn't change it).
    let name = if format == Format::Txt {
        let stem = src
            .file_stem()
            .ok_or_else(|| anyhow::anyhow!("source has no filename"))?;
        let mut n = stem.to_os_string();
        n.push(".txt");
        n
    } else {
        src.file_name()
            .ok_or_else(|| anyhow::anyhow!("source has no filename"))?
            .to_os_string()
    };
    let target = dedup_path(dest_dir.join(name));

    if format == Format::Txt {
        export_as_txt(src, &target)?;
    } else {
        std::fs::copy(src, &target)?;
    }
    Ok(target)
}

fn existing(path: Option<&str>) -> Option<&Path> {
    path.map(Path::new).filter(|p| p.exists())
}

/// Convert a book file (EPUB or KFX, auto-detected by extension) to Markdown and
/// write it to `target`. CPU-bound: bokai's KFX decode and IR walk.
fn export_as_txt(src: &Path, target: &Path) -> anyhow::Result<()> {
    let mut book = bokai::Book::open(src).map_err(|e| anyhow::anyhow!("open: {e}"))?;
    let mut file = std::fs::File::create(target)?;
    book.export(bokai::Format::Markdown, &mut file)
        .map_err(|e| anyhow::anyhow!("convert: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_round_trip_their_names() {
        for name in ["epub", "pdf", "kfx", "txt"] {
            assert_eq!(Format::parse(name).unwrap().as_str(), name);
        }
        assert!(Format::parse("azw3").is_err());
    }
}
