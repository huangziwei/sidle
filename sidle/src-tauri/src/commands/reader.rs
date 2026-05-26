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
