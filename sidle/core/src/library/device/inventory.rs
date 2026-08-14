//! What is on the device right now, and which library row it belongs to.
//!
//! `documents/Sidle/` IS the source of truth — there is no on-device manifest
//! and no per-device table — so this scan always reflects exactly what the
//! Kindle holds, including files the reader deleted through its own UI (which
//! simply don't appear).

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use crate::library::db;
use crate::library::device::{TPath, Transport};
use crate::library::paths::parse_sha_infix;

/// One on-device file under `documents/Sidle/`, plus its link back to the local
/// library when one can be found.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// The file's sha8 (or basename) matched a library row.
    Sent {
        book_id: i64,
        sha256: String,
        title: String,
        author: String,
        filename: String,
    },
    /// Nothing in the local library matches: the book was removed here, or this
    /// Kindle was last paired with another machine.
    Orphan { sha8: String, filename: String },
}

impl Entry {
    pub fn filename(&self) -> &str {
        match self {
            Entry::Sent { filename, .. } | Entry::Orphan { filename, .. } => filename,
        }
    }
}

/// Scan `documents/Sidle/` and resolve every `*.<sha8>.kfx` against the library.
pub fn list_ours(conn: &Connection, transport: &dyn Transport) -> Result<Vec<Entry>> {
    let docs = TPath::parse("documents/Sidle");
    let entries = transport.list(&docs)?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        // `._foo.<sha>.kfx` AppleDouble companions carry the same sha8 suffix as
        // the real file and would otherwise be emitted as a duplicate row
        // pointing at the same book. Same for `.DS_Store` and friends.
        if entry.is_dir || entry.name.starts_with('.') {
            continue;
        }
        match resolve(conn, &entry.name)? {
            Some(book) => out.push(Entry::Sent {
                book_id: book.id,
                sha256: book.sha256,
                title: book.title,
                author: book.author,
                filename: entry.name,
            }),
            None => out.push(Entry::Orphan {
                // Legacy un-prefixed files have no sha8; the empty string is the
                // explicit sentinel for "unknown".
                sha8: parse_sha_infix(&entry.name).unwrap_or_default(),
                filename: entry.name,
            }),
        }
    }
    Ok(out)
}

/// The library row an on-device filename belongs to.
///
/// Primary match is the modern `<basename>.<sha8>.kfx` shape, by `kfx_sha256`
/// prefix. On a miss, fall back to the stem: a desktop reconvert changes
/// `kfx_sha256`, but the device filename is frozen at the old hash (the Kindle
/// won't re-bind a renamed `.sdr`), so the basename is the only stable link —
/// without it a reconverted book wrongly shows as "not in library". Legacy
/// pushes (pre-sha8 naming) carry the row's kfx basename verbatim.
fn resolve(conn: &Connection, filename: &str) -> Result<Option<db::BookRow>> {
    let Some(sha8) = parse_sha_infix(filename) else {
        return Ok(db::find_by_kfx_filename(conn, filename)?);
    };
    if let Some(book) = db::find_by_kfx_sha_prefix(conn, &sha8)? {
        return Ok(Some(book));
    }
    let stem = filename
        .strip_suffix(".kfx")
        .and_then(|s| s.rsplit_once('.'))
        .map_or(filename, |(stem, _sha)| stem);
    Ok(db::find_by_kfx_basename(conn, stem)?)
}
