//! Tauri commands for Scribe handwritten notebooks (the Notes tab).
//!
//! Mirrors `commands::library` but for the `notebooks` entity: list, per-page
//! SVG (read from the import-time cache — no SQLite re-parse), cover thumbnail,
//! rename, remove, and a manual `.notebooks/` folder import (the Phase 1 way to
//! populate the tab and the Phase 2 fallback when MTP can't expose notebooks).

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use tokio::sync::oneshot;

use crate::library::LibraryPaths;
use crate::library::db::{self, NotebookRow};
use crate::library::notebook;
use crate::state::AppState;

#[tauri::command]
pub async fn notebook_list(state: State<'_, AppState>) -> Result<Vec<NotebookRow>, String> {
    let conn = state.db.lock().await;
    db::list_notebooks(&conn).map_err(|e| e.to_string())
}

/// One page's cached SVG markup, returned as a string the webview injects
/// directly. Reads the derived asset written at import — no SQLite re-parse.
#[tauri::command]
pub async fn notebook_page_svg(
    state: State<'_, AppState>,
    notebook_id: i64,
    page: usize,
) -> Result<String, String> {
    let uuid = {
        let conn = state.db.lock().await;
        db::get_notebook(&conn, notebook_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no notebook with id {notebook_id}"))?
            .uuid
    };
    let path = state.paths.notebook_page_svg(&uuid, page);
    std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// The notebook's cover thumbnail path (the device PNG) if present, for the
/// grid tile via the asset protocol. `None` → the frontend falls back to the
/// first page's SVG as the tile.
#[tauri::command]
pub async fn notebook_thumbnail(
    state: State<'_, AppState>,
    notebook_id: i64,
) -> Result<Option<String>, String> {
    let uuid = {
        let conn = state.db.lock().await;
        match db::get_notebook(&conn, notebook_id).map_err(|e| e.to_string())? {
            Some(n) => n.uuid,
            None => return Ok(None),
        }
    };
    let cover = state.paths.notebook_cover(&uuid);
    Ok(cover.exists().then(|| cover.to_string_lossy().into_owned()))
}

/// Rename a notebook (titles are cloud-only, so the name is user-editable;
/// default "Notebook"). Returns the refreshed row.
#[tauri::command]
pub async fn notebook_rename(
    state: State<'_, AppState>,
    notebook_id: i64,
    title: String,
) -> Result<NotebookRow, String> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("title cannot be empty".into());
    }
    let conn = state.db.lock().await;
    db::rename_notebook(&conn, notebook_id, &title).map_err(|e| e.to_string())?;
    db::get_notebook(&conn, notebook_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no notebook with id {notebook_id}"))
}

/// Remove a notebook from the library (DB row + `notebooks/<uuid>/` files).
#[tauri::command]
pub async fn notebook_remove(state: State<'_, AppState>, notebook_id: i64) -> Result<(), String> {
    let uuid = {
        let conn = state.db.lock().await;
        db::remove_notebook(&conn, notebook_id).map_err(|e| e.to_string())?
    };
    if let Some(uuid) = uuid {
        state.paths.remove_notebook(&uuid).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Summary of a manual `.notebooks/` folder import.
#[derive(Debug, Serialize)]
pub struct NotebookImportSummary {
    pub imported: usize,
    pub unchanged: usize,
    pub failed: Vec<String>,
}

/// Manual import: pick a folder, scan it for Scribe notebooks, ingest each.
/// Accepts either a `.notebooks/` parent (many `<uuid>/nbk`) or a single
/// notebook dir (one `nbk`). Returns `None` if the picker was cancelled.
#[tauri::command]
pub async fn notebook_import_folder(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<NotebookImportSummary>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog().file().pick_folder(move |p| {
        let _ = tx.send(p);
    });
    let Some(folder) = rx.await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let folder = PathBuf::from(folder.to_string());

    let db = state.db.clone();
    let paths = state.paths.clone();
    let summary = tokio::task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        import_dir(&conn, &paths, &folder)
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(Some(summary))
}

/// Scan `folder` for notebook dirs and import each. `folder` may itself be one
/// notebook dir (holds `nbk` directly) or a parent of many.
fn import_dir(conn: &rusqlite::Connection, paths: &LibraryPaths, folder: &Path) -> NotebookImportSummary {
    let mut summary = NotebookImportSummary {
        imported: 0,
        unchanged: 0,
        failed: Vec::new(),
    };
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    if folder.join("nbk").is_file() {
        let uuid = folder
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("notebook")
            .to_string();
        candidates.push((uuid, folder.to_path_buf()));
    } else if let Ok(entries) = std::fs::read_dir(folder) {
        for e in entries.flatten() {
            let dir = e.path();
            if dir.join("nbk").is_file()
                && let Some(uuid) = dir.file_name().and_then(|s| s.to_str())
            {
                candidates.push((uuid.to_string(), dir));
            }
        }
    }

    for (uuid, dir) in candidates {
        // Phase 1: standalone (dashed-UUID) notebooks only. Skip the
        // `!!PDOC!!`/`!!EBOK!!notebook` annotation dirs (Phase 4).
        if uuid.contains("!!") {
            continue;
        }
        let nbk = dir.join("nbk");
        let cover = find_cover(&dir, &uuid);
        match notebook::import_notebook(conn, paths, &uuid, &nbk, cover.as_deref()) {
            Ok(notebook::NotebookOutcome::Imported(_)) => summary.imported += 1,
            Ok(notebook::NotebookOutcome::Unchanged(_)) => summary.unchanged += 1,
            Err(e) => summary.failed.push(format!("{uuid}: {e:#}")),
        }
    }
    summary
}

/// Locate the device cover thumbnail for a notebook. The device keeps these in
/// a sibling `thumbnails/<uuid>.png` (relative to `.notebooks/`); also accept
/// an in-dir `thumbnail.png`. Best-effort — `None` is fine (the viewer falls
/// back to page 0).
fn find_cover(dir: &Path, uuid: &str) -> Option<PathBuf> {
    let mut candidates = vec![dir.join("thumbnail.png")];
    if let Some(parent) = dir.parent() {
        candidates.push(parent.join("thumbnails").join(format!("{uuid}.png")));
        if let Some(grand) = parent.parent() {
            candidates.push(grand.join("thumbnails").join(format!("{uuid}.png")));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}
