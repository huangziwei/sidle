//! Tauri commands for the conversion queue.

use serde::Serialize;
use tauri::State;

use crate::library::db;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct JobRow {
    pub book_id: i64,
    pub title: String,
    pub status: String,
    pub error: Option<String>,
}

/// Snapshot of all per-book job states. The frontend uses events for live
/// updates; this is for first-paint and "Sync now" tooling.
#[tauri::command]
pub async fn conversion_status(state: State<'_, AppState>) -> Result<Vec<JobRow>, String> {
    let conn = state.db.lock().await;
    let books = db::list_books(&conn).map_err(|e| e.to_string())?;
    Ok(books
        .into_iter()
        .map(|b| JobRow {
            book_id: b.id,
            title: b.title,
            status: b.status,
            error: b.error,
        })
        .collect())
}

/// `color` selects the EPUB→KFX interior image encoding for a forced re-convert
/// of a `done` book (the nested "Re-convert · full color" / "· grayscale"
/// actions): `true` ⇒ `24bppRGB` JXR, `false` ⇒ `8bppGray` (default). Ignored
/// when retrying a failed/pending first import (that always uses grayscale).
#[tauri::command]
pub async fn conversion_retry(
    state: State<'_, AppState>,
    book_id: i64,
    color: bool,
) -> Result<(), String> {
    // A re-convert of an already-`done` book (the "Force re-convert" action) is
    // source→target ONLY: skip the import-time cover enrichment so the source
    // KFX (and its `kfx_sha256`) is untouched — a re-stamp would change the
    // on-device filename infix and break annotation-sync matching for a pushed
    // book. Retrying a failed/pending conversion still enriches: it's completing
    // the first import (the "Retry" action on an errored book).
    let was_done = {
        let conn = state.db.lock().await;
        let prior = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .map(|b| b.status);
        // Reset status to `pending` and clear any prior error. `kind` is
        // preserved by `set_job_status` — the worker still dispatches in the
        // right direction on the retry attempt.
        db::set_job_status(&conn, book_id, "pending", None).map_err(|e| e.to_string())?;
        prior.as_deref() == Some("done")
    };
    let queued = if was_done {
        state.queue.enqueue_reconvert(book_id, color).await
    } else {
        state.queue.enqueue(book_id).await
    };
    queued.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn conversion_set_workers(state: State<'_, AppState>, n: usize) -> Result<usize, String> {
    state
        .queue
        .set_workers(n)
        .await
        .map_err(|e| e.to_string())?;
    Ok(state.queue.current_workers().await)
}
