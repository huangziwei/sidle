//! Tauri commands for Kindle device sync.

use std::path::PathBuf;

use rusqlite::OptionalExtension;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::device::dedrm::{self, DedrmRow, PullResult};
use crate::device::detect::DeviceInfo;
use crate::device::push::{self, DeleteResult, PushResult};
use crate::device::{manifest, manifest::Manifest};
use crate::library::db;
use crate::state::AppState;

#[tauri::command]
pub async fn device_status(state: State<'_, AppState>) -> Result<Option<DeviceInfo>, String> {
    Ok(state.device.lock().await.clone())
}

#[derive(Debug, Serialize)]
pub struct DeviceBookRow {
    pub sha256: String,
    pub title: String,
    pub author: String,
    pub filename: String,
    pub sent_at: String,
    /// Local book id if we still have the row; None if removed locally.
    pub book_id: Option<i64>,
    /// True if the on-device file is still present (didn't get deleted on-device).
    pub file_present: bool,
}

#[tauri::command]
pub async fn device_list_ours(state: State<'_, AppState>) -> Result<Vec<DeviceBookRow>, String> {
    let Some(device) = state.device.lock().await.clone() else {
        return Ok(Vec::new());
    };
    let db_handle = state.db.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DeviceBookRow>> {
        let manifest = manifest::load(&device.mount_path())?;
        let docs = device.documents_dir();
        let conn = db_handle.blocking_lock();
        let mut out = Vec::with_capacity(manifest.sent.len());
        for (sha, entry) in &manifest.sent {
            let book_id: Option<i64> = conn
                .query_row(
                    "SELECT id FROM books WHERE sha256 = ?1",
                    rusqlite::params![sha],
                    |r| r.get(0),
                )
                .optional()?;
            let file_present = docs.join(&entry.filename).exists();
            out.push(DeviceBookRow {
                sha256: sha.clone(),
                title: entry.title.clone(),
                author: entry.author.clone(),
                filename: entry.filename.clone(),
                sent_at: entry.sent_at.clone(),
                book_id,
                file_present,
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn device_delete(
    app: AppHandle,
    state: State<'_, AppState>,
    sha256s: Vec<String>,
) -> Result<Vec<DeleteResult>, String> {
    let Some(device) = state.device.lock().await.clone() else {
        return Err("no Kindle connected".to_string());
    };
    let db_handle = state.db.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DeleteResult>> {
        let conn = db_handle.blocking_lock();
        let mut manifest: Manifest = manifest::load(&device.mount_path())?;
        let mut out = Vec::with_capacity(sha256s.len());
        for sha in sha256s {
            let result = match push::delete_one(&conn, &device, &mut manifest, &sha) {
                Ok(r) => r,
                Err(e) => DeleteResult::Failed {
                    sha256: sha.clone(),
                    error: format!("{e:#}"),
                },
            };
            let _ = app.emit("device:delete-progress", &result);
            out.push(result);
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn device_scan_dedrm(state: State<'_, AppState>) -> Result<Vec<DedrmRow>, String> {
    let Some(device) = state.device.lock().await.clone() else {
        return Ok(Vec::new());
    };
    let db_handle = state.db.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DedrmRow>> {
        let conn = db_handle.blocking_lock();
        dedrm::scan(&conn, &device)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn device_pull(
    app: AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<PullResult>, String> {
    let Some(device) = state.device.lock().await.clone() else {
        return Err("no Kindle connected".to_string());
    };
    let db_handle = state.db.clone();
    let paths_handle = state.paths.clone();

    // Each freshly-imported KFX/KFX-zip now needs a background `kfx_to_epub`
    // job — import_file no longer runs `convert_to_epub` inline. Pull_one
    // returns the book_id whenever an enqueue is required; we collect those
    // and submit them after the blocking import loop finishes.
    let outcomes = tokio::task::spawn_blocking(move || -> Vec<(PullResult, Option<i64>)> {
        let conn = db_handle.blocking_lock();
        let mut out = Vec::with_capacity(paths.len());
        for raw in paths {
            let path = PathBuf::from(&raw);
            let pair = dedrm::pull_one(&conn, &paths_handle, &device, &path);
            let _ = app.emit("device:pull-progress", &pair.0);
            out.push(pair);
        }
        out
    })
    .await
    .map_err(|e| e.to_string())?;

    let mut results = Vec::with_capacity(outcomes.len());
    for (result, enqueue) in outcomes {
        if let Some(book_id) = enqueue {
            let _ = state.queue.enqueue(book_id).await;
        }
        results.push(result);
    }
    Ok(results)
}

#[tauri::command]
pub async fn device_send(
    app: AppHandle,
    state: State<'_, AppState>,
    book_ids: Vec<i64>,
) -> Result<Vec<PushResult>, String> {
    let Some(device) = state.device.lock().await.clone() else {
        return Err("no Kindle connected".to_string());
    };
    let db_handle = state.db.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<PushResult>> {
        let conn = db_handle.blocking_lock();
        let mut manifest: Manifest = manifest::load(&device.mount_path())?;
        let mut out = Vec::with_capacity(book_ids.len());
        for book_id in book_ids {
            let result = match db::get_book(&conn, book_id)? {
                Some(book) => match push::push_one(&conn, &device, &mut manifest, &book) {
                    Ok(r) => r,
                    Err(e) => PushResult::Failed {
                        book_id,
                        error: format!("{e:#}"),
                    },
                },
                None => PushResult::Failed {
                    book_id,
                    error: "book not found".into(),
                },
            };
            // Best-effort: notify UI before moving to the next book so a long
            // batch shows live progress.
            let _ = app.emit("device:send-progress", &result);
            out.push(result);
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}
