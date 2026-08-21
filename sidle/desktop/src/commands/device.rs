//! Tauri commands for Kindle device sync.

use serde::Serialize;
use sidle_core::library::paths::parse_sha_infix;
use tauri::{AppHandle, Emitter, State};

use crate::device_monitor::{ensure_transport, evict_transport, refresh_free_space};
use crate::library::{db, ingest};
use crate::state::AppState;
use sidle_core::library::device::DeviceInfo;
use sidle_core::library::device::dedrm::{self, PullResult};
use sidle_core::library::device::deploy::{
    self, DeployInstallReport, DeployOverall, DeployStatus, ServerConfRender,
};
use sidle_core::library::device::inventory;
use sidle_core::library::device::push::{self, DeleteResult, PushResult};

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

/// Scan `documents/Sidle/` on the connected device — see
/// [`inventory::list_ours`] for how each file is matched back to a library row.
#[tauri::command]
pub async fn device_list_ours(state: State<'_, AppState>) -> Result<Vec<inventory::Entry>, String> {
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
    let result = tokio::task::spawn_blocking(move || {
        let conn = db_handle.blocking_lock();
        inventory::list_ours(&conn, transport.as_ref())
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
        .map_err(|e| format!("{e:#}"))
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
                sidle_core::library::device::annotations::SyncProgress {
                    stage: stage.to_string(),
                    current,
                    total,
                    label: label.to_string(),
                },
            );
        };
        sidle_core::library::device::annotations::import_device_annotations(
            &device,
            transport.as_ref(),
            &crate::state::Borrowed(&db_handle),
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
/// re-pull (clear this device's sync checkpoints). NEVER deletes a Sidle row.
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
                    sidle_core::library::device::annotations::SyncProgress {
                        stage: stage.to_string(),
                        current,
                        total,
                        label: label.to_string(),
                    },
                );
            };
            let report = sidle_core::library::device::annotations::import_device_annotations(
                &device,
                transport.as_ref(),
                &crate::state::Borrowed(&db_handle),
                &paths,
                &on_progress,
            )?;
            // Notebooks ride a separate import path; re-pull them too (their records
            // were just cleared, so any deleted ones come back).
            let _ = sidle_core::library::device::notebooks::import_device_notebooks(
                transport.as_ref(),
                &paths,
                &crate::state::Borrowed(&db_handle),
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
            let on_device =
                sidle_core::library::device::TPath::parse("documents/Sidle").join(&filename);
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
// On-device app deploy (the Install / Update on Kindle button)
// ----------------------------------------------------------------------------

/// Resolve the live `ServerConfRender` from app state. Same shape used
/// by both `device_app_status` and `device_app_install` — keeping it in one place
/// guarantees the staleness check and the actual install agree on
/// what `server.conf` *should* contain. `serial` is the connected device's
/// USB iSerial, threaded in from the same `DeviceInfo` snapshot the mount came
/// from (the picker pushes it back as `device_serial`).
async fn render_conf(state: &AppState, serial: String) -> Option<ServerConfRender> {
    let server_status = state.server.status(&state.paths).await;
    let host = deploy::detect_lan_ipv4()?.to_string();
    let token = server_status.token?;
    let port = server_status.port.unwrap_or(server_status.default_port);
    Some(ServerConfRender {
        host,
        port,
        serial,
        token,
    })
}

/// The mount tree the fleet should have: the picker and bokai from the tree
/// that ships with this app, plus every app registered in the library.
///
/// Stages the cross-built picker into that tree first, so a bare `cargo build
/// --target armv7-…` is enough for the dev loop — the binary lands in `target/`
/// under another name, and the walk looks for it at the path it installs to.
pub async fn compose_plan(
    state: &AppState,
    source: &deploy::DeploySource,
) -> Result<sidle_core::library::apps::DevicePlan, String> {
    if let Err(e) = source.stage_binary() {
        eprintln!("[sidle/device] staging the picker into the mount tree: {e:#}");
    }
    let rows = {
        let conn = state.db.lock().await;
        sidle_core::library::db::list_app_sources(&conn).map_err(|e| e.to_string())?
    };
    let builtin = source.mount_dir.clone();
    tokio::task::spawn_blocking(move || sidle_core::library::apps::plan_from(&builtin, &rows))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn device_app_status(state: State<'_, AppState>) -> Result<DeployStatus, String> {
    let device = state.device.lock().await.clone();
    // No Kindle connected at all → the UI hides the section on DeviceDisconnected.
    // A connected device — mass-storage OR MTP — gets a real status; the deploy
    // runs over either transport.
    let Some(device) = device else {
        return Ok(DeployStatus {
            overall: DeployOverall::DeviceDisconnected,
            files: Vec::new(),
            binary_mtime_ms: None,
            native_source_mtime_ms: None,
        });
    };

    // No server token or no LAN IP → `None`, and the `etc/server.conf` slot
    // reports `SourceMissing` while every other slot is checked normally. This
    // is the same value `device_app_install` passes, so the status the user
    // reads is exactly what the button would push.
    let serial = device.serial.clone();
    let conf = render_conf(&state, serial).await;

    let source = state.device_app_source.clone();
    // Cheap and idempotent, and it has to exist before the status can say
    // anything true about `etc/ca.pem`. Creating the CA needs no server and no
    // network — just two files — so there is nothing to wait for.
    let _ = sidle_core::library::tls::ensure_ca(&state.paths);
    let ca_cert = state.paths.ca_cert();
    let plan = compose_plan(&state, &source).await?;
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| format!("open device transport: {e:#}"))?;
    let cell = state.transport.clone();
    let result = tokio::task::spawn_blocking(move || {
        deploy::compute_status(&plan, &source, conf.as_ref(), &ca_cert, transport.as_ref())
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
pub async fn device_app_install(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DeployInstallReport, String> {
    let device = state.device.lock().await.clone();
    let device = device.ok_or_else(|| "no Kindle connected".to_string())?;

    // `None` when there is no running server token or no routable LAN address.
    // That does not stop the push: `etc/server.conf` is the only slot that
    // needs one, it reports `SourceMissing`, and the binary, launcher, CA, KUAL
    // metadata and tile all land. A device on a network that cannot carry a
    // LAN address is exactly the device that needs the cable. A half-rendered
    // conf is still never written — the slot is skipped, not filled in with
    // blanks that would silently 403 the picker.
    let serial = device.serial.clone();
    let conf = render_conf(&state, serial).await;

    let source = state.device_app_source.clone();
    let plan = compose_plan(&state, &source).await?;
    let app_handle = app.clone();
    let dist_dir = state.paths.device_dist();
    // Hard-fail rather than push a bundle without the trust root: the picker
    // pins this CA and nothing else, so a device that receives every other file
    // but not `etc/ca.pem` cannot complete a single handshake — and would look
    // like a successful install.
    sidle_core::library::tls::ensure_ca(&state.paths)
        .map_err(|e| format!("issue the CA the device must pin: {e:#}"))?;
    let ca_cert = state.paths.ca_cert();
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| format!("open device transport: {e:#}"))?;
    let cell = state.transport.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<DeployInstallReport, String> {
        let report = deploy::install_all(
            &plan,
            conf.as_ref(),
            &ca_cert,
            transport.as_ref(),
            |progress| {
                let _ = app_handle.emit("device-app:install-progress", progress);
            },
        )
        .map_err(|e| format!("{e:#}"))?;
        // Refresh the LAN dist so an untethered in-app Update pull gets the
        // exact binary this push just wrote. Non-fatal — a staging miss
        // doesn't undo a successful device install.
        if let Err(e) = deploy::stage_dist(&source, &dist_dir) {
            eprintln!("[sidle/device_app_install] device-dist staging failed: {e:#}");
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

/// Re-stage the LAN self-update bundle (`<data-dir>/device-dist/`) when the
/// freshly cross-built picker binary is newer than the staged copy. Called on
/// the device-popover-open path alongside [`device_app_status`], so the dev loop is
/// "rebuild armv7 → open the popover → device pulls" — no cable, no app restart.
/// mtime-gated (a near-instant no-op once warm) and device-independent (it
/// stages the repo binary into the data dir), so it works whether or not a
/// Kindle is plugged in. Errors are surfaced but the frontend treats them as
/// non-fatal.
#[tauri::command]
pub async fn device_app_stage_dist(state: State<'_, AppState>) -> Result<(), String> {
    let source = state.device_app_source.clone();
    let dist_dir = state.paths.device_dist();
    tokio::task::spawn_blocking(move || deploy::stage_dist(&source, &dist_dir))
        .await
        .map_err(|e| e.to_string())?
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}
