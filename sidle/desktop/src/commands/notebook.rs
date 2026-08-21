//! Tauri commands for Scribe handwritten notebooks (the Notes tab).
//!
//! Mirrors `commands::library` but for the `notebooks` entity: list, per-page
//! SVG (read from the import-time cache — no SQLite re-parse), cover thumbnail,
//! rename, remove, and a manual `.notebooks/` folder import — the fallback for
//! a device whose MTP session will not expose notebooks.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::oneshot;

use sidle_core::library::export::Summary as ExportSummary;
use sidle_core::library::notebook::{self, ImportSummary};

use crate::device_monitor::{ensure_transport, evict_transport};
use crate::library::db::{self, NotebookRow};
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
        state
            .paths
            .remove_notebook(&uuid)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Manual import: pick a folder, scan it for Scribe notebooks, ingest each.
/// Accepts either a `.notebooks/` parent (many `<uuid>/nbk`) or a single
/// notebook dir (one `nbk`). Returns `None` if the picker was cancelled.
#[tauri::command]
pub async fn notebook_import_folder(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<ImportSummary>, String> {
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
        notebook::import_folder(&conn, &paths, &folder)
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(Some(summary))
}

/// Progress for a notebook device-import: emitted as `notebook:import-progress`
/// after each candidate so the frontend can show "Importing N/M…". The pull +
/// decode of each notebook over USB is slow, so this is the difference between
/// the button looking dead and looking busy.
#[derive(Clone, Serialize)]
struct NotebookImportProgress {
    done: usize,
    total: usize,
}

/// Import notebooks straight off the connected Kindle — the toolbar's Import
/// button. Pulls `.notebooks/<uuid>/nbk` over the device transport (MTP for the
/// Scribe), capturing each notebook's on-device Date Modified as `updated_at`,
/// and emits `notebook:import-progress` as it goes. Errors with "no Kindle
/// connected" when nothing is plugged in; a device that doesn't expose
/// `.notebooks/` over MTP simply yields 0 imported.
#[tauri::command]
pub async fn notebook_import_device(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ImportSummary, String> {
    let device = state
        .device
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no Kindle connected".to_string())?;
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| e.to_string())?;
    let db = state.db.clone();
    let paths = state.paths.clone();
    let cell = state.transport.clone();
    let serial = device.serial.clone();
    eprintln!("[sidle/nbk-import] {serial}: scanning .notebooks/ on device…");
    let emitter = app.clone();
    let result = tokio::task::spawn_blocking(move || {
        let on_progress = |done: usize, total: usize| {
            let _ = emitter.emit(
                "notebook:import-progress",
                NotebookImportProgress { done, total },
            );
        };
        sidle_core::library::device::notebooks::import_device_notebooks(
            transport.as_ref(),
            &paths,
            &crate::state::Borrowed(&db),
            &on_progress,
        )
    })
    .await;

    // On a wire error the cached transport may be talking to a wedged endpoint —
    // drop it so the next attempt reopens fresh (mirrors the annotation sync).
    if matches!(result, Err(_) | Ok(Err(_))) {
        evict_transport(&cell).await;
    }

    let summary = result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    eprintln!(
        "[sidle/nbk-import] {serial}: {} imported, {} unchanged, {} failed",
        summary.imported,
        summary.unchanged,
        summary.failed.len()
    );
    Ok(summary)
}

/// Export each notebook in `notebook_ids` to a multi-page PDF in `dest_dir` —
/// one `<title>.pdf` per notebook (notebooks have no author, so the folder is
/// flat). Filenames come from the (sanitized) title; a collision gets a ` (n)`
/// suffix. A notebook with no pages, or whose render fails, is skipped and
/// counted; the export never aborts on a single failure. Reuses the library
/// export's [`ExportSummary`] shape.
#[tauri::command]
pub async fn notebook_export_pdf(
    state: State<'_, AppState>,
    notebook_ids: Vec<i64>,
    dest_dir: String,
) -> Result<ExportSummary, String> {
    let dest_root = Path::new(&dest_dir).to_path_buf();
    if !dest_root.is_dir() {
        return Err(format!("{} is not a folder", dest_root.display()));
    }

    // Resolve the rows under the lock; render + write outside it (resvg
    // rasterization is CPU-heavy and synchronous — keep it off the async runtime).
    let rows = {
        let conn = state.db.lock().await;
        let mut v = Vec::with_capacity(notebook_ids.len());
        for &id in &notebook_ids {
            if let Some(n) = db::get_notebook(&conn, id).map_err(|e| e.to_string())? {
                v.push(n);
            }
        }
        v
    };

    let paths = state.paths.clone();
    tokio::task::spawn_blocking(move || {
        let mut exported = 0usize;
        let mut skipped = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for n in rows {
            let title = crate::library::paths::sanitize_segment(&n.title);
            let title = if title.is_empty() { "Notebook" } else { &title };
            let pdf = match notebook::export_notebook_pdf(&paths, &n.uuid, n.page_count as usize) {
                Ok(bytes) => bytes,
                Err(e) => {
                    skipped += 1;
                    if errors.len() < 8 {
                        errors.push(format!("{title}: {e:#}"));
                    }
                    continue;
                }
            };
            let target = crate::library::paths::dedup_path(dest_root.join(format!("{title}.pdf")));
            match std::fs::write(&target, &pdf) {
                Ok(()) => exported += 1,
                Err(e) => {
                    skipped += 1;
                    if errors.len() < 8 {
                        errors.push(format!("{title}: {e}"));
                    }
                }
            }
        }
        ExportSummary {
            exported,
            skipped,
            dest: dest_dir,
            errors,
        }
    })
    .await
    .map_err(|e| e.to_string())
}
