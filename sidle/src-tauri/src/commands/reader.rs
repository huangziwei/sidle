//! Tauri command for the built-in reader: KFX → renderable DOM.
//!
//! `reader_open` looks up a book's KFX path, runs boko's KFX→DOM front half
//! (with `data-eid` stamping so annotations can anchor), and returns the
//! sections + resources + TOC + metadata the webview reader needs. Resources
//! (images, `style.css`) are base64-encoded for the JSON IPC boundary; the
//! frontend rebuilds blobs from them.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;
use tauri::State;

use crate::library::db::{self, AnnotationRow};
use crate::library::ingest;
use crate::state::AppState;

/// One spine document in reading order. `html` carries `data-eid` attributes.
#[derive(Debug, Serialize)]
pub struct ReaderSectionDto {
    pub href: String,
    pub html: String,
}

/// A non-spine asset the chapters reference by relative href.
#[derive(Debug, Serialize)]
pub struct ReaderResourceDto {
    pub href: String,
    pub mime: String,
    /// base64 (STANDARD, padded) of the resource bytes.
    pub data_base64: String,
}

/// Table-of-contents entry; `href` points into a section (`"c5.xhtml#anchor"`).
#[derive(Debug, Serialize)]
pub struct ReaderTocDto {
    pub label: String,
    pub href: String,
    pub children: Vec<ReaderTocDto>,
}

/// The reader's view of a book, ready for the webview paginator.
#[derive(Debug, Serialize)]
pub struct ReaderBookDto {
    pub sections: Vec<ReaderSectionDto>,
    pub resources: Vec<ReaderResourceDto>,
    pub toc: Vec<ReaderTocDto>,
    pub title: String,
    pub authors: Vec<String>,
    pub language: String,
    /// `"vertical-rl"` / `"horizontal-tb"` — drives the reader's writing mode.
    pub writing_mode: String,
    /// `"rtl"` / `"ltr"` — spine progression / page-turn direction.
    pub page_progression_direction: String,
    /// `[eid, linear]` pairs for the Location/% readout (real device Loc when the
    /// KFX has a position map, else reader-synthesized). See `ReaderBook::locations`.
    pub locations: Vec<(i64, i64)>,
    /// Largest linear position — the denominator for whole-book %.
    pub max_location: i64,
}

fn map_toc(points: Vec<boko::kfx_to_epub::navigation::NavPoint>) -> Vec<ReaderTocDto> {
    points
        .into_iter()
        .map(|p| ReaderTocDto {
            label: p.label,
            href: p.href,
            children: map_toc(p.children),
        })
        .collect()
}

impl From<boko::kfx_to_epub::ReaderBook> for ReaderBookDto {
    fn from(b: boko::kfx_to_epub::ReaderBook) -> Self {
        ReaderBookDto {
            sections: b
                .sections
                .into_iter()
                .map(|s| ReaderSectionDto {
                    href: s.href,
                    html: s.html,
                })
                .collect(),
            resources: b
                .resources
                .into_iter()
                .map(|r| ReaderResourceDto {
                    href: r.href,
                    mime: r.mime,
                    data_base64: B64.encode(&r.data),
                })
                .collect(),
            toc: map_toc(b.toc),
            title: b.metadata.title,
            authors: b.metadata.authors,
            language: b.metadata.language,
            writing_mode: b.writing_mode,
            page_progression_direction: b.page_progression_direction,
            locations: b.locations,
            max_location: b.max_location,
        }
    }
}

/// Open a library book for the reader: KFX → DOM sections + resources + TOC.
#[tauri::command]
pub async fn reader_open(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<ReaderBookDto, String> {
    // Fetch the KFX path under the lock, then release it before the CPU-bound
    // parse/render (which can take a beat on a large book).
    let kfx_path = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?
            .kfx_path
            .ok_or_else(|| "this book has no KFX file yet".to_string())?
    };

    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&kfx_path).map_err(|e| format!("read {kfx_path}: {e}"))?;
        let book = boko::kfx_to_epub::kfx_to_reader_book(&bytes)
            .map_err(|e| format!("KFX→DOM render failed: {e}"))?;
        Ok::<ReaderBookDto, String>(ReaderBookDto::from(book))
    })
    .await
    .map_err(|e| format!("reader task join error: {e}"))?
}

/// One stored annotation, shaped for the reader's painter + sidebar.
#[derive(Debug, Serialize)]
pub struct AnnotationDto {
    pub id: i64,
    /// `"highlight"` | `"note"` | `"bookmark"` | other.
    pub kind: String,
    pub eid_start: Option<i64>,
    pub off_start: Option<i64>,
    pub eid_end: Option<i64>,
    pub off_end: Option<i64>,
    pub loc_start: Option<i64>,
    pub loc_end: Option<i64>,
    /// Highlighted text (or bookmark/element preview).
    pub text: String,
    pub note_body: Option<String>,
    /// CSS color hint, if the source carried one.
    pub color: Option<String>,
    /// `"yjr"` | `"clippings"` — provenance.
    pub source: String,
}

impl From<AnnotationRow> for AnnotationDto {
    fn from(a: AnnotationRow) -> Self {
        AnnotationDto {
            id: a.id,
            kind: a.kind,
            eid_start: a.eid_start,
            off_start: a.off_start,
            eid_end: a.eid_end,
            off_end: a.off_end,
            loc_start: a.loc_start,
            loc_end: a.loc_end,
            text: a.text,
            note_body: a.note_body,
            color: a.color,
            source: a.source,
        }
    }
}

/// List a book's stored annotations, ordered by reading position. The reader
/// paints highlights/notes and lists them in the sidebar.
#[tauri::command]
pub async fn annotations_for_book(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<AnnotationDto>, String> {
    let conn = state.db.lock().await;
    let rows = db::list_annotations_for_book(&conn, book_id).map_err(|e| e.to_string())?;
    Ok(rows.into_iter().map(AnnotationDto::from).collect())
}

/// One stored last-read position. `source` = `"sidle"` (the reader's own) or
/// `"device"`; for a device row, `device_serial` is the Kindle's serial so the
/// reader can label/distinguish multiple devices. The anchor is an `(eid,
/// offset)` pair, resolved to a DOM element exactly like an annotation;
/// `linear_pos` is the human "Location" for the menu label.
#[derive(Debug, Serialize)]
pub struct ReadingPositionDto {
    pub eid: Option<i64>,
    pub offset: Option<i64>,
    pub linear_pos: Option<i64>,
    pub source: String,
    pub device_serial: String,
    pub updated_at: String,
}

/// A book's saved positions — the reader's own ('sidle') plus one per Kindle
/// that synced it ('device', keyed by serial). The reader auto-restores the
/// 'sidle' one on open and offers all of them as Resume jump targets.
#[tauri::command]
pub async fn reading_position_get(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<ReadingPositionDto>, String> {
    let conn = state.db.lock().await;
    let rows = db::list_reading_positions(&conn, book_id).map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|p| ReadingPositionDto {
            eid: p.eid,
            offset: p.offset,
            linear_pos: p.linear_pos,
            source: p.source,
            device_serial: p.device_serial,
            updated_at: p.updated_at,
        })
        .collect())
}

/// Save the reader's OWN last-read position (`source='sidle'`). Called on close,
/// so the Sidle position only moves between sessions. Device positions are
/// written by the import path (keyed by serial), never through this command.
#[tauri::command]
pub async fn reading_position_set(
    state: State<'_, AppState>,
    book_id: i64,
    eid: Option<i64>,
    offset: Option<i64>,
    linear_pos: Option<i64>,
) -> Result<(), String> {
    let conn = state.db.lock().await;
    db::set_reading_position(&conn, book_id, eid, offset, linear_pos, "sidle", "")
        .map_err(|e| e.to_string())
}

/// One search hit. `off_end` is the **inclusive** last-char index of the match
/// (matches the annotation `(eid_start, off_start, eid_end, off_end)` convention,
/// so JS `rangeFor`'s end-inclusive `+1` walk paints the correct characters).
/// `preview_*` is a three-piece split for the UI — render before + match + after,
/// usually with `match` highlighted.
#[derive(Debug, Serialize)]
pub struct SearchMatchDto {
    pub eid: i64,
    pub off_start: i64,
    pub off_end: i64,
    pub linear_pos: i64,
    pub preview_before: String,
    pub preview_match: String,
    pub preview_after: String,
}

impl From<boko::kfx_to_epub::SearchMatch> for SearchMatchDto {
    fn from(m: boko::kfx_to_epub::SearchMatch) -> Self {
        SearchMatchDto {
            eid: m.eid,
            off_start: m.off_start as i64,
            off_end: m.off_end as i64,
            linear_pos: m.linear_pos,
            preview_before: m.preview_before,
            preview_match: m.preview_match,
            preview_after: m.preview_after,
        }
    }
}

/// In-book full-text search. v1 = strict char match, ASCII case-insensitive,
/// intra-eid only — see `TextIndex::search`.
///
/// Reuses a per-session `TextIndex` cache (one entry, keyed by `book_id`):
/// the first search per book parses the KFX once on the blocking pool (same
/// cost as `reader_open`); subsequent searches are pure `HashMap` walks.
/// Switching to a different `book_id` rebuilds.
#[tauri::command]
pub async fn book_search(
    state: State<'_, AppState>,
    book_id: i64,
    query: String,
) -> Result<Vec<SearchMatchDto>, String> {
    use std::sync::Arc;

    // Fast path: TextIndex for this book already built this session?
    let cached: Option<Arc<boko::kfx_to_epub::TextIndex>> = {
        let guard = state.reader_search_cache.lock().await;
        match &*guard {
            Some((id, idx)) if *id == book_id => Some(idx.clone()),
            _ => None,
        }
    };

    let index = match cached {
        Some(i) => i,
        None => {
            // Resolve KFX path under the db lock, then release it before the
            // CPU-bound parse — same shape as reader_open.
            let kfx_path = {
                let conn = state.db.lock().await;
                db::get_book(&conn, book_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("no book with id {book_id}"))?
                    .kfx_path
                    .ok_or_else(|| "this book has no KFX file yet".to_string())?
            };
            let built = tokio::task::spawn_blocking(move || {
                let bytes =
                    std::fs::read(&kfx_path).map_err(|e| format!("read {kfx_path}: {e}"))?;
                boko::kfx_to_epub::TextIndex::from_kfx(&bytes)
                    .map_err(|e| format!("TextIndex build: {e}"))
            })
            .await
            .map_err(|e| format!("search task join error: {e}"))??;
            let arc = Arc::new(built);
            // Store; replaces any other book's cached index (single-entry cache).
            // A concurrent search for a different book may race us here — harmless,
            // last writer wins and the loser just rebuilds next time.
            let mut guard = state.reader_search_cache.lock().await;
            *guard = Some((book_id, arc.clone()));
            arc
        }
    };

    // search() is HashMap-bounded but on a large book scans the whole corpus
    // (worst case: ~1M chars across thousands of eids); keep the async runtime
    // unblocked by hopping to the blocking pool.
    let matches = tokio::task::spawn_blocking(move || index.search(&query))
        .await
        .map_err(|e| format!("search task join error: {e}"))?;

    Ok(matches.into_iter().map(SearchMatchDto::from).collect())
}

// ---------------------------------------------------------------------------
// Native annotations (T0): create / edit / delete the reader's own annotations.
// Stored with `source='sidle'`. The anchor `(eid, offset)` comes from the
// webview's reverse resolution of a DOM selection (foliate-kfx `anchorFromRange`);
// the highlight `text` is the base-text slice the webview extracted (ruby-free,
// matching the offset semantics), so the stored text re-paints exactly.
// ---------------------------------------------------------------------------

/// Create a native annotation. Salts the **shared** content dedup hash with the
/// book's title key, so a passage highlighted both in Sidle and on a Kindle
/// collapses to one row. `insert_annotation` is `ON CONFLICT DO NOTHING`, so on a
/// hash collision (the passage already exists) we return the row already present
/// rather than erroring. Returns the stored row so the webview gets its real id.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn annotation_create(
    state: State<'_, AppState>,
    book_id: i64,
    kind: String,
    eid_start: Option<i64>,
    off_start: Option<i64>,
    eid_end: Option<i64>,
    off_end: Option<i64>,
    loc_start: Option<i64>,
    linear_pos: Option<i64>,
    text: String,
    note_body: Option<String>,
    color: Option<String>,
) -> Result<AnnotationDto, String> {
    let conn = state.db.lock().await;
    let title = db::get_book(&conn, book_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no book with id {book_id}"))?
        .title;
    let book_key = ingest::book_match_key(&title);
    let hash = ingest::annotation_dedup_hash(
        &book_key,
        &kind,
        eid_start,
        off_start,
        eid_end,
        off_end,
        loc_start,
        &text,
        note_body.as_deref().unwrap_or(""),
    );
    let now = db::now_iso();
    let row = db::NewAnnotation {
        dedup_hash: &hash,
        book_id: Some(book_id),
        kind: &kind,
        eid_start,
        off_start,
        eid_end,
        off_end,
        loc_start,
        loc_end: loc_start, // native annotations carry a single-point Location
        linear_pos,
        text: &text,
        note_body: note_body.as_deref(),
        color: color.as_deref(),
        clip_title: None,
        clip_author: None,
        added_at: Some(&now),
        added_raw: None,
        imported_at: &now,
        source: ingest::SOURCE_SIDLE,
    };
    db::insert_annotation(&conn, &row).map_err(|e| e.to_string())?;
    // Fresh insert or pre-existing duplicate — the canonical row is the one with
    // this hash.
    let stored = db::get_annotation_by_hash(&conn, &hash)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "annotation missing after insert".to_string())?;
    Ok(AnnotationDto::from(stored))
}

/// Edit a native annotation's `kind` / `note_body` / `color` (e.g. promote a
/// highlight to a note, recolor, retype). The content hash folds in kind + note
/// body, so it's recomputed (salted with the same book key) and moved with the
/// edit. Returns the refreshed row.
#[tauri::command]
pub async fn annotation_update(
    state: State<'_, AppState>,
    id: i64,
    kind: String,
    note_body: Option<String>,
    color: Option<String>,
) -> Result<AnnotationDto, String> {
    let conn = state.db.lock().await;
    let row = db::get_annotation(&conn, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no annotation with id {id}"))?;
    let book_key = match row.book_id {
        Some(bid) => db::get_book(&conn, bid)
            .map_err(|e| e.to_string())?
            .map(|b| ingest::book_match_key(&b.title))
            .unwrap_or_default(),
        None => String::new(),
    };
    let hash = ingest::annotation_dedup_hash(
        &book_key,
        &kind,
        row.eid_start,
        row.off_start,
        row.eid_end,
        row.off_end,
        row.loc_start,
        &row.text,
        note_body.as_deref().unwrap_or(""),
    );
    db::update_annotation(
        &conn,
        id,
        &kind,
        note_body.as_deref(),
        color.as_deref(),
        &hash,
    )
    .map_err(|e| e.to_string())?;
    let updated = db::get_annotation(&conn, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "annotation missing after update".to_string())?;
    Ok(AnnotationDto::from(updated))
}

/// Delete a native annotation by id.
#[tauri::command]
pub async fn annotation_delete(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock().await;
    db::delete_annotation(&conn, id).map_err(|e| e.to_string())?;
    Ok(())
}
