//! Annotation ingest: turn parsed device sources into linked DB rows.
//!
//! The device scan + file IO live in the Tauri command; this module is the pure
//! logic, unit-testable against an in-memory DB:
//!   - [`match_book_id`] — device/clipping title → library `book_id` (T0/T1);
//!   - [`import_yjr`] — resolve `.yjr` handles (via [`anchor`]) and insert,
//!     idempotently (dedup hash), linked or as an orphan;
//!   - [`import_clipping_orphans`] — archive `My Clippings.txt` entries whose
//!     book isn't in the library;
//!   - [`relink_unmatched`] — re-link orphans once their book is added;
//!   - [`export_book_markdown`] / [`export_book_json`] — durability dumps.
//!
//! `.yjr` is authoritative for books in the library (precise anchors); `My
//! Clippings.txt` only contributes *orphans* (no library match), so the two
//! sources don't double-insert in the common path. (Known gap: a clippings
//! orphan that is later acquired + `.yjr`-imported isn't yet superseded — see
//! the TODO in [`relink_unmatched`].)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use boko::kfx_to_epub::TextIndex;

use super::anchor::{self, Resolved};
use super::clippings::Clipping;
use super::db::{self, NewAnnotation};
use super::yjr::{Annotation, Kind};

/// `source` column value for precise `.yjr`-derived annotations.
pub const SOURCE_YJR: &str = "yjr";
/// `source` column value for `My Clippings.txt` orphan archive entries.
pub const SOURCE_CLIPPINGS: &str = "clippings";

/// Outcome counts for one import call; sums across calls.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ImportStats {
    /// New rows written.
    pub inserted: usize,
    /// Rows skipped because an identical `dedup_hash` already existed.
    pub duplicate: usize,
    /// Span-bearing records (highlight/note) whose text didn't resolve (eid not
    /// in this book's KFX) — still inserted, but flagged here for the caller.
    pub unresolved: usize,
}

impl ImportStats {
    /// Accumulate another call's counts (the command sums per-book stats).
    pub fn merge(&mut self, other: ImportStats) {
        self.inserted += other.inserted;
        self.duplicate += other.duplicate;
        self.unresolved += other.unresolved;
    }
}

// ---------------------------------------------------------------------------
// Title matching
// ---------------------------------------------------------------------------

/// Normalise a title for comparison: strip the BOM and zero-width marks, collapse
/// internal whitespace, and trim. (Unicode NFC is a deferred refinement; in
/// practice both sides originate from the same book metadata, so byte-level
/// composition already agrees.)
pub fn normalize_title(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !matches!(*c, '\u{feff}' | '\u{200b}' | '\u{200c}' | '\u{200d}'))
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drop a single trailing **ASCII** `(...)` group (Kindle's author/imprint
/// suffix). Fullwidth `（…）` is part of the title and left alone.
fn strip_trailing_ascii_paren(s: &str) -> Option<&str> {
    let s = s.trim_end();
    let inner = s.strip_suffix(')')?;
    let open = inner.rfind('(')?;
    Some(s[..open].trim_end())
}

/// Drop a trailing ` - <author>` suffix (the `Title - Author` shape).
fn strip_trailing_author(s: &str) -> Option<&str> {
    let idx = s.rfind(" - ")?;
    Some(s[..idx].trim_end())
}

/// The key both library and device titles reduce to for T0 comparison.
fn match_key(title: &str) -> String {
    let n = normalize_title(title);
    strip_trailing_ascii_paren(&n).unwrap_or(&n).trim().to_string()
}

/// Match a device/clipping title to a library `book_id`.
///
/// T0: equality of [`match_key`] (normalised, trailing ASCII paren removed on
/// both sides). T1 (fallback): also strip a trailing ` - <author>` suffix from
/// the wanted title, so `…春- (ファミ通文庫)` matches at T0 before T1 ever fires.
pub fn match_book_id(conn: &Connection, title: &str) -> rusqlite::Result<Option<i64>> {
    let books = db::list_books(conn)?;
    let candidates: Vec<(i64, String)> = books.iter().map(|b| (b.id, match_key(&b.title))).collect();
    let want = match_key(title);

    if let Some((id, _)) = candidates.iter().find(|(_, k)| *k == want) {
        return Ok(Some(*id));
    }
    if let Some(stripped) = strip_trailing_author(&want) {
        let stripped = stripped.to_string();
        if let Some((id, _)) = candidates.iter().find(|(_, k)| *k == stripped) {
            return Ok(Some(*id));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Dedup
// ---------------------------------------------------------------------------

/// Stable identity for an annotation, so re-importing the same device state is a
/// no-op. Keyed on the book title + kind + anchor + text, so extending a
/// highlight (new end anchor) is correctly a *new* record, mirroring the device.
fn dedup_hash(book_key: &str, kind: &str, r: &Resolved) -> String {
    let mut h = Sha256::new();
    let i = |x: Option<i64>| x.map(|v| v.to_string()).unwrap_or_default();
    for part in [
        book_key,
        kind,
        &i(r.eid_start),
        &i(r.off_start),
        &i(r.eid_end),
        &i(r.off_end),
        &i(r.loc_start),
        &r.text,
        r.note_body.as_deref().unwrap_or(""),
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    format!("{:x}", h.finalize())
}

/// Dedup identity for a coarse clipping orphan (no `.yjr` anchor): title + kind +
/// Location + text.
fn clipping_dedup_hash(c: &Clipping) -> String {
    let mut h = Sha256::new();
    let i = |x: Option<i64>| x.map(|v| v.to_string()).unwrap_or_default();
    for part in [
        c.title.as_str(),
        c.kind.as_str(),
        &i(c.loc_start),
        &c.text,
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Resolve and insert one book's `.yjr` annotations. `book_id` is `None` when the
/// device book isn't in the library yet — the rows land as orphans carrying their
/// precise anchors, and [`relink_unmatched`] links them once the book is added.
/// `clip_title`/`clip_author` are stored so an orphan can be relinked by title.
#[allow(clippy::too_many_arguments)]
pub fn import_yjr(
    conn: &Connection,
    annotations: &[Annotation],
    idx: &TextIndex,
    book_id: Option<i64>,
    clip_title: Option<&str>,
    clip_author: Option<&str>,
    now: &str,
) -> rusqlite::Result<ImportStats> {
    let book_key = clip_title.map(match_key).unwrap_or_default();
    let mut stats = ImportStats::default();

    for ann in annotations {
        let r = anchor::resolve(ann, idx);
        let kind = r.kind.as_str();
        let is_span = matches!(r.kind, Kind::Highlight | Kind::Note);
        if is_span && !r.has_text() {
            stats.unresolved += 1;
        }
        let hash = dedup_hash(&book_key, kind, &r);
        let row = NewAnnotation {
            dedup_hash: &hash,
            book_id,
            kind,
            eid_start: r.eid_start,
            off_start: r.off_start,
            eid_end: r.eid_end,
            off_end: r.off_end,
            loc_start: r.loc_start,
            loc_end: r.loc_end,
            linear_pos: r.linear_pos,
            text: &r.text,
            note_body: r.note_body.as_deref(),
            color: None,
            clip_title,
            clip_author,
            added_at: None,
            added_raw: None,
            imported_at: now,
            source: SOURCE_YJR,
        };
        if db::insert_annotation(conn, &row)? {
            stats.inserted += 1;
        } else {
            stats.duplicate += 1;
        }
    }
    Ok(stats)
}

/// Archive `My Clippings.txt` entries whose book is **not** in the library, as
/// unlinked orphans. Entries whose title matches a library book are skipped —
/// their precise version comes from the book's `.yjr`.
pub fn import_clipping_orphans(
    conn: &Connection,
    clippings: &[Clipping],
    now: &str,
) -> rusqlite::Result<ImportStats> {
    let mut stats = ImportStats::default();
    for c in clippings {
        if match_book_id(conn, &c.title)?.is_some() {
            continue; // book is in the library → `.yjr` is authoritative.
        }
        let hash = clipping_dedup_hash(c);
        let row = NewAnnotation {
            dedup_hash: &hash,
            book_id: None,
            kind: c.kind.as_str(),
            eid_start: None,
            off_start: None,
            eid_end: None,
            off_end: None,
            loc_start: c.loc_start,
            loc_end: c.loc_end,
            linear_pos: c.loc_start,
            text: &c.text,
            note_body: None,
            color: None,
            clip_title: Some(&c.title),
            clip_author: c.author.as_deref(),
            added_at: None,
            added_raw: c.added_raw.as_deref(),
            imported_at: now,
            source: SOURCE_CLIPPINGS,
        };
        if db::insert_annotation(conn, &row)? {
            stats.inserted += 1;
        } else {
            stats.duplicate += 1;
        }
    }
    Ok(stats)
}

/// Re-link orphan annotations (`book_id IS NULL`) whose `clip_title` now matches
/// a library book. Run after every import and after a book is added/edited.
/// Returns the number of rows linked.
///
/// TODO(supersede): a clippings orphan linked here can duplicate a later `.yjr`
/// import of the same highlight (coarse vs precise). Superseding the coarse row
/// needs a (book, location) match that the two sources don't share exactly yet.
pub fn relink_unmatched(conn: &Connection) -> rusqlite::Result<usize> {
    let orphans = db::list_unlinked_annotations(conn)?;
    let mut linked = 0;
    for o in orphans {
        let Some(title) = o.clip_title.as_deref() else {
            continue;
        };
        if let Some(book_id) = match_book_id(conn, title)? {
            db::set_annotation_book_id(conn, o.id, book_id)?;
            linked += 1;
        }
    }
    Ok(linked)
}

// ---------------------------------------------------------------------------
// Export (durability — annotations outlive a DB rebuild)
// ---------------------------------------------------------------------------

/// Markdown dump of one book's annotations, in reading order.
pub fn export_book_markdown(conn: &Connection, book_id: i64) -> rusqlite::Result<String> {
    let book = db::get_book(conn, book_id)?;
    let anns = db::list_annotations_for_book(conn, book_id)?;
    let mut out = String::new();
    if let Some(b) = &book {
        out.push_str(&format!("# {}\n", b.title));
        if !b.author.is_empty() {
            out.push_str(&format!("_{}_\n", b.author));
        }
        out.push('\n');
    }
    for a in &anns {
        let loc = a
            .loc_start
            .or(a.linear_pos)
            .map(|l| format!(" (loc {l})"))
            .unwrap_or_default();
        match a.kind.as_str() {
            "bookmark" => out.push_str(&format!("- 🔖 Bookmark{loc}\n")),
            "note" => {
                out.push_str(&format!("- 📝 {}{loc}\n", a.text));
                if let Some(body) = &a.note_body {
                    out.push_str(&format!("  > {body}\n"));
                }
            }
            _ => out.push_str(&format!("- {}{loc}\n", a.text)),
        }
    }
    Ok(out)
}

/// JSON dump of one book's annotations (the full rows, for lossless re-import).
pub fn export_book_json(conn: &Connection, book_id: i64) -> rusqlite::Result<String> {
    let anns = db::list_annotations_for_book(conn, book_id)?;
    Ok(serde_json::to_string_pretty(&anns).unwrap_or_else(|_| "[]".to_string()))
}

// ---------------------------------------------------------------------------
// Device scan (the Tauri command's pure core)
// ---------------------------------------------------------------------------

/// Summary of one device import, returned to the UI.
#[derive(Debug, Default, serde::Serialize)]
pub struct DeviceImportReport {
    /// `.sdr` dirs in `documents/Sidle/` that carried a `.yjr`.
    pub yjr_books: usize,
    /// Of those, how many matched a library book (by `kfx_sha256` infix).
    pub matched: usize,
    /// `.sdr` names with a `.yjr` but no library match — skipped (no readable
    /// KFX to resolve text; `My Clippings.txt` is the fallback for these).
    pub unmatched: Vec<String>,
    /// Counts from `.yjr` imports, summed across matched books.
    pub annotations: ImportStats,
    /// Counts from `My Clippings.txt` orphan archiving.
    pub clippings: ImportStats,
    /// Orphans linked to a book by the post-import relink pass.
    pub relinked: usize,
}

/// Scan a mounted Kindle (`device_root` = the volume, e.g. `/Volumes/Kindle`)
/// and import its annotations.
///
/// Only `documents/Sidle/` is scanned: those `.sdr` dirs are named
/// `<stem>.<8hex>.sdr` where `<8hex>` is the library `kfx_sha256` prefix, so each
/// matches its library book exactly — and the `TextIndex` is built from the
/// library's own readable KFX, not the device file. `documents/Downloads/Items01/`
/// is deliberately ignored (DRM'd Amazon KFX can't be read). `My Clippings.txt`
/// then archives any orphan (no library match), and a relink pass links orphans
/// whose book has since been added.
pub fn import_from_device(
    conn: &Connection,
    device_root: &Path,
    now: &str,
) -> anyhow::Result<DeviceImportReport> {
    let mut report = DeviceImportReport::default();
    let sidle_dir = device_root.join("documents").join("Sidle");

    if let Ok(entries) = std::fs::read_dir(&sidle_dir) {
        for entry in entries.flatten() {
            let sdr = entry.path();
            if sdr.extension().and_then(|e| e.to_str()) != Some("sdr") {
                continue;
            }
            let Some(yjr_path) = find_yjr_in(&sdr) else {
                continue; // a pagination-cache `.sdr` with no annotations
            };
            report.yjr_books += 1;
            let sdr_name = sdr.file_name().and_then(|n| n.to_str()).unwrap_or_default();

            let book = match sdr_infix(sdr_name) {
                Some(infix) => db::find_by_kfx_sha_prefix(conn, infix)
                    .with_context(|| format!("kfx_sha lookup for {sdr_name}"))?,
                None => None,
            };
            let Some(book) = book else {
                report.unmatched.push(sdr_name.to_string());
                continue;
            };
            report.matched += 1;

            let idx = build_index(book.kfx_path.as_deref());
            let anns = super::yjr::parse_file(&yjr_path)
                .with_context(|| format!("parse {}", yjr_path.display()))?;
            let stats = import_yjr(
                conn,
                &anns,
                &idx,
                Some(book.id),
                Some(&book.title),
                Some(&book.author),
                now,
            )
            .context("import yjr annotations")?;
            report.annotations.merge(stats);
        }
    }

    let clip_path = device_root.join("documents").join("My Clippings.txt");
    if clip_path.exists() {
        let clips = super::clippings::parse_file(&clip_path).context("parse My Clippings")?;
        let stats = import_clipping_orphans(conn, &clips, now).context("import clipping orphans")?;
        report.clippings.merge(stats);
    }

    report.relinked = relink_unmatched(conn).context("relink unmatched")?;
    Ok(report)
}

/// The `.sdr` filename's `kfx_sha256` infix: the hex segment before `.sdr`. Only
/// returns it when it looks like a hash prefix (≥8 hex chars), so non-hash
/// schemes (`_<ASIN>.sdr`, `.boko.sdr`) fall through to "unmatched".
fn sdr_infix(sdr_name: &str) -> Option<&str> {
    let stem = sdr_name.strip_suffix(".sdr")?;
    let infix = stem.rsplit('.').next()?;
    (infix.len() >= 8 && infix.bytes().all(|b| b.is_ascii_hexdigit())).then_some(infix)
}

/// The live `.yjr` inside a `.sdr` dir, if any. Excludes `.yjr.bad_file` (a
/// device-rejected write) since that doesn't end in `.yjr`.
fn find_yjr_in(sdr_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(sdr_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".yjr"))
        })
}

/// A `TextIndex` from the library's readable KFX, or an empty one when the path
/// is missing/unreadable — annotations still import with their anchors (text
/// backfillable once the KFX exists).
fn build_index(kfx_path: Option<&str>) -> TextIndex {
    kfx_path
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| TextIndex::from_kfx(&bytes).ok())
        .unwrap_or_else(|| TextIndex::from_parts(HashMap::new(), HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db::{self, NewBook};
    use crate::library::yjr::Handle;
    use std::collections::HashMap;
    use std::path::Path;

    fn mem_db() -> Connection {
        db::open(Path::new(":memory:")).unwrap()
    }

    fn add_book(conn: &Connection, title: &str) -> i64 {
        db::insert_book(
            conn,
            &NewBook {
                sha256: title, // unique enough for tests
                title,
                author: "Author",
                language: "ja",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: None,
                kfx_sha256: None,
                file_size: 0,
                imported_at: "t0",
                asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
            },
        )
        .unwrap()
    }

    fn idx() -> TextIndex {
        let text_of: HashMap<i64, String> =
            [(10, "Hello world".to_string())].into_iter().collect();
        let pid_of: HashMap<i64, i64> = [(10, 100)].into_iter().collect();
        TextIndex::from_parts(text_of, pid_of)
    }

    fn highlight(eid: u32, os: u32, oe: u32) -> Annotation {
        Annotation {
            kind: Kind::Highlight,
            handles: vec![
                Handle { type_byte: 1, eid, offset: os, linear: 100 + os as u64, b64: String::new() },
                Handle { type_byte: 1, eid, offset: oe, linear: 100 + oe as u64, b64: String::new() },
            ],
            note_body: None,
        }
    }

    #[test]
    fn match_handles_paren_and_author_suffix() {
        let conn = mem_db();
        add_book(&conn, "文学少女");
        // T0: trailing ASCII (author) stripped from the device title.
        assert_eq!(
            match_book_id(&conn, "文学少女 (野村美月)").unwrap(),
            Some(1)
        );
        // BOM + extra whitespace tolerated.
        assert_eq!(
            match_book_id(&conn, "\u{feff}文学少女  ").unwrap(),
            Some(1)
        );
        // No match → None (orphan).
        assert_eq!(match_book_id(&conn, "Some Other Book").unwrap(), None);
    }

    #[test]
    fn match_t1_strips_author_dash_suffix() {
        let conn = mem_db();
        // Library stores the bare title; device adds " - Author (imprint)".
        add_book(&conn, "この恋と、その未来。1");
        assert_eq!(
            match_book_id(&conn, "この恋と、その未来。1 - 森橋 ビンゴ (ファミ通文庫)").unwrap(),
            Some(1)
        );
    }

    #[test]
    fn yjr_import_is_idempotent() {
        let conn = mem_db();
        let book = add_book(&conn, "B");
        let anns = vec![highlight(10, 0, 4)]; // "Hello" (inclusive end → +1)
        let s1 = import_yjr(&conn, &anns, &idx(), Some(book), Some("B"), None, "t0").unwrap();
        assert_eq!((s1.inserted, s1.duplicate), (1, 0));
        // Re-import: same dedup_hash → no new row.
        let s2 = import_yjr(&conn, &anns, &idx(), Some(book), Some("B"), None, "t1").unwrap();
        assert_eq!((s2.inserted, s2.duplicate), (0, 1));

        let stored = db::list_annotations_for_book(&conn, book).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "Hello");
        assert_eq!(stored[0].source, SOURCE_YJR);
    }

    #[test]
    fn unresolved_highlight_is_counted_but_still_stored() {
        let conn = mem_db();
        let book = add_book(&conn, "B");
        let anns = vec![highlight(999, 0, 4)]; // eid not in the index
        let s = import_yjr(&conn, &anns, &idx(), Some(book), Some("B"), None, "t").unwrap();
        assert_eq!(s.unresolved, 1);
        assert_eq!(s.inserted, 1);
        assert_eq!(db::list_annotations_for_book(&conn, book).unwrap()[0].text, "");
    }

    #[test]
    fn clipping_orphan_then_relink() {
        let conn = mem_db();
        let clips = vec![Clipping {
            title: "Ghost Book (Some Author)".to_string(),
            author: Some("Some Author".to_string()),
            kind: Kind::Highlight,
            page: None,
            loc_start: Some(42),
            loc_end: Some(43),
            added_raw: Some("Added when".to_string()),
            text: "orphaned highlight".to_string(),
        }];
        // No matching book → archived as an unlinked orphan.
        let s = import_clipping_orphans(&conn, &clips, "t").unwrap();
        assert_eq!(s.inserted, 1);
        assert_eq!(db::list_unlinked_annotations(&conn).unwrap().len(), 1);

        // Book shows up later (bare title) → relink by clip_title (T0 paren strip).
        let book = add_book(&conn, "Ghost Book");
        let linked = relink_unmatched(&conn).unwrap();
        assert_eq!(linked, 1);
        assert!(db::list_unlinked_annotations(&conn).unwrap().is_empty());
        assert_eq!(db::list_annotations_for_book(&conn, book).unwrap().len(), 1);
    }

    #[test]
    fn clipping_for_known_book_is_skipped() {
        let conn = mem_db();
        add_book(&conn, "Known");
        let clips = vec![Clipping {
            title: "Known (Author)".to_string(),
            author: Some("Author".to_string()),
            kind: Kind::Highlight,
            page: None,
            loc_start: Some(1),
            loc_end: Some(2),
            added_raw: None,
            text: "dup of yjr".to_string(),
        }];
        let s = import_clipping_orphans(&conn, &clips, "t").unwrap();
        assert_eq!(s.inserted, 0, "clipping for a library book must defer to .yjr");
    }

    #[test]
    fn markdown_export_renders_kinds() {
        let conn = mem_db();
        let book = add_book(&conn, "B");
        let mut anns = vec![highlight(10, 0, 4)];
        anns.push(Annotation {
            kind: Kind::Note,
            handles: vec![
                Handle { type_byte: 1, eid: 10, offset: 6, linear: 106, b64: String::new() },
                Handle { type_byte: 1, eid: 10, offset: 10, linear: 110, b64: String::new() },
            ],
            note_body: Some("a thought".to_string()),
        });
        import_yjr(&conn, &anns, &idx(), Some(book), Some("B"), None, "t").unwrap();
        let md = export_book_markdown(&conn, book).unwrap();
        assert!(md.contains("# B"));
        assert!(md.contains("- Hello"));
        assert!(md.contains("📝 world"));
        assert!(md.contains("> a thought"));
        // JSON export is valid and lists both rows.
        let json = export_book_json(&conn, book).unwrap();
        assert_eq!(json.matches("\"id\"").count(), 2);
    }

    #[test]
    fn sdr_infix_only_accepts_hash_prefixes() {
        assert_eq!(sdr_infix("[綾辻行人] 十角館 (2007).205b82bc.sdr"), Some("205b82bc"));
        assert_eq!(sdr_infix("Title_KPN6H7BZB6B5HTAHKMDZ.sdr"), None); // underscore ASIN scheme
        assert_eq!(sdr_infix("サクラダリセット5.boko.sdr"), None); // not hex
        assert_eq!(sdr_infix("nope"), None);
    }

    /// P1 gate, end-to-end on the live Kindle: scan `documents/Sidle/`, match the
    /// bungaku `.sdr` by its `kfx_sha256` infix to a library book pointed at the
    /// readable artifacts KFX, and confirm the highlight imports with the
    /// inclusive-end text. Skips when the Kindle or the sample KFX is absent.
    #[test]
    fn real_device_import_links_bungaku_highlight() {
        let kindle = Path::new("/Volumes/Kindle");
        let bungaku_kfx = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("artifacts/p0/bungaku.kfx");
        if !kindle.join("documents/Sidle").is_dir() || !bungaku_kfx.exists() {
            eprintln!("skipping: Kindle not mounted or bungaku.kfx absent");
            return;
        }

        let conn = mem_db();
        // Library book whose kfx_sha256 prefix == the device .sdr infix (7f4e9d33),
        // with its KFX path pointing at the readable artifacts copy.
        let kfx_sha = format!("7f4e9d33{}", "0".repeat(56)); // 64-hex, right prefix
        db::insert_book(
            &conn,
            &NewBook {
                sha256: "bungaku-src",
                title: "01 〝文学少女〟と死にたがりの道化",
                author: "野村美月",
                language: "ja",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: bungaku_kfx.to_str(),
                kfx_sha256: Some(&kfx_sha),
                file_size: 0,
                imported_at: "t0",
                asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
            },
        )
        .unwrap();

        let report = import_from_device(&conn, kindle, "now").unwrap();
        assert!(report.matched >= 1, "bungaku .sdr should match by infix");

        let book = db::find_by_kfx_sha_prefix(&conn, "7f4e9d33").unwrap().unwrap();
        let anns = db::list_annotations_for_book(&conn, book.id).unwrap();
        assert!(
            anns.iter().any(|a| a.text.ends_with("自分だけの大事な宝物だもの」")),
            "bungaku highlight not imported with inclusive end; got {:?}",
            anns.iter().map(|a| &a.text).collect::<Vec<_>>()
        );
    }
}
