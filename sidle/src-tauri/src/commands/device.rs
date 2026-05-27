//! Tauri commands for Kindle device sync.

use serde::Serialize;
use sidle_core::library::paths::parse_sha_infix;
use tauri::{AppHandle, Emitter, State};

use crate::device::DeviceInfo;
use crate::device::dedrm::{self, PullResult};
use crate::device::kual::{
    self, KualInstallReport, KualOverall, KualStatus, ServerConfRender,
};
use crate::device::push::{self, DeleteResult, PushResult};
use crate::library::{db, ingest};
use crate::state::AppState;

#[tauri::command]
pub async fn device_status(state: State<'_, AppState>) -> Result<Option<DeviceInfo>, String> {
    Ok(state.device.lock().await.clone())
}

/// Unmount + spin down a mass-storage Kindle so the user can unplug
/// safely without macOS scolding them. Shells out to `diskutil eject`
/// — the same command Finder's eject button runs.
///
/// MTP devices don't need (and don't support) eject in the
/// mass-storage sense — they just need to have their USB session
/// closed, which happens automatically on unplug. The frontend hides
/// the button when transport is MTP, so a caller hitting this with an
/// MTP device is treated as a programming error.
#[tauri::command]
pub async fn device_eject(state: State<'_, AppState>) -> Result<(), String> {
    let device = state.device.lock().await.clone();
    let mount = device
        .as_ref()
        .and_then(|d| d.mass_storage_mount())
        .ok_or_else(|| "no mass-storage Kindle connected".to_string())?;

    let mount_arg = mount
        .to_str()
        .ok_or_else(|| format!("mount path is not utf-8: {}", mount.display()))?
        .to_string();

    tokio::task::spawn_blocking(move || {
        std::process::Command::new("diskutil")
            .arg("eject")
            .arg(&mount_arg)
            .output()
            .map_err(|e| format!("diskutil eject: {e}"))
            .and_then(|out| {
                if out.status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "diskutil eject failed ({}): {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    ))
                }
            })
    })
    .await
    .map_err(|e| e.to_string())?
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

/// Import highlights / notes / bookmarks from the connected Kindle — either
/// transport. Mass-storage reads `documents/Sidle/` off the volume; MTP (Scribe)
/// pulls the `.yjr` over USB. Matches each annotated `.sdr` to a library book by
/// its `kfx_sha256` infix, extracts the highlighted text from the book's own
/// (readable) KFX, archives `My Clippings.txt` orphans, then relinks. The dedup
/// hash makes it idempotent — safe to re-run on every connect.
///
/// MTP import only yields records if the device exposes its `.sdr/.yjr` sidecars
/// over MTP; if it doesn't, the report is simply 0 books.
#[tauri::command]
pub async fn annotations_import_from_device(
    state: State<'_, AppState>,
) -> Result<ingest::DeviceImportReport, String> {
    let device = state
        .device
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no Kindle connected".to_string())?;
    let db_handle = state.db.clone();
    tokio::task::spawn_blocking(move || {
        crate::device::annotations::import_device_annotations(&device, &db_handle)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
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
            let asin = parse_sha_infix(&name)
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

// ----------------------------------------------------------------------------
// KUAL deploy (Install / Update KUAL button)
// ----------------------------------------------------------------------------

/// Resolve the live `ServerConfRender` from app state. Same shape used
/// by both `kual_status` and `kual_install` — keeping it in one place
/// guarantees the staleness check and the actual install agree on
/// what `server.conf` *should* contain. `serial` is the connected device's
/// USB iSerial, threaded in from the same `DeviceInfo` snapshot the mount came
/// from (the picker pushes it back as `device_serial`).
async fn render_conf(state: &AppState, serial: String) -> Option<ServerConfRender> {
    let server_status = state.server.status(&state.paths).await;
    let host = kual::detect_lan_ipv4()?.to_string();
    let token = server_status.token?;
    let port = server_status.port.unwrap_or(server_status.default_port);
    Some(ServerConfRender { host, port, serial, token })
}

#[tauri::command]
pub async fn kual_status(state: State<'_, AppState>) -> Result<KualStatus, String> {
    let device = state.device.lock().await.clone();
    let mount = match device.as_ref().and_then(|d| d.mass_storage_mount()) {
        Some(m) => m,
        // No mass-storage Kindle connected (or MTP-only Scribe). The
        // section is hidden by the UI when overall == DeviceDisconnected.
        None => {
            return Ok(KualStatus {
                overall: KualOverall::DeviceDisconnected,
                files: Vec::new(),
                binary_mtime_ms: None,
                native_source_mtime_ms: None,
            });
        }
    };

    // If we can't render a conf (no server token, no LAN IP), fall back
    // to a placeholder so the binary/bundle slots still get checked.
    // The status will read "server.conf stale" but at least the user
    // sees which other files need pushing.
    let serial = device.as_ref().map(|d| d.serial.clone()).unwrap_or_default();
    let conf = render_conf(&state, serial).await.unwrap_or(ServerConfRender {
        host: String::new(),
        port: 0,
        serial: String::new(),
        token: String::new(),
    });

    let source = state.kual_source.clone();
    tokio::task::spawn_blocking(move || kual::compute_status(&source, &conf, &mount))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn kual_install(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<KualInstallReport, String> {
    let device = state.device.lock().await.clone();
    let mount = device
        .as_ref()
        .and_then(|d| d.mass_storage_mount())
        .ok_or_else(|| "no mass-storage Kindle connected".to_string())?;

    // Hard-fail if any input the conf needs is missing — better than
    // writing a broken server.conf that'd silently 403 the picker.
    let serial = device.as_ref().map(|d| d.serial.clone()).unwrap_or_default();
    let conf = render_conf(&state, serial).await.ok_or_else(|| {
        "couldn't resolve server.conf inputs (need a running server with token + a LAN IP)"
            .to_string()
    })?;

    let source = state.kual_source.clone();
    let app_handle = app.clone();
    tokio::task::spawn_blocking(move || {
        kual::install_all(&source, &conf, &mount, |progress| {
            let _ = app_handle.emit("kual:install-progress", progress);
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}
