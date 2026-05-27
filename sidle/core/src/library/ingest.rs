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
/// `source` column value for native, Sidle-created annotations (T0).
pub const SOURCE_SIDLE: &str = "sidle";

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
    /// Annotation rows removed because they were deleted on the device and no
    /// other device still asserts them (full-mirror delete-propagation).
    pub removed: usize,
}

impl ImportStats {
    /// Accumulate another call's counts (the command sums per-book stats).
    pub fn merge(&mut self, other: ImportStats) {
        self.inserted += other.inserted;
        self.duplicate += other.duplicate;
        self.unresolved += other.unresolved;
        self.removed += other.removed;
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

/// The `book_key` an annotation's [`annotation_dedup_hash`] is salted with —
/// exposed so the reader's native-annotation create path derives the SAME key
/// from a library book's title that device imports use, making a passage
/// highlighted both natively and on the Kindle hash identically.
pub fn book_match_key(title: &str) -> String {
    match_key(title)
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

/// The canonical content identity for an annotation, shared by the device-import
/// path ([`dedup_hash`]) and native (Sidle-created) annotations, so the same
/// passage highlighted both on a Kindle and in Sidle hashes identically (→ one
/// row). Keyed on book + kind + anchor + text + note body — NO timestamp, NO
/// device id, NO source — so identity is content + anchor only. The byte sequence
/// (each part NUL-terminated) is load-bearing; do not reorder.
#[allow(clippy::too_many_arguments)]
pub fn annotation_dedup_hash(
    book_key: &str,
    kind: &str,
    eid_start: Option<i64>,
    off_start: Option<i64>,
    eid_end: Option<i64>,
    off_end: Option<i64>,
    loc_start: Option<i64>,
    text: &str,
    note_body: &str,
) -> String {
    let mut h = Sha256::new();
    let i = |x: Option<i64>| x.map(|v| v.to_string()).unwrap_or_default();
    for part in [
        book_key,
        kind,
        &i(eid_start),
        &i(off_start),
        &i(eid_end),
        &i(off_end),
        &i(loc_start),
        text,
        note_body,
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    format!("{:x}", h.finalize())
}

/// Stable identity for a device-resolved annotation, so re-importing the same
/// device state is a no-op. Thin wrapper over [`annotation_dedup_hash`] (the
/// shared codec) — extending a highlight (new end anchor) is correctly a *new*
/// record, mirroring the device.
fn dedup_hash(book_key: &str, kind: &str, r: &Resolved) -> String {
    annotation_dedup_hash(
        book_key,
        kind,
        r.eid_start,
        r.off_start,
        r.eid_end,
        r.off_end,
        r.loc_start,
        &r.text,
        r.note_body.as_deref().unwrap_or(""),
    )
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
    device_serial: Option<&str>,
    now: &str,
) -> rusqlite::Result<ImportStats> {
    let book_key = clip_title.map(match_key).unwrap_or_default();
    let mut stats = ImportStats::default();
    // The device's full current set for this book — feeds delete-reconcile.
    let mut current_hashes = Vec::with_capacity(annotations.len());

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
        current_hashes.push(hash);
    }

    // Delete-propagation: reconcile this device's asserted set for the book, so
    // annotations deleted on the device (and held by no other device) are
    // removed. Only when both device and book are known — orphan imports
    // (`book_id` None) and clipping archives carry no device authority.
    if let (Some(serial), Some(bid)) = (device_serial, book_id) {
        let removed = db::reconcile_device_book(conn, serial, bid, &current_hashes, now)?;
        stats.removed += removed.len();
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
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct DeviceImportReport {
    /// `.sdr` dirs in `documents/Sidle/` that carried a `.yjr`.
    pub yjr_books: usize,
    /// Of those, how many matched a library book (by `kfx_sha256` infix).
    pub matched: usize,
    /// `.sdr` names with a `.yjr` but no library match — skipped (no readable
    /// KFX to resolve text; `My Clippings.txt` is the fallback for these).
    pub unmatched: Vec<String>,
    /// Matched books whose `.yjr` was byte-identical to the last import, so the
    /// (expensive) TextIndex rebuild + re-parse was skipped entirely.
    pub unchanged: usize,
    /// Counts from `.yjr` imports, summed across matched books.
    pub annotations: ImportStats,
    /// Counts from `My Clippings.txt` orphan archiving.
    pub clippings: ImportStats,
    /// Orphans linked to a book by the post-import relink pass.
    pub relinked: usize,
    /// Matched books whose `.yjf` last-read position was imported (stored as the
    /// `(book_id, 'device')` `reading_position` row — a Resume jump target).
    pub positions: usize,
}

/// The reading-state sidecars pulled from one `.sdr` directory, tagged with the
/// directory name (which carries the `kfx_sha256` infix that matches it to a
/// library book). Bytes are read on the device side — `std::fs` for
/// mass-storage, MTP `GetObject` for Scribe-class — so [`import_collected`]
/// stays transport-agnostic and the device-IO half can live wherever the
/// transport does (the app's `device` module). At least one of the two is
/// present (a `.sdr` with neither is a pagination cache, not collected):
/// `.yjr` carries annotations, `.yjf` the last-read position — and a book read
/// without highlighting has a `.yjf` but no `.yjr`.
pub struct CollectedYjr {
    pub sdr_name: String,
    /// `<book>.yjr` bytes (annotations), if present.
    pub yjr_bytes: Option<Vec<u8>>,
    /// `<book>.yjf` bytes (last-read position `lpr`/`fpr`), if present.
    pub yjf_bytes: Option<Vec<u8>>,
}

/// Import device annotations a caller has already pulled off the device: match
/// each `.yjr` to a library book by its `.sdr` `kfx_sha256` infix, skip the ones
/// whose bytes are unchanged since last import, resolve highlight text from the
/// library's own readable KFX, and archive any `My Clippings.txt` orphans. This
/// is the pure DB + parse half shared by both transports; the device-side scan
/// lives in the caller — [`import_from_device`] is the mass-storage `std::fs`
/// scanner, and the app has a `Transport`-based one (`collect_device_yjr`) for
/// MTP (Scribe).
pub fn import_collected(
    conn: &Connection,
    collected: Vec<CollectedYjr>,
    clippings_txt: Option<&str>,
    device_serial: &str,
    now: &str,
) -> anyhow::Result<DeviceImportReport> {
    let mut report = DeviceImportReport::default();

    for item in collected {
        let CollectedYjr { sdr_name, yjr_bytes, yjf_bytes } = item;
        let book = match sdr_infix(&sdr_name) {
            Some(infix) => db::find_by_kfx_sha_prefix(conn, infix)
                .with_context(|| format!("kfx_sha lookup for {sdr_name}"))?,
            None => None,
        };

        // Last-read position (`.yjf` `lpr`) — stored for every matched book on
        // EVERY sync, before the unchanged-`.yjr` skip below: position moves
        // independently of highlights (you can read on without highlighting), so
        // gating it on the `.yjr` would freeze it. Cheap and idempotent — a handle
        // decode + single-row upsert, no TextIndex. Lands in the `(book_id,
        // 'device')` row; never auto-applied (it's a Resume jump target).
        if let (Some(book), Some(yjf)) = (book.as_ref(), yjf_bytes.as_ref())
            && let Some(h) = super::yjr::decode_position(yjf, "lpr")
        {
            db::set_reading_position(
                conn,
                book.id,
                Some(i64::from(h.eid)),
                Some(i64::from(h.offset)),
                Some(h.linear as i64),
                "device",
                device_serial,
            )
            .context("store device reading position")?;
            report.positions += 1;
        }

        // Annotations (`.yjr`). A `.sdr` carrying only a `.yjf` (read, never
        // highlighted) has no annotations to import — its position is done above.
        let Some(yjr_bytes) = yjr_bytes else { continue };
        report.yjr_books += 1;
        let Some(book) = book else {
            report.unmatched.push(sdr_name);
            continue;
        };
        report.matched += 1;

        // Cheap skip before the expensive `build_index` (which parses the full
        // library KFX): if this device's on-device `.yjr` is byte-for-byte what
        // we imported last time, there's nothing new (no inserts, and no deletes
        // to reconcile). The bytes are already in hand (the `.yjr` is tiny — KB),
        // so this is just a hash + compare. Keyed per device.
        let yjr_sha = super::import::sha256_of_bytes(&yjr_bytes);
        if db::get_yjr_sync_sha(conn, device_serial, book.id)
            .context("read yjr sync checkpoint")?
            .as_deref()
            == Some(yjr_sha.as_str())
        {
            report.unchanged += 1;
            continue;
        }

        let idx = build_index(book.kfx_path.as_deref());
        let anns = super::yjr::parse(&yjr_bytes);
        let stats = import_yjr(
            conn,
            &anns,
            &idx,
            Some(book.id),
            Some(&book.title),
            Some(&book.author),
            Some(device_serial),
            now,
        )
        .context("import yjr annotations")?;
        report.annotations.merge(stats);
        // Checkpoint so this device's next connect skips this `.yjr` unless it
        // changes.
        db::set_yjr_sync_sha(conn, device_serial, book.id, &yjr_sha, now)
            .context("record yjr sync checkpoint")?;
    }

    if let Some(txt) = clippings_txt {
        let clips = super::clippings::parse(txt);
        let stats = import_clipping_orphans(conn, &clips, now).context("import clipping orphans")?;
        report.clippings.merge(stats);
    }

    report.relinked = relink_unmatched(conn).context("relink unmatched")?;
    Ok(report)
}

/// Scan a mounted Kindle (`device_root` = the volume, e.g. `/Volumes/Kindle`)
/// and import its annotations — the mass-storage `std::fs` collector for
/// [`import_collected`].
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
    device_serial: &str,
    now: &str,
) -> anyhow::Result<DeviceImportReport> {
    let sidle_dir = device_root.join("documents").join("Sidle");
    let mut collected = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&sidle_dir) {
        for entry in entries.flatten() {
            let sdr = entry.path();
            if sdr.extension().and_then(|e| e.to_str()) != Some("sdr") {
                continue;
            }
            let yjr_path = find_sidecar(&sdr, ".yjr");
            let yjf_path = find_sidecar(&sdr, ".yjf");
            if yjr_path.is_none() && yjf_path.is_none() {
                continue; // a pagination-cache `.sdr` — no annotations, no position
            }
            let sdr_name = sdr
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let read = |p: Option<PathBuf>| -> anyhow::Result<Option<Vec<u8>>> {
                p.map(|p| std::fs::read(&p).with_context(|| format!("read {}", p.display())))
                    .transpose()
            };
            collected.push(CollectedYjr {
                sdr_name,
                yjr_bytes: read(yjr_path)?,
                yjf_bytes: read(yjf_path)?,
            });
        }
    }

    // Orphan archive: read it on the device side and hand the text to the shared
    // importer. `from_utf8_lossy` matches `clippings::parse_file`; a read error
    // (vs. simply absent) just means no orphans, never a failed import.
    let clip_path = device_root.join("documents").join("My Clippings.txt");
    let clippings_txt = std::fs::read(&clip_path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned());

    import_collected(conn, collected, clippings_txt.as_deref(), device_serial, now)
}

/// The `.sdr` filename's `kfx_sha256` infix: the hex segment before `.sdr`. Only
/// returns it when it looks like a hash prefix (≥8 hex chars), so non-hash
/// schemes (`_<ASIN>.sdr`, `.boko.sdr`) fall through to "unmatched".
fn sdr_infix(sdr_name: &str) -> Option<&str> {
    let stem = sdr_name.strip_suffix(".sdr")?;
    let infix = stem.rsplit('.').next()?;
    (infix.len() >= 8 && infix.bytes().all(|b| b.is_ascii_hexdigit())).then_some(infix)
}

/// The live sidecar matching `suffix` (`.yjr` / `.yjf`) inside a `.sdr` dir, if
/// any. The `ends_with` test excludes `.yjr.bad_file` (a device-rejected write)
/// since that doesn't end in `.yjr`.
fn find_sidecar(sdr_dir: &Path, suffix: &str) -> Option<PathBuf> {
    std::fs::read_dir(sdr_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
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
    fn native_and_device_dedup_hash_agree() {
        // The shared codec (the native create path uses `annotation_dedup_hash`)
        // and the device wrapper (`dedup_hash`) must produce the SAME hash for the
        // same book + kind + anchor + text + note — so a passage highlighted both
        // in Sidle and on a Kindle dedups to one row.
        let r = Resolved {
            kind: Kind::Highlight,
            eid_start: Some(1254),
            off_start: Some(44),
            eid_end: Some(1257),
            off_end: Some(68),
            loc_start: Some(12937),
            loc_end: Some(12961),
            linear_pos: Some(12937),
            text: "走れメロス".to_string(),
            note_body: None,
        };
        let device = dedup_hash("メロス", "highlight", &r);
        let native = annotation_dedup_hash(
            "メロス",
            "highlight",
            r.eid_start,
            r.off_start,
            r.eid_end,
            r.off_end,
            r.loc_start,
            &r.text,
            "",
        );
        assert_eq!(device, native);
    }

    #[test]
    fn yjr_import_is_idempotent() {
        let conn = mem_db();
        let book = add_book(&conn, "B");
        let anns = vec![highlight(10, 0, 4)]; // "Hello" (inclusive end → +1)
        let s1 = import_yjr(&conn, &anns, &idx(), Some(book), Some("B"), None, None, "t0").unwrap();
        assert_eq!((s1.inserted, s1.duplicate), (1, 0));
        // Re-import: same dedup_hash → no new row.
        let s2 = import_yjr(&conn, &anns, &idx(), Some(book), Some("B"), None, None, "t1").unwrap();
        assert_eq!((s2.inserted, s2.duplicate), (0, 1));

        let stored = db::list_annotations_for_book(&conn, book).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "Hello");
        assert_eq!(stored[0].source, SOURCE_YJR);
    }

    #[test]
    fn device_delete_propagates_through_import_yjr() {
        let conn = mem_db();
        let book = add_book(&conn, "B");

        // DEV1's first sync: two annotations.
        let first = vec![highlight(10, 0, 4), highlight(20, 0, 4)];
        let s1 = import_yjr(&conn, &first, &idx(), Some(book), Some("B"), None, Some("DEV1"), "t1").unwrap();
        assert_eq!((s1.inserted, s1.removed), (2, 0));
        assert_eq!(db::list_annotations_for_book(&conn, book).unwrap().len(), 2);

        // Next DEV1 sync: the second annotation was deleted on the device → GC'd.
        let second = vec![highlight(10, 0, 4)];
        let s2 = import_yjr(&conn, &second, &idx(), Some(book), Some("B"), None, Some("DEV1"), "t2").unwrap();
        assert_eq!(s2.removed, 1, "deleted-on-device annotation is removed");
        assert_eq!(db::list_annotations_for_book(&conn, book).unwrap().len(), 1);

        // An import with no device serial (orphan/clipping path) never reconciles
        // — add-only, as before. An empty set must not wipe the survivor.
        let s3 = import_yjr(&conn, &[], &idx(), Some(book), Some("B"), None, None, "t3").unwrap();
        assert_eq!(s3.removed, 0);
        assert_eq!(
            db::list_annotations_for_book(&conn, book).unwrap().len(),
            1,
            "deviceless import never deletes"
        );
    }

    #[test]
    fn unresolved_highlight_is_counted_but_still_stored() {
        let conn = mem_db();
        let book = add_book(&conn, "B");
        let anns = vec![highlight(999, 0, 4)]; // eid not in the index
        let s = import_yjr(&conn, &anns, &idx(), Some(book), Some("B"), None, None, "t").unwrap();
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
        import_yjr(&conn, &anns, &idx(), Some(book), Some("B"), None, None, "t").unwrap();
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

    /// A second connect with an unchanged `.yjr` is skipped wholesale — no
    /// re-parse, no re-insert — by the per-book content-hash checkpoint. (The
    /// `dedup_hash` already made re-import a no-op at the DB layer; this avoids
    /// the expensive TextIndex rebuild that precedes it.)
    #[test]
    fn device_import_skips_unchanged_yjr() {
        use base64::Engine as _;

        // `.yjr` token = [marker][len:3 BE][payload]; one bookmark record is a
        // key followed by a single anchor-handle string value.
        fn token(marker: u8, payload: &[u8]) -> Vec<u8> {
            let len = payload.len();
            let mut v = vec![marker, (len >> 16) as u8, (len >> 8) as u8, len as u8];
            v.extend_from_slice(payload);
            v
        }
        fn handle(eid: u32, off: u32, linear: u64) -> String {
            let mut raw = vec![1u8];
            raw.extend_from_slice(&eid.to_le_bytes());
            raw.extend_from_slice(&off.to_le_bytes());
            let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&raw);
            format!("{b64}:{linear}")
        }
        let mut yjr = Vec::new();
        yjr.extend(token(0xfe, b"annotation.personal.bookmark"));
        yjr.extend(token(0x03, handle(1492, 0, 9).as_bytes()));

        // Device tree: documents/Sidle/<stem>.<sha8>.sdr/<file>.yjr
        let root = tempfile::tempdir().unwrap();
        let sdr = root.path().join("documents/Sidle/book.deadbeef.sdr");
        std::fs::create_dir_all(&sdr).unwrap();
        std::fs::write(sdr.join("book.deadbeef0000.yjr"), &yjr).unwrap();

        // A library book whose kfx_sha256 the `.sdr` infix prefix-matches.
        let conn = mem_db();
        let book_id = db::insert_book(
            &conn,
            &NewBook {
                sha256: "book-sha",
                title: "栞のある本",
                author: "Author",
                language: "ja",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: None, // empty TextIndex; a bookmark imports on anchor alone
                kfx_sha256: Some(
                    "deadbeef00000000000000000000000000000000000000000000000000000000",
                ),
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

        let r1 = import_from_device(&conn, root.path(), "DEV", "now").unwrap();
        assert_eq!(r1.matched, 1);
        assert_eq!(r1.unchanged, 0);
        assert!(r1.annotations.inserted >= 1, "first import should insert the bookmark");
        assert!(db::get_yjr_sync_sha(&conn, "DEV", book_id).unwrap().is_some(), "checkpoint recorded");

        // Identical `.yjr` on the next connect → skipped, nothing inserted.
        let r2 = import_from_device(&conn, root.path(), "DEV", "now").unwrap();
        assert_eq!(r2.matched, 1);
        assert_eq!(r2.unchanged, 1);
        assert_eq!(r2.annotations.inserted, 0);
    }

    #[test]
    fn device_import_pulls_yjf_last_position() {
        use base64::Engine as _;
        fn token(marker: u8, payload: &[u8]) -> Vec<u8> {
            let len = payload.len();
            let mut v = vec![marker, (len >> 16) as u8, (len >> 8) as u8, len as u8];
            v.extend_from_slice(payload);
            v
        }
        fn handle(eid: u32, off: u32, linear: u64) -> String {
            let mut raw = vec![1u8];
            raw.extend_from_slice(&eid.to_le_bytes());
            raw.extend_from_slice(&off.to_le_bytes());
            let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&raw);
            format!("{b64}:{linear}")
        }
        // A `.yjf` carrying an `lpr` position (eid 978, off 170) — and NO `.yjr`,
        // i.e. a book read but never highlighted. Its position must still import.
        let mut yjf = Vec::new();
        yjf.extend(token(0xfe, b"lpr"));
        yjf.extend(token(0x03, handle(978, 170, 12345).as_bytes()));

        let root = tempfile::tempdir().unwrap();
        let sdr = root.path().join("documents/Sidle/book.deadbeef.sdr");
        std::fs::create_dir_all(&sdr).unwrap();
        std::fs::write(sdr.join("book.deadbeef0000.yjf"), &yjf).unwrap();

        let conn = mem_db();
        let book_id = db::insert_book(
            &conn,
            &NewBook {
                sha256: "book-sha",
                title: "位置だけの本",
                author: "Author",
                language: "ja",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: None,
                kfx_sha256: Some(
                    "deadbeef00000000000000000000000000000000000000000000000000000000",
                ),
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

        let r = import_from_device(&conn, root.path(), "DEV", "now").unwrap();
        assert_eq!(r.positions, 1, "the .yjf lpr position imported");
        assert_eq!(r.yjr_books, 0, "no .yjr present");
        assert_eq!(r.matched, 0, "matched counts .yjr books; a position-only .sdr isn't one");

        let pos = db::list_reading_positions(&conn, book_id).unwrap();
        assert_eq!(pos.len(), 1);
        assert_eq!(pos[0].source, "device");
        assert_eq!(pos[0].device_serial, "DEV", "tagged with the importing device's serial");
        assert_eq!(
            (pos[0].eid, pos[0].offset, pos[0].linear_pos),
            (Some(978), Some(170), Some(12345)),
        );

        // Re-sync is idempotent: same single 'device' row (upsert, not a 2nd row).
        let r2 = import_from_device(&conn, root.path(), "DEV", "now").unwrap();
        assert_eq!(r2.positions, 1);
        assert_eq!(db::list_reading_positions(&conn, book_id).unwrap().len(), 1);
    }
}
