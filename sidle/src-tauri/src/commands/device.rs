//! Tauri commands for Kindle device sync.

use serde::Serialize;
use sidle_core::library::paths::parse_sha_infix;
use tauri::{AppHandle, Emitter, State};

use crate::device::DeviceInfo;
use crate::device::dedrm::{self, PullResult};
use crate::device::kual::{self, KualInstallReport, KualOverall, KualStatus, ServerConfRender};
use crate::device::monitor::{ensure_transport, evict_transport, refresh_free_space};
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
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| {
            eprintln!("[sidle/device_list] open transport failed: {e:#}");
            e.to_string()
        })?;
    let db_handle = state.db.clone();
    let serial = device.serial.clone();
    let cell = state.transport.clone();
    let inner_serial = serial.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DeviceRow>> {
        let docs = crate::device::TPath::parse("documents/Sidle");
        // Surface USB/MTP errors instead of swallowing them — a silent
        // empty `list` is exactly the "popover shows 0 books while files
        // exist" symptom we're trying to make debuggable.
        let entries = match transport.list(&docs) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[sidle/device_list] {inner_serial}: list(documents/Sidle) failed: {e:#}"
                );
                return Err(e);
            }
        };
        let total = entries.len();
        let conn = db_handle.blocking_lock();

        let mut out = Vec::new();
        let mut skipped_meta = 0usize;
        let mut sent = 0usize;
        let mut orphan = 0usize;
        let mut legacy_matched = 0usize;
        let mut legacy_unmatched = 0usize;
        let mut dirs = 0usize;
        for entry in entries {
            if entry.is_dir {
                dirs += 1;
                continue;
            }
            if is_macos_metadata(&entry.name) {
                // `._foo.<sha>.kfx` AppleDouble companions get the same
                // sha8 suffix as the real file and would otherwise be
                // emitted as a duplicate row pointing at the same book.
                // Same logic for `.DS_Store` etc.
                skipped_meta += 1;
                continue;
            }
            // Primary match: the modern `<basename>.<sha8>.kfx` shape, by
            // `kfx_sha256` prefix. Fallback: legacy Sidle pushes (pre-sha8
            // naming) whose on-device filename is just the library row's
            // kfx basename — match by `kfx_path` suffix.
            let resolved = match parse_sha_infix(&entry.name) {
                Some(sha8) => db::find_by_kfx_sha_prefix(&conn, &sha8).map_err(anyhow::Error::from)?,
                None => match db::find_by_kfx_filename(&conn, &entry.name)
                    .map_err(anyhow::Error::from)?
                {
                    Some(book) => {
                        legacy_matched += 1;
                        Some(book)
                    }
                    None => {
                        legacy_unmatched += 1;
                        None
                    }
                },
            };
            match resolved {
                Some(book) => {
                    sent += 1;
                    out.push(DeviceRow::Sent {
                        book_id: book.id,
                        sha256: book.sha256,
                        title: book.title,
                        author: book.author,
                        filename: entry.name,
                    });
                }
                None => {
                    orphan += 1;
                    // Carry the parsed sha8 when we have one; for legacy
                    // un-prefixed files there is no sha8 and the empty
                    // string is the explicit sentinel for "unknown".
                    let sha8 = parse_sha_infix(&entry.name).unwrap_or_default();
                    out.push(DeviceRow::Orphan {
                        sha8,
                        filename: entry.name,
                    });
                }
            }
        }
        eprintln!(
            "[sidle/device_list] {inner_serial}: {total} entries → {} rows ({sent} sent, {orphan} orphan; {dirs} dirs, {skipped_meta} mac-meta skipped; legacy: {legacy_matched} matched / {legacy_unmatched} unmatched)",
            out.len(),
        );
        Ok(out)
    })
    .await;

    // Drop the cached MTP transport on USB/MTP error so the next caller's
    // `ensure_transport` opens a fresh session — a stalled endpoint or
    // post-error mtp-rs state isn't recoverable by reusing the same `Arc`.
    if matches!(result, Err(_) | Ok(Err(_))) {
        evict_transport(&cell).await;
        eprintln!("[sidle/device_list] {serial}: transport evicted after error");
    }

    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Import highlights / notes / bookmarks from the connected Kindle — either
/// transport. Mass-storage reads `documents/Sidle/` off the volume; MTP (Scribe)
/// pulls the `.yjr` over USB. Matches each annotated `.sdr` to a library book by
/// its `kfx_sha256` infix and extracts the highlighted text from the book's own
/// (readable) KFX. The dedup hash makes it idempotent — safe to re-run on every
/// connect.
///
/// MTP import only yields records if the device exposes its `.sdr/.yjr` sidecars
/// over MTP; if it doesn't, the report is simply 0 books.
#[tauri::command]
pub async fn annotations_import_from_device(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ingest::DeviceImportReport, String> {
    let device = state
        .device
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no Kindle connected".to_string())?;
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| e.to_string())?;
    let db_handle = state.db.clone();
    let paths = state.paths.clone();
    let cell = state.transport.clone();
    let result = tokio::task::spawn_blocking(move || {
        let on_progress = |stage: &str, current: usize, total: usize, label: &str| {
            let _ = app.emit(
                "annotations:sync-progress",
                crate::device::annotations::SyncProgress {
                    stage: stage.to_string(),
                    current,
                    total,
                    label: label.to_string(),
                },
            );
        };
        crate::device::annotations::import_device_annotations(
            &device,
            transport.as_ref(),
            &db_handle,
            &paths,
            &on_progress,
        )
    })
    .await;

    if matches!(result, Err(_) | Ok(Err(_))) {
        evict_transport(&cell).await;
    }

    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// "Restore from device" — re-import everything the connected Kindle holds and
/// UNDO Sidle-side deletions: clear all deletion records so accidentally-deleted
/// annotations / ink / notebooks the device still has come back, then force a full
/// re-pull (clear this device's sync checkpoints). NEVER deletes a Sidle row. See
/// .claude/plans/backup-source-of-truth.md.
#[tauri::command]
pub async fn device_restore(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ingest::DeviceImportReport, String> {
    let device = state
        .device
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no Kindle connected".to_string())?;
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| e.to_string())?;
    let db_handle = state.db.clone();
    let paths = state.paths.clone();
    let cell = state.transport.clone();
    let serial = device.serial.clone();

    let result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<ingest::DeviceImportReport> {
            // Undo every Sidle-side deletion + force a full re-pull, so anything still
            // on the device is re-imported.
            {
                let conn = db_handle.blocking_lock();
                db::clear_all_deletions(&conn)?;
                db::clear_device_sync_checkpoints(&conn, &serial)?;
            }
            let on_progress = |stage: &str, current: usize, total: usize, label: &str| {
                let _ = app.emit(
                    "annotations:sync-progress",
                    crate::device::annotations::SyncProgress {
                        stage: stage.to_string(),
                        current,
                        total,
                        label: label.to_string(),
                    },
                );
            };
            let report = crate::device::annotations::import_device_annotations(
                &device,
                transport.as_ref(),
                &db_handle,
                &paths,
                &on_progress,
            )?;
            // Notebooks ride a separate import path; re-pull them too (their records
            // were just cleared, so any deleted ones come back).
            let _ = crate::device::notebooks::import_device_notebooks(
                transport.as_ref(),
                &paths,
                &db_handle,
                &|_done, _total| {},
            );
            Ok(report)
        })
        .await;

    if matches!(result, Err(_) | Ok(Err(_))) {
        evict_transport(&cell).await;
    }

    result
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
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| e.to_string())?;
    let db_handle = state.db.clone();
    let cell = state.transport.clone();
    // `app` is moved into the blocking closure below (progress events); keep a
    // clone for the post-delete free-space refresh.
    let app_refresh = app.clone();

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DeleteResult>> {
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
            let result = match push::delete_one(&device, transport.as_ref(), &name, asin.as_deref())
            {
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
    .await;

    // Any per-file `Failed` (or a panic) means the cached MTP transport is
    // likely wedged — drop it so the next on-wire call ( the refreshDeviceList
    // that the frontend fires right after delete) opens a fresh session
    // instead of compounding errors against a stalled endpoint.
    let needs_evict = match &result {
        Err(_) => true,
        Ok(Err(_)) => true,
        Ok(Ok(rs)) => rs.iter().any(|r| matches!(r, DeleteResult::Failed { .. })),
    };
    if needs_evict {
        evict_transport(&cell).await;
        eprintln!("[sidle/device_delete] transport evicted after error");
    } else {
        // Files (and their `.sdr` sidecars) left the device — re-read free
        // space so the popover climbs back up immediately.
        refresh_free_space(&app_refresh, &state.device, &state.transport).await;
    }

    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Live byte-progress for the file currently being sent, emitted as
/// `device:send-active` so the footer / queue can show "Sending «title» —
/// 45 MB / 72 MB" while the transfer is in flight — distinct from the per-book
/// terminal `device:send-progress` (a `PushResult`). `total` is 0 when the size
/// is unknown. The frontend matches `book_id` to its seeded queue task and
/// derives batch position from that queue, so no index/count is sent.
#[derive(Clone, serde::Serialize)]
struct SendActive {
    book_id: i64,
    title: String,
    done: u64,
    total: u64,
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
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| e.to_string())?;
    let db_handle = state.db.clone();
    let cell = state.transport.clone();
    // `app` is moved into the blocking closure below (progress events); keep a
    // clone for the post-send free-space refresh.
    let app_refresh = app.clone();

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<PushResult>> {
        let conn = db_handle.blocking_lock();
        let mut out = Vec::with_capacity(book_ids.len());
        for book_id in book_ids {
            let result = match db::get_book(&conn, book_id)? {
                Some(book) => {
                    // Stream byte-progress for THIS file into the queue. The
                    // closure names the file (title); `push_one` ticks it as
                    // bytes land on the device. Captured by ref, lives only for
                    // this iteration.
                    let title = book.title.clone();
                    let on_progress = |done: u64, total: u64| {
                        let _ = app.emit(
                            "device:send-active",
                            SendActive {
                                book_id,
                                title: title.clone(),
                                done,
                                total,
                            },
                        );
                    };
                    match push::push_one(&device, transport.as_ref(), &conn, &book, &on_progress) {
                        Ok(r) => r,
                        Err(e) => PushResult::Failed {
                            book_id,
                            error: format!("{e:#}"),
                        },
                    }
                }
                None => PushResult::Failed {
                    book_id,
                    error: "book not found".into(),
                },
            };
            // Best-effort: notify UI of the per-book terminal result before
            // moving on so a long batch shows live progress.
            let _ = app.emit("device:send-progress", &result);
            out.push(result);
        }
        Ok(out)
    })
    .await;

    // Same MTP-stall recovery dance as `device_delete` — drop the cached
    // transport on any failure so the next call gets a fresh session.
    let needs_evict = match &result {
        Err(_) => true,
        Ok(Err(_)) => true,
        Ok(Ok(rs)) => rs.iter().any(|r| matches!(r, PushResult::Failed { .. })),
    };
    if needs_evict {
        evict_transport(&cell).await;
        eprintln!("[sidle/device_send] transport evicted after error");
    } else {
        // Books landed on the device — re-read free space so the popover drops
        // by what we just pushed instead of waiting for a reconnect. Skipped on
        // the evict path (the session is wedged; a reconnect refreshes anyway).
        refresh_free_space(&app_refresh, &state.device, &state.transport).await;
    }

    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Live progress for a single orphan import, emitted as `device:import-progress`
/// while the object is pulled off the device. `done`/`total` are byte counts;
/// `total` is 0 when the size wasn't known up front.
#[derive(Clone, Serialize)]
struct ImportProgress {
    filename: String,
    done: u64,
    total: u64,
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
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| e.to_string())?;
    let db_handle = state.db.clone();
    let paths = state.paths.clone();
    let queue = state.queue.clone();
    let cell = state.transport.clone();
    let app_progress = app.clone();

    let outer =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(PullResult, Option<i64>)> {
            let on_device = crate::device::TPath::parse("documents/Sidle").join(&filename);
            // Live byte-progress for the slow part: pulling the object off the
            // device. Over MTP this spans several PTP sessions (the Scribe's
            // per-session cap) and takes seconds for a multi-MiB book, which
            // otherwise looks hung between the click and the final toast.
            let on_progress = |done: u64, total: u64| {
                let _ = app_progress.emit(
                    "device:import-progress",
                    ImportProgress {
                        filename: filename.clone(),
                        done,
                        total,
                    },
                );
            };
            let bytes = transport.read_with_progress(&on_device, &on_progress)?;

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
        })
        .await;

    if matches!(outer, Err(_) | Ok(Err(_))) {
        evict_transport(&cell).await;
        eprintln!("[sidle/device_import_orphan] transport evicted after error");
    }

    let (result, enqueue) = outer
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
    Some(ServerConfRender {
        host,
        port,
        serial,
        token,
    })
}

#[tauri::command]
pub async fn kual_status(state: State<'_, AppState>) -> Result<KualStatus, String> {
    let device = state.device.lock().await.clone();
    // No Kindle connected at all → the UI hides the section on DeviceDisconnected.
    // A connected device — mass-storage OR MTP — gets a real status; the deploy
    // runs over either transport.
    let Some(device) = device else {
        return Ok(KualStatus {
            overall: KualOverall::DeviceDisconnected,
            files: Vec::new(),
            binary_mtime_ms: None,
            native_source_mtime_ms: None,
        });
    };

    // If we can't render a conf (no server token, no LAN IP), fall back
    // to a placeholder so the binary/bundle slots still get checked.
    // The status will read "server.conf stale" but at least the user
    // sees which other files need pushing.
    let serial = device.serial.clone();
    let conf = render_conf(&state, serial)
        .await
        .unwrap_or(ServerConfRender {
            host: String::new(),
            port: 0,
            serial: String::new(),
            token: String::new(),
        });

    let source = state.kual_source.clone();
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| format!("open device transport: {e:#}"))?;
    let cell = state.transport.clone();
    let result = tokio::task::spawn_blocking(move || {
        kual::compute_status(&source, &conf, transport.as_ref())
    })
    .await
    .map_err(|e| e.to_string())?;
    match result {
        Ok(status) => Ok(status),
        Err(e) => {
            // On-wire failure (mainly MTP): drop the cached session so the next
            // call reopens fresh rather than reusing a wedged endpoint.
            evict_transport(&cell).await;
            Err(format!("{e:#}"))
        }
    }
}

#[tauri::command]
pub async fn kual_install(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<KualInstallReport, String> {
    let device = state.device.lock().await.clone();
    let device = device.ok_or_else(|| "no Kindle connected".to_string())?;

    // Hard-fail if any input the conf needs is missing — better than
    // writing a broken server.conf that'd silently 403 the picker.
    let serial = device.serial.clone();
    let conf = render_conf(&state, serial).await.ok_or_else(|| {
        "couldn't resolve server.conf inputs (need a running server with token + a LAN IP)"
            .to_string()
    })?;

    let source = state.kual_source.clone();
    let app_handle = app.clone();
    let dist_dir = state.paths.kual_dist();
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| format!("open device transport: {e:#}"))?;
    let cell = state.transport.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<KualInstallReport, String> {
        let report = kual::install_all(&source, &conf, transport.as_ref(), |progress| {
            let _ = app_handle.emit("kual:install-progress", progress);
        })
        .map_err(|e| format!("{e:#}"))?;
        // Refresh the LAN dist so an untethered in-app Update pull gets the
        // exact binary this push just wrote. Non-fatal — a staging miss
        // doesn't undo a successful device install.
        if let Err(e) = kual::stage_dist(&source, &dist_dir) {
            eprintln!("[sidle/kual_install] kual-dist staging failed: {e:#}");
        }
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?;
    // On-wire failure (mainly MTP): drop the cached session so a retry reopens.
    if result.is_err() {
        evict_transport(&cell).await;
    }
    result
}

/// Re-stage the LAN self-update bundle (`<data-dir>/kual-dist/`) when the
/// freshly cross-built picker binary is newer than the staged copy. Called on
/// the device-popover-open path alongside [`kual_status`], so the dev loop is
/// "rebuild armv7 → open the popover → device pulls" — no cable, no app restart.
/// mtime-gated (a near-instant no-op once warm) and device-independent (it
/// stages the repo binary into the data dir), so it works whether or not a
/// Kindle is plugged in. Errors are surfaced but the frontend treats them as
/// non-fatal.
#[tauri::command]
pub async fn kual_stage_dist(state: State<'_, AppState>) -> Result<(), String> {
    let source = state.kual_source.clone();
    let dist_dir = state.paths.kual_dist();
    tokio::task::spawn_blocking(move || kual::stage_dist(&source, &dist_dir))
        .await
        .map_err(|e| e.to_string())?
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}
