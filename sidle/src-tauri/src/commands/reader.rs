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

use crate::library::db;
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
