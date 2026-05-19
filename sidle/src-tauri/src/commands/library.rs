//! Tauri commands for library operations.

use std::path::PathBuf;

use serde::Serialize;
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::oneshot;

use crate::library::db::{self, BookRow};
use crate::library::import::{self, ImportOutcome};
use crate::state::AppState;

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportResult {
    /// `needs_enqueue` is true when the row was inserted with a pending job —
    /// either an EPUB-import awaiting EPUB→KFX, or a KFX-import awaiting
    /// KFX→EPUB. False on an idempotent re-import where the other side was
    /// already on disk.
    Imported { book: BookRow, needs_enqueue: bool },
    Duplicate { book: BookRow },
    Failed { path: String, error: String },
}

#[tauri::command]
pub async fn library_list(state: State<'_, AppState>) -> Result<Vec<BookRow>, String> {
    let conn = state.db.lock().await;
    db::list_books(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn library_import(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<ImportResult>, String> {
    let mut out = Vec::with_capacity(paths.len());

    for raw in paths {
        let path = PathBuf::from(&raw);
        let db_handle = state.db.clone();
        let paths_handle = state.paths.clone();
        let raw_for_err = raw.clone();

        let result = tokio::task::spawn_blocking(move || {
            let conn = db_handle.blocking_lock();
            import::import_file(&conn, &paths_handle, &path)
        })
        .await
        .map_err(|e| e.to_string())?;

        match result {
            Ok(ImportOutcome::Imported { book, needs_enqueue }) => {
                let book_id = book.id;
                if needs_enqueue {
                    let _ = state.queue.enqueue(book_id).await;
                }
                out.push(ImportResult::Imported { book, needs_enqueue });
            }
            Ok(ImportOutcome::Duplicate(book)) => {
                out.push(ImportResult::Duplicate { book });
            }
            Err(e) => out.push(ImportResult::Failed {
                path: raw_for_err,
                error: format!("{e:#}"),
            }),
        }
    }

    Ok(out)
}

#[tauri::command]
pub async fn library_remove(state: State<'_, AppState>, book_id: i64) -> Result<(), String> {
    let sha = {
        let conn = state.db.lock().await;
        db::remove_book(&conn, book_id).map_err(|e| e.to_string())?
    };
    if let Some(sha) = sha {
        state.paths.remove_sha(&sha);
    }
    Ok(())
}

#[tauri::command]
pub async fn library_open_in_finder(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<(), String> {
    let path = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .and_then(|b| {
                // Prefer the EPUB if it exists; fall back to KFX so books
                // imported as `.kfx` with a still-converting EPUB can still
                // be revealed by their `.kfx` file.
                b.epub_path.or(b.kfx_path)
            })
    };
    let Some(path) = path else {
        return Err("no file on disk for this book yet".into());
    };
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn library_cover_path(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Option<String>, String> {
    let conn = state.db.lock().await;
    Ok(db::get_book(&conn, book_id)
        .map_err(|e| e.to_string())?
        .and_then(|b| b.cover_path))
}

/// Open the system file dialog and return selected ebook paths.
///
/// Accepts EPUB, KFX, and KFX-zip (the multi-container bundle Kindle DeDRM
/// produces) — the import pipeline dispatches on extension. Exposed from Rust
/// because vanilla-JS (no bundler) can't import the dialog plugin's JS module.
#[tauri::command]
pub async fn library_pick_files(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Ebooks", &["epub", "kfx", "kfx-zip"])
        .pick_files(move |paths| {
            let _ = tx.send(paths);
        });
    let result = rx.await.map_err(|e| e.to_string())?;
    Ok(result
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string())
        .collect())
}
