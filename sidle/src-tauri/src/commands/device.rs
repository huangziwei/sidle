//! Tauri commands for Kindle device sync.

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::device::DeviceInfo;
use crate::device::dedrm::{self, PullResult};
use crate::device::push::{self, DeleteResult, PushResult};
use crate::library::db;
use crate::state::AppState;

/// Same sha8 width as `push::SHA_INFIX_LEN`. Kept private here so
/// `device_list_ours` can parse it back out of filenames without
/// re-importing the constant.
const SHA_INFIX_LEN: usize = 8;

#[tauri::command]
pub async fn device_status(state: State<'_, AppState>) -> Result<Option<DeviceInfo>, String> {
    Ok(state.device.lock().await.clone())
}

/// One on-device file under `documents/Sidle/`, plus its link back to the
/// local library if we can find one.
///
/// Tagged enum so the frontend can switch on `kind`:
///  - `sent`: file's sha8 matched a `books.sha256` prefix → we know title/
///    author; the row shows up in the popover the same way as before.
///  - `orphan`: file's sha8 didn't match anything in the local library
///    (book was removed locally, or this Kindle was last paired with a
///    different Mac). Frontend offers a one-click "Import to library".
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceRow {
    Sent {
        book_id: i64,
        sha256: String,
        title: String,
        author: String,
        filename: String,
    },
    Orphan {
        sha8: String,
        filename: String,
    },
}

/// Scan `documents/Sidle/` on the connected device. Each `*.<sha8>.kfx`
/// becomes one [`DeviceRow`]; the directory itself IS the source of truth
/// (no on-device manifest, no separate per-device DB table), so the popup
/// always reflects exactly what's on the Kindle right now — including
/// files the user deleted via the device UI (which simply don't appear).
#[tauri::command]
pub async fn device_list_ours(state: State<'_, AppState>) -> Result<Vec<DeviceRow>, String> {
    let Some(device) = state.device.lock().await.clone() else {
        return Ok(Vec::new());
    };
    let db_handle = state.db.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DeviceRow>> {
        let transport = device.open_transport()?;
        let docs = crate::device::TPath::parse("documents/Sidle");
        let entries = transport.list(&docs).unwrap_or_default();
        let conn = db_handle.blocking_lock();

        let mut out = Vec::new();
        for entry in entries {
            if entry.is_dir {
                continue;
            }
            if is_macos_metadata(&entry.name) {
                // `._foo.<sha>.kfx` AppleDouble companions get the same
                // sha8 suffix as the real file and would otherwise be
                // emitted as a duplicate row pointing at the same book.
                // Same logic for `.DS_Store` etc.
                continue;
            }
            let Some(sha8) = parse_sha_infix(&entry.name) else {
                // Not one of ours (no sha8 infix). Shouldn't happen in
                // practice — anything under Sidle/ was put there by us —
                // but skip rather than treat as an orphan.
                continue;
            };
            match db::find_by_kfx_sha_prefix(&conn, &sha8).map_err(anyhow::Error::from)? {
                Some(book) => out.push(DeviceRow::Sent {
                    book_id: book.id,
                    sha256: book.sha256,
                    title: book.title,
                    author: book.author,
                    filename: entry.name,
                }),
                None => out.push(DeviceRow::Orphan {
                    sha8,
                    filename: entry.name,
                }),
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// Parse the `<sha8>` out of a `<basename>.<sha8>.kfx` filename.
/// Returns None for files that don't follow our scheme.
fn parse_sha_infix(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".kfx")?;
    let (_, sha) = stem.rsplit_once('.')?;
    if sha.len() == SHA_INFIX_LEN && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(sha.to_string())
    } else {
        None
    }
}

/// Skip files macOS scatters into FAT/exFAT mounts as a side effect of
/// xattr/Finder-metadata handling: `._<filename>` AppleDouble companions
/// (the real bug — they'd otherwise be parsed as a second copy of the
/// same book), `.DS_Store`, `.Spotlight-V100`, etc.
fn is_macos_metadata(name: &str) -> bool {
    name.starts_with('.')
}

#[tauri::command]
pub async fn device_delete(
    app: AppHandle,
    state: State<'_, AppState>,
    filenames: Vec<String>,
) -> Result<Vec<DeleteResult>, String> {
    let Some(device) = state.device.lock().await.clone() else {
        return Err("no Kindle connected".to_string());
    };
    let db_handle = state.db.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DeleteResult>> {
        let transport = device.open_transport()?;
        let conn = db_handle.blocking_lock();
        let mut out = Vec::with_capacity(filenames.len());
        for name in filenames {
            // Pull the book's ASIN from the local row so delete_one can
            // also wipe Kindle's `<title>_<ASIN>.sdr/` (catalog-style
            // sidecar). Best-effort: if the row is gone or the lookup
            // errors out, we still delete the file + filename-style .sdr.
            let asin = sha_from_filename(&name)
                .and_then(|sha| db::find_by_kfx_sha_prefix(&conn, &sha).ok().flatten())
                .and_then(|b| b.asin);
            let result =
                match push::delete_one(&device, transport.as_ref(), &name, asin.as_deref()) {
                    Ok(r) => r,
                    Err(e) => DeleteResult::Failed {
                        filename: name.clone(),
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

/// Pull the `.<sha8>.kfx` infix out of a filename. Returns `None` if the
/// filename doesn't have our shape; the catalog-sdr lookup just gets
/// skipped in that case (and `delete_one` will refuse the delete with
/// `NotOurs` anyway).
fn sha_from_filename(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".kfx")?;
    let (_, sha) = stem.rsplit_once('.')?;
    if sha.len() == SHA_INFIX_LEN && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(sha.to_string())
    } else {
        None
    }
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
        let transport = device.open_transport()?;
        let conn = db_handle.blocking_lock();
        let mut out = Vec::with_capacity(book_ids.len());
        for book_id in book_ids {
            let result = match db::get_book(&conn, book_id)? {
                Some(book) => match push::push_one(&device, transport.as_ref(), &conn, &book) {
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

/// Pull an orphan `.kfx` off the device and into the local library.
///
/// The orphan flow exists for the "I removed it from the library but it's
/// still on the Kindle" / "I sent it from another Mac" cases. We read the
/// bytes via [`Transport`] (free for mass-storage, an MTP `GetObject`
/// otherwise), stage to a temp file, and run the same import pipeline as
/// a drag-drop — which also enqueues the KFX→EPUB background job.
#[tauri::command]
pub async fn device_import_orphan(
    app: AppHandle,
    state: State<'_, AppState>,
    filename: String,
) -> Result<PullResult, String> {
    let Some(device) = state.device.lock().await.clone() else {
        return Err("no Kindle connected".to_string());
    };
    let db_handle = state.db.clone();
    let paths = state.paths.clone();
    let queue = state.queue.clone();

    let (result, enqueue) = tokio::task::spawn_blocking(
        move || -> anyhow::Result<(PullResult, Option<i64>)> {
            let transport = device.open_transport()?;
            let on_device = crate::device::TPath::parse("documents/Sidle").join(&filename);
            let bytes = transport.read(&on_device)?;

            // Stage to a temp file with the original extension preserved so
            // the import pipeline's extension-based dispatch routes
            // correctly. Drop-on-scope cleans up after `pull_one`.
            let suffix = std::path::Path::new(&filename)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_else(|| ".kfx".to_string());
            let tmp = tempfile::Builder::new()
                .prefix("sidle-orphan-")
                .suffix(&suffix)
                .tempfile()?;
            std::fs::write(tmp.path(), &bytes)?;

            let conn = db_handle.blocking_lock();
            let pair = dedrm::pull_one(&conn, &paths, &device, tmp.path());
            Ok(pair)
        },
    )
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    if let Some(book_id) = enqueue {
        let _ = queue.enqueue(book_id).await;
    }
    let _ = app.emit("device:pull-progress", &result);
    Ok(result)
}
