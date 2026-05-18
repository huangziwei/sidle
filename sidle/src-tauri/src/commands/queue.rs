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

#[tauri::command]
pub async fn conversion_retry(state: State<'_, AppState>, book_id: i64) -> Result<(), String> {
    {
        let conn = state.db.lock().await;
        db::upsert_job(&conn, book_id, "pending", None).map_err(|e| e.to_string())?;
    }
    state.queue.enqueue(book_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn conversion_set_workers(
    state: State<'_, AppState>,
    n: usize,
) -> Result<usize, String> {
    state.queue.set_workers(n).await.map_err(|e| e.to_string())?;
    Ok(state.queue.current_workers().await)
}
