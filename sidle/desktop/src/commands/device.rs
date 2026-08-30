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
use sidle_core::library::device::digest::DigestCache;
use sidle_core::library::device::dist;
use sidle_core::library::device::inventory;
use sidle_core::library::device::push::{self, DeleteResult, PushResult};

#[tauri::command]
pub async fn device_status(state: State<'_, AppState>) -> Result<Option<DeviceInfo>, String> {
    Ok(state.device.lock().await.clone())
}

/// Unmount and spin down a mass-storage Kindle, shelling out to `diskutil
/// eject`. A device with no `mass_storage_mount` errors here.
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

    // An error leaves the cached MTP transport wedged; the next
    // `ensure_transport` opens a fresh session.
    if matches!(result, Err(_) | Ok(Err(_))) {
        evict_transport(&cell).await;
        eprintln!("[sidle/device_list] {serial}: transport evicted after error");
    }

    result
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

/// Import highlights, notes and bookmarks from the connected Kindle over
/// either transport. Each annotated `.sdr` matches a library book by its
/// `kfx_sha256` infix, and a dedup hash keeps a re-run idempotent.
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

/// Re-import everything the connected Kindle holds: clears every deletion
/// record and this device's sync checkpoints. Deletes no library row.
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
            // `clear_all_deletions` and cleared checkpoints make the pull full.
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
            // Notebooks ride a separate import path.
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
            // `delete_one` takes the row's ASIN to reach a
            // `<title>_<ASIN>.sdr/` sidecar.
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

    // A per-file `Failed` or a panic leaves the cached MTP transport wedged.
    let needs_evict = match &result {
        Err(_) => true,
        Ok(Err(_)) => true,
        Ok(Ok(rs)) => rs.iter().any(|r| matches!(r, DeleteResult::Failed { .. })),
    };
    if needs_evict {
        evict_transport(&cell).await;
        eprintln!("[sidle/device_delete] transport evicted after error");
    } else {
        // Free space after the files and their `.sdr` sidecars left the device.
        refresh_free_space(&app_refresh, &state.device, &state.transport).await;
    }

    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Byte-progress for the file in flight, emitted as `device:send-active`.
/// `total` is 0 when the size is unknown; `book_id` keys the queue task.
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
                    // `push_one` ticks this closure as bytes land on the device.
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
            // `device:send-progress` carries the per-book terminal result.
            let _ = app.emit("device:send-progress", &result);
            out.push(result);
        }
        Ok(out)
    })
    .await;

    // A failure leaves the cached MTP transport wedged.
    let needs_evict = match &result {
        Err(_) => true,
        Ok(Err(_)) => true,
        Ok(Ok(rs)) => rs.iter().any(|r| matches!(r, PushResult::Failed { .. })),
    };
    if needs_evict {
        evict_transport(&cell).await;
        eprintln!("[sidle/device_send] transport evicted after error");
    } else {
        // Free space after the books landed. Skipped on the evict path.
        refresh_free_space(&app_refresh, &state.device, &state.transport).await;
    }

    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Progress for one orphan import, emitted as `device:import-progress` while
/// the object comes off the device. `done` and `total` are byte counts, and
/// `total` is 0 when the size is unknown.
#[derive(Clone, Serialize)]
struct ImportProgress {
    filename: String,
    done: u64,
    total: u64,
}

/// Pull an orphan `.kfx` off the device and into the local library: read via
/// [`Transport`], stage to a temp file, run the drag-drop import pipeline.
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
            // Byte-progress while the object comes off the device.
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

            // The staged file keeps its extension; the import pipeline
            // dispatches on it.
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
// Pushing the fleet to the device (the Apps tab's Install / Update / Update all)
// ----------------------------------------------------------------------------

/// The `ServerConfRender` `device_app_status` and `device_app_install` share.
/// `serial` is the connected device's USB iSerial.
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

/// The mount tree the fleet should have: the built-in tree plus every app
/// registered in the library. `stage_binary` puts the cross-built picker into
/// that tree at the path it installs to.
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
    let source = source.clone();
    let dist_dir = state.paths.device_dist();
    let digest_path = state.paths.source_digests();
    tokio::task::spawn_blocking(move || {
        let plan = sidle_core::library::apps::plan_from(&builtin, &rows);
        let mut digests = DigestCache::open(&digest_path);
        if let Err(e) = dist::refresh(&plan, &source, &dist_dir, &mut digests) {
            eprintln!("[sidle/device] device-dist refresh failed: {e:#}");
        }
        let _ = digests.save();
        plan
    })
    .await
    .map_err(|e| e.to_string())
}

/// Per-file and per-app state of `plan` against the connected Kindle.
pub async fn device_app_status(
    state: &AppState,
    plan: &sidle_core::library::apps::DevicePlan,
    source: &deploy::DeploySource,
) -> Result<DeployStatus, String> {
    let device = state.device.lock().await.clone();
    // `DeviceDisconnected` with no device; either transport gets a real status.
    let Some(device) = device else {
        return Ok(DeployStatus {
            overall: DeployOverall::DeviceDisconnected,
            apps: Vec::new(),
            files: Vec::new(),
            binary_mtime_ms: None,
            native_source_mtime_ms: None,
        });
    };

    // `None` with no server token or no LAN IP: the `etc/server.conf` slot then
    // reports `SourceMissing`, and `device_app_install` passes the same value.
    let serial = device.serial.clone();
    let conf = render_conf(state, serial).await;

    let plan = plan.clone();
    let source = source.clone();
    // The CA the `etc/ca.pem` slot is compared against.
    let _ = sidle_core::library::tls::ensure_ca(&state.paths);
    let ca_cert = state.paths.ca_cert();
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| format!("open device transport: {e:#}"))?;
    let cell = state.transport.clone();
    let digest_path = state.paths.source_digests();
    let result = tokio::task::spawn_blocking(move || {
        let mut digests = DigestCache::open(&digest_path);
        let status = deploy::compute_status(
            &plan,
            &source,
            conf.as_ref(),
            &ca_cert,
            transport.as_ref(),
            &mut digests,
        );
        let _ = digests.save();
        status
    })
    .await
    .map_err(|e| e.to_string())?;
    match result {
        Ok(status) => Ok(status),
        Err(e) => {
            // An on-wire failure leaves the cached session wedged.
            evict_transport(&cell).await;
            Err(format!("{e:#}"))
        }
    }
}

/// Push the apps `only` names, or the whole fleet with `only` absent. `force`
/// overwrites files the device changed. An empty `only` pushes nothing.
#[tauri::command]
pub async fn device_app_install(
    app: AppHandle,
    state: State<'_, AppState>,
    only: Option<Vec<String>>,
    force: Option<bool>,
) -> Result<DeployInstallReport, String> {
    let device = state.device.lock().await.clone();
    let device = device.ok_or_else(|| "no Kindle connected".to_string())?;

    // `None` with no server token or no routable LAN address: the
    // `etc/server.conf` slot reports `SourceMissing` and every other slot lands.
    let serial = device.serial.clone();
    let conf = render_conf(&state, serial).await;

    let source = state.device_app_source.clone();
    let fleet = compose_plan(&state, &source).await?;
    let plan = match &only {
        Some(ids) => fleet.narrow(ids).map_err(|e| format!("{e:#}"))?,
        None => fleet,
    };
    let app_handle = app.clone();
    // The only root the picker pins. A push without it completes no handshake.
    sidle_core::library::tls::ensure_ca(&state.paths)
        .map_err(|e| format!("issue the CA the device must pin: {e:#}"))?;
    let ca_cert = state.paths.ca_cert();
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| format!("open device transport: {e:#}"))?;
    let cell = state.transport.clone();
    let digest_path = state.paths.source_digests();
    let result = tokio::task::spawn_blocking(move || -> Result<DeployInstallReport, String> {
        let mut digests = DigestCache::open(&digest_path);
        let report = deploy::install_all(
            &plan,
            conf.as_ref(),
            &ca_cert,
            transport.as_ref(),
            force.unwrap_or(false),
            &mut digests,
            |progress| {
                let _ = app_handle.emit("device-app:install-progress", progress);
            },
        )
        .map_err(|e| format!("{e:#}"))?;
        let _ = digests.save();
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?;
    // An on-wire failure leaves the cached session wedged.
    if result.is_err() {
        evict_transport(&cell).await;
    }
    result
}

/// Take one app off the connected Kindle: its extension directory and its
/// tile. Its `apps` row stands; `apps_remove` drops that.
#[tauri::command]
pub async fn device_app_uninstall(
    state: State<'_, AppState>,
    id: String,
) -> Result<deploy::UninstallReport, String> {
    let device = state.device.lock().await.clone();
    let device = device.ok_or_else(|| "no Kindle connected".to_string())?;
    let source = state.device_app_source.clone();
    let plan = compose_plan(&state, &source).await?;
    let mut narrowed = plan
        .narrow(std::slice::from_ref(&id))
        .map_err(|e| format!("{e:#}"))?;
    let tree = narrowed.apps.swap_remove(0);
    let transport = ensure_transport(&state.transport, &device)
        .await
        .map_err(|e| format!("open device transport: {e:#}"))?;
    let cell = state.transport.clone();
    let result = tokio::task::spawn_blocking(move || deploy::uninstall(&tree, transport.as_ref()))
        .await
        .map_err(|e| e.to_string())?;
    match result {
        Ok(report) => Ok(report),
        Err(e) => {
            evict_transport(&cell).await;
            Err(format!("{e:#}"))
        }
    }
}
